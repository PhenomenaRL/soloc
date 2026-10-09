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


# -- wind: an analytic field per venue (sim/wind.py) ---------------------------------------------

WIND_GRID_M = 250.0                  # out/wind.arrow defaults: grid spacing and time step
WIND_TABLE_S = 60


@dataclass(frozen=True)
class WindSpec:
    """Mean speed and direction, a sinusoidal direction shift, a linear build, and Gaussian
    puffs advected with the mean wind. Speed and direction hold their `on` values before `on`
    and their `off` values after `off`. `extent_m` (east lo, hi, north lo, hi, venue ENU m) bounds
    the puffs, which wrap around it, and the written table."""
    venue: str
    tws_m_s: float
    twd_deg: float                   # compass direction the wind blows from
    build_m_s_h: float
    shift_deg: float
    shift_period_s: float
    puffs: int
    puff_radius_m: float             # Gaussian sigma
    puff_gain: float                 # peak fractional speed-up
    on: datetime
    off: datetime
    extent_m: tuple[float, float, float, float]
    grid_m: float = WIND_GRID_M
    table_s: int = WIND_TABLE_S


# -- regatta: Bedford Basin, Halifax. Children report venue ENU metres about the course centre --

REGATTA_VENUE = FacilitySpec("Bedford Basin", "BBN", EARTH, 44.6970, -63.6380)
REGATTA_ON = datetime(2026, 9, 5, 16)          # 13:00 ADT, boats leave the dock
REGATTA_OFF = datetime(2026, 9, 5, 21)         # 18:00 ADT
REGATTA_WARNING = datetime(2026, 9, 5, 16, 55)
REGATTA_GUN = datetime(2026, 9, 5, 17)
REGATTA_TIME_LIMIT = datetime(2026, 9, 5, 19, 30)   # boats still racing are DNF and motor home
MARKS_LAID = (datetime(2026, 9, 5, 16, 30), datetime(2026, 9, 5, 20, 30))

REGATTA_CADENCE_S = 5                # boats, RC boat and met buoys inside [REGATTA_ON, REGATTA_OFF]
REGATTA_IDLE_CADENCE_S = 3600        # and outside it
MARK_CADENCE_S = 60
DECISION_S = 10
BOAT_TIMESCALE = "GPST"
MARK_TIMESCALE = "TAI"

REGATTA_WIND = WindSpec("Bedford Basin", tws_m_s=5.0, twd_deg=200.0, build_m_s_h=0.3,
                        shift_deg=10.0, shift_period_s=720.0, puffs=6, puff_radius_m=300.0,
                        puff_gain=0.3, on=REGATTA_ON, off=REGATTA_OFF,
                        extent_m=(-2750.0, 2250.0, -2500.0, 3750.0))

BOATS = 10
BOAT_MASS_KG = 1400.0
BOAT_DIMENSIONS_M = (7.3, 2.7, 11.0)           # L × W × H, the mast included
RC_NAME = "RC-BOAT"
RC_MASS_KG = 9000.0
RC_DIMENSIONS_M = (12.0, 4.0, 4.0)
MOTOR_M_S = 2.5
DEPART_EVERY_S = 60                  # from REGATTA_ON: the RC boat, then the boats in turn
RC_LEAVES_AFTER_S = 300              # after the last boat finishes or is DNF

# Polar: 0 inside the no-go zone, else min(cap, gain · TWS · (floor + (1 − floor) · sin(π (TWA − no-go) / (180 − no-go)))).
NO_GO_DEG = 40.0
POLAR_GAIN = 0.65
POLAR_FLOOR = 0.7
POLAR_MAX_M_S = 3.5
TACK_PENALTY_S = 8.0                 # sailing time lost per tack or gybe
GYBE_PENALTY_S = 5.0

# Windward-leeward: start, W, gate, W, gate, W, finish on the start line from above.
COURSE_AXIS_DEG = REGATTA_WIND.twd_deg         # compass bearing from the line to the windward mark
BEAT_M = 1200.0                      # line centre → windward mark; the course centre is halfway
LINE_M = 250.0                       # RC boat (starboard end) ↔ pin
GATE_ABOVE_LINE_M = 100.0
GATE_WIDTH_M = 80.0
LAPS = 3
ROUND_RADIUS_M = 30.0
STAGING_BELOW_LINE_M = 200.0         # boats wait for the warning spread along this line
STAGING_SPREAD_M = 200.0

DOCK = (44.7260, -63.6639)           # the first berth, off the BBYC shore; boats berth east of it
BERTH_STEP_M = (15.0, 0.0)
DOCK_HEADING_DEG = 90.0
MOTOR_ROUTE = ((44.7180, -63.6653), (44.7113, -63.6602))   # dock → down Bedford Bay → the neck

