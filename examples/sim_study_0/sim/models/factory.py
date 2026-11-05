"""The Steyr plant: three conveyor lines, each with its drive's bearings modelled part by part.

Per line, in metres in each part's parent frame:

    Line (on the plant) → Machine A, Machine B (the conveyor), Machine C
    Machine B → stator (the gearmotor housing; its x is B's y, the pulley axle)
      stator → shaft (spins about x)  → rotor, inner rings 1-2 (rigid on the shaft)
      stator → outer rings 1-2        → cage (turns about x at cage speed) → 8 balls (spin about x)
    Machine B → boxes (on the belt), then each box's last row is on Machine C

The shaft follows a prescribed trapezoid speed profile per run, and a box on the belt has moved
`PULLEY_R_M` × the shaft's angle since it was spawned. The shaft, cages and balls take their
poses from the bearing dynamics (`sim/models/bearing.py`): the shaft's µm displacement and
tilt, cage slip, and each ball's pocket wander, seat and spin slip. Rotating parts report every
`PART_CADENCE_S` in the shift and at `BURST_HZ` in the burst through `samples` (the driver's
sub-tick hook), and the shafts at `CAPTURE_HZ` in the captures; static parts report hourly.
"""

import functools
import math

import numpy as np

from sim import scenario as sc
from sim.geo import quat_yaw
from sim.models import Row
from sim.models import bearing as br
from sim.models.bearing import BALL_RATIO, CAGE_RATIO
from sim.models.facility import Facility
from soloc_client import KIND_SOLOC, mint

RUNS_S = [(sc.seconds(a), sc.seconds(b)) for a, b in sc.RUNS]
SHIFT_S = (RUNS_S[0][0], RUNS_S[-1][1])
BURST_S = (sc.seconds(sc.BURST[0]), sc.seconds(sc.BURST[1]))
CAPTURES_S = [sc.seconds(t) for t in sc.CAPTURES]
OMEGA = 2 * math.pi * sc.SHAFT_HZ
SUB_TICKS = sc.PART_CADENCE_S * sc.BURST_HZ             # burst rows per cadence tick
SUB_NS = 10**9 // sc.BURST_HZ
CAPTURE_TICKS = sc.BASE_TICK_S * sc.CAPTURE_HZ          # capture rows per driver tick
CAPTURE_NS = 10**9 // sc.CAPTURE_HZ


def _omega(t: float) -> float:
    return sum(OMEGA * min(max(min((t - a) / sc.RAMP_S, (b - t) / sc.RAMP_S), 0.0), 1.0) for a, b in RUNS_S)


def _angle(t: float) -> float:
    r, w, out = sc.RAMP_S, OMEGA, 0.0
    up = w * r / 2
    for a, b in RUNS_S:
        if t <= a:
            continue
        if t < a + r:
            out += w * (t - a) ** 2 / (2 * r)
        elif t < b - r:
            out += up + w * (t - a - r)
        elif t < b:
            out += 2 * up + w * (b - a - 2 * r) - w * (b - t) ** 2 / (2 * r)
        else:
            out += w * (b - a - r)
    return out


def omega(t_s):
    """Shaft speed (rad/s) at `t_s` (a number or an array)."""
    if np.ndim(t_s) == 0:
        return _omega(float(t_s))
    return np.array([_omega(float(t)) for t in np.ravel(t_s)]).reshape(np.shape(t_s))


def angle(t_s):
    """Shaft angle (rad) since T0: the exact integral of `omega`."""
    if np.ndim(t_s) == 0:
        return _angle(float(t_s))
    return np.array([_angle(float(t)) for t in np.ravel(t_s)]).reshape(np.shape(t_s))


def rot_x(a: float) -> list[float]:
    return [math.cos(a / 2), math.sin(a / 2), 0.0, 0.0]


def in_shift(t_s: float) -> bool:
    return SHIFT_S[0] <= t_s <= SHIFT_S[1]


def cage_pose(model: br.LineBearings, k: int, t_s: float):
    """Bearing k's cage, turned by its slipped angle."""
    i = model.at(t_s)
    ratio = CAGE_RATIO * (1 - model.cage_slip[k])
    return [0.0, 0.0, 0.0], rot_x(ratio * model.phi[i]), [0.0, 0.0, 0.0], [ratio * model.w[i], 0.0, 0.0], None


