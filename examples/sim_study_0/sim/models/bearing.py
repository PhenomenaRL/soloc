"""Bearing dynamics for a factory line: a rigid shaft on two deep-groove ball bearings with Hertz
contacts, radial clearance, viscous damping and per-ball wear (the lumped model of Sopanen &
Mikkola and Sawalhi & Randall).

The shaft moves in the stator's y-z plane: its two bearing stations at x = ∓L each have a
radial displacement u_k, which carries the shaft's translation and tilt. Ball j of bearing k
sits at angle θ_kj = cage angle + 2πj/Z + pocket wander and takes load
Q = K·δ^1.5, δ = u_k·e(θ) − c_r/2 + Δd_kj − spall depth, when δ > 0.

Over the shift the shaft is in quasi-static equilibrium at every row epoch. In a capture it is
integrated by RK4 from that equilibrium. Cage slip, ball spin slip and pocket wander are
functions of the shaft angle, so they hold still when the line stops.
"""

import math
from typing import Callable

import numpy as np
import pyarrow as pa

from sim import scenario as sc
from sim.models.robot import own_rng

# Rolling-bearing kinematics, per radian of the shaft (inner ring turning, outer ring fixed).
_GAMMA = sc.BALL_D_M / sc.PITCH_D_M * math.cos(math.radians(sc.CONTACT_DEG))
CAGE_RATIO = (1 - _GAMMA) / 2                            # cage about the outer ring
BALL_RATIO = -sc.PITCH_D_M / (2 * sc.BALL_D_M) * (1 - _GAMMA ** 2)   # ball spin relative to the cage
# Defect frequencies per shaft revolution.
FTF, BSF = CAGE_RATIO, -BALL_RATIO
BPFO, BPFI = sc.BALLS * CAGE_RATIO, sc.BALLS * (1 - CAGE_RATIO)

WANDER_TERMS = 3
WANDER_RATE = (0.05, 0.5)            # cycles per shaft radian
SLIP_SPREAD = 0.2                    # per-bearing and per-ball slip, relative to the class mean
NEWTON_ITERS = 60
NEWTON_STEP_M = 5e-6

TRUTH_SCHEMA = pa.schema([
    ("line", pa.string()),
    ("t_ns", pa.int64()),            # since scenario.T0
    ("y", pa.float64()), ("z", pa.float64()),               # shaft centre, stator frame, m
    ("tilt_y", pa.float64()), ("tilt_z", pa.float64()),     # rad
    ("acc_y", pa.float64()), ("acc_z", pa.float64()),       # shaft centre, m/s²
    ("forces", pa.list_(pa.float64(), 2 * sc.BALLS)),       # N, bearing 1 balls then bearing 2
])


def _hertz(race_rx: float) -> float:
    """K (N/m^1.5) of a ball on a race whose rolling-direction radius is `race_rx` (negative
    when concave), with the groove conformity across it (Brewe & Hamrock's approximations)."""
    d = sc.BALL_D_M
    rx = 1 / (2 / d + 1 / race_rx)
    ry = 1 / (2 / d - 1 / (sc.GROOVE_CONFORMITY * d))
    k = 1.0339 * (ry / rx) ** 0.636
    e = 1.0003 + 0.5968 * rx / ry
    f = 1.5277 + 0.6023 * math.log(ry / rx)
    r = 1 / (1 / rx + 1 / ry)
    e_prime = sc.STEEL_E_PA / (1 - sc.STEEL_NU ** 2)
    return math.pi * k * e_prime * math.sqrt(2 * e * r / 9) / f ** 1.5


K_INNER = _hertz((sc.PITCH_D_M - sc.BALL_D_M) / 2)
K_OUTER = _hertz(-(sc.PITCH_D_M + sc.BALL_D_M) / 2)
K = (K_INNER ** (-2 / 3) + K_OUTER ** (-2 / 3)) ** -1.5     # the two contacts in series


def wrap(a):
    return (a + np.pi) % (2 * np.pi) - np.pi


def quat_mul(a, b) -> np.ndarray:
    """Hamilton product, scalar first, over trailing axes."""
    w1, x1, y1, z1 = np.moveaxis(np.asarray(a, float), -1, 0)
    w2, x2, y2, z2 = np.moveaxis(np.asarray(b, float), -1, 0)
    return np.stack([w1 * w2 - x1 * x2 - y1 * y2 - z1 * z2, w1 * x2 + x1 * w2 + y1 * z2 - z1 * y2,
                     w1 * y2 - x1 * z2 + y1 * w2 + z1 * x2, w1 * z2 + x1 * y2 - y1 * x2 + z1 * w2], -1)


