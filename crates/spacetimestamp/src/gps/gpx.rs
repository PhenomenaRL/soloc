//! Streaming GPX 1.1 parsing and explicit observation mapping (`gpx` feature).
//!
//! [`GpxReader`](crate::gps::gpx::GpxReader) preserves file order, duplicate points and empty/disconnected track
//! segments. It recognizes the GPX 1.1 namespace, not just XML local names. GPX 1.0
//! is unsupported; standalone waypoints follow the required [`WaypointPolicy`](crate::gps::gpx::WaypointPolicy).
//! This is a reader for the documented track/route subset, not a full XSD validator.
//!
//! Parsing does not assign identities or manufacture a complete pose. [`GpxMapper`](crate::gps::gpx::GpxMapper)
//! requires one explicit mapping for every track/route used, including unnamed ones.
//! Names are metadata, never globally unique identities. Missing time and elevation
//! fail unless the caller explicitly supplies the applicable policy. Conversion
//! reuses [`convert_gps_observation`](crate::gps::convert_gps_observation) and does not append to a ledger.
//! Use [`import_gpx`](crate::gps::gpx::import_gpx) or
//! [`import_gpx_async`](crate::gps::gpx::import_gpx_async) to build bounded batches
//! and submit through a caller-supplied sink, with partial-completion accounting.
//!
//! [`GpxEvent`](crate::gps::gpx::GpxEvent) retains object/segment boundaries and reports ignored XML. A mapped
//! point retains its original fields, context, and selected height/time policies;
//! the standard entity schema has no GPX segment/metadata columns. Keep that context
//! (and the input file when complete provenance is needed) alongside Arrow output.
//! Do not infer continuous motion across segment gaps. Points are not sorted,
//! deduplicated, resampled, or required to have monotonically increasing times.
//!
//! Timestamp text is a deliberately strict, timezone-qualified XML dateTime subset:
//! years 0001–9999, uppercase `T`/`Z`, explicit `Z` or `±HH:MM` (at most 14:00),
//! seconds 00–59, and at most nine fractional digits. `24:00`, timezone-free text and
//! literal leap-second `:60` are rejected before hifitime parsing can lose information.
//! XML `-00:00` is accepted as UTC. UTC offsets are resolved as calendar offsets,
//! then the UTC calendar is converted to an [`Epoch`].
//!
//! ```
//! use anise::prelude::Almanac;
//! use spacetimestamp::{PrescribedId, gps::{GpsOptions, GpsOrientation, gpx::*}};
//! let xml = br#"<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1" creator="example">
//!   <trk><trkseg><trkpt lat="44.6488" lon="-63.5752">
//!     <ele>12</ele><time>2025-01-01T00:00:00Z</time>
//!   </trkpt></trkseg></trk>
//! </gpx>"#;
//! let gps = GpsOptions::new(
//!     PrescribedId::new("example.org", "vessel-1")?,
//!     PrescribedId::abstract_source("example.org", "gpx-receiver")?,
//! );
//! let mapper = GpxMapper::new("journey.gpx", [(GpxObjectId::Track(1), GpxMapping {
//!     gps,
//!     height: GpxHeightPolicy::EllipsoidalElevation, // explicit datum declaration
//!     time: GpxTimePolicy::RequireTimestamp,
//!     orientation: GpsOrientation::LocalEnu,
//! })])?;
//! for event in GpxReader::new(xml.as_slice(), "journey.gpx", WaypointPolicy::Reject) {
//!     match event? {
//!         GpxEvent::Point(point) => {
//!             let mapped = mapper.convert(&point, &Almanac::default())?;
//!             let batch = mapped.converted.to_entity_batch();
//!             assert_eq!(batch.num_rows(), 1);
//!             // Keep mapped.point.context/metadata beside the entity rows.
//!         }
//!         // A real importer also retains metadata, ignored-field reports and
//!         // segment events; they are deliberately separate from EntitySchema.
//!         _ => {}
//!     }
//! }
//! let report = import_gpx(
//!     GpxReader::new(xml.as_slice(), "journey.gpx", WaypointPolicy::Reject),
//!     &mapper, &Almanac::default(), GpxImportOptions::default(),
//!     |batch| { assert_eq!(batch.num_rows(), 1); Ok(()) }, // replace with your append sink
//!     |event| { /* stream provenance to your sidecar here */ },
//! );
//! assert!(report.complete);
//! assert_eq!(report.submitted, 1);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;
use std::fmt;

