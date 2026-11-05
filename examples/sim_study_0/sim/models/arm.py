"""A UR10e-class arm in the Odense cell, unstacking pallet A onto pallet B. In metres, each in its
parent's frame:

    cell (on IAU_EARTH) → ARM-1-BASE (the pedestal top) → L1 → L2 → … → L6 → ARM-1-GRIPPER
    cell → PALLET-A, PALLET-B (turned 180° about up, so its slots mirror A's)
    CELL-BOX-nn on PALLET-A, then on the gripper while held, then on PALLET-B

Link k's frame sits at joint k with z along its axis, as a ROS tf tree stores it: its row in
link k-1 is the fixed DH origin of the joint (Tz(d)·Tx(a)·Rx(α) of DH row k-1) turned by
Rz(q_k). The tip points down and moves on straight vertical legs (closed-form IK per sample) and
quintic joint transfers between them. Links and the held box report at `ARM_HZ` through the
episode, else hourly; the base, pallets and gripper are static `Part`s.
"""

import functools
import math

import numpy as np
import pyarrow as pa

from sim import scenario as sc
from sim.geo import EARTH, matrix_from_quat, quat_yaw, rot_x, rot_z
from sim.models import Row
from sim.models.bearing import quat_mul, wrap
from sim.models.facility import Facility
from sim.models.factory import Part
from soloc_client import KIND_SOLOC, mint, positions, sts_field

EPISODE_S0 = sc.seconds(sc.ARM_START)
SAMPLE_NS = 10**9 // sc.ARM_HZ
TICK_SAMPLES = sc.BASE_TICK_S * sc.ARM_HZ
D, A, ALPHA = (np.array(v) for v in (sc.DH_D, sc.DH_A, sc.DH_ALPHA))
H = sc.CELL_BOX_DIMENSIONS_M[2]
PALLET_TOP_M = sc.PALLET_DIMENSIONS_M[2]
BRANCH = (1, -1, 1)                  # shoulder, wrist, elbow: the wrist-down, elbow-up solution
DIFF_S = 1e-4                        # central difference for joint speeds on the straight legs
HELD_M = [0.0, 0.0, sc.TOOL_M + H / 2]
HELD_Q = [0.0, 1.0, 0.0, 0.0]        # a held box is turned over in the gripper (whose z points down)

TRUTH_SCHEMA = pa.schema([
    ("arm", pa.string()),
    ("t_ns", pa.int64()),            # since scenario.T0
    ("q", pa.list_(pa.float64(), 6)),           # rad
    ("qd", pa.list_(pa.float64(), 6)),          # rad/s
    ("tip", pa.list_(pa.float64(), 3)),         # cell ENU, m, from the model's FK
    ("box", pa.string()),            # the box held, else null
])


def dh(i: int, theta) -> np.ndarray:
    """DH row i's transform at `theta`, over leading axes: Rz(θ)·Tz(d)·Tx(a)·Rx(α)."""
    theta = np.asarray(theta, float)
    ct, st = np.cos(theta), np.sin(theta)
    ca, sa = math.cos(ALPHA[i]), math.sin(ALPHA[i])
    t = np.zeros(theta.shape + (4, 4))
    t[..., 0, 0], t[..., 0, 1], t[..., 0, 2], t[..., 0, 3] = ct, -st * ca, st * sa, A[i] * ct
    t[..., 1, 0], t[..., 1, 1], t[..., 1, 2], t[..., 1, 3] = st, ct * ca, -ct * sa, A[i] * st
    t[..., 2, 1], t[..., 2, 2], t[..., 2, 3] = sa, ca, D[i]
    t[..., 3, 3] = 1.0
    return t


def fk(q) -> np.ndarray:
    """The flange in the base frame, `(..., 4, 4)`, of joint angles `(..., 6)`."""
    q = np.asarray(q, float)
    t = dh(0, q[..., 0])
    for i in range(1, 6):
        t = t @ dh(i, q[..., i])
    return t


