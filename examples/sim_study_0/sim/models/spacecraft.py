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

from sim.geo import (GCRF, ICRF, SUN, Conic, Frame, Kepler, RadialHermite, lambert, lvlh,
                     matrix_from_quat, plane_through, quat_from_matrix, spin)
from sim.models import Row
from sim.models.facility import Spot
from sim.scenario import (AUTHORITY, DURATION_S, HOST_CADENCE_S, MANOEUVRE_CADENCE_S, MOON_SOI_KM,
                          SPACECRAFT_CADENCE_S, SPACECRAFT_DIMENSIONS_M, SPACECRAFT_TIMESCALE,
                          LanderSpec, LaunchSpec, MoonshotSpec, OrbiterSpec, TransferSpec, seconds)
from soloc_client import KIND_SOLOC, mint

ZERO = np.zeros(3)


class Orbit:
    """A `Kepler` or a body-centred `Conic`, stored in the body's live IAU frame."""

    def __init__(self, start_s: float, kepler: Kepler | Conic, cadence_s: int | None = None):
        self.start_s, self.kepler, self.cadence_s = start_s, kepler, cadence_s
        self.frame_id = kepler.body.frame_id

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
        self.frame_id = body.frame_id

    def state(self, t_s):
        r, v = self.path.state(t_s - self.start_s)
        return self.body.frame_id, r, v, lvlh(spin(self.body, t_s) @ self.normal, r)


class Parked:
    """At rest on a facility's pad, attitude fixed in the facility's ENU frame."""

    def __init__(self, start_s: float, spot: Spot, attitude_enu: np.ndarray, cadence_s: int | None = None):
        self.start_s, self.spot, self.attitude, self.cadence_s = start_s, spot, attitude_enu, cadence_s
        self.frame_id = spot.facility.id

    def state(self, t_s):
        return self.frame_id, self.spot.offset_km, ZERO, self.attitude


class Coast:
    """A conic about an inertial root's own centre, stored in that frame as it is."""

    def __init__(self, start_s: float, frame: Frame, conic: Conic, cadence_s: int | None = None):
        self.start_s, self.conic, self.cadence_s = start_s, conic, cadence_s
        self.frame_id = frame.frame_id

    def state(self, t_s):
        r, v = self.conic.state(t_s)
        return self.frame_id, r, v, lvlh(self.conic.normal, r)


class Cruise:
    """A Sun-centred conic stored in ICRF, whose origin is the solar-system barycentre: each
    row is the Sun's barycentric state plus the heliocentric one. The Sun's centre comes from
    the kernels on the `cadence_s` grid over the window, so the phase is only sampled on it.
    Attitude is LVLH about the Sun: body z points at it."""

    def __init__(self, start_s: float, conic: Conic, ephemeris, cadence_s: int):
        self.start_s, self.conic, self.cadence_s = start_s, conic, cadence_s
        self.frame_id = ICRF.frame_id
        self.sun_p = ephemeris.centre(SUN, np.arange(0, DURATION_S + 1, cadence_s, dtype=float))
        self.sun_v = np.gradient(self.sun_p, cadence_s, axis=0)

    def state(self, t_s):
        k = t_s // self.cadence_s
        r, v = self.conic.state(t_s)
        return ICRF.frame_id, self.sun_p[k] + r, self.sun_v[k] + v, lvlh(self.conic.normal, r)


