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

use anise::prelude::Almanac;
use arrow::record_batch::RecordBatch;
use hifitime::Epoch;
use spacetimestamp::ephemeris::{epoch_to_parts, query_celestial_state, query_naif_state};

use crate::schemas::entity::EntityBuilder;
use crate::ledger::Ledger;

// Re-export CelestialBody so existing callers (`soloc::ephemeris::CelestialBody`) are unaffected.
pub use spacetimestamp::ephemeris::CelestialBody;

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
///   IAU body-fixed frame to ICRF.
/// - **Angular velocity**: body spin in ICRF (rad/s), present when the PCK includes derivatives.
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
        let cs = query_celestial_state(almanac, body, epoch)?;

        builder.append_entity(
            body.entity_id(),
            "ICRF",
            "km",
            "TAI",
            "naif:de440s",
            "MEASURED",
            cs.position_km,
            cs.orientation,
            centuries,
            ns,
            Some(cs.velocity_km_s),
            cs.angular_velocity,
            None,
            cs.mass_kg,
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
        let cs = query_naif_state(almanac, naif_id, epoch)
            .map_err(|e| format!("NAIF ID {naif_id} ({entity_id}): {e}"))?;

        builder.append_entity(
            entity_id,
            "ICRF",
            "km",
            "TAI",
            "anise",
            "MEASURED",
            cs.position_km,
            cs.orientation,
            centuries,
            ns,
            Some(cs.velocity_km_s),
            cs.angular_velocity,
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
    ledger.append(batch)?;
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
    ledger.append(batch)?;
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
    use spacetimestamp::ephemeris::{epoch_to_parts, j2000_tai};

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
        assert!((CelestialBody::Earth.gm_km3_s2() - 3.986_004_418e5).abs() < 1.0);
        assert!((CelestialBody::Sun.gm_km3_s2() - 1.327_124_400_419_393e11).abs() < 1e6);
        assert!((CelestialBody::Moon.gm_km3_s2() - 4.904_869_5e3).abs() < 1.0);
    }

    // --- epoch_to_parts round-trip (now tests the canonical implementation) ---

    #[test]
    fn test_epoch_to_parts_j2000_is_zero() {
        use std::str::FromStr;
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
        assert_eq!(recovered, target);
    }

    #[test]
    fn test_epoch_to_parts_before_j2000() {
        let j2000 = j2000_tai();
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
        let mut ledger = crate::ledger::Ledger::new(
            &crate::schemas::entity::entity_schema(None),
            "entity_id",
        )
        .unwrap();
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

        assert!(batch.schema().field_with_name("entity_id").is_ok());
        assert!(batch.schema().field_with_name("spacetimestamp").is_ok());
        assert!(batch.schema().field_with_name("velocity").is_ok());
        assert!(batch.schema().field_with_name("mass_kg").is_ok());

        let vel = batch.column_by_name("velocity").unwrap();
        assert_eq!(vel.null_count(), 0, "all bodies should have velocity");

        let mass = batch.column_by_name("mass_kg").unwrap();
        assert_eq!(mass.null_count(), 0, "all bodies should have mass");

        let ang_vel = batch.column_by_name("angular_velocity").unwrap();
        assert_eq!(ang_vel.null_count(), 0, "all bodies should have angular velocity from PCK");

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
