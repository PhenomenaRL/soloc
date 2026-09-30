"""Python side of the soloc contract: id minting, epoch encoding, batch building, Flight calls."""

import hashlib
import json
from datetime import datetime, timedelta

import numpy as np
import pyarrow as pa
import pyarrow.flight as fl

# -- ids: a port of the reference in identity.rs `frozen_mint_vectors` --------------------------

NAMESPACE = bytes.fromhex("ee8c42090dd59d8d14afc9541cafe95d")
KIND_ASTRO, KIND_SOLOC, KIND_ABSTRACT = 0x0, 0x1, 0x2


def mint(kind: int, authority: str, name: str) -> bytes:
    h = hashlib.sha256()
    h.update(NAMESPACE)
    h.update(bytes([kind]))
    h.update(authority.lower().encode())
    h.update(b"\x00")
    h.update(name.encode())
    b = bytearray(h.digest()[:16])
    b[6] = 0x80 | kind
    b[8] = (b[8] & 0x3F) | 0x80
    return bytes(b)


def astronomical(ephemeris_id: int, orientation_id: int) -> bytes:
    b = bytearray(16)
    b[0:4] = ephemeris_id.to_bytes(4, "big", signed=True)
    b[6] = 0x80 | KIND_ASTRO
    b[8] = 0x80
    b[9:13] = orientation_id.to_bytes(4, "big", signed=True)
    return bytes(b)


SIM_SOURCE = mint(KIND_ABSTRACT, "sim.soloc", "kinematic_sim_v1")

# -- epochs: integer TAI nanoseconds since J2000 TAI (2000-01-01T12:00:00 TAI) -----------------

CENTURY_NS = 36525 * 86400 * 10**9
J2000 = datetime(2000, 1, 1, 12)
# TAI - UTC since 2017-01-01; the sim window has no leap second.
TAI_MINUS_UTC_S = 37
# Where each timescale's own J2000 falls, in ns after J2000 TAI. A row tagged with that scale
# stores its offset from this moment (ephemeris.rs `j2000_in_timescale`). Fixed offsets only.
J2000_IN_TAI_NS = {
    "TAI": 0,
    "TT": -32_184_000_000,
    "GPST": 19_000_000_000,
    "UTC": 32_000_000_000,
}


def tai_ns_from_utc(dt: datetime) -> int:
    assert dt >= datetime(2017, 1, 1), "TAI_MINUS_UTC_S only holds after 2017-01-01"
    td = dt + timedelta(seconds=TAI_MINUS_UTC_S) - J2000
    return (td.days * 86400 + td.seconds) * 10**9 + td.microseconds * 1000


def to_parts(ns: int) -> tuple[int, int]:
    """`(duration_centuries, duration_ns)`, matching hifitime `Duration::to_parts`."""
    return divmod(ns, CENTURY_NS)


def from_parts(centuries: int, ns: int) -> int:
    return centuries * CENTURY_NS + ns


# The JSON `epoch_tai_s` fields count TAI seconds from J1900, not J2000 (step-0 finding (a)).
J1900_TO_J2000_S = 3_155_716_800


def epoch_tai_s(tai_ns: int) -> float:
    return tai_ns / 1e9 + J1900_TO_J2000_S


# -- batches ---------------------------------------------------------------------------------


def vocabulary(field: pa.Field) -> dict[str, int]:
    """Token → code, decoded from the field's own metadata (vocabulary.rs)."""
    tokens = field.metadata[b"ARROW:extension:metadata"].decode().split(",")
    return {t: i for i, t in enumerate(tokens)}


def _column(values: list, typ: pa.DataType) -> pa.Array:
    if isinstance(typ, pa.BaseExtensionType):
        return pa.ExtensionArray.from_storage(typ, _column(values, typ.storage_type))
    if pa.types.is_struct(typ):
        children = [_column([v.get(f.name) for v in values], f.type) for f in typ]
        return pa.StructArray.from_arrays(children, fields=list(typ))
    return pa.array(values, type=typ)


class RowBuffer:
    """Rows for one batch, built against the server's schema so every batch matches it."""

    def __init__(self, schema: pa.Schema):
        self.schema = schema
        sts = schema.field("spacetimestamp").type
        self.codes = {f.name: vocabulary(f) for f in sts if f.metadata and f.type == pa.uint8()}
        self.rows: list[dict] = []

    def __len__(self) -> int:
        return len(self.rows)

    def append(self, entity_id, frame_id, position, quaternion, tai_ns, *, units="km",
               timescale="TAI", source_id=SIM_SOURCE, estimate="SIMULATED", **optional):
        """`optional` takes the entity columns by name: velocity, mass_kg, dimensions, ..."""
        centuries, ns = to_parts(tai_ns - J2000_IN_TAI_NS[timescale])
        self.rows.append({
            "entity_id": entity_id,
            "spacetimestamp": {
                "frame_id": frame_id,
                "units_pos": self.codes["units_pos"][units],
                "timescale_id": self.codes["timescale_id"][timescale],
                "source_id": source_id,
                "estimate_type": self.codes["estimate_type"][estimate],
                "position": list(position),
                "quaternion": list(quaternion),
                "duration_centuries": centuries,
                "duration_ns": ns,
            },
            **optional,
        })

    def flush(self) -> pa.RecordBatch:
        cols = [_column([r.get(f.name) for r in self.rows], f.type) for f in self.schema]
        self.rows = []
        return pa.RecordBatch.from_arrays(cols, schema=self.schema)


