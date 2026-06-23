# soloc & spacetimestamp — Usage Guide

This guide covers the two in-process crates: **`spacetimestamp`** (the core Arrow primitive)
and **`soloc`** (the ledger and entity schema built on top of it).
The `soloc-server` (Flight RPC) will be covered separately.

---

## Concepts

### SpaceTimestamp

A **SpaceTimestamp** is an Arrow `RecordBatch` (or nested `StructArray`) that records one pose
per row: a position `[x, y, z]`, an orientation quaternion `[w, x, y, z]`, and a timestamp, all anchored to a named reference frame and timescale.

It has 11 columns:

| Column | Arrow type | Nullable | Description |
|---|---|---|---|
| `frame_id` | `Dictionary(UInt32, Utf8)` | no | Reference frame of this row's position and orientation. Three accepted forms: astronomical names (`"ICRF"`, `"IAU_EARTH"`, …), namespaced local frames (`"uuid:cam"`), or entity URIs (`"demo:truck_A"`). |
| `units_pos` | `Dictionary(UInt16, Utf8)` | no | Unit of the `position` vector. Accepted: `"m"`, `"km"`, `"au"`. Used by `transform_batch` to normalize before applying ephemeris offsets. |
| `timescale_id` | `Dictionary(UInt32, Utf8)` | no | Timescale of the stored timestamp. Any timescale recognized by hifitime: `"TAI"`, `"UTC"`, `"TDB"`, `"GPS"`, etc. The ledger normalizes all rows to `"TAI"` on append. |
| `source_id` | `Dictionary(UInt32, Utf8)` | no | Who produced this measurement or estimate. Typically a sensor URI, instrument name, or algorithm identifier (e.g. `"naif:de440s"`, `"demo:lidar_1"`). |
| `estimate_type` | `Dictionary(UInt16, Utf8)` | no | Quality tag. `"MEASURED"` beats `"PREDICTED"` beats `"SIMULATED"` when `current_state` resolves ties at the same epoch. |
| `position` | `FixedSizeList(3, f64)` | no | Cartesian position `[x, y, z]` in the frame and unit given by `frame_id` and `units_pos`. |
| `quaternion` | `FixedSizeList(4, f64)` | no | Orientation as a unit quaternion `[w, x, y, z]` (scalar-first). Describes the rotation of the body relative to the reference frame. |
| `duration_centuries` | `i16` | no | Whole Julian centuries from the J2000 reference epoch in `timescale_id`. |
| `duration_ns` | `u64` | no | Nanoseconds within the century given by `duration_centuries`. Together these two fields encode a timestamp with sub-nanosecond headroom and no floating-point drift. |
| `position_covariance` | `FixedSizeList(6, f64)` | **yes** | Upper triangle of the 3×3 position covariance matrix, row-major: `[σ_xx, σ_xy, σ_xz, σ_yy, σ_yz, σ_zz]`. Same frame and unit as `position`. Null when unknown. |
| `orientation_covariance` | `FixedSizeList(6, f64)` | **yes** | Upper triangle of the 3×3 orientation covariance in the tangent space of SO(3) (axis-angle perturbation), row-major: `[σ_11, σ_12, σ_13, σ_22, σ_23, σ_33]`. Null when unknown. |

Low-cardinality string columns (`frame_id`, `timescale_id`, `source_id`, `estimate_type`,
`units_pos`) use Arrow dictionary encoding so repeated values cost only an integer index per row.

**Time encoding.** The J2000 TAI epoch is `2000-01-01T12:00:00 TAI`. Storing time as
`(duration_centuries: i16, duration_ns: u64)` avoids floating-point precision loss across
long mission durations (a single `f64` loses sub-millisecond resolution after ~300 years).

**Frame IDs.** Three kinds of frame identifier are accepted in `frame_id`:

- *Astronomical* — recognized by anise: `"ICRF"`, `"Earth"`, `"IAU_MARS"`, `"GCRF"`, etc.
  See `KNOWN_EXTERNAL_FRAMES` for the full static list.
- *Custom local* — registered in a `FrameRegistry`, stored as `"namespace:local_name"`
  (e.g. `"robot_1:cam"`).
- *Entity URI* — an entity's own pose as its frame anchor, e.g. `"demo:truck_A"`.
  Used when a sensor is rigidly attached to a moving entity tracked in the ledger.

### FrameRegistry

A `FrameRegistry` is a directed acyclic graph of static transforms (translation + rotation)
from custom local frames to an astronomical root. It is serialized as JSON and embedded
into the Arrow schema metadata under the key `"soloc.frame_registry"`.

### Entity

An **Entity** is a row in the `entity_schema`: it adds an `entity_id` URI and optional
kinematic fields (`velocity`, `angular_velocity`, `acceleration`, `mass_kg`,
`state_covariance`) around the embedded `spacetimestamp` struct column.

### Ledger

A **`Ledger`** is an append-only in-process store of `RecordBatch`es. It is schema-agnostic:
any Arrow schema that embeds a `"spacetimestamp"` struct column is accepted.

---

## 1. Building a SpaceTimestamp batch

```rust
use spacetimestamp::{SpaceTimestampBuilder, sts_schema};

let mut builder = SpaceTimestampBuilder::new(
    1024,   // capacity hint (rows)
    None,   // optional FrameRegistry — pass None for astronomical-only frames
);

// Append one row. Time: J2000 TAI + 0 ns (exactly J2000).
builder.append_spacetimestamp(
    "ICRF",       // frame_id
    "km",         // units_pos
    "TAI",        // timescale_id
    "sensor_1",   // source_id
    "MEASURED",   // estimate_type
    [149_598_023.0, 0.0, 0.0],  // position [x, y, z]
    [1.0, 0.0, 0.0, 0.0],       // quaternion [w, x, y, z] — identity
    0,            // duration_centuries
    0,            // duration_ns
    None,         // position_covariance (upper triangle [σ_xx,σ_xy,σ_xz,σ_yy,σ_yz,σ_zz])
    None,         // orientation_covariance
);

let batch = builder.flush(); // → RecordBatch
```

`flush()` returns a standalone `RecordBatch`. To embed inside a parent schema
(as `EntityBuilder` does internally), call `finish_as_struct()` instead — it returns
a `StructArray`.

---

## 2. Custom local frames (FrameRegistry)

Use a `FrameRegistry` when you have sensor or robot frames that are fixed offsets
from an astronomical root.

```rust
use spacetimestamp::schema::{FrameRegistry, SpaceTimestampBuilder};

let mut reg = FrameRegistry::new_with_uuid(); // random namespace UUID

// camera is at [0.1, 0, 0] m from the robot base, identity rotation
reg.add_frame("cam", "IAU_EARTH", [0.1, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);

// Multi-hop chain: lidar -> base_link -> IAU_MARS
reg.add_frame("base_link", "IAU_MARS", [0.0, 0.0, 0.5], [1.0, 0.0, 0.0, 0.0]);
reg.add_frame("lidar", "base_link", [0.2, 0.0, 0.1], [1.0, 0.0, 0.0, 0.0]);

reg.validate().unwrap(); // checks for cycles

let mut builder = SpaceTimestampBuilder::new(64, Some(reg));

// Use the local name — the builder qualifies it to "uuid:cam" automatically.
builder.append_spacetimestamp(
    "cam", "m", "TAI", "cam_driver", "MEASURED",
    [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, None, None,
);

let batch = builder.flush();
// The FrameRegistry JSON is embedded in batch.schema().metadata()
```

**`add_frame_validated`** is the stricter variant: it checks that the parent exists
in the registry (for local parents) or that anise can resolve it (for
`add_external_frame`-registered names).

---

## 3. Timestamp helpers

