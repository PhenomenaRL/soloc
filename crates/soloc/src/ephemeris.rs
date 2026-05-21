//! Ephemeris queries — converting solar system body positions into standard entity batches.
//!
//! [`celestial_snapshot`] is the primary entry point. It queries an [`anise::Almanac`] for the
//! ICRF positions and velocities of any selection of [`CelestialBody`] values at a given epoch,
//! and returns a standard entity [`RecordBatch`] that is schema-compatible with any other entity
//! batch and can be appended directly to a [`crate::ledger::Ledger`].
//!
//! Celestial bodies are not special — they are entities like any other, recorded with the same
//! schema as a spacecraft or robot. The only distinction is `source_id = "naif:de440s"` and
//! `estimate_type = "MEASURED"` (ephemeris data is derived from real observations).
//!
//! # Ephemeris data
//!
//! Positions come from the DE440/DE440s SPK files. Load them via `anise::MetaAlmanac::latest()`,
//! which downloads ~150 MB on first run and caches them in `~/.local/share/nyx-space/anise/`.

use anise::constants::celestial_objects::{
    EARTH, JUPITER, JUPITER_BARYCENTER, MARS, MERCURY, MOON, NEPTUNE,
    NEPTUNE_BARYCENTER, SATURN, SATURN_BARYCENTER, SUN, URANUS, URANUS_BARYCENTER,
    VENUS,
};
use anise::constants::frames::SSB_J2000;
use anise::prelude::{Almanac, Frame};
use nalgebra::{Rotation3, UnitQuaternion};
use arrow::record_batch::RecordBatch;
use hifitime::Epoch;
use std::str::FromStr;

use crate::entity::EntityBuilder;
use crate::ledger::Ledger;

// ---------------------------------------------------------------------------
// CelestialBody
// ---------------------------------------------------------------------------

/// Well-known solar system bodies whose ephemeris is available in the DE440 SPK family.
///
/// Each variant maps to a NAIF body center ID via [`naif_id`](Self::naif_id), which is
/// used to construct both the IAU body-fixed frame ([`iau_frame`](Self::iau_frame)) and
/// the J2000 barycenter frame ([`frame`](Self::frame)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CelestialBody {
    Sun,
    Mercury,
    Venus,
    Earth,
    Moon,
    Mars,
    Jupiter,
    Saturn,
    Uranus,
    Neptune,
}