use anise::prelude::{Almanac, Epoch};
use chrono::{DateTime, Datelike, Timelike};

use super::{ConvertedGpsObservation, GpsError, GpsObservation, GpsOptions, GpsOrientation};
use crate::EstimateType;

mod reader;
pub use reader::GpxReader;
mod import;
pub use import::{
    GpxBatchInfo, GpxImportEvent, GpxImportFailure, GpxImportOptions, GpxImportReport,
    GpxPointErrorPolicy, GpxSubmitError, GpxSubmitOutcome, import_gpx, import_gpx_async,
};

/// One-based ordinal within the file, counted separately for tracks and routes.
/// A name is not used as identity; empty objects still consume an ordinal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GpxObjectId {
    Track(usize),
    Route(usize),
}

impl fmt::Display for GpxObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Track(index) => write!(f, "track {index}"),
            Self::Route(index) => write!(f, "route {index}"),
        }
    }
}

/// Original file position. Indices are one-based, never sorted by timestamp.
/// Track point indices restart in each segment; route points have no segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpxContext {
    pub source: String,
    pub object: Option<GpxObjectId>,
    pub segment: Option<usize>,
    pub point: Option<usize>,
}

impl fmt::Display for GpxContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.source)?;
        if let Some(object) = self.object {
            write!(f, ": {object}")?;
        }
        if let Some(segment) = self.segment {
            write!(f, ", segment {segment}")?;
        }
        if let Some(point) = self.point {
            write!(f, ", point {point}")?;
        }
        Ok(())
    }
}

/// A standalone waypoint is not assumed to be an observation of a track object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaypointPolicy {
    Reject,
    /// Report an ignored waypoint and validate its XML, without converting it.
    Ignore,
}

