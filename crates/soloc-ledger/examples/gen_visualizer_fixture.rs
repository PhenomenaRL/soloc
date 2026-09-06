//! Generates the ledger fixture consumed by the `visualizer/` front end.
//!
//! Writes `visualizer/public/data/dummy.arrows` (plus its `.names.arrow` sibling):
//! a 7-day window, 2026-08-01 → 2026-08-08 TAI, of entity rows.
//!
//! The celestial half is **real**: the Sun, the eight planets and the Moon come
//! straight from [`celestial_snapshot`] against a NAIF almanac, so positions,
//! velocities, body-fixed orientations, angular velocities and masses are the
//! ones anise resolves — no invented orbits. The demo half is synthetic, but it
//! is *hung off* those real states: an asteroid, a surveyed Moon base with a
//! rover driving away from it, a spaceship that performs a trans-lunar injection
//! to where the Moon actually is, and an asteroid miner that docks in
//! millimetres.
//!
//! The base gives the tree its deepest chain — `rover → base → Moon → ICRF` —
//! and the base carries a real local-level orientation, so the rover's stored
//! coordinates are plain east/north/up metres from the front door.
//!
//! Two scripted re-parenting events are the payload:
//!
//! * `demo:spaceship-1` re-parents Earth → Moon at T+84 h
//! * `demo:miner-1` re-parents ICRF → `demo:asteroid-1` (docking, mm) at T+120 h
//!
//! Both hand-offs are continuous *in world space*: each trajectory is shaped in
//! an inertial frame and only then expressed in whichever parent's coordinates
//! the row declares, so the re-parent changes the numbers without moving the
//! spacecraft.
//!
//! All rows go through a real [`Ledger`], so the fixture is validated by the same
//! frame checks, topology ingest, and cycle detection as production data. The file
//! is then reloaded and its topology re-derived as a round-trip self-test.
//!
//! # Ephemeris data
//!
//! Kernels resolve from `SOLOC_KERNEL_PATHS` (colon-separated) when set, otherwise
//! from `MetaAlmanac::latest()`, which downloads DE440s + PCK (~150 MB) on first
//! run and caches them. Unlike `soloc-server`, this example *requires* an almanac:
//! there is nothing to generate without one.
//!
//! Run with: `cargo run -p soloc-ledger --example gen_visualizer_fixture`

use std::f64::consts::TAU;
use std::path::PathBuf;

use anise::almanac::Almanac;
use anise::almanac::metaload::MetaAlmanac;
use arrow::array::{Int16Array, UInt64Array};
use hifitime::{Epoch, TimeScale, Unit};
use nalgebra::{Matrix3, Quaternion, Rotation3, UnitQuaternion, Vector3};
use soloc_ledger::ephemeris::celestial_snapshot;
use soloc_ledger::ledger::Ledger;
use soloc_ledger::schemas::entity::{EntityBuilder, EntitySchema};
use spacetimestamp::ephemeris::celestial_state;
use spacetimestamp::identity::{ASTRO_AUTHORITY, id_at};
use spacetimestamp::{
    EstimateType, LengthUnit, PrescribedId, TimeScaleCode, epoch_from_parts, epoch_to_parts,
};

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

/// Hours in the fixture window: 7 days inclusive of both endpoints.
const HOURS: u32 = 169;

/// The spaceship's Earth → Moon re-parent, in hours since window start.
const TLI_HANDOFF_H: u32 = 84;

/// The miner's ICRF → asteroid re-parent (docking), in hours since window start.
const DOCK_HANDOFF_H: u32 = 120;

// ---------------------------------------------------------------------------
// Celestial bodies
// ---------------------------------------------------------------------------

/// A body the fixture snapshots from the almanac.
///
/// `ephemeris`/`orientation` are the anise pair the body's [`PrescribedId`] embeds; a body's
/// own id *is* its IAU body-fixed frame, which is why the rover can sit in `IAU_MOON` and the
/// spaceship in `IAU_EARTH` without either frame needing a row of its own.
///
/// `cadence_h` is how often the body is sampled. It is a *rendering* choice, not a physical
/// one: a body that hosts another entity must carry an orientation sample fine enough to
/// interpolate, so Earth — which turns 15° an hour and hosts the spaceship — is sampled every
/// hour, while Mars is a dot on a 12-hour cadence.
struct Body {
    ephemeris: i32,
    orientation: i32,
    cadence_h: u32,
}

