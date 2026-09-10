//! Helpers for ephemeris/astronomical functionality.
//!
//! This module provides the single source of truth for the J2000 TAI reference epoch,
//! the duration-encoding helpers used throughout the spacetimestamp schema, the
//! [`CelestialBody`] catalog, and query functions that extract state vectors from an
//! [`anise::almanac::Almanac`].
//!
//! Two output shapes are available, in increasing structure:
//!
//! - [`celestial_state`]: a raw [`CelestialState`] for one KIND_ASTRO body id, not Arrow.
//! - [`celestial_snapshot`]: a full entity batch for a list of body ids, carrying `entity_id`,
//!   velocity, and mass, ready for [`crate::topology::TransformTree`] or a `soloc` ledger.

use anise::constants::celestial_objects::{
    EARTH, JUPITER, MARS, MERCURY, MOON, NEPTUNE, SATURN, SUN, URANUS, VENUS,
};
use anise::constants::frames::SSB_J2000;
use anise::prelude::{Almanac, Frame};
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, Epoch, TimeScale};
use nalgebra::{Rotation3, UnitQuaternion};
use std::str::FromStr;

use crate::identity::PrescribedId;

use crate::schemas::entity::EntityBuilder;
use crate::vocabulary::{EstimateType, LengthUnit, TimeScaleCode};

// ---------------------------------------------------------------------------
// Canonical astronomical frames
// ---------------------------------------------------------------------------

/// The single source of truth mapping an astronomical frame name to its anise
/// `(ephemeris_id, orientation_id)` pair, and defining which pairs a [`PrescribedId`] may
/// embed.
///
/// A KIND_ASTRO id embeds one of these pairs rather than hashing a name, so this table is
/// the mint gate. No kernel load req. Names are byte-exact and case-sensitive.
///
/// Curation: a bare body name resolves to that body's IAU body-fixed frame `(naif, naif)`,
/// so `"Earth"` and `"IAU_EARTH"` are one id. Bodies with no PCK body-fixed model, and pure
/// reference frames, stay inertial `(naif, 1)`.
pub(crate) const ASTRO_FRAMES: &[(&str, i32, i32)] = &[
    // Reference / inertial frames (SSB- or Earth-centred, J2000 orientation id 1).
    ("ICRF", 0, 1),
    ("J2000", 0, 1),
    ("SSB", 0, 1),
    ("GCRF", 399, 1),
    ("EME2000", 399, 1),
    ("EMB", 3, 1),
    // Inertial `(naif, 1)`: a barycentre is a point, not a body, so it has no body-fixed
    // orientation to name. The Earth-Moon barycentre is `EMB`, above.
    ("MERCURY_BARYCENTER", 1, 1),
    ("VENUS_BARYCENTER", 2, 1),
    ("MARS_BARYCENTER", 4, 1),
    ("JUPITER_BARYCENTER", 5, 1),
    ("SATURN_BARYCENTER", 6, 1),
    ("URANUS_BARYCENTER", 7, 1),
    ("NEPTUNE_BARYCENTER", 8, 1),
    ("PLUTO_BARYCENTER", 9, 1),
    // The ten well-known bodies: bare name = body-fixed (naif, naif).
    ("Sun", 10, 10),
    ("Mercury", 199, 199),
    ("Venus", 299, 299),
    ("Earth", 399, 399),
    ("Moon", 301, 301),
    ("Mars", 499, 499),
    ("Jupiter", 599, 599),
    ("Saturn", 699, 699),
    ("Uranus", 799, 799),
    ("Neptune", 899, 899),
    // Bodies with no PCK body-fixed model stay inertial (naif, 1).
    ("Pluto", 999, 1),
    ("Phobos", 401, 1),
    ("Deimos", 402, 1),
    ("Io", 501, 1),
    ("Europa", 502, 1),
    ("Ganymede", 503, 1),
    ("Callisto", 504, 1),
    ("Titan", 606, 1),
    ("Enceladus", 602, 1),
    // IAU body-fixed frames (naif, naif). The ten bodies above already carry these pairs
    // under their bare names; these add the IAU_ spelling and the bodies outside the ten.
    ("IAU_SUN", 10, 10),
    ("IAU_MERCURY", 199, 199),
    ("IAU_VENUS", 299, 299),
    ("IAU_EARTH", 399, 399),
    ("IAU_MOON", 301, 301),
    ("IAU_MARS", 499, 499),
    ("IAU_JUPITER", 599, 599),
    ("IAU_SATURN", 699, 699),
    ("IAU_URANUS", 799, 799),
    ("IAU_NEPTUNE", 899, 899),
    ("IAU_PLUTO", 999, 999),
    ("IAU_CHARON", 901, 901),
    ("IAU_PHOBOS", 401, 401),
    ("IAU_DEIMOS", 402, 402),
    ("IAU_IO", 501, 501),
    ("IAU_EUROPA", 502, 502),
    ("IAU_GANYMEDE", 503, 503),
    ("IAU_CALLISTO", 504, 504),
    ("IAU_MIMAS", 601, 601),
    ("IAU_ENCELADUS", 602, 602),
    ("IAU_TETHYS", 603, 603),
    ("IAU_DIONE", 604, 604),
    ("IAU_RHEA", 605, 605),
    ("IAU_TITAN", 606, 606),
    ("IAU_IAPETUS", 608, 608),
    ("IAU_ARIEL", 701, 701),
    ("IAU_UMBRIEL", 702, 702),
    ("IAU_TITANIA", 703, 703),
    ("IAU_OBERON", 704, 704),
    ("IAU_MIRANDA", 705, 705),
    ("IAU_TRITON", 801, 801),
];

