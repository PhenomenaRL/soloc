# solar-sim

A Python-driven, 3-day solar-system simulation that writes a soloc ledger through
`soloc-server` over Arrow Flight. Work in progress: steps 1-2 (facilities, facility robots,
spacecraft with their hull crawlers, the driver and its checks) exist so far.

## Setup (once)

From `examples/solar-sim/`:

```bash
./fetch_kernels.sh                  # de440s, mar099s, pck11 into kernels/ (~96 MB)
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
```

`kernels/`, `.venv/` and `out/` are gitignored.

## Smoke test

Terminal 1 starts the server with an in-memory ledger (the first run builds it in release mode):

```bash
./serve.sh
```

Terminal 2:

```bash
source .venv/bin/activate
python smoke_test.py
```

It prints one PASS/FAIL line per check, plus the findings, and exits non-zero on any FAIL. The
test needs an empty ledger, so restart `serve.sh` before each run.

| Check | What it confirms |
|---|---|
| `get_schema` | the client builds batches against the server's own schema |
| mint vectors | Python ids match `frozen_mint_vectors` in `identity.rs` |
| append per timescale | TAI, UTC, GPST and TT rows are stored as the same TAI instant |
| (a) epoch base | the JSON `epoch_tai_s` counts from J1900 |
| (b) Mars | body 499 resolves (it comes from `mar099s.bsp`; `de440s` lacks it) |
| (c) quaternion | `[w,x,y,z]` rotates child-frame vectors into the parent frame |
| round trip | `save_ledger` → `load_ledger` keeps every row and writes the `.names.arrow` sibling |

`--server grpc://host:port` points it at a server other than `localhost:50051`.

## Run the sim

With a freshly started `./serve.sh` (the driver refuses to write into a non-empty ledger):

```bash
source .venv/bin/activate
python run_sim.py --out out/solar_sim.arrow     # per-batch progress; stops on the first append failure
python check_sim.py out/solar_sim.arrow         # reload + PASS/FAIL table
python plot_sim.py out/solar_sim.arrow          # PNGs into out/plots/
```

`run_sim.py` ticks 2026-09-01T00:00 → 2026-09-04T00:00 UTC on a 5 s grid, appends one batch per
10 min of sim time, snapshots Sun/Earth/Moon/Mars hourly, and saves the ledger plus its
`.names.arrow` sibling. `check_sim.py` and `plot_sim.py` load the file back into the server
first, so they can run straight after it. All three take `--server`; `run_sim.py` and the two
readers take `--seed` (default 7), which must match between them.

| Scenario so far | |
|---|---|
| 4 facilities | KSC LC-39A, Andøya Spaceport, JSC Houston (IAU_EARTH), Shackleton Base (IAU_MOON); pose is the site's ENU frame, hourly rows |
| 40 robots | 10 per facility, random-waypoint roving in a 300 m × 300 m site area, positions in site-ENU metres, 30 s rows |
| 7 orbiters | LUNA-2/3 (100 km polar), MARS-1 (300 km, 93°), MARS-2 (3,200 × 8,800 km, 75°), LEO-1 (420 km, 51.6°), SSO-1 (700 km, 98.2°), GEO-1 (75° W); two-body Kepler in the IAU body-fixed frame, nadir-pointing LVLH, km, TT |
| LUNA-1 | 100 km polar lunar orbit; deorbits 09-02 12:00, coasts half an ellipse to a 15 km perilune, then a 10 min powered descent onto Shackleton Base, where it reparents at touchdown (13:07) |
| 2 launches | LAUNCH-A from KSC LC-39A (09-01 14:00, 400 km, 51.6°) and LAUNCH-B from Andøya (09-02 18:00, 550 km, 97.6°): framed on the pad's facility until liftoff, then a 9 min ascent on IAU_EARTH into an orbit whose plane passes over the pad |
| 10 crawlers | hull robots looping a band around their host's 4 × 2 × 2 m hull, in the host's body frame (m); every lander carries at least one, and its first steps off onto the site 1 h after touchdown and roves there |
| 4 bodies | hourly `append_snapshot` of Sun, Earth, Moon, Mars |

Spacecraft report every 60 s, or every 30 s when they carry crawlers (a crawler's epochs must
be a subset of its moving host's), and every 5 s during an ascent or from deorbit to touchdown.

`check_sim.py` checks only the fleets present in the file:

| Check | What it confirms |
|---|---|
| roster | `current_state` holds every facility, spacecraft, robot, crawler and body, and nothing else |
| names | the `.names.arrow` sibling reloaded a name for every sim entity |
| units / timescale | only `km`/`m` stored, every row normalised to TAI |
| schedule | each entity's stored epochs are exactly its model's schedule |
| zero-order hold | every row framed on a spacecraft shares its epoch with a row of that spacecraft |
| robot frames | every robot row is framed on its own facility |
| site area | every stored robot position inside the site square, at z = 0 |
| ground level | robot rows at 0/24/48/72 h resolved through the facility to IAU_EARTH/IAU_MOON sit on the surface and near the site |
| ICRF | the same rows in ICRF sit at the body's radius from its snapshot position |
| orbits | per orbit, stored radii inside Kepler's periapsis–apoapsis range and the nodal period (timed from z crossings) equal to Kepler's; GEO instead hangs still in IAU_EARTH |
| launches | pad rows at the facility origin and resolving onto the pad; the ascent climbs monotonically |
| landing | the descent stays above the ground; after touchdown the lander resolves onto the site |
| crawlers | on the hull in the stored frame, and within hull reach of the host once both are resolved to the body frame |
| disembark | the crawler is framed on the facility afterwards, at ground level inside the site |
| topology | the parent changes are exactly the 2 launches, the landing and the disembark |

`plot_sim.py` draws robot tracks per site, the spacecraft body-centred with ICRF axes
(`orbits.png`), and altitude around each launch and the landing (`altitudes.png`).

`scenario.py` holds the roster and every schedule; the models live in `models/`.

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