MET_BUOYS_M = ((-900.0, 0.0), (900.0, -300.0), (0.0, -1500.0))
MARK_MASS_KG = 25.0
MARK_DIMENSIONS_M = (1.5, 1.5, 1.8)
BUOY_MASS_KG = 1500.0
BUOY_DIMENSIONS_M = (3.0, 3.0, 4.0)

# (lat, lon) rings, simplified to ~25 m from the OpenStreetMap outlines of Bedford Bay and
# Bedford Basin. The Basin's south edge is OSM's cut across the harbour, not a shore.
BASIN_WATER = (
    ((44.7149, -63.6713), (44.7122, -63.6708), (44.7118, -63.6691), (44.7123, -63.6666),
     (44.7116, -63.6652), (44.7111, -63.6648), (44.7107, -63.6652), (44.7105, -63.6666),
     (44.7100, -63.6659), (44.7099, -63.6673), (44.7082, -63.6640), (44.7139, -63.6551),
     (44.7149, -63.6552), (44.7150, -63.6562), (44.7157, -63.6569), (44.7165, -63.6596),
     (44.7165, -63.6607), (44.7168, -63.6604), (44.7172, -63.6610), (44.7211, -63.6597),
     (44.7211, -63.6581), (44.7231, -63.6578), (44.7239, -63.6563), (44.7251, -63.6561),
     (44.7244, -63.6573), (44.7254, -63.6583), (44.7264, -63.6607), (44.7291, -63.6620),
     (44.7288, -63.6626), (44.7260, -63.6641), (44.7257, -63.6653), (44.7238, -63.6676),
     (44.7211, -63.6698), (44.7209, -63.6692), (44.7203, -63.6710), (44.7197, -63.6703),
     (44.7187, -63.6709), (44.7185, -63.6704), (44.7176, -63.6707), (44.7170, -63.6704)),
    ((44.7082, -63.6640), (44.7065, -63.6630), (44.7056, -63.6631), (44.7039, -63.6616),
     (44.7015, -63.6604), (44.6991, -63.6599), (44.6948, -63.6604), (44.6915, -63.6597),
     (44.6910, -63.6590), (44.6898, -63.6593), (44.6881, -63.6588), (44.6803, -63.6511),
     (44.6766, -63.6223), (44.6808, -63.6143), (44.6824, -63.6132), (44.6844, -63.6140),
     (44.6854, -63.6135), (44.6853, -63.6142), (44.6864, -63.6144), (44.6883, -63.6159),
     (44.6896, -63.6161), (44.6896, -63.6166), (44.6918, -63.6168), (44.6917, -63.6175),
     (44.6933, -63.6170), (44.6921, -63.6187), (44.6942, -63.6161), (44.6958, -63.6167),
     (44.6979, -63.6188), (44.6992, -63.6209), (44.7031, -63.6234), (44.7035, -63.6233),
     (44.7044, -63.6276), (44.7059, -63.6298), (44.7066, -63.6316), (44.7066, -63.6330),
     (44.7059, -63.6333), (44.7067, -63.6349), (44.7063, -63.6337), (44.7070, -63.6328),
     (44.7083, -63.6349), (44.7087, -63.6373), (44.7092, -63.6372), (44.7092, -63.6366),
     (44.7096, -63.6370), (44.7099, -63.6387), (44.7109, -63.6407), (44.7106, -63.6418),
     (44.7125, -63.6469), (44.7129, -63.6470), (44.7123, -63.6500), (44.7128, -63.6510),
     (44.7138, -63.6513), (44.7137, -63.6526), (44.7142, -63.6530), (44.7139, -63.6551)),
)


def boat_name(i: int) -> str:
    return f"SAIL-{i + 1:02d}"


# -- wildfire: Squamish Valley, BC. Children report venue ENU metres about the ignition point --

FIRE_VENUE = FacilitySpec("Squamish Valley fire", "SQF", EARTH, 49.8078, -123.1969)
FIRE_IGNITION = datetime(2026, 9, 3, 21)       # 14:00 PDT
FIRE_DISPATCH_AFTER_S = 3600                   # the first orders to the crews
FIRE_STEP_S = 60                     # spread step; moving vertices report on it
FIRE_IDLE_CADENCE_S = 600            # stopped vertices and finished trenches
CREW_CADENCE_S = 30                  # from ignition on
CREW_IDLE_CADENCE_S = 3600           # at the ICP before it
FIRE_DECISION_S = 300
FIRE_TIMESCALE = "TAI"
CREW_TIMESCALE = "GPST"

FIRE_WIND = WindSpec("Squamish Valley fire", tws_m_s=3.5, twd_deg=160.0, build_m_s_h=0.0,
                     shift_deg=20.0, shift_period_s=3 * 3600.0, puffs=4, puff_radius_m=800.0,
                     puff_gain=0.3, on=FIRE_IGNITION, off=T_END,
                     extent_m=(-3000.0, 2500.0, -2500.0, 5000.0), table_s=600)

