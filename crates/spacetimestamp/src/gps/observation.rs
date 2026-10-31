//! Validated GPS observations and EntitySchema construction.

use std::fmt;

use anise::prelude::{Almanac, Epoch};
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, TimeScale};
use nalgebra::{Matrix3, Quaternion, Rotation3, UnitQuaternion, Vector3};

use super::{itrf93_rotation, resolve_earth_frame, wgs84_to_ecef_m};
use crate::ephemeris::{epoch_from_parts, epoch_to_parts};
use crate::identity::PrescribedId;
use crate::schemas::entity::EntityBuilder;
use crate::vocabulary::{EstimateType, LengthUnit, TimeScaleCode};

/// How to assign the orientation required by Soloc's pose schema.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GpsOrientation {
    /// Conventional east/north/up axes at the fix, not measured vehicle attitude.
    /// Requires `ESTIMATED` or `SIMULATED`, since estimate type describes the whole pose.
    /// At a pole, the supplied longitude determines the east/north convention.
    LocalEnu,
    /// Caller-supplied entity-to-local-ENU rotation in `[w, x, y, z]` order.
    /// Components must be finite and the norm within `1e-6` of one. Accepted
    /// quaternions are normalized to remove roundoff. Use `MEASURED` only when
    /// both position and attitude are measurements; the adapter cannot verify that.
    QuaternionInEnu([f64; 4]),
}

/// One WGS84 fix with an explicit height, physical instant, and orientation policy.
///
/// Latitude/longitude are degrees, height is metres above the WGS84 ellipsoid
/// (negative is allowed). Missing height/time and an unknown vertical datum must be
/// resolved by the caller; the converter supplies no defaults for them.
///
/// Supply an unambiguous [`Epoch`]. In hifitime 4.3.1, parsing literal UTC `:60`
/// loses the leap-second distinction; use TAI/GPST for that instant instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpsObservation {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub ellipsoidal_height_m: f64,
    pub epoch: Epoch,
    pub orientation: GpsOrientation,
}

/// Identity/provenance and output policy, reusable across successive fixes.
///
/// Output units and time encoding are always metres and J2000 TAI. Stable entity
/// identity is supplied by the caller, never derived from the coordinates or time.
#[derive(Debug, Clone, Copy)]
pub struct GpsOptions<'a> {
    /// The tracked object's Soloc ID; astronomical and abstract IDs are rejected.
    pub entity_id: PrescribedId,
    /// A Soloc receiver ID or abstract import/process ID. May equal the entity ID
    /// for a self-reporting receiver; it is still a separate provenance role.
    pub source_id: PrescribedId,
    /// `ITRF93`, `Earth`/`IAU_EARTH`, or `GCRF`/`EME2000`.
    pub target_frame: &'a str,
    /// Applies to the entire pose; conventional LocalEnu cannot be `MEASURED`.
    pub estimate_type: EstimateType,
}

impl<'a> GpsOptions<'a> {
    /// Defaults to approximate ITRF93 and `ESTIMATED`, suitable for a fix whose
    /// orientation uses conventional local axes. No IDs are minted implicitly.
    pub fn new(entity_id: PrescribedId, source_id: PrescribedId) -> Self {
        Self {
            entity_id,
            source_id,
            target_frame: "ITRF93",
            estimate_type: EstimateType::ESTIMATED,
        }
    }
}

/// Recoverable conversion failures. Conversion performs no ledger or builder writes.
#[derive(Debug, Clone, PartialEq)]
pub enum GpsError {
    InvalidCoordinates(String),
    InvalidEntityId(PrescribedId),
    InvalidSourceId(PrescribedId),
    InvalidOrientation(String),
    /// A conventional orientation cannot be advertised as a fully measured pose.
    InvalidEstimateType,
    /// The instant is outside supported bounds or cannot round-trip through TAI row parts.
    InvalidEpoch {
        epoch: Epoch,
    },
    InvalidFrame {
        frame: String,
        message: String,
    },
    Transform {
        frame: String,
        epoch: Epoch,
        message: String,
    },
}

