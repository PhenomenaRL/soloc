# What's in the ledger

The window is 2026-09-01T00:00 → 2026-09-06T00:00 UTC. The driver (`run_sim.py`) ticks a 5 s
grid, asks every entity whether it is due, and appends one batch per 10 min of sim time (721
appends), plus one before every arena decision tick. Each `do_put` is `Ledger::append` (validation, TAI normalisation, topology and cycle
checks on every batch), `append_snapshot` adds the celestial bodies from the kernels, and
`save_ledger` is `Ledger::save_ipc`.

A full run (~6.5 min) prints `1703 entities, 3,583,676 entity rows + 484 snapshot rows` and a
result line per arena and for the factory. It writes these files:
- `out/sim_study_0.arrow`
- its `out/sim_study_0.arrow.names.arrow` name registry
- the `out/wind.arrow`, `out/fuel.arrow` and `out/bearing_truth.arrow` side tables

The wildfire's vertices and trenches come from fixed pools of 240 and 620 names, so the run
registers 2,434 names; 1,703 of them get rows. The ledger holds about 1.77 GB in the server, of
the 2 GB `memory_limit`. The factory's 20 Hz burst accounts for 684,000 of its rows and its
shaft captures for 1,200,000; the bearing truth table, `out/bearing_truth.arrow` (12M rows,
~2.3 GB), sits beside it. Batches
go to the server in chunks of 65,536 rows (`PUT_CHUNK_ROWS`), so the burst stays under the 64 MiB
message limit.

`sim/scenario.py` holds the roster and every schedule (times, orbits, airports, lanes); the
models live in `sim/models/`.

