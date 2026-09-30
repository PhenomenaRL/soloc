"""Robots, in metres relative to their host's frame.

- `Robot`: a ground rover working a patterned job in its host site's ENU frame, by its index at
  the site (`scenario.ROBOT_ROLES`): patrolling the fence, surveying a cell of the road grid,
  or running logistics tours from the depot along the roads.
- `Crawler`: a hull robot looping around its host spacecraft in the host's body frame. The first
  crawler on a lander disembarks onto the lander's facility and surveys the one grid cell the
  site's own surveyors leave free.

Each robot draws its whole itinerary up front from its own RNG stream, so its track depends only
on the seed and its name, not on the rest of the roster.
"""

import bisect
import hashlib
import math

import numpy as np

from geo import quat_from_matrix, quat_yaw
from models import Row
from scenario import (AUTHORITY, CRAWLER_CADENCE_S, CRAWLER_DIMENSIONS_M, CRAWLER_MASS_KG,
                      CRAWLER_SPEED_M_S, CRAWLER_TIMESCALE, DEPOT_M, DISEMBARK_OFFSET_M,
                      DURATION_S, LOGISTICS_STATIONS, PATROL_CORNER_PAUSE_S, PATROL_FENCE_M,
                      PATTERN_UNDER_WAY_S, ROBOT_CADENCE_S, ROBOT_DIMENSIONS_M, ROBOT_ROLES,
                      ROBOT_TIMESCALE, SITE_ROADS_M, SPACECRAFT_DIMENSIONS_M, SURVEY_INSET_M,
                      SURVEY_ROW_SPACING_M, SURVEY_TURN_PAUSE_S, RoverSpec)
from soloc_client import KIND_SOLOC, mint


def own_rng(seed: int, name: str) -> np.random.Generator:
    name_key = int.from_bytes(hashlib.sha256(name.encode()).digest()[:8], "big")
    return np.random.default_rng([seed, name_key])


class Leg:
    """Straight-line travel `p0 → p1` over `[t0, t1)`; a pause has `p0 == p1`."""

    __slots__ = ("t0", "t1", "p0", "p1", "yaw", "speed")

    def __init__(self, t0, t1, p0, p1, yaw, speed):
        self.t0, self.t1, self.p0, self.p1, self.yaw, self.speed = t0, t1, p0, p1, yaw, speed


Stop = tuple[np.ndarray, tuple[float, float]]   # (site ENU xy, pause range; (0, 0) = drive through)
NO_PAUSE = (0.0, 0.0)


def _xy(x: float, y: float) -> np.ndarray:
    return np.array([x, y], dtype=float)


def survey_cells() -> list[tuple[float, float, float, float]]:
    """The road-grid cells as `(x0, x1, y0, y1)`. The last one (NE of the origin) is left to a
    disembarked crawler, next to where the lander sits."""
    spans = list(zip(SITE_ROADS_M, SITE_ROADS_M[1:]))
    return [(x0, x1, y0, y1) for x0, x1 in spans for y0, y1 in spans]


def patrol(rng, lap: int) -> list[Stop]:
    """Laps of a square `5 m × lap` inside the fence, odd laps clockwise, from a random corner."""
    w = PATROL_FENCE_M - 5.0 * lap
    corners = [_xy(w, w), _xy(-w, w), _xy(-w, -w), _xy(w, -w)]
    if lap % 2:
        corners.reverse()
    k = int(rng.integers(4))
    return [(c, PATROL_CORNER_PAUSE_S) for c in corners[k:] + corners[:k]]


