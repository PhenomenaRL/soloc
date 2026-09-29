# solar-sim

A Python-driven, 3-day solar-system simulation that writes a soloc ledger through
`soloc-server` over Arrow Flight. Work in progress: step 1 (facilities, facility robots, the
driver and its checks) exists so far.

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
| 4 bodies | hourly `append_snapshot` of Sun, Earth, Moon, Mars |

`check_sim.py` checks only the fleets present in the file:

| Check | What it confirms |
|---|---|
| roster | `current_state` holds every facility, robot and body, and nothing else |
| names | the `.names.arrow` sibling reloaded a name for every sim entity |
| units / timescale | only `km`/`m` stored, every row normalised to TAI |
| cadence | row counts per entity and every epoch on its cadence grid |
| robot frames | every robot row is framed on its own facility |
| site area | every stored robot position inside the site square, at z = 0 |
| ground level | robot rows at 0/24/48/72 h resolved through the facility to IAU_EARTH/IAU_MOON sit on the surface and near the site |
| ICRF | the same rows in ICRF sit at the body's radius from its snapshot position |

`scenario.py` holds the roster and every schedule; the models live in `models/`.