```rust
use spacetimestamp::{epoch_to_parts, epoch_from_parts, j2000_tai, j2000_in_timescale};
use hifitime::{Epoch, TimeScale};
use std::str::FromStr;

// Convert a hifitime Epoch → (centuries, ns) for storage
let epoch = Epoch::from_str("2025-03-15T10:00:00 TAI").unwrap();
let (centuries, ns) = epoch_to_parts(epoch);

// Reconstruct from storage
let recovered = epoch_from_parts(centuries, ns, TimeScale::TAI);
assert_eq!(recovered, epoch);

// J2000 reference epochs per timescale
let j2000_utc = j2000_in_timescale(TimeScale::UTC); // ~32 s after j2000_tai()
```

---

## 4. Validation

```rust
use spacetimestamp::validation::validate_spacetimestamp_batch;

// Works on both flat STS batches and nested entity batches.
validate_spacetimestamp_batch(&batch)?;
// Checks that all timescale_id values are known to hifitime and
// all frame_id values are known to anise, the FrameRegistry, or are entity URIs.
```

---

## 5. Frame transforms

`transform_batch` reprojects the `"spacetimestamp"` struct column into a new
astronomical frame. All other columns (velocity, entity_id, …) pass through unchanged.

```rust
use spacetimestamp::transforms::transform_batch;
use anise::prelude::Almanac; // Almanac::default() has no SPK; use MetaAlmanac::latest() for real data

let result = transform_batch(
    &batch,
    "IAU_EARTH",     // target frame
    &almanac,
    "km",            // output unit
    None,            // dynamic_frames — None when no entity-URI frame_ids are present
)?;
```

The three-stage pipeline per row:

1. **Static** — walk the `FrameRegistry` chain from the custom frame to its astronomical root.
2. **Rotate** — apply the `Almanac`'s DCM to re-orient axes into the target frame.
3. **Translate** — apply the `Almanac`'s origin shift into the target frame.

> **Covariance note**: `position_covariance` and `orientation_covariance` are set to null
> in the output. Covariance propagation (`C' = R·C·Rᵀ`) is not yet implemented.

### Timescale normalization

```rust
use spacetimestamp::transforms::normalize_batch_to_tai;

// Rewrites all rows so timescale_id = "TAI" and (centuries, ns) are J2000 TAI offsets.
// Rows already in TAI are a cheap pass-through (no allocation).
let normalized = normalize_batch_to_tai(&batch)?;
```

---

## 6. Spatiotemporal filtering

```rust
use spacetimestamp::query::{SpatiotemporalFilter, filter_batch};
use hifitime::Epoch;

let t_start = Epoch::from_str("2025-01-01T00:00:00 TAI").unwrap();
let t_end   = Epoch::from_str("2025-12-31T23:59:59 TAI").unwrap();

let filter = SpatiotemporalFilter::new()
    .with_time_range(t_start, t_end)
    .with_spatial([0.0, 0.0, 0.0], 1_000_000.0); // 1M km sphere

let filtered = filter_batch(&batch, &filter)?;
```

Spatial filters require all rows in the batch to share the same `frame_id`.
If they don't, `filter_batch` returns an error suggesting a call to `transform_batch` first.

---

## 7. Entity schema (soloc)

`EntitySchema` wraps a `spacetimestamp` struct and adds entity-specific fields:

| Column | Type | Notes |
|---|---|---|
| `entity_id` | `Dictionary(UInt32, Utf8)` | Federated URI, e.g. `"naif:399"` |
| `spacetimestamp` | Struct | Nested STS fields |
| `velocity` | `FixedSizeList(3, f64)` | Nullable, in same unit as `units_pos` |
| `angular_velocity` | `FixedSizeList(3, f64)` | Nullable, rad/s |
| `acceleration` | `FixedSizeList(3, f64)` | Nullable |
| `mass_kg` | `f64` | Nullable |
| `state_covariance` | `FixedSizeList(21, f64)` | Nullable, upper triangle of 6×6 `[x,y,z,vx,vy,vz]` matrix |