/// One raw GPX point. Time and elevation remain optional until mapping.
/// Metadata/quality values are retained as text, not inferred attitude/covariance.
#[derive(Debug, Clone, PartialEq)]
pub struct GpxPoint {
    pub context: GpxContext,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub elevation_m: Option<f64>,
    pub geoid_height_m: Option<f64>,
    pub time: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

/// Ordered stream events. Empty objects/segments and disconnected segments remain
/// observable even when they contain no successfully converted points.
#[derive(Debug, Clone, PartialEq)]
pub enum GpxEvent {
    DocumentStart {
        creator: String,
    },
    ObjectStart {
        context: GpxContext,
    },
    SegmentStart {
        context: GpxContext,
    },
    Point(GpxPoint),
    SegmentEnd {
        context: GpxContext,
    },
    ObjectEnd {
        context: GpxContext,
    },
    Metadata {
        context: GpxContext,
        field: String,
        value: String,
    },
    /// An outer unsupported element/subtree. Its contents are not retained or
    /// interpreted as observations, even if they contain GPX-looking local names.
    Ignored {
        context: GpxContext,
        element: String,
    },
    DocumentEnd,
}

/// A parse, policy or conversion error at its original file/object/point context.
#[derive(Debug, Clone, PartialEq)]
pub struct GpxError {
    pub context: GpxContext,
    pub kind: GpxErrorKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GpxErrorKind {
    Xml(String),
    Unsupported(String),
    Structure(String),
    MissingField(&'static str),
    InvalidField {
        field: &'static str,
        message: String,
    },
    MissingMapping,
    InvalidPolicy(String),
    Conversion(Box<GpsError>),
}

impl fmt::Display for GpxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.context)?;
        match &self.kind {
            GpxErrorKind::Xml(message) => write!(f, "invalid XML: {message}"),
            GpxErrorKind::Unsupported(message) => write!(f, "unsupported GPX: {message}"),
            GpxErrorKind::Structure(message) => write!(f, "invalid GPX structure: {message}"),
            GpxErrorKind::MissingField(field) => write!(f, "missing {field}"),
            GpxErrorKind::InvalidField { field, message } => {
                write!(f, "invalid {field}: {message}")
            }
            GpxErrorKind::MissingMapping => {
                write!(f, "an explicit track/route identity mapping is required")
            }
            GpxErrorKind::InvalidPolicy(message) => write!(f, "invalid mapping policy: {message}"),
            GpxErrorKind::Conversion(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for GpxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            GpxErrorKind::Conversion(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// A required declaration about the GPX elevation datum. Nothing is inferred from
/// `<ele>` or the presence of `<geoidheight>` alone; `h = H + N` is used only when
/// the caller declares orthometric elevation and compatible geoid undulation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GpxHeightPolicy {
    /// `<ele>` is already WGS84 ellipsoidal height. Missing elevation is an error.
    EllipsoidalElevation,
    /// `<ele>` is orthometric H; `<geoidheight>` is undulation N. Both are required.
    ElevationPlusGeoidHeight,
    /// Apply a caller-declared constant geoid undulation N to every `<ele>` H.
    ElevationWithGeoidOffset(f64),
    /// Explicitly replace all elevations, including absent ones, with this height.
    /// This is a caller assumption, not a measured elevation.
    FixedEllipsoidalHeight(f64),
}

/// Called only for missing point times; it may use the original segment/point
/// context to supply a schedule. No default start time or interval is inferred.
pub type GpxSchedule<'a> = dyn Fn(&GpxPoint) -> Result<Epoch, String> + 'a;

#[derive(Clone, Copy)]
pub enum GpxTimePolicy<'a> {
    RequireTimestamp,
    /// Existing timestamp text is still validated and used. A missing timestamp
    /// calls this schedule and requires the mapping's estimate type to be SIMULATED.
    FillMissing(&'a GpxSchedule<'a>),
}

impl fmt::Debug for GpxTimePolicy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequireTimestamp => f.write_str("RequireTimestamp"),
            Self::FillMissing(_) => f.write_str("FillMissing(<caller schedule>)"),
        }
    }
}

/// Explicit, stable settings for one track or route. Construct one per object and
/// pass them all to [`GpxMapper::new`]. The mapper never guesses from GPX names.
#[derive(Debug, Clone, Copy)]
pub struct GpxMapping<'a> {
    pub gps: GpsOptions<'a>,
    pub height: GpxHeightPolicy,
    pub time: GpxTimePolicy<'a>,
    pub orientation: GpsOrientation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpxTimeOrigin {
    Recorded,
    Scheduled,
}

/// Validated output together with the original GPX point and explicit assumptions.
/// `converted.append_to(...)` reuses an EntityBuilder; context/metadata and segment
/// boundaries must be retained separately because EntitySchema has no GPX columns.
#[derive(Debug, Clone, PartialEq)]
pub struct MappedGpxPoint {
    pub point: GpxPoint,
    pub observation: GpsObservation,
    pub converted: ConvertedGpsObservation,
    pub time_origin: GpxTimeOrigin,
    pub height_policy: GpxHeightPolicy,
}

/// Maps individual streamed points without retaining a whole journey or writing
/// to storage. Structural and policy errors carry the reader's original context.
pub struct GpxMapper<'a> {
    source: String,
    mappings: BTreeMap<GpxObjectId, GpxMapping<'a>>,
}

