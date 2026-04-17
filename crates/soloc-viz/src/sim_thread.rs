use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anise::prelude::Almanac;
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, Epoch};

use soloc::ledger::Ledger;
use soloc::sim::Simulation;

pub struct SimState {
    pub snapshot: Option<RecordBatch>,
    pub current_epoch: Epoch,
    pub step_count: usize,
}

/// Runs the simulation loop in a background thread.
///
/// Reads `playing` each tick; when true, advances the simulation by `steps_per_tick` steps,
/// then pushes the latest snapshot into `state`. Sleeps 16 ms between ticks (~60 Hz).
pub fn run(
    ledger: Ledger,
    almanac: Almanac,
    dt: Duration,
    state: Arc<Mutex<SimState>>,
    playing: Arc<AtomicBool>,
    steps_per_tick: Arc<AtomicUsize>,
) {
    let mut sim = Simulation::new(ledger, almanac, dt);

    loop {
        if playing.load(Ordering::Relaxed) {
            let n = steps_per_tick.load(Ordering::Relaxed).max(1);

            for _ in 0..n {
                if let Err(e) = sim.step() {
                    eprintln!("[sim] propagation error: {e}");
                    return;
                }
            }

            let snap = sim.ledger().latest_snapshot(None);
            let epoch = sim.current_epoch;

            let mut guard = state.lock().unwrap();
            guard.snapshot = snap;
            guard.current_epoch = epoch;
            guard.step_count += n;
        }

        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}