```rust
use soloc::schemas::entity::{EntityBuilder, entity_schema};

let schema = entity_schema(None); // pass Some(&reg) to embed a FrameRegistry

let mut builder = EntityBuilder::new(64, None);
builder.append_entity(
    "naif:399",          // entity_id
    "ICRF",              // frame_id
    "km",                // units_pos
    "TAI",               // timescale_id
    "naif:de440s",       // source_id
    "MEASURED",          // estimate_type
    [149_598_023.0, 0.0, 0.0], // position
    [1.0, 0.0, 0.0, 0.0],      // quaternion
    0, 0,                // (duration_centuries, duration_ns)
    Some([0.0, 29.8, 0.0]),    // velocity km/s
    None,                // angular_velocity
    None,                // acceleration
    Some(5.972e24),      // mass_kg
    None,                // state_covariance
);

let batch = builder.flush();
```

### Constructing a Ledger from the entity schema

```rust
use soloc::schemas::entity::EntitySchema;
use soloc::ledger::Ledger;

let mut ledger = Ledger::for_schema::<EntitySchema>(None)?;
```

---

## 8. The Ledger

### Appending data

```rust
ledger.append(batch)?;
// Validates STS fields, normalizes all timestamps to TAI, then stores.
```

Batches are merged automatically when the internal count exceeds 50 to keep
query latency bounded.

### Querying

```rust
// Returns a single concatenated RecordBatch of all matching rows.
let result = ledger.query(&SpatiotemporalFilter::new().with_time_range(t1, t2))?;

// Streaming variant — yields per-batch, no full-ledger allocation.
for maybe_batch in ledger.stream_query(&filter) {
    let batch = maybe_batch?;
    // process batch …
}

// Most recent complete snapshot (last appended batch).
let snap = ledger.latest_snapshot(None);                         // all entities
let snap = ledger.latest_snapshot(Some(&["demo:truck_A"]));      // specific entities

// Single best pose per entity across the entire ledger.
// "Best" = most recent timestamp; ties broken by MEASURED > PREDICTED > SIMULATED.
let state = ledger.current_state(None, None)?;                   // all entities, default window
let state = ledger.current_state(Some(&["demo:sat"]), Some(cutoff_epoch))?;
```

### In-process frame transform through the ledger

When a batch contains entity-URI `frame_id` values (e.g. a sensor whose pose is
stored relative to a moving vehicle), use `Ledger::transform` instead of calling
`transform_batch` directly — it resolves the URI chain automatically:

```rust
let result = ledger.transform(&batch, "ICRF", "km", &almanac)?;
```

### Persistence

```rust
// Save / load to a file
ledger.save_ipc(path)?;
let ledger = Ledger::load_ipc(path, "entity_id")?;

// In-memory bytes (e.g. for network transfer)
let bytes = ledger.save_ipc_to_bytes()?;
let ledger = Ledger::load_ipc_from_bytes(&bytes, "entity_id")?;

// Schema-only (no data — useful for distributing a schema file at startup)
ledger.save_schema_ipc(path)?;
let empty = Ledger::load_schema_ipc(path, "entity_id")?;
```

---

## 9. Ephemeris seeding

Requires loading DE440 data. On first run `MetaAlmanac::latest()` downloads
~150 MB to `~/.local/share/nyx-space/anise/`.

```rust
use anise::MetaAlmanac;
use soloc::ephemeris::{celestial_snapshot, CelestialBody};
use spacetimestamp::epoch_to_parts;
use hifitime::Epoch;

let almanac = MetaAlmanac::latest()?.process(None)?;
let epoch = Epoch::from_str("2025-06-01T00:00:00 TAI").unwrap();

// Seed a ledger with all inner planets + Moon
let batch = celestial_snapshot(&almanac, CelestialBody::ALL, epoch)?;
ledger.append(batch)?;

// Or use the convenience method on Ledger directly:
ledger.seed_solar_system(&almanac, CelestialBody::ALL, epoch)?;
```

