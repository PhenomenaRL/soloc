# solar-sim

A Python-driven, 5-day solar-system simulation that writes a soloc ledger through
`soloc-server` over Arrow Flight. 102 entities (facilities, ground robots, spacecraft, hull
robots, aircraft, ships and a real probe) report pose, frame and time on Earth, the Moon and
Mars and in the space between them, in four timescales and two units. Each is framed on
whatever it rides on, and the frame tree changes under launches, landings, a flight from a pad
on Earth to the lunar surface, and robots boarding and leaving craft. The output is one
validated Arrow IPC file that reloads into a soloc ledger from Python (`load_ledger`) or Rust
(`Ledger::load_ipc`).

Python has no soloc bindings, so everything goes through the server: `do_put` is
`Ledger::append` (validation, TAI normalisation, topology and cycle checks on every batch),
`append_snapshot` adds the celestial bodies from the kernels, and `save_ledger` is
`Ledger::save_ipc`. The client is plain pyarrow.

## Setup (once)

From `examples/solar-sim/`:

```bash
./fetch_kernels.sh                  # de440s, mar099s, pck11, Natural Earth land, three.js into kernels/ (~99 MB)
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python fetch_horizons.py  # Parker Solar Probe's state vectors for the window, from JPL Horizons (1.4 MB)
```

`fetch_horizons.py` reads the window from `scenario.py`; run it again (it skips a table that
already covers the window) after changing `T0` or `T_END`. `kernels/`, `.venv/`, `out/` and
`__pycache__/` are gitignored.

## Run and verify

Terminal 1 starts the server with an empty in-memory ledger (the first run builds it in release
mode):

```bash
./serve.sh
```

Terminal 2:

```bash
source .venv/bin/activate
python run_sim.py --out out/solar_sim.arrow     # ~35 s; per-batch progress; stops on the first append failure
python check_sim.py out/solar_sim.arrow         # reload + PASS/FAIL table, exits non-zero on any FAIL
python plot_sim.py out/solar_sim.arrow          # PNGs into out/plots/, ~100 s
python topo_sim.py out/solar_sim.arrow --server grpc://localhost:50051   # frame tree + export_topology cross-check
```

`run_sim.py` refuses to write into a non-empty ledger, so restart `serve.sh` before each run.
`check_sim.py`, `plot_sim.py` and `topo_sim.py --server` load the file into the server
themselves (replacing its ledger), so they can run straight after it. All four take `--server`;
`run_sim.py`, `check_sim.py` and `plot_sim.py` take `--seed` (default 7), which must match.
All of them also read the kernels through the server while building the roster: the Moon
flight and the Mars transfer are aimed at where the bodies really are.

A full run prints `102 entities, 1,168,613 entity rows + 484 snapshot rows`. It writes
`out/solar_sim.arrow` (~580 MB) plus its `out/solar_sim.arrow.names.arrow` name registry.
`check_sim.py` then reports `66/66 passed`.

## What's in the ledger

The window is 2026-09-01T00:00 → 2026-09-06T00:00 UTC. The driver ticks a 5 s grid, asks every
entity whether it is due, and appends one batch per 10 min of sim time (721 appends).

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

`scenario.py` holds the roster and every schedule (times, orbits, airports, lanes); the models
live in `models/`.

### Simplifications

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
- **Sea lanes** are checked against Natural Earth 1:50m land (`land.py`). At that scale the
  Suez and Panama canals are land, so rows inside the boxes in `land.CANALS` are exempt.

## What check_sim.py checks

It checks only the fleets present in the file.