impl fmt::Display for GpsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCoordinates(message) => write!(f, "Invalid GPS coordinates: {message}"),
            Self::InvalidEntityId(id) => write!(f, "GPS entity_id {id} must be a Soloc entity ID"),
            Self::InvalidSourceId(id) => write!(
                f,
                "GPS source_id {id} must be a Soloc receiver or abstract provenance ID"
            ),
            Self::InvalidOrientation(message) => write!(f, "Invalid GPS orientation: {message}"),
            Self::InvalidEstimateType => write!(
                f,
                "LocalEnu is conventional, not measured attitude; select ESTIMATED or SIMULATED, or supply QuaternionInEnu for a measured pose"
            ),
            Self::InvalidEpoch { epoch } => write!(
                f,
                "GPS epoch {epoch} cannot be safely encoded as J2000 TAI row parts"
            ),
            Self::InvalidFrame { frame, message } => {
                write!(f, "Invalid GPS target frame '{frame}': {message}")
            }
            Self::Transform {
                frame,
                epoch,
                message,
            } => write!(f, "GPS transform to '{frame}' at {epoch} failed: {message}"),
        }
    }
}

impl std::error::Error for GpsError {}

/// A validated pose ready for an EntityBuilder, always in metres and TAI.
///
/// Private fields preserve validation between conversion and append. Accessors expose
/// reusable values for other schemas. Covariances, velocity, acceleration, mass, and
/// dimensions are unknown and remain null in the standard entity output.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertedGpsObservation {
    entity_id: PrescribedId,
    source_id: PrescribedId,
    frame_id: PrescribedId,
    estimate_type: EstimateType,
    position_m: [f64; 3],
    quaternion_wxyz: [f64; 4],
    epoch_parts: (i16, u64),
}

impl ConvertedGpsObservation {
    pub fn entity_id(&self) -> PrescribedId {
        self.entity_id
    }
    pub fn source_id(&self) -> PrescribedId {
        self.source_id
    }
    pub fn frame_id(&self) -> PrescribedId {
        self.frame_id
    }
    pub fn estimate_type(&self) -> EstimateType {
        self.estimate_type
    }
    pub fn position_m(&self) -> [f64; 3] {
        self.position_m
    }
    /// Entity-to-output-frame rotation, `[w, x, y, z]`, unit length.
    pub fn quaternion_wxyz(&self) -> [f64; 4] {
        self.quaternion_wxyz
    }
    /// J2000 TAI offsets; always pair these with [`TimeScaleCode::TAI`].
    pub fn epoch_parts(&self) -> (i16, u64) {
        self.epoch_parts
    }

    /// Append the validated observation to a reusable entity builder.
    pub fn append_to(&self, builder: &mut EntityBuilder) {
        builder.append_entity(
            self.entity_id,
            self.frame_id,
            LengthUnit::m,
            TimeScaleCode::TAI,
            self.source_id,
            self.estimate_type,
            self.position_m,
            self.quaternion_wxyz,
            self.epoch_parts.0,
            self.epoch_parts.1,
            None,
            None,
            None,
            None,
            None,
            None,
        );
    }

    /// Build one row with `entity_id` and a nested `spacetimestamp` struct.
    pub fn to_entity_batch(&self) -> RecordBatch {
        let mut builder = EntityBuilder::new(1);
        self.append_to(&mut builder);
        builder.flush()
    }
}

fn entity_to_enu(orientation: GpsOrientation) -> Result<UnitQuaternion<f64>, GpsError> {
    match orientation {
        GpsOrientation::LocalEnu => Ok(UnitQuaternion::identity()),
        GpsOrientation::QuaternionInEnu([w, x, y, z]) => {
            let values = [w, x, y, z];
            let norm = values
                .iter()
                .fold(0.0_f64, |norm, &value| norm.hypot(value));
            if !values.iter().all(|v| v.is_finite()) || (norm - 1.0).abs() > 1e-6 {
                return Err(GpsError::InvalidOrientation(
                    "QuaternionInEnu requires finite [w, x, y, z] with unit norm (tolerance 1e-6)"
                        .into(),
                ));
            }
            Ok(UnitQuaternion::from_quaternion(Quaternion::new(w, x, y, z)))
        }
    }
}

