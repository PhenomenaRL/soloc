# Memory limit

`soloc-server` can cap the memory its ledger holds. `serve.sh` starts it with
[`config.toml`](../config.toml), which sets:

```toml
[storage]
memory_limit = "2GB"     # B, KB, MB, GB, TB (powers of 1000) or KiB, MiB, GiB, TiB (1024)
```

The full 5-day sim is about 575 MB, so 2 GB holds every row and the README's runs are
unaffected. The server prints the limit at startup and the ledger's size on every load and save:

```text
soloc-server: memory limit 2000000000 bytes (0 batches, 0.0 MB resident)
saved 42 batches, 574.5 MB resident to out/sim_study_0.arrow
```

## Past the limit

- **The oldest rows are evicted, for good.** The ledger becomes a rolling window over the most
  recent history. Eviction works in pieces of about an eighth of the limit, so the window
  moves smoothly rather than in big jumps.
- **Each entity's latest row is always kept.** `current_state` still returns every entity, and
  exchanges through any entity's frame still resolve at the present time, even for an entity
  that last reported before the window.
- **The limit covers everything the server holds,** including a file loaded with `load_ledger`.
  Loading a file bigger than the limit trims it to the newest part as it loads.
- **The limit counts the ledger, not the process.** The server also holds the kernels (about
  135 MB idle) and decoding buffers, so set the limit well below the memory you have.

## Try a small limit

Record the sim under a limit that holds only part of it:

```bash
# terminal 1: the same server with a copy of config.toml that sets 150MB
sed 's/"2GB"/"150MB"/' config.toml > out/config_150mb.toml
SOLOC_CONFIG=out/config_150mb.toml ./serve.sh

# terminal 2
python run_sim.py --out out/sim_window.arrow
python -m tools.view_sim out/sim_window.arrow --summary
```

The run takes the same ~30 s but saves `14 batches, 137.9 MB resident`: about 280,000 of the
1.17 million rows, covering roughly the last 28 hours (the summary's `first_utc` column starts
between `09-04 19:10` and `20:00`), with all 106 entities. The server's peak memory drops from
about 725 MB to about 345 MB. `topo_sim` on that file shows every entity as a first sighting:
the parent changes happened before the window.

## Working with a windowed ledger

- **`current_state` hides old rows by default.** It returns rows from the hour before the newest
  one. To include an entity whose latest row is older, pass `not_before_tai_ns` (see the
  [client reference](client_reference.md)).
- **The checks and plots need the full 5 days.** `check_sim` stops with
  `FAIL  ledger holds the full history from t0  (history starts at 09-04 20:00:00; …)`, and
  `plot_sim` fails on the evicted spans. `view_sim`, `topo_sim` and `export_viewer` work on a
  window.
- **`check_sim` loads its file into the running server,** so checking even the full ledger
  against a 150 MB server trims it first. Run the checks under the 2 GB default.