def survey(rng, cell) -> list[Stop]:
    """Back-and-forth rows across the cell, alternately along x or y by a coin toss, then the
    same rows swept back in reverse, so the loop never cuts across the cell."""
    x0, x1, y0, y1 = (v + s * SURVEY_INSET_M for v, s in zip(cell, (1, -1, 1, -1)))
    along_x = bool(rng.integers(2))
    lo, hi = (y0, y1) if along_x else (x0, x1)
    a, b = (x0, x1) if along_x else (y0, y1)
    stops = []
    for i, c in enumerate(np.arange(lo, hi + 1e-9, SURVEY_ROW_SPACING_M)):
        ends = (a, b) if i % 2 == 0 else (b, a)
        for e in ends:
            stops.append((_xy(e, c) if along_x else _xy(c, e), SURVEY_TURN_PAUSE_S))
    return stops + stops[-2:0:-1]


def logistics(rng, rover: RoverSpec) -> list[Stop]:
    """A tour from the depot to a few road intersections and back, loading at every stop.
    Each hop goes along x then along y, so it stays on the roads."""
    crossings = [_xy(x, y) for x in SITE_ROADS_M for y in SITE_ROADS_M if (x, y) != DEPOT_M]
    n = int(rng.integers(LOGISTICS_STATIONS[0], LOGISTICS_STATIONS[1] + 1))
    tour = [_xy(*DEPOT_M)] + [crossings[i] for i in rng.permutation(len(crossings))[:n]]
    stops = []
    for p, q in zip(tour, tour[1:] + tour[:1]):
        stops.append((_xy(q[0], p[1]), NO_PAUSE))
        stops.append((q, rover.pause_s))
    return stops


class Itinerary:
    """Drives the `stops` loop from `start` (site ENU, m) at `speed`, beginning at time `t`:
    straight legs between stops, pausing at each for a time drawn from its range."""

    def __init__(self, rng, stops: list[Stop], speed: float, start: np.ndarray, t: float):
        self.legs = []
        p, yaw, k = start, 0.0, 0
        while t <= DURATION_S:
            q, pause = stops[k % len(stops)]
            k += 1
            dist = float(np.linalg.norm(q - p))
            if dist > 1e-9:
                yaw = math.atan2(q[1] - p[1], q[0] - p[0])
                self.legs.append(Leg(t, t + dist / speed, p, q, yaw, speed))
                t, p = t + dist / speed, q
            if pause[1] > 0:
                dwell = rng.uniform(*pause)
                self.legs.append(Leg(t, t + dwell, p, p, yaw, 0.0))
                t += dwell
        self.starts = [leg.t0 for leg in self.legs]

    def row(self, frame_id: bytes, t_s: int, rover: RoverSpec, dimensions, timescale) -> Row:
        leg = self.legs[bisect.bisect_right(self.starts, t_s) - 1]
        frac = (t_s - leg.t0) / (leg.t1 - leg.t0)
        x, y = leg.p0 + (leg.p1 - leg.p0) * frac
        v = leg.speed
        return Row(frame_id, [float(x), float(y), 0.0], quat_yaw(leg.yaw),
                   units="m", timescale=timescale,
                   optional={"velocity": [v * math.cos(leg.yaw), v * math.sin(leg.yaw), 0.0],
                             "mass_kg": rover.mass_kg,
                             "dimensions": list(dimensions)})


class Robot:
    def __init__(self, name: str, host_id: bytes, rover: RoverSpec, seed: int, index: int):
        self.name = name
        self.id = mint(KIND_SOLOC, AUTHORITY, name)
        self.host_id = host_id
        self.rover = rover
        self.role = ROBOT_ROLES[index]
        slot = index - ROBOT_ROLES.index(self.role)     # which patroller, surveyor, ...
        rng = own_rng(seed, name)
        if self.role == "patrol":
            stops = patrol(rng, slot)
        elif self.role == "survey":
            stops = survey(rng, survey_cells()[slot])
        else:
            stops = logistics(rng, rover)
        # Started some time before T0, from its last stop, so it is mid-pattern at T0.
        speed = rng.uniform(*rover.speed_m_s)
        self.itinerary = Itinerary(rng, stops, speed, stops[-1][0], -rng.uniform(*PATTERN_UNDER_WAY_S))

    def due(self, t_s: int) -> bool:
        return t_s % ROBOT_CADENCE_S == 0

    def sample(self, t_s: int) -> Row | None:
        if not self.due(t_s):
            return None
        return self.itinerary.row(self.host_id, t_s, self.rover, ROBOT_DIMENSIONS_M, ROBOT_TIMESCALE)