def ik(t, branch=BRANCH) -> np.ndarray:
    """Closed-form IK (Hawkins 2013) of flange poses `(..., 4, 4)` on one `(shoulder, wrist,
    elbow)` branch of ±1 signs: joint angles `(..., 6)` in [-π, π)."""
    s_sh, s_wr, s_el = branch
    p, z = t[..., :3, 3], t[..., :3, 2]
    p5 = p - D[5] * z
    q1 = (np.arctan2(p5[..., 1], p5[..., 0]) + math.pi / 2
          + s_sh * np.arccos(np.clip(D[3] / np.hypot(p5[..., 0], p5[..., 1]), -1, 1)))
    s1, c1 = np.sin(q1), np.cos(q1)
    q5 = s_wr * np.arccos(np.clip((p[..., 0] * s1 - p[..., 1] * c1 - D[3]) / D[5], -1, 1))
    s5 = np.sin(q5)
    x, y = t[..., 0, :3], t[..., 1, :3]              # the base's x and y axes, seen from the flange
    q6 = np.arctan2((-x[..., 1] * s1 + y[..., 1] * c1) / s5, (x[..., 0] * s1 - y[..., 0] * c1) / s5)
    t14 = np.linalg.inv(dh(0, q1)) @ t @ np.linalg.inv(dh(4, q5) @ dh(5, q6))
    p13 = (t14 @ np.array([0.0, -D[3], 0.0, 1.0]))[..., :3]
    r = np.linalg.norm(p13, axis=-1)
    q3 = s_el * np.arccos(np.clip((r ** 2 - A[1] ** 2 - A[2] ** 2) / (2 * A[1] * A[2]), -1, 1))
    q2 = -np.arctan2(p13[..., 1], -p13[..., 0]) + np.arcsin(A[2] * np.sin(q3) / r)
    t34 = np.linalg.inv(dh(1, q2) @ dh(2, q3)) @ t14
    q4 = np.arctan2(t34[..., 1, 0], t34[..., 0, 0])
    return wrap(np.stack([q1, q2, q3, q4, q5, q6], -1))


def flange_at(tip, psi: float) -> np.ndarray:
    """Flange poses in the base frame, `(n, 4, 4)`, for tips `(n, 3)` in cell ENU pointing down
    and turned `psi` about up."""
    tip = np.atleast_2d(tip)
    t = np.tile(np.eye(4), (len(tip), 1, 1))
    t[:, :3, :3] = rot_z(psi) @ rot_x(math.pi)
    t[:, :3, 3] = tip + [0.0, 0.0, sc.TOOL_M - sc.PEDESTAL_M]
    return t


def tip_of(q) -> np.ndarray:
    """The tip in cell ENU of joint angles `(..., 6)`."""
    return (fk(q) @ np.array([0.0, 0.0, sc.TOOL_M, 1.0]))[..., :3] + [0.0, 0.0, sc.PEDESTAL_M]


PARK_PSI = -math.pi / 2
Q_PARK = ik(flange_at(sc.PARK_M, PARK_PSI))[0]


def joints(tip, psi: float) -> np.ndarray:
    """IK on `BRANCH`, each angle taken within ±π of the park pose's, so the base swings
    through east between the pallets and no joint wraps."""
    return Q_PARK + wrap(ik(flange_at(tip, psi)) - Q_PARK)


def trapezoid(length: float, t):
    """Fraction of a straight leg covered at `t` s, at `LINE_SPEED_M_S` with `LINE_ACCEL_M_S2`
    ramps, and the leg's duration."""
    v, a = sc.LINE_SPEED_M_S, sc.LINE_ACCEL_M_S2
    if length >= v * v / a:
        ramp, cruise = v / a, length / v - v / a
    else:
        ramp, cruise = math.sqrt(length / a), 0.0
        v = a * ramp
    total = 2 * ramp + cruise
    t = np.clip(t, 0.0, total)
    s = np.where(t < ramp, a * t ** 2 / 2,
                 np.where(t < ramp + cruise, a * ramp ** 2 / 2 + v * (t - ramp),
                          length - a * (total - t) ** 2 / 2))
    return s / length if length else np.ones_like(t), total


