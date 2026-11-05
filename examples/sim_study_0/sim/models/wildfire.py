"""The Squamish Valley wildfire: an arena. A ring of perimeter vertices spreads from the ignition
point (Huygens: each vertex moves along its outward normal at the rate of a wind-aligned
ellipse, scaled by the fuel there) and stops for good at a finished trench, non-burnable fuel,
or ground it has already burnt (where two fronts meet). Six crews walk from the ICP and dig line where the incident commander
(`sim/strategies/`) sends them; every 50 m of finished line becomes a trench entity.

Vertices and trenches come from fixed pools (`VERTEX_CAP`, `TRENCH_CAP`), so their names and
ids are known up front; one is born when the fire spawns a vertex or a crew finishes a trench.
Their schedules depend on the run (`dynamic`), unlike every other entity's.

Every 30 s from ignition the group is stepped once, from the first crew to sample: the fire
first (on its 60 s grid), then the crews, each refusing a move into burnt ground or within
`SAFE_M` of a moving vertex, and walking away when one comes within `ESCAPE_M`.
"""

import math
from datetime import timedelta
from typing import Callable

import numpy as np
from matplotlib.path import Path as MplPath

from sim import scenario as sc
from sim.fuel import Fuel
from sim.geo import quat_yaw
from sim.models import Row
from sim.models.facility import Facility
from sim.strategies import WildfireObservation, decode_state, wildfire_commands
from sim.strategies.wildfire import commander
from sim.wind import Wind
from soloc_client import KIND_SOLOC, SolocClient, mint, tai_ns_from_utc

IGNITION_S = sc.seconds(sc.FIRE_IGNITION)
DISPATCH_S = IGNITION_S + sc.FIRE_DISPATCH_AFTER_S


def head_rate(fuel: Fuel, wind: Wind, east, north, t_s: float, tws=None) -> np.ndarray:
    """The fastest spread (m/s), downwind, at points."""
    if tws is None:
        _, tws = wind.at(east, north, t_s)
    return sc.R0_M_MIN / 60 * fuel.factor(east, north) * (1 + sc.WIND_GAIN * tws)


def spread_rate(fuel: Fuel, wind: Wind, east, north, nx, ny, t_s: float, twd=None, tws=None) -> np.ndarray:
    """Outward speed (m/s) along unit normals at points: the support function of the
    wind-aligned ellipse grown in unit time from the point (head ahead, back behind). `twd` and
    `tws` replace the field's wind."""
    field_twd, field_tws = wind.at(east, north, t_s)
    twd = field_twd if twd is None else twd
    tws = field_tws if tws is None else tws
    head = head_rate(fuel, wind, east, north, t_s, tws)
    lb = 1 + sc.LB_PER_M_S * tws
    ecc = np.sqrt(1 - 1 / lb ** 2)
    back = head * (1 - ecc) / (1 + ecc)
    a, c = (head + back) / 2, (head - back) / 2
    d = np.radians(twd)
    wx, wy = -np.sin(d), -np.cos(d)                  # downwind
    along, across = nx * wx + ny * wy, ny * wx - nx * wy
    return c * along + np.sqrt((a * along) ** 2 + (a / lb * across) ** 2)


def row(venue_id: bytes, xy, yaw: float, timescale: str, **optional) -> Row:
    return Row(venue_id, [float(xy[0]), float(xy[1]), 0.0], quat_yaw(yaw), units="m",
               timescale=timescale, optional=optional)