// ---------------------------------------------------------------------------
// Epoch helpers
// ---------------------------------------------------------------------------

/// Returns the J2000 TAI reference epoch: 2000-01-01T12:00:00 TAI.
///
/// All `duration_centuries` / `duration_ns` fields in the spacetimestamp schema are
/// offsets from this epoch.
pub fn j2000_tai() -> Epoch {
    Epoch::from_str("2000-01-01T12:00:00 TAI").expect("J2000 TAI is a valid epoch string")
}

/// Returns the J2000 reference epoch in the given timescale.
///
/// `(duration_centuries, duration_ns)` stored with `timescale_id` equal to `ts` are
/// SI-second offsets from this calendar moment. `2000-01-01T12:00:00` in that timescale.
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
// Astronomical frame resolution
// ---------------------------------------------------------------------------

/// Resolves an astronomical frame name to its anise `(ephemeris_id, orientation_id)` pair.
///
/// The convenience half of the resolver, for callers that still hold a name
pub fn frame_pair(name: &str) -> Option<(i32, i32)> {
    ASTRO_FRAMES
        .iter()
        .find(|(frame_name, _, _)| *frame_name == name)
        .map(|(_, e, o)| (*e, *o))
}

/// Returns `true` if `(ephemeris_id, orientation_id)` is a recognised astronomical frame.
///
/// The KIND_ASTRO mint gate: it accepts exactly the pairs in [`ASTRO_FRAMES`], rejecting
/// nonsense combinations like `(399, 499)` that a component-wise check would pass.
pub fn recognised(ephemeris_id: i32, orientation_id: i32) -> bool {
    ASTRO_FRAMES
        .iter()
        .any(|(_, e, o)| *e == ephemeris_id && *o == orientation_id)
}

/// The canonical display name for an `(ephemeris_id, orientation_id)` pair, or `None` if the
/// pair is not a recognised astronomical frame.
///
/// The reverse of [`frame_pair`]
pub fn frame_name(ephemeris_id: i32, orientation_id: i32) -> Option<&'static str> {
    ASTRO_FRAMES
        .iter()
        .find(|(_, e, o)| *e == ephemeris_id && *o == orientation_id)
        .map(|(name, _, _)| *name)
}

/// Every astronomical frame name a fresh deployment can name without setup.
pub fn registrable_frame_names() -> impl Iterator<Item = &'static str> {
    ASTRO_FRAMES.iter().map(|(name, _, _)| *name)
}

/// Resolves a bare astronomical frame name to an anise [`Frame`].
///
/// Thin wrapper over [`frame_pair`]. Kept for the name-based `transform_batch` target path
/// until frame resolution moves to ids.
pub(crate) fn resolve_astronomical_frame(name: &str) -> Option<Frame> {
    frame_pair(name).map(|(e, o)| Frame::new(e, o))
}

// ---------------------------------------------------------------------------
// CelestialBody
// ---------------------------------------------------------------------------