class Motion:
    """The episode's piecewise trajectory, sampled on the `ARM_HZ` grid from `ARM_START`: `t_ns`
    (since T0), `q`, `qd`, and each pick's grasp and release times."""

    def __init__(self, picks: list[tuple]):
        """`picks`: `(tip on A, ψ on A, tip on B, ψ on B, travel z)` per box, in order."""
        self.segments: list[tuple] = []          # (t0, t1, kind, args)
        self.grasp_s, self.release_s = [], []
        self.t, self.q = float(EPISODE_S0), Q_PARK
        for tip_a, psi_a, tip_b, psi_b, z in picks:
            above_a, above_b = np.array([*tip_a[:2], z]), np.array([*tip_b[:2], z])
            self.joint(joints(above_a, psi_a)[0])
            self.line(above_a, tip_a, psi_a)
            self.grasp_s.append(self.t)
            self.dwell()
            self.line(tip_a, above_a, psi_a)
            self.joint(joints(above_b, psi_b)[0])
            self.line(above_b, tip_b, psi_b)
            self.release_s.append(self.t)
            self.dwell()
            self.line(tip_b, above_b, psi_b)
        self.joint(Q_PARK)
        self.end_s = self.t
        self.ticks_s = (EPISODE_S0, int(math.ceil(self.end_s / sc.BASE_TICK_S)) * sc.BASE_TICK_S)
        n = (self.ticks_s[1] - self.ticks_s[0]) * sc.ARM_HZ
        self.t_ns = EPISODE_S0 * 10**9 + np.arange(n, dtype=np.int64) * SAMPLE_NS
        self.q, self.qd = self.sample(self.t_ns / 1e9)

    def joint(self, q1):
        """A quintic in joint space, as short as the speed and acceleration limits allow."""
        dq = np.abs(q1 - self.q).max()
        span = max(1.875 * dq / sc.JOINT_SPEED, math.sqrt(5.7735 * dq / sc.JOINT_ACCEL))
        self.segments.append((self.t, self.t + span, "joint", (self.q, q1)))
        self.t, self.q = self.t + span, q1

    def line(self, p0, p1, psi: float):
        _, span = trapezoid(float(np.linalg.norm(p1 - p0)), 0.0)
        self.segments.append((self.t, self.t + span, "line", (np.asarray(p0), np.asarray(p1), psi)))
        self.t, self.q = self.t + span, joints(p1, psi)[0]

    def dwell(self):
        self.segments.append((self.t, self.t + sc.DWELL_S, "dwell", (self.q,)))
        self.t += sc.DWELL_S

    def sample(self, t) -> tuple[np.ndarray, np.ndarray]:
        """`(q, qd)` at times `t` (s since T0); parked outside the segments."""
        q, qd = np.tile(Q_PARK, (len(t), 1)), np.zeros((len(t), 6))
        for t0, t1, kind, args in self.segments:
            m = (t >= t0) & (t < t1)
            if not m.any():
                continue
            tau = t[m] - t0
            if kind == "dwell":
                q[m] = args[0]
            elif kind == "joint":
                q0, q1 = args
                s = tau / (t1 - t0)
                q[m] = q0 + np.outer(s ** 3 * (10 - 15 * s + 6 * s ** 2), q1 - q0)
                qd[m] = np.outer(30 * s ** 2 * (1 - s) ** 2 / (t1 - t0), q1 - q0)
            else:
                p0, p1, psi = args
                length = float(np.linalg.norm(p1 - p0))
                at = lambda u: joints(p0 + np.outer(trapezoid(length, u)[0], p1 - p0), psi)
                q[m] = at(tau)
                qd[m] = (at(tau + DIFF_S) - at(tau - DIFF_S)) / (2 * DIFF_S)
        return q, qd

    def index(self, t_s: int) -> range:
        """Grid samples within the driver tick starting at `t_s`."""
        i0 = (t_s - self.ticks_s[0]) * sc.ARM_HZ
        return range(i0, i0 + TICK_SAMPLES)

    def first_at(self, t_s: float) -> int:
        """The first grid sample at or after `t_s`."""
        return int(np.searchsorted(self.t_ns, round(t_s * 1e9)))

    def episode(self, t_s: int) -> bool:
        return self.ticks_s[0] <= t_s < self.ticks_s[1]