/// Sun, the eight planets and the Moon.
///
/// Each body is anchored at whichever point the **base DE440s kernels actually carry**, which
/// is what keeps the fixture reproducible from a bare checkout:
///
/// * Sun, Mercury, Venus, Earth and the Moon have body centres in DE440s, so they are recorded
///   at `(naif, naif)` — their own IAU body-fixed frame, carrying a real orientation and
///   angular velocity. That is what lets the rover ride a Moon that turns.
/// * From **Mars outward** DE440s carries only the system barycentre; a body centre would
///   additionally need a satellite SPK (`mar097.bsp`, `jup365.bsp`, …). Those are recorded at
///   `(n, 1)`. The offset is the moons' share of system mass — tens of metres for Mars, a few
///   hundred kilometres for the giants, far below one pixel at solar-system zoom. Being
///   barycentres they are inertial, so they carry an identity orientation.
///
/// Kept as a one-line-per-body table; rustfmt would explode it and the point is the columns.
#[rustfmt::skip]
const BODIES: &[Body] = &[
    Body { ephemeris:  10, orientation:  10, cadence_h: 12 }, // Sun
    Body { ephemeris: 199, orientation: 199, cadence_h: 12 }, // Mercury
    Body { ephemeris: 299, orientation: 299, cadence_h: 12 }, // Venus
    Body { ephemeris: 399, orientation: 399, cadence_h:  1 }, // Earth   — hosts the spaceship
    Body { ephemeris: 301, orientation: 301, cadence_h:  1 }, // Moon    — hosts the rover
    Body { ephemeris:   4, orientation:   1, cadence_h: 12 }, // Mars barycentre
    Body { ephemeris:   5, orientation:   1, cadence_h: 12 }, // Jupiter barycentre
    Body { ephemeris:   6, orientation:   1, cadence_h: 12 }, // Saturn barycentre
    Body { ephemeris:   7, orientation:   1, cadence_h: 12 }, // Uranus barycentre
    Body { ephemeris:   8, orientation:   1, cadence_h: 12 }, // Neptune barycentre
];

// ---------------------------------------------------------------------------
// Demo entities
// ---------------------------------------------------------------------------

/// The authority every demo entity and demo source is minted under.
const DEMO: &str = "demo";

/// The asteroid's synthetic circular orbit. There is no real ephemeris to draw on here, so
/// this one body stays invented — it is the docking target, not a claim about the sky.
///
/// Irregular extents rather than a sphere: at 4.1e12 kg this is a rubble pile, not a body big
/// enough for gravity to have rounded it.
const ASTEROID_RADIUS_KM: f64 = 3.3e8;
const ASTEROID_PERIOD_DAYS: f64 = 1200.0;
const ASTEROID_PHASE: f64 = 2.1;
const ASTEROID_INCL_DEG: f64 = 8.0;
const ASTEROID_MASS_KG: f64 = 4.1e12;
const ASTEROID_DIMS_M: [f64; 3] = [92.0, 78.0, 64.0];

/// The miner's fixed approach/dock direction in the asteroid's frame (unit vector).
const DOCK_DIR: [f64; 3] = [0.585, -0.683, 0.439];

/// Hull extents in metres for the crewed and robotic demo entities. Chosen to sit sensibly
/// against their masses, and small enough that the miner's 100 m final standoff still clears
/// the asteroid.
const ROVER_DIMS_M: [f64; 3] = [3.0, 2.3, 2.2]; //     899 kg — Perseverance class
const SHIP_DIMS_M: [f64; 3] = [7.0, 5.0, 5.0]; //   12 500 kg — Orion class
const MINER_DIMS_M: [f64; 3] = [5.5, 3.2, 3.2]; //   3 400 kg
const BASE_DIMS_M: [f64; 3] = [24.0, 18.0, 7.5]; // 42 000 kg — a habitat and its landing pad

/// Mean lunar radius in metres — where the base's foundations are.
const R_MOON_M: f64 = 1_737_400.0;

