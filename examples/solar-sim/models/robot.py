"""A ground rover doing random-waypoint roving in its host site's ENU frame, in metres.

The whole itinerary is drawn up front from the robot's own RNG stream, so a robot's track
depends only on the seed and its name, not on the rest of the roster.
"""

import bisect
import hashlib
import math

import numpy as np

from geo import quat_yaw
from models import Row
from scenario import (AUTHORITY, DURATION_S, ROBOT_CADENCE_S, ROBOT_DIMENSIONS_M, ROBOT_TIMESCALE,
                      SITE_HALF_WIDTH_M, RoverSpec)
from soloc_client import KIND_SOLOC, mint


class Leg:
    """Straight-line travel `p0 → p1` over `[t0, t1)`; a pause has `p0 == p1`."""

    __slots__ = ("t0", "t1", "p0", "p1", "yaw", "speed")

    def __init__(self, t0, t1, p0, p1, yaw, speed):
        self.t0, self.t1, self.p0, self.p1, self.yaw, self.speed = t0, t1, p0, p1, yaw, speed


class Robot:
    def __init__(self, name: str, host_id: bytes, rover: RoverSpec, seed: int):
        self.name = name
        self.id = mint(KIND_SOLOC, AUTHORITY, name)
        self.host_id = host_id
        self.rover = rover
        name_key = int.from_bytes(hashlib.sha256(name.encode()).digest()[:8], "big")
        self.legs = self._itinerary(np.random.default_rng([seed, name_key]))
        self.starts = [leg.t0 for leg in self.legs]

    def _waypoint(self, rng) -> np.ndarray:
        return rng.uniform(-SITE_HALF_WIDTH_M, SITE_HALF_WIDTH_M, 2)

    def _itinerary(self, rng) -> list[Leg]:
        # Start mid-pause at a random spot, facing a random way.
        p = self._waypoint(rng)
        yaw = rng.uniform(-math.pi, math.pi)
        t = -rng.uniform(*self.rover.pause_s)
        legs = []
        while t <= DURATION_S:
            pause = rng.uniform(*self.rover.pause_s)
            legs.append(Leg(t, t + pause, p, p, yaw, 0.0))
            t += pause
            q = self._waypoint(rng)
            d = q - p
            speed = rng.uniform(*self.rover.speed_m_s)
            yaw = math.atan2(d[1], d[0])
            dt = float(np.linalg.norm(d)) / speed
            legs.append(Leg(t, t + dt, p, q, yaw, speed))
            t, p = t + dt, q
        return legs

    def sample(self, t_s: int) -> Row | None:
        if t_s % ROBOT_CADENCE_S:
            return None
        leg = self.legs[bisect.bisect_right(self.starts, t_s) - 1]
        frac = (t_s - leg.t0) / (leg.t1 - leg.t0)
        x, y = leg.p0 + (leg.p1 - leg.p0) * frac
        v = leg.speed
        return Row(self.host_id, [float(x), float(y), 0.0], quat_yaw(leg.yaw),
                   units="m", timescale=ROBOT_TIMESCALE,
                   optional={"velocity": [v * math.cos(leg.yaw), v * math.sin(leg.yaw), 0.0],
                             "mass_kg": self.rover.mass_kg,
                             "dimensions": list(ROBOT_DIMENSIONS_M)})