class Spacecraft:
    def __init__(self, name: str, mass_kg: float, phases: list, events: dict[str, float] | None = None,
                 facility=None, body=None):
        self.name = name
        self.id = mint(KIND_SOLOC, AUTHORITY, name)
        self.mass_kg = mass_kg
        self.phases = phases
        self.starts = [p.start_s for p in phases]
        self.events = events or {}       # liftoff/insertion or deorbit/perilune/touchdown, in t_s
        self.facility = facility         # the pad or landing site, if any
        self.launch_spot = self.landing_spot = None   # where on a facility; set by the builders
        # The frame its free-flight rows are in: a body's IAU frame, or an inertial root.
        self.body = body or (facility.spec.body if facility else phases[0].kepler.body)
        self.cadence_s = SPACECRAFT_CADENCE_S   # the roster lowers this for crawler hosts

    def phase_at(self, t_s: float):
        return self.phases[bisect.bisect_right(self.starts, t_s) - 1]

    def frame_changes(self) -> list[tuple[bytes, bytes, int, str]]:
        """`(old frame, new frame, t_s of the first row in the new one, kind)` wherever
        consecutive phases are framed differently."""
        changes = []
        for old, new in zip(self.phases, self.phases[1:]):
            if old.frame_id != new.frame_id:
                cadence = new.cadence_s or self.cadence_s
                kind = ("launch" if isinstance(old, Parked) else
                        "landing" if isinstance(new, Parked) else "hand-off")
                changes.append((old.frame_id, new.frame_id, math.ceil(new.start_s / cadence) * cadence, kind))
        return changes

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


def transfer(spec: TransferSpec, ephemeris) -> Spacecraft:
    t_dep, t_arr = seconds(spec.depart), seconds(spec.arrive)
    sun_dep, sun_v = ephemeris.state(SUN, t_dep)
    origin, origin_v = ephemeris.state(spec.origin, t_dep)
    r1 = origin[0] - sun_dep[0]
    r2 = (ephemeris.centre(spec.target, t_arr) - ephemeris.centre(SUN, t_arr))[0]
    # Round the Sun the way the origin planet goes.
    v1, _ = lambert(r1, r2, t_arr - t_dep, SUN.gm, np.cross(r1, origin_v[0] - sun_v[0]))
    craft = Spacecraft(spec.name, spec.mass_kg,
                       [Cruise(0, Conic(SUN.gm, r1, v1, t_dep), ephemeris, HOST_CADENCE_S)],
                       events={"departure": t_dep, "arrival": t_arr}, body=ICRF)
    craft.cadence_s = HOST_CADENCE_S
    craft.spec = spec
    return craft


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
    craft = Spacecraft(spec.name, spec.mass_kg, [
        Parked(0, Spot(facility), on_pad),
        Path(t_lift, body, ascent, kepler.normal),
        Orbit(t_ins, kepler),
    ], events={"liftoff": t_lift, "insertion": t_ins}, facility=facility)
    craft.launch_spot = Spot(facility)
    return craft


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
    craft = Spacecraft(spec.name, spec.mass_kg, [
        Orbit(0, circular),
        Orbit(t_deorbit, coast, cadence_s=MANOEUVRE_CADENCE_S),
        Path(t_peri, body, descent, circular.normal),
        Parked(t_down, Spot(facility), landed),
    ], events={"deorbit": t_deorbit, "perilune": t_peri, "touchdown": t_down}, facility=facility)
    craft.landing_spot = Spot(facility)
    return craft


PATCH_ITERATIONS = 30        # the velocity mismatch at the patch shrinks ~2.3× per pass
FREEZE_PATCH_AFTER = 5       # passes after which the patch instant stops moving between ticks


