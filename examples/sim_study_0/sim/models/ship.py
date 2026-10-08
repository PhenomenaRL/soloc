"""Ships sailing sea lanes at sea level on IAU_EARTH, in km.

A ship shuttles along its lane at a constant speed, docking at each end port for a while
before sailing back. At T0 it is either at a berth or already under way at a random point on
the lane (as if it had departed earlier).
"""

import math

from sim.geo import GreatCircle
from sim.models.robot import own_rng
from sim.models.track import Stay, Track, course_rad
from sim.scenario import (DOCK_S, DOCKED_AT_T0, DOCKED_AT_T0_S, DURATION_S, SHIP_CADENCE_S,
                          SHIP_DIMENSIONS_M, SHIP_KM_H, SHIP_MASS_KG, SHIP_TIMESCALE, Lane)


class Sail:
    def __init__(self, start_s: float, path: GreatCircle, speed_km_s: float):
        self.start_s, self.path, self.speed_km_s = start_s, path, speed_km_s
        self.end_s = start_s + path.length_km / speed_km_s
        self.heading_rad = course_rad(path, 0.0)

    def where(self, t_s: float) -> tuple[float, float, float]:
        return (*self.path.at(self.speed_km_s * (t_s - self.start_s)), 0.0)


def _berth(start_s: float, port: str, path: GreatCircle, at_end: bool) -> Stay:
    s = path.length_km if at_end else 0.0
    return Stay(start_s, port, *path.at(s), 0.0, course_rad(path, s))


def ship(name: str, lane: Lane, seed: int) -> Track:
    rng = own_rng(seed, name)
    path, ports = GreatCircle(lane.waypoints), lane.ports
    if rng.random() < 0.5:
        path, ports = path.reversed(), ports[::-1]
    speed = rng.uniform(*SHIP_KM_H) / 3600
    segments = [_berth(-math.inf, ports[0], path, at_end=False)]
    if rng.random() < DOCKED_AT_T0:
        t = rng.uniform(*DOCKED_AT_T0_S)
    else:
        t = -rng.uniform(0, path.length_km) / speed
    while t <= DURATION_S:
        sail = Sail(t, path, speed)
        segments.append(sail)
        segments.append(_berth(sail.end_s, ports[1], path, at_end=True))
        t = sail.end_s + rng.uniform(*DOCK_S)
        path, ports = path.reversed(), ports[::-1]
    return Track(name, SHIP_CADENCE_S, SHIP_TIMESCALE, SHIP_MASS_KG, SHIP_DIMENSIONS_M, segments)