def hourly_ns(skip: tuple[int, int] = (0, 0)) -> np.ndarray:
    """Hourly epochs (ns since T0) outside `[skip)` seconds."""
    hours = np.arange(0, sc.DURATION_S + 1, sc.STATIC_CADENCE_S, dtype=np.int64)
    return hours[(hours < skip[0]) | (hours >= skip[1])] * 10**9


class Link:
    """Link k (0-based) in link k-1, or in the base: its joint's DH origin turned by Rz(q_k)."""

    def __init__(self, name: str, parent_id: bytes, k: int, motion: Motion):
        self.name, self.k, self.motion = name, k, motion
        self.id = mint(KIND_SOLOC, sc.AUTHORITY, name)
        self.parent_id = parent_id
        self.offset = [0.0, 0.0, 0.0] if k == 0 else [float(A[k - 1]), 0.0, float(D[k - 1])]
        self.origin = [1.0, 0.0, 0.0, 0.0] if k == 0 else [math.cos(ALPHA[k - 1] / 2), math.sin(ALPHA[k - 1] / 2), 0.0, 0.0]
        self.mass_kg, self.dimensions_m = sc.LINK_MASS_KG[k], list(sc.LINK_DIMENSIONS_M[k])

    def quaternion(self, q):
        """The row quaternion at joint angles `q` (scalar or array)."""
        q = np.asarray(q, float)
        turn = np.stack([np.cos(q / 2), np.zeros_like(q), np.zeros_like(q), np.sin(q / 2)], -1)
        return quat_mul(self.origin, turn)

    def angle(self, quaternion) -> np.ndarray:
        """The joint angle of row quaternions `(..., 4)`: the DH origin's rotation taken off."""
        joint = quat_mul(np.array(self.origin) * [1, -1, -1, -1], quaternion)
        return 2 * np.arctan2(joint[..., 3], joint[..., 0])

    def due(self, t_s: int) -> bool:
        return self.motion.episode(t_s) or t_s % sc.STATIC_CADENCE_S == 0

    def row(self, i: int | None) -> Row:
        """At grid sample `i`, or parked when `None`."""
        q, qd = (Q_PARK[self.k], 0.0) if i is None else (self.motion.q[i, self.k], self.motion.qd[i, self.k])
        return Row(self.parent_id, self.offset, self.quaternion(q).tolist(), units="m", timescale=sc.ARM_TIMESCALE,
                   optional={"velocity": [0.0, 0.0, 0.0], "angular_velocity": [0.0, 0.0, float(qd)],
                             "mass_kg": self.mass_kg, "dimensions": self.dimensions_m})

    def samples(self, t_s: int) -> list[tuple[int, Row]]:
        if self.motion.episode(t_s):
            return [(j * SAMPLE_NS, self.row(i)) for j, i in enumerate(self.motion.index(t_s))]
        return [(0, self.row(None))] if t_s % sc.STATIC_CADENCE_S == 0 else []

    def epochs_ns(self) -> np.ndarray:
        return np.union1d(hourly_ns(self.motion.ticks_s), self.motion.t_ns)


