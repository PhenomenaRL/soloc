"""Vehicles that move over Earth's surface along great circles: aircraft and ships.

A track is a sequence of segments, each in force from its start until the next one's. A segment
gives `where(t) -> (lat, lon, h_km)` and a `heading_rad` to face while at rest; the track turns that into a row on IAU_EARTH. Velocity
comes from a central difference of the position, and attitude is forward-right-down along the
direction of travel (heading, plus the climb angle for aircraft). A vehicle at rest keeps the
heading its segment names.
"""

import bisect
import math

import numpy as np

from sim.geo import EARTH, enu_basis, frd, geodetic_to_fixed, quat_from_matrix
from sim.models import Row
from sim.scenario import AUTHORITY
from soloc_client import KIND_SOLOC, mint

AT_REST_KM_S = 1e-6   # below this (1 mm/s) a vehicle is parked and keeps its segment's heading


def course_rad(path, s_km: float) -> float:
    """Heading (clockwise from north) of travel along `path` at `s_km`."""
    a = geodetic_to_fixed(EARTH, *path.at(s_km - 1.0))
    b = geodetic_to_fixed(EARTH, *path.at(s_km + 1.0))
    east, north, _ = enu_basis(*path.at(s_km)).T @ (b - a)
    return math.atan2(east, north)


class Stay:
    """At rest at a place (airport or berth) from `start_s`, facing `heading_rad`."""

    def __init__(self, start_s: float, place: str, lat: float, lon: float, h_km: float = 0.0,
                 heading_rad: float = 0.0):
        self.start_s, self.place = start_s, place
        self.lat, self.lon, self.h_km, self.heading_rad = lat, lon, h_km, heading_rad

    def where(self, t_s: float) -> tuple[float, float, float]:
        return self.lat, self.lon, self.h_km


class Track:
    def __init__(self, name: str, cadence_s: int, timescale: str, mass_kg: float, dimensions_m,
                 segments: list):
        self.name = name
        self.id = mint(KIND_SOLOC, AUTHORITY, name)
        self.cadence_s, self.timescale = cadence_s, timescale
        self.mass_kg, self.dimensions_m = mass_kg, dimensions_m
        self.segments = segments
        self.starts = [s.start_s for s in segments]

    def segment_at(self, t_s: float):
        return self.segments[bisect.bisect_right(self.starts, t_s) - 1]

    def where(self, t_s: float) -> tuple[float, float, float]:
        return self.segment_at(t_s).where(t_s)

    def fixed(self, t_s: float) -> np.ndarray:
        return geodetic_to_fixed(EARTH, *self.where(t_s))

    def due(self, t_s: int) -> bool:
        return t_s % self.cadence_s == 0

    def sample(self, t_s: int) -> Row | None:
        if not self.due(t_s):
            return None
        lat, lon, h = self.where(t_s)
        p = geodetic_to_fixed(EARTH, lat, lon, h)
        v = self.fixed(t_s + 0.5) - self.fixed(t_s - 0.5)          # km/s
        east, north, up = enu_basis(lat, lon).T @ v
        ground = math.hypot(east, north)
        segment = self.segment_at(t_s)
        if ground > AT_REST_KM_S:
            heading, pitch = math.atan2(east, north), math.atan2(up, ground)
        else:
            heading, pitch = segment.heading_rad, 0.0
        return Row(EARTH.frame_id, p.tolist(), quat_from_matrix(frd(lat, lon, heading, pitch)),
                   units="km", timescale=self.timescale,
                   optional={"velocity": (v * 1000).tolist(),
                             "mass_kg": self.mass_kg,
                             "dimensions": list(self.dimensions_m)})
