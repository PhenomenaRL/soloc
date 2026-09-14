<img src="../../assets/soloc-icon.svg" width="72" align="right" alt="soloc">

# soloc-ledger

**The append-only store at the heart of [soloc](../../).** A `Ledger` holds a stream of Arrow
`RecordBatch`es built on any schema that embeds a `spacetimestamp` struct column (see
[`spacetimestamp`](../spacetimestamp/)), and turns them into answers about *where everything is*.

## Core concepts

- **Store raw, forever.** Rows are appended in their original frame and native units and never
  rewritten. Reprojection to an astronomical frame is a query-time view, never re-stored.
- **Normalized on ingest.** `append` validates each batch, normalizes its timestamps to **TAI** timeframe
  (rows already on TAI pass through with no allocation), and folds its edges into a derived
  [`TransformTree`](../spacetimestamp/). A batch that would form a cycle or name an unresolvable
  parent frame is rejected whole.
- **Fast "where is X now."** A per-entity **pose cache** tracks each entity's highest-epoch pose, so
  the common query costs a handful of targeted reads rather than a full scan.
- **Sealing.** Once more than `SEGMENT_THRESHOLD` (50) batches accumulate, they are merged into one
  via `concat_batches` [Likely the site of future optimizations]
- **Persistence.** `save_ipc` / `load_ipc` round-trip the ledger through Arrow IPC (local paths or,
  from the server, object-store URLs), rebuilding all derived state on load. A schema-only IPC form
  lets you bake an empty schema into an image so a fresh server starts with the right shape.

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
use soloc_ledger::ephemeris::celestial_snapshot;

ledger.append(celestial_snapshot(&almanac, &ids, epoch)?)?;
```

## Roadmap

- [x] **Core capabilities** — append, spatiotemporal & current-state queries, IPC persistence
  (local + object-store), topology/name federation
- [ ] **Performance benchmarks** — a `criterion` suite to pin down append, query, and seal costs
- [ ] **Storage-organization optimizations** — smarter decisions about *how and when* the ledger
  segments and seals `RecordBatch`es on ingest, and how it organizes them across memory and storage
  on save/load

## License

Apache-2.0. See the [workspace README](../../) for the ecosystem overview.
