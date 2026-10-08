<img src="../../assets/soloc-icon.svg" width="72" align="right" alt="soloc">

# soloc-ledger

**The append-only store at the heart of [soloc](../../).** A `Ledger` holds a stream of Arrow
`RecordBatch`es built on any schema that embeds a `spacetimestamp` struct column (see
[`spacetimestamp`](../spacetimestamp/)), and turns them into answers about *where everything is*.

## Core concepts

- **Store raw.** Rows are appended in their original frame and native units and never
  rewritten. Reprojection to an astronomical frame is a query-time view, never re-stored.
- **Normalized on ingest.** `append` validates each batch, normalizes its timestamps to **TAI** timeframe
  (rows already on TAI pass through with no allocation), and folds its edges into a derived
  [`TransformTree`](../spacetimestamp/). A batch that would form a cycle or name an unresolvable
  parent frame is rejected whole.
- **Fast "where is X now."** A per-entity **pose cache** tracks each entity's highest-epoch pose, so
  the common query costs a handful of targeted reads rather than a full scan.
- **Bounded segments.** Appends collect in an unsealed tail. Once it holds more than 50 batches or
  256 MiB, the tail alone is concatenated into a sealed segment. Older segments are never copied
  again, so ingest costs O(rows), and a time-range query skips segments outside its range.
- **Memory limit.** `set_memory_limit(Some(bytes))` bounds `resident_bytes()` with a rolling
  window: the oldest segments are evicted for good, but each entity's latest row is kept
  ("pinned"), so `current_state` and present-epoch transforms still see every entity, and the
  topology history is trimmed to the window. Segments are kept to about an eighth of the limit
  (splitting larger ones), so eviction is gradual. The limit bounds the ledger's bytes, not the
  process, and needs an `id_column`.
- **Persistence.** `save_ipc` / `load_ipc` round-trip the ledger through Arrow IPC (local paths or,
  from the server, object-store URLs), one IPC batch per segment, rebuilding all derived state on
  load. A schema-only IPC form lets you bake an empty schema into an image so a fresh server starts
  with the right shape.

Querying/Reading:
- `query` / `stream_query` for spatiotemporal filters
- `current_state` for the single best row per entity (most recent wins; ties broken `MEASURED > ESTIMATED > SIMULATED`)
- Topology and names federate through `export_topology`/`merge_topology` and `export_names`/`merge_names_from_ipc_bytes`.

## A simple use case

```rust
use soloc_ledger::ledger::Ledger;
use soloc_ledger::schemas::entity::EntitySchema;
use spacetimestamp::query::SpatiotemporalFilter;

// Build a ledger for the first-party entity schema (or Ledger::new(&schema, "entity_id")).
let mut ledger = Ledger::for_schema::<EntitySchema>()?;

// Append measurements — validated, TAI-normalized, and topology-ingested in one call.
ledger.append(batch)?;

// "Where is everything right now?" — the best row per entity.
let now = ledger.current_state(None, None)?;

// Spatiotemporal query: everything seen in a time window.
let hits = ledger.query(&SpatiotemporalFilter::new().with_time_range(t0, t1))?;
```

Planetary and lunar poses come from the ephemeris for free — appending a celestial snapshot needs
no wrapper:

```rust
use soloc_ledger::ephemeris::{celestial_orbits, celestial_snapshot};

ledger.append(celestial_snapshot(&almanac, &ids, epoch)?)?;

// Orbits for display (elements + a path over one period, about each body's NAIF parent); not stored.
let orbits = celestial_orbits(&almanac, &[(earth, None), (moon, None)], epoch, 361)?;
```

## Roadmap

- [x] **Core capabilities** — append, spatiotemporal & current-state queries, IPC persistence
  (local + object-store), topology/name federation
- [ ] **Performance benchmarks** — a `criterion` suite to pin down append, query, and seal costs
- [x] **Bounded memory** — segmented storage and a rolling-window memory limit
- [ ] **Storage-organization optimizations** — spilling evicted segments to disk or object storage
  and rehydrating them on historical queries, and streaming loads larger than memory

## License

Apache-2.0. See the [workspace README](../../) for the ecosystem overview.
