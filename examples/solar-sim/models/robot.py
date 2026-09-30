"""Robots, in metres relative to their host's frame.

- `Robot`: a ground rover doing random-waypoint roving in its host site's ENU frame.
- `Crawler`: a hull robot looping around its host spacecraft in the host's body frame. The first
  crawler on a lander disembarks onto the lander's facility and roves there like a `Robot`.

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
                      CRAWLER_SPEED_M_S, CRAWLER_TIMESCALE, DISEMBARK_OFFSET_M, DURATION_S,
                      ROBOT_CADENCE_S,
                      ROBOT_DIMENSIONS_M, ROBOT_TIMESCALE, SITE_HALF_WIDTH_M,
                      SPACECRAFT_DIMENSIONS_M, RoverSpec)
from soloc_client import KIND_SOLOC, mint


def own_rng(seed: int, name: str) -> np.random.Generator:
    name_key = int.from_bytes(hashlib.sha256(name.encode()).digest()[:8], "big")
    return np.random.default_rng([seed, name_key])


class Leg:
    """Straight-line travel `p0 → p1` over `[t0, t1)`; a pause has `p0 == p1`."""

    __slots__ = ("t0", "t1", "p0", "p1", "yaw", "speed")

    def __init__(self, t0, t1, p0, p1, yaw, speed):
        self.t0, self.t1, self.p0, self.p1, self.yaw, self.speed = t0, t1, p0, p1, yaw, speed


def _waypoint(rng) -> np.ndarray:
    return rng.uniform(-SITE_HALF_WIDTH_M, SITE_HALF_WIDTH_M, 2)


class Itinerary:
    """Random-waypoint roving from `p` (site ENU, m) facing `yaw`, starting with a pause at `t`."""

    def __init__(self, rng, rover: RoverSpec, p: np.ndarray, yaw: float, t: float):
        self.legs = []
        while t <= DURATION_S:
            pause = rng.uniform(*rover.pause_s)
            self.legs.append(Leg(t, t + pause, p, p, yaw, 0.0))
            t += pause
            q = _waypoint(rng)
            d = q - p
            speed = rng.uniform(*rover.speed_m_s)
            yaw = math.atan2(d[1], d[0])
            dt = float(np.linalg.norm(d)) / speed
            self.legs.append(Leg(t, t + dt, p, q, yaw, speed))
            t, p = t + dt, q
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
    def __init__(self, name: str, host_id: bytes, rover: RoverSpec, seed: int):
        self.name = name
        self.id = mint(KIND_SOLOC, AUTHORITY, name)
        self.host_id = host_id
        self.rover = rover
        rng = own_rng(seed, name)
        # Start mid-pause at a random spot, facing a random way.
        p = _waypoint(rng)
        yaw = rng.uniform(-math.pi, math.pi)
        self.itinerary = Itinerary(rng, rover, p, yaw, -rng.uniform(*rover.pause_s))

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
    facility and roves its site from `DISEMBARK_OFFSET_M`."""

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
            self.itinerary = Itinerary(rng, rover, np.array(DISEMBARK_OFFSET_M), 0.0, t_s)

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
