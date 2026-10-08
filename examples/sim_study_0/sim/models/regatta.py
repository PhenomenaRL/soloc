"""The Bedford Basin regatta: an arena. Ten boats and the RC boat leave the BBYC docks, motor to
the start area and, from the warning signal, sail a windward-leeward course under a strategy
each (`sim/strategies/`), then motor home. Course marks exist only while laid; met buoys are
moored all window.

The driver calls `decide(client, t_s)` on every `decide_at(t_s)` tick, after flushing its buffer,
so each policy sees the ledger's latest rows. Boats are stepped by `advance`, once per tick,
from whichever boat samples first.
"""

import math
from datetime import timedelta
from typing import Callable

import numpy as np

from sim import scenario as sc
from sim.models import Row
from sim.models.facility import Facility
from sim.models.sailboat import Boat, polar, unit
from sim.strategies import RegattaObservation, decode_state, regatta_command
from sim.strategies.regatta import tactician
from sim.wind import Wind
from soloc_client import KIND_SOLOC, SolocClient, mint, tai_ns_from_utc

ON_S, OFF_S = sc.seconds(sc.REGATTA_ON), sc.seconds(sc.REGATTA_OFF)
WARNING_S, GUN_S = sc.seconds(sc.REGATTA_WARNING), sc.seconds(sc.REGATTA_GUN)
TIME_LIMIT_S = sc.seconds(sc.REGATTA_TIME_LIMIT)
LAID_S = tuple(sc.seconds(t) for t in sc.MARKS_LAID)


def regatta_due(t_s: int) -> bool:
    cadence = sc.REGATTA_CADENCE_S if ON_S <= t_s <= OFF_S else sc.REGATTA_IDLE_CADENCE_S
    return t_s % cadence == 0


def mark_due(t_s: int) -> bool:
    return LAID_S[0] <= t_s <= LAID_S[1] and t_s % sc.MARK_CADENCE_S == 0


class Course:
    """Course geometry in venue ENU metres; the venue origin is the course centre."""

    def __init__(self):
        self.up, self.right = unit(sc.COURSE_AXIS_DEG), unit(sc.COURSE_AXIS_DEG + 90)
        self.line_mid = -sc.BEAT_M / 2 * self.up
        self.rc = self.line_mid + sc.LINE_M / 2 * self.right
        gate = self.line_mid + sc.GATE_ABOVE_LINE_M * self.up
        self.marks = {
            "MARK-W": self.line_mid + sc.BEAT_M * self.up,
            "MARK-GATE-1": gate + sc.GATE_WIDTH_M / 2 * self.right,
            "MARK-GATE-2": gate - sc.GATE_WIDTH_M / 2 * self.right,
            "MARK-PIN": self.line_mid - sc.LINE_M / 2 * self.right,
        }
        self.legs = ("W", "GATE") * (sc.LAPS - 1) + ("W", "FINISH")
        spread = np.linspace(-sc.STAGING_SPREAD_M / 2, sc.STAGING_SPREAD_M / 2, sc.BOATS)
        self.staging = [self.line_mid - sc.STAGING_BELOW_LINE_M * self.up + x * self.right for x in spread]

    def above(self, p: np.ndarray) -> float:
        """Distance on the course side of the line (negative below it)."""
        return float((p - self.line_mid) @ self.up)

    def crossing(self, p0, p1, upward: bool) -> float | None:
        """Where `p0 → p1` crosses the line between its ends in the given direction, as a fraction."""
        d0, d1 = self.above(p0), self.above(p1)
        if not ((d0 < 0 <= d1) if upward else (d0 >= 0 > d1)):
            return None
        f = d0 / (d0 - d1)
        lateral = float((p0 + f * (p1 - p0) - self.line_mid) @ self.right)
        return f if abs(lateral) <= sc.LINE_M / 2 else None