| Check | What it confirms |
|---|---|
| roster | `current_state` holds every facility, spacecraft, probe, robot, crawler, aircraft, ship and body, and nothing else |
| names | the `.names.arrow` sibling reloaded a name for every sim entity |
| units / timescale | only `km`/`m` stored, every row normalised to TAI |
| schedule | each entity's stored epochs are exactly its model's schedule |
| zero-order hold | every row framed on a spacecraft shares its epoch with a row of that spacecraft |
| robot frames | every robot row is framed on its own facility |
| site area | every stored robot position inside the site square, at z = 0 |
| ground level | robot rows at every 24 h resolved through the facility to IAU_EARTH/IAU_MOON sit on the surface and near the site |
| ICRF | the same rows in ICRF sit at the body's radius from its snapshot position |
| orbits | per orbit, stored radii inside Kepler's periapsis–apoapsis range and the nodal period (timed from z crossings) equal to Kepler's; GEO instead hangs still in IAU_EARTH |
| launches | pad rows at the pad's offset on the facility and resolving onto the pad; the ascent climbs monotonically |
| landings | the descent stays above the ground; after touchdown the lander resolves onto its landing point |
| Moon flight | each leg is on its frame (IAU_EARTH, GCRF, IAU_MOON); resolved to GCRF, the first row after each hand-off continues the curve of the three rows before it; the approach bottoms out at the capture altitude and the orbit holds it |
| cargo | CARGO-01 is framed on KSC, the ship and Shackleton in turn; resolved through the ledger it is on the ground at both ends and within 1 m of the ship at 9 epochs in between |
| Mars transfer | the arc leaves Earth's centre and, propagated 330 days, meets Mars's; the stored rows, made Sun-centred, keep their energy and angular momentum |
| Parker Solar Probe | the stored rows equal the Horizons table; its distance from the Sun agrees with the Sun's snapshot rows; perihelion falls inside the window at 9.8–9.9 solar radii |
| crawlers | on the hull in the stored frame, and within hull reach of the host once both are resolved to the host's frame |
| disembark | the crawler is framed on the facility afterwards, at ground level inside the site |
| topology | the parent changes are exactly the 10 that the models' phases predict |
| aircraft | every row between the lowest field and cruise altitude; at rest only on an airfield; 900 km/h (±1%) ground speed at cruise altitude |
| ships | at sea level; 22-40 km/h (±1%) under way; at rest only at a berth; every row on water |

Resolving rows through the ledger at historical epochs scans for the parent's pose once per
`(frame, epoch)`. The resolution checks therefore run on a few epochs, and the per-row checks
run on the stored values.

## Plots

`plot_sim.py` writes to `out/plots/`:

| File | Shows |
|---|---|
| `robots_{ksc,and,jsc,shk}.png` | each site's robot rows in site ENU over the road grid, one panel per robot titled with its role; Shackleton's includes CRAWLER-01 after it disembarks, and KSC's and Shackleton's include CARGO-01 |
| `orbits.png` | every orbiter body-centred with ICRF axes (x–y and x–z) around Earth, Moon and Mars |
| `altitudes.png` | altitude around each launch and landing, resolved through the ledger |
| `cislunar_selene-1.png` | SELENE-1 from liftoff to touchdown, resolved to GCRF: Earth-centred with the Moon's path, and Moon-centred in the orbit plane |
| `transfer_mars-transfer-1.png` | the Mars transfer Sun-centred: Earth's and Mars's paths over the same dates, the whole arc, and the stored 5 days on it |
| `heliocentric.png` | Parker Solar Probe Sun-centred, with perihelion marked and the Sun to scale |
| `aircraft.png` | aircraft tracks over the land polygons |
| `flight_altitudes.png` | each aircraft's altitude over the 5 days |
| `ships.png` | ship tracks over the land polygons, with every full lane dotted and the canal boxes outlined |

## 3D viewer

`export_viewer.py` turns a saved ledger into one standalone HTML file. The tracks, the body
ephemerides, the Earth coastlines and three.js are inlined, so the page opens offline by
double-click in any WebGL browser. Building it needs a running server:

```bash
python export_viewer.py out/solar_sim.arrow                     # → out/solar_sim_3d.html (~43 MB)
```