/// Well-known solar system bodies whose ephemeris is available in the DE440 SPK family.
///
/// A convenience catalog for naming the ten bodies; each maps to a NAIF body center ID via
/// [`naif_id`](Self::naif_id) and to its `(naif, naif)` body-fixed id via
/// [`entity_id`](Self::entity_id). State comes from [`celestial_state`], not from here.
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
    /// By NAIF convention this also serves as the IAU orientation ID: the body's IAU
    /// body-fixed frame is `(naif_id, naif_id)`, which is what [`entity_id`](Self::entity_id)
    /// embeds.
    pub fn naif_id(self) -> i32 {
        match self {
            CelestialBody::Sun => SUN,         // 10
            CelestialBody::Mercury => MERCURY, // 199
            CelestialBody::Venus => VENUS,     // 299
            CelestialBody::Earth => EARTH,     // 399
            CelestialBody::Moon => MOON,       // 301
            CelestialBody::Mars => MARS,       // 499
            CelestialBody::Jupiter => JUPITER, // 599
            CelestialBody::Saturn => SATURN,   // 699
            CelestialBody::Uranus => URANUS,   // 799
            CelestialBody::Neptune => NEPTUNE, // 899
        }
    }

    /// [`PrescribedId`] for this body as stored in the soloc ledger.
    ///
    /// A body is its own IAU body-fixed frame `(naif, naif)`, so `Earth` and `IAU_EARTH` are
    /// one id. Any producer that starts from the same NAIF integer arrives at the same id
    /// without having to agree with anyone.
    pub fn entity_id(self) -> PrescribedId {
        PrescribedId::astronomical(self.naif_id(), self.naif_id())
            .expect("a body's (naif, naif) frame is in the canonical table")
    }
}

/// Gravitational constant G in km³/(kg·s²), for converting a GM (km³/s²) to a mass (kg).
const G_KM3_KG_S2: f64 = 6.674e-20;

// ---------------------------------------------------------------------------
// CelestialState
// ---------------------------------------------------------------------------

/// Raw state vector returned by ephemeris queries.
///
/// All fields are in ICRF relative to the Solar System Barycentre (SSB). Units are mixed and
/// each field name carries its own: position stays km, anise's native unit and the
/// astrodynamics convention, while the rates are SI to match the entity schema. The duration
/// fields are the standard spacetimestamp offset from J2000 TAI.
///
/// This struct is schema-neutral. It carries no Arrow dependency.
#[derive(Debug)]
pub struct CelestialState {
    pub position_km: [f64; 3],
    pub velocity_m_s: [f64; 3],
    pub orientation: [f64; 4],
    pub angular_velocity_rad_s: Option<[f64; 3]>,
    pub mass_kg: Option<f64>,
    pub duration_centuries: i16,
    pub duration_ns: u64,
}

/// Converts an anise velocity, which is always km/s, to the m/s the entity schema stores.
///
/// The one place this conversion happens; putting it at the `append_entity` call sites instead
/// would reintroduce the duplicated-table shape the vocabulary work exists to remove.
fn velocity_to_m_s(km_s: [f64; 3]) -> [f64; 3] {
    km_s.map(|v| LengthUnit::convert(v, LengthUnit::km, LengthUnit::m))
}

// ---------------------------------------------------------------------------
// Query function
// ---------------------------------------------------------------------------