/// Where the base was surveyed, in selenographic degrees.
const BASE_LAT_DEG: f64 = 5.0;
const BASE_LON_DEG: f64 = -20.0;

/// How fast the rover drives east, metres per second. About 12 km over the
/// window — a sane week for something Perseverance-sized.
const ROVER_EAST_M_S: f64 = 0.02;

/// The spaceship's selenocentric state at the moment of capture, km.
///
/// Phase A blends *to* this point (expressed relative to the real Moon) and phase B starts
/// *from* it, which is what makes the Earth → Moon hand-off continuous in world space.
const CAPTURE_OFFSET_KM: [f64; 3] = [19_800.0, 0.0, 0.0];

// ---------------------------------------------------------------------------
// Small math helpers
// ---------------------------------------------------------------------------

/// Position on the asteroid's circular orbit at `t_days` since window start, km in ICRF.
///
/// ICRF here is solar-system-barycentric, and the Sun sits within ~0.005 au of the SSB, so a
/// circle about the origin is a heliocentric orbit to well inside its own line width.
fn asteroid_pos_km(t_days: f64) -> [f64; 3] {
    let th = ASTEROID_PHASE + TAU * t_days / ASTEROID_PERIOD_DAYS;
    let (s, c) = th.sin_cos();
    let (ic, is) = (
        ASTEROID_INCL_DEG.to_radians().cos(),
        ASTEROID_INCL_DEG.to_radians().sin(),
    );
    [
        ASTEROID_RADIUS_KM * c,
        ASTEROID_RADIUS_KM * s * ic,
        ASTEROID_RADIUS_KM * s * is,
    ]
}

/// Velocity on that same orbit, m/s — the entity schema stores rates in SI.
fn asteroid_vel_m_s(t_days: f64) -> [f64; 3] {
    let th = ASTEROID_PHASE + TAU * t_days / ASTEROID_PERIOD_DAYS;
    let (s, c) = th.sin_cos();
    let (ic, is) = (
        ASTEROID_INCL_DEG.to_radians().cos(),
        ASTEROID_INCL_DEG.to_radians().sin(),
    );
    // km/s tangential speed → m/s.
    let w = 1000.0 * TAU * ASTEROID_RADIUS_KM / (ASTEROID_PERIOD_DAYS * 86_400.0);
    [-w * s, w * c * ic, w * c * is]
}

/// The base's pose in the Moon's body-fixed frame: where it stands, and which way is up.
///
/// Returns `(position_m, orientation)` where the orientation takes a vector from the base's
/// **local-level** axes — x east, y north, z up — into Moon body-fixed axes. Storing the base
/// this way is what makes its child's coordinates readable: the rover's row is plain
/// east/north/up metres from the front door, not a selenographic vector that has to be
/// unpicked before it means anything.
fn base_pose() -> ([f64; 3], [f64; 4]) {
    let (lat, lon) = (BASE_LAT_DEG.to_radians(), BASE_LON_DEG.to_radians());
    let (sin_lat, cos_lat) = lat.sin_cos();
    let (sin_lon, cos_lon) = lon.sin_cos();

    // The standard ENU triad at (lat, lon) on a sphere. `east × north = up`, so the three
    // columns form a right-handed rotation matrix rather than a mirrored one.
    let east = Vector3::new(-sin_lon, cos_lon, 0.0);
    let north = Vector3::new(-sin_lat * cos_lon, -sin_lat * sin_lon, cos_lat);
    let up = Vector3::new(cos_lat * cos_lon, cos_lat * sin_lon, sin_lat);

    let q = UnitQuaternion::from_rotation_matrix(&Rotation3::from_matrix_unchecked(
        Matrix3::from_columns(&[east, north, up]),
    ));
    let p = up * R_MOON_M;
    ([p.x, p.y, p.z], [q.w, q.i, q.j, q.k])
}

/// `[w, x, y, z]` unit quaternion for a rotation of `angle` about +Z.
fn yaw_quat(angle: f64) -> [f64; 4] {
    [(angle / 2.0).cos(), 0.0, 0.0, (angle / 2.0).sin()]
}