class Referee:
    """One boat's race: the start, roundings in course order, the finish. Fed every stored row
    while the boat is sailing, so check_sim can replay it from the ledger."""

    def __init__(self, course: Course):
        self.course = course
        self.leg = -1
        self.ocs = False
        self.start_s = self.finish_s = None
        self.roundings: list[tuple[str, float]] = []

    @property
    def next_mark(self) -> str:
        return "START" if self.leg < 0 else self.course.legs[self.leg] if self.finish_s is None else "DONE"

    def update(self, t0: float, p0: np.ndarray, t1: float, p1: np.ndarray):
        c = self.course
        if t1 == GUN_S and self.leg < 0 and c.above(p1) > 0:
            self.ocs = True
        if self.leg < 0:
            f = c.crossing(p0, p1, upward=True)
            if f is not None and t0 + f * (t1 - t0) >= GUN_S:
                self.start_s, self.leg = t0 + f * (t1 - t0), 0
            return
        if self.finish_s is not None:
            return
        name = c.legs[self.leg]
        if name == "FINISH":
            f = c.crossing(p0, p1, upward=False)
            if f is not None:
                self.finish_s = t0 + f * (t1 - t0)
            return
        marks = ["MARK-W"] if name == "W" else ["MARK-GATE-1", "MARK-GATE-2"]
        if min(np.linalg.norm(p1 - c.marks[m]) for m in marks) <= sc.ROUND_RADIUS_M:
            self.roundings.append((name, t1))
            self.leg += 1


def replay(course: Course, t_s: np.ndarray, xy: np.ndarray, upto_s: float = math.inf) -> tuple[Referee, float]:
    """A boat's time-ordered stored rows fed to a fresh referee as the run fed them, up to
    `upto_s`. Also returns where its sailing ended: the finish row, else the time limit."""
    r, end = Referee(course), float(TIME_LIMIT_S)
    for j in range(1, len(t_s)):
        if t_s[j] < WARNING_S:
            continue
        if t_s[j] > min(TIME_LIMIT_S, upto_s):
            break
        r.update(t_s[j - 1], xy[j - 1], t_s[j], xy[j])
        if r.finish_s is not None:
            end = float(t_s[j])
            break
    return r, end


class Marker:
    """A static point on the venue: a course mark while laid, or a moored met buoy."""

    def __init__(self, name: str, venue_id: bytes, xy, mass_kg: float, dimensions_m, due):
        self.name = name
        self.id = mint(KIND_SOLOC, sc.AUTHORITY, name)
        self.venue_id, self.xy = venue_id, np.asarray(xy, float)
        self.mass_kg, self.dimensions_m, self._due = mass_kg, dimensions_m, due

    def due(self, t_s: int) -> bool:
        return self._due(t_s)

    def sample(self, t_s: int) -> Row | None:
        if not self.due(t_s):
            return None
        return Row(self.venue_id, [float(self.xy[0]), float(self.xy[1]), 0.0], [1.0, 0.0, 0.0, 0.0],
                   units="m", timescale=sc.MARK_TIMESCALE,
                   optional={"velocity": [0.0, 0.0, 0.0], "mass_kg": self.mass_kg,
                             "dimensions": list(self.dimensions_m)})


