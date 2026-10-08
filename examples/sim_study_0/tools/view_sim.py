"""Prints a saved sim ledger as a readable table, decoded: ids as names, vocabulary codes as
tokens, epochs as UTC. Reads the file directly; no server needed.

    python -m tools.view_sim out/sim_study_0.arrow                # first 20 rows
    python -m tools.view_sim out/sim_study_0.arrow --entity AND-R03 --limit 10
    python -m tools.view_sim out/sim_study_0.arrow --entity shackleton --tail --limit 5
    python -m tools.view_sim out/sim_study_0.arrow --summary      # one line per entity
    python -m tools.view_sim out/sim_study_0.arrow --schema
"""

import argparse
from datetime import timedelta
from pathlib import Path

import pyarrow as pa
import pyarrow.compute as pc

from sim.geo import EARTH, MARS, MOON, SUN
from soloc_client import (CENTURY_NS, J2000, KIND_ABSTRACT, KIND_ASTRO, TAI_MINUS_UTC_S, id_bytes,
                          mint, sts_field, vocabulary)

# Astro ids embed their (ephemeris_id, orientation_id) pair rather than hashing a name, so they
# decode without the registry. Bodies read as bare names as entities and IAU_* names as frames.
ASTRO_NAMES = {(0, 1): ("ICRF", "ICRF"), (399, 1): ("GCRF", "GCRF")} | {
    (b.naif, b.naif): (b.name, b.frame) for b in (SUN, EARTH, MOON, MARS)}

# The server stamps append_snapshot rows with this source (ephemeris.rs `anise_source_id`),
# and it isn't in the sim's registry.
KNOWN_NAMES = {mint(KIND_ABSTRACT, "anise", "almanac"): "anise/almanac"}

# Columns shown by default, in order; any that are null in every shown row are dropped.
COLUMNS = ["entity", "frame", "epoch_utc", "units", "position", "quaternion", "velocity",
           "angular_velocity", "acceleration", "mass_kg", "dimensions", "timescale", "estimate",
           "source"]


def astro_pair(b: bytes) -> tuple[int, int] | None:
    if b[6] != 0x80 | KIND_ASTRO:
        return None
    return int.from_bytes(b[0:4], "big", signed=True), int.from_bytes(b[9:13], "big", signed=True)


def label(b: bytes, names: dict[bytes, str], as_frame: bool = False) -> str:
    if b in names:
        return names[b]
    pair = astro_pair(b)
    if pair is not None:
        known = ASTRO_NAMES.get(pair)
        return known[as_frame] if known else f"astro{pair}"
    return b.hex()[:12] + "…"