def moonshot(spec: MoonshotSpec, origin, destination, ephemeris) -> Spacecraft:
    pad, site = Spot(origin, spec.pad_m), Spot(destination, spec.landing_m)
    earth, moon = pad.body, site.body
    r_orbit = moon.a_km + spec.lunar_alt_km
    a_coast = (r_orbit + moon.a_km + spec.perilune_alt_km) / 2

    def axes(body, t_s):
        """`(inertial → GCRF rotation, centre km, velocity km/s)` of a body at `t_s`, from the
        kernels. The sim's inertial frame for a body is its IAU frame with the spin undone."""
        p, q = ephemeris.poses(body, [t_s - 30, t_s, t_s + 30], GCRF)
        return matrix_from_quat(q[1]) @ spin(body, t_s), p[1], (p[2] - p[0]) / 60

    # Earth's pole moves ~1e-4° over the window, so one rotation serves the searches below; the
    # burn itself is converted with the rotation at its own instant.
    earth_axes = axes(earth, seconds(spec.liftoff_after))[0]

    def parking(t_lift):
        raan, u_pad = plane_through(spin(earth, t_lift).T @ pad.position_km, spec.inc_deg, ascending=True)
        return Kepler(earth, earth.a_km + spec.parking_alt_km, 0.0, spec.inc_deg, raan, 0.0,
                      u_pad + spec.downrange_deg, epoch_s=t_lift + spec.ascent_s)

    def liftoff(target):
        """The first tick after `liftoff_after` when the parking plane holds `target` (GCRF)."""
        direction = earth_axes.T @ target
        miss = lambda t: float(parking(t).normal @ direction)
        start = seconds(spec.liftoff_after)
        for lo in range(start, start + 86400, 600):
            if miss(lo) * miss(lo + 600) <= 0:
                break
        else:
            raise SystemExit(f"{spec.name}: no parking plane at {spec.inc_deg}° reaches the Moon")
        hi = lo + 600.0
        for _ in range(40):
            mid = (lo + hi) / 2
            lo, hi = (mid, hi) if miss(mid) * miss(hi) > 0 else (lo, mid)
        return math.ceil(hi / HOST_CADENCE_S) * HOST_CADENCE_S

    def injection(park, t_ins, target, t_target):
        """The tick in the first ~1.3 parking revolutions where the burn onto the arc to
        `target` is smallest."""
        def burn(t):
            r, v = (earth_axes @ x for x in park.inertial(t))
            return float(np.linalg.norm(lambert(r, target, t_target - t, earth.gm, np.cross(r, v))[0] - v))
        first = math.ceil((t_ins + 300) / HOST_CADENCE_S) * HOST_CADENCE_S
        return min(range(first, int(t_ins + 1.3 * park.period_s), HOST_CADENCE_S), key=burn)

    # Outer loop: liftoff and burn times depend on where the arc is aimed, which depends on
    # them. Inner loop: the arc is aimed at the point where the Moon-centred hyperbola crosses
    # the sphere of influence, and the hyperbola is rebuilt from the arc's velocity there.
    t_target = seconds(spec.liftoff_after) + 3600 + spec.transfer_s
    target = axes(moon, t_target)[1]
    t_lift = t_tli = None
    for _ in range(6):
        previous = (t_lift, t_tli)
        t_lift = liftoff(target)
        park, t_ins = parking(t_lift), t_lift + spec.ascent_s
        t_tli = injection(park, t_ins, target, t_target)
        if (t_lift, t_tli) == previous:
            break
        t_loi = t_tli + spec.transfer_s
        tli_axes = axes(earth, t_tli)[0]
        r1, v_park = (tli_axes @ x for x in park.inertial(t_tli))
        way = np.cross(r1, v_park)

        moon_axes, moon_p, moon_v = axes(moon, t_loi)
        v_rel = moon_axes.T @ (lambert(r1, moon_p, spec.transfer_s, earth.gm, way)[1] - moon_v)
        radius, t_soi = MOON_SOI_KM, None
        t_down = t_loi + spec.lunar_revs * 2 * math.pi * math.sqrt(r_orbit ** 3 / moon.gm)
        for i in range(PATCH_ITERATIONS):
            speed = float(np.linalg.norm(v_rel))
            v_inf = math.sqrt(speed ** 2 - 2 * moon.gm / radius)
            e = 1 + r_orbit * v_inf ** 2 / moon.gm
            p = r_orbit * (1 + e)
            nu = -math.acos((p / radius - 1) / e)                    # inbound, so before periapsis
            heading = math.atan2(e + math.cos(nu), -math.sin(nu))    # of the velocity, from P towards Q
            # The plane holds the inbound velocity and the landing point at touchdown.
            d = v_rel / speed
            site_in = spin(moon, t_down).T @ site.position_km
            n = np.cross(d, site_in)
            n /= np.linalg.norm(n)
            peri = math.cos(heading) * d - math.sin(heading) * np.cross(n, d)
            ahead = np.cross(n, peri)
            hyperbola = Conic(moon.gm, r_orbit * peri, math.sqrt(moon.gm * p) / r_orbit * ahead,
                              t_loi, body=moon)
            if i < FREEZE_PATCH_AFTER:
                nu_soi = math.acos((p / MOON_SOI_KM - 1) / e)
                f = 2 * math.atanh(math.sqrt((e - 1) / (e + 1)) * math.tan(nu_soi / 2))
                fall_s = (e * math.sinh(f) - f) / math.sqrt(moon.gm * (v_inf ** 2 / moon.gm) ** 3)
                t_soi = round((t_loi - fall_s) / HOST_CADENCE_S) * HOST_CADENCE_S
            r_in, v_in = hyperbola.state(t_soi)
            radius = float(np.linalg.norm(r_in))
            moon_axes, moon_p, moon_v = axes(moon, t_soi)
            target, t_target = moon_p + moon_axes @ r_in, t_soi
            v1, v2 = lambert(r1, target, t_soi - t_tli, earth.gm, way)
            v_rel = moon_axes.T @ (v2 - moon_v)

            # The landing, as in `lander`: deorbit half a turn before perilune, which is
            # `braking_arc_deg` short of the site; angles run from periapsis along the motion.
            at_site = math.atan2(site_in @ ahead, site_in @ peri)
            turn = ((at_site - math.radians(spec.braking_arc_deg) - math.pi) % (2 * math.pi)
                    + 2 * math.pi * spec.lunar_revs)
            t_deorbit = t_loi + turn / math.sqrt(moon.gm / r_orbit ** 3)
            t_peri = t_deorbit + math.pi * math.sqrt(a_coast ** 3 / moon.gm)
            t_down = math.ceil((t_peri + spec.powered_s) / HOST_CADENCE_S) * HOST_CADENCE_S

    along = lambda angle: (math.cos(angle) * peri + math.sin(angle) * ahead,
                           -math.sin(angle) * peri + math.cos(angle) * ahead)
    v_orbit = math.sqrt(moon.gm / r_orbit)
    circular = Conic(moon.gm, r_orbit * peri, v_orbit * ahead, t_loi, body=moon)
    out, forward = along(turn)
    coast = Conic(moon.gm, r_orbit * out, math.sqrt(moon.gm * (2 / r_orbit - 1 / a_coast)) * forward,
                  t_deorbit, body=moon)
    ascent = RadialHermite(pad.position_km, ZERO, *park.fixed(t_ins), spec.ascent_s)
    descent = RadialHermite(*coast.fixed(t_peri), site.position_km, ZERO, t_down - t_peri)
    on_pad = origin.basis.T @ lvlh(spin(earth, t_lift) @ park.normal, pad.position_km)
    landed = destination.basis.T @ lvlh(spin(moon, t_down) @ n, site.position_km)

    # Every orbit phase has its own cadence, which also keeps the generic full-orbit check off
    # these partial arcs.
    craft = Spacecraft(spec.name, spec.mass_kg, [
        Parked(0, pad, on_pad),
        Path(t_lift, earth, ascent, park.normal),
        Orbit(t_ins, park, cadence_s=HOST_CADENCE_S),
        Coast(t_tli, GCRF, Conic(earth.gm, r1, v1, t_tli), cadence_s=HOST_CADENCE_S),
        Orbit(t_soi, hyperbola, cadence_s=HOST_CADENCE_S),
        Orbit(t_loi, circular, cadence_s=HOST_CADENCE_S),
        Orbit(t_deorbit, coast, cadence_s=MANOEUVRE_CADENCE_S),
        Path(t_peri, moon, descent, n),
        Parked(t_down, site, landed),
    ], events={"liftoff": t_lift, "insertion": t_ins, "tli": t_tli, "soi": t_soi, "loi": t_loi,
               "deorbit": t_deorbit, "perilune": t_peri, "touchdown": t_down}, body=GCRF)
    craft.cadence_s = HOST_CADENCE_S
    craft.spec, craft.launch_spot, craft.landing_spot = spec, pad, site
    craft.design = {
        "tli_dv_km_s": float(np.linalg.norm(v1 - v_park)),
        "loi_dv_km_s": float(np.linalg.norm(hyperbola.state(t_loi)[1])) - v_orbit,
        "patch_radius_km": radius,
        "patch_mismatch_km_s": float(np.linalg.norm(v_rel - v_in)),
        "lunar_inc_deg": math.degrees(math.acos(n[2])),
    }
    return craft