def quat_x(a) -> np.ndarray:
    a = np.asarray(a, float)
    return np.stack([np.cos(a / 2), np.sin(a / 2), np.zeros_like(a), np.zeros_like(a)], -1)


def quat_tilt(tilt_y, tilt_z) -> np.ndarray:
    """The rotation by the vector (0, tilt_y, tilt_z)."""
    r = np.hypot(tilt_y, tilt_z)
    s = np.where(r > 0, np.sin(r / 2) / np.where(r > 0, r, 1), 0.5)
    return np.stack([np.cos(r / 2), np.zeros_like(r), s * tilt_y, s * tilt_z], -1)


class LineBearings:
    """One line's shaft and its two bearings: wear drawn per ball from the ball's name, the
    shaft's mass and inertia from its parts, and the quasi-static solution over `epochs_ns`."""

    def __init__(self, name: str, wear: sc.Wear, rigid_parts: list, ball_names: list[list[str]],
                 angle: Callable, omega: Callable, epochs_ns: np.ndarray):
        self.name, self.wear = name, wear
        self.angle, self.omega = angle, omega
        self.L = sc.BEARING_X_M
        self.half_clearance = wear.clearance_m / 2
        self.m = sum(p.mass_kg for p in rigid_parts)
        # Transverse inertia about the centre: each part a uniform cylinder (length × Ø × Ø).
        self.inertia = sum(p.mass_kg * (3 * (p.dimensions_m[1] / 2) ** 2 + p.dimensions_m[0] ** 2) / 12
                           + p.mass_kg * p.offset[0] ** 2 for p in rigid_parts)
        L = self.L
        a, b = self.m / 4, self.inertia / (4 * L * L)
        self.m_inv = np.linalg.inv([[a + b, a - b], [a - b, a + b]])
        w = np.array([(L - sc.BELT_PULL_AT_M) / (2 * L), (L + sc.BELT_PULL_AT_M) / (2 * L)])
        self.load = (w[:, None] * np.array(sc.BELT_PULL_N[1:])
                     + 0.5 * np.array([0.0, -self.m * sc.GRAVITY_M_S2]))   # (station, axis), N

        z = sc.BALLS
        self.beta = 2 * np.pi * np.arange(z) / z
        self.dd = np.zeros((2, z))
        self.spin_slip = np.zeros((2, z))
        self.cage_slip = np.zeros(2)
        self.wander = np.zeros((3, 2, z, WANDER_TERMS))          # amplitude, rate, phase
        for k in range(2):
            self.cage_slip[k] = wear.slip * (1 + SLIP_SPREAD * own_rng(sc.SEED, f"{name}-B{k + 1}").standard_normal())
            for j in range(z):
                rng = own_rng(sc.SEED, ball_names[k][j])
                self.dd[k, j] = wear.ball_sigma_m * rng.standard_normal()
                self.spin_slip[k, j] = wear.slip * (1 + SLIP_SPREAD * rng.standard_normal())
                self.wander[0, k, j] = math.radians(wear.wander_deg) * rng.dirichlet(np.ones(WANDER_TERMS))
                self.wander[1, k, j] = rng.uniform(*WANDER_RATE, WANDER_TERMS)
                self.wander[2, k, j] = rng.uniform(0, 2 * np.pi, WANDER_TERMS)
        self.spall_at = np.full((2, z), np.nan)                  # the spall's angle on the ball
        if wear.spall:
            k, j = wear.spall
            self.spall_at[k - 1, j - 1] = own_rng(sc.SEED, ball_names[k - 1][j - 1] + "-spall").uniform(0, 2 * np.pi)
        self.spall_half = sc.SPALL_M[0] / sc.BALL_D_M            # half its arc on the ball, rad

        self.epochs_ns = np.asarray(epochs_ns, np.int64)
        self.index = {int(t): i for i, t in enumerate(self.epochs_ns)}
        t = self.epochs_ns / 1e9
        self.phi, self.w = angle(t), omega(t)
        self.u, self.q = self.equilibrium(self.phi)

    # -- kinematics ------------------------------------------------------------------------------

    def cage_angle(self, phi):
        return CAGE_RATIO * (1 - self.cage_slip) * np.asarray(phi)[..., None]          # (..., 2)

    def wander_at(self, phi):
        """Pocket wander ε and dε/dφ, (..., 2, Z)."""
        amp, rate, phase = self.wander
        arg = rate * np.asarray(phi)[..., None, None, None] + phase
        return (amp * np.sin(arg)).sum(-1), (amp * rate * np.cos(arg)).sum(-1)

    def ball_spin(self, phi):
        return BALL_RATIO * (1 - self.spin_slip) * np.asarray(phi)[..., None, None]

    def theta(self, phi):
        """Ball angles in the stator frame, (..., 2, Z)."""
        return self.cage_angle(phi)[..., None] + self.beta + self.wander_at(phi)[0]

    def spall_depth(self, phi):
        """Depth under each ball's contacts: the spall meets the outer race when its angle on the
        ball faces out, and the inner race half a ball turn later."""
        if np.isnan(self.spall_at).all():
            return np.zeros(np.shape(phi) + (2, sc.BALLS))
        eps = self.wander_at(phi)[0]
        d = np.abs(wrap(self.spall_at + self.ball_spin(phi) - self.beta - eps))
        hit = (d < self.spall_half) | (np.pi - d < self.spall_half)
        return np.where(hit, sc.SPALL_M[1], 0.0)

    # -- contacts --------------------------------------------------------------------------------

    def deflection(self, u, c, s, h):
        """δ (..., 2, Z) for station displacements u (..., 2, 2)."""
        return u[..., 0:1] * c + u[..., 1:2] * s - self.half_clearance + self.dd - h

    def equilibrium(self, phi) -> tuple[np.ndarray, np.ndarray]:
        """Station displacements (N, 2, 2) and ball loads (N, 2, Z) balancing `load`, by Newton."""
        phi = np.atleast_1d(phi)
        th = self.theta(phi)
        c, s, h = np.cos(th), np.sin(th), self.spall_depth(phi)
        p = self.load
        mag = np.linalg.norm(p, axis=-1, keepdims=True)
        u = np.broadcast_to(p / mag * (self.half_clearance + (mag / K) ** (2 / 3)), phi.shape + (2, 2)).copy()
        for _ in range(NEWTON_ITERS):
            d = np.maximum(self.deflection(u, c, s, h), 0.0)
            q, dq = K * d ** 1.5, 1.5 * K * np.sqrt(d)
            g = np.stack([(q * c).sum(-1), (q * s).sum(-1)], -1) - p
            jxx, jxy, jyy = (dq * c * c).sum(-1) + 1e3, (dq * c * s).sum(-1), (dq * s * s).sum(-1) + 1e3
            det = jxx * jyy - jxy * jxy
            du = np.stack([(jyy * g[..., 0] - jxy * g[..., 1]) / det,
                           (jxx * g[..., 1] - jxy * g[..., 0]) / det], -1)
            n = np.linalg.norm(du, axis=-1, keepdims=True)
            u -= du * np.minimum(1.0, NEWTON_STEP_M / np.maximum(n, 1e-30))
            if np.abs(g).max() < 1e-6:
                break
        d = np.maximum(self.deflection(u, c, s, h), 0.0)
        return u, K * d ** 1.5

    def at(self, t_s: float) -> int:
        return self.index[round(t_s * 1e9)]


