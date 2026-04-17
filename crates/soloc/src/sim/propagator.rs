//! N-body gravitational propagator.
//!
//! [`NBodyPropagator`] advances a snapshot batch of entities by one timestep using RK4.
//! At each substep it queries the [`anise::Almanac`] for the positions of the most
//! gravitationally dominant solar-system bodies and sums their accelerations.
//!
//! # Input contract
//!
//! The input [`RecordBatch`] **must** already be in ICRF frame with `units_pos = "km"`.
//! [`crate::sim::Simulation::step`] ensures this by calling `transform_batch` before
//! passing the snapshot to this propagator.
//!
//! # Gravitational model
//!
//! Standard gravitational parameters (GM) for ten solar-system bodies are hardcoded from
//! the DE440 / IAU 2012 constants. At each RK4 substep, the propagator:
//! 1. Tries to fetch each body's ICRF position from the almanac.
//! 2. Computes GM/r² (acceleration magnitude) as an influence metric.
//! 3. Keeps the top `dominant_body_count` bodies.
//! 4. Sums −GM/r³ · **r** accelerations.
//!
//! Bodies whose positions cannot be resolved (e.g., not in the loaded SPK) are silently
//! skipped. If no bodies are resolved, gravity is zero and entities drift at constant
//! velocity — load a full ephemeris (e.g., `de440s.bsp`) for realistic propagation.
//!
//! # Static entities
//!
//! Rows with a null `velocity` column are treated as static: their positions and
//! velocities are carried forward unchanged.

use anise::constants::frames::{
    EARTH_J2000, JUPITER_BARYCENTER_J2000, MARS_J2000, MERCURY_J2000, MOON_J2000,
    NEPTUNE_BARYCENTER_J2000, SATURN_BARYCENTER_J2000, SSB_J2000, SUN_J2000,
    URANUS_BARYCENTER_J2000, VENUS_J2000,
};
use anise::prelude::*;
use arrow::array::{
    Array, DictionaryArray, FixedSizeListArray, Float64Array, Int16Array, StringArray,
    StructArray, UInt64Array,
};
use arrow::datatypes::UInt32Type;
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, Epoch};
use std::str::FromStr;

use crate::entity::EntityBuilder;
use super::integrator::{rk4_step, State6};

// ---------------------------------------------------------------------------
// Gravitational constants
// ---------------------------------------------------------------------------

/// `(anise Frame constant, GM in km³/s²)` for major solar-system bodies.
///
/// Frame constants from `anise::constants::frames` are pure compile-time values —
/// no ephemeris lookup required. GMs from DE440 / IAU 2012.
///
/// Note: outer planets use their *barycentre* frames because that is what DE440 SPK
/// files provide directly; the difference from the planet centre is negligible for
/// force-model purposes.
fn body_gm_table() -> [(Frame, f64); 10] {
    [
        (SUN_J2000,               1.327_124_400_419_393e11),
        (MERCURY_J2000,           2.203_186_855_140_000_3e4),
        (VENUS_J2000,             3.248_585_920_000_000_6e5),
        (EARTH_J2000,             3.986_004_418e5),
        (MOON_J2000,              4.904_869_5e3),
        (MARS_J2000,              4.282_837_362_069_909e4),
        (JUPITER_BARYCENTER_J2000,1.266_865_34e8),
        (SATURN_BARYCENTER_J2000, 3.793_120_8e7),
        (URANUS_BARYCENTER_J2000, 5.793_951_322_279_009e6),
        (NEPTUNE_BARYCENTER_J2000,6.835_099_502_439_672e6),
    ]
}

// ---------------------------------------------------------------------------
// Helper — read [x, y, z] from a flat FixedSizeListArray values buffer
// ---------------------------------------------------------------------------

#[inline]
fn read_vec3(values: &Float64Array, list_offset: usize, row: usize) -> [f64; 3] {
    let base = (list_offset + row) * 3;
    [values.value(base), values.value(base + 1), values.value(base + 2)]
}

#[inline]
fn read_vec4(values: &Float64Array, list_offset: usize, row: usize) -> [f64; 4] {
    let base = (list_offset + row) * 4;
    [
        values.value(base),
        values.value(base + 1),
        values.value(base + 2),
        values.value(base + 3),
    ]
}

// ---------------------------------------------------------------------------
// NBodyPropagator
// ---------------------------------------------------------------------------

/// Propagates entity snapshots forward by one timestep using N-body gravity.
pub struct NBodyPropagator {
    /// The `anise` ephemeris engine. Must have a planetary SPK loaded for realistic gravity.
    pub almanac: Almanac,
    /// Timestep per simulation step.
    pub dt: Duration,
    /// How many gravitational bodies to include per entity per substep (top by GM/r²).
    pub dominant_body_count: usize,
}

impl NBodyPropagator {
    /// Creates a new propagator.
    ///
    /// `dominant_body_count` controls how many of the ten candidate bodies are included
    /// in the gravity sum. 3–5 is sufficient for most inner solar-system trajectories.
    pub fn new(almanac: Almanac, dt: Duration, dominant_body_count: usize) -> Self {
        Self { almanac, dt, dominant_body_count }
    }