`Almanac::default()` (no SPK loaded) is fine for tests that only exercise
static `FrameRegistry` transforms and don't call the ephemeris.

---

## 10. Implementing a custom schema

Any Arrow schema that contains a `"spacetimestamp"` struct column with the correct
sub-field types works with `Ledger`, `transform_batch`, `filter_batch`, and
`validate_spacetimestamp_batch`. Implement `SolocSchema` to use the typed
`Ledger::for_schema` constructor:

```rust
use soloc::schemas::SolocSchema;
use spacetimestamp::schema::FrameRegistry;
use arrow::datatypes::SchemaRef;

pub struct MySchema;

impl SolocSchema for MySchema {
    fn schema(registry: Option<&FrameRegistry>) -> SchemaRef {
        // Build and return your Arrow schema here.
        // Must include a "spacetimestamp" struct column.
        todo!()
    }
    fn id_column() -> &'static str {
        "my_id"
    }
}

let ledger = Ledger::for_schema::<MySchema>(None)?;
```

---

## 11. Performance

Numbers from Criterion benchmarks on this machine (Linux 6.6 / WSL2, release build).
All benchmarks are in `crates/spacetimestamp/benches/` and `crates/soloc/benches/`.

### Ingestion

| Operation | 1 k rows | 10 k rows | 100 k rows | Throughput |
|---|---|---|---|---|
| `SpaceTimestampBuilder` + `flush` | 181 µs | 1.7 ms | 23 ms | **~4.5 M rows/s** |
| `EntityBuilder` + `flush` | 404 µs | 3.9 ms | 46 ms | **~2.5 M rows/s** |

EntityBuilder is roughly half the speed of SpaceTimestampBuilder because it builds seven
columns instead of eleven (velocity, angular_velocity, acceleration, mass_kg, and
state_covariance all add allocation work).

### Filter (`filter_batch`)

| Filter type | 1 k rows | 10 k rows | 100 k rows | Throughput |
|---|---|---|---|---|
| Time range | 321 µs | 2.9 ms | 28 ms | **~3.5 M rows/s** |
| Spatial radius | 35 µs | 255 µs | 2.5 ms | **~40 M rows/s** |
| Time + spatial | 349 µs | 3.1 ms | 30 ms | **~3.3 M rows/s** |

Time filtering is ~10× slower than spatial filtering because it reconstructs a hifitime
`Epoch` from `(duration_centuries, duration_ns, timescale_id)` for every row. Spatial
filtering only does a squared-distance comparison per row.

### Frame transform (`transform_batch`, static FrameRegistry chain)

| Rows | Time | Throughput |
|---|---|---|
| 100 | 90 µs | |
| 1 000 | 771 µs | |
| 10 000 | 7.2 ms | **~1.4 M rows/s** |

This measures a two-hop static `FrameRegistry` chain (`arm → base_link → Earth`) with
`Almanac::default()` (no SPK loaded). When source and target share the same astronomical
body the almanac returns identity transforms. **Full cross-body transforms** (e.g. ICRF →
IAU_MARS requiring DE440) add a `translate` and `rotate` almanac call per row; that cost
depends on SPK complexity and is not benchmarked here.

### Timescale normalization (`normalize_batch_to_tai`, mixed TAI/UTC input)

| Rows | Time | Throughput |
|---|---|---|
| 1 000 | 371 µs | |
| 10 000 | 3.6 ms | |
| 100 000 | 34 ms | **~2.9 M rows/s** |

The ledger calls `normalize_batch_to_tai` on every `append`. When all rows are already TAI
(the common case in production), this is a zero-copy `batch.clone()` — effectively free.

