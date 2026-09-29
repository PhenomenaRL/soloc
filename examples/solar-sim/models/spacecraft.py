"""Spacecraft: Kepler orbits in IAU body-fixed frames, launches from a pad, and a landing.

A craft is a sequence of phases, each in force from its start until the next one's. A phase
names the parent frame, so moving from a pad (framed on the facility) to an ascent (framed on
IAU_EARTH), or from a descent to the ground, is a reparenting: one topology event.

Attitude is nadir-pointing LVLH throughout. On a pad and after touchdown it is frozen at its
liftoff/touchdown value and stored relative to the facility's ENU frame, so the rows are
continuous across the reparenting.
"""

import bisect
import math

import numpy as np

from geo import Kepler, RadialHermite, lvlh, plane_through, quat_from_matrix, spin
from models import Row
from scenario import (AUTHORITY, HOST_CADENCE_S, MANOEUVRE_CADENCE_S, SPACECRAFT_CADENCE_S,
                      SPACECRAFT_DIMENSIONS_M, SPACECRAFT_TIMESCALE, LanderSpec, LaunchSpec,
                      OrbiterSpec, seconds)
from soloc_client import KIND_SOLOC, mint

ZERO = np.zeros(3)


class Orbit:
    def __init__(self, start_s: float, kepler: Kepler, cadence_s: int | None = None):
        self.start_s, self.kepler, self.cadence_s = start_s, kepler, cadence_s

    def state(self, t_s):
        body = self.kepler.body
        r, v = self.kepler.fixed(t_s)
        return body.frame_id, r, v, lvlh(spin(body, t_s) @ self.kepler.normal, r)


class Path:
    """A `RadialHermite` from `start_s`, with LVLH attitude about the given inertial orbit normal."""

    def __init__(self, start_s: float, body, path: RadialHermite, normal: np.ndarray,
                 cadence_s: int | None = MANOEUVRE_CADENCE_S):
        self.start_s, self.body, self.path, self.normal, self.cadence_s = (
            start_s, body, path, normal, cadence_s)

    def state(self, t_s):
        r, v = self.path.state(t_s - self.start_s)
        return self.body.frame_id, r, v, lvlh(spin(self.body, t_s) @ self.normal, r)


class Parked:
    """At rest on a facility's origin, attitude fixed in the facility's ENU frame."""

    def __init__(self, start_s: float, facility, attitude_enu: np.ndarray, cadence_s: int | None = None):
        self.start_s, self.facility, self.attitude, self.cadence_s = (
            start_s, facility, attitude_enu, cadence_s)

    def state(self, t_s):
        return self.facility.id, ZERO, ZERO, self.attitude


class Spacecraft:
    def __init__(self, name: str, mass_kg: float, phases: list, events: dict[str, float] | None = None,
                 facility=None):
        self.name = name
        self.id = mint(KIND_SOLOC, AUTHORITY, name)
        self.mass_kg = mass_kg
        self.phases = phases
        self.starts = [p.start_s for p in phases]
        self.events = events or {}       # liftoff/insertion or deorbit/perilune/touchdown, in t_s
        self.facility = facility         # the pad or landing site, if any
        self.body = facility.spec.body if facility else phases[0].kepler.body
        self.cadence_s = SPACECRAFT_CADENCE_S   # the roster lowers this for crawler hosts

    def phase_at(self, t_s: float):
        return self.phases[bisect.bisect_right(self.starts, t_s) - 1]

    def due(self, t_s: int) -> bool:
        return t_s % (self.phase_at(t_s).cadence_s or self.cadence_s) == 0

    def sample(self, t_s: int) -> Row | None:
        if not self.due(t_s):
            return None
        frame_id, r, v, attitude = self.phase_at(t_s).state(t_s)
        return Row(frame_id, r.tolist(), quat_from_matrix(attitude),
                   units="km", timescale=SPACECRAFT_TIMESCALE,
                   optional={"velocity": (v * 1000).tolist(),
                             "mass_kg": self.mass_kg,
                             "dimensions": list(SPACECRAFT_DIMENSIONS_M)})


def orbiter(spec: OrbiterSpec) -> Spacecraft:
    kepler = Kepler.from_altitudes(spec.body, spec.peri_alt_km, spec.apo_alt_km, spec.inc_deg,
                                   spec.raan_deg, spec.argp_deg, spec.m0_deg)
    return Spacecraft(spec.name, spec.mass_kg, [Orbit(0, kepler)])


def launcher(spec: LaunchSpec, facility) -> Spacecraft:
    body = facility.spec.body
    t_lift = seconds(spec.liftoff)
    t_ins = t_lift + spec.ascent_s
    pad = facility.position_km
    raan, u_pad = plane_through(spin(body, t_lift).T @ pad, spec.inc_deg, ascending=True)
    kepler = Kepler(body, body.a_km + spec.alt_km, 0.0, spec.inc_deg, raan, 0.0,
                    u_pad + spec.downrange_deg, epoch_s=t_ins)
    ascent = RadialHermite(pad, ZERO, *kepler.fixed(t_ins), spec.ascent_s)
    on_pad = facility.basis.T @ lvlh(spin(body, t_lift) @ kepler.normal, pad)
    return Spacecraft(spec.name, spec.mass_kg, [
        Parked(0, facility, on_pad),
        Path(t_lift, body, ascent, kepler.normal),
        Orbit(t_ins, kepler),
    ], events={"liftoff": t_lift, "insertion": t_ins}, facility=facility)


def lander(spec: LanderSpec, facility) -> Spacecraft:
    body = facility.spec.body
    r_orbit = body.a_km + spec.alt_km
    r_peri = body.a_km + spec.perilune_alt_km
    a_coast = (r_orbit + r_peri) / 2
    t_deorbit = seconds(spec.deorbit)
    t_peri = t_deorbit + math.pi * math.sqrt(a_coast ** 3 / body.gm)
    # Touchdown on the crawler grid, so the lander's parked rows share every crawler epoch.
    t_down = math.ceil((t_peri + spec.powered_s) / HOST_CADENCE_S) * HOST_CADENCE_S

    # The plane holds the site at touchdown on a southbound pass; perilune sits `braking_arc_deg`
    # before it, and the deorbit point (apolune of the coast) half a turn before that.
    site = facility.position_km
    raan, u_site = plane_through(spin(body, t_down).T @ site, spec.inc_deg, ascending=False)
    u_peri = u_site - spec.braking_arc_deg
    circular = Kepler(body, r_orbit, 0.0, spec.inc_deg, raan, 0.0, u_peri - 180, epoch_s=t_deorbit)
    coast = Kepler(body, a_coast, (r_orbit - r_peri) / (r_orbit + r_peri), spec.inc_deg, raan,
                   u_peri, 180.0, epoch_s=t_deorbit)
    descent = RadialHermite(*coast.fixed(t_peri), site, ZERO, t_down - t_peri)
    landed = facility.basis.T @ lvlh(spin(body, t_down) @ circular.normal, site)
    return Spacecraft(spec.name, spec.mass_kg, [
        Orbit(0, circular),
        Orbit(t_deorbit, coast, cadence_s=MANOEUVRE_CADENCE_S),
        Path(t_peri, body, descent, circular.normal),
        Parked(t_down, facility, landed),
    ], events={"deorbit": t_deorbit, "perilune": t_peri, "touchdown": t_down}, facility=facility)