class FireReplay:
    """The fire rebuilt from stored vertex rows (`t_s`, `names`, `xy`, any order): which
    vertices exist and move at each tick, and their ring order. The first `VERTICES` start the
    ring in name order; each later one goes between the two neighbours it was born midway
    between. A vertex moves from its birth to the end of its unbroken run of `FIRE_STEP_S` rows."""

    def __init__(self, t_s: np.ndarray, names: np.ndarray, xy: np.ndarray):
        self.rows = {}
        for n in np.unique(names):
            k = np.flatnonzero(names == n)
            k = k[np.argsort(t_s[k], kind="stable")]
            self.rows[n] = (t_s[k], xy[k])
        self.born = {n: float(t[0]) for n, (t, _) in self.rows.items()}
        self.stop = {}
        for n, (t, _) in self.rows.items():
            gaps = np.flatnonzero(np.diff(t) > sc.FIRE_STEP_S)
            self.stop[n] = float(t[gaps[0]]) if len(gaps) else (float(t[-1]) if t[-1] < sc.DURATION_S else math.inf)
        first = [sc.vertex_name(i) for i in range(sc.VERTICES)]
        self.history = [(self.born[first[0]], list(first))]
        self.unmatched = []
        ring = list(first)
        for t in sorted({b for n, b in self.born.items() if n not in first}):
            pos = self.at(ring, t)
            new = sorted((n for n, b in self.born.items() if b == t), key=lambda n: n)
            slots = []
            for n in new:
                mid = (pos + np.roll(pos, -1, 0)) / 2
                k = int(np.argmin(np.linalg.norm(mid - self.rows[n][1][0], axis=1)))
                if np.linalg.norm(mid[k] - self.rows[n][1][0]) > 1e-6:
                    self.unmatched.append(n)
                slots.append((k, n))
            for k, n in sorted(slots, reverse=True):
                ring.insert(k + 1, n)
            self.history.append((t, list(ring)))

    def ring_at(self, t_s: float) -> list[str]:
        k = max(i for i, (t, _) in enumerate(self.history) if t <= t_s) if t_s >= self.history[0][0] else None
        return [] if k is None else self.history[k][1]

    def at(self, names: list[str], t_s: float) -> np.ndarray:
        """Each vertex's position at its latest row at or before `t_s`."""
        out = np.empty((len(names), 2))
        for i, n in enumerate(names):
            t, xy = self.rows[n]
            out[i] = xy[np.searchsorted(t, t_s, side="right") - 1]
        return out

    def moving(self, n: str, t_s: float) -> bool:
        return self.born[n] <= t_s < self.stop[n]


class Vertex:
    dynamic = True

    def __init__(self, name: str, venue_id: bytes, group):
        self.name = name
        self.id = mint(KIND_SOLOC, sc.AUTHORITY, name)
        self.venue_id, self.group = venue_id, group
        self.born_s = self.stop_s = None
        self.pos, self.vel, self.yaw = np.zeros(2), np.zeros(2), 0.0

    @property
    def moving(self) -> bool:
        return self.born_s is not None and self.stop_s is None

    def due(self, t_s: int) -> bool:
        if self.born_s is None or t_s < self.born_s:
            return False
        if self.stop_s is None or t_s <= self.stop_s:
            return t_s % sc.FIRE_STEP_S == 0
        # Never one step after the stop, so the moving rows end at the first gap over a step.
        return t_s % sc.FIRE_IDLE_CADENCE_S == 0 and t_s > self.stop_s + sc.FIRE_STEP_S

    def sample(self, t_s: int) -> Row | None:
        if self.born_s is None:
            return None
        self.group.advance(t_s)
        if not self.due(t_s):
            return None
        vel = self.vel if self.stop_s is None or t_s == self.stop_s else np.zeros(2)
        return row(self.venue_id, self.pos, self.yaw, sc.FIRE_TIMESCALE,
                   velocity=[float(vel[0]), float(vel[1]), 0.0])


class Trench:
    dynamic = True

    def __init__(self, name: str, venue_id: bytes):
        self.name = name
        self.id = mint(KIND_SOLOC, sc.AUTHORITY, name)
        self.venue_id = venue_id
        self.born_s = None
        self.a = self.b = None

    def due(self, t_s: int) -> bool:
        return self.born_s is not None and t_s >= self.born_s and (
            t_s == self.born_s or t_s % sc.FIRE_IDLE_CADENCE_S == 0)

    def sample(self, t_s: int) -> Row | None:
        if not self.due(t_s):
            return None
        d = self.b - self.a
        return row(self.venue_id, (self.a + self.b) / 2, math.atan2(d[1], d[0]), sc.FIRE_TIMESCALE,
                   velocity=[0.0, 0.0, 0.0],
                   dimensions=[float(np.linalg.norm(d)), sc.TRENCH_WIDTH_M, sc.TRENCH_DEPTH_M])