| Entities | Frame (parent) | Units | Timescale | Rows every | What they do |
|---|---|---|---|---|---|
| 4 facilities | IAU_EARTH / IAU_MOON | km | TAI | 1 h | KSC LC-39A, Andøya Spaceport, JSC Houston, Shackleton Base; pose is the site's ENU frame |
| 40 robots | their facility | m | TAI | 30 s | 10 per facility on a shared site layout (300 m × 300 m, a 3 × 3 road grid, a depot): 3 patrol the fence, 3 sweep back and forth over a cell of the road grid, 4 run tours from the depot along the roads |
| 7 orbiters | IAU_MOON / IAU_MARS / IAU_EARTH | km | TT | 30 or 60 s | LUNA-2/3 (100 km polar), MARS-1 (300 km, 93°), MARS-2 (3,200 × 8,800 km, 75°), LEO-1 (420 km, 51.6°), SSO-1 (700 km, 98.2°), GEO-1 (75° W) |
| LUNA-1 | IAU_MOON, then Shackleton Base | km | TT | 30 s; 5 s in descent | 100 km polar orbit; deorbits 09-02 12:00, coasts half an ellipse to a 15 km perilune, then a 10 min powered descent onto Shackleton Base, where it reparents at touchdown (13:07) |
| 2 launches | their pad's facility, then IAU_EARTH | km | TT | 60 s; 5 s in ascent | LAUNCH-A from KSC LC-39A (09-01 14:00, 400 km, 51.6°), LAUNCH-B from Andøya (09-02 18:00, 550 km, 97.6°); a 9 min ascent into an orbit whose plane passes over the pad |
| SELENE-1 | KSC LC-39A → IAU_EARTH → GCRF → IAU_MOON → Shackleton Base | km | TT | 30 s; 5 s in ascent and descent | pad to lunar surface as a patched conic: liftoff 09-01 08:35 from a pad 120 m east of the KSC origin, a 200 km parking orbit at 30°, translunar injection 09:35 (3.15 km/s), the Moon's sphere of influence 09-03 18:01, capture into a 100 km near-polar orbit 09-04 09:35 (0.88 km/s), two revolutions, touchdown 16:16, 120 m north of the Shackleton origin |
| CARGO-01 | KSC LC-39A, then SELENE-1, then Shackleton Base | m | TAI | 30 s | a rover that drives the KSC roads to the pad, boards SELENE-1 at 08:05, rides stowed on its hull, steps off 1 h after touchdown and joins the base's logistics tours |
| MARS-TRANSFER-1 | ICRF | km | TT | 30 s | cruising to Mars on the Sun-centred conic from the real Earth on 2026-08-24 to the real Mars on 2027-07-20 (C3 39.7 km²/s²); 4.4 → 7.2 million km from Earth during the window |
| Parker Solar Probe | ICRF | km | UTC | 60 s | the real spacecraft, on JPL Horizons' state vectors; perihelion 09-04 14:33 at 9.85 solar radii and 191 km/s |
| 14 crawlers | their host spacecraft | m | TAI | 30 s | hull robots looping a band around the host's 4 × 2 × 2 m hull, 4 of them on MARS-TRANSFER-1; CRAWLER-01 steps off LUNA-1 onto Shackleton Base 1 h after touchdown and surveys the grid cell the base's own surveyors leave free |
| 10 aircraft | IAU_EARTH | km | UTC | 60 s | 2-4 great-circle legs among 15 real airports: climb to 11 km, cruise at 900 km/h, descend, 1.5-3 h turnarounds |
| 20 ships | IAU_EARTH | km | GPST | 60 s | 8 hand-placed sea lanes (Malacca/Suez, Panama, Cape route, transpacific, transatlantic, …) at 22-40 km/h, docking 8-24 h at each end |
| Bedford Basin | IAU_EARTH | km | TAI | 1 h | the regatta venue, a facility whose ENU frame sits at the course centre in Bedford Basin, Halifax |
| 10 sailboats, RC-BOAT | Bedford Basin | m | GPST | 5 s on race day (09-05 13:00–18:00 ADT), else 1 h | leave the Bedford Basin Yacht Club docks one a minute from 13:00 ADT, motor through Bedford Bay to the start area, race a 3-lap windward-leeward (1.2 km beat, gun 14:00 ADT) under a strategy each, then motor home. RC-BOAT anchors at the line's starboard end |
| 4 marks | Bedford Basin | m | TAI | 1 min while laid (13:30–17:30 ADT) | windward mark, a 2-mark leeward gate and the start pin; no rows outside that window |
| 3 met buoys | Bedford Basin | m | TAI | as the boats | moored; the wind field itself is in `out/wind.arrow` |
| Squamish Valley fire | IAU_EARTH | km | TAI | 1 h | the wildfire venue, a facility whose ENU frame sits at the ignition point, east of the Squamish River, BC |
| 12 crews | Squamish Valley fire | m | GPST | 30 s from ignition (09-03 14:00 PDT), else 1 h | wait at the ICP 2 km down-valley; from 1 h after ignition walk and dig line (100 m/h) where the incident commander sends them |
| fire vertices (90 of 240) | Squamish Valley fire | m | TAI | 1 min while spreading, then 10 min | the perimeter: 48 on a 20 m ignition circle, more born midway as the front stretches; each stops for good at finished line, the river, or burnt ground |
| trenches (41 of 620) | Squamish Valley fire | m | TAI | 10 min once finished | one per 50 m of finished line: pose at the midpoint along the line, `dimensions` 50 × 1 × 0.5 m |
| Steyr plant | IAU_EARTH | km | TAI | 1 h | the factory, a facility on an industrial parcel in Steyr, Upper Austria; 3 conveyor lines 15 m apart |
| lines, machines A/B/C, stators, outer rings, rotors, inner rings | the plant, the line, Machine B, the stator, the shaft | m | TAI | 1 h | each part fixed in its parent's frame. Machine B is a 10 m belt; its gearmotor (the stator) sits at the drive pulley with its x along the pulley axle; two 6205-size bearings sit 0.15 m either side of the shaft's centre |
| 3 shafts, 6 cages, 48 balls | the stator, an outer ring, a cage | m | TAI | 5 s in the shift (09-02 06:00–14:00 CEST), 20 Hz in the burst (08:00–08:10), else 1 h; shafts also 5 kHz in eight 10 s captures | the shaft turns at 60 rpm with 10 s ramps and a break 10:00–10:30. Poses come from the bearing dynamics ([bearing_dynamics.md](bearing_dynamics.md)): the shaft's µm displacement and tilt in its stator, each cage at 0.397 × the shaft less its slip, each ball on its pocket's wandered angle, seated on the outer race when loaded, spinning at −2.32 × the shaft less its slip. The spin is in `angular_velocity`; shaft capture rows carry `acceleration`, ball rows their contact load over their mass |
| 1,350 boxes | Machine B, then Machine C | m | TAI | 5 s while on the belt | one spawned every 60 s per running line; it rides the 10 m belt at the pulley's turn × 0.1 m (0.63 m/s, 16 s) and its last row is on Machine C; 450 per line |
| 4 bodies | ICRF | km | TAI | 1 h | `append_snapshot` of Sun, Earth, Moon, Mars |

