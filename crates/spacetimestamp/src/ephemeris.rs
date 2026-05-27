//! Canonical epoch helpers and raw ephemeris query functions.
//!
//! This module provides the single source of truth for the J2000 TAI reference epoch,
//! the duration-encoding helpers used throughout the spacetimestamp schema, the
//! [`CelestialBody`] catalog, and schema-neutral query functions that extract raw
//! state vectors from an [`anise::Almanac`] without coupling to any Arrow schema.
//!
//! Callers that need Arrow output (e.g. entity batches) should use `soloc::ephemeris`.

use anise::constants::celestial_objects::{
    EARTH, JUPITER, JUPITER_BARYCENTER, MARS, MERCURY, MOON, NEPTUNE, NEPTUNE_BARYCENTER,
    SATURN, SATURN_BARYCENTER, SUN, URANUS, URANUS_BARYCENTER, VENUS,
};
use anise::constants::frames::SSB_J2000;
use anise::prelude::{Almanac, Frame};
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, Epoch, TimeScale};
use nalgebra::{Rotation3, UnitQuaternion};
use std::str::FromStr;

use crate::schema::SpaceTimestampBuilder;

// ---------------------------------------------------------------------------
// Epoch helpers
// ---------------------------------------------------------------------------

/// Returns the J2000 TAI reference epoch: 2000-01-01T12:00:00 TAI.
///
/// This is the single canonical definition used by every module in this workspace.
/// All `duration_centuries` / `duration_ns` fields in the spacetimestamp schema are
/// offsets from this epoch.
pub fn j2000_tai() -> Epoch {
    Epoch::from_str("2000-01-01T12:00:00 TAI").expect("J2000 TAI is a valid epoch string")
}

/// Returns the J2000 reference epoch in the given timescale.
///
/// `(duration_centuries, duration_ns)` stored with `timescale_id` equal to `ts` are
/// SI-second offsets from this calendar moment — `2000-01-01T12:00:00` in that timescale.
/// Each timescale's J2000 is a different physical moment (e.g. J2000 UTC is 32 SI seconds
/// earlier than J2000 TAI due to the leap-second offset in 2000).
pub fn j2000_in_timescale(ts: TimeScale) -> Epoch {
    Epoch::from_gregorian(2000, 1, 1, 12, 0, 0, 0, ts)
}

/// Reconstructs a physical [`Epoch`] from stored `(duration_centuries, duration_ns)` and
/// the timescale that was used as the J2000 reference when those values were computed.
///
/// This is the read-side complement to `epoch_to_parts_in`: given the stored raw integers
/// and their declared `timescale_id`, return the unambiguous physical moment as a hifitime
/// `Epoch` (internally always TAI-equivalent), suitable for almanac queries or comparison.
pub fn epoch_from_parts(centuries: i16, ns: u64, ts: TimeScale) -> Epoch {
    j2000_in_timescale(ts) + Duration::from_parts(centuries, ns)
}

/// Converts an [`Epoch`] to the `(duration_centuries, duration_ns)` offset from J2000 TAI
/// used throughout the spacetimestamp schema.
pub fn epoch_to_parts(epoch: Epoch) -> (i16, u64) {
    (epoch - j2000_tai()).to_parts()
}

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
    pub fn mass_kg(self) -> f64 {
        const G_KM3_KG_S2: f64 = 6.674e-20;
        self.gm_km3_s2() / G_KM3_KG_S2
    }
}

// ---------------------------------------------------------------------------
// CelestialState
// ---------------------------------------------------------------------------

/// Raw state vector returned by ephemeris queries.
///
/// All fields are in ICRF relative to the Solar System Barycentre (SSB),
/// in km and km/s. The duration fields are the standard spacetimestamp
/// offset from J2000 TAI.
///
/// This struct is schema-neutral — it carries no Arrow dependency.
/// Callers in `soloc::ephemeris` convert it into entity [`RecordBatch`]es.
///
/// [`RecordBatch`]: arrow::record_batch::RecordBatch
#[derive(Debug)]
pub struct CelestialState {
    pub position_km: [f64; 3],
    pub velocity_km_s: [f64; 3],
    /// Rotation from the body's IAU body-fixed frame to ICRF, as `[w, x, y, z]`.
    pub orientation: [f64; 4],
    pub angular_velocity: Option<[f64; 3]>,
    pub mass_kg: Option<f64>,
    pub duration_centuries: i16,
    pub duration_ns: u64,
}

// ---------------------------------------------------------------------------
// Query functions
// ---------------------------------------------------------------------------