def centre(u) -> tuple[np.ndarray, np.ndarray, np.ndarray, np.ndarray]:
    """Shaft centre (y, z) and tilts (about y, about z) from station displacements (..., 2, 2)."""
    L = sc.BEARING_X_M
    y = (u[..., 0, 0] + u[..., 1, 0]) / 2
    z = (u[..., 0, 1] + u[..., 1, 1]) / 2
    return y, z, (u[..., 0, 1] - u[..., 1, 1]) / (2 * L), (u[..., 1, 0] - u[..., 0, 0]) / (2 * L)


class Captures:
    """Every line's captures integrated together, RK4 at `CAPTURE_STEP_S` from equilibrium
    `CAPTURE_SETTLE_S` early. The shaft turns at a steady `omega` through each one."""

    def __init__(self, lines: list[LineBearings], starts_s: list[int], omega: float):
        self.lines, self.starts_s, self.omega = lines, starts_s, omega
        dt = sc.CAPTURE_STEP_S
        self.steps = round(sc.CAPTURE_S / dt)
        settle = round(sc.CAPTURE_SETTLE_S / dt)
        n_sys = len(starts_s) * len(lines)
        n, z = len(starts_s), sc.BALLS
        self.u = np.zeros((self.steps, n_sys, 2, 2))
        self.v = np.zeros((self.steps, n_sys, 2, 2))
        self.acc = np.zeros((self.steps, n_sys, 2, 2))
        self.q = np.zeros((self.steps, n_sys, 2, z))

        # Systems are (capture, line), line fastest; per-system constants broadcast over balls.
        sys_line = [line for _ in starts_s for line in lines]
        hc = np.array([l.half_clearance for l in sys_line])[:, None, None]
        dd = np.stack([l.dd for l in sys_line])
        load = np.stack([l.load for l in sys_line])
        m_inv = np.stack([l.m_inv for l in sys_line])
        phi0 = np.repeat([lines[0].angle(t - sc.CAPTURE_SETTLE_S) for t in starts_s], len(lines))
        for line in lines:
            assert all(abs(line.omega(t + f) - omega) < 1e-12 for t in starts_s
                       for f in (-sc.CAPTURE_SETTLE_S, sc.CAPTURE_S)), "a capture leaves steady speed"

        def kinematics(k0: int, k1: int):
            """cos, sin and spall depth at the half steps k0/2 … k1/2 after the settle start."""
            t = np.arange(k0, k1 + 1) * dt / 2
            phi = phi0[None, :] + omega * t[:, None]                      # (T, n_sys)
            th = np.empty(phi.shape + (2, z))
            h = np.empty_like(th)
            for i, line in enumerate(lines):
                th[:, i::len(lines)] = line.theta(phi[:, i::len(lines)])
                h[:, i::len(lines)] = line.spall_depth(phi[:, i::len(lines)])
            return np.cos(th), np.sin(th), h

        def accel(u, v, c, s, h):
            d = np.maximum(u[..., 0:1] * c + u[..., 1:2] * s - hc + dd - h, 0.0)
            q = K * d ** 1.5
            f = load - np.stack([(q * c).sum(-1), (q * s).sum(-1)], -1) - sc.STATION_DAMPING_NS_M * v
            return np.einsum("skl,sla->ska", m_inv, f), q

        u = np.empty((n_sys, 2, 2))
        for i, line in enumerate(lines):
            for c_i, t in enumerate(starts_s):
                u[c_i * len(lines) + i] = line.equilibrium(line.angle(t - sc.CAPTURE_SETTLE_S))[0][0]
        v = np.zeros_like(u)
        total, chunk = settle + self.steps, 5000
        for k0 in range(0, total, chunk):
            k1 = min(k0 + chunk, total)
            c, s, h = kinematics(2 * k0, 2 * k1)
            for k in range(k1 - k0):
                i = 2 * k
                a1, q1 = accel(u, v, c[i], s[i], h[i])
                n_step = k0 + k - settle
                if n_step >= 0:
                    self.u[n_step], self.v[n_step], self.acc[n_step], self.q[n_step] = u, v, a1, q1
                a2, _ = accel(u + dt / 2 * v, v + dt / 2 * a1, c[i + 1], s[i + 1], h[i + 1])
                a3, _ = accel(u + dt / 2 * (v + dt / 2 * a1), v + dt / 2 * a2, c[i + 1], s[i + 1], h[i + 1])
                a4, _ = accel(u + dt * (v + dt / 2 * a2), v + dt * a3, c[i + 2], s[i + 2], h[i + 2])
                u, v = (u + dt * (v + dt / 6 * (a1 + a2 + a3)),
                        v + dt / 6 * (a1 + 2 * a2 + 2 * a3 + a4))
        self.n_lines = len(lines)

    def window(self, t_s: float) -> tuple[int, int] | None:
        """(system, step) of the capture holding `t_s`, if any."""
        for c_i, start in enumerate(self.starts_s):
            k = round((t_s - start) / sc.CAPTURE_STEP_S)
            if 0 <= k < self.steps:
                return c_i, k
        return None

    def system(self, c_i: int, line: LineBearings) -> int:
        return c_i * self.n_lines + self.lines.index(line)


def write_table(path, captures: Captures) -> int:
    """Every capture step of every line. Returns the row count."""
    t0 = np.arange(captures.steps, dtype=np.int64) * round(sc.CAPTURE_STEP_S * 1e9)
    with pa.ipc.new_file(str(path), TRUTH_SCHEMA) as f:
        for c_i, start in enumerate(captures.starts_s):
            for line in captures.lines:
                i = captures.system(c_i, line)
                y, z, ty, tz = centre(captures.u[:, i])
                acc = captures.acc[:, i].mean(axis=1)            # the centre's, from the stations'
                forces = pa.FixedSizeListArray.from_arrays(captures.q[:, i].reshape(-1), 2 * sc.BALLS)
                f.write_batch(pa.record_batch([
                    pa.array([line.name] * captures.steps), pa.array(start * 10**9 + t0),
                    pa.array(y), pa.array(z), pa.array(ty), pa.array(tz),
                    pa.array(acc[:, 0]), pa.array(acc[:, 1]), forces.cast(TRUTH_SCHEMA.field("forces").type)],
                    schema=TRUTH_SCHEMA))
    return len(captures.starts_s) * len(captures.lines) * captures.steps
