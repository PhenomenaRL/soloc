"""Merges the ledgers of separate `run_sim.py --scenario` runs into one, through a fresh
soloc-server, so `Ledger::append` re-validates every row. Parts go in in `GROUPS` order. An
entity found in several parts (every run writes the body snapshots) is kept from the first,
and the merge fails if a later copy differs in any column. The parts' side tables (wind, fuel,
bearing truth) are concatenated beside the output.

    python -m tools.merge_sim                  # every out/<group>/ that exists
    python -m tools.merge_sim space factory    # just those
"""

import argparse
import sys
from pathlib import Path

import pyarrow as pa
import pyarrow.compute as pc

from sim.fuel import FUEL_SCHEMA
from sim.models.bearing import TRUTH_SCHEMA
from sim.roster import GROUPS
from sim.wind import WIND_SCHEMA
from soloc_client import SolocClient

PART = "sim_study_0.arrow"
# Each side table, and the column no two parts may share a value of.
SIDE_TABLES = {"wind.arrow": (WIND_SCHEMA, "venue"), "fuel.arrow": (FUEL_SCHEMA, "venue"),
               "bearing_truth.arrow": (TRUTH_SCHEMA, "line")}


def read(path: Path) -> pa.Table:
    """Memory-mapped, so a part costs no copy until it is filtered."""
    return pa.ipc.open_file(pa.memory_map(str(path))).read_all()


def ids(table: pa.Table) -> pa.Array:
    # Compute kernels don't take the arrow.uuid extension type; its binary storage they do.
    return table.column("entity_id").combine_chunks().storage


def of(table: pa.Table, wanted: set[bytes]) -> pa.Array:
    return pc.is_in(ids(table), value_set=pa.array(sorted(wanted), pa.binary(16)))


def by_epoch(table: pa.Table) -> pa.Table:
    sts = table.column("spacetimestamp").combine_chunks()
    keys = pa.table({"id": ids(table), "c": sts.field("duration_centuries"),
                     "ns": sts.field("duration_ns")})
    return table.take(pc.sort_indices(keys, sort_keys=[(k, "ascending") for k in keys.column_names]))


def merge_side_tables(dirs: list[Path], out: Path):
    for name, (schema, key) in SIDE_TABLES.items():
        tables = [read(d / name) for d in dirs if (d / name).exists()]
        if not tables:
            continue
        keys = [set(pc.unique(t[key]).to_pylist()) for t in tables]
        if sum(map(len, keys)) != len(set().union(*keys)):
            sys.exit(f"{name}: a {key} appears in more than one part")
        table = pa.concat_tables(tables)
        with pa.ipc.new_file(out.with_name(name), schema) as f:
            f.write_table(table)
        print(f"{table.num_rows:,} rows → {out.with_name(name)}")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("groups", nargs="*", metavar="GROUP",
                   help=f"parts to merge, from {', '.join(GROUPS)} (default: every one in out/)")
    p.add_argument("--out", default="out/sim_study_0.arrow")
    p.add_argument("--server", default="grpc://localhost:50051")
    args = p.parse_args()
    if unknown := set(args.groups) - set(GROUPS):
        p.error(f"unknown groups {sorted(unknown)}; choose from {', '.join(GROUPS)}")

    root = Path("out")
    groups = [g for g in GROUPS if g in args.groups or (not args.groups and (root / g / PART).exists())]
    if not groups:
        sys.exit(f"no out/<group>/{PART}; run run_sim.py --scenario GROUP first")
    if missing := [g for g in groups if not (root / g / PART).exists()]:
        sys.exit(f"no part for {', '.join(missing)}; run run_sim.py --scenario first")

    client = SolocClient(args.server)
    if not client.is_empty():
        sys.exit("ledger is not empty; restart serve.sh first")

    parts = {g: read(root / g / PART) for g in groups}
    present = {g: set(pc.unique(ids(t)).to_pylist()) for g, t in parts.items()}
    shared = {i for g in groups for i in present[g]
              if sum(i in present[h] for h in groups) > 1}
    owned: set[bytes] = set()
    kept: list[pa.Table] = []          # the first copy of each shared entity's rows
    sent = 0
    for g, table in parts.items():
        repeat = present[g] & owned
        if repeat:
            theirs = by_epoch(pa.concat_tables([t.filter(of(t, repeat)) for t in kept]))
            mine = by_epoch(table.filter(of(table, repeat)))
            if not mine.equals(theirs):
                sys.exit(f"{g}: rows of {len(repeat)} entities shared with an earlier part differ")
            table = table.filter(pc.invert(of(table, repeat)))
        if present[g] & shared - owned:
            kept.append(table.filter(of(table, shared)))
        owned |= present[g]
        for batch in table.to_batches():
            client.put(batch)
        sent += table.num_rows
        print(client.action("import_names", (root / g / f"{PART}.names.arrow").read_bytes()))
        print(f"{g}: {table.num_rows:,} rows, {len(present[g] - shared):,} entities of its own"
              + (f", {len(repeat)} shared kept from an earlier part" if repeat else ""))

    out = Path(args.out).resolve()
    out.parent.mkdir(parents=True, exist_ok=True)
    print(client.action("save_ledger", {"path": str(out)}))
    reader = pa.ipc.open_file(pa.memory_map(str(out)))
    saved = sum(reader.get_batch(i).num_rows for i in range(reader.num_record_batches))
    if saved != sent:
        sys.exit(f"saved {saved:,} rows of the {sent:,} sent; a memory_limit evicted the rest")
    print(f"{saved:,} rows from {len(groups)} parts, {len(owned):,} entities")
    merge_side_tables([root / g for g in groups], out)


if __name__ == "__main__":
    main()
