"""The seeded roster and schedules: the one place to tweak the story."""

from dataclasses import dataclass
from datetime import datetime

from geo import EARTH, MARS, MOON, SUN, Body

AUTHORITY = "sim.soloc"
SEED = 7

T0 = datetime(2026, 9, 1)            # UTC
T_END = datetime(2026, 9, 4)
DURATION_S = int((T_END - T0).total_seconds())

BASE_TICK_S = 5                      # the driver's grid; every cadence is a multiple of it
BATCH_S = 600                        # one do_put per 10 min of sim time
SNAPSHOT_S = 3600
SNAPSHOT_BODIES = (SUN, EARTH, MOON, MARS)


@dataclass(frozen=True)
class FacilitySpec:
    name: str
    code: str                        # short prefix for its robots' names
    body: Body
    lat_deg: float
    lon_deg: float                   # east-positive
    h_km: float = 0.0


FACILITIES = (
    FacilitySpec("KSC LC-39A", "KSC", EARTH, 28.608, -80.604),
    FacilitySpec("Andøya Spaceport", "AND", EARTH, 69.294, 16.021),
    FacilitySpec("JSC Houston", "JSC", EARTH, 29.559, -95.090),
    FacilitySpec("Shackleton Base", "SHK", MOON, -89.45, 222.7),
)
FACILITY_CADENCE_S = 3600
FACILITY_TIMESCALE = "TAI"


@dataclass(frozen=True)
class RoverSpec:
    speed_m_s: tuple[float, float]   # drawn once per leg
    pause_s: tuple[float, float]     # dwell at each waypoint
    mass_kg: float


ROBOTS_PER_FACILITY = 10
SITE_HALF_WIDTH_M = 150.0            # waypoints lie in a 300 m × 300 m square about the site origin
ROBOT_CADENCE_S = 30
ROBOT_TIMESCALE = "TAI"
ROBOT_DIMENSIONS_M = (0.9, 0.6, 0.5)
ROVERS = {
    EARTH.name: RoverSpec(speed_m_s=(0.5, 1.5), pause_s=(30, 300), mass_kg=120.0),
    MOON.name: RoverSpec(speed_m_s=(0.18, 0.22), pause_s=(60, 600), mass_kg=90.0),
}


def robot_name(facility: FacilitySpec, i: int) -> str:
    return f"{facility.code}-R{i + 1:02d}"


def seconds(dt: datetime) -> int:
    """UTC instant → seconds since T0, the models' time axis."""
    return int((dt - T0).total_seconds())


# -- spacecraft --------------------------------------------------------------------------------
# Orbits are given in the body's IAU frame frozen at T0; see geo.py.

SPACECRAFT_CADENCE_S = 60
HOST_CADENCE_S = 30                  # a crawler's host reports on every crawler epoch (zero-order hold)
MANOEUVRE_CADENCE_S = 5              # launch ascent, and the lunar descent from deorbit to touchdown
SPACECRAFT_TIMESCALE = "TT"
SPACECRAFT_DIMENSIONS_M = (4.0, 2.0, 2.0)   # also the hull box the crawlers loop around


@dataclass(frozen=True)
class OrbiterSpec:
    name: str
    body: Body
    peri_alt_km: float
    apo_alt_km: float
    inc_deg: float
    raan_deg: float
    argp_deg: float
    m0_deg: float                    # mean anomaly at T0
    mass_kg: float


# The altitude where the orbit turns at the body's spin rate, so it hangs over one longitude.
GEO_ALT_KM = (EARTH.gm / EARTH.spin_rad_s ** 2) ** (1 / 3) - EARTH.a_km

ORBITERS = (
    OrbiterSpec("LUNA-2", MOON, 100, 100, 90.0, 0.0, 0.0, 40.0, 1800.0),
    OrbiterSpec("LUNA-3", MOON, 100, 100, 90.0, 120.0, 0.0, 200.0, 1800.0),
    OrbiterSpec("MARS-1", MARS, 300, 300, 93.0, 30.0, 0.0, 0.0, 2500.0),
    OrbiterSpec("MARS-2", MARS, 3200, 8800, 75.0, 150.0, 45.0, 0.0, 2500.0),
    OrbiterSpec("LEO-1", EARTH, 420, 420, 51.6, 40.0, 0.0, 0.0, 12000.0),
    OrbiterSpec("SSO-1", EARTH, 700, 700, 98.2, 200.0, 0.0, 90.0, 1200.0),
    # i = 0, e = 0: the longitude at T0 is raan + argp + m0, here 75° W.
    OrbiterSpec("GEO-1", EARTH, GEO_ALT_KM, GEO_ALT_KM, 0.0, 0.0, 0.0, -75.0, 5000.0),
)


@dataclass(frozen=True)
class LanderSpec:
    """A circular polar orbiter that deorbits onto a facility. Its plane and phase are derived
    from the landing: a half-ellipse coast from the deorbit point down to perilune, then a
    powered descent over `braking_arc_deg` of ground onto the facility origin."""
    name: str
    facility: str
    alt_km: float
    inc_deg: float
    deorbit: datetime
    perilune_alt_km: float
    braking_arc_deg: float
    powered_s: float
    mass_kg: float


LANDERS = (
    LanderSpec("LUNA-1", "Shackleton Base", 100, 90.0, datetime(2026, 9, 2, 12),
               perilune_alt_km=15, braking_arc_deg=16, powered_s=600, mass_kg=1800.0),
)


@dataclass(frozen=True)
class LaunchSpec:
    """Parked on a facility's origin until liftoff, then a kinematic ascent into a circular orbit
    whose plane passes over the pad at liftoff, inserting `downrange_deg` along it."""
    name: str
    facility: str
    liftoff: datetime
    alt_km: float
    inc_deg: float
    mass_kg: float
    ascent_s: int = 540
    downrange_deg: float = 18.0


LAUNCHES = (
    LaunchSpec("LAUNCH-A", "KSC LC-39A", datetime(2026, 9, 1, 14), 400, 51.6, 4000.0),
    LaunchSpec("LAUNCH-B", "Andøya Spaceport", datetime(2026, 9, 2, 18), 550, 97.6, 800.0),
)

# -- crawlers: hull robots on the orbiters and landers ----------------------------------------

CRAWLERS = 10
CRAWLER_CADENCE_S = HOST_CADENCE_S
CRAWLER_TIMESCALE = "TAI"
CRAWLER_SPEED_M_S = (0.02, 0.05)
CRAWLER_MASS_KG = 8.0
CRAWLER_DIMENSIONS_M = (0.3, 0.2, 0.1)
DISEMBARK_AFTER_S = 3600             # the first crawler on a lander steps off this long after touchdown
DISEMBARK_OFFSET_M = (3.0, 0.0)      # where it starts roving, in the facility's ENU frame


def crawler_name(i: int) -> str:
    return f"CRAWLER-{i + 1:02d}"