    /// Returns the gravitational acceleration (km/s²) at `pos_icrf_km` at `epoch`.
    ///
    /// Fetches planetary positions from the almanac and sums contributions from the
    /// most influential bodies. Bodies that cannot be resolved (e.g., SPK not loaded)
    /// are silently skipped, resulting in zero gravity if no bodies resolve.
    fn gravitational_acceleration(&self, pos_icrf_km: [f64; 3], epoch: Epoch) -> [f64; 3] {
        let mut influences: Vec<(f64 /* GM/r² */, [f64; 3] /* accel vector */)> =
            Vec::with_capacity(10);

        for (body_frame, gm) in body_gm_table() {
            // SSB_J2000 is the ICRF reference — compile-time constant, no ephemeris needed.
            let state = match self.almanac.translate(body_frame, SSB_J2000, epoch, None) {
                Ok(s) => s,
                Err(_) => continue, // Body not covered by the loaded SPK — skip
            };

            // Vector from body to entity (in ICRF km)
            let rx = pos_icrf_km[0] - state.radius_km.x;
            let ry = pos_icrf_km[1] - state.radius_km.y;
            let rz = pos_icrf_km[2] - state.radius_km.z;
            let r2 = rx * rx + ry * ry + rz * rz;

            if r2 < 1.0 {
                continue; // Singularity guard: skip if entity is inside the body
            }

            let r  = r2.sqrt();
            let r3 = r2 * r;
            // Influence metric for ranking; acceleration vector pointing toward the body
            influences.push((gm / r2, [-gm * rx / r3, -gm * ry / r3, -gm * rz / r3]));
        }

        // Keep only the most dominant bodies
        influences.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        influences.truncate(self.dominant_body_count);

        let mut ax = 0.0_f64;
        let mut ay = 0.0_f64;
        let mut az = 0.0_f64;
        for (_, a) in &influences {
            ax += a[0];
            ay += a[1];
            az += a[2];
        }
        [ax, ay, az]
    }

    /// Propagates a single entity's position and velocity by one timestep using RK4.
    ///
    /// # Arguments
    /// * `pos_km`   — ICRF position in km.
    /// * `vel_km_s` — ICRF velocity in km/s.
    /// * `epoch`    — Epoch at the start of the step.
    ///
    /// Returns `(new_position_km, new_velocity_km_s)`.
    pub fn propagate_entity(
        &self,
        pos_km: [f64; 3],
        vel_km_s: [f64; 3],
        epoch: Epoch,
    ) -> ([f64; 3], [f64; 3]) {
        let dt_s = self.dt.to_seconds();
        let state: State6 = [
            pos_km[0], pos_km[1], pos_km[2],
            vel_km_s[0], vel_km_s[1], vel_km_s[2],
        ];

        let new_state = rk4_step(&state, epoch, dt_s, |t, pos| {
            self.gravitational_acceleration(pos, t)
        });

        (
            [new_state[0], new_state[1], new_state[2]],
            [new_state[3], new_state[4], new_state[5]],
        )
    }

