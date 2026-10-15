<img src="../../assets/soloc-icon.svg" width="72" align="right" alt="soloc">

# spacetimestamp

**The core of the [soloc](../../) ecosystem.** This crate defines the Arrow
`spacetimestamp` schema. One row = one measurement of *where something is, in what frame, at what
instant*. The core includes the pipeline that validates, relates, reprojects, and filters those rows 
as follows:

```text
builder → validate → topology → transform → filter
```

## The schema

`sts_schema()` returns the fixed 11-field `spacetimestamp` struct. Every field is required except
the two covariance columns.

| Field | Arrow type | Purpose |
|---|---|---|
| `frame_id` | `FixedSizeBinary(16)` · `arrow.uuid` | id of the reference frame the pose is expressed in |
| `units_pos` | `UInt8` · `soloc.length_unit` | length-unit code for `position` |
| `timescale_id` | `UInt8` · `soloc.timescale` | time scale the epoch is measured on |
| `source_id` | `FixedSizeBinary(16)` · `arrow.uuid` | id of the observer/process that produced the row |
| `estimate_type` | `UInt8` · `soloc.estimate_type` | how the row was arrived at |
| `position` | `FixedSizeList(3, f64)` | `[x, y, z]` translation |
| `quaternion` | `FixedSizeList(4, f64)` | `[w, x, y, z]` orientation |
| `duration_centuries` | `Int16` | whole-centuries half of the epoch offset from the tagged timescale's J2000 reference |
| `duration_ns` | `UInt64` | sub-century nanosecond remainder of that offset |
| `position_covariance` | `FixedSizeList(6, f64)` *(nullable)* | upper triangle of the 3×3 position covariance |
| `orientation_covariance` | `FixedSizeList(6, f64)` *(nullable)* | upper triangle of the 3×3 orientation covariance (axis-angle) |

Epochs are two-part offsets from `2000-01-01T12:00:00` in the declared `timescale_id`:
whole centuries in `duration_centuries`, the remainder in `duration_ns`. New normalized
rows use **J2000 TAI**, so both parts represent continuous elapsed time. Legacy UTC rows
advance the UTC clock and omit inserted leap seconds; normalization applies the UTC
offset at the observation date. Rows are built with `SpaceTimestampBuilder` and read
back with `StsColumns`, which accepts both nested (struct-column) and flat layouts.

### The reference `entity` schema

`schemas::entity` is the first-party schema: it embeds the whole `spacetimestamp` struct as a
column named `spacetimestamp` and adds the physical state of a tracked entity.