impl<'a> GpxMapper<'a> {
    /// Duplicate/zero object ordinals are rejected, so a mapping cannot silently
    /// overwrite another object's policy. Several explicit mappings may share an
    /// entity ID when the caller knows those tracks/routes describe the same object.
    pub fn new(
        source: impl Into<String>,
        mappings: impl IntoIterator<Item = (GpxObjectId, GpxMapping<'a>)>,
    ) -> Result<Self, GpxError> {
        let source = source.into();
        let mut by_object = BTreeMap::new();
        for (object, mapping) in mappings {
            let context = GpxContext {
                source: source.clone(),
                object: Some(object),
                segment: None,
                point: None,
            };
            let fail = |message: &str| GpxError {
                context: context.clone(),
                kind: GpxErrorKind::InvalidPolicy(message.into()),
            };
            if matches!(object, GpxObjectId::Track(0) | GpxObjectId::Route(0)) {
                return Err(fail("object ordinals are one-based"));
            }
            if by_object.contains_key(&object) {
                return Err(fail("duplicate object mapping"));
            }
            if let GpxHeightPolicy::ElevationWithGeoidOffset(value)
            | GpxHeightPolicy::FixedEllipsoidalHeight(value) = mapping.height
                && !value.is_finite()
            {
                return Err(fail("declared height/offset must be finite"));
            }
            if matches!(mapping.height, GpxHeightPolicy::FixedEllipsoidalHeight(_))
                && mapping.gps.estimate_type == EstimateType::MEASURED
            {
                return Err(fail(
                    "a fixed assumed height cannot describe a MEASURED pose",
                ));
            }
            by_object.insert(object, mapping);
        }
        Ok(Self {
            source,
            mappings: by_object,
        })
    }

    /// Convert one point through the shared GPS adapter. No mutation, append, sort
    /// or deduplication is performed. A caller can stop on failure or explicitly
    /// choose to record a skipped-point error and continue reading later points.
    pub fn convert(&self, point: &GpxPoint, almanac: &Almanac) -> Result<MappedGpxPoint, GpxError> {
        let error = |kind| GpxError {
            context: point.context.clone(),
            kind,
        };
        if point.context.source != self.source {
            return Err(error(GpxErrorKind::InvalidPolicy(
                "reader source does not match this mapper's source".into(),
            )));
        }
        let mapping = point
            .context
            .object
            .and_then(|object| self.mappings.get(&object))
            .ok_or_else(|| error(GpxErrorKind::MissingMapping))?;
        let required = |value: Option<f64>, field| {
            value.ok_or_else(|| error(GpxErrorKind::MissingField(field)))
        };
        let height = match mapping.height {
            GpxHeightPolicy::EllipsoidalElevation => required(point.elevation_m, "ele")?,
            GpxHeightPolicy::ElevationPlusGeoidHeight => {
                required(point.elevation_m, "ele")? + required(point.geoid_height_m, "geoidheight")?
            }
            GpxHeightPolicy::ElevationWithGeoidOffset(offset) => {
                required(point.elevation_m, "ele")? + offset
            }
            GpxHeightPolicy::FixedEllipsoidalHeight(height) => height,
        };
        let (epoch, time_origin) = match &point.time {
            Some(text) => (
                parse_gpx_time(text).map_err(|message| {
                    error(GpxErrorKind::InvalidField {
                        field: "time",
                        message,
                    })
                })?,
                GpxTimeOrigin::Recorded,
            ),
            None => match mapping.time {
                GpxTimePolicy::RequireTimestamp => {
                    return Err(error(GpxErrorKind::MissingField("time")));
                }
                GpxTimePolicy::FillMissing(schedule) => {
                    if mapping.gps.estimate_type != EstimateType::SIMULATED {
                        return Err(error(GpxErrorKind::InvalidPolicy(
                            "scheduled timestamps require SIMULATED; no estimate type is changed implicitly".into(),
                        )));
                    }
                    (
                        schedule(point).map_err(|message| {
                            error(GpxErrorKind::InvalidField {
                                field: "schedule",
                                message,
                            })
                        })?,
                        GpxTimeOrigin::Scheduled,
                    )
                }
            },
        };
        let observation = GpsObservation {
            latitude_deg: point.latitude_deg,
            longitude_deg: point.longitude_deg,
            ellipsoidal_height_m: height,
            epoch,
            orientation: mapping.orientation,
        };
        let converted = super::convert_gps_observation(&observation, &mapping.gps, almanac)
            .map_err(|cause| error(GpxErrorKind::Conversion(Box::new(cause))))?;
        Ok(MappedGpxPoint {
            point: point.clone(),
            observation,
            converted,
            time_origin,
            height_policy: mapping.height,
        })
    }
}

/// Parse the documented timezone-qualified GPX timestamp subset without silently
/// dropping sub-nanosecond precision or folding a literal UTC leap second.
pub fn parse_gpx_time(text: &str) -> Result<Epoch, String> {
    let text = text.trim_matches([' ', '\t', '\r', '\n']);
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|s| s.iter().all(u8::is_ascii_digit))
    };
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || !digits(0..4)
        || !digits(5..7)
        || !digits(8..10)
        || !digits(11..13)
        || !digits(14..16)
        || !digits(17..19)
    {
        return Err("expected YYYY-MM-DDTHH:MM:SS[.nanoseconds] with Z or ±HH:MM timezone".into());
    }
    if &bytes[0..4] == b"0000" {
        return Err("year must be 0001–9999".into());
    }
    if &bytes[17..19] == b"60" {
        return Err("literal leap-second :60 is unsupported; use the single-observation API with an explicit TAI/GPST Epoch for that instant".into());
    }
    if &bytes[11..13] == b"24" {
        return Err("24:00 is unsupported; use the next calendar date at 00:00".into());
    }
    let mut timezone_start = 19;
    if bytes.get(timezone_start) == Some(&b'.') {
        timezone_start += 1;
        let fraction_start = timezone_start;
        while bytes.get(timezone_start).is_some_and(u8::is_ascii_digit) {
            timezone_start += 1;
        }
        if !(1..=9).contains(&(timezone_start - fraction_start)) {
            return Err(
                "fractional seconds must have 1–9 digits; precision is never silently truncated"
                    .into(),
            );
        }
    }
    let timezone = &bytes[timezone_start..];
    if timezone != b"Z" {
        if timezone.len() != 6
            || !matches!(timezone[0], b'+' | b'-')
            || timezone[3] != b':'
            || !timezone[1..3].iter().all(u8::is_ascii_digit)
            || !timezone[4..6].iter().all(u8::is_ascii_digit)
        {
            return Err("a Z or ±HH:MM timezone is required".into());
        }
        let hours = (timezone[1] - b'0') * 10 + timezone[2] - b'0';
        let minutes = (timezone[4] - b'0') * 10 + timezone[5] - b'0';
        if hours > 14 || minutes > 59 || (hours == 14 && minutes != 0) {
            return Err("timezone offset must be within ±14:00".into());
        }
    }
    // Offset conversion belongs to the calendar parser, not SI subtraction from a
    // hifitime UTC Epoch: the latter crosses leap seconds as elapsed physical time.
    let utc = DateTime::parse_from_rfc3339(text)
        .map_err(|error| error.to_string())?
        .naive_utc();
    if !(1..=9999).contains(&utc.year()) {
        return Err("UTC date after timezone conversion must be within years 0001–9999".into());
    }
    Epoch::maybe_from_gregorian_utc(
        utc.year(),
        utc.month() as u8,
        utc.day() as u8,
        utc.hour() as u8,
        utc.minute() as u8,
        utc.second() as u8,
        utc.nanosecond(),
    )
    .map_err(|error| error.to_string())
}
