# soloc_client reference

The quickstart is in [client_quickstart.md](client_quickstart.md).

## Ids

Every id is 16 bytes. There are three kinds:

| Kind | Made with | Used for |
|---|---|---|
| `KIND_SOLOC` | `mint(KIND_SOLOC, authority, name)` | your entities and the frames they define |
| `KIND_ABSTRACT` | `mint(KIND_ABSTRACT, authority, name)` | things with no pose, such as the `source_id` of a sensor or pipeline |
| `KIND_ASTRO` | `astronomical(ephemeris_id, orientation_id)` | frames the kernels resolve |

`mint` is a hash, so the same `(kind, authority, name)` gives the same id on every machine,
with no registry or server call. Use a domain you control as the authority. The authority is
lower-cased before hashing; the name is not.

An astronomical id holds a NAIF pair: the origin and the orientation. The server accepts only
the pairs in `ASTRO_FRAMES` in
[`ephemeris.rs`](../../../crates/spacetimestamp/src/ephemeris.rs): `ICRF`, `GCRF`, `EMB`, and
the IAU body-fixed frames of the Sun, the planets, Pluto and the larger moons.

## Time

`tai_ns_from_utc(datetime)` converts a naive UTC `datetime` from 2017 onwards, using
TAI − UTC = 37 s (valid until the next leap second). `RowBuffer` can tag `TAI`, `TT`, `GPST`
and `UTC`. Rows read back carry the epoch as `duration_centuries` and `duration_ns`;
`from_parts` turns the pair into TAI nanoseconds again.

## Units and vocabularies

`units`, `timescale` and `estimate` are stored as one-byte codes. The tokens are in the
schema's field metadata, and `RowBuffer` reads them from there, so you pass tokens:

| Argument | Tokens |
|---|---|
| `units` | `km` `m` `cm` `mm` `au` `in` `ft` `mi` `nmi` |
| `estimate` | `MEASURED` `ESTIMATED` `SIMULATED` |

`units` applies to `position` only. The other columns are fixed SI.

## Optional columns

Pass these to `append` by name. Any you leave out are null.

| Column | Type | Units |
|---|---|---|
| `velocity` | 3 floats | m/s |
| `angular_velocity` | 3 floats | rad/s |
| `acceleration` | 3 floats | m/s² |
| `mass_kg` | float | kg |
| `dimensions` | 3 floats | m, the bounding box along the entity's own axes |
| `state_covariance` | 21 floats | upper triangle of the 6 × 6 position-velocity covariance |

## `SolocClient(url="grpc://localhost:50051")`

Connecting fetches the server's schema into `client.schema`.

| Method | Does |
|---|---|
| `buffer()` | a new `RowBuffer` on the server's schema |
| `put(batch)` | appends a batch, sent in chunks of `PUT_CHUNK_ROWS` (65,536) rows so no message nears the server's 64 MiB limit; raises if the server rejects it |
| `query_all()` | every stored row |
| `current_state(entity_ids=None, not_before_tai_ns=None)` | the latest row of each entity, or of the ones listed; rows older than `not_before_tai_ns` (J2000 TAI ns) are dropped, by default those over an hour older than the newest row |
| `exchange(table, target_frame, units="km")` | the same rows re-expressed in `target_frame`; nothing is stored |
| `snapshot(body_ids, tai_ns)` | appends the kernels' state of each body at that epoch, as rows under ICRF |
| `orbits(body_ids, tai_ns, centres=None, samples=None)` | each body's osculating orbit at that epoch, one `query_orbits` row per body; nothing is stored |
| `names()` | id → name, from the server's registry |
| `action(name, body)` | any server action; `body` is a dict (sent as JSON) or bytes |

`exchange` takes the name of an astronomical frame (`"ICRF"`, `"GCRF"`, `"IAU_MOON"`, …). The
rows do not have to be in the ledger. It sends 2,048 rows per message. The server accepts up to
`server.max_message_size` bytes per message (default 64 MiB).

## Server actions

| Action | Body | Does |
|---|---|---|
| `save_ledger` | `{"path": ...}` | writes the ledger to an Arrow IPC file, with a `.names.arrow` file beside it |
| `load_ledger` | `{"path": ...}` | replaces the in-memory ledger from a file |
| `import_names` | `registry_ipc(bindings)` | adds names; each is checked against the id it claims |
| `export_names` | none | the registry as Arrow IPC (`names()` decodes it) |
| `append_snapshot` | `{"bodies": [...], "epoch_tai_s": ...}` | what `snapshot()` sends |
| `query_orbits` | `{"orbits": [{"body": ..., "centre": ...}], "epoch_tai_s": ..., "samples": ...}` | what `orbits()` sends; replies with orbit rows as Arrow IPC (see [orbits.md](orbits.md)) |
| `load_kernel` | `{"source": ...}` | loads another kernel from a URL or a path on the server |
| `export_topology`, `import_topology` | none / Arrow IPC | the log of parent changes, for passing between servers |

## `RowBuffer`

| Method | Does |
|---|---|
| `append(entity_id, frame_id, position, quaternion, tai_ns, *, units="km", timescale="TAI", source_id=SIM_SOURCE, estimate="SIMULATED", **optional)` | adds a row |
| `flush()` | returns the rows as a `RecordBatch` and empties the buffer |
| `len(buf)` | rows waiting |

## Reading results

Every query returns a `pyarrow.Table` in the ledger's schema, except `orbits()`, which returns
orbit rows.

| Helper | Returns |
|---|---|
| `positions(table)` | `(N, 3)` numpy array, each row in its own `units_pos` |
| `sts_field(table, name)` | one field of the `spacetimestamp` struct, such as `"quaternion"` or `"duration_ns"` |
| `entity_ids(table)` | the `entity_id` column as `bytes` |
| `id_bytes(column)` | any id column as `bytes`, such as `sts_field(table, "frame_id")` |
| `matches(ids, value)` | boolean mask of the ids equal to `value` |

## Without this client

Any Arrow Flight client in any language can do the same. The wire contract is:

| Call | Request |
|---|---|
| `get_schema` | any descriptor; returns the ledger's schema |
| `do_put` | batches in that schema |
| `do_get` | a JSON ticket: `{"query_type": "filter"}` with optional `time_range_tai_s: [start, end]`, or `{"query_type": "current_state"}` with optional `entity_ids` and `not_before_tai_s` |
| `do_exchange` | a command descriptor `{"target_frame": "GCRF", "target_units": 1}`, then batches; `target_units` is the unit's code |
| `do_action` | the actions above |

JSON epochs (`epoch_tai_s`, `time_range_tai_s`, `not_before_tai_s`) are TAI seconds since
J1900. Wherever the JSON takes an id, any of three forms works:

```json
{"authority": "example.org", "name": "rover_1"}
{"ephemeris_id": 399, "orientation_id": 399}
{"id": "cdeaa9ed9ac78135bb004920ec1f46ac"}
```

The ledger file that `save_ledger` writes is plain Arrow IPC. `pyarrow.ipc.open_file` reads it
with no server, and Rust reloads it with `Ledger::load_ipc`.