### Validation (`validate_spacetimestamp_batch`)

100 k rows: **< 1 µs** — validates the dictionary of unique values, not individual rows.
Cost is independent of row count.

### Ledger append overhead

The table below measures `Ledger::append` on a pre-built 1 000-row batch (ingestion
cost is already paid). The jump at 1 000 batches reflects the automatic segment-compaction
that fires every 50 appends.

| Batches (× 1 000 rows) | Total time | Per-append |
|---|---|---|
| 10 (10 k rows) | 20 µs | **~2 µs** |
| 100 (100 k rows) | 8 ms | ~80 µs avg (includes 1 compaction merge) |
| 1 000 (1 M rows) | ~840 ms | ~840 µs avg (includes ~19 compaction merges) |

For steady-state simulation where batches arrive continuously, the ~2 µs per-append
overhead is negligible. Compaction merges are off the hot path — they fire at most
once every 50 appends.

### Ledger query (10 k total rows)

| Query type | 10 batches × 1 k rows | 100 batches × 100 rows | 1 000 batches × 10 rows |
|---|---|---|---|
| Time filter | 2.6 ms | 3.4 ms | 3.4 ms |
| Spatial filter | 706 µs | 2.1 ms | 2.2 ms |

Query time scales with batch count as well as row count because each batch carries a
~11 µs fixed dispatch overhead. Keeping the ledger in a small number of large batches is
more efficient than many small ones — segment-compaction handles this automatically.

`latest_snapshot` is O(1) — **~110 ns** regardless of ledger size.

### IPC persistence (10 k rows)

| Operation | Time |
|---|---|
| `save_ipc` | 5.7 ms |
| `load_ipc` | 458 µs |

### Practical scale guidance

| Scenario | Rows/s needed | Headroom |
|---|---|---|
| 1 000 entities at 10 Hz | 10 k rows/s | 250× ingestion budget |
| 10 000 entities at 10 Hz | 100 k rows/s | 25× ingestion budget |
| 100 000 entities at 1 Hz | 100 k rows/s | 25× ingestion budget |
| Time-range query over 100 k rows | — | ~28 ms per query |
| Spatial query over 100 k rows | — | ~2.5 ms per query |

**Memory.** A SpaceTimestamp row occupies roughly 80–90 bytes of Arrow data (without
covariance). An entity row (with velocity and mass, no state covariance) is roughly
160 bytes. At those densities, 1 M entity rows ≈ 160 MB in-process.

---

## 12. Example: Multi-Entity Frame Graph

**Scenario.** A Mars surface mission with a relay orbiter and a ground control antenna.
Four entity types are tracked simultaneously, each using a different kind of frame anchor.

```
ICRF  (Solar System Barycentre — inertial root)
│
├── naif:499  "Mars"               frame_id = "ICRF"         DE440 ephemeris
│   │
│   └── IAU_MARS  (body-fixed)
│       │
│       ├── demo:rover             frame_id = "IAU_MARS"     on-surface position
│       │   │
│       │   └── demo:sample_cache  frame_id = "demo:rover"   ← entity URI
│       │                          position = [3.0, 0.0, 0.0] m from rover
│       │
│       └── demo:lander            frame_id = "IAU_MARS"     static surface asset
│
├── demo:relay_orbiter             frame_id = "ICRF"         orbital telemetry
│
└── IAU_EARTH  (body-fixed)
    │
    └── [FrameRegistry]  ← static antenna positions baked into schema metadata
        │
        ├── uuid:dss14             parent = "IAU_EARTH"      Goldstone dish
        │   offset [−2465.7, −4702.0, 3553.9] km
        │
        └── uuid:dss43             parent = "IAU_EARTH"      Canberra dish
            offset [−4460.9,  2682.2, 3674.4] km
```

**Three frame anchor types, one ledger.**