/// Queries the almanac for a well-known [`CelestialBody`] and returns a raw [`CelestialState`].
///
/// - Position and velocity come from DE440 (body center relative to SSB).
/// - Orientation is the rotation from the IAU body-fixed frame to ICRF (from the loaded PCK).
/// - Angular velocity is derived from the PCK rotation derivative when available.
/// - Mass is populated from the DE440/IAU 2012 GM constants on [`CelestialBody`].
///
/// # Errors
///
/// Returns `Err` if the almanac cannot resolve position or orientation for the body.
pub fn query_celestial_state(
    almanac: &Almanac,
    body: CelestialBody,
    epoch: Epoch,
) -> Result<CelestialState, String> {
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
    let angular_velocity = dcm.rot_mat_dt.map(|r_dt| {
        let omega = r_dt * dcm.rot_mat.transpose();
        [omega[(2, 1)], omega[(0, 2)], omega[(1, 0)]]
    });

    let (duration_centuries, duration_ns) = epoch_to_parts(epoch);

    Ok(CelestialState {
        position_km: [state.radius_km.x, state.radius_km.y, state.radius_km.z],
        velocity_km_s: [state.velocity_km_s.x, state.velocity_km_s.y, state.velocity_km_s.z],
        orientation: [q.w, q.i, q.j, q.k],
        angular_velocity,
        mass_kg: Some(body.mass_kg()),
        duration_centuries,
        duration_ns,
    })
}