    /// Propagates an entire entity snapshot batch by one timestep.
    ///
    /// The input batch must be in ICRF/km (guaranteed by [`crate::sim::Simulation::step`]).
    /// Returns a new batch with:
    /// - Propagated positions and velocities (entities with null velocity are static).
    /// - `frame_id = "ICRF"`, `units_pos = "km"`, `timescale_id = "TAI"`.
    /// - `source_id = "soloc::sim"`, `estimate_type = "SIMULATED"`.
    /// - Epoch advanced by `self.dt`.
    /// - Quaternion, angular_velocity, acceleration, and mass_kg carried forward unchanged.
    pub fn step(&self, snapshot: &RecordBatch) -> Result<RecordBatch, String> {
        let num_rows = snapshot.num_rows();
        let j2000 = Epoch::from_str("2000-01-01T12:00:00 TAI")
            .expect("J2000 TAI is a valid epoch string");

        // --- Extract entity_id ---
        let entity_col = snapshot
            .column_by_name("entity_id")
            .ok_or("'entity_id' column not found in snapshot")?
            .as_any()
            .downcast_ref::<DictionaryArray<UInt32Type>>()
            .ok_or("'entity_id' is not Dictionary<UInt32>")?;
        let entity_dict = entity_col
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or("'entity_id' values are not Utf8")?;

        // --- Extract spacetimestamp struct ---
        let sts = snapshot
            .column_by_name("spacetimestamp")
            .ok_or("'spacetimestamp' column not found in snapshot")?
            .as_any()
            .downcast_ref::<StructArray>()
            .ok_or("'spacetimestamp' is not a StructArray")?;

        let pos_list = sts
            .column_by_name("position")
            .ok_or("'position' missing")?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .ok_or("'position' is not FixedSizeList")?;
        let pos_values_arc = pos_list.values();
        let pos_values = pos_values_arc
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or("'position' values are not Float64")?;
        let pos_offset = pos_list.offset();

        let quat_list = sts
            .column_by_name("quaternion")
            .ok_or("'quaternion' missing")?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .ok_or("'quaternion' is not FixedSizeList")?;
        let quat_values_arc = quat_list.values();
        let quat_values = quat_values_arc
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or("'quaternion' values are not Float64")?;
        let quat_offset = quat_list.offset();

        let cent_arr = sts
            .column_by_name("duration_centuries")
            .ok_or("'duration_centuries' missing")?
            .as_any()
            .downcast_ref::<Int16Array>()
            .ok_or("'duration_centuries' is not Int16")?;

        let ns_arr = sts
            .column_by_name("duration_ns")
            .ok_or("'duration_ns' missing")?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or("'duration_ns' is not UInt64")?;

        // --- Extract top-level entity columns ---
        let vel_list = snapshot
            .column_by_name("velocity")
            .ok_or("'velocity' column not found")?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .ok_or("'velocity' is not FixedSizeList")?;
        let vel_values_arc = vel_list.values();
        let vel_values = vel_values_arc
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or("'velocity' values are not Float64")?;
        let vel_offset = vel_list.offset();

        let ang_vel_list = snapshot
            .column_by_name("angular_velocity")
            .ok_or("'angular_velocity' column not found")?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .ok_or("'angular_velocity' is not FixedSizeList")?;
        let ang_vel_values_arc = ang_vel_list.values();
        let ang_vel_values = ang_vel_values_arc
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or("'angular_velocity' values are not Float64")?;
        let ang_vel_offset = ang_vel_list.offset();

        let accel_list = snapshot
            .column_by_name("acceleration")
            .ok_or("'acceleration' column not found")?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .ok_or("'acceleration' is not FixedSizeList")?;
        let accel_values_arc = accel_list.values();
        let accel_values = accel_values_arc
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or("'acceleration' values are not Float64")?;
        let accel_offset = accel_list.offset();

        let mass_arr = snapshot
            .column_by_name("mass_kg")
            .ok_or("'mass_kg' column not found")?
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or("'mass_kg' is not Float64")?;

        // --- Build output batch ---
        let mut builder = EntityBuilder::new(num_rows, None);

        for i in 0..num_rows {
            let entity_id = entity_dict.value(entity_col.keys().value(i) as usize);
            let pos       = read_vec3(pos_values,  pos_offset,  i);
            let quat      = read_vec4(quat_values, quat_offset, i);
            let centuries = cent_arr.value(i);
            let ns        = ns_arr.value(i);

            // Advance epoch by dt
            let new_duration = Duration::from_parts(centuries, ns) + self.dt;
            let (new_centuries, new_ns) = new_duration.to_parts();

            // Velocity (nullable — null means static entity)
            let vel_opt = if vel_list.is_null(i) {
                None
            } else {
                Some(read_vec3(vel_values, vel_offset, i))
            };

            // Carry-forward nullable columns
            let ang_vel_opt = if ang_vel_list.is_null(i) {
                None
            } else {
                Some(read_vec3(ang_vel_values, ang_vel_offset, i))
            };

            let accel_opt = if accel_list.is_null(i) {
                None
            } else {
                Some(read_vec3(accel_values, accel_offset, i))
            };

            let mass_opt = if mass_arr.is_null(i) {
                None
            } else {
                Some(mass_arr.value(i))
            };

            // Propagate position and velocity (or carry forward if static)
            let epoch = j2000 + Duration::from_parts(centuries, ns);
            let (new_pos, new_vel_opt) = match vel_opt {
                Some(vel) => {
                    let (np, nv) = self.propagate_entity(pos, vel, epoch);
                    (np, Some(nv))
                }
                None => (pos, None), // static entity — position unchanged
            };

            builder.append_entity(
                entity_id,
                "ICRF",
                "km",
                "TAI",
                "soloc::sim",
                "SIMULATED",
                new_pos,
                quat,
                new_centuries,
                new_ns,
                new_vel_opt,
                ang_vel_opt,
                accel_opt,
                mass_opt,
            );
        }

        Ok(builder.flush())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_propagate_entity_zero_gravity() {
        // With an empty almanac no bodies resolve, so gravity = 0.
        // A body with constant velocity should travel x = v*t.
        let almanac = Almanac::default();
        let dt = Duration::from_parts(0, 60_000_000_000); // 60 seconds
        let prop = NBodyPropagator::new(almanac, dt, 5);

        let epoch = Epoch::from_str("2000-01-01T12:00:00 TAI").unwrap();
        let (new_pos, new_vel) = prop.propagate_entity(
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0], // 1 km/s along X
            epoch,
        );

        // With zero gravity: pos = [60, 0, 0], vel = [1, 0, 0]
        assert!((new_pos[0] - 60.0).abs() < 1e-6, "x = {}", new_pos[0]);
        assert!(new_pos[1].abs() < 1e-12);
        assert!((new_vel[0] - 1.0).abs() < 1e-12, "vx = {}", new_vel[0]);
    }
}