def ball_pose(model: br.LineBearings, k: int, j: int, mass_kg: float, t_s: float):
    """Ball j of bearing k in its cage: on its pocket's wandered angle, seated on the outer race
    when loaded, spinning with its slip; `acceleration` is its contact load over its mass."""
    i = model.at(t_s)
    eps, deps = (e[k, j] for e in model.wander_at(model.phi[i]))
    q = model.q[i, k, j]
    r = sc.PITCH_D_M / 2
    if q > 0:
        r += model.half_clearance / 2 - model.dd[k, j] / 2 - (q / br.K_OUTER) ** (2 / 3)
    a = model.beta[j] + eps
    e = np.array([0.0, math.cos(a), math.sin(a)])
    tangent = np.array([0.0, -math.sin(a), math.cos(a)])
    spin = BALL_RATIO * (1 - model.spin_slip[k, j])
    return ((r * e).tolist(), rot_x(spin * model.phi[i]), (r * deps * model.w[i] * tangent).tolist(),
            [spin * model.w[i], 0.0, 0.0], (q / mass_kg * e).tolist())


class Part:
    """An entity fixed in its parent frame at `offset`, turned by `base` and, when `spin` is not
    0, rotating about its x by `spin` × the shaft's angle. A `model(t_s)` returning
    `(offset, quaternion, velocity, angular_velocity, acceleration)` overrides that pose; a part
    with `captures` (start times) also reports at `CAPTURE_HZ` for `CAPTURE_S` from each."""

    def __init__(self, name: str, parent_id: bytes, offset, mass_kg: float, dimensions_m,
                 spin: float = 0.0, base: list[float] | None = None):
        self.name = name
        self.id = mint(KIND_SOLOC, sc.AUTHORITY, name)
        self.parent_id, self.offset = parent_id, [float(v) for v in offset]
        self.mass_kg, self.dimensions_m = mass_kg, list(dimensions_m)
        self.spin, self.base = spin, base or [1.0, 0.0, 0.0, 0.0]
        self.model = None
        self.captures: list[int] = []

    @property
    def rotating(self) -> bool:
        return self.spin != 0.0

    def due(self, t_s: int) -> bool:
        if self.rotating and in_shift(t_s):
            return t_s % sc.PART_CADENCE_S == 0
        return t_s % sc.STATIC_CADENCE_S == 0

    def row(self, t_s: float) -> Row:
        if self.model is not None:
            offset, q, v, w, a = self.model(t_s)
        else:
            offset, v, a = self.offset, [0.0, 0.0, 0.0], None
            q = rot_x(self.spin * _angle(t_s)) if self.rotating else self.base
            w = [self.spin * _omega(t_s), 0.0, 0.0]
        optional = {"velocity": v, "angular_velocity": w, "mass_kg": self.mass_kg,
                    "dimensions": self.dimensions_m}
        if a is not None:
            optional["acceleration"] = a
        return Row(self.parent_id, offset, q, units="m", timescale=sc.PART_TIMESCALE, optional=optional)

    def capturing(self, t_s: float) -> bool:
        return any(c <= t_s < c + sc.CAPTURE_S for c in self.captures)

    def samples(self, t_s: int) -> list[tuple[int, Row]]:
        """`(offset_ns, row)` within this tick: `SUB_TICKS` of them in the burst,
        `CAPTURE_TICKS` in a capture."""
        if self.capturing(t_s):
            return [(k * CAPTURE_NS, self.row(t_s + k * CAPTURE_NS / 1e9)) for k in range(CAPTURE_TICKS)]
        if self.rotating and BURST_S[0] <= t_s < BURST_S[1]:
            return [(k * SUB_NS, self.row(t_s + k * SUB_NS / 1e9)) for k in range(SUB_TICKS)]
        return [(0, self.row(t_s))] if self.due(t_s) else []

    def epochs_ns(self) -> np.ndarray:
        """Every stored epoch, ns since T0."""
        grid = np.arange(0, sc.DURATION_S + 1, sc.BASE_TICK_S, dtype=np.int64)
        hourly = grid[grid % sc.STATIC_CADENCE_S == 0]
        if not self.rotating:
            return hourly * 10**9
        shift = grid[(grid >= SHIFT_S[0]) & (grid <= SHIFT_S[1])]
        burst = shift[(shift >= BURST_S[0]) & (shift < BURST_S[1])]
        ticks = np.union1d(hourly[(hourly < SHIFT_S[0]) | (hourly > SHIFT_S[1])], shift)
        sub = (burst[:, None] * 10**9 + np.arange(1, SUB_TICKS)[None] * SUB_NS).ravel()
        cap = (np.array(self.captures, np.int64)[:, None] * 10**9
               + np.arange(sc.CAPTURE_S * sc.CAPTURE_HZ)[None] * CAPTURE_NS).ravel()
        return np.union1d(np.union1d(ticks * 10**9, sub), cap)