/// Hermite smoothstep on [0, 1].
fn smoothstep(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Upper-triangle row-major 6×6 state covariance with diagonal `pos_var` (×3) then
/// `vel_var` (×3); off-diagonals zero.
fn cov21(pos_var: f64, vel_var: f64) -> [f64; 21] {
    let mut c = [0.0; 21];
    let diag = [0, 6, 11, 15, 18, 20];
    for i in 0..3 {
        c[diag[i]] = pos_var;
    }
    for i in 3..6 {
        c[diag[i]] = vel_var;
    }
    c
}

const IDENTITY_Q: [f64; 4] = [1.0, 0.0, 0.0, 0.0];

// ---------------------------------------------------------------------------
// Real body states
// ---------------------------------------------------------------------------

/// One body's real state at one epoch, in the shape this generator needs.
struct BodyState {
    /// Body centre relative to the SSB, km in ICRF.
    position_km: Vector3<f64>,
    /// Rotation taking a vector from the body-fixed frame *into* ICRF.
    rotation: UnitQuaternion<f64>,
}

impl BodyState {
    /// Expresses an ICRF-relative offset from this body's centre in its body-fixed frame.
    ///
    /// The whole reason the demo trajectories can be shaped inertially and still be *stored*
    /// in a rotating frame: shape the curve in world space, then push it through here.
    fn to_body_fixed(&self, offset_icrf_km: Vector3<f64>) -> [f64; 3] {
        let v = self.rotation.inverse_transform_vector(&offset_icrf_km);
        [v.x, v.y, v.z]
    }
}

/// Queries one body's real state, or explains which kernel is missing.
fn body_state(almanac: &Almanac, id: PrescribedId, epoch: Epoch) -> Result<BodyState, String> {
    let cs = celestial_state(almanac, id, epoch)?;
    let [w, x, y, z] = cs.orientation;
    Ok(BodyState {
        position_km: Vector3::new(cs.position_km[0], cs.position_km[1], cs.position_km[2]),
        rotation: UnitQuaternion::from_quaternion(Quaternion::new(w, x, y, z)),
    })
}

// ---------------------------------------------------------------------------
// Almanac
// ---------------------------------------------------------------------------

/// Loads kernels from `SOLOC_KERNEL_PATHS`, else downloads via `MetaAlmanac::latest()`.
///
/// Mirrors `soloc-server`'s resolution order minus the config file, but *fails* where the
/// server degrades: a server with no kernels is still a useful server, whereas this example
/// exists only to write real ephemeris to disk.
fn load_almanac() -> Result<Almanac, String> {
    if let Ok(paths) = std::env::var("SOLOC_KERNEL_PATHS")
        && !paths.trim().is_empty()
    {
        let mut almanac = Almanac::default();
        for path in paths.split(':').filter(|p| !p.is_empty()) {
            almanac = almanac
                .load(path)
                .map_err(|e| format!("failed to load kernel '{path}': {e}"))?;
            println!("loaded kernel {path}");
        }
        return Ok(almanac);
    }

    println!("no SOLOC_KERNEL_PATHS set — falling back to MetaAlmanac::latest()");
    println!("(first run downloads DE440s + PCK, ~150 MB, cached under ~/.local/share/nyx-space)");
    MetaAlmanac::latest().map_err(|e| {
        format!(
            "MetaAlmanac::latest() failed: {e}. Point SOLOC_KERNEL_PATHS at a local DE440s \
             SPK and a PCK (e.g. /kernels/de440s.bsp:/kernels/pck11.pca) and re-run."
        )
    })
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Prints a derived topology batch, resolving ids to names through the ledger's registry.
fn print_topology(label: &str, ledger: &Ledger) -> Result<(), String> {
    let batch = ledger.export_topology()?;
    let get = |name: &str| {
        batch
            .column_by_name(name)
            .ok_or_else(|| format!("topology batch missing '{name}'"))
    };
    let child = spacetimestamp::identity::as_id_column(get("child_id")?, "child_id")?;
    let parent = spacetimestamp::identity::as_id_column(get("parent_id")?, "parent_id")?;
    let cen = get("duration_centuries")?
        .as_any()
        .downcast_ref::<Int16Array>()
        .ok_or("duration_centuries is not Int16")?;
    let ns = get("duration_ns")?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or("duration_ns is not UInt64")?;

    println!("{label} ({} events):", batch.num_rows());
    for i in 0..batch.num_rows() {
        let epoch = epoch_from_parts(cen.value(i), ns.value(i), TimeScale::TAI);
        println!(
            "  {:<18} ← {:<18} @ {}",
            ledger.names().display(id_at(child, i)?),
            ledger.names().display(id_at(parent, i)?),
            epoch
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<(), String> {
    let almanac = load_almanac()?;
    let t0 = Epoch::from_gregorian(2026, 8, 1, 0, 0, 0, 0, TimeScale::TAI);

    // --- ids -------------------------------------------------------------
    let icrf = PrescribedId::astronomical_from_name("ICRF")?;
    let earth = PrescribedId::astronomical(399, 399)?;
    let moon = PrescribedId::astronomical(301, 301)?;

    let asteroid = PrescribedId::new(DEMO, "asteroid-1")?;
    let base = PrescribedId::new(DEMO, "moon-base-1")?;
    let rover = PrescribedId::new(DEMO, "rover-1")?;
    let ship = PrescribedId::new(DEMO, "spaceship-1")?;
    let miner = PrescribedId::new(DEMO, "miner-1")?;

    let survey_net = PrescribedId::abstract_source(DEMO, "survey-net")?;
    let base_survey = PrescribedId::abstract_source(DEMO, "base-1-survey")?;
    let rover_imu = PrescribedId::abstract_source(DEMO, "rover-1-imu")?;
    let gs_madrid = PrescribedId::abstract_source(DEMO, "gs-madrid")?;
    let miner_nav = PrescribedId::abstract_source(DEMO, "miner-1-nav")?;

    let body_ids: Vec<PrescribedId> = BODIES
        .iter()
        .map(|b| PrescribedId::astronomical(b.ephemeris, b.orientation))
        .collect::<Result<_, _>>()?;

    let mut ledger = Ledger::for_schema::<EntitySchema>()?;

    // --- names ------------------------------------------------------------
    // Display only — nothing below reads them. Registered here so `save_ipc` writes a
    // `.names.arrow` sibling and the front end can label a 16-byte id without shipping a
    // hard-coded table of its own. Astronomical names are verified against the canonical
    // frame table on insert, so a typo is rejected rather than mislabelled.
    for &id in &body_ids {
        let name = id
            .astro_frame()
            .and_then(|(e, o)| spacetimestamp::ephemeris::frame_name(e, o))
            .ok_or_else(|| format!("{id} has no canonical frame name"))?;
        ledger.register_name(id, ASTRO_AUTHORITY, name)?;
    }
    ledger.register_name(icrf, ASTRO_AUTHORITY, "ICRF")?;
    ledger.register_name(asteroid, DEMO, "asteroid-1")?;
    ledger.register_name(base, DEMO, "moon-base-1")?;
    ledger.register_name(rover, DEMO, "rover-1")?;
    ledger.register_name(ship, DEMO, "spaceship-1")?;
    ledger.register_name(miner, DEMO, "miner-1")?;
    ledger.register_name(survey_net, DEMO, "survey-net")?;
    ledger.register_name(base_survey, DEMO, "base-1-survey")?;
    ledger.register_name(rover_imu, DEMO, "rover-1-imu")?;
    ledger.register_name(gs_madrid, DEMO, "gs-madrid")?;
    ledger.register_name(miner_nav, DEMO, "miner-1-nav")?;
    ledger.register_name(
        PrescribedId::abstract_source("anise", "almanac")?,
        "anise",
        "almanac",
    )?;

    // --- rows -------------------------------------------------------------
    let mut celestial_rows = 0usize;
    let mut demo_rows = 0usize;

    for h in 0..HOURS {
        let t_d = f64::from(h) / 24.0;
        let epoch = t0 + f64::from(h) * Unit::Hour;
        let (cen, ns) = epoch_to_parts(epoch);

        // Real bodies, straight from the almanac, at each body's own cadence.
        let due: Vec<PrescribedId> = BODIES
            .iter()
            .zip(&body_ids)
            .filter(|(b, _)| h % b.cadence_h == 0)
            .map(|(_, &id)| id)
            .collect();
        if !due.is_empty() {
            let batch = celestial_snapshot(&almanac, &due, epoch)?;
            celestial_rows += batch.num_rows();
            ledger.append(batch)?;
        }

        // The two real bodies the demo entities hang off. Queried every hour regardless of
        // the sampling cadence above: these drive *where the synthetic curves go*, which is
        // a different question from how often the bodies themselves are recorded.
        let earth_state = body_state(&almanac, earth, epoch)?;
        let moon_state = body_state(&almanac, moon, epoch)?;
        let moon_geo_km = moon_state.position_km - earth_state.position_km;

        let mut b = EntityBuilder::new(5);

        // --- asteroid: synthetic, ICRF, hourly ---------------------------
        let ast_pos = asteroid_pos_km(t_d);
        b.append_entity(
            asteroid,
            icrf,
            LengthUnit::km,
            TimeScaleCode::TAI,
            survey_net,
            EstimateType::SIMULATED,
            ast_pos,
            IDENTITY_Q,
            cen,
            ns,
            Some(asteroid_vel_m_s(t_d)),
            None,
            None,
            Some(ASTEROID_MASS_KG),
            None,
            Some(ASTEROID_DIMS_M),
        );

        // --- moon base: surveyed, stationary, body-fixed on the real Moon --
        // Never moves in the Moon's frame, so every row is identical — a fixed
        // installation is exactly the case where "store raw" costs nothing. The
        // Moon's own orientation rows carry the rotation, so the base rides it
        // for free, and so does everything parented to the base.
        let (base_pos_m, base_quat) = base_pose();
        b.append_entity(
            base,
            moon,
            LengthUnit::m,
            TimeScaleCode::TAI,
            base_survey,
            EstimateType::MEASURED,
            base_pos_m,
            base_quat,
            cen,
            ns,
            None,
            None,
            None,
            Some(42_000.0),
            None,
            Some(BASE_DIMS_M),
        );

        // --- rover: a child of the base, in local-level metres -------------
        // East/north/up from the front door. The base's own orientation rows do
        // the work of turning that into a selenographic position, and the Moon's
        // do the work of turning *that* into an ICRF one — which is the whole
        // point of a three-deep chain.
        let east_m = ROVER_EAST_M_S * f64::from(h) * 3_600.0;
        // The local level is a tangent plane, so driving straight along it would
        // walk the rover off the Moon. Dropping by d²/2R keeps its wheels on the
        // surface to well under a metre over this distance.
        let drop_m = east_m * east_m / (2.0 * R_MOON_M);
        b.append_entity(
            rover,
            base,
            LengthUnit::m,
            TimeScaleCode::TAI,
            rover_imu,
            EstimateType::MEASURED,
            [east_m, 40.0 * (0.03 * f64::from(h)).sin(), -drop_m],
            yaw_quat(0.002 * f64::from(h)),
            cen,
            ns,
            None,
            Some([0.0, 0.0, 9.7e-8]),
            None,
            Some(899.0),
            None,
            Some(ROVER_DIMS_M),
        );

        // --- spaceship: LEO spiral → trans-lunar injection → lunar orbit --
        // Shaped inertially, stored body-fixed. Phase A blends a geocentric spiral onto the
        // real Moon's position plus CAPTURE_OFFSET_KM, reaching it a few hours *before* the
        // re-parent; phase B starts from that same offset. The two therefore describe one
        // continuous world-space path across the hand-off, even though the stored numbers
        // jump from Earth-fixed to Moon-fixed coordinates.
        let (ship_frame, ship_pos) = if h < TLI_HANDOFF_H {
            let x = f64::from(h) / f64::from(TLI_HANDOFF_H);
            let r = 7_000.0 + 90_000.0 * x.powf(1.4);
            let th = 30.0 * x.powf(0.55);
            let spiral = Vector3::new(r * th.cos(), r * th.sin(), 0.05 * r * (2.0 * th).sin());
            let target = moon_geo_km + Vector3::from_row_slice(&CAPTURE_OFFSET_KM);
            // Reaches the target with 6 h to spare, so the last rows before the re-parent
            // already track the Moon exactly.
            let s = smoothstep(f64::from(h) / f64::from(TLI_HANDOFF_H - 6));
            (earth, earth_state.to_body_fixed(spiral.lerp(&target, s)))
        } else {
            let y = f64::from(h - TLI_HANDOFF_H) / f64::from(TLI_HANDOFF_H);
            let r = 18_000.0 * (1.0 - y) + 1_800.0;
            let th = 14.0 * y;
            let selenocentric = Vector3::new(r * th.cos(), r * th.sin(), 0.1 * r * th.sin());
            (moon, moon_state.to_body_fixed(selenocentric))
        };
        b.append_entity(
            ship,
            ship_frame,
            LengthUnit::km,
            TimeScaleCode::TAI,
            gs_madrid,
            EstimateType::ESTIMATED,
            ship_pos,
            yaw_quat(0.05 * f64::from(h)),
            cen,
            ns,
            None,
            None,
            None,
            Some(12_500.0),
            None,
            Some(SHIP_DIMS_M),
        );

        // --- miner: heliocentric chase, then docked in millimetres --------
        // The chase ends at 60.1 km — exactly the first docked row — so this hand-off is
        // continuous in the asteroid's own frame.
        let (miner_frame, miner_units, miner_pos, miner_cov) = if h < DOCK_HANDOFF_H {
            let u = f64::from(h) / f64::from(DOCK_HANDOFF_H);
            let d_km = 60.1 + 1.1e6 * (1.0 - u).powi(2);
            (
                icrf,
                LengthUnit::km,
                [
                    ast_pos[0] + DOCK_DIR[0] * d_km,
                    ast_pos[1] + DOCK_DIR[1] * d_km,
                    ast_pos[2] + DOCK_DIR[2] * d_km,
                ],
                None,
            )
        } else {
            let v = f64::from(h - DOCK_HANDOFF_H) / 48.0;
            // 60.1 km (the chase hand-off distance) down to 100 m, in mm.
            let d_mm = 6.0e7 * (1.0 - v).powi(2) + 1.0e5;
            let cov = (h % 6 == 0).then(|| cov21(2.5e4, 1.0e-2)); // mm², (mm/s)²
            (
                asteroid,
                LengthUnit::mm,
                [DOCK_DIR[0] * d_mm, DOCK_DIR[1] * d_mm, DOCK_DIR[2] * d_mm],
                cov,
            )
        };
        b.append_entity(
            miner,
            miner_frame,
            miner_units,
            TimeScaleCode::TAI,
            miner_nav,
            EstimateType::MEASURED,
            miner_pos,
            yaw_quat(-0.3),
            cen,
            ns,
            None,
            None,
            None,
            Some(3_400.0),
            miner_cov,
            Some(MINER_DIMS_M),
        );

        let batch = b.flush();
        demo_rows += batch.num_rows();
        ledger.append(batch)?;
    }

    // --- write ------------------------------------------------------------
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../visualizer/public/data");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;
    let out_path = out_dir.join("dummy.arrows");
    ledger.save_ipc(&out_path)?;

    let size_kib = std::fs::metadata(&out_path)
        .map(|m| m.len() as f64 / 1024.0)
        .unwrap_or(0.0);
    println!(
        "\n✓ wrote {} ({size_kib:.1} KiB, {} rows: {celestial_rows} celestial + {demo_rows} demo)",
        out_path.display(),
        celestial_rows + demo_rows,
    );
    println!(
        "✓ wrote {} ({} names)",
        soloc_ledger::ledger::names_sibling_path(&out_path).display(),
        ledger.names().len(),
    );

    print_topology("\nDerived topology", &ledger)?;

    // --- round-trip self-test ---------------------------------------------
    // Reload the file and re-derive topology from rows alone. Every body and demo entity
    // contributes one event for its first parent, and the two scripted re-parents add one
    // each.
    let reloaded = Ledger::load_ipc(&out_path, "entity_id")?;
    let topo = reloaded.export_topology()?;
    let expected_events = BODIES.len() + 5 /* demo entities */ + 2 /* re-parents */;
    if topo.num_rows() != expected_events {
        return Err(format!(
            "round-trip topology mismatch: expected {expected_events} events, got {}",
            topo.num_rows()
        ));
    }
    println!(
        "\n✓ round-trip reload OK: {expected_events} topology events re-derived \
         (incl. spaceship Earth→Moon and miner ICRF→asteroid re-parents)"
    );
    Ok(())
}