- **Frame tree.** The deepest chain is 7 hops, a factory ball to IAU_EARTH; among the vehicles
  it is 3, CRAWLER-04 riding the landed LUNA-1 on Shackleton Base on IAU_MOON. Astro frames are
  roots that the kernels resolve.
  The bodies also appear as entities under ICRF; their ids are the IAU frames' ids, but frame
  resolution never reads those rows.
- **Inertial roots.** Rows between bodies are stored in GCRF (Earth-centred) or ICRF (centred
  on the solar-system barycentre), both with J2000 axes. soloc has no Sun-centred inertial
  frame, so a Sun-centred path is stored as the Sun's barycentric position plus the
  heliocentric one.
- **Parent changes.** Exactly 1,360. There are 10 for vehicles: three launches, SELENE-1's two
  hand-offs between frames in flight, two landings, CARGO-01 boarding, and two disembarks. The
  other 1,350 are box handovers from Machine B to Machine C.
- **The factory tree.** A ball resolves through 7 frames to IAU_EARTH: ball → cage → outer ring
  → stator → Machine B → line → plant → IAU_EARTH. At the 5 s cadence a ball spins 11.6 turns
  between rows, so its quaternion alone aliases the motion; `angular_velocity` carries the spin,
  and the 20 Hz burst resolves it (at most 42° per row).
- **Real data.** Parker Solar Probe's rows are Horizons' vectors as they come, with no
  interpolation. They carry `estimate_type` `ESTIMATED` and `source_id`
  `jpl.nasa.gov/horizons`. After 2026-06-17 Horizons serves the mission's reference
  trajectory, not a tracking fit. Its attitude is modelled (body z to the Sun).
- **Timescales.** Rows are tagged UTC, GPST, TT or TAI, and `append` normalises every one to
  TAI. Only fixed-offset scales are used, so a child's epoch and its host's land on the same TAI
  nanosecond.
- **Zero-order hold.** A row framed on a moving entity resolves through that entity's latest pose
  at or before it. Every spacecraft therefore reports on every epoch of its crawlers (30 s when
  it carries one).
- **Attitude.** Robots face their direction of travel (body z up). Spacecraft fly nadir-pointing
  LVLH, frozen relative to the site on a pad or after touchdown; on a Sun-centred path nadir is
  the Sun. Aircraft and ships are forward-right-down along their track.
- **Other columns.** Every sim row carries velocity (m/s, in its parent frame), and every row
  except a facility's also carries `mass_kg` and `dimensions` (m). Apart from Parker's,
  `source_id` is `sim.soloc/kinematic_sim_v1` and `estimate_type` is `SIMULATED`.
- **Arenas.** The regatta's boats and the wildfire's crews are stepped online. On each decision
  tick, the driver flushes its buffer and the strategy reads its pieces back through
  `current_state`. The regatta decides every 10 s from the warning signal, the fire every 5 min
  from 1 h after ignition. The default commander has the fire contained 20.5 h after ignition,
  at 18.7 ha and 1.8 km of line. See [arena.md](arena.md).
- **Run-dependent schedules.** Fire vertices and trenches are born (and vertices stop) when the
  run says so. `check_sim` checks their rows against the replayed fire instead of a fixed
  schedule.
- **Aiming.** Every command builds the roster with kernel reads through the server, so the Moon
  flight and the Mars transfer are aimed at where the bodies really are.

`sim/fetch_horizons.py` reads the window from `sim/scenario.py`; run it again (it skips a table
that already covers the window) after changing `T0` or `T_END`.

## Separate runs

