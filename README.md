# soloc

**A federated, physically-grounded ledger for tracking any entity in space-time.**

## What is soloc?

soloc is a Rust workspace for recording, transforming, and querying spatiotemporal observations, spacecraft, ground stations, underwater vehicles, sensor readings or any data-generating entity, using [Apache Arrow](https://arrow.apache.org/) as the storage primitive.

Each observation is stored in its **original reference frame and native units, forever**. Reprojection to any astronomical frame (ICRF, GCRF, body-fixed) happens at query time via a physics engine backed by [NAIF SPICE](https://naif.jpl.nasa.gov/naif/toolkit.html) ephemeris data. This matters: a docking measurement recorded near Neptune in millimetres cannot survive normalization to heliocentric kilometres without catastrophic precision loss.

## Key Design Principles

- **Store raw, reproject on demand.** The ledger is immutable truth. Transforms are a view-layer operation, never re-stored.
- **Arrow-native throughout.** Every observation is an Arrow `RecordBatch`. Zero-copy interop with Python (`pyarrow`), Julia, and the rest of the Arrow ecosystem comes for free.
- **Federated by design.** Each operator runs their own `soloc-server` instance. Federation happens by exchanging Arrow Flight streams, no shared cluster, no central authority.

## Workspace

| Crate | Description |
|---|---|
| [`spacetimestamp`](crates/spacetimestamp/) | Core Arrow schema, row-derived transform tree, and physics transforms |
| [`soloc`](crates/soloc/) | custom schemas, append-only ledger |
| [`soloc-server`](crates/soloc-server/) | Arrow Flight gRPC server |

## Quick Start

**Prerequisites:** Rust stable ≥ 1.85.

```bash
# Build
cargo build -p spacetimestamp -p soloc -p soloc-server

# Test
cargo test -p spacetimestamp -p soloc -p soloc-server
```

### Running the server

The server takes no positional arguments. It reads a TOML config from `$SOLOC_CONFIG`, falling back to `./config.toml`; with neither present it starts fully in memory with sensible defaults.

```bash
# Zero-config: in-memory ledger, entity schema, bound to 0.0.0.0:50051
cargo run --release -p soloc-server

# With NAIF ephemeris kernels mounted (no network required at runtime)
SOLOC_KERNEL_PATHS=/path/to/de440s.bsp:/path/to/pck11.pca \
  cargo run --release -p soloc-server
```

```toml
# config.toml
[server]
bind = "0.0.0.0:50051"

[storage]
# ledger_url takes priority over ledger_path when both are set.
# s3://, gs://, az:// and file:// are all supported.
ledger_path = "/var/data/ledger.arrows"
# Optional zero-row Arrow IPC file defining the schema for a fresh ledger.
# Defaults to the standard entity schema.
# schema_path = "/var/data/schema.arrow"
id_column = "entity_id"

[ephemeris]
# Pre-mounted kernels; takes priority over SOLOC_KERNEL_PATHS.
kernels = ["/path/to/de440s.bsp", "/path/to/pck11.pca"]
```

Kernels resolve in order: the `kernels` list, then `SOLOC_KERNEL_PATHS` (colon-separated), then `MetaAlmanac::latest()`, which downloads DE440s + PCK files and caches them locally (~150 MB on first run). With no kernels at all the server still starts, but astronomical transforms will fail.

**Federation actions:** `export_topology` emits the transform-tree event log as Arrow IPC; `import_topology` merges a peer's back in, rejecting anything that would form a cycle. Both are advertised via `list_actions`, alongside `save_ledger`, `load_ledger`, `load_kernel`, and `append_snapshot`.

### Python client

```python
import pyarrow.flight as fl
import json

client = fl.FlightClient("grpc://localhost:50051")

# There is no separate frame-registration step. A custom frame exists as soon as a row
# declares it: append a row with entity_id "acme.com:radar_boresight" and frame_id
# "IAU_EARTH", and the server derives the parent edge from the data itself. Re-parenting
# is just another append. Cycles and unreachable parent frames are rejected at append time.

# Push observations
writer, _ = client.do_put(fl.FlightDescriptor.for_path(["obs"]), batch.schema)
writer.write_batch(batch)
writer.close()

# Query with a time filter
ticket = fl.Ticket(json.dumps({"time_range_tai_s": [1_000_000.0, 2_000_000.0]}).encode())
table = client.do_get(ticket).read_all()

# Transform to a different frame for visualization
writer, reader = client.do_exchange(
    fl.FlightDescriptor.for_command(json.dumps({"target_frame": "GCRF", "target_units": "km"}).encode())
)
writer.write_batch(batch)
writer.done_writing()
transformed = next(reader).data
```