# Huygens spread along each vertex's outward normal, from a wind-aligned ellipse: head rate
# R0 · fuel · (1 + WIND_GAIN · U), length-to-breadth 1 + LB_PER_M_S · U, U the wind speed (m/s).
R0_M_MIN = 0.5
WIND_GAIN = 1.0
LB_PER_M_S = 0.3
IGNITION_RADIUS_M = 20.0
VERTICES = 48                        # on the ignition circle
VERTEX_GAP_M = 60.0                  # a new vertex spawns midway once neighbours are this far apart
VERTEX_CAP = 240

CREWS = 12
CREW_MASS_KG = 2000.0                # 20 people with tools
CREW_DIMENSIONS_M = (10.0, 10.0, 2.0)
WALK_M_S = 1.2
DIG_M_H = 100.0
SAFE_M = 30.0                        # a crew never moves within this of a moving vertex
ESCAPE_M = 40.0                      # and drops its work and walks away inside this
ICP_M = (1286.0, -1532.0)            # 2 km down-valley, on the same bank
TRENCH_M = 50.0                      # a trench entity per this much finished line
TRENCH_WIDTH_M = 1.0
TRENCH_DEPTH_M = 0.5
TRENCH_CAP = 620                     # 6 crews digging the whole window

# Fuel zones, by precedence: the river and its riparian band (distance to the centreline), the
# power-line corridor, a slash cutblock (venue ENU box), conifer elsewhere.
FUEL_FACTORS = {"river": 0.0, "riparian": 0.5, "corridor": 0.3, "slash": 1.8, "conifer": 1.0}
RIVER_HALF_WIDTH_M = 60.0
RIPARIAN_M = 250.0
CORRIDOR_HALF_WIDTH_M = 40.0
SLASH_M = (-300.0, 300.0, 1100.0, 1500.0)
FUEL_GRID_M = 50.0                   # out/fuel.arrow

# (lat, lon) centrelines simplified to ~20 m from OpenStreetMap: the Squamish River, and the
# power line (way 161389488) that passes the ignition point.
SQUAMISH_RIVER = (
    (49.86582, -123.25151), (49.86466, -123.25227), (49.86392, -123.25171), (49.86332, -123.25077),
    (49.86276, -123.24806), (49.86333, -123.24352), (49.86452, -123.24165), (49.86552, -123.23919),
    (49.86481, -123.23686), (49.86388, -123.23627), (49.86223, -123.23637), (49.85979, -123.23823),
    (49.85844, -123.23987), (49.85757, -123.24176), (49.85637, -123.24751), (49.85387, -123.24667),
    (49.85254, -123.24473), (49.84917, -123.24254), (49.84644, -123.23941), (49.84551, -123.23725),
    (49.84435, -123.23131), (49.84354, -123.22486), (49.84273, -123.22311), (49.84126, -123.22166),
    (49.83964, -123.22184), (49.83702, -123.22470), (49.83406, -123.22600), (49.82823, -123.22742),
    (49.82472, -123.22585), (49.82172, -123.22536), (49.82101, -123.22431), (49.82016, -123.22112),
    (49.81898, -123.21857), (49.81711, -123.21609), (49.81586, -123.21585), (49.81462, -123.21700),
    (49.81335, -123.21912), (49.81147, -123.21915), (49.80688, -123.21478), (49.80328, -123.21426),
    (49.80144, -123.21313), (49.79898, -123.20722), (49.79636, -123.20389), (49.79488, -123.20091),
    (49.79309, -123.19277), (49.79109, -123.19033), (49.78954, -123.18933), (49.78713, -123.18671),
    (49.78193, -123.18281), (49.77962, -123.18134), (49.77831, -123.18109), (49.77776, -123.18041),
    (49.77630, -123.17834), (49.77569, -123.17612), (49.77552, -123.17163), (49.77517, -123.17037),
    (49.77437, -123.16988), (49.77347, -123.16767), (49.77290, -123.16713), (49.77106, -123.16696),
    (49.77021, -123.16703), (49.76934, -123.16790), (49.76847, -123.16792),
)
POWER_LINE = (
    (49.85774, -123.23075), (49.85090, -123.22661), (49.84716, -123.22373), (49.83643, -123.21363),
    (49.82958, -123.20802), (49.81715, -123.20609), (49.80995, -123.19930), (49.79868, -123.17341),
    (49.79692, -123.16549), (49.79279, -123.16122), (49.79158, -123.16120),
)


def crew_name(i: int) -> str:
    return f"CREW-{i + 1}"


def vertex_name(i: int) -> str:
    return f"FIRE-V{i + 1:03d}"


def trench_name(i: int) -> str:
    return f"LINE-{i + 1:03d}"
