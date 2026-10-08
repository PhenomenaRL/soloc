"""The seeded roster and schedules: the one place to tweak the story."""

from dataclasses import dataclass
from datetime import datetime

from sim.geo import EARTH, MARS, MOON, SUN, Body

AUTHORITY = "sim.soloc"
SEED = 7

T0 = datetime(2026, 9, 1)            # UTC
T_END = datetime(2026, 9, 6)
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
    speed_m_s: tuple[float, float]   # drawn once per robot
    pause_s: tuple[float, float]     # loading stops (logistics)
    mass_kg: float


ROBOTS_PER_FACILITY = 10
SITE_HALF_WIDTH_M = 150.0            # the site is a 300 m × 300 m square about the facility origin

# Site layout, in facility ENU metres. Every site uses the same one.
# - patrol: laps of the fence, stopping briefly at the corners
# - survey: back-and-forth sweeps over one cell of the road grid
# - logistics: tours from the depot to a few road intersections, driving only on the roads
ROBOT_ROLES = ("patrol",) * 3 + ("survey",) * 3 + ("logistics",) * 4   # by index at the site
PATROL_FENCE_M = 145.0               # half-width of the outermost patrol lap; each next one is 5 m in
PATROL_CORNER_PAUSE_S = (5.0, 20.0)
SITE_ROADS_M = (-100.0, 0.0, 100.0)  # roads run along x and y at these coordinates
DEPOT_M = (-100.0, -100.0)
LOGISTICS_STATIONS = (3, 4)          # intersections per tour, inclusive
SURVEY_INSET_M = 10.0                # sweep area = the road-grid cell inset by this
SURVEY_ROW_SPACING_M = 10.0
SURVEY_TURN_PAUSE_S = (3.0, 8.0)
PATTERN_UNDER_WAY_S = (1800.0, 7200.0)   # how long each robot has been at it by T0
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

@dataclass(frozen=True)
class MoonshotSpec:
    """Pad to lunar surface as a patched conic: a parking orbit, a translunar arc about the
    Earth (stored in GCRF) up to the Moon's sphere of influence, a hyperbola about the Moon
    down to a circular orbit, then a landing like a `LanderSpec`'s. Every time after
    `liftoff_after` is derived: liftoff waits for the parking plane to hold the translunar
    target, and the burn is wherever on the parking orbit it is cheapest."""
    name: str
    origin: str                      # facility, and the pad on it (east, north) metres off its origin
    pad_m: tuple[float, float]
    destination: str
    landing_m: tuple[float, float]
    liftoff_after: datetime
    inc_deg: float                   # parking orbit; must exceed the Moon's declination at arrival
    parking_alt_km: float
    transfer_s: float                # translunar injection → perilune
    lunar_alt_km: float
    lunar_revs: int                  # whole revolutions between capture and the deorbit burn
    perilune_alt_km: float
    braking_arc_deg: float
    powered_s: float
    mass_kg: float
    ascent_s: int = 540
    downrange_deg: float = 18.0


MOON_SOI_KM = 66183.0                # where the translunar arc hands over to the Moon's gravity

# The pads sit in the strip between the road grid (±100 m) and the patrol laps (135 m out), so
# they are clear of LAUNCH-A's pad, LUNA-1 and every robot's route.
MOONSHOTS = (
    MoonshotSpec("SELENE-1", "KSC LC-39A", (120.0, 0.0), "Shackleton Base", (0.0, 120.0),
                 liftoff_after=datetime(2026, 9, 1, 6), inc_deg=30.0, parking_alt_km=200,
                 transfer_s=3 * 86400, lunar_alt_km=100, lunar_revs=2, perilune_alt_km=15,
                 braking_arc_deg=16, powered_s=600, mass_kg=15000.0),
)

# -- the cargo robot each moonshot carries: a ground rover at both ends ------------------------

CARGO_BOARDS_BEFORE_LIFTOFF_S = 1800
CARGO_STOP_SHORT_M = 3.0             # it parks this far from the ship's centre, and steps off there
CARGO_STOWED_M = (0.0, 0.0, -1.0)    # in the ship's body frame: on the face away from nadir


def cargo_name(i: int) -> str:
    return f"CARGO-{i + 1:02d}"


@dataclass(frozen=True)
class TransferSpec:
    """An interplanetary cruise: the Sun-centred conic from `origin`'s centre at `depart` to
    `target`'s centre at `arrive` (real positions, from the kernels), stored in ICRF. The
    planets' own gravity is left out, so the arc starts and ends at their centres."""
    name: str
    origin: Body
    target: Body
    depart: datetime
    arrive: datetime
    mass_kg: float
    crawlers: int                    # hull robots aboard, named after the CRAWLERS above them


