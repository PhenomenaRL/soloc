//! RK4 numerical integrator for orbital mechanics.
//!
//! Operates on a 6-component state vector `[px, py, pz, vx, vy, vz]` (km, km/s).
//! The acceleration closure makes this completely physics-agnostic: any force model
//! can be injected, and the integrator simply advances the state.
//!
//! # Accuracy
//!
//! RK4 has local truncation error O(h⁵) and global error O(h⁴). For a LEO orbit
//! (~7000 km, ~90 min period) with a 60-second timestep, position drift is typically
//! under 1 metre per orbit.

use hifitime::{Duration, Epoch};

/// A 6-component orbital state vector: `[px, py, pz, vx, vy, vz]` in km and km/s.
pub type State6 = [f64; 6];

/// Advances a state vector by one timestep using the classical 4th-order Runge-Kutta method.
///
/// # Arguments
/// * `state` — Current `[px, py, pz, vx, vy, vz]` in km and km/s.
/// * `epoch` — Epoch at the start of the step. Passed to `accel` at each substep so
///   the gravitational field can be evaluated at the correct time.
/// * `dt_s` — Timestep in seconds. Must be positive.
/// * `accel` — Closure returning gravitational acceleration `[ax, ay, az]` km/s² given
///   the substep epoch and entity position `[px, py, pz]` km.
pub fn rk4_step<F>(state: &State6, epoch: Epoch, dt_s: f64, accel: F) -> State6
where
    F: Fn(Epoch, [f64; 3]) -> [f64; 3],
{
    debug_assert!(dt_s > 0.0, "rk4_step requires a positive timestep");

    let t_half = epoch + secs_to_duration(dt_s * 0.5);
    let t_end  = epoch + secs_to_duration(dt_s);

    // k1..k4 are raw derivatives: [vx, vy, vz, ax, ay, az]  (km/s and km/s²)
    let k1 = raw_deriv(epoch,  state,                          &accel);
    let k2 = raw_deriv(t_half, &add_scaled(state, &k1, dt_s * 0.5), &accel);
    let k3 = raw_deriv(t_half, &add_scaled(state, &k2, dt_s * 0.5), &accel);
    let k4 = raw_deriv(t_end,  &add_scaled(state, &k3, dt_s),       &accel);

    let mut result = *state;
    for i in 0..6 {
        result[i] += (dt_s / 6.0) * (k1[i] + 2.0 * k2[i] + 2.0 * k3[i] + k4[i]);
    }
    result
}

/// Returns the raw derivative `[vx, vy, vz, ax, ay, az]` at the given epoch and state.
#[inline]
fn raw_deriv<F>(t: Epoch, s: &State6, accel: &F) -> State6
where
    F: Fn(Epoch, [f64; 3]) -> [f64; 3],
{
    let [ax, ay, az] = accel(t, [s[0], s[1], s[2]]);
    [s[3], s[4], s[5], ax, ay, az]
}

/// Returns `state + scale * delta` element-wise.
#[inline]
fn add_scaled(state: &State6, delta: &State6, scale: f64) -> State6 {
    [
        state[0] + scale * delta[0],
        state[1] + scale * delta[1],
        state[2] + scale * delta[2],
        state[3] + scale * delta[3],
        state[4] + scale * delta[4],
        state[5] + scale * delta[5],
    ]
}

/// Converts a positive duration in seconds to a hifitime [`Duration`].
///
/// Accurate up to one Julian century (~3.15 × 10¹² s).
pub(crate) fn secs_to_duration(dt_s: f64) -> Duration {
    debug_assert!(dt_s >= 0.0, "secs_to_duration expects a non-negative value");
    Duration::from_parts(0, (dt_s * 1e9).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_epoch() -> Epoch {
        use std::str::FromStr;
        Epoch::from_str("2000-01-01T12:00:00 TAI").unwrap()
    }

    /// Free fall under constant 1 km/s² acceleration along X.
    /// x(t) = ½·a·t²  →  x(10 s) = 50 km,  vx(10 s) = 10 km/s.
    #[test]
    fn test_rk4_constant_acceleration() {
        let state: State6 = [0.0; 6];
        let result = rk4_step(&state, test_epoch(), 10.0, |_, _| [1.0, 0.0, 0.0]);
        assert!((result[0] - 50.0).abs() < 1e-9, "x = {}", result[0]);
        assert!((result[3] - 10.0).abs() < 1e-9, "vx = {}", result[3]);
        assert!(result[1].abs() < 1e-12);
        assert!(result[4].abs() < 1e-12);
    }

    /// Circular orbit closure check: after one full revolution position and velocity
    /// should be within 1 km / 0.01 km/s of the initial values.
    ///
    /// Parameters: r = 7000 km (LEO), GM = 3.986004418e5 km³/s², 10 000 steps per orbit.
    #[test]
    fn test_rk4_circular_orbit_closure() {
        let gm = 3.986004418e5_f64;
        let r  = 7000.0_f64;
        let v  = (gm / r).sqrt();
        let period = 2.0 * std::f64::consts::PI * r / v;
        let epoch = test_epoch();
        let n_steps = 10_000usize;
        let dt_s = period / n_steps as f64;

        let mut s: State6 = [r, 0.0, 0.0, 0.0, v, 0.0];
        for step in 0..n_steps {
            let t = epoch + secs_to_duration(step as f64 * dt_s);
            s = rk4_step(&s, t, dt_s, |_, pos| {
                let r3 = (pos[0] * pos[0] + pos[1] * pos[1] + pos[2] * pos[2]).powf(1.5);
                [-gm * pos[0] / r3, -gm * pos[1] / r3, -gm * pos[2] / r3]
            });
        }

        assert!((s[0] - r).abs() < 1.0,   "x drift = {:.3} km", (s[0] - r).abs());
        assert!(s[1].abs()        < 1.0,   "y drift = {:.3} km", s[1].abs());
        assert!((s[4] - v).abs()  < 0.01,  "vy drift = {:.4} km/s", (s[4] - v).abs());
    }
}