/// Convert and validate a fix without constructing Arrow arrays or modifying a ledger.
///
/// The same Earth rotation is applied to position and orientation. Attitude composition
/// is `R_ITRF93_to_target * R_ENU_to_ITRF93 * R_entity_to_ENU`. At the poles, longitude
/// selects the otherwise ambiguous local east/north axes. WGS84→ITRF93 follows the
/// ordinary-tracking approximation documented in [`crate::gps`].
pub fn convert_gps_observation(
    observation: &GpsObservation,
    options: &GpsOptions<'_>,
    almanac: &Almanac,
) -> Result<ConvertedGpsObservation, GpsError> {
    if !options.entity_id.is_soloc() {
        return Err(GpsError::InvalidEntityId(options.entity_id));
    }
    if !options.source_id.is_soloc() && !options.source_id.is_abstract() {
        return Err(GpsError::InvalidSourceId(options.source_id));
    }
    if observation.orientation == GpsOrientation::LocalEnu
        && options.estimate_type == EstimateType::MEASURED
    {
        return Err(GpsError::InvalidEstimateType);
    }
    let attitude = entity_to_enu(observation.orientation)?;
    let tai_duration = observation.epoch.to_tai_duration();
    // hifitime saturates out-of-range arithmetic, including conversions into TAI.
    // Reject the extremes before comparing the row encoding against that result.
    if observation.epoch.duration == Duration::MIN
        || observation.epoch.duration == Duration::MAX
        || tai_duration == Duration::MIN
        || tai_duration == Duration::MAX
    {
        return Err(GpsError::InvalidEpoch {
            epoch: observation.epoch,
        });
    }
    let parts = epoch_to_parts(observation.epoch);
    if epoch_from_parts(parts.0, parts.1, TimeScale::TAI).to_tai_duration() != tai_duration {
        return Err(GpsError::InvalidEpoch {
            epoch: observation.epoch,
        });
    }
    let frame_error = |message| GpsError::InvalidFrame {
        frame: options.target_frame.into(),
        message,
    };
    let target = resolve_earth_frame(options.target_frame).map_err(&frame_error)?;
    let frame_id = PrescribedId::astronomical(target.ephemeris_id, target.orientation_id)
        .map_err(frame_error)?;
    let position = wgs84_to_ecef_m(
        observation.latitude_deg,
        observation.longitude_deg,
        observation.ellipsoidal_height_m,
    )
    .map_err(GpsError::InvalidCoordinates)?;
    let transform_error = |message| GpsError::Transform {
        frame: options.target_frame.into(),
        epoch: observation.epoch,
        message,
    };
    let rotation = itrf93_rotation(observation.epoch, options.target_frame, target, almanac)
        .map_err(&transform_error)?;
    let position = rotation * Vector3::from(position);

    let (sin_lat, cos_lat) = observation.latitude_deg.to_radians().sin_cos();
    let (sin_lon, cos_lon) = observation.longitude_deg.to_radians().sin_cos();
    // Columns are local east, north, up expressed in ECEF/approximate ITRF93 axes.
    let enu_to_itrf = Rotation3::from_matrix_unchecked(Matrix3::from_columns(&[
        Vector3::new(-sin_lon, cos_lon, 0.0),
        Vector3::new(-sin_lat * cos_lon, -sin_lat * sin_lon, cos_lat),
        Vector3::new(cos_lat * cos_lon, cos_lat * sin_lon, sin_lat),
    ]));
    let quaternion = UnitQuaternion::from_rotation_matrix(&(rotation * enu_to_itrf)) * attitude;
    let quaternion_wxyz = [quaternion.w, quaternion.i, quaternion.j, quaternion.k];
    if !position
        .iter()
        .chain(quaternion_wxyz.iter())
        .all(|v| v.is_finite())
    {
        return Err(transform_error(
            "rotation produced non-finite pose components".into(),
        ));
    }
    Ok(ConvertedGpsObservation {
        entity_id: options.entity_id,
        source_id: options.source_id,
        frame_id,
        estimate_type: options.estimate_type,
        position_m: [position.x, position.y, position.z],
        quaternion_wxyz,
        epoch_parts: parts,
    })
}