class Box:
    """Spawned on Machine B's belt start at `spawn_s`, carried at `PULLEY_R_M` × the shaft's
    angle, and taken by Machine C: its last row (the first tick it is off the belt) is on C."""

    def __init__(self, name: str, b_id: bytes, c_id: bytes, spawn_s: int):
        self.name = name
        self.id = mint(KIND_SOLOC, sc.AUTHORITY, name)
        self.b_id, self.c_id, self.spawn_s = b_id, c_id, spawn_s
        self.start = _angle(spawn_s)
        need = self.start + sc.BELT_M / sc.PULLEY_R_M
        lo, hi = float(spawn_s), float(spawn_s) + 3600.0
        for _ in range(60):                               # the angle is monotone: bisect
            mid = (lo + hi) / 2
            lo, hi = (mid, hi) if _angle(mid) < need else (lo, mid)
        self.arrive_s = hi
        self.end_s = int(math.ceil(hi / sc.BASE_TICK_S) * sc.BASE_TICK_S)

    def due(self, t_s: int) -> bool:
        return self.spawn_s <= t_s <= self.end_s and t_s % sc.BASE_TICK_S == 0

    def sample(self, t_s: int) -> Row | None:
        if t_s < self.spawn_s or t_s > self.end_s or not self.due(t_s):
            return None
        h = sc.BOX_DIMENSIONS_M[2] / 2
        if t_s == self.end_s:
            return Row(self.c_id, list(sc.BOX_ON_C_M), [1.0, 0.0, 0.0, 0.0], units="m",
                       timescale=sc.BOX_TIMESCALE,
                       optional={"velocity": [0.0, 0.0, 0.0], "mass_kg": sc.BOX_MASS_KG,
                                 "dimensions": list(sc.BOX_DIMENSIONS_M)})
        x = sc.PULLEY_R_M * (_angle(t_s) - self.start)
        return Row(self.b_id, [x, 0.0, sc.BELT_TOP_M + h], [1.0, 0.0, 0.0, 0.0], units="m",
                   timescale=sc.BOX_TIMESCALE,
                   optional={"velocity": [sc.PULLEY_R_M * _omega(t_s), 0.0, 0.0],
                             "mass_kg": sc.BOX_MASS_KG, "dimensions": list(sc.BOX_DIMENSIONS_M)})


def box_spawns() -> list[int]:
    """Spawn times: every `BOX_EVERY_S` from the end of a run's start ramp, while the box clears
    the belt `BOX_CLEAR_S` before the stop ramp begins."""
    travel = sc.BELT_M / (sc.PULLEY_R_M * OMEGA)
    out = []
    for a, b in RUNS_S:
        t = a + sc.RAMP_S
        while t + travel + sc.BOX_CLEAR_S <= b - sc.RAMP_S:
            out.append(t)
            t += sc.BOX_EVERY_S
    return out


