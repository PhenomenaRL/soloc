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
| [`spacetimestamp`](crates/spacetimestamp/) | Core Arrow schema, `FrameRegistry`, and physics transforms |
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

```bash
# With NAIF ephemeris kernels mounted (no network required at runtime)
SOLOC_KERNEL_PATHS=/path/to/de440s.bsp:/path/to/pck11.pca \
  cargo run --release -p soloc-server -- /path/to/registry.json /path/to/ledger.arrow
```

If `SOLOC_KERNEL_PATHS` is not set, the server falls back to downloading kernels via `MetaAlmanac::latest()` and caching them locally (~150 MB on first run). Custom frame registration and static transforms work without any kernels loaded.

Arguments:
- `argv[1]` — path for the persisted frame registry JSON (reloaded on restart)
- `argv[2]` — path for the Arrow IPC ledger file (reloaded on restart, saved on SIGTERM)

### Python client

```python
import pyarrow.flight as fl
import json

client = fl.FlightClient("grpc://localhost:50051")

# Register a custom sensor frame (persisted server-side)
for _ in client.do_action(fl.Action(
    "register_frame",
    json.dumps({
        "local_name": "radar_boresight",
        "parent": "IAU_EARTH",
        "translation": [0.0, 0.0, 42.0],   # 42 m above ground
        "rotation_quat": [1.0, 0.0, 0.0, 0.0],
    }).encode(),
)):
    pass

# Push observations (batch schema embeds your local FrameRegistry)
writer, _ = client.do_put(fl.FlightDescriptor.for_path(["obs"]), batch.schema)
writer.write_batch(batch)
writer.close()

# Query with a time filter — returned batches carry the canonical frame map
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
