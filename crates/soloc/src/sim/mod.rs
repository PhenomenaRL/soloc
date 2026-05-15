//! Simulation orchestrator for the soloc Universal Ledger.
//!
//! [`Simulation`] drives the N-body propagator in a step-by-step loop:
//! 1. Read the latest entity snapshot from the ledger.
//! 2. Reproject it to ICRF/km using `spacetimestamp::transforms::transform_batch`.
//! 3. Advance by one timestep with [`NBodyPropagator::step`].
//! 4. Append the result back to the ledger as `estimate_type = "SIMULATED"`.
//!
//! The ledger itself is never modified — only appended to — so the complete history
//! of observations and simulation outputs is always preserved.
//!
//! # Setup
//!
//! ```rust,ignore
//! use anise::prelude::Almanac;
//! use hifitime::{Duration, Epoch};
//! use soloc::ledger::Ledger;
//! use soloc::sim::Simulation;
//!
//! // Seed the ledger with an initial entity snapshot
//! let mut ledger = Ledger::new();
//! ledger.append(initial_entity_batch);
//!
//! // Load a real almanac with DE440 data for accurate gravity
//! let almanac = Almanac::default().load("de440s.bsp").unwrap();
//!
//! let dt = Duration::from_parts(0, 60_000_000_000); // 60-second steps
//! let mut sim = Simulation::new(ledger, almanac, dt);
//!
//! // Advance 100 steps
//! sim.run_until(start_epoch + 100 * dt).unwrap();
//! ```

pub mod integrator;
pub mod propagator;

pub use propagator::NBodyPropagator;

use anise::prelude::Almanac;
use hifitime::{Duration, Epoch};

use arrow::array::{Array, DictionaryArray, StringArray, StructArray};
use arrow::datatypes::{UInt16Type, UInt32Type};
use arrow::record_batch::RecordBatch;
use crate::ledger::Ledger;
use spacetimestamp::transforms::transform_batch;

/// Orchestrates the N-body simulation loop, reading from and writing to the [`Ledger`].
pub struct Simulation {
    /// The underlying data store. Accessible for queries after stepping.
    pub ledger: Ledger,
    propagator: NBodyPropagator,
    /// The epoch of the most recent simulation step.
    pub current_epoch: Epoch,
}

impl Simulation {
    /// Creates a new simulation wrapping the given ledger.
    ///
    /// `almanac` should have a full planetary SPK loaded (e.g. `de440s.bsp`) for realistic
    /// N-body gravity. `Almanac::default()` (no SPK) is valid for testing — entities will
    /// propagate at constant velocity because no gravitational bodies can be resolved.
    ///
    /// `dominant_body_count` is set to 5 (Sun + 4 nearest planets per entity).
    pub fn new(ledger: Ledger, almanac: Almanac, dt: Duration) -> Self {
        // Derive the starting epoch from the latest snapshot in the ledger.
        // Falls back to J2000 if the ledger is empty.
        let current_epoch = Self::epoch_from_ledger(&ledger)
            .unwrap_or_else(|| {
                use std::str::FromStr;
                Epoch::from_str("2000-01-01T12:00:00 TAI").unwrap()
            });

        let propagator = NBodyPropagator::new(almanac, dt, 5);
        Self { ledger, propagator, current_epoch }
    }

    /// Advances the simulation by one timestep.
    ///
    /// Reads the latest snapshot, reprojects to ICRF/km (if needed), propagates by `dt`,
    /// and appends the result to the ledger.
    pub fn step(&mut self) -> Result<(), String> {
        let snapshot = self
            .ledger
            .latest_snapshot(None)
            .ok_or("Ledger is empty — seed it with at least one entity batch before stepping")?;

        // Reproject to ICRF/km if any rows are not already there.
        // Skipping the transform when the batch is already in ICRF/km avoids an
        // almanac frame-resolution call (which requires a loaded SPK for "ICRF").
        // After the first sim step every batch is always ICRF/km, so this is the hot path.
        let icrf_snapshot = if is_icrf_km(&snapshot) {
            snapshot
        } else {
            transform_batch(
                &snapshot,
                "spacetimestamp",
                "ICRF",
                &self.propagator.almanac,
                "km",
                None,
            )?
        };

        let new_batch = self.propagator.step(&icrf_snapshot)?;
        self.current_epoch = self.current_epoch + self.propagator.dt;
        self.ledger.append(new_batch);

        Ok(())
    }

    /// Repeatedly calls [`step`][Self::step] until `current_epoch >= end_epoch`.
    pub fn run_until(&mut self, end_epoch: Epoch) -> Result<(), String> {
        while self.current_epoch < end_epoch {
            self.step()?;
        }
        Ok(())
    }

    /// Borrows the underlying ledger for queries.
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Extracts the starting epoch from the last row of the last batch in the ledger.
    fn epoch_from_ledger(ledger: &Ledger) -> Option<Epoch> {
        use arrow::array::{Int16Array, StructArray, UInt64Array};
        use std::str::FromStr;

        let last = ledger.latest_snapshot(None)?;
        let sts = last
            .column_by_name("spacetimestamp")?
            .as_any()
            .downcast_ref::<StructArray>()?;

        let num_rows = last.num_rows();
        if num_rows == 0 {
            return None;
        }

        let row = num_rows - 1; // use the last row
        let centuries = sts
            .column_by_name("duration_centuries")?
            .as_any()
            .downcast_ref::<Int16Array>()?
            .value(row);
        let ns = sts
            .column_by_name("duration_ns")?
            .as_any()
            .downcast_ref::<UInt64Array>()?
            .value(row);

        let j2000 = Epoch::from_str("2000-01-01T12:00:00 TAI").ok()?;
        Some(j2000 + Duration::from_parts(centuries, ns))
    }
}