class Factory:
    def __init__(self):
        self.plant = Facility(sc.FACTORY_VENUE)
        self.parts: list[Part] = []
        self.lines = []
        self.boxes: list[Box] = []
        for k in range(sc.LINES):
            self.lines.append(self.build_line(k))
        self.names = {e.id: e.name for e in self.entities}

    def build_line(self, k: int) -> dict:
        n = sc.line_name(k)
        add = lambda p: (self.parts.append(p), p)[1]
        line = add(Part(n, self.plant.id, (0.0, k * sc.LINE_SPACING_M, 0.0), 0.0, (16.0, 2.0, 0.1)))
        m = {key: add(Part(f"{n}-{key}", line.id, off, 500.0, sc.MACHINE_DIMENSIONS_M[key]))
             for key, off in (("A", sc.MACHINE_A_M), ("B", sc.MACHINE_B_M), ("C", sc.MACHINE_C_M))}
        stator = add(Part(f"{n}-STATOR", m["B"].id, sc.STATOR_ON_B_M, sc.STATOR_MASS_KG,
                          sc.STATOR_DIMENSIONS_M, base=quat_yaw(math.pi / 2)))
        shaft = add(Part(f"{n}-SHAFT", stator.id, (0.0, 0.0, 0.0), 2.0, sc.SHAFT_DIMENSIONS_M, spin=1.0))
        rigid = [shaft, add(Part(f"{n}-ROTOR", shaft.id, (0.0, 0.0, 0.0), 6.0, sc.ROTOR_DIMENSIONS_M))]
        bearings = []
        for j, x in enumerate((-sc.BEARING_X_M, sc.BEARING_X_M)):
            rigid.append(add(Part(f"{n}-IR{j + 1}", shaft.id, (x, 0.0, 0.0), 0.05, sc.RING_DIMENSIONS_M["inner"])))
            outer = add(Part(f"{n}-OR{j + 1}", stator.id, (x, 0.0, 0.0), 0.08, sc.RING_DIMENSIONS_M["outer"]))
            cage = add(Part(f"{n}-CAGE{j + 1}", outer.id, (0.0, 0.0, 0.0), 0.01,
                            (0.010, sc.PITCH_D_M, sc.PITCH_D_M), spin=CAGE_RATIO))
            balls = []
            for i in range(sc.BALLS):
                beta = 2 * math.pi * i / sc.BALLS
                balls.append(add(Part(f"{n}-B{j + 1}-BALL{i + 1}", cage.id,
                                      (0.0, sc.PITCH_D_M / 2 * math.cos(beta), sc.PITCH_D_M / 2 * math.sin(beta)),
                                      0.002, (sc.BALL_D_M,) * 3, spin=BALL_RATIO)))
            bearings.append({"cage": cage, "balls": balls, "outer": outer})
        model = br.LineBearings(n, sc.WEAR[k], rigid, [[b.name for b in bear["balls"]] for bear in bearings],
                                angle, omega, bearings[0]["cage"].epochs_ns())
        shaft.model, shaft.captures = functools.partial(self.shaft_pose, model), CAPTURES_S
        for kb, bear in enumerate(bearings):
            bear["cage"].model = functools.partial(cage_pose, model, kb)
            for j, ball in enumerate(bear["balls"]):
                ball.model = functools.partial(ball_pose, model, kb, j, ball.mass_kg)
        boxes = [Box(f"{n}-BOX-{i + 1:03d}", m["B"].id, m["C"].id, t) for i, t in enumerate(box_spawns())]
        self.boxes += boxes
        return {"name": n, "line": line, "machines": m, "stator": stator, "shaft": shaft,
                "bearings": bearings, "boxes": boxes, "model": model}

    @functools.cached_property
    def captures(self) -> br.Captures:
        """Integrated on first use: every line's captures, ~1–2 min."""
        return br.Captures([l["model"] for l in self.lines], CAPTURES_S, OMEGA)

    def shaft_pose(self, model: br.LineBearings, t_s: float):
        if any(c <= t_s < c + sc.CAPTURE_S for c in CAPTURES_S):
            cap = self.captures
            c_i, k = cap.window(t_s)
            i = cap.system(c_i, model)
            u, v, acc = cap.u[k, i], cap.v[k, i], cap.acc[k, i].mean(axis=0)
            vy, vz, wy, wz = br.centre(v)
            a = [0.0, *acc]
        else:
            u = model.u[model.at(t_s)]
            vy = vz = wy = wz = 0.0
            a = None
        y, z, ty, tz = br.centre(u)
        q = br.quat_mul(br.quat_tilt(ty, tz), br.quat_x(_angle(t_s)))
        return [0.0, float(y), float(z)], q.tolist(), [0.0, float(vy), float(vz)], \
            [_omega(t_s), float(wy), float(wz)], a

    @property
    def entities(self) -> list:
        """Parents before their children."""
        return [self.plant, *self.parts, *self.boxes]

    def result(self) -> str:
        per_line = ", ".join(f"{l['name']} {len(l['boxes'])}" for l in self.lines)
        burst = sum(p.rotating for p in self.parts) * (BURST_S[1] - BURST_S[0]) * sc.BURST_HZ
        return (f"factory: boxes delivered per line {per_line}; burst {burst:,} rows at "
                f"{sc.BURST_HZ} Hz on {sum(p.rotating for p in self.parts)} rotating parts")