/// Queries the almanac for an arbitrary NAIF body by integer ID and returns a raw [`CelestialState`].
///
/// Unlike [`query_celestial_state`], orientation silently falls back to the identity quaternion
/// and `angular_velocity` to `None` when the loaded PCK has no rotation model for the body —
/// so this succeeds for any body that has SPK position data, regardless of PCK coverage.
/// Mass is not populated (unknown for arbitrary bodies).
///
/// # Errors
///
/// Returns `Err` if the almanac cannot resolve the position of the body (SPK data missing).
pub fn query_naif_state(
    almanac: &Almanac,
    naif_id: i32,
    epoch: Epoch,
) -> Result<CelestialState, String> {
    let iau = Frame::new(naif_id, naif_id);

    let state = almanac
        .translate(iau, SSB_J2000, epoch, None)
        .map_err(|e| format!(
            "Failed to get position for NAIF ID {naif_id} at {epoch}: {e}.",
        ))?;

    let (orientation, angular_velocity) = match almanac.rotate(iau, SSB_J2000, epoch) {
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

    let (duration_centuries, duration_ns) = epoch_to_parts(epoch);

    Ok(CelestialState {
        position_km: [state.radius_km.x, state.radius_km.y, state.radius_km.z],
        velocity_km_s: [state.velocity_km_s.x, state.velocity_km_s.y, state.velocity_km_s.z],
        orientation,
        angular_velocity,
        mass_kg: None,
        duration_centuries,
        duration_ns,
    })
}

// ---------------------------------------------------------------------------
// STS snapshot
// ---------------------------------------------------------------------------

/// Queries the ephemeris for the given bodies at `epoch` and returns a plain
/// SpaceTimestamp [`RecordBatch`].
///
/// Each row contains position, orientation, and time in ICRF/TAI/km — the core
/// spatiotemporal data only. There are no schema-specific fields (velocity, mass,
/// entity_id). Use `soloc::ephemeris::celestial_snapshot` when you need those.
///
/// # Errors
/// Returns `Err` if `bodies` is empty or the almanac cannot resolve any body.
pub fn celestial_sts_snapshot(
    almanac: &Almanac,
    bodies: &[CelestialBody],
    epoch: Epoch,
) -> Result<RecordBatch, String> {
    if bodies.is_empty() {
        return Err("bodies list is empty — provide at least one CelestialBody".to_string());
    }
    let (centuries, ns) = epoch_to_parts(epoch);
    let mut builder = SpaceTimestampBuilder::new(bodies.len(), None);
    for &body in bodies {
        let cs = query_celestial_state(almanac, body, epoch)?;
        builder.append_spacetimestamp(
            "ICRF",
            "km",
            "TAI",
            "naif:de440s",
            "MEASURED",
            cs.position_km,
            cs.orientation,
            centuries,
            ns,
            None,
            None,
        );
    }
    Ok(builder.flush())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use hifitime::Duration;

    #[test]
    fn test_j2000_tai_is_correct() {
        let j2000 = j2000_tai();
        // J2000 is 2000-01-01T12:00:00 TAI — epoch_to_parts should give (0, 0).
        let (c, n) = epoch_to_parts(j2000);
        assert_eq!(c, 0);
        assert_eq!(n, 0);
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
        assert_eq!(recovered, before);
    }

    #[test]
    fn test_j2000_in_timescale_tai_matches_j2000_tai() {
        // J2000 in TAI must exactly equal j2000_tai().
        assert_eq!(j2000_in_timescale(TimeScale::TAI), j2000_tai());
    }

    #[test]
    fn test_j2000_utc_differs_from_j2000_tai() {
        // TAI leads UTC by 32 s in 2000, so the TAI clock reads noon 32 s before the UTC clock.
        // J2000 UTC (noon UTC) is therefore a physical moment 32 s AFTER J2000 TAI (noon TAI).
        let tai = j2000_tai();
        let utc = j2000_in_timescale(TimeScale::UTC);
        let diff_s = (tai - utc).to_seconds();
        // tai < utc in physical time, so (tai − utc) ≈ −32 s.
        assert!((diff_s + 32.0).abs() < 1.0, "J2000 UTC should be ~32s after J2000 TAI, got diff = {diff_s}s");
    }

    #[test]
    fn test_epoch_from_parts_tai_round_trips() {
        let original = j2000_tai() + Duration::from_parts(0, 500_000_000_000u64);
        let (c, n) = epoch_to_parts(original);
        let recovered = epoch_from_parts(c, n, TimeScale::TAI);
        assert_eq!(recovered, original);
    }

    #[test]
    fn test_epoch_from_parts_utc_gives_correct_physical_moment() {
        // A UTC timestamp: compute parts relative to J2000 UTC, then reconstruct.
        // The recovered Epoch should match the original physical moment.
        let j2000_utc = j2000_in_timescale(TimeScale::UTC);
        let offset = Duration::from_parts(0, 1_000_000_000u64); // 1 second
        let utc_epoch = j2000_utc + offset;
        let (c, n) = (utc_epoch - j2000_utc).to_parts();
        let recovered = epoch_from_parts(c, n, TimeScale::UTC);
        assert_eq!(recovered, utc_epoch);
    }

    #[test]
    fn test_tai_and_utc_parts_for_same_moment_differ() {
        // For the same physical moment, parts relative to J2000 TAI vs J2000 UTC differ.
        // J2000 UTC is 32 s LATER than J2000 TAI, so UTC parts are ~32 s SMALLER
        // (closer to zero) than TAI parts for any epoch after both J2000 references.
        let physical_moment = j2000_tai() + Duration::from_parts(0, 1_000_000_000u64);
        let (tai_c, tai_n) = epoch_to_parts(physical_moment);
        let utc_j2000 = j2000_in_timescale(TimeScale::UTC);
        let (utc_c, utc_n) = (physical_moment - utc_j2000).to_parts();
        let tai_ns_total = tai_c as i64 * 36_525i64 * 86_400 * 1_000_000_000 + tai_n as i64;
        let utc_ns_total = utc_c as i64 * 36_525i64 * 86_400 * 1_000_000_000 + utc_n as i64;
        // utc_parts < tai_parts because the UTC reference is later: utc_ns - tai_ns ≈ -32 s.
        let diff_s = (utc_ns_total - tai_ns_total) as f64 / 1e9;
        assert!((diff_s + 32.0).abs() < 1.0, "UTC parts should be ~32s less than TAI parts, got {diff_s}s");
    }

    #[test]
    fn test_celestial_body_naif_ids() {
        assert_eq!(CelestialBody::Earth.naif_id(), 399);
        assert_eq!(CelestialBody::Moon.naif_id(), 301);
        assert_eq!(CelestialBody::Sun.naif_id(), 10);
    }

    #[test]
    fn test_entity_ids_are_unique() {
        let ids: Vec<&str> = CelestialBody::ALL.iter().map(|b| b.entity_id()).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len(), "duplicate entity IDs detected");
    }

    #[test]
    fn test_all_bodies_have_positive_mass() {
        for body in CelestialBody::ALL {
            assert!(body.mass_kg() > 0.0, "{:?} has non-positive mass", body);
        }
    }

    #[test]
    fn test_query_celestial_state_fails_without_spk() {
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let err = query_celestial_state(&almanac, CelestialBody::Earth, epoch).unwrap_err();
        assert!(
            err.contains("MetaAlmanac") || err.contains("SPK") || err.contains("ephemeris"),
            "error should guide user to load an SPK; got: {err}"
        );
    }

    #[test]
    fn test_query_naif_state_fails_without_spk() {
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let err = query_naif_state(&almanac, 399, epoch).unwrap_err();
        assert!(
            err.contains("399") || err.contains("position") || err.contains("SPK"),
            "error should mention the NAIF ID; got: {err}"
        );
    }
}