/// Returns `true` if every row in the entity batch already has `frame_id = "ICRF"`
/// and `units_pos = "km"`, meaning the propagator can consume it directly without
/// calling `transform_batch`.
fn is_icrf_km(snapshot: &RecordBatch) -> bool {
    let sts = match snapshot
        .column_by_name("spacetimestamp")
        .and_then(|c| c.as_any().downcast_ref::<StructArray>())
    {
        Some(s) => s,
        None => return false,
    };

    // Check frame_id (Dictionary<UInt32, Utf8>)
    let frames = match sts
        .column_by_name("frame_id")
        .and_then(|c| c.as_any().downcast_ref::<DictionaryArray<UInt32Type>>())
    {
        Some(d) => d,
        None => return false,
    };
    let frames_dict = match frames.values().as_any().downcast_ref::<StringArray>() {
        Some(s) => s,
        None => return false,
    };
    for i in 0..frames.len() {
        if frames.is_null(i) || frames_dict.value(frames.keys().value(i) as usize) != "ICRF" {
            return false;
        }
    }

    // Check units_pos (Dictionary<UInt16, Utf8>)
    let units = match sts
        .column_by_name("units_pos")
        .and_then(|c| c.as_any().downcast_ref::<DictionaryArray<UInt16Type>>())
    {
        Some(d) => d,
        None => return false,
    };
    let units_dict = match units.values().as_any().downcast_ref::<StringArray>() {
        Some(s) => s,
        None => return false,
    };
    for i in 0..units.len() {
        if units.is_null(i) || units_dict.value(units.keys().value(i) as usize) != "km" {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Array; // needed for FixedSizeListArray::offset()
    use hifitime::Duration;

    // Re-use the same helper from ledger tests: a minimal entity-like batch
    // with a `spacetimestamp` struct column and top-level `entity_id`.
    fn make_simple_sim_batch() -> arrow::record_batch::RecordBatch {
        use crate::entity::EntityBuilder;

        let mut builder = EntityBuilder::new(2, None);

        // Entity 0: a spacecraft with velocity (will be propagated)
        builder.append_entity(
            "urn:soloc:test:ship_a",
            "ICRF",
            "km",
            "TAI",
            "test",
            "MEASURED",
            [6_578.0, 0.0, 0.0],  // ~200 km LEO altitude
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            Some([0.0, 7.784, 0.0]),  // ~circular LEO velocity km/s
            None,
            None,
            Some(1000.0),
            None,
        );

        // Entity 1: a static reference point (no velocity)
        builder.append_entity(
            "urn:soloc:test:ground_station",
            "ICRF",
            "km",
            "TAI",
            "test",
            "MEASURED",
            [6_371.0, 0.0, 0.0], // Earth surface
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None, // static
            None,
            None,
            None,
            None,
        );

        builder.flush()
    }

    #[test]
    fn test_simulation_step_produces_simulated_batch() {
        use arrow::array::{DictionaryArray, StringArray};
        use arrow::datatypes::UInt16Type;

        let mut ledger = Ledger::new();
        ledger.append(make_simple_sim_batch());

        let dt = Duration::from_parts(0, 60_000_000_000u64); // 60 seconds
        let mut sim = Simulation::new(ledger, Almanac::default(), dt);

        sim.step().expect("first step should succeed");

        assert_eq!(sim.ledger().len(), 2, "ledger should have seed + 1 step");

        // The new batch should have estimate_type = "SIMULATED"
        let last = sim.ledger().latest_snapshot(None).unwrap();
        let sts = last
            .column_by_name("spacetimestamp")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StructArray>()
            .unwrap();
        let est_col = sts
            .column_by_name("estimate_type")
            .unwrap()
            .as_any()
            .downcast_ref::<DictionaryArray<UInt16Type>>()
            .unwrap();
        let est_dict = est_col
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();

        for row in 0..last.num_rows() {
            let est = est_dict.value(est_col.keys().value(row) as usize);
            assert_eq!(est, "SIMULATED", "row {} should be SIMULATED", row);
        }
    }

    #[test]
    fn test_static_entity_position_unchanged() {
        use arrow::array::{Array, StructArray};

        let mut ledger = Ledger::new();
        ledger.append(make_simple_sim_batch());

        let dt = Duration::from_parts(0, 60_000_000_000u64);
        let mut sim = Simulation::new(ledger, Almanac::default(), dt);
        sim.step().unwrap();

        // Ground station (row 1, null velocity) should keep [6371, 0, 0]
        let last = sim.ledger().latest_snapshot(None).unwrap();
        let sts = last
            .column_by_name("spacetimestamp")
            .unwrap()
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let pos_list = sts
            .column_by_name("position")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::FixedSizeListArray>()
            .unwrap();
        let pos_vals = pos_list
            .values()
            .as_any()
            .downcast_ref::<arrow::array::Float64Array>()
            .unwrap();
        let offset = pos_list.offset();
        let row = 1; // ground station
        let base = (offset + row) * 3;
        assert!((pos_vals.value(base) - 6_371.0).abs() < 1e-9, "static entity moved");
    }

    #[test]
    fn test_run_until_advances_epoch() {
        let mut ledger = Ledger::new();
        ledger.append(make_simple_sim_batch());

        let dt = Duration::from_parts(0, 60_000_000_000u64); // 60 s
        let mut sim = Simulation::new(ledger, Almanac::default(), dt);
        let start = sim.current_epoch;

        let target = start + Duration::from_parts(0, 300_000_000_000u64); // +300 s
        sim.run_until(target).unwrap();

        // 5 steps of 60 s = 300 s
        assert_eq!(sim.ledger().len(), 6, "seed + 5 steps");
        assert!(sim.current_epoch >= target);
    }
}