| Field | Arrow type | |
|---|---|---|
| `entity_id` | `FixedSizeBinary(16)` · `arrow.uuid` | the **target** being described (the pose's `source_id` is the **observer**) |
| `spacetimestamp` | `Struct(<11 fields above>)` | the pose |
| `velocity` / `angular_velocity` / `acceleration` | `FixedSizeList(3, f64)` *(nullable)* | m/s · rad/s · m/s² |
| `mass_kg` | `Float64` *(nullable)* | physical mass |
| `state_covariance` | `FixedSizeList(21, f64)` *(nullable)* | upper triangle of the 6×6 covariance over `[x,y,z,vx,vy,vz]` |
| `dimensions` | `FixedSizeList(3, f64)` *(nullable)* | bounding-box extents in metres |

## Extending the schema

Define your own top-level schema by implementing the `SpaceTimestampSchema` trait. The only rules
are that it must embed a `spacetimestamp` struct column and define an id_column that works with the Identity system (see below).

```rust
use spacetimestamp::schemas::SpaceTimestampSchema;

impl SpaceTimestampSchema for MySchema {
    fn schema() -> arrow::datatypes::SchemaRef { /* embed sts_schema()'s fields + your own */ }
    fn id_column() -> &'static str { "entity_id" }   // "" if there is no id column
}
```

The whole topology/ledger machinery is generic over any implementor
(`Ledger::for_schema::<MySchema>()`), and `validate_sts_schema` checks the embedded struct without
constraining your additions.

### Vocabularies (self-describing codes)

`units_pos`, `timescale_id`, and `estimate_type` are `UInt8` codes. Each column carries its own
decode table in Arrow field metadata, so a foreign reader never needs a hard-coded table:

```python
names = field.metadata[b"ARROW:extension:metadata"].decode().split(",")
names[code]   # index 0 is "-", the reserved slot
```

| Column | `ARROW:extension:name` | Codes |
|---|---|---|
| `units_pos` | `soloc.length_unit` | `km`=1 `m`=2 `cm`=3 `mm`=4 `au`=5 `in`=6 `ft`=7 `mi`=8 `nmi`=9 |
| `timescale_id` | `soloc.timescale` | `TAI`=1 `TT`=2 `ET`=3 `TDB`=4 `UTC`=5 `GPST`=6 `GST`=7 `BDT`=8 `QZSST`=9 `TCG`=10 `TCB`=11 `TL`=12 `TCL`=13 |
| `estimate_type` | `soloc.estimate_type` | `MEASURED`=1 `ESTIMATED`=2 `SIMULATED`=3 |

Two rules:
1. **code `0` is reserved and never valid in data** 
2. **codes are append-only**. 

`units_pos` governs `position`, `position_covariance`, and the position block of `state_covariance`; 
every other quantity is fixed SI (`velocity` m/s, `acceleration` m/s², `angular_velocity` rad/s, 
`dimensions` m, `mass_kg` kg).

## Identity

Every id is a `PrescribedId`: 16 bytes, stored as a plain `FixedSizeBinary(16)` carrying the
`arrow.uuid` extension, and a valid RFC 9562 **UUIDv8**. An id is a **pure function** of its
inputs: every writer in every process derives the same bytes with no shared state and no lookup.

```rust
use spacetimestamp::identity::PrescribedId;

// Soloc id: hashes (authority, common_name). Its pose comes from ledger rows.
let robot  = PrescribedId::new("acme.com", "radar_boresight")?;

// Astronomical id: embeds anise's (ephemeris_id, orientation_id) pair directly — not a hash.
let earth  = PrescribedId::astronomical(399, 399)?;   // Earth == IAU_EARTH
let itrf93 = PrescribedId::astronomical(399, 3000)?;  // ITRF93, distinct Earth-fixed axes
let icrf   = PrescribedId::astronomical(0, 1)?;        // ICRF == J2000 == SSB
let (e, o) = earth.astro_frame().unwrap();             // (399, 399) — read straight back out

// Abstract id: provenance only. Valid as a source_id, rejected as a frame or entity.
let filter = PrescribedId::abstract_source("acme.com", "kalman_v3")?;
```

There are three **kinds**, encoded in the low nibble of byte 6:

| Kind | Nibble | Meaning |
|---|---|---|
| **astronomical** | `0x0` | a terminal frame the anise almanac resolves; chain resolution stops here |
| **soloc** | `0x1` | pose comes from ledger rows; chain resolution recurses into it |
| **abstract** | `0x2` | used only in 'source_id' field; never valid as a frame or entity |

**Soloc and abstract** ids hash `(kind, authority, common_name)`; **astronomical** ids embed the
frame pair, which makes them self-describing. They resolve to an anise `Frame` (by extention a NAIF pair 
defined reference frame) with no registry, and a client reads `(ephemeris_id, orientation_id)` straight back:

```text
soloc / abstract:                          astronomical:
id = SHA-256(NAMESPACE ‖ kind ‖            bytes[0..4]  = ephemeris_id    (i32, big-endian)
      lower(authority) ‖ 0x00 ‖ name)[:16] byte 6       = 0x80            (v8 | KIND_ASTRO 0x0)
  byte 6 = 0x80 | kind   (v8, kind nibble) byte 8       = 0x80            (RFC 9562 variant)
  byte 8 = (byte 8 & 0x3F) | 0x80          bytes[9..13] = orientation_id  (i32, big-endian)
```

The full name↔pair table (`ASTRO_FRAMES`, e.g. `ICRF (0,1)`, `Earth/IAU_EARTH (399,399)`,
`EME2000/GCRF (399,1)`) lives in `src/ephemeris.rs`. Because the algorithm is a pure hash, it is
portable. a Python reference (`mint` / `astronomical`) and byte-exact **frozen test
vectors** ship in `src/identity.rs`; check any reimplementation against them.

Names are **display only**: `NameRegistry` maps an id to `(authority, common_name)` for
presentation; identity, topology, and pose resolution never consult it, so a ledger with no
registry is fully functional.

## Topology

Topology can be thought of as a forest of transformation trees, where the root is an astronomical 
reference frame defined under NAIF/SPICE. Crucially, it is **derived, not declared.** Every 'row' in a 
RecordBatch already says "entity X is at this pose relative to `frame_id` as of this epoch," so 
`TransformTree` infers the parent graph from `(id, frame_id, epoch)` triples alone. 
It never reads `position`/`quaternion` or any name.

```mermaid
flowchart LR
  robot["robot · soloc"] --> truck["truck · soloc"]
  truck --> earth["Earth · astronomical (terminal)"]
```

## Federation

At this crate's level, federation can be achieved by exchanging the two *derived* structures. The topology log
and the name registry. Both are **idempotent, all-or-nothing, and replayable**:

- **Topology** — `TransformTree::to_log_batch` / `merge_log_batch` (the full event log, so a peer
  can replay history, not just the endpoint).
- **Names** — `NameRegistry::to_batch` / `merge_batch` (each row re-verified against the mint, so a
  peer cannot assert a binding an id does not hash to).

The pose *data* itself, `current_state`, spatiotemporal queries, and `entity_id` subsetting, 
federates one level up in [`soloc-ledger`](../soloc-ledger/). A peer that receives an exported log
gets **structure only**; poses stay with their owner.

## ITRF93 Earth frame

`PrescribedId::astronomical_from_name("ITRF93")` resolves to `(399, 3000)` and UUID
`0000018f-0000-8000-8000-000bb8000000`. `Earth` and `IAU_EARTH` retain `(399, 399)`.
Use ANISE's orientation ID `3000`, not the SPICE frame code `13000`.

Already-correct Cartesian ITRF93 observations can be validated, stored, queried, and
converted between length units without kernels. `transform_batch` supports ITRF93 as
either source or target, including entity frames rooted in it. The necessary loaded data
depends on the requested transformation:

| Transformation | Required data |
|---|---|
| ITRF93 → ITRF93 (including unit changes) | None |
| ITRF93 ↔ GCRF | Earth high-precision binary PCK (`.bpc`) |
| ITRF93 ↔ Earth / IAU_EARTH | Earth BPC plus ANISE planetary constants (`pck11.pca`) |
| ITRF93 ↔ ICRF | Earth BPC plus SPK ephemerides connecting Earth to the solar-system barycenter (e.g. `de440s.bsp`) |

Each row's observation epoch must be covered by a usable BPC segment and, when needed,
the SPK data. Unknown target names produce a catalog error; absent Earth BPC data and
uncovered epochs produce distinct errors with the row index (zero-based), epoch, and
frames. Other ANISE failures retain their underlying explanation. No alternative frame
is substituted. See [server kernel configuration](../soloc-server/README.md#itrf93-kernel-configuration).

The required integration tests use explicit local PCA/BPC files covering January 1, 2020
and January 1, 2025 (TAI). They are ignored in the default suite and never download data;
an explicit invocation fails if its files or coverage are missing:

```bash
SOLOC_KERNEL_PATHS=/path/pck11.pca:/path/earth_high_prec_snapshot.bpc \
  cargo test -p spacetimestamp --test itrf93_transforms -- --ignored
```

These tests verify the Soloc pipeline against ANISE rotations; they do not independently
validate the Earth orientation model.

## GPS coordinate and time conversion

`gps::wgs84_to_ecef_m(latitude_deg, longitude_deg, ellipsoidal_height_m)` converts WGS84
geodetic coordinates into Earth-centered, Earth-fixed metres without kernels. Latitude
must be finite and within `[-90, 90]`, longitude within `[-180, 180]`, and height finite.
Negative height is supported. Height is required and must refer to the WGS84 ellipsoid;
mean-sea-level height or an unspecified GPX elevation requires an explicit datum decision
before conversion. Longitude is east-positive and is not automatically wrapped.

`gps::wgs84_to_frame_m(latitude_deg, longitude_deg, ellipsoidal_height_m, epoch, target, almanac)`
uses an **approximate WGS84 ECEF → ITRF93 mapping for ordinary tracking**, then rotates
at that observation's epoch if requested. `ITRF93` works with an empty almanac;
`Earth`/`IAU_EARTH` and `GCRF`/`EME2000` use the data listed above. Other origins such as
ICRF require the existing batch transform after constructing an Earth-frame observation.
The approximation applies no realization or epoch-dependent datum correction and offers
no survey/RTK, sub-metre, or fixed-error guarantee. Numerical test tolerances describe
calculation error, not measurement or datum accuracy.

Encode observation time using `epoch_to_parts(epoch)` and `TimeScaleCode::TAI`. It converts
UTC, GPST, and other input epochs to TAI before subtracting J2000, retaining integer
nanoseconds. `duration_since_j2000_tai(epoch)` provides the equivalent duration for ledger
comparisons. For example, `2025-01-01T00:00:00 UTC` becomes `00:00:37 TAI` with row parts
`(0, 788961637000000000)`. Flight's JSON time fields instead use `epoch.to_tai_seconds()`
from **J1900** (`3944678437` for this instant); row offsets must not be sent directly.

UTC conversion uses the installed hifitime leap-second table. The locked version (4.3.1)
folds a parsed literal UTC `:60` onto `:59`, so supply inserted-second instants as TAI or
GPST instead (e.g. `2017-01-01T00:00:36 TAI`). The integer TAI row encoding preserves that
second and fractional nanoseconds. A later text/GPX parser must handle or reject literal
`:60` explicitly. Existing TAI rows produced with the former UTC conversion bug cannot
be identified or repaired automatically; rebuild affected rows from their original times.

## One GPS observation into the ledger

`gps::gps_observation_to_batch(&fix, &options, &almanac)` converts one fix into a complete
one-row `EntitySchema` batch, ready for `Ledger::append` or Flight `do_put`. It supplies
the nested `spacetimestamp`, frame and provenance IDs, metre units, TAI epoch parts,
position, and quaternion. All optional state and covariances remain null.

```rust
use anise::prelude::Almanac;
use soloc_ledger::ledger::Ledger;
use spacetimestamp::gps::{GpsObservation, GpsOptions, GpsOrientation, gps_observation_to_batch};
use spacetimestamp::schemas::entity::EntitySchema;
use spacetimestamp::PrescribedId;

let fix = GpsObservation {
    latitude_deg: 44.6488,
    longitude_deg: -63.5752,
    ellipsoidal_height_m: 12.0,
    epoch: "2025-01-01T00:00:00 UTC".parse()?,
    orientation: GpsOrientation::LocalEnu,
};
let options = GpsOptions::new(
    PrescribedId::new("example.org", "vessel-1")?,
    PrescribedId::abstract_source("example.org", "gps-receiver")?,
);
let batch = gps_observation_to_batch(&fix, &options, &Almanac::default())?;
let mut ledger = Ledger::for_schema::<EntitySchema>()?;
ledger.append(batch)?;
let latest = ledger.current_state(None, None)?;
```

Reuse the same options/entity ID across successive fixes. `GpsOptions::new` selects
approximate ITRF93 and `ESTIMATED`; its public fields allow another supported Earth
frame and estimate type. The tracked entity must have a Soloc ID. Its source may be a
Soloc receiver or abstract provenance ID, including the same Soloc ID for a self-report.

Orientation is always an explicit choice:

| Policy | Meaning |
|---|---|
| `GpsOrientation::LocalEnu` | Conventional east/north/up axes at the fix, not vehicle attitude. Requires `ESTIMATED` or `SIMULATED`; `MEASURED` returns an error because the schema's estimate type applies to the whole pose. At a pole, the supplied longitude selects east/north. |
| `GpsOrientation::QuaternionInEnu([w, x, y, z])` | Caller-supplied rotation from entity axes into local ENU. Requires finite components and norm within `1e-6` of one; roundoff is normalized. `MEASURED` is appropriate only when both position and attitude were measured. |

Both position and orientation use the same Earth rotation at the fix's epoch. A frame
change never leaves the quaternion expressed in the previous axes. Height and epoch
are required inputs; unknown elevation, vertical datum, or time must be resolved before
conversion. The ordinary-tracking approximation and leap-second parsing limits above
also apply to this API.

For a batch of fixes, `gps::convert_gps_observation` returns a validated
`ConvertedGpsObservation`. Its accessors expose pose fields for another schema, while
`append_to(&mut EntityBuilder)` reuses an existing builder and `to_entity_batch()`
produces a single row. Conversion finishes before any append, so errors cannot insert
a partial observation. Private result fields preserve validation until it is appended.

`GpsError` distinguishes invalid coordinates, identity, orientation, estimate policy,
unrepresentable time, invalid frame, and transformation failures. Transform errors carry
the target frame and epoch. A successful batch submitted to a ledger with a different
schema receives the ledger's existing recoverable schema-mismatch error. GPX parsing
and file-ingestion policies remain subsequent work.

## License

Apache-2.0. See the [workspace README](../../) for the ecosystem overview and roadmap.
