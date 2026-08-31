# soloc

**A federated, physically-grounded ledger for tracking any entity in space-time.**

## What is soloc?

soloc is a tool for recording, transforming, and querying spatiotemporal observations, spacecraft, ground stations, underwater vehicles, sensor readings or any data-generating entity, using [Apache Arrow](https://arrow.apache.org/) as the storage primitive.

Each observation is stored in its **original reference frame and native units, forever**. Reprojection to any astronomical frame (ICRF, GCRF, body-fixed) happens at query time via a physics engine backed by [NAIF SPICE](https://naif.jpl.nasa.gov/naif/toolkit.html) ephemeris data. This helps maintain numerical precision when reporting anywhere in the solar system. 

## Key Design Principles

- **Store raw, reproject on demand.** The ledger is immutable truth. Transforms are a view-layer operation, never re-stored.
- **Arrow-native throughout.** Every observation is an Arrow `RecordBatch`. Zero-copy interop with Python (`pyarrow`), Julia, and the rest of the Arrow ecosystem comes for free.
- **Federated by design.** Each operator runs their own `soloc-server` instance. Federation happens by exchanging Arrow Flight streams, no shared cluster, no central authority.

## Workspace

| Crate | Description |
|---|---|
| [`spacetimestamp`](crates/spacetimestamp/) | Arrow schemas and builders, row-derived transform tree, physics transforms, spatiotemporal filters |
| [`soloc`](crates/soloc/) | Append-only ledger: persistence, pose cache, federation |
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

**Federation actions:** `export_topology` emits the transform-tree event log as Arrow IPC and `import_topology` merges a peer's back in, rejecting anything that would form a cycle. `export_names` and `import_names` do the same for the display-name registry, re-deriving every claimed binding so a peer cannot assert that some id "is" a name it does not hash to. All four are advertised via `list_actions`, alongside `save_ledger`, `load_ledger`, `load_kernel`, and `append_snapshot`.

## Identity

The three identity columns: 
- `entity_id`
- `frame_id`
- `source_id` 

Each are `FixedSizeBinary(16)` carrying the `arrow.uuid` extension name. An id is a **pure function** of its inputs: every writer in every process derives the same 16 bytes with no shared state and no lookup.

There are three **kinds**, in the low nibble of byte 6: `0x1` **soloc** (pose comes from ledger rows), `0x2` **abstract** (provenance only, never valid as a frame), and `0x0` **astronomical** (a terminal frame the anise almanac resolves). Soloc and abstract ids hash a name; astronomical ids embed a frame pair.

**Soloc and abstract** ids hash `(kind, authority, common_name)`:

```
id = SHA-256(NAMESPACE ‖ kind ‖ lowercase(authority) ‖ 0x00 ‖ name)[:16]
     with byte 6 = 0x80 | kind   (UUID version 8, kind in the low nibble)
     and  byte 8 = (byte 8 & 0x3F) | 0x80   (RFC 9562 variant)
```

Common names are free-form; the authority is a separate input.

**Astronomical** ids embed anise's `(ephemeris_id, orientation_id)` integers directly, instead of hashing:

```
bytes[0..4]  = ephemeris_id    (i32, big-endian)
byte 6       = 0x80            (version 8 | KIND_ASTRO 0x0)
byte 8       = 0x80            (RFC 9562 variant)
bytes[9..13] = orientation_id  (i32, big-endian)
bytes 4, 5, 7, 13, 14, 15 = 0x00
```

This makes an astro id **self-describing**: it resolves to a frame with no name registry, and a client reads `(ephemeris_id, orientation_id)` straight back out. eg.:
- `Earth` = `IAU_EARTH` = `(399, 399)`
- Earth-centred inertial is `EME2000` = `GCRF` = `(399, 1)`
- the reference frames `ICRF` = `J2000` = `SSB` = `(0, 1)`. 
 
 
The full name↔pair table is `ASTRO_FRAMES` in `crates/spacetimestamp/src/ephemeris.rs`. 

Names are **display only**. A ledger with no registry is fully functional.

```python
import hashlib

NAMESPACE = bytes.fromhex("ee8c42090dd59d8d14afc9541cafe95d")
KIND_ASTRO, KIND_SOLOC, KIND_ABSTRACT = 0x0, 0x1, 0x2

def mint(kind: int, authority: str, name: str) -> bytes:
    """Soloc and abstract ids: a hash of the name."""
    assert authority and name and "\0" not in authority and "\0" not in name
    h = hashlib.sha256(
        NAMESPACE + bytes([kind & 0x0F]) + authority.lower().encode() + b"\0" + name.encode()
    ).digest()
    b = bytearray(h[:16])
    b[6] = 0x80 | (kind & 0x0F)
    b[8] = (b[8] & 0x3F) | 0x80
    return bytes(b)

def astronomical(ephemeris_id: int, orientation_id: int) -> bytes:
    """Astronomical ids: the anise frame pair, embedded (not hashed)."""
    b = bytearray(16)
    b[0:4]  = ephemeris_id.to_bytes(4, "big", signed=True)
    b[6]    = 0x80          # version 8 | KIND_ASTRO
    b[8]    = 0x80          # RFC 9562 variant
    b[9:13] = orientation_id.to_bytes(4, "big", signed=True)
    return bytes(b)

truck = mint(KIND_SOLOC, "acme.com", "radar_boresight")
earth = astronomical(399, 399)   # Earth == IAU_EARTH
icrf  = astronomical(0, 1)
```

`crates/spacetimestamp/src/identity.rs` carries frozen test vectors for both algorithms; check any reimplementation against them byte for byte.

## Vocabularies

`units_pos`, `timescale_id` and `estimate_type` are `UInt8` codes. Each column carries its own decode table in Arrow field metadata, so a client reads the table:

```python
names = field.metadata[b"ARROW:extension:metadata"].decode().split(",")
names[code]   # index 0 is "-", the reserved slot
```

| column | `ARROW:extension:name` | codes |
|---|---|---|
| `units_pos` | `soloc.length_unit` | `km`=1 `m`=2 `cm`=3 `mm`=4 `au`=5 `in`=6 `ft`=7 `mi`=8 `nmi`=9 |
| `timescale_id` | `soloc.timescale` | `TAI`=1 `TT`=2 `ET`=3 `TDB`=4 `UTC`=5 `GPST`=6 `GST`=7 `BDT`=8 `QZSST`=9 `TCG`=10 `TCB`=11 `TL`=12 `TCL`=13 |
| `estimate_type` | `soloc.estimate_type` | `MEASURED`=1 `ESTIMATED`=2 `SIMULATED`=3 |

Two rules: 1. **code `0` is reserved and never valid in data** 2. **codes are append-only**. 

`units_pos` governs `position`, `position_covariance`, and the position block of `state_covariance`. Every other quantity is fixed SI: `velocity` m/s, `acceleration` m/s², `angular_velocity` rad/s, `dimensions` m, `mass_kg` kg. `state_covariance` is therefore mixed: position block in `units_pos`², velocity block in (m/s)², cross terms in `units_pos`·m/s.

### Python client

```python
import pyarrow.flight as fl
import json

client = fl.FlightClient("grpc://localhost:50051")

# There is no separate frame-registration step. A custom frame exists as soon as a row
# declares it: append a row whose entity_id is mint(KIND_SOLOC, "acme.com", "radar_boresight")
# and whose frame_id is astronomical(399, 399) (Earth / IAU_EARTH), and the server derives the
# parent edge from the data itself. Re-parenting is just another append. Cycles and
# unreachable parent frames are rejected at append time.

# Push observations
writer, _ = client.do_put(fl.FlightDescriptor.for_path(["obs"]), batch.schema)
writer.write_batch(batch)
writer.close()

# Query with a time filter
ticket = fl.Ticket(json.dumps({"time_range_tai_s": [1_000_000.0, 2_000_000.0]}).encode())
table = client.do_get(ticket).read_all()

# "Where is everything now", restricted to specific entities. Ids may be given as the
# (authority, name) they mint from, as an astronomical (ephemeris_id, orientation_id) pair,
# or as raw hex 
ticket = fl.Ticket(json.dumps({
    "query_type": "current_state",
    "entity_ids": [
        {"authority": "acme.com", "name": "radar_boresight"},
        {"ephemeris_id": 399, "orientation_id": 399},
        {"id": truck.hex()},
    ],
}).encode())
table = client.do_get(ticket).read_all()

# Transform to a different frame for visualization
writer, reader = client.do_exchange(
    # target_units is a soloc.length_unit code: 1 = km. Unknown codes are rejected.
    fl.FlightDescriptor.for_command(json.dumps({"target_frame": "GCRF", "target_units": 1}).encode())
)
writer.write_batch(batch)
writer.done_writing()
transformed = next(reader).data
```