def utc(tai_ns: int) -> str:
    """Valid for epochs after 2017-01-01, where TAI − UTC is a constant 37 s."""
    t = J2000 + timedelta(microseconds=tai_ns // 1000) - timedelta(seconds=TAI_MINUS_UTC_S)
    return t.isoformat(timespec="milliseconds" if t.microsecond else "seconds")


def vector(v) -> str | None:
    return None if v is None else "[" + ", ".join(f"{x:.3f}" for x in v) + "]"


def load(path: Path) -> tuple[pa.Table, dict[bytes, str]]:
    table = pa.ipc.open_file(path).read_all()
    names_path = path.with_name(path.name + ".names.arrow")
    names = dict(KNOWN_NAMES)
    if names_path.exists():
        reg = pa.ipc.open_file(names_path).read_all()
        names.update(zip(reg.column("prescribed_id").to_pylist(),
                         reg.column("common_name").to_pylist()))
    return table, names


def decode(table: pa.Table, names: dict[bytes, str]) -> list[dict]:
    sts_type = table.schema.field("spacetimestamp").type
    tokens = {f: {v: k for k, v in vocabulary(sts_type.field(f)).items()}
              for f in ("units_pos", "timescale_id", "estimate_type")}
    s = {f.name: sts_field(table, f.name).to_pylist() for f in sts_type
         if f.name not in ("frame_id", "source_id")}
    frames = id_bytes(sts_field(table, "frame_id"))
    sources = id_bytes(sts_field(table, "source_id"))
    rest = {c: table.column(c).to_pylist() for c in
            ("velocity", "angular_velocity", "acceleration", "mass_kg", "dimensions")}
    rows = []
    for i, eid in enumerate(id_bytes(table.column("entity_id"))):
        rows.append({
            "entity": label(eid, names),
            "frame": label(frames[i], names, as_frame=True),
            "epoch_utc": utc(s["duration_centuries"][i] * CENTURY_NS + s["duration_ns"][i]),
            "units": tokens["units_pos"][s["units_pos"][i]],
            "position": vector(s["position"][i]),
            "quaternion": vector(s["quaternion"][i]),
            "velocity": vector(rest["velocity"][i]),
            "angular_velocity": vector(rest["angular_velocity"][i]),
            "acceleration": vector(rest["acceleration"][i]),
            "mass_kg": None if rest["mass_kg"][i] is None else f"{rest['mass_kg'][i]:.6g}",
            "dimensions": vector(rest["dimensions"][i]),
            "timescale": tokens["timescale_id"][s["timescale_id"][i]],
            "estimate": tokens["estimate_type"][s["estimate_type"][i]],
            "source": label(sources[i], names),
        })
    return rows


def print_grid(rows: list[dict], columns: list[str]):
    columns = [c for c in columns if any(r[c] is not None for r in rows)]
    cell = lambda v: "" if v is None else str(v)
    width = {c: max(len(c), *(len(cell(r[c])) for r in rows)) for c in columns}
    print("  ".join(c.ljust(width[c]) for c in columns))
    print("  ".join("-" * width[c] for c in columns))
    for r in rows:
        print("  ".join(cell(r[c]).ljust(width[c]) for c in columns).rstrip())


def select(table: pa.Table, names: dict[bytes, str], patterns: list[str]) -> pa.Table:
    """Rows whose entity label contains any pattern, case-insensitively."""
    # Compute kernels don't take the arrow.uuid extension type; its binary storage they do.
    storage = table.column("entity_id").combine_chunks().storage
    wanted = [b for b in pc.unique(storage).to_pylist()
              if any(p.lower() in label(b, names).lower() for p in patterns)]
    if not wanted:
        raise SystemExit(f"no entity matches {patterns}")
    return table.filter(pc.is_in(storage, value_set=pa.array(wanted, pa.binary(16))))


def summary(table: pa.Table, names: dict[bytes, str]):
    ids = id_bytes(table.column("entity_id"))
    frames = id_bytes(sts_field(table, "frame_id"))
    t = (sts_field(table, "duration_centuries").to_numpy().astype("int64") * CENTURY_NS
         + sts_field(table, "duration_ns").to_numpy().astype("int64"))
    stats: dict[bytes, dict] = {}
    for eid, fid, ti in zip(ids, frames, t.tolist()):
        s = stats.setdefault(eid, {"rows": 0, "first": ti, "last": ti, "frames": []})
        s["rows"] += 1
        s["first"], s["last"] = min(s["first"], ti), max(s["last"], ti)
        if fid not in s["frames"]:
            s["frames"].append(fid)
    rows = [{"entity": label(e, names), "rows": f"{s['rows']:,}", "first_utc": utc(s["first"]),
             "last_utc": utc(s["last"]),
             "frames": ", ".join(label(f, names, as_frame=True) for f in s["frames"])}
            for e, s in stats.items()]
    rows.sort(key=lambda r: r["entity"])
    print_grid(rows, ["entity", "rows", "first_utc", "last_utc", "frames"])
    print(f"\n{len(rows)} entities, {table.num_rows:,} rows")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("path", type=Path)
    p.add_argument("--entity", action="append", default=[],
                   help="keep entities whose name contains this (case-insensitive); repeatable")
    p.add_argument("--limit", type=int, default=20, help="rows to print (default 20)")
    p.add_argument("--offset", type=int, default=0, help="rows to skip first")
    p.add_argument("--tail", action="store_true", help="print the last --limit rows instead")
    p.add_argument("--summary", action="store_true", help="one line per entity instead of rows")
    p.add_argument("--schema", action="store_true", help="print the Arrow schema and exit")
    args = p.parse_args()

    table, names = load(args.path)
    if args.schema:
        print(table.schema)
        return
    if args.entity:
        table = select(table, names, args.entity)
    if args.summary:
        summary(table, names)
        return

    start = max(table.num_rows - args.limit, 0) if args.tail else args.offset
    shown = table.slice(start, args.limit)
    if not shown.num_rows:
        raise SystemExit("no rows in that range")
    print_grid(decode(shown, names), COLUMNS)
    print(f"\nrows {start:,}–{start + shown.num_rows - 1:,} of {table.num_rows:,}"
          + (f" matching {args.entity}" if args.entity else ""))


if __name__ == "__main__":
    main()