The roster splits into six groups (`GROUPS` in `sim/roster.py`) that share no rows except
the body snapshots: `space` (the facilities, spacecraft, probes, robots, crawlers and cargo,
which ride on one another), `aircraft`, `ships`, `regatta`, `wildfire` and `factory`.
`python run_sim.py --scenario GROUP` runs one group on a fresh server into
`out/GROUP/sim_study_0.arrow`. Its wind and fuel tables go beside it when the group has
them, so every tool works on a part as it does on the full ledger, and `check_sim` runs the
checks for the fleets the part holds.

`python -m tools.merge_sim` then puts every part through an empty server, in group order, so
`Ledger::append` validates each row again. It saves `out/sim_study_0.arrow` and concatenates
the side tables. Every part writes the hourly Sun, Earth, Moon and Mars snapshots. The merge
keeps one copy of an entity found in several parts, and fails if the copies differ in any
column. It also fails if the saved file holds fewer rows than it sent, which a
`memory_limit` eviction would cause. To change one scenario, rerun only its group and merge
again.

| Group | Entities | Rows | Run |
|---|---|---|---|
| space | 72 | 952,583 | 33 s |
| aircraft | 10 | 72,010 | 5 s |
| ships | 20 | 144,020 | 12 s |
| regatta | 19 | 53,109 | 4 s |
| wildfire | 144 | 139,580 | 13 s |
| factory | 1,438 | 2,222,374 | 319 s (~140 s of it integrating the bearing captures) |

Each part also holds the 484 snapshot rows. The merge takes about 45 s, most of it copying the
12M-row bearing truth table. Merging the six
parts reproduces the monolithic run row for row (`python -m tests.compare_ledgers A B`),
names and side tables included. That holds because each group's rows depend only on the
seed and on the group's own entities.

## Simplifications

- **Orbits** are two-body Kepler in each body's IAU frame frozen at T0, spun about the IAU z axis
  at the IAU rotation rate. Libration and pole drift are ignored, so lunar orbits wobble by tens
  of km when viewed in ICRF.
- **Launch, descent and flight profiles** are kinematic. They are splines and smoothsteps that
  match position and velocity at their ends, not dynamics.
- **The Moon flight** is a patched conic: two-body about the Earth out to the Moon's sphere of
  influence (66,183 km), two-body about the Moon inside it, with impulsive burns. Position and
  velocity match at the patch. Liftoff is timed so the parking plane holds the translunar
  target, and the burn is placed where it is cheapest.
- **The Mars transfer** is a Sun-centred two-body arc between the planets' centres; their own
  gravity is left out. The low-energy window opens in October 2026, so leaving in August costs
  about four times the usual launch energy.
- **Great-circle tracks** are measured on a 6,371 km sphere and placed on WGS84 with their
  latitudes read as geodetic. Aircraft ground speed therefore comes out at 897-906 km/h rather
  than exactly 900.
- **Sea lanes** are checked against Natural Earth 1:50m land (`sim/land.py`). At that scale the
  Suez and Panama canals are land, so rows inside the boxes in `land.CANALS` are exempt.
- **Sailing** is kinematic: polar speed for the true wind at the boat, with tacks and gybes
  costing sailing time. There is no current, leeway, heel, acceleration or collision, and boats
  do not affect each other. Natural Earth does not resolve Bedford Basin, so the water check uses
  `scenario.BASIN_WATER`, simplified from OpenStreetMap.
- **The fire** spreads on flat ground over hand-drawn fuel zones (`sim/fuel.py`): the river from
  OpenStreetMap buffered 60 m (non-burnable), a riparian band to 250 m (×0.5), a power-line
  corridor (×0.3), a slash cutblock (×1.8) and conifer elsewhere. The perimeter is a ring of
  points moving along their normals (Huygens), with no spotting, crowning, slope or burnout. A
  pocket of fuel left inside the line (here, the low-fuel corridor) keeps the fire "spreading"
  long after the line is closed, which is why containment takes ~20 h.
- **The factory** drives its shaft at a prescribed speed (a stiff drive), so the bearings never
  slow the belt, and the three lines run the same shift and produce the same 450 boxes. The
  bearing model is a lumped one with a rigid housing; see
  [bearing_dynamics.md](bearing_dynamics.md#simplifications). The plant site is an unnamed
  industrial parcel on OpenStreetMap.
- **The venue plane.** Regatta rows sit at z = 0 on the venue's ENU plane, which rises above the
  sea away from its origin: about 1.1 m at the docks, 3.8 km out.
