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