/// Queries the almanac for a KIND_ASTRO body `id` and returns a raw [`CelestialState`].
///
/// The id embeds the anise `(ephemeris_id, orientation_id)` pair (see
/// [`PrescribedId::astro_frame`](crate::identity::PrescribedId::astro_frame)); the frame is
/// reconstructed from it and queried directly, so this needs no name registry.
///
/// - Position and velocity come from `translate` (body center relative to SSB).
/// - Orientation is the rotation from the body-fixed frame to ICRF, from `rotate`.
/// - Angular velocity is the PCK rotation derivative, present only when the PCK carries one.
/// - Mass is `GM / G` with GM from the loaded planetary data ([`Almanac::frame_info`]).
///
/// There is **no silent fallback**: a missing position, orientation, or GM is an error.
/// ICRF, SSB frames have no GM and are never body-queried, so they do not reach here.
///
/// # Errors
///
/// - `id` is not a KIND_ASTRO id.
/// - The almanac cannot resolve position, orientation, or GM (a kernel or PCK is missing).
pub fn celestial_state(
    almanac: &Almanac,
    id: PrescribedId,
    epoch: Epoch,
) -> Result<CelestialState, String> {
    let (ephemeris_id, orientation_id) = id.astro_frame().ok_or_else(|| {
        format!("{id} is not an astronomical id; celestial_state needs a KIND_ASTRO body")
    })?;
    let frame = Frame::new(ephemeris_id, orientation_id);
    let uid = format!("({ephemeris_id}, {orientation_id})");

    let state = almanac
        .translate(frame, SSB_J2000, epoch, None)
        .map_err(|e| {
            format!(
                "no position for {uid} at {epoch}: {e}. Inner planets require DE440; outer \
             planets additionally need a satellite SPK (e.g. jup365.bsp for Jupiter)."
            )
        })?;

    let dcm = almanac.rotate(frame, SSB_J2000, epoch).map_err(|e| {
        format!(
            "no orientation for {uid} at {epoch}: {e}. Ensure a PCK (e.g. pck11.pca) is loaded."
        )
    })?;
    let q = UnitQuaternion::from_rotation_matrix(&Rotation3::from_matrix_unchecked(dcm.rot_mat));
    // The PCK rotation derivative is already rad/s; no conversion, only the name.
    let angular_velocity_rad_s = dcm.rot_mat_dt.map(|r_dt| {
        let omega = r_dt * dcm.rot_mat.transpose();
        [omega[(2, 1)], omega[(0, 2)], omega[(1, 0)]]
    });

    let gm_km3_s2 = almanac
        .frame_info(frame)
        .map_err(|e| format!("no GM for {uid}: {e}. Ensure a PCK (e.g. pck11.pca) is loaded."))?
        .mu_km3_s2()
        .map_err(|e| format!("no GM for {uid}: {e}"))?;

    let (duration_centuries, duration_ns) = epoch_to_parts(epoch);

    Ok(CelestialState {
        position_km: [state.radius_km.x, state.radius_km.y, state.radius_km.z],
        velocity_m_s: velocity_to_m_s([
            state.velocity_km_s.x,
            state.velocity_km_s.y,
            state.velocity_km_s.z,
        ]),
        orientation: [q.w, q.i, q.j, q.k],
        angular_velocity_rad_s,
        mass_kg: Some(gm_km3_s2 / G_KM3_KG_S2),
        duration_centuries,
        duration_ns,
    })
}

// ---------------------------------------------------------------------------
// Ids used by the snapshot builders
// ---------------------------------------------------------------------------

/// The id of the ICRF frame every snapshot row is expressed in.
fn icrf_id() -> PrescribedId {
    PrescribedId::astronomical(0, 1).expect("ICRF (0, 1) is in the canonical table")
}

/// The source id every snapshot row carries: the rows are resolved through the almanac.
///
/// [`KIND_ABSTRACT`](crate::identity::KIND_ABSTRACT): a kernel is a data source, never a
/// frame, so handing this to frame resolution is rejected rather than resolved.
fn anise_source_id() -> PrescribedId {
    PrescribedId::abstract_source("anise", "almanac").expect("static source name is valid")
}

// ---------------------------------------------------------------------------
// Entity snapshots
// ---------------------------------------------------------------------------