# The low-energy window to Mars opens in October 2026; leaving this early costs C3 ≈ 40 km²/s².
TRANSFERS = (
    TransferSpec("MARS-TRANSFER-1", EARTH, MARS, datetime(2026, 8, 24), datetime(2027, 7, 20),
                 mass_kg=6000.0, crawlers=4),
)


# -- probes: real ephemerides from JPL Horizons, ICRF (centred on the solar-system barycentre), km


@dataclass(frozen=True)
class ProbeSpec:
    name: str
    horizons_id: str                 # the Horizons COMMAND; spacecraft ids are negative
    mass_kg: float
    dimensions_m: tuple[float, float, float]


PROBES = (
    ProbeSpec("Parker Solar Probe", "-96", 555.0, (3.0, 2.3, 2.3)),
)
PROBE_CADENCE_S = 60                 # whole minutes: rows are taken straight from the Horizons table
PROBE_TIMESCALE = "UTC"
HORIZONS_AUTHORITY = "jpl.nasa.gov"  # the probes' source_id, in place of the sim's own
HORIZONS_SOURCE = "horizons"


def horizons_file(spec: ProbeSpec) -> str:
    """Where fetch_horizons.py saves the probe's table, under data/."""
    return f"horizons_{spec.horizons_id.lstrip('-')}.txt"


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


# -- aircraft: IAU_EARTH, km -------------------------------------------------------------------


@dataclass(frozen=True)
class Airport:
    code: str
    lat_deg: float
    lon_deg: float
    h_km: float                      # field elevation, taken as height above the ellipsoid


AIRPORTS = (
    Airport("JFK", 40.6413, -73.7781, 0.004),
    Airport("LHR", 51.4700, -0.4543, 0.025),
    Airport("CDG", 49.0097, 2.5479, 0.119),
    Airport("FRA", 50.0379, 8.5622, 0.111),
    Airport("DXB", 25.2532, 55.3657, 0.019),
    Airport("DEL", 28.5562, 77.1000, 0.237),
    Airport("SIN", 1.3644, 103.9915, 0.007),
    Airport("HKG", 22.3080, 113.9185, 0.009),
    Airport("ICN", 37.4602, 126.4407, 0.007),
    Airport("HND", 35.5494, 139.7798, 0.006),
    Airport("SYD", -33.9399, 151.1753, 0.006),
    Airport("LAX", 33.9416, -118.4085, 0.038),
    Airport("ORD", 41.9742, -87.9073, 0.205),
    Airport("GRU", -23.4356, -46.4731, 0.750),
    Airport("JNB", -26.1367, 28.2411, 1.694),
)

AIRCRAFT = 10
AIRCRAFT_CADENCE_S = 60
AIRCRAFT_TIMESCALE = "UTC"
AIRCRAFT_MASS_KG = 250_000.0
AIRCRAFT_DIMENSIONS_M = (64.0, 60.0, 17.0)
CRUISE_ALT_KM = 11.0
CRUISE_KM_H = 900.0
CLIMB_S = 25 * 60                    # speed ramps 0 → cruise while climbing to cruise altitude
DESCENT_S = 40 * 60                  # and back down to 0 at the destination's elevation
LEGS_PER_AIRCRAFT = (2, 4)           # inclusive; parked at the last airport after them
LEG_KM = (1500.0, 14000.0)           # great-circle length of an eligible leg
TURNAROUND_S = (1.5 * 3600, 3 * 3600)
FIRST_DEPARTURE_S = (-8 * 3600, 12 * 3600)   # before 0: airborne (or already turned round) at T0


def aircraft_name(i: int) -> str:
    return f"AIR-{i + 1:02d}"


# -- ships: IAU_EARTH, km, at sea level ---------------------------------------------------------


@dataclass(frozen=True)
class Lane:
    """A sea lane between two ports: great-circle arcs through the waypoints, which are hand
    placed to keep every arc on water (check_sim tests this against the land polygons). The
    first and last waypoints are the ports' berths."""
    name: str
    ports: tuple[str, str]
    waypoints: tuple[tuple[float, float], ...]