class CellBox:
    """On pallet A at `slot_a` until grasped, on the gripper while held (at `ARM_HZ`), then on
    pallet B at `slot_b`: its first row there is the first grid sample at or after release."""

    def __init__(self, name: str, slot_a, slot_b, a_id: bytes, b_id: bytes, gripper_id: bytes,
                 motion: Motion, grasp_s: float, release_s: float):
        self.name = name
        self.id = mint(KIND_SOLOC, sc.AUTHORITY, name)
        self.slot_a, self.slot_b = list(slot_a), list(slot_b)
        self.a_id, self.b_id, self.gripper_id = a_id, b_id, gripper_id
        self.motion, self.grasp_s, self.release_s = motion, grasp_s, release_s
        self.held = (motion.first_at(grasp_s), motion.first_at(release_s))   # grid samples, inclusive

    def due(self, t_s: int) -> bool:
        return bool(self.samples(t_s))

    def row(self, frame_id: bytes) -> Row:
        offset, q = ((self.slot_a, [1.0, 0.0, 0.0, 0.0]) if frame_id == self.a_id else
                     (self.slot_b, [1.0, 0.0, 0.0, 0.0]) if frame_id == self.b_id else (HELD_M, HELD_Q))
        return Row(frame_id, offset, q, units="m", timescale=sc.ARM_TIMESCALE,
                   optional={"velocity": [0.0, 0.0, 0.0], "mass_kg": sc.CELL_BOX_MASS_KG,
                             "dimensions": list(sc.CELL_BOX_DIMENSIONS_M)})

    def samples(self, t_s: int) -> list[tuple[int, Row]]:
        out = []
        g, r = self.held
        if self.motion.episode(t_s):
            i0 = self.motion.index(t_s).start
            for i in range(max(g, i0), min(r, i0 + TICK_SAMPLES - 1) + 1):
                out.append(((i - i0) * SAMPLE_NS, self.row(self.gripper_id if i < r else self.b_id)))
        if t_s % sc.STATIC_CADENCE_S == 0 and not (out and out[0][0] == 0):
            out.insert(0, (0, self.row(self.a_id if t_s < self.grasp_s else self.b_id)))
        return out

    def epochs_ns(self) -> np.ndarray:
        g, r = self.held
        return np.union1d(hourly_ns(), self.motion.t_ns[g:r + 1])