impl CelestialBody {
    /// All ten supported bodies ordered by increasing semi-major axis.
    pub const ALL: &'static [CelestialBody] = &[
        CelestialBody::Sun,
        CelestialBody::Mercury,
        CelestialBody::Venus,
        CelestialBody::Earth,
        CelestialBody::Moon,
        CelestialBody::Mars,
        CelestialBody::Jupiter,
        CelestialBody::Saturn,
        CelestialBody::Uranus,
        CelestialBody::Neptune,
    ];

    /// NAIF body center integer ID for this body.
    ///
    /// By NAIF convention this also serves as the IAU orientation ID — the body's
    /// IAU body-fixed frame has the same integer for both ephemeris origin and
    /// orientation (see [`iau_frame`](Self::iau_frame)).
    pub fn naif_id(self) -> i32 {
        match self {
            CelestialBody::Sun     => SUN,      // 10
            CelestialBody::Mercury => MERCURY,  // 199
            CelestialBody::Venus   => VENUS,    // 299
            CelestialBody::Earth   => EARTH,    // 399
            CelestialBody::Moon    => MOON,     // 301
            CelestialBody::Mars    => MARS,     // 499
            CelestialBody::Jupiter => JUPITER,  // 599
            CelestialBody::Saturn  => SATURN,   // 699
            CelestialBody::Uranus  => URANUS,   // 799
            CelestialBody::Neptune => NEPTUNE,  // 899
        }
    }

    /// The IAU body-fixed frame for this body.
    ///
    /// Constructed as `Frame::new(naif_id, naif_id)` — the ephemeris origin is
    /// the planet body center and the orientation follows the IAU rotation model
    /// stored in the loaded PCK (e.g. `pck11.pca` from `MetaAlmanac::latest()`).
    /// The z-axis of this frame is the body's north pole (rotation axis); the
    /// x-axis points toward the prime meridian.
    ///
    /// This convention holds for all supported bodies including the Sun, for which
    /// anise does not export a named constant but the NAIF ID (10) is correct.
    pub fn iau_frame(self) -> Frame {
        Frame::new(self.naif_id(), self.naif_id())
    }

    /// The J2000-oriented frame used to query this body's state vector from DE440.
    ///
    /// Inner planets use their body center NaifId (same as their IAU frame origin).
    /// Outer planets (Jupiter–Neptune) use their system barycenter NaifId, which is
    /// what DE440 provides directly without additional satellite SPK files.
    ///
    /// Retained for callers that explicitly need barycenter frames (e.g. as transform
    /// targets). For body-center positions and IAU orientation use
    /// [`iau_frame`](Self::iau_frame).
    pub fn frame(self) -> Frame {
        let orientation = 1; // J2000 orientation NaifId
        match self {
            CelestialBody::Jupiter => Frame::new(JUPITER_BARYCENTER, orientation),
            CelestialBody::Saturn  => Frame::new(SATURN_BARYCENTER, orientation),
            CelestialBody::Uranus  => Frame::new(URANUS_BARYCENTER, orientation),
            CelestialBody::Neptune => Frame::new(NEPTUNE_BARYCENTER, orientation),
            _                      => Frame::new(self.naif_id(), orientation),
        }
    }

    /// Canonical entity ID for this body as stored in the soloc ledger.
    ///
    /// Uses NAIF body-center IDs (`naif:<id>`) as the globally unique identifier.
    /// These IDs are recognized by all soloc instances without any namespace configuration.
    pub fn entity_id(self) -> &'static str {
        match self {
            CelestialBody::Sun     => "naif:10",
            CelestialBody::Mercury => "naif:199",
            CelestialBody::Venus   => "naif:299",
            CelestialBody::Earth   => "naif:399",
            CelestialBody::Moon    => "naif:301",
            CelestialBody::Mars    => "naif:499",
            CelestialBody::Jupiter => "naif:599",
            CelestialBody::Saturn  => "naif:699",
            CelestialBody::Uranus  => "naif:799",
            CelestialBody::Neptune => "naif:899",
        }
    }

    /// Standard gravitational parameter GM in km³/s² (DE440 / IAU 2012).
    pub fn gm_km3_s2(self) -> f64 {
        match self {
            CelestialBody::Sun     => 1.327_124_400_419_393e11,
            CelestialBody::Mercury => 2.203_186_855_140_000_3e4,
            CelestialBody::Venus   => 3.248_585_920_000_000_6e5,
            CelestialBody::Earth   => 3.986_004_418e5,
            CelestialBody::Moon    => 4.904_869_5e3,
            CelestialBody::Mars    => 4.282_837_362_069_909e4,
            CelestialBody::Jupiter => 1.266_865_34e8,
            CelestialBody::Saturn  => 3.793_120_8e7,
            CelestialBody::Uranus  => 5.793_951_322_279_009e6,
            CelestialBody::Neptune => 6.835_099_502_439_672e6,
        }
    }

    /// Mass in kg, derived from GM / G where G = 6.674×10⁻²⁰ km³/(kg·s²).
    ///
    /// Consistent with the GM values used by the propagator in `soloc::sim`.
    pub fn mass_kg(self) -> f64 {
        const G_KM3_KG_S2: f64 = 6.674e-20;
        self.gm_km3_s2() / G_KM3_KG_S2
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn j2000_tai() -> Epoch {
    Epoch::from_str("2000-01-01T12:00:00 TAI").expect("J2000 TAI is a valid epoch string")
}

/// Converts an [`Epoch`] to the `(duration_centuries, duration_ns)` offset from J2000 TAI
/// used throughout the spacetimestamp schema.
fn epoch_to_parts(epoch: Epoch) -> (i16, u64) {
    (epoch - j2000_tai()).to_parts()
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Queries the ephemeris for the given bodies at `epoch` and returns a standard entity
/// [`RecordBatch`].
///
/// All rows use `frame_id = "ICRF"`, `units_pos = "km"`, `timescale_id = "TAI"`,
/// `source_id = "naif:de440s"`, and `estimate_type = "MEASURED"`.
///
/// - **Position**: body center relative to the Solar System Barycentre (SSB), from DE440.
/// - **Velocity**: body-center linear velocity from DE440.
/// - **Orientation**: quaternion `[w, x, y, z]` encoding the rotation from the body's
///   IAU body-fixed frame to ICRF. The z-axis of the IAU frame is the body's north pole
///   (rotation axis); the x-axis points toward the prime meridian. Derived from the IAU
///   PCK rotation model via `almanac.rotate()`.
/// - **Angular velocity**: body spin in ICRF (rad/s), derived from the time derivative of
///   the IAU rotation model. Present when the loaded PCK includes that derivative.
/// - **Mass**: from DE440/IAU 2012 GM constants.
///
/// The returned batch is schema-compatible with any other entity batch and can be appended
/// directly to a [`Ledger`]:
///
/// ```rust,ignore
/// ledger.append(celestial_snapshot(&almanac, CelestialBody::ALL, epoch)?);
/// ```
///
/// # Data requirements
///
/// - **Inner planets** (Sun, Mercury, Venus, Earth, Moon, Mars): position and orientation
///   are fully resolved by `MetaAlmanac::latest()` (DE440 + pck11.pca).
/// - **Outer planets** (Jupiter, Saturn, Uranus, Neptune): orientation is resolved by
///   `MetaAlmanac::latest()`, but body-center position additionally requires a satellite
///   SPK (e.g. `jup365.bsp` for Jupiter). Load it via `MetaAlmanac` or supply the file
///   directly to the `Almanac`.
///
/// # Errors
///
/// Returns `Err` if:
/// - `bodies` is empty.
/// - The almanac fails to resolve position or orientation for any body.
pub fn celestial_snapshot(
    almanac: &Almanac,
    bodies: &[CelestialBody],
    epoch: Epoch,
) -> Result<RecordBatch, String> {
    if bodies.is_empty() {
        return Err(
            "bodies list is empty — provide at least one CelestialBody".to_string(),
        );
    }

    let (centuries, ns) = epoch_to_parts(epoch);
    let mut builder = EntityBuilder::new(bodies.len(), None);

    for &body in bodies {
        let iau = body.iau_frame();

        let state = almanac
            .translate(iau, SSB_J2000, epoch, None)
            .map_err(|e| format!(
                "Failed to get body-center position for {:?} at {epoch}: {e}. \
                 Inner planets require DE440; outer planets (Jupiter+) additionally \
                 need a satellite SPK (e.g. jup365.bsp for Jupiter). Load via \
                 MetaAlmanac or supply the file directly.",
                body,
            ))?;

        let dcm = almanac
            .rotate(iau, SSB_J2000, epoch)
            .map_err(|e| format!(
                "Failed to get IAU orientation for {:?} at {epoch}: {e}. \
                 Ensure a PCK (e.g. pck11.pca) is loaded, available via \
                 MetaAlmanac::latest().",
                body,
            ))?;

        let q = UnitQuaternion::from_rotation_matrix(
            &Rotation3::from_matrix_unchecked(dcm.rot_mat),
        );

        // Angular velocity of the body in ICRF (rad/s): ω = axial_vec(dR/dt · Rᵀ).
        // Present when the PCK encodes the time derivative of the rotation model.
        let angular_velocity = dcm.rot_mat_dt.map(|r_dt| {
            let omega = r_dt * dcm.rot_mat.transpose();
            [omega[(2, 1)], omega[(0, 2)], omega[(1, 0)]]
        });

        builder.append_entity(
            body.entity_id(),
            "ICRF",
            "km",
            "TAI",
            "naif:de440s",
            "MEASURED",
            [state.radius_km.x, state.radius_km.y, state.radius_km.z],
            [q.w, q.i, q.j, q.k],
            centuries,
            ns,
            Some([state.velocity_km_s.x, state.velocity_km_s.y, state.velocity_km_s.z]),
            angular_velocity,
            None,
            Some(body.mass_kg()),
            None,
        );
    }

    Ok(builder.flush())
}

/// Queries the almanac for arbitrary NAIF bodies at `epoch` and returns a standard entity
/// [`RecordBatch`].
///
/// Each entry in `bodies` is a `(naif_id, entity_id)` pair — `naif_id` is the NAIF integer
/// ID of the body (e.g. `2099942` for Apophis, `599` for Jupiter center), and `entity_id`
/// is the URI to store in the ledger (e.g. `"naif:2099942"` for Apophis, or `"jpl-sb:2004-MN4"`).
///
/// Unlike [`celestial_snapshot`], orientation silently falls back to the identity quaternion
/// and `angular_velocity` to `None` when the loaded PCK has no rotation model for the
/// requested body — so this function succeeds for any body that has SPK position data,
/// regardless of PCK coverage. Mass is not populated (unknown for arbitrary bodies).
///
/// # Errors
///
/// Returns `Err` if:
/// - `bodies` is empty.
/// - The almanac cannot resolve the position of any body (SPK data missing).
pub fn naif_snapshot(
    almanac: &Almanac,
    bodies: &[(i32, &str)],
    epoch: Epoch,
) -> Result<RecordBatch, String> {
    if bodies.is_empty() {
        return Err(
            "bodies list is empty — provide at least one (naif_id, entity_id) pair".to_string(),
        );
    }

    let (centuries, ns) = epoch_to_parts(epoch);
    let mut builder = EntityBuilder::new(bodies.len(), None);

    for &(naif_id, entity_id) in bodies {
        let iau = Frame::new(naif_id, naif_id);

        let state = almanac
            .translate(iau, SSB_J2000, epoch, None)
            .map_err(|e| format!(
                "Failed to get position for NAIF ID {naif_id} ({entity_id}) at {epoch}: {e}.",
            ))?;

        let (quaternion, angular_velocity) = match almanac.rotate(iau, SSB_J2000, epoch) {
            Ok(dcm) => {
                let q = UnitQuaternion::from_rotation_matrix(
                    &Rotation3::from_matrix_unchecked(dcm.rot_mat),
                );
                let ang_vel = dcm.rot_mat_dt.map(|r_dt| {
                    let omega = r_dt * dcm.rot_mat.transpose();
                    [omega[(2, 1)], omega[(0, 2)], omega[(1, 0)]]
                });
                ([q.w, q.i, q.j, q.k], ang_vel)
            }
            Err(_) => ([1.0, 0.0, 0.0, 0.0], None),
        };

        builder.append_entity(
            entity_id,
            "ICRF", "km", "TAI", "anise", "MEASURED",
            [state.radius_km.x, state.radius_km.y, state.radius_km.z],
            quaternion,
            centuries,
            ns,
            Some([state.velocity_km_s.x, state.velocity_km_s.y, state.velocity_km_s.z]),
            angular_velocity,
            None,
            None,
            None,
        );
    }

    Ok(builder.flush())
}

/// Convenience wrapper: calls [`naif_snapshot`] and appends the result to `ledger`.
pub fn append_naif(
    ledger: &mut Ledger,
    almanac: &Almanac,
    bodies: &[(i32, &str)],
    epoch: Epoch,
) -> Result<(), String> {
    let batch = naif_snapshot(almanac, bodies, epoch)?;
    ledger.append(batch);
    Ok(())
}

/// Convenience wrapper: calls [`celestial_snapshot`] and appends the result to `ledger`.
///
/// Equivalent to `ledger.append(celestial_snapshot(almanac, bodies, epoch)?)`.
/// Returns `Err` without modifying the ledger if the snapshot fails.
pub fn append_celestial(
    ledger: &mut Ledger,
    almanac: &Almanac,
    bodies: &[CelestialBody],
    epoch: Epoch,
) -> Result<(), String> {
    let batch = celestial_snapshot(almanac, bodies, epoch)?;
    ledger.append(batch);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anise::prelude::Almanac;
    use arrow::array::Array;
    use hifitime::Duration;

    // --- CelestialBody metadata (no almanac required) ---

    #[test]
    fn test_entity_ids_are_unique() {
        let ids: Vec<&str> = CelestialBody::ALL.iter().map(|b| b.entity_id()).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len(), "duplicate entity IDs detected");
    }

    #[test]
    fn test_entity_ids_use_naif_prefix() {
        for body in CelestialBody::ALL {
            let id = body.entity_id();
            assert!(
                id.starts_with("naif:"),
                "{id:?} does not match expected naif: prefix"
            );
        }
    }

    #[test]
    fn test_all_bodies_have_positive_mass() {
        for body in CelestialBody::ALL {
            assert!(
                body.mass_kg() > 0.0,
                "{:?} has non-positive mass: {}",
                body,
                body.mass_kg()
            );
        }
    }

    #[test]
    fn test_sun_is_most_massive() {
        let sun_mass = CelestialBody::Sun.mass_kg();
        for body in CelestialBody::ALL {
            if *body != CelestialBody::Sun {
                assert!(
                    sun_mass > body.mass_kg(),
                    "Sun should be heavier than {:?}",
                    body
                );
            }
        }
    }

    #[test]
    fn test_gm_values_match_propagator_table() {
        // Spot-check a few values to ensure the ephemeris module stays in sync with
        // the GM table used by the N-body propagator.
        assert!((CelestialBody::Earth.gm_km3_s2() - 3.986_004_418e5).abs() < 1.0);
        assert!((CelestialBody::Sun.gm_km3_s2() - 1.327_124_400_419_393e11).abs() < 1e6);
        assert!((CelestialBody::Moon.gm_km3_s2() - 4.904_869_5e3).abs() < 1.0);
    }

    // --- epoch_to_parts round-trip ---

    #[test]
    fn test_epoch_to_parts_j2000_is_zero() {
        let j2000 = Epoch::from_str("2000-01-01T12:00:00 TAI").unwrap();
        let (centuries, ns) = epoch_to_parts(j2000);
        assert_eq!(centuries, 0);
        assert_eq!(ns, 0);
    }

    #[test]
    fn test_epoch_to_parts_round_trips() {
        let j2000 = j2000_tai();
        let target = j2000 + Duration::from_parts(0, 123_456_789_000u64);
        let (c, n) = epoch_to_parts(target);
        let recovered = j2000 + Duration::from_parts(c, n);
        // Should recover the exact same epoch (lossless round-trip).
        assert_eq!(recovered, target);
    }

    #[test]
    fn test_epoch_to_parts_before_j2000() {
        let j2000 = j2000_tai();
        // One second before J2000.
        let before = j2000 - Duration::from_parts(0, 1_000_000_000u64);
        let (c, n) = epoch_to_parts(before);
        let recovered = j2000 + Duration::from_parts(c, n);
        assert_eq!(recovered, before, "pre-J2000 epoch should round-trip correctly");
    }

    // --- celestial_snapshot with empty almanac ---

    #[test]
    fn test_empty_bodies_returns_error() {
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let err = celestial_snapshot(&almanac, &[], epoch).unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn test_no_spk_returns_descriptive_error() {
        // Almanac::default() has no SPK loaded — translate will fail.
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let err = celestial_snapshot(&almanac, &[CelestialBody::Earth], epoch).unwrap_err();
        assert!(
            err.contains("MetaAlmanac") || err.contains("SPK") || err.contains("ephemeris"),
            "error should guide user to load an SPK; got: {err}"
        );
    }

    #[test]
    fn test_append_celestial_does_not_mutate_ledger_on_error() {
        let mut ledger = crate::ledger::Ledger::new();
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let result = append_celestial(&mut ledger, &almanac, &[CelestialBody::Earth], epoch);
        assert!(result.is_err());
        assert!(ledger.is_empty(), "ledger should be unchanged after a failed append");
    }

    // --- integration test (requires DE440s download) ---

    #[test]
    #[ignore = "requires DE440s ephemeris (~150 MB download on first run, then cached)"]
    fn test_celestial_snapshot_real_almanac() {
        let almanac = anise::prelude::MetaAlmanac::latest()
            .expect("MetaAlmanac::latest() should succeed");

        let epoch = j2000_tai();
        let batch = celestial_snapshot(&almanac, CelestialBody::ALL, epoch)
            .expect("snapshot should succeed with a loaded almanac");

        assert_eq!(batch.num_rows(), 10, "one row per body");

        // Schema sanity
        assert!(batch.schema().field_with_name("entity_id").is_ok());
        assert!(batch.schema().field_with_name("spacetimestamp").is_ok());
        assert!(batch.schema().field_with_name("velocity").is_ok());
        assert!(batch.schema().field_with_name("mass_kg").is_ok());

        // All velocity rows should be non-null (planetary state vectors always have velocity)
        let vel = batch.column_by_name("velocity").unwrap();
        assert_eq!(vel.null_count(), 0, "all bodies should have velocity");

        // All mass rows should be non-null
        let mass = batch.column_by_name("mass_kg").unwrap();
        assert_eq!(mass.null_count(), 0, "all bodies should have mass");

        // Angular velocity should be non-null (PCK includes rotation rate derivatives)
        let ang_vel = batch.column_by_name("angular_velocity").unwrap();
        assert_eq!(ang_vel.null_count(), 0, "all bodies should have angular velocity from PCK");

        // Earth is ~1 AU from SSB (within a factor of 2 for a rough check)
        use arrow::array::{FixedSizeListArray, Float64Array, StructArray};
        let sts = batch
            .column_by_name("spacetimestamp")
            .unwrap()
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let pos_list = sts
            .column_by_name("position")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        let pos_vals = pos_list
            .values()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();

        // Earth is row 3 (Sun, Mercury, Venus, Earth, ...)
        let earth_row = 3;
        let base = (pos_list.offset() + earth_row) * 3;
        let x = pos_vals.value(base);
        let y = pos_vals.value(base + 1);
        let z = pos_vals.value(base + 2);
        let dist_km = (x * x + y * y + z * z).sqrt();
        let dist_au = dist_km / 149_597_870.7;
        assert!(
            (0.9..=1.1).contains(&dist_au),
            "Earth should be ~1 AU from SSB at J2000, got {dist_au:.4} AU"
        );

        // Earth's orientation at J2000 is not the identity — the IAU rotation model
        // encodes Earth's ~23.4° axial tilt and prime-meridian angle.
        let quat_list = sts
            .column_by_name("quaternion")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        let quat_vals = quat_list
            .values()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let base = (quat_list.offset() + earth_row) * 4;
        let qw = quat_vals.value(base);
        let qx = quat_vals.value(base + 1);
        let qy = quat_vals.value(base + 2);
        let qz = quat_vals.value(base + 3);
        assert!(
            !(qx.abs() < 1e-9 && qy.abs() < 1e-9 && qz.abs() < 1e-9),
            "Earth orientation at J2000 should not be identity, got [{qw}, {qx}, {qy}, {qz}]"
        );
    }
}