LANES = (
    Lane("transpacific", ("Shanghai", "Los Angeles"), (
        (30.90, 122.60), (30.20, 128.50), (30.00, 130.30), (30.20, 131.50), (32.30, 140.50), (34.20, 145.00),
        (33.20, -121.00), (33.70, -118.25))),
    Lane("Asia-Europe via Suez", ("Singapore", "Rotterdam"), (
        (1.20, 103.85), (1.25, 103.50), (2.20, 101.80), (3.30, 100.60), (5.50, 97.90),
        (6.20, 95.00), (5.60, 80.60), (12.00, 52.50), (12.60, 43.40), (15.00, 41.80),
        (20.00, 38.50), (27.35, 34.00), (27.90, 33.60), (28.60, 33.05), (29.30, 32.72),
        (29.95, 32.57), (31.30, 32.35), (31.80, 32.30), (33.00, 28.00), (36.10, 15.00), (37.35, 11.70), (37.60, 9.50), (37.00, 2.00),
        (35.95, -5.60), (36.20, -9.50), (43.50, -9.80), (48.50, -5.60), (50.00, -2.00),
        (51.00, 1.55), (51.98, 4.05))),
    Lane("transatlantic", ("New York", "Le Havre"), (
        (40.45, -73.90), (40.40, -73.30), (40.50, -69.00), (49.30, -6.00),
        (50.00, -1.50), (49.50, 0.00))),
    Lane("via Panama", ("Los Angeles", "New York"), (
        (33.70, -118.25), (32.50, -118.00), (28.00, -116.50), (22.00, -110.00),
        (18.00, -106.00), (15.50, -100.00), (13.50, -94.00), (11.00, -88.00), (8.50, -86.00),
        (7.00, -82.00), (7.00, -79.80), (8.30, -79.50), (8.88, -79.55),
        (9.40, -79.92), (10.50, -79.50), (17.50, -75.50), (18.50, -75.00), (19.90, -73.90),
        (20.20, -73.60), (21.20, -72.70), (22.30, -72.50), (23.00, -72.00), (32.00, -74.00), (37.00, -74.50), (40.40, -73.30), (40.45, -73.90))),
    Lane("Cape route", ("Santos", "Singapore"), (
        (-24.05, -46.30), (-35.50, 18.00), (-36.00, 22.00), (-25.00, 58.00),
        (5.80, 94.50), (6.20, 95.00), (5.50, 97.90), (3.30, 100.60), (2.20, 101.80),
        (1.25, 103.50), (1.20, 103.85))),
    Lane("Gulf-Asia tankers", ("Ras Tanura", "Ningbo"), (
        (26.70, 50.30), (26.90, 51.60), (26.20, 53.50), (26.05, 55.60), (26.55, 56.45),
        (24.50, 58.80), (22.50, 60.00), (6.50, 76.50),
        (5.60, 80.60), (6.20, 95.00), (5.50, 97.90), (3.30, 100.60), (2.20, 101.80),
        (1.25, 103.50), (1.20, 103.85), (1.22, 104.20), (1.45, 104.65), (5.00, 106.00),
        (12.00, 111.00), (22.00, 117.00), (24.50, 119.50), (28.00, 122.50), (29.75, 122.75))),
    Lane("transpacific north", ("Busan", "San Francisco"), (
        (35.00, 129.10), (34.30, 128.80), (32.00, 128.30), (30.00, 130.30), (30.20, 131.50),
        (32.80, 136.00), (34.50, 141.50), (50.00, -175.00), (37.75, -122.75))),
    Lane("South America-Europe", ("Santos", "Rotterdam"), (
        (-24.05, -46.30), (-24.50, -44.00), (-23.50, -41.50), (-20.50, -39.00),
        (-18.00, -37.50), (-13.00, -37.50), (-8.00, -34.00), (-5.00, -33.50), (0.00, -30.00),
        (13.00, -26.50), (18.00, -27.00), (29.00, -19.50), (33.50, -18.50), (43.50, -10.00), (48.50, -5.60), (50.00, -2.00), (51.00, 1.55), (51.98, 4.05))),
)

SHIPS = 20
SHIP_CADENCE_S = 60
SHIP_TIMESCALE = "GPST"
SHIP_MASS_KG = 1.5e8
SHIP_DIMENSIONS_M = (300.0, 48.0, 30.0)
SHIP_KM_H = (22.0, 40.0)             # drawn once per ship
DOCK_S = (8 * 3600, 24 * 3600)
DOCKED_AT_T0 = 0.3                   # the share of ships that start the window at a berth
DOCKED_AT_T0_S = (2 * 3600, 20 * 3600)   # how long those still stay


def ship_name(i: int) -> str:
    return f"SHIP-{i + 1:02d}"