It is one scene, nested the way the ledger's frames are:
- **ICRF:** the Sun, Mercury, Venus, Earth, the Moon and Mars, at their true positions, with a
  full orbit drawn for each (the Moon's around Earth). Lighting comes from the Sun. Parker
  Solar Probe and MARS-TRANSFER-1 are here, with the whole Mars arc dashed.
- **GCRF:** Earth-centred and not turning. SELENE-1's translunar leg is here.
- **Each body's IAU frame:** turns with the body. Its spacecraft, aircraft and ships are in there,
  in body-fixed km as stored.
- **Each site's ENU frame:** sits on its body at the facility's stored pose, with its robots in
  metres over the road grid.

Body positions and orientations come from the server: a zero offset in each body frame is
exchanged to ICRF, at 5 min steps over the window and along each orbit. That is the same
resolution the ledger does. Rows framed on a facility (a craft on its pad or landed) are composed
through the facility's stored pose.

**Navigating:**
- "Go to" flies the camera to the solar system, a body or a site; "Follow" tracks any vehicle or
  robot. A craft that changes frame is followed across them: SELENE-1 from its pad, round the
  Earth, across GCRF and down to the Moon.
- Vehicles, trails and labels appear as you close in on their body, and robots on their site.
  Craft in GCRF or ICRF show from any distance.
- The camera can turn with the body or stay fixed in ICRF. Trails can be drawn body-fixed or
  non-rotating.
- Floating-origin rendering keeps it precise from 4 AU down to a metre.

Every entity is a small model with its body axes (x red, y green, z blue) at its interpolated
pose; models keep a constant size on screen. The controls:
- a time slider with ticks at the launches, burns, hand-offs, touchdowns, boardings and
  disembarks, and at Parker's perihelion
- play at 1 min/s up to 3 h/s
- trails from 15 min to the whole track
- labels, a hover readout (position in the entity's own frame), and a clickable legend that
  hides a category

Robots riding a craft are left out (crawlers on a hull, CARGO-01 in flight), since a 2 m hull
is invisible at orbit scale; following CARGO-01 holds at the pad until it steps off. On Earth sites
the survey tracks look jagged: a robot moves up to 45 m between its 30 s rows, and the survey
rows are 10 m apart, so straight lines between samples cut the corners.

## Look at the data

`view_sim.py` prints a saved ledger as a table straight from the file (no server). Ids show as
names, vocabulary codes as tokens and epochs as UTC; columns that are empty in every shown row
are left out.

```bash
python view_sim.py out/solar_sim.arrow                          # first 20 rows
python view_sim.py out/solar_sim.arrow --entity AND-R03 --limit 10
python view_sim.py out/solar_sim.arrow --entity shackleton --tail --limit 5
python view_sim.py out/solar_sim.arrow --summary                # one line per entity: rows, time span, frames
python view_sim.py out/solar_sim.arrow --schema
```

`--entity` matches any part of a name, case-insensitively, and can be repeated.

`topo_sim.py` summarises the frame topology the same way: the parent/child tree at an instant
(astro frames as roots, each entity with its row count and cadence), then every parent change
over the run.

```bash
python topo_sim.py out/solar_sim.arrow                          # tree at the last epoch + events
python topo_sim.py out/solar_sim.arrow --at 2026-09-02T12:00:00 # tree at a UTC instant
python topo_sim.py out/solar_sim.arrow --server grpc://localhost:50051
```

It derives topology from the rows as the ledger does: an entity's parent is the `frame_id` of
its latest row at or before t. `--server` also loads the file into a running server and checks
that its `export_topology` log matches the row-derived events exactly.

## Reload from Rust

The file is a plain soloc ledger. The server's `load_ledger` action, which `check_sim.py` uses,
is exactly this call:

```rust
use std::path::Path;
use soloc_ledger::ledger::Ledger;

let ledger = Ledger::load_ipc(Path::new("examples/solar-sim/out/solar_sim.arrow"), "entity_id")?;
```

`load_ipc` also picks up the `.names.arrow` sibling next to the file, if present.

## Smoke test

`smoke_test.py` checks the server contract the sim relies on. It needs an empty ledger, so
restart `serve.sh` first:

```bash
python smoke_test.py
```

| Check | What it confirms |
|---|---|
| `get_schema` | the client builds batches against the server's own schema |
| mint vectors | Python ids match `frozen_mint_vectors` in `identity.rs` |
| append per timescale | TAI, UTC, GPST and TT rows are stored as the same TAI instant |
| (a) epoch base | the JSON `epoch_tai_s` counts from J1900 |
| (b) Mars | body 499 resolves (it comes from `mar099s.bsp`; `de440s` lacks it) |
| (c) quaternion | `[w,x,y,z]` rotates child-frame vectors into the parent frame |
| round trip | `save_ledger` → `load_ledger` keeps every row and writes the `.names.arrow` sibling |

## Files

| File | Role |
|---|---|
| `fetch_kernels.sh`, `serve.sh` | fetch kernels, land polygons and three.js; run `soloc-server` with the kernels |
| `fetch_horizons.py` | fetch each probe's state vectors for the window from JPL Horizons |
| `ephemeris.py` | body centres and rotations from the server's kernels, at any epoch |
| `export_viewer.py`, `viewer_template.html` | the standalone 3D viewer: data export and the page it is inlined into |
| `soloc_client.py` | id minting, epoch encoding, batches built against the server schema, Flight calls |
| `geo.py` | body shapes, inertial roots, geodetic ↔ body-fixed, ENU, quaternions, Kepler, conics from a state, Lambert, body spin, LVLH, splines, great circles |
| `land.py` | Natural Earth land: point-in-land test and outlines |
| `scenario.py` | the roster and every schedule |
| `models/` | `facility.py`, `robot.py` (robots, crawlers and the cargo robot), `spacecraft.py` (orbiters, launches, landers, the Moon flight, the Mars transfer), `probe.py`, `track.py` (shared by `aircraft.py` and `ship.py`) |
| `run_sim.py` | the driver: roster, tick loop, batches, snapshots, names, save |
| `check_sim.py`, `plot_sim.py`, `topo_sim.py`, `view_sim.py` | verify and inspect a saved run |
| `smoke_test.py` | the server contract checks |