# -- Flight ----------------------------------------------------------------------------------


def wire(id_bytes: bytes) -> dict:
    return {"id": id_bytes.hex()}


def entity_ids(table: pa.Table) -> list[bytes]:
    """The id column as bytes; pyarrow reads `arrow.uuid` back as `uuid.UUID`."""
    return [u.bytes for u in table.column("entity_id").to_pylist()]


def id_bytes(column) -> list[bytes]:
    """Any id column (top-level or a struct child) as bytes, without a `uuid.UUID` per row."""
    arr = column.combine_chunks() if isinstance(column, pa.ChunkedArray) else column
    if isinstance(arr, pa.ExtensionArray):
        arr = arr.storage
    return arr.to_pylist()


def matches(ids, value: bytes) -> np.ndarray:
    """Boolean mask of `ids == value`. Not numpy's `==`: that goes through `np.bytes_`, which
    drops trailing NULs, and astro ids end in zero bytes."""
    return np.fromiter((i == value for i in ids), bool, len(ids))


def sts_field(table: pa.Table, name: str) -> pa.Array:
    return table.column("spacetimestamp").combine_chunks().field(name)


def positions(table: pa.Table) -> np.ndarray:
    """`(N, 3)` positions in each row's own `units_pos`."""
    return sts_field(table, "position").flatten().to_numpy().reshape(-1, 3)


REGISTRY_SCHEMA = pa.schema([
    pa.field("prescribed_id", pa.binary(16), nullable=False),
    pa.field("authority", pa.utf8(), nullable=False),
    pa.field("common_name", pa.utf8(), nullable=False),
    pa.field("kind", pa.uint8(), nullable=False),
])


def registry_ipc(bindings: list[tuple[int, str, str]]) -> bytes:
    """An Arrow IPC file of `(kind, authority, name)` bindings, as `import_names` expects."""
    table = pa.table({
        "prescribed_id": [mint(k, a, n) for k, a, n in bindings],
        "authority": [a for _, a, _ in bindings],
        "common_name": [n for _, _, n in bindings],
        "kind": [k for k, _, _ in bindings],
    }, schema=REGISTRY_SCHEMA)
    sink = pa.BufferOutputStream()
    with pa.ipc.new_file(sink, table.schema) as w:
        w.write_table(table)
    return sink.getvalue().to_pybytes()


EXCHANGE_CHUNK_ROWS = 2048


class SolocClient:
    def __init__(self, url: str = "grpc://localhost:50051"):
        self.flight = fl.FlightClient(url)
        self.schema = self.flight.get_schema(fl.FlightDescriptor.for_path("entities")).schema

    def buffer(self) -> RowBuffer:
        return RowBuffer(self.schema)

    def put(self, batch: pa.RecordBatch):
        writer, _ = self.flight.do_put(fl.FlightDescriptor.for_path("entities"), batch.schema)
        writer.write_batch(batch)
        writer.close()

    def action(self, name: str, body: dict | bytes | None = None) -> str:
        if isinstance(body, bytes):
            payload = body
        else:
            payload = json.dumps(body).encode() if body is not None else b""
        results = list(self.flight.do_action(fl.Action(name, payload)))
        return results[0].body.to_pybytes().decode() if results else ""

    def snapshot(self, body_ids: list[bytes], tai_ns: int) -> str:
        return self.action("append_snapshot", {"bodies": [wire(b) for b in body_ids],
                                               "epoch_tai_s": epoch_tai_s(tai_ns)})

    def names(self) -> dict[bytes, str]:
        """id → common name, from the server's registry (`export_names`)."""
        [result] = self.flight.do_action(fl.Action("export_names", b""))
        table = pa.ipc.open_file(result.body).read_all()
        return dict(zip(table.column("prescribed_id").to_pylist(),
                        table.column("common_name").to_pylist()))

    def _get(self, ticket: dict) -> pa.Table:
        return self.flight.do_get(fl.Ticket(json.dumps(ticket).encode())).read_all()

    def query_all(self) -> pa.Table:
        """Every stored row, unlike `current_state`, which keeps only the latest per entity."""
        return self._get({"query_type": "filter"})

    def current_state(self, entity_ids: list[bytes] | None = None) -> pa.Table:
        ticket = {"query_type": "current_state"}
        if entity_ids is not None:
            ticket["entity_ids"] = [wire(i) for i in entity_ids]
        return self._get(ticket)

    def exchange(self, data: pa.Table, target_frame: str, units: str = "km") -> pa.Table:
        units_code = vocabulary(self.schema.field("spacetimestamp").type.field("units_pos"))[units]
        cmd = json.dumps({"target_frame": target_frame, "target_units": units_code}).encode()
        writer, reader = self.flight.do_exchange(fl.FlightDescriptor.for_command(cmd))
        writer.begin(data.schema)
        # The server decodes at most 4 MiB per message (tonic's default), ~8k entity rows.
        writer.write_table(data, max_chunksize=EXCHANGE_CHUNK_ROWS)
        writer.done_writing()
        table = reader.read_all()
        writer.close()
        return table
