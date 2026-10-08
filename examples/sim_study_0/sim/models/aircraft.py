"""Airliners flying great-circle legs between real airports on IAU_EARTH, in km.

A leg ramps ground speed from 0 to cruise while climbing to cruise altitude, cruises, then
ramps back down to 0 while descending onto the destination's field elevation. Both altitude
ramps are smoothsteps, so the climb and descent angles fall to zero at the top and are ~10°
(climb) and ~6° (descent) at the runway. Between legs the aircraft is parked on its turnaround.
"""

import math

from sim.geo import GreatCircle
from sim.models.robot import own_rng
from sim.models.track import Stay, Track, course_rad
from sim.scenario import (AIRCRAFT_CADENCE_S, AIRCRAFT_DIMENSIONS_M, AIRCRAFT_MASS_KG,
                          AIRCRAFT_TIMESCALE, AIRPORTS, CLIMB_S, CRUISE_ALT_KM, CRUISE_KM_H,
                          DESCENT_S, FIRST_DEPARTURE_S, LEG_KM, LEGS_PER_AIRCRAFT, TURNAROUND_S,
                          Airport)

CRUISE_KM_S = CRUISE_KM_H / 3600


def _smooth(x: float) -> float:
    return x * x * (3 - 2 * x)


def _track(a: Airport, b: Airport) -> GreatCircle:
    return GreatCircle([(a.lat_deg, a.lon_deg), (b.lat_deg, b.lon_deg)])


class Flight:
    def __init__(self, start_s: float, origin: Airport, dest: Airport):
        self.start_s, self.origin, self.dest = start_s, origin, dest
        self.path = _track(origin, dest)
        self.climb_km = CRUISE_KM_S * CLIMB_S / 2
        self.descent_km = CRUISE_KM_S * DESCENT_S / 2
        cruise_km = self.path.length_km - self.climb_km - self.descent_km
        self.duration_s = CLIMB_S + cruise_km / CRUISE_KM_S + DESCENT_S
        self.end_s = start_s + self.duration_s
        self.heading_rad = course_rad(self.path, 0.0)
        self.arrival_heading_rad = course_rad(self.path, self.path.length_km)

    def where(self, t_s: float) -> tuple[float, float, float]:
        tau = t_s - self.start_s
        descent_tau = tau - (self.duration_s - DESCENT_S)
        if tau < CLIMB_S:
            s = CRUISE_KM_S * tau * tau / (2 * CLIMB_S)
            h = self.origin.h_km + (CRUISE_ALT_KM - self.origin.h_km) * _smooth(tau / CLIMB_S)
        elif descent_tau > 0:
            s = (self.path.length_km - self.descent_km + CRUISE_KM_S * descent_tau
                 - CRUISE_KM_S * descent_tau * descent_tau / (2 * DESCENT_S))
            h = self.dest.h_km + (CRUISE_ALT_KM - self.dest.h_km) * (1 - _smooth(descent_tau / DESCENT_S))
        else:
            s = self.climb_km + CRUISE_KM_S * (tau - CLIMB_S)
            h = CRUISE_ALT_KM
        return (*self.path.at(s), h)


def _parked(start_s: float, airport: Airport, heading_rad: float) -> Stay:
    return Stay(start_s, airport.code, airport.lat_deg, airport.lon_deg, airport.h_km, heading_rad)


def aircraft(name: str, seed: int) -> Track:
    """2-4 legs from a random airport, the first departing somewhere in `FIRST_DEPARTURE_S`
    (before T0 means airborne or already turned round at T0); parked after the last."""
    rng = own_rng(seed, name)
    here = AIRPORTS[rng.integers(len(AIRPORTS))]
    t = rng.uniform(*FIRST_DEPARTURE_S)
    segments = [_parked(-math.inf, here, 0.0)]
    for _ in range(rng.integers(LEGS_PER_AIRCRAFT[0], LEGS_PER_AIRCRAFT[1] + 1)):
        reachable = [a for a in AIRPORTS if LEG_KM[0] <= _track(here, a).length_km <= LEG_KM[1]]
        there = reachable[rng.integers(len(reachable))]
        flight = Flight(t, here, there)
        segments.append(flight)
        segments.append(_parked(flight.end_s, there, flight.arrival_heading_rad))
        t = flight.end_s + rng.uniform(*TURNAROUND_S)
        here = there
    segments[0].heading_rad = segments[1].heading_rad
    return Track(name, AIRCRAFT_CADENCE_S, AIRCRAFT_TIMESCALE, AIRCRAFT_MASS_KG,
                 AIRCRAFT_DIMENSIONS_M, segments)