| Entity | `frame_id` value | Resolved by |
|---|---|---|
| `naif:499`, `demo:relay_orbiter`, `demo:lander`, `demo:rover` | Astronomical (`"ICRF"`, `"IAU_MARS"`) | `anise::Almanac` |
| `demo:sample_cache` | Entity URI (`"demo:rover"`) | `Ledger::build_dynamic_frame_map` |
| Antenna measurements | Namespaced local (`"uuid:dss14"`) | `FrameRegistry` in schema metadata |

**Setting it up in code.**

```rust
use soloc::{schemas::entity::{EntityBuilder, EntitySchema}, ledger::Ledger};
use spacetimestamp::schema::FrameRegistry;

// 1. Build a FrameRegistry for the two ground antennas.
//    These positions never change — embed them in the schema once.
let mut reg = FrameRegistry::new_with_namespace("dsn");
reg.add_frame(
    "dss14", "IAU_EARTH",
    [-2_465.7, -4_702.0, 3_553.9],  // km from Earth centre
    [1.0, 0.0, 0.0, 0.0],
);
reg.add_frame(
    "dss43", "IAU_EARTH",
    [-4_460.9, 2_682.2, 3_674.4],
    [1.0, 0.0, 0.0, 0.0],
);

// 2. Create the ledger with the registry baked into its schema metadata.
let mut ledger = Ledger::for_schema::<EntitySchema>(Some(&reg))?;

// 3. Append Mars and the relay orbiter (absolute ICRF positions from ephemeris/telemetry).
let mut b = EntityBuilder::new(4, None);
b.append_entity("naif:499",           "ICRF",     "km", "TAI", "naif:de440s", "MEASURED",
    [2.28e8, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, None, None, None, Some(6.417e23), None);
b.append_entity("demo:relay_orbiter", "ICRF",     "km", "TAI", "demo:obc",    "MEASURED",
    [2.28e8, 400.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, Some([0.0, 3.4, 0.0]), None, None, None, None);
ledger.append(b.flush())?;

// 4. Append surface entities in IAU_MARS (Mars body-fixed frame).
let mut b = EntityBuilder::new(2, None);
b.append_entity("demo:rover",  "IAU_MARS", "m", "TAI", "demo:obc", "MEASURED",
    [320.5, 14.2, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, Some([0.5, 0.0, 0.0]), None, None, None, None);
b.append_entity("demo:lander", "IAU_MARS", "m", "TAI", "demo:obc", "MEASURED",
    [300.0, 10.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, None, None, None, None, None);
ledger.append(b.flush())?;

// 5. Append a dropped sample cache expressed relative to the rover (entity URI as frame_id).
//    position [3, 0, 0] means "3 m along rover's +X axis from rover's current position".
let mut b = EntityBuilder::new(1, None);
b.append_entity("demo:sample_cache", "demo:rover", "m", "TAI", "demo:obc", "MEASURED",
    [3.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, None, None, None, None, None);
ledger.append(b.flush())?;

// 6. Append a DSN antenna observation using the local FrameRegistry frame.
//    The builder auto-qualifies "dss14" → "dsn:dss14".
let mut b = EntityBuilder::new(1, Some(reg));
b.append_entity("demo:dss14_obs", "dss14", "km", "TAI", "dsn:dss14", "MEASURED",
    [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, None, None, None, None, None);
ledger.append(b.flush())?;

// 7. Transform everything into ICRF/km for a unified view.
//    Ledger::transform resolves the entity URI chain for demo:sample_cache automatically.
let state = ledger.current_state(None, None)?;
let unified = ledger.transform(&state, "ICRF", "km", &almanac)?;
```

After step 7, every row in `unified` has `frame_id = "ICRF"` and `units_pos = "km"`,
regardless of whether it started as an ephemeris body, a surface asset in IAU_MARS,
a rover-relative dropped sample, or a fixed antenna in a FrameRegistry.
`SpatiotemporalFilter::with_spatial` can then be applied safely across all rows.