/// Queries the almanac for each KIND_ASTRO body `id` at `epoch` and returns a standard entity
/// [`RecordBatch`], following [`crate::schemas::entity::entity_schema`].
///
/// Each id is queried through its embedded `(ephemeris_id, orientation_id)` frame via
/// [`celestial_state`] and stored under that same id, so a body is recorded under its own
/// canonical astronomical id (`Earth` = `IAU_EARTH` = `(399, 399)`); there is no separate
/// query-vs-store id. A body list is therefore just a list of `(naif, naif)` ids, e.g.
/// `CelestialBody::ALL.iter().map(|b| b.entity_id())`.
///
/// All rows use `frame_id = ICRF`, `units_pos = km`, `timescale_id = TAI`,
/// `source_id = anise:almanac`, `estimate_type = MEASURED`. Position/velocity, orientation,
/// angular velocity, and mass come from [`celestial_state`] (see it for the strict, no-fallback
/// contract). The batch is schema-compatible with any other entity batch and appends directly
/// to a `soloc` ledger.
///
/// # Data requirements
///
/// - **Inner planets** (Sun..Mars, Moon): resolved by `MetaAlmanac::latest()` (DE440 + pck11.pca).
/// - **Outer planets** (Jupiter..Neptune): body-center position additionally needs a satellite
///   SPK (e.g. `jup365.bsp` for Jupiter); its absence is a clean per-body error.
///
/// # Errors
///
/// Returns `Err` if `ids` is empty, an id is not a KIND_ASTRO body, or the almanac cannot
/// resolve a body's position, orientation, or GM.
pub fn celestial_snapshot(
    almanac: &Almanac,
    ids: &[PrescribedId],
    epoch: Epoch,
) -> Result<RecordBatch, String> {
    if ids.is_empty() {
        return Err("bodies list is empty — provide at least one astronomical body id".to_string());
    }

    let (centuries, ns) = epoch_to_parts(epoch);
    let (icrf, source) = (icrf_id(), anise_source_id());
    let mut builder = EntityBuilder::new(ids.len());

    for &id in ids {
        let cs = celestial_state(almanac, id, epoch)?;

        builder.append_entity(
            id,
            icrf,
            LengthUnit::km,
            TimeScaleCode::TAI,
            source,
            EstimateType::MEASURED,
            cs.position_km,
            cs.orientation,
            centuries,
            ns,
            Some(cs.velocity_m_s),
            cs.angular_velocity_rad_s,
            None,
            cs.mass_kg,
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

    /// Every pair in the canonical table must be recognised by its own mint gate, and every
    /// `IAU_*` frame must be body-fixed `(naif, naif)`. Catches a future edit that adds a name
    /// without a self-consistent pair.
    #[test]
    fn test_astro_frames_are_self_consistent() {
        for &(name, e, o) in ASTRO_FRAMES {
            assert!(recognised(e, o), "{name} = ({e}, {o}) is not recognised");
            assert_eq!(frame_pair(name), Some((e, o)), "{name} resolves wrong");
            if name.starts_with("IAU_") {
                assert_eq!(e, o, "{name}: IAU body-fixed frame must be (naif, naif)");
            }
        }
    }

    #[test]
    fn test_bare_body_name_is_its_body_fixed_frame() {
        // Decision B: a bare body name and its IAU_ spelling are one frame, one id.
        assert_eq!(frame_pair("Earth"), Some((399, 399)));
        assert_eq!(frame_pair("Earth"), frame_pair("IAU_EARTH"));
        assert_eq!(
            PrescribedId::astronomical(399, 399).unwrap(),
            PrescribedId::astronomical(399, 399).unwrap(),
        );
    }

    #[test]
    fn test_frame_pair_is_case_sensitive_and_rejects_unknowns() {
        assert_eq!(frame_pair("ICRF"), Some((0, 1)));
        assert_eq!(frame_pair("icrf"), None, "names are byte-exact");
        assert_eq!(frame_pair("NOT_A_FRAME"), None);
    }

    #[test]
    fn test_recognised_rejects_nonsense_pairs() {
        // Both components are valid NAIF ids, but the combination is not a real frame.
        assert!(
            !recognised(399, 499),
            "(Earth ephem, Mars orient) is nonsense"
        );
        assert!(PrescribedId::astronomical(399, 499).is_err());
    }

    /// Startup registration mints every name it registers, so an entry that cannot be minted
    /// would be registered nowhere and fail at transform time. The exact failure the static
    /// list exists to prevent. Fails loudly if a future entry is added that `PrescribedId`
    /// will not accept.
    #[test]
    fn test_every_registrable_frame_name_is_mintable() {
        let failures: Vec<&str> = registrable_frame_names()
            .filter(|name| {
                frame_pair(name)
                    .map(|(e, o)| PrescribedId::astronomical(e, o).is_err())
                    .unwrap_or(true)
            })
            .collect();
        assert!(
            failures.is_empty(),
            "these names are registered at startup but cannot be minted: {failures:?}"
        );
    }

    #[test]
    fn test_j2000_tai_is_correct() {
        let j2000 = j2000_tai();
        // J2000 is 2000-01-01T12:00:00 TAI: epoch_to_parts should give (0, 0).
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

    /// anise reports velocity in km/s and the entity schema stores m/s, so this must scale
    /// *up* by 1000. The snapshot test that reads a real velocity is `#[ignore]`d currently
    #[test]
    fn velocity_converts_km_s_to_m_s() {
        assert_eq!(velocity_to_m_s([1.0, -2.5, 0.0]), [1000.0, -2500.0, 0.0]);

        // Earth's orbital speed is ~29.78 km/s, i.e. ~29 780 m/s — not ~0.02978.
        let [vx, _, _] = velocity_to_m_s([29.78, 0.0, 0.0]);
        assert!((vx - 29_780.0).abs() < 1e-9, "got {vx}");
    }

    #[test]
    fn test_j2000_utc_differs_from_j2000_tai() {
        // TAI leads UTC by 32 s in 2000, so the TAI clock reads noon 32 s before the UTC clock.
        // J2000 UTC (noon UTC) is therefore a physical moment 32 s AFTER J2000 TAI (noon TAI).
        let tai = j2000_tai();
        let utc = j2000_in_timescale(TimeScale::UTC);
        let diff_s = (tai - utc).to_seconds();
        // tai < utc in physical time, so (tai − utc) ≈ −32 s.
        assert!(
            (diff_s + 32.0).abs() < 1.0,
            "J2000 UTC should be ~32s after J2000 TAI, got diff = {diff_s}s"
        );
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
        assert!(
            (diff_s + 32.0).abs() < 1.0,
            "UTC parts should be ~32s less than TAI parts, got {diff_s}s"
        );
    }

    #[test]
    fn test_celestial_body_naif_ids() {
        assert_eq!(CelestialBody::Earth.naif_id(), 399);
        assert_eq!(CelestialBody::Moon.naif_id(), 301);
        assert_eq!(CelestialBody::Sun.naif_id(), 10);
    }

    #[test]
    fn test_entity_ids_are_unique() {
        let ids: Vec<PrescribedId> = CelestialBody::ALL.iter().map(|b| b.entity_id()).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len(), "duplicate entity IDs detected");
    }

    #[test]
    fn test_entity_ids_are_their_body_fixed_frame() {
        for body in CelestialBody::ALL {
            let id = body.entity_id();
            let naif = body.naif_id();
            assert_eq!(id.astro_frame(), Some((naif, naif)), "{body:?} pair");
            assert_eq!(
                id,
                PrescribedId::astronomical(naif, naif).unwrap(),
                "{body:?} id is its (naif, naif) frame",
            );
            assert!(id.is_astro(), "{body:?} id should be KIND_ASTRO");
        }
    }

    #[test]
    fn test_celestial_state_fails_without_kernels() {
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let err = celestial_state(&almanac, CelestialBody::Earth.entity_id(), epoch).unwrap_err();
        assert!(
            err.contains("position") || err.contains("SPK"),
            "error should guide user to load a kernel; got: {err}"
        );
    }

    #[test]
    fn test_celestial_state_rejects_a_non_astro_id() {
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let soloc = PrescribedId::new("acme.com", "truck_A").unwrap();
        let err = celestial_state(&almanac, soloc, epoch).unwrap_err();
        assert!(
            err.contains("astronomical") || err.contains("KIND_ASTRO"),
            "a non-astro id should be rejected before any kernel query; got: {err}"
        );
    }

    // --- Entity snapshots ---

    #[test]
    fn test_empty_bodies_returns_error() {
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let err = celestial_snapshot(&almanac, &[], epoch).unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn test_no_kernel_returns_descriptive_error() {
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let err =
            celestial_snapshot(&almanac, &[CelestialBody::Earth.entity_id()], epoch).unwrap_err();
        assert!(
            err.contains("SPK") || err.contains("position"),
            "error should guide user to load a kernel; got: {err}"
        );
    }

    // --- integration test (requires DE440s download) ---

    #[test]
    #[ignore = "requires DE440s ephemeris (~150 MB download on first run, then cached)"]
    fn test_celestial_snapshot_real_almanac() {
        use arrow::array::Array;

        let almanac =
            anise::prelude::MetaAlmanac::latest().expect("MetaAlmanac::latest() should succeed");

        let epoch = j2000_tai();
        let ids: Vec<PrescribedId> = CelestialBody::ALL.iter().map(|b| b.entity_id()).collect();
        let batch = celestial_snapshot(&almanac, &ids, epoch)
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
        assert_eq!(
            ang_vel.null_count(),
            0,
            "all bodies should have angular velocity from PCK"
        );

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