class Regatta:
    def __init__(self, seed: int, policy: Callable | None = None):
        self.venue = Facility(sc.REGATTA_VENUE)
        self.wind = Wind(sc.REGATTA_WIND, self.venue, seed)
        self.course = c = Course()
        vid = self.venue.id
        first = self.venue.enu(*sc.DOCK)[:2]
        berths = [first + k * np.array(sc.BERTH_STEP_M) for k in range(sc.BOATS + 1)]
        self.route = [self.venue.enu(*w)[:2] for w in sc.MOTOR_ROUTE]
        self.rc = Boat(sc.RC_NAME, vid, berths[0], ON_S, sc.RC_MASS_KG, sc.RC_DIMENSIONS_M, regatta_due)
        self.boats = [Boat(sc.boat_name(i), vid, berths[i + 1], ON_S + (i + 1) * sc.DEPART_EVERY_S,
                           sc.BOAT_MASS_KG, sc.BOAT_DIMENSIONS_M, regatta_due) for i in range(sc.BOATS)]
        for b in (self.rc, *self.boats):
            b.group = self
        self.marks = [Marker(n, vid, p, sc.MARK_MASS_KG, sc.MARK_DIMENSIONS_M, mark_due)
                      for n, p in c.marks.items()]
        self.buoys = [Marker(f"METBUOY-{k + 1}", vid, p, sc.BUOY_MASS_KG, sc.BUOY_DIMENSIONS_M, regatta_due)
                      for k, p in enumerate(sc.MET_BUOYS_M)]
        self.referees = {b.name: Referee(c) for b in self.boats}
        self.policies = {b.name: tactician for b in self.boats}
        if policy is not None:
            self.policies[self.boats[-1].name] = policy
        self.dnf: set[str] = set()
        self.done_s: float | None = None
        self.t_s: int | None = None
        self.names = {e.id: e.name for e in self.entities}
        self.t0_ns = tai_ns_from_utc(sc.T0)

    @property
    def entities(self) -> list:
        return [self.venue, self.rc, *self.boats, *self.marks, *self.buoys]

    def racing(self) -> list[Boat]:
        return [b for b in self.boats if b.mode == "sail"]

    # -- stepping ------------------------------------------------------------------------------

    def advance(self, t_s: int):
        """Step every boat to `t_s` (once per tick, inside the venue window)."""
        if t_s == self.t_s or not ON_S <= t_s <= OFF_S:
            return
        dt = 0 if self.t_s is None else t_s - self.t_s
        for b in (self.rc, *self.boats):
            p0 = b.pos
            b.move(dt)
            if b.mode == "sail":
                self.referees[b.name].update(t_s - dt, p0, t_s, b.pos)
        self._transitions(t_s)
        for b in (self.rc, *self.boats):
            twd, tws = self.wind.at(b.pos[0], b.pos[1], t_s)
            b.steer(float(twd), float(tws))
        self.t_s = t_s

    def _home(self, b: Boat):
        b.motor([*self.route[::-1], b.berth], homeward=True)

    def _transitions(self, t_s: int):
        for i, b in enumerate(self.boats):
            if b.mode == "docked" and not b.homeward and t_s >= b.depart_s:
                b.motor([*self.route, self.course.staging[i]], homeward=False)
            elif b.mode == "motor" and not b.route:
                b.mode = "docked" if b.homeward else "hold"
            elif b.mode == "sail" and self.referees[b.name].finish_s is not None:
                self._home(b)
            elif b.mode == "sail" and t_s >= TIME_LIMIT_S:
                self.dnf.add(b.name)
                self._home(b)
        if self.done_s is None and t_s > WARNING_S and all(b.mode != "sail" for b in self.boats):
            self.done_s = t_s
        rc = self.rc
        if rc.mode == "docked" and not rc.homeward and t_s >= rc.depart_s:
            rc.motor([*self.route, self.course.rc], homeward=False)
        elif rc.mode == "motor" and not rc.route:
            rc.mode = "docked" if rc.homeward else "hold"
        elif rc.mode == "hold" and self.done_s is not None and t_s >= self.done_s + sc.RC_LEAVES_AFTER_S:
            self._home(rc)

    # -- the arena -----------------------------------------------------------------------------

    def decide_at(self, t_s: int) -> bool:
        return (WARNING_S <= t_s <= TIME_LIMIT_S and (t_s - WARNING_S) % sc.DECISION_S == 0
                and (t_s == WARNING_S or bool(self.racing())))

    def decide(self, client: SolocClient, t_s: int):
        if t_s == WARNING_S:
            for b in self.boats:
                if b.mode == "hold":
                    b.mode = "sail"
        racers = self.racing()
        if not racers:
            return
        ids = [self.rc.id, *(b.id for b in self.boats), *(m.id for m in self.marks)]
        poses = decode_state(client.current_state(ids), self.names, self.t0_ns)
        marks = {m.name: poses[m.name] for m in self.marks if m.name in poses}
        boats = {n: p for n, p in poses.items() if n not in marks}
        c = self.course

        def wind(east: float, north: float) -> tuple[float, float]:
            twd, tws = self.wind.at(east, north, t_s)
            return float(twd), float(tws)

        for b in racers:
            r = self.referees[b.name]
            obs = RegattaObservation(
                t_s=t_s, decision_s=sc.DECISION_S, me=b.name, boats=boats, marks=marks, next_mark=r.next_mark,
                legs_done=max(r.leg, 0), legs=c.legs, started=r.start_s is not None, ocs=r.ocs,
                gun_s=GUN_S, time_limit_s=TIME_LIMIT_S, course_axis_deg=sc.COURSE_AXIS_DEG,
                line=(self.rc.name, "MARK-PIN"), gate=("MARK-GATE-1", "MARK-GATE-2"),
                windward="MARK-W", wind=wind, polar=polar)
            policy = self.policies[b.name]
            b.command = regatta_command(policy(obs), policy)

    def result(self) -> str:
        done = sorted((r.finish_s - GUN_S, n) for n, r in self.referees.items() if r.finish_s is not None)
        parts = [f"{k + 1}. {n} {timedelta(seconds=round(s))}" for k, (s, n) in enumerate(done)]
        ocs = sorted(n for n, r in self.referees.items() if r.ocs)
        line = "regatta: " + (", ".join(parts) or "no finishers")
        if ocs:
            line += f"; OCS {', '.join(ocs)}"
        if self.dnf:
            line += f"; DNF {', '.join(sorted(self.dnf))}"
        return line
