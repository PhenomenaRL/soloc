//! Ephemeris queries — converting solar system body positions into standard entity batches.
//!
//! [`celestial_snapshot`] is the primary entry point. It queries an [`anise::Almanac`] for the
//! ICRF positions and velocities of any selection of [`CelestialBody`] values at a given epoch,
//! and returns a standard entity [`RecordBatch`] that is schema-compatible with any other entity
//! batch and can be appended directly to a [`crate::ledger::Ledger`].
//!
//! Celestial bodies are not special — they are entities like any other, recorded with the same
//! schema as a spacecraft or robot. The only distinction is `source_id = "anise"` and
//! `estimate_type = "MEASURED"` (ephemeris data is derived from real observations).
//!
//! # Ephemeris data
//!
//! Positions come from the DE440/DE440s SPK files. Load them via `anise::MetaAlmanac::latest()`,
//! which downloads ~150 MB on first run and caches them in `~/.local/share/nyx-space/anise/`.

use anise::constants::frames::{
    EARTH_J2000, JUPITER_BARYCENTER_J2000, MARS_J2000, MERCURY_J2000, MOON_J2000,
    NEPTUNE_BARYCENTER_J2000, SATURN_BARYCENTER_J2000, SSB_J2000, SUN_J2000,
    URANUS_BARYCENTER_J2000, VENUS_J2000,
};
use anise::prelude::{Almanac, Frame};
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
/// Outer planets use their *system barycentre* frames (e.g. `JUPITER_BARYCENTER_J2000`)
/// because that is what DE440 provides natively; the offset from the planet centre is
/// negligible for most applications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CelestialBody {
    Sun,
    Mercury,
    Venus,
    Earth,
    Moon,
    Mars,
    /// Jupiter system barycentre.
    Jupiter,
    /// Saturn system barycentre.
    Saturn,
    /// Uranus system barycentre.
    Uranus,
    /// Neptune system barycentre.
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

    /// The `anise` frame constant used to query this body's ICRF state vector.
    pub fn frame(self) -> Frame {
        match self {
            CelestialBody::Sun     => SUN_J2000,
            CelestialBody::Mercury => MERCURY_J2000,
            CelestialBody::Venus   => VENUS_J2000,
            CelestialBody::Earth   => EARTH_J2000,
            CelestialBody::Moon    => MOON_J2000,
            CelestialBody::Mars    => MARS_J2000,
            CelestialBody::Jupiter => JUPITER_BARYCENTER_J2000,
            CelestialBody::Saturn  => SATURN_BARYCENTER_J2000,
            CelestialBody::Uranus  => URANUS_BARYCENTER_J2000,
            CelestialBody::Neptune => NEPTUNE_BARYCENTER_J2000,
        }
    }

    /// Canonical entity ID for this body as stored in the soloc ledger.
    pub fn entity_id(self) -> &'static str {
        match self {
            CelestialBody::Sun     => "urn:soloc:solar_system:sun",
            CelestialBody::Mercury => "urn:soloc:solar_system:mercury",
            CelestialBody::Venus   => "urn:soloc:solar_system:venus",
            CelestialBody::Earth   => "urn:soloc:solar_system:earth",
            CelestialBody::Moon    => "urn:soloc:solar_system:moon",
            CelestialBody::Mars    => "urn:soloc:solar_system:mars",
            CelestialBody::Jupiter => "urn:soloc:solar_system:jupiter",
            CelestialBody::Saturn  => "urn:soloc:solar_system:saturn",
            CelestialBody::Uranus  => "urn:soloc:solar_system:uranus",
            CelestialBody::Neptune => "urn:soloc:solar_system:neptune",
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
/// `source_id = "anise"`, and `estimate_type = "MEASURED"`. Velocity is populated from
/// the DE440 state vector. Orientation is the identity quaternion (body rotation is not
/// tracked). Mass is derived from DE440/IAU 2012 GM constants.
///
/// The returned batch is schema-compatible with any other entity batch and can be appended
/// directly to a [`Ledger`]:
///
/// ```rust,ignore
/// ledger.append(celestial_snapshot(&almanac, CelestialBody::ALL, epoch)?);
/// ```
///
/// # Errors
///
/// Returns `Err` if:
/// - `bodies` is empty.
/// - The almanac fails to resolve any body (most commonly: no SPK loaded).
///   Load one with `anise::MetaAlmanac::latest()`.
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
        let state = almanac
            .translate(body.frame(), SSB_J2000, epoch, None)
            .map_err(|e| {
                format!(
                    "Failed to get ephemeris state for {:?} at {epoch}: {e}. \
                     Ensure a planetary SPK is loaded via anise::MetaAlmanac::latest().",
                    body,
                )
            })?;

        builder.append_entity(
            body.entity_id(),
            "ICRF",
            "km",
            "TAI",
            "anise",
            "MEASURED",
            [state.radius_km.x, state.radius_km.y, state.radius_km.z],
            [1.0, 0.0, 0.0, 0.0], // identity quaternion — body rotation not tracked
            centuries,
            ns,
            Some([
                state.velocity_km_s.x,
                state.velocity_km_s.y,
                state.velocity_km_s.z,
            ]),
            None, // angular_velocity
            None, // acceleration
            Some(body.mass_kg()),
        );
    }

    Ok(builder.flush())
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
    fn test_entity_ids_are_urns() {
        for body in CelestialBody::ALL {
            let id = body.entity_id();
            assert!(
                id.starts_with("urn:soloc:solar_system:"),
                "{id:?} does not match expected URN prefix"
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
    }
}
