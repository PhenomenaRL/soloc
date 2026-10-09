# sim_study_0

A 5-day simulation of 1,703 entities (sites, robots, spacecraft, aircraft, ships, a sailing
regatta, a wildfire with its crews, a factory down to its bearing balls, and the real Parker Solar
Probe) across the Earth, the Moon and Mars, written into a soloc ledger from Python. The regatta and the wildfire are arenas: their boats
and crews are steered by strategies that read the ledger back as events run
([docs/arena.md](docs/arena.md)).
Python has no soloc bindings, so everything goes through `soloc-server` over Arrow Flight using
`soloc_client.py`, a single pyarrow file (see [docs/client_quickstart.md](docs/client_quickstart.md)
to use it on its own).

## Layout

| Path | What it is |
|---|---|
| `fetch_data.sh`, `serve.sh` | download kernels and data into `data/`; run `soloc-server` |
| `config.toml` | the server config `serve.sh` uses: a 2 GB memory limit |
| `soloc_client.py` | the Python client for `soloc-server` |
| `run_sim.py` | generates the ledger into `out/` |
| `check_sim.py` | verifies a saved ledger |
| `sim/` | the simulation itself: `scenario.py` (roster and schedules), `roster.py`, `models/`, geometry, ephemerides |
| `tools/` | inspect a saved ledger: plots, a row viewer, the frame tree, a standalone 3D viewer |
| `tests/` | `smoke_test.py`, the server contract the sim relies on |
| `docs/` | [scenario](docs/scenario.md), [arena](docs/arena.md), [checks](docs/checks.md), [outputs](docs/outputs.md), [client quickstart](docs/client_quickstart.md), [client reference](docs/client_reference.md), [orbits](docs/orbits.md), [bearing dynamics](docs/bearing_dynamics.md), [memory limit](docs/memory_limit.md) |
| `data/`, `out/` | downloads and results (gitignored) |

## Setup (once)

From `examples/sim_study_0/`:

```bash
./fetch_data.sh                          # kernels, land polygons, three.js into data/ (~99 MB)
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python -m sim.fetch_horizons   # Parker Solar Probe's vectors from JPL Horizons
```

## Run

Terminal 1, the server (an empty in-memory ledger; the first run builds it in release mode):

```bash
./serve.sh
```

Terminal 2, the client:

```bash
source .venv/bin/activate
python run_sim.py                          # ~6.5 min → out/sim_study_0.arrow (~1.8 GB)
python check_sim.py out/sim_study_0.arrow  # PASS/FAIL table; expect 95/95
```

To develop one scenario without rerunning the rest, run it alone and merge the parts
([docs/scenario.md](docs/scenario.md#separate-runs)):

```bash
python run_sim.py --scenario factory       # → out/factory/sim_study_0.arrow; restart serve.sh per group
python -m tools.merge_sim                  # every out/<group>/ → out/sim_study_0.arrow
```

`run_sim.py` and `tools.merge_sim` refuse a non-empty ledger, so restart `serve.sh` before each run. The server runs
with a 2 GB memory limit from `config.toml`, which holds the whole sim; see
[docs/memory_limit.md](docs/memory_limit.md) to run it with a smaller one.

## All commands

Run every command from `examples/sim_study_0/` with the venv active. The ones in `sim/`,
`tools/` and `tests/` run as modules (`python -m`), so they can import `soloc_client` and `sim`.

| Command | Server | Does |
|---|---|---|
| `./fetch_data.sh` | – | downloads kernels, land polygons and three.js into `data/` |
| `python -m sim.fetch_horizons` | – | downloads Horizons tables into `data/`; rerun after changing the window |
| `./serve.sh` | – | runs `soloc-server` with the kernels in `data/` and `config.toml` (`SOLOC_CONFIG=FILE` to use another) |
| `python run_sim.py` | empty | generates `out/sim_study_0.arrow` and its side tables `out/wind.arrow`, `out/fuel.arrow` and `out/bearing_truth.arrow` (`--out` to change); `--regatta-policy MOD:FN` / `--wildfire-policy MOD:FN` swap a strategy in; `--scenario GROUP` runs one group into `out/GROUP/` |
| `python -m tools.merge_sim [GROUP...]` | empty | merges the parts in `out/*/` into `out/sim_study_0.arrow` with its wind and fuel tables |
| `python -m tests.compare_ledgers A B` | none | checks two saved ledgers hold identical rows and names |
| `python check_sim.py FILE` | any | loads `FILE` and prints the checks in [docs/checks.md](docs/checks.md) |
| `python -m tools.plot_sim FILE` | any | PNGs into `out/plots/`, ~4 min |
| `python -m tools.bearing_eval FILE` | any | the bearing dynamics evaluation (fidelity, detection, cost) and 3 PNGs into `out/plots/`; see [docs/bearing_dynamics.md](docs/bearing_dynamics.md) |
| `python -m tools.export_viewer FILE` | any | standalone 3D viewer → `out/sim_study_0_3d.html` |
| `python -m tools.view_sim FILE` | none | prints rows decoded; `--summary`, `--entity NAME`, `--tail`, `--limit N`, `--schema` |
| `python -m tools.snapshot_sim FILE --scenario regatta\|wildfire\|factory` | none | situation PNGs into `out/snapshots/`; `--at UTC` or `--every 10m` |
| `python -m tools.topo_sim FILE` | none | frame tree and parent changes; `--at UTC`, `--server URL` to cross-check |
| `python -m tests.smoke_test` | empty | checks the server contract |

"any" means the command loads the file into the server itself, replacing its ledger, so it can
run straight after `run_sim.py`. Commands that talk to a server take `--server` (default
`grpc://localhost:50051`). `run_sim.py`, `check_sim.py` and `tools.plot_sim` take `--seed`
(default 7), which must match. Details of each output are in [docs/outputs.md](docs/outputs.md).
