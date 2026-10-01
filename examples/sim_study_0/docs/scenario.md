# What's in the ledger

The window is 2026-09-01T00:00 → 2026-09-06T00:00 UTC. The driver (`run_sim.py`) ticks a 5 s
grid, asks every entity whether it is due, and appends one batch per 10 min of sim time (721
appends). Each `do_put` is `Ledger::append` (validation, TAI normalisation, topology and cycle
checks on every batch), `append_snapshot` adds the celestial bodies from the kernels, and
`save_ledger` is `Ledger::save_ipc`.

A full run prints `102 entities, 1,168,613 entity rows + 484 snapshot rows` and writes
`out/sim_study_0.arrow` plus its `out/sim_study_0.arrow.names.arrow` name registry.

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
| 4 bodies | ICRF | km | TAI | 1 h | `append_snapshot` of Sun, Earth, Moon, Mars |

- **Frame tree.** The deepest chain is 3 hops: CRAWLER-04 rides the landed LUNA-1, which sits
  on Shackleton Base, which sits on IAU_MOON. Astro frames are roots that the kernels resolve.
  The bodies also appear as entities under ICRF; their ids are the IAU frames' ids, but frame
  resolution never reads those rows.
- **Inertial roots.** Rows between bodies are stored in GCRF (Earth-centred) or ICRF (centred
  on the solar-system barycentre), both with J2000 axes. soloc has no Sun-centred inertial
  frame, so a Sun-centred path is stored as the Sun's barycentric position plus the
  heliocentric one.
- **Parent changes.** Exactly 10: three launches, SELENE-1's two hand-offs between frames in
  flight, two landings, CARGO-01 boarding, and two disembarks.
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
- **Aiming.** Every command builds the roster with kernel reads through the server, so the Moon
  flight and the Mars transfer are aimed at where the bodies really are.

`sim/fetch_horizons.py` reads the window from `sim/scenario.py`; run it again (it skips a table
that already covers the window) after changing `T0` or `T_END`.

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