class Arm:
    def __init__(self):
        self.cell = Facility(sc.ARM_VENUE)
        n = sc.ARM_NAME
        self.base = Part(f"{n}-BASE", self.cell.id, (0.0, 0.0, sc.PEDESTAL_M), sc.BASE_MASS_KG, sc.BASE_DIMENSIONS_M)
        self.pallets = [Part("PALLET-A", self.cell.id, sc.PALLET_A_M, sc.PALLET_MASS_KG, sc.PALLET_DIMENSIONS_M),
                        Part("PALLET-B", self.cell.id, sc.PALLET_B_M, sc.PALLET_MASS_KG, sc.PALLET_DIMENSIONS_M,
                             base=quat_yaw(math.pi))]
        layers, rows, cols = sc.STACK
        per = rows * cols
        wx, wy, _ = sc.CELL_BOX_DIMENSIONS_M
        slot = lambda layer, j: np.array([(j % cols - (cols - 1) / 2) * wx, (j // cols - (rows - 1) / 2) * wy,
                                          PALLET_TOP_M + H * (layer + 0.5)])
        a, b = (np.array(p.offset) for p in self.pallets)
        picks, slots = [], []
        for i in range(layers * per):
            j, layer_a, layer_b = i % per, layers - 1 - i // per, i // per
            s_a, s_b = slot(layer_a, j), slot(layer_b, j)
            tip_a = a + s_a + [0.0, 0.0, H / 2]
            tip_b = b + s_b * [-1.0, -1.0, 1.0] + [0.0, 0.0, H / 2]        # pallet B is turned 180°
            top_b = PALLET_TOP_M + H * (layer_b + (j > 0))
            picks.append((tip_a, 0.0, tip_b, math.pi, max(tip_a[2], top_b) + H + sc.TRAVEL_CLEAR_M))
            slots.append((s_a, s_b))
        self.motion = Motion(picks)
        self.links = []
        parent = self.base
        for k in range(6):
            parent = Link(f"{n}-L{k + 1}", parent.id, k, self.motion)
            self.links.append(parent)
        self.gripper = Part(f"{n}-GRIPPER", self.links[-1].id, (0.0, 0.0, float(D[5])), sc.GRIPPER_MASS_KG,
                            sc.GRIPPER_DIMENSIONS_M)
        self.boxes = [CellBox(f"CELL-BOX-{i + 1:02d}", s_a, s_b, self.pallets[0].id, self.pallets[1].id,
                              self.gripper.id, self.motion, g, r)
                      for i, ((s_a, s_b), g, r) in enumerate(zip(slots, self.motion.grasp_s, self.motion.release_s))]
        self.names = {e.id: e.name for e in self.entities}

    @property
    def parts(self) -> list:
        """Everything but the cell and the boxes, parents first."""
        return [self.base, *self.pallets, *self.links, self.gripper]

    @property
    def entities(self) -> list:
        """Parents before their children."""
        return [self.cell, *self.parts, *self.boxes]

    @functools.cached_property
    def tip(self) -> np.ndarray:
        """The tip in cell ENU at every grid sample, from the model's FK."""
        return tip_of(self.motion.q)

    def in_cell(self, client, rows: pa.Table, offset_m) -> np.ndarray:
        """Cell ENU (m) of the point at `offset_m` in each row's entity, resolved by soloc: the rows
        are exchanged to IAU_EARTH (`exchange` targets astro frames only), then taken into the
        cell's ENU."""
        out = client.exchange(rows, EARTH.frame)
        q = sts_field(out, "quaternion").flatten().to_numpy().reshape(-1, 4)
        p = positions(out) + np.array([matrix_from_quat(v) @ offset_m for v in q]) / 1000
        return (p - self.cell.position_km) @ self.cell.basis * 1000

    def holding(self) -> np.ndarray:
        """Per grid sample, the index of the box held, else -1."""
        out = np.full(len(self.motion.t_ns), -1)
        for k, b in enumerate(self.boxes):
            out[b.held[0]:b.held[1]] = k
        return out

    def result(self) -> str:
        m = self.motion
        span = m.end_s - EPISODE_S0
        return (f"arm: {len(self.boxes)} boxes moved from PALLET-A to PALLET-B in {span:.0f} s "
                f"({span / len(self.boxes):.1f} s each); {len(m.t_ns):,} samples at {sc.ARM_HZ} Hz, "
                f"peak tip speed {self.peak_tip_speed():.2f} m/s")

    def peak_tip_speed(self) -> float:
        return float((np.linalg.norm(np.diff(self.tip, axis=0), axis=1) * sc.ARM_HZ).max())


def write_table(path, arm: Arm) -> int:
    """One row per grid sample. Returns the row count."""
    held = arm.holding()
    names = [arm.boxes[k].name if k >= 0 else None for k in held]
    n = len(held)
    as_list = lambda a, width: pa.FixedSizeListArray.from_arrays(pa.array(a.reshape(-1)), width)
    with pa.ipc.new_file(str(path), TRUTH_SCHEMA) as f:
        f.write_batch(pa.record_batch([
            pa.array([sc.ARM_NAME] * n), pa.array(arm.motion.t_ns),
            as_list(arm.motion.q, 6).cast(TRUTH_SCHEMA.field("q").type),
            as_list(arm.motion.qd, 6).cast(TRUTH_SCHEMA.field("qd").type),
            as_list(arm.tip, 3).cast(TRUTH_SCHEMA.field("tip").type), pa.array(names, pa.string())],
            schema=TRUTH_SCHEMA))
    return n
