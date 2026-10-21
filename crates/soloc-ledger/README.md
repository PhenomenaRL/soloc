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

## GPX ingestion (optional)

Enable `soloc-ledger`'s `gpx` feature to use
`soloc_ledger::gpx::import_gpx_into_ledger`. It streams the GPX reader through explicit
track/route mappings and the shared GPS converter, building batches of at most the configured
row count. The [core GPX documentation](../spacetimestamp/README.md#gpx-tracks-and-routes-optional)
describes identity, timestamp, elevation, and orientation policies.

```rust
use soloc_ledger::gpx::import_gpx_into_ledger;
use spacetimestamp::gps::gpx::{GpxImportOptions, GpxPointErrorPolicy};

// `reader`, `mapper`, and `almanac` use the core GPX API. Keep the source file
// and consume observer events when segment boundaries or provenance matter.
let report = import_gpx_into_ledger(
    &mut ledger,
    reader,
    &mapper,
    &almanac,
    GpxImportOptions::new(1024, GpxPointErrorPolicy::Strict)?,
    |event| { /* stream metadata, mapping assumptions, and outcomes to a sidecar */ },
);
if !report.complete {
    eprintln!("Partial import: {report:#?}");
}
```

`Strict` stops at the first mapping failure. `SkipInvalid` records and skips those points,
continuing to subsequent points; malformed XML and other reader errors always stop. The report
counts complete points read, converted, confirmed submitted, skipped and failed, as well as
rejected batches' rows and converted rows left unsubmitted. A malformed point that the reader
could not emit is identified by the reader failure rather than counted as a complete point.
The report and observer preserve source/object/segment/point context. Observer mapping events
precede acceptance; only submission outcomes confirm that rows entered the ledger.

Each local append either accepts its whole batch or rejects it without changing the ledger.
Previously accepted batches remain after a later error. Pending converted points are left
unsubmitted on a parsing or strict mapping failure. There is no whole-file transaction,
automatic retry, or idempotency key: reimporting a file appends duplicate observations.
Retain the source, mapping configuration, report and observer output before deciding how to
recover a partial import. The report alone does not contain every point's provenance or policies.

**Submission is not retention.** This helper preserves the existing memory limit. A configured
rolling window can permanently evict the beginning of a journey while an import succeeds.
For complete-route history, explicitly use adequate capacity or `ledger.set_memory_limit(None)`
before importing. Removing a limit afterwards cannot recover evicted data. `current_state`
returns the latest pose per entity; query history when verifying the complete route. Persist
the ledger with `save_ipc` if it must outlive the process. Entity IPC does not include GPX segment
boundaries or source metadata; retain those separately and keep the original input.

The [minimal example](examples/import_gpx.rs) imports five synthetic Halifax observations across
two disconnected segments in batches of two, explicitly disables eviction, and checks that
all five observations remain queryable. It uses SIMULATED poses, explicitly ellipsoidal heights,
recorded UTC timestamp fields, LocalEnu orientation, and the approximate ITRF93 policy. No kernel
download is needed. The optional output is entity Arrow IPC plus its name-registry sidecar;
Earth/frame anchors and segment-aware visualization remain Step 6.

```sh
cargo run --locked --offline -p soloc-ledger --features gpx --example import_gpx
# Optional: persist the entity rows after verifying full history retention.
cargo run --locked --offline -p soloc-ledger --features gpx --example import_gpx -- /tmp/halifax.arrow
cargo test --locked --offline -p soloc-ledger --features gpx --test gpx_import
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