# The hull band: the loop around the box in the host's body x–z plane, as
# (start corner, unit tangent, outward normal, length) per face, walked top → front → bottom → back.
_HX, _HY, _HZ = (d / 2 for d in SPACECRAFT_DIMENSIONS_M)
_BAND = [
    (np.array([-_HX, 0, _HZ]), np.array([1.0, 0, 0]), np.array([0, 0, 1.0]), 2 * _HX),
    (np.array([_HX, 0, _HZ]), np.array([0, 0, -1.0]), np.array([1.0, 0, 0]), 2 * _HZ),
    (np.array([_HX, 0, -_HZ]), np.array([-1.0, 0, 0]), np.array([0, 0, -1.0]), 2 * _HX),
    (np.array([-_HX, 0, -_HZ]), np.array([0, 0, 1.0]), np.array([-1.0, 0, 0]), 2 * _HZ),
]
BAND_LENGTH_M = sum(face[3] for face in _BAND)
HULL_REACH_M = float(np.linalg.norm([_HX, _HY, _HZ]))   # the farthest a crawler gets from its host


class Crawler:
    """Loops the hull band at a fixed body-y offset and constant speed (either direction), with
    body z along the face normal and body x along the direction of travel.

    `disembark` is `(facility, t_s, rover)`: from `t_s` on, the crawler is framed on the
    facility, drives from `DISEMBARK_OFFSET_M` to the site's free survey cell and surveys it."""

    def __init__(self, name: str, host, seed: int, disembark=None):
        self.name = name
        self.id = mint(KIND_SOLOC, AUTHORITY, name)
        self.host = host
        rng = own_rng(seed, name)
        self.speed = rng.uniform(*CRAWLER_SPEED_M_S) * rng.choice([-1.0, 1.0])
        self.s0 = rng.uniform(0, BAND_LENGTH_M)
        self.y = rng.uniform(-0.8, 0.8) * _HY
        self.disembark = disembark
        if disembark:
            _, t_s, rover = disembark
            self.itinerary = Itinerary(rng, survey(rng, survey_cells()[-1]),
                                       rng.uniform(*rover.speed_m_s), _xy(*DISEMBARK_OFFSET_M), t_s)

    def due(self, t_s: int) -> bool:
        return t_s % CRAWLER_CADENCE_S == 0

    def frame_at(self, t_s: int) -> bytes:
        if self.disembark and t_s >= self.disembark[1]:
            return self.disembark[0].id
        return self.host.id

    def sample(self, t_s: int) -> Row | None:
        if not self.due(t_s):
            return None
        if self.disembark and t_s >= self.disembark[1]:
            facility, _, rover = self.disembark
            return self.itinerary.row(facility.id, t_s, rover, CRAWLER_DIMENSIONS_M, CRAWLER_TIMESCALE)

        s = (self.s0 + self.speed * t_s) % BAND_LENGTH_M
        for corner, tangent, normal, length in _BAND:
            if s < length:
                break
            s -= length
        p = corner + tangent * s + np.array([0.0, self.y, 0.0])
        x = tangent * math.copysign(1.0, self.speed)
        attitude = np.column_stack([x, np.cross(normal, x), normal])
        return Row(self.host.id, p.tolist(), quat_from_matrix(attitude),
                   units="m", timescale=CRAWLER_TIMESCALE,
                   optional={"velocity": (tangent * self.speed).tolist(),
                             "mass_kg": CRAWLER_MASS_KG,
                             "dimensions": list(CRAWLER_DIMENSIONS_M)})