class Crew:
    def __init__(self, name: str, venue_id: bytes, group, home: np.ndarray):
        self.name = name
        self.id = mint(KIND_SOLOC, sc.AUTHORITY, name)
        self.venue_id, self.group = venue_id, group
        self.pos, self.vel, self.yaw = home.astype(float), np.zeros(2), math.pi / 2
        self.task = "idle"
        self.route: list[np.ndarray] = []           # walked first
        self.dig: list[np.ndarray] = []             # then dug
        self.piece: np.ndarray | None = None        # where the trench being dug starts
        self.piece_m = 0.0

    def due(self, t_s: int) -> bool:
        return t_s % (sc.CREW_CADENCE_S if t_s >= IGNITION_S else sc.CREW_IDLE_CADENCE_S) == 0

    def sample(self, t_s: int) -> Row | None:
        if not self.due(t_s):
            return None
        self.group.advance(t_s)
        return row(self.venue_id, self.pos, self.yaw, sc.CREW_TIMESCALE,
                   velocity=[float(self.vel[0]), float(self.vel[1]), 0.0],
                   mass_kg=sc.CREW_MASS_KG, dimensions=list(sc.CREW_DIMENSIONS_M))

    # -- orders --------------------------------------------------------------------------------

    def order(self, cmd, t_s: int):
        self.close_piece(t_s)
        self.route, self.dig = [], []
        if cmd[0] == "move":
            self.route, self.task = [np.array(p) for p in cmd[1]], "move"
        elif cmd[0] == "dig":
            pts = [np.array(p) for p in cmd[1]]
            self.route, self.dig, self.task = pts[:1], pts[1:], "dig"
        else:
            self.task = "idle"

    def close_piece(self, t_s: int):
        """Finish the trench being dug here, if it has any length."""
        if self.piece is not None and self.piece_m >= 1.0:
            self.group.finish_trench(self.piece, self.pos.copy(), t_s)
        self.piece, self.piece_m = None, 0.0

    # -- stepping ------------------------------------------------------------------------------

    def _along(self, path: list[np.ndarray], dist: float) -> tuple[np.ndarray, list[np.ndarray]]:
        p, path = self.pos, list(path)
        while path and dist > 0:
            gap = path[0] - p
            d = float(np.linalg.norm(gap))
            if d <= dist:
                p, dist = path.pop(0), dist - d
            else:
                p, dist = p + gap * (dist / d), 0.0
        return p, path

    def step(self, dt: float, t_s: int):
        g, start = self.group, self.pos
        near, away = g.nearest_moving(self.pos)
        if near < sc.ESCAPE_M or (self.task == "escape" and near < sc.ESCAPE_M + 20):
            if self.task != "escape":
                self.close_piece(t_s)
                self.route, self.dig, self.task = [], [], "escape"
            self.pos = self.pos + away * sc.WALK_M_S * dt
        elif self.task == "escape":
            self.task = "idle"
        elif self.route:
            p, rest = self._along(self.route, sc.WALK_M_S * dt)
            if g.unsafe(p):
                self.task = "blocked"
            else:
                self.pos, self.route = p, rest
                if rest:
                    self.task = "dig" if self.dig else "move"
                elif self.dig:
                    self.piece, self.piece_m, self.task = self.pos.copy(), 0.0, "dig"
                else:
                    self.task = "idle"
        elif self.dig:
            p, rest = self._along(self.dig, sc.DIG_M_H / 3600 * dt)
            if g.unsafe(p):
                self.task = "blocked"
            else:
                self.piece_m += float(np.linalg.norm(p - self.pos))
                self.pos, self.dig, self.task = p, rest, "dig"
                if self.piece_m >= sc.TRENCH_M:
                    g.finish_trench(self.piece, self.pos.copy(), t_s)
                    self.piece, self.piece_m = self.pos.copy(), 0.0
                if not rest:
                    self.close_piece(t_s)
                    self.task = "idle"
        elif self.task in ("move", "dig", "blocked"):
            self.task = "idle"
        self.vel = (self.pos - start) / dt
        if self.vel.any():
            self.yaw = math.atan2(self.vel[1], self.vel[0])