/// Convert one GPS fix into a ledger-ready, one-row EntitySchema batch.
///
/// Output uses the requested Earth frame, metres, TAI, stable caller-supplied IDs,
/// and null optional state/covariances. All fallible conversion happens before an
/// entity builder is created. Append the successful result to an EntitySchema ledger;
/// other ledger schemas use their existing schema-mismatch error.
///
/// ```
/// use anise::prelude::Almanac;
/// use spacetimestamp::PrescribedId;
/// use spacetimestamp::gps::{GpsObservation, GpsOptions, GpsOrientation, gps_observation_to_batch};
///
/// let fix = GpsObservation {
///     latitude_deg: 44.6488,
///     longitude_deg: -63.5752,
///     ellipsoidal_height_m: 12.0,
///     epoch: "2025-01-01T00:00:00 UTC".parse()?,
///     orientation: GpsOrientation::LocalEnu,
/// };
/// let options = GpsOptions::new(
///     PrescribedId::new("example.org", "vessel-1")?,
///     PrescribedId::abstract_source("example.org", "gps-receiver")?,
/// ); // ITRF93, ESTIMATED: the orientation is conventional, not measured attitude.
/// let batch = gps_observation_to_batch(&fix, &options, &Almanac::default())?;
/// assert_eq!(batch.num_rows(), 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn gps_observation_to_batch(
    observation: &GpsObservation,
    options: &GpsOptions<'_>,
    almanac: &Almanac,
) -> Result<RecordBatch, GpsError> {
    Ok(convert_gps_observation(observation, options, almanac)?.to_entity_batch())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix() -> GpsObservation {
        GpsObservation {
            latitude_deg: 0.0,
            longitude_deg: 0.0,
            ellipsoidal_height_m: 0.0,
            epoch: "2025-01-01T00:00:00 UTC".parse().unwrap(),
            orientation: GpsOrientation::LocalEnu,
        }
    }

    fn options() -> GpsOptions<'static> {
        GpsOptions::new(
            PrescribedId::new("gps-test", "receiver").unwrap(),
            PrescribedId::abstract_source("gps-test", "receiver-log").unwrap(),
        )
    }

    fn convert(
        fix: &GpsObservation,
        options: &GpsOptions<'_>,
    ) -> Result<ConvertedGpsObservation, GpsError> {
        convert_gps_observation(fix, options, &Almanac::default())
    }

    #[test]
    fn rejects_non_entity_and_unknown_id_kinds() {
        let options = options();
        let astro = PrescribedId::astronomical_from_name("Earth").unwrap();
        let mut bytes = *options.entity_id.as_bytes();
        bytes[6] = 0x83; // UUIDv8, but an unsupported future kind.
        let unknown = PrescribedId::from_bytes(&bytes).unwrap();
        for id in [astro, options.source_id, unknown] {
            let invalid = GpsOptions {
                entity_id: id,
                ..options
            };
            assert_eq!(
                convert(&fix(), &invalid),
                Err(GpsError::InvalidEntityId(id))
            );
        }
        for id in [astro, unknown] {
            let invalid = GpsOptions {
                source_id: id,
                ..options
            };
            assert_eq!(
                convert(&fix(), &invalid),
                Err(GpsError::InvalidSourceId(id))
            );
        }
        // A physical receiver can report its own position without an artificial source ID.
        let self_report = GpsOptions {
            source_id: options.entity_id,
            ..options
        };
        assert!(convert(&fix(), &self_report).is_ok());
    }

    #[test]
    fn validates_coordinates_and_target_frame_before_building_rows() {
        for invalid in [
            GpsObservation {
                latitude_deg: 91.0,
                ..fix()
            },
            GpsObservation {
                longitude_deg: -181.0,
                ..fix()
            },
            GpsObservation {
                ellipsoidal_height_m: f64::NAN,
                ..fix()
            },
        ] {
            assert!(matches!(
                convert(&invalid, &options()),
                Err(GpsError::InvalidCoordinates(_))
            ));
        }
        for target in ["itrf93", "UNKNOWN", "ICRF", "Mars"] {
            let options = GpsOptions {
                target_frame: target,
                ..options()
            };
            match convert(&fix(), &options).unwrap_err() {
                GpsError::InvalidFrame { frame, message } => {
                    assert_eq!(frame, target);
                    assert!(!message.is_empty());
                }
                other => panic!("unexpected error: {other}"),
            }
        }
    }

    #[test]
    fn conventional_axes_cannot_claim_measured_attitude() {
        let mut options = options();
        options.estimate_type = EstimateType::MEASURED;
        assert_eq!(
            convert(&fix(), &options),
            Err(GpsError::InvalidEstimateType)
        );
        for estimate in [EstimateType::ESTIMATED, EstimateType::SIMULATED] {
            options.estimate_type = estimate;
            assert_eq!(convert(&fix(), &options).unwrap().estimate_type(), estimate);
        }
    }

    #[test]
    fn caller_quaternions_must_be_finite_and_near_unit() {
        for quaternion in [
            [0.0; 4],
            [2.0, 0.0, 0.0, 0.0],
            [1.000_01, 0.0, 0.0, 0.0],
            [f64::NAN, 0.0, 0.0, 0.0],
            [1.0, f64::INFINITY, 0.0, 0.0],
            [f64::MAX; 4],
        ] {
            let fix = GpsObservation {
                orientation: GpsOrientation::QuaternionInEnu(quaternion),
                ..fix()
            };
            assert!(matches!(
                convert(&fix, &options()),
                Err(GpsError::InvalidOrientation(_))
            ));
        }
        // Tiny length drift is normalized; signs q and -q describe the same attitude.
        let options = GpsOptions {
            estimate_type: EstimateType::MEASURED,
            ..options()
        };
        for sign in [-1.0, 1.0] {
            let fix = GpsObservation {
                orientation: GpsOrientation::QuaternionInEnu([sign * 1.000_000_5, 0.0, 0.0, 0.0]),
                ..fix()
            };
            let pose = convert(&fix, &options).unwrap();
            assert_eq!(pose.estimate_type(), EstimateType::MEASURED);
            let q = pose.quaternion_wxyz();
            assert!((q.iter().map(|v| v * v).sum::<f64>() - 1.0).abs() < 1e-14);
            let attitude = UnitQuaternion::from_quaternion(Quaternion::new(q[0], q[1], q[2], q[3]));
            assert!((attitude * Vector3::x() - Vector3::y()).norm() < 1e-14);
            assert!((attitude * Vector3::y() - Vector3::z()).norm() < 1e-14);
        }
    }

    #[test]
    fn pole_longitude_selects_explicit_local_axes() {
        for (latitude, longitude, east, north, up) in [
            (90.0, 0.0, Vector3::y(), -Vector3::x(), Vector3::z()),
            (90.0, 90.0, -Vector3::x(), -Vector3::y(), Vector3::z()),
            (-90.0, 0.0, Vector3::y(), Vector3::x(), -Vector3::z()),
        ] {
            let fix = GpsObservation {
                latitude_deg: latitude,
                longitude_deg: longitude,
                ..fix()
            };
            let q = convert(&fix, &options()).unwrap().quaternion_wxyz();
            let attitude = UnitQuaternion::from_quaternion(Quaternion::new(q[0], q[1], q[2], q[3]));
            for (axis, expected) in [
                (Vector3::x(), east),
                (Vector3::y(), north),
                (Vector3::z(), up),
            ] {
                assert!((attitude * axis - expected).norm() < 1e-14);
            }
        }
    }

    #[test]
    fn time_encoding_preserves_instants_and_rejects_saturation() {
        for input in [
            "2025-01-01T00:00:00.123456789 UTC",
            "2025-01-01T00:00:37.123456789 TAI",
            "2025-01-01T00:00:18.123456789 GPST",
        ] {
            let fix = GpsObservation {
                epoch: input.parse().unwrap(),
                ..fix()
            };
            assert_eq!(
                convert(&fix, &options()).unwrap().epoch_parts(),
                (0, 788_961_637_123_456_789)
            );
        }
        let leap = GpsObservation {
            epoch: "2017-01-01T00:00:36.123456789 TAI".parse().unwrap(),
            ..fix()
        };
        assert_eq!(
            convert(&leap, &options()).unwrap().epoch_parts(),
            (0, 536_500_836_123_456_789)
        );
        for epoch in [
            Epoch::from_tai_duration(Duration::MIN),
            Epoch::from_tai_duration(Duration::MIN + Duration::from_seconds(1.0)),
            Epoch {
                duration: Duration::MAX,
                time_scale: TimeScale::GPST,
            },
        ] {
            let fix = GpsObservation { epoch, ..fix() };
            assert_eq!(
                convert(&fix, &options()),
                Err(GpsError::InvalidEpoch { epoch })
            );
        }
    }
}
