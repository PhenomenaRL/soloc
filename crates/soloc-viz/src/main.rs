mod app;
mod sim_thread;

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use anise::prelude::MetaAlmanac;
use anise::constants::frames::{EARTH_J2000, SSB_J2000};
use hifitime::{Duration, Epoch};

use soloc::entity::EntityBuilder;
use soloc::ledger::Ledger;

use app::SolVizApp;
use sim_thread::SimState;

fn main() -> eframe::Result<()> {
    // Block here until ephemeris data is ready. On first run this downloads ~150 MB
    // (DE440s + PCK files) to ~/.local/share/nyx-space/anise/ and is reused on every
    // subsequent run via CRC32-checked cache.
    println!("Loading ephemeris data (downloads ~150 MB on first run, then cached)...");
    let almanac = MetaAlmanac::latest().expect("Failed to load almanac");
    println!("Almanac ready.");

    let j2000 = Epoch::from_str("2000-01-01T12:00:00 TAI").unwrap();

    // Query Earth's actual ICRF position and velocity at J2000 from DE440s.
    // translate(from, to, epoch) → radius_km is the `from` origin in `to` coordinates.
    let earth_state = almanac
        .translate(EARTH_J2000, SSB_J2000, j2000, None)
        .expect("DE440s should contain Earth state at J2000");

    let earth_pos = [
        earth_state.radius_km.x,
        earth_state.radius_km.y,
        earth_state.radius_km.z,
    ];
    let earth_vel = [
        earth_state.velocity_km_s.x,
        earth_state.velocity_km_s.y,
        earth_state.velocity_km_s.z,
    ];

    // Place a spacecraft in LEO: offset +6778 km along X from Earth,
    // with Earth's orbital velocity plus ~7.668 km/s LEO velocity along Y.
    let sc_pos = [earth_pos[0] + 6_778.0, earth_pos[1], earth_pos[2]];
    let sc_vel = [earth_vel[0], earth_vel[1] + 7.668, earth_vel[2]];

    let mut builder = EntityBuilder::new(2, None);

    builder.append_entity(
        "urn:soloc:viz:spacecraft",
        "ICRF", "km", "TAI",
        "soloc-viz", "MEASURED",
        sc_pos,
        [1.0, 0.0, 0.0, 0.0],
        0, 0,
        Some(sc_vel),
        None, None,
        Some(1_000.0),
    );

    builder.append_entity(
        "urn:soloc:viz:earth",
        "ICRF", "km", "TAI",
        "soloc-viz", "MEASURED",
        earth_pos,
        [1.0, 0.0, 0.0, 0.0],
        0, 0,
        Some(earth_vel),
        None, None,
        Some(5.972e24),
    );

    let initial = builder.flush();

    let mut ledger = Ledger::new();
    ledger.append(initial);

    let initial_snap = ledger.latest_snapshot(None);
    let state = Arc::new(Mutex::new(SimState {
        snapshot: initial_snap,
        current_epoch: j2000,
        step_count: 0,
    }));
    let playing = Arc::new(AtomicBool::new(false));
    let steps_per_tick = Arc::new(AtomicUsize::new(1));

    {
        let state = Arc::clone(&state);
        let playing = Arc::clone(&playing);
        let steps_per_tick = Arc::clone(&steps_per_tick);
        let dt = Duration::from_parts(0, 60_000_000_000u64); // 60-second timestep
        std::thread::spawn(move || {
            sim_thread::run(ledger, almanac, dt, state, playing, steps_per_tick);
        });
    }

    eframe::run_native(
        "SoloC Orbital Viewer",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(SolVizApp {
            state,
            playing,
            steps_per_tick,
            use_au: true,       // default to AU scale — Earth is ~1 AU from SSB
            camera_entity: Some("earth".to_string()), // start Earth-relative
        }))),
    )
}