class Wildfire:
    def __init__(self, seed: int, policy: Callable | None = None):
        self.venue = Facility(sc.FIRE_VENUE)
        self.wind = Wind(sc.FIRE_WIND, self.venue, seed)
        self.fuel = Fuel(self.venue)
        vid = self.venue.id
        icp = np.array(sc.ICP_M)
        self.crews = [Crew(sc.crew_name(k), vid, self, icp + 15.0 * np.array([k % 3, k // 3]))
                      for k in range(sc.CREWS)]
        self.vertices = [Vertex(sc.vertex_name(i), vid, self) for i in range(sc.VERTEX_CAP)]
        self.trenches = [Trench(sc.trench_name(i), vid) for i in range(sc.TRENCH_CAP)]
        self.ring: list[Vertex] = []
        self.lines: list[Trench] = []
        self.policy = policy or commander
        self.memory: dict = {}
        self.contained_s: int | None = None
        self.t_s: int | None = None
        self.path = None
        self.names = {e.id: e.name for e in self.entities}
        self.t0_ns = tai_ns_from_utc(sc.T0)

    @property
    def entities(self) -> list:
        """Crews first: their sample steps the group, which births vertices and trenches."""
        return [self.venue, *self.crews, *self.vertices, *self.trenches]

    # -- the fire ------------------------------------------------------------------------------

    def ignite(self, t_s: int):
        for k in range(sc.VERTICES):
            a = 2 * math.pi * k / sc.VERTICES
            v = self.vertices[k]
            v.born_s, v.pos, v.yaw = t_s, sc.IGNITION_RADIUS_M * np.array([math.cos(a), math.sin(a)]), a
            self.ring.append(v)

    def step_fire(self, t_s: int):
        ring = self.ring
        p = np.array([v.pos for v in ring])
        tangent = np.roll(p, -1, 0) - np.roll(p, 1, 0)
        normal = np.column_stack([tangent[:, 1], -tangent[:, 0]])
        normal /= np.linalg.norm(normal, axis=1, keepdims=True)
        moving = np.array([v.moving for v in ring])
        rate = spread_rate(self.fuel, self.wind, p[:, 0], p[:, 1], normal[:, 0], normal[:, 1],
                           t_s - sc.FIRE_STEP_S)
        new = p + normal * (rate * sc.FIRE_STEP_S)[:, None]
        stop = moving & (self.fuel.factor(new[:, 0], new[:, 1]) == 0)
        # Fronts that meet go out: a step into ground the fire has already burnt.
        stop |= moving & self.path.contains_points(new)
        new[stop] = p[stop]
        if self.lines:
            s = self.crossing(p, new - p)
            hit = moving & ~stop & (s <= 1)
            step = np.linalg.norm(new - p, axis=1)
            back = np.clip(s - 0.5 / np.maximum(step, 1e-9), 0, 1)
            new[hit] = p[hit] + (new[hit] - p[hit]) * back[hit, None]
            stop |= hit
        for k, v in enumerate(ring):
            if not moving[k]:
                continue
            v.vel = (new[k] - p[k]) / sc.FIRE_STEP_S
            v.pos, v.yaw = new[k], math.atan2(normal[k, 1], normal[k, 0])
            if stop[k]:
                v.stop_s = t_s
        self.spawn(t_s)
        if not any(v.moving for v in self.ring):
            self.contained_s = t_s

    def crossing(self, p: np.ndarray, d: np.ndarray) -> np.ndarray:
        """Per vertex, the fraction of its step `p → p + d` at the first finished trench it
        crosses (inf if none); 0 if it already sits on one."""
        a = np.array([t.a for t in self.lines])
        e = np.array([t.b - t.a for t in self.lines])
        cross = lambda u, v: u[..., 0] * v[..., 1] - u[..., 1] * v[..., 0]
        ap = a[None] - p[:, None]
        den = cross(d[:, None], e[None])
        with np.errstate(divide="ignore", invalid="ignore"):
            s, u = cross(ap, e[None]) / den, cross(ap, d[:, None]) / den
        s = np.where((den != 0) & (s >= 0) & (s <= 1) & (u >= 0) & (u <= 1), s, np.inf).min(1)
        # Already on a line (a vertex spawned between two stopped ones).
        t = np.clip(((p[:, None] - a[None]) * e[None]).sum(-1) / (e * e).sum(-1)[None], 0, 1)
        on = np.linalg.norm(a[None] + t[..., None] * e[None] - p[:, None], axis=-1).min(1)
        return np.where(on <= sc.TRENCH_WIDTH_M, 0.0, s)

    def spawn(self, t_s: int):
        """A new vertex midway wherever a stretch of front (a neighbour still moving) has grown
        wider than `VERTEX_GAP_M`."""
        ring, out = self.ring, []
        free = sum(v.born_s is not None for v in self.vertices)
        for k, v in enumerate(ring):
            out.append(v)
            w = ring[(k + 1) % len(ring)]
            stretching = v.moving or w.moving
            if stretching and float(np.linalg.norm(w.pos - v.pos)) > sc.VERTEX_GAP_M and free < sc.VERTEX_CAP:
                n = self.vertices[free]
                free += 1
                n.born_s, n.pos, n.yaw = t_s, (v.pos + w.pos) / 2, (v.yaw + w.yaw) / 2
                out.append(n)
        self.ring = out

    def finish_trench(self, a: np.ndarray, b: np.ndarray, t_s: int):
        n = len(self.lines)
        if n >= sc.TRENCH_CAP:
            return
        tr = self.trenches[n]
        tr.a, tr.b, tr.born_s = a, b, t_s
        self.lines.append(tr)

    # -- what the crews may do -----------------------------------------------------------------

    def nearest_moving(self, p: np.ndarray) -> tuple[float, np.ndarray]:
        """Distance to the nearest moving vertex, and the unit vector away from it."""
        q = np.array([v.pos for v in self.ring if v.moving] or [[np.inf, np.inf]])
        d = np.linalg.norm(q - p, axis=1)
        k = int(np.argmin(d))
        if not np.isfinite(d[k]):
            return math.inf, np.zeros(2)
        return float(d[k]), (p - q[k]) / max(d[k], 1e-9)

    def burning(self, p) -> bool:
        return self.path is not None and bool(self.path.contains_point(p))

    def unsafe(self, p: np.ndarray) -> bool:
        return self.nearest_moving(p)[0] < sc.SAFE_M or self.burning(p)

    # -- stepping ------------------------------------------------------------------------------

    def advance(self, t_s: int):
        """Step to `t_s`: once per 30 s tick from ignition."""
        if t_s == self.t_s or t_s < IGNITION_S or t_s % sc.CREW_CADENCE_S:
            return
        if self.t_s is None:
            self.ignite(t_s)
        elif t_s % sc.FIRE_STEP_S == 0 and self.contained_s is None:
            self.step_fire(t_s)
        self.path = MplPath(np.array([v.pos for v in self.ring]))
        if self.t_s is not None:
            for c in self.crews:
                c.step(t_s - self.t_s, t_s)
        self.t_s = t_s

    # -- the arena -----------------------------------------------------------------------------

    def decide_at(self, t_s: int) -> bool:
        return (t_s >= DISPATCH_S and (t_s - DISPATCH_S) % sc.FIRE_DECISION_S == 0
                and self.contained_s is None)

    def decide(self, client: SolocClient, t_s: int):
        ids = [*(c.id for c in self.crews), *(v.id for v in self.ring), *(tr.id for tr in self.lines)]
        poses = decode_state(client.current_state(ids), self.names, self.t0_ns)
        lines = []
        for tr in self.lines:
            p = poses[tr.name]
            half = p.dimensions[0] / 2 * np.array([math.sin(math.radians(p.heading_deg)),
                                                   math.cos(math.radians(p.heading_deg))])
            lines.append((tuple(p.xy - half), tuple(p.xy + half)))
        f, w = self.fuel, self.wind

        def wind(e, n):
            twd, tws = w.at(e, n, t_s)
            return float(twd), float(tws)

        obs = WildfireObservation(
            t_s=t_s, decision_s=sc.FIRE_DECISION_S, memory=self.memory,
            crews={c.name: poses[c.name] for c in self.crews},
            tasks={c.name: c.task for c in self.crews},
            perimeter=[poses[v.name] for v in self.ring],
            moving=frozenset(v.name for v in self.ring if v.moving), lines=lines,
            ignition_s=IGNITION_S, icp=tuple(sc.ICP_M), safe_m=sc.SAFE_M, escape_m=sc.ESCAPE_M,
            walk_m_s=sc.WALK_M_S, dig_m_h=sc.DIG_M_H, wind=wind,
            fuel=lambda e, n: float(f.factor(e, n)),
            spread=lambda e, n, nx, ny, twd=None, tws=None: float(spread_rate(f, w, e, n, nx, ny, t_s, twd, tws)),
            burning=lambda e, n: self.burning((e, n)))
        cmds = wildfire_commands(self.policy(obs), obs.crews, self.policy)
        by_name = {c.name: c for c in self.crews}
        for name, cmd in cmds.items():
            by_name[name].order(cmd, t_s)

    def area_ha(self) -> float:
        p = np.array([v.pos for v in self.ring])
        return float(abs(np.dot(p[:, 0], np.roll(p[:, 1], -1)) - np.dot(p[:, 1], np.roll(p[:, 0], -1))) / 2e4)

    def result(self) -> str:
        km = sum(float(np.linalg.norm(t.b - t.a)) for t in self.lines) / 1000
        if self.contained_s is not None:
            when = sc.T0 + timedelta(seconds=self.contained_s)
            head = (f"contained {when:%m-%d %H:%M} UTC, "
                    f"{timedelta(seconds=self.contained_s - IGNITION_S)} after ignition")
        else:
            head = "not contained by the window's end"
        return (f"wildfire: {head}; {self.area_ha():.1f} ha burnt, {km:.2f} km of line in "
                f"{len(self.lines)} trenches, {len(self.ring)} vertices")
