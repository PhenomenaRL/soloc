"""Summarises the frame topology in a saved sim ledger: the tree at an instant, and every
parent change over the run.

Topology is derived from the rows the way the ledger derives it: an entity's parent is the
`frame_id` of its latest row at or before t, and an event is a first sighting or a change of
parent. Astronomical frames are roots; the ledger hands them to anise rather than recursing
into their own rows, so the snapshot bodies show up twice: as entities under ICRF, and as
the IAU_* root that surface sites hang from.

    python topo_sim.py out/solar_sim.arrow                    # tree at the last epoch + events
    python topo_sim.py out/solar_sim.arrow --at 2026-09-02T12:00:00
    python topo_sim.py out/solar_sim.arrow --server grpc://localhost:50051   # also cross-check export_topology
"""

import argparse
from collections import defaultdict
from datetime import datetime
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.flight as fl

from soloc_client import CENTURY_NS, SolocClient, id_bytes, sts_field, tai_ns_from_utc
from view_sim import astro_pair, label, load, utc


def events_from_rows(table: pa.Table) -> tuple[list[tuple[bytes, bytes, int]], dict]:
    """`(child, parent, tai_ns)` at every first sighting and parent change, plus per-entity
    row stats. Rows are ordered by epoch per entity (ties keep file order, as the ledger does)."""
    ids = id_bytes(table.column("entity_id"))
    frames = id_bytes(sts_field(table, "frame_id"))
    t = (sts_field(table, "duration_centuries").to_numpy().astype(np.int64) * CENTURY_NS
         + sts_field(table, "duration_ns").to_numpy().astype(np.int64))

    code = {b: i for i, b in enumerate(dict.fromkeys(ids))}
    order = np.lexsort((t, np.array([code[b] for b in ids])))   # stable: ties keep file order

    events, stats = [], {}
    prev_id = prev_parent = None
    for k in order.tolist():
        eid, parent, tk = ids[k], frames[k], int(t[k])
        s = stats.setdefault(eid, {"rows": 0, "times": []})
        s["rows"] += 1
        s["times"].append(tk)
        if eid != prev_id or parent != prev_parent:
            events.append((eid, parent, tk))
        prev_id, prev_parent = eid, parent
    for s in stats.values():
        s["last"] = s["times"][-1]
        steps = np.diff(s.pop("times"))
        s["cadence_s"] = int(np.median(steps)) // 10**9 if len(steps) else None
    return events, stats


def tree_at(events, tai_ns: int) -> dict[bytes, bytes]:
    """child → parent as of `tai_ns`: the latest event at or before it."""
    parent = {}
    for child, p, t in sorted(events, key=lambda e: e[2]):
        if t <= tai_ns:
            parent[child] = p
    return parent


def cadence(seconds: int | None) -> str:
    if seconds is None:
        return "1 row"
    return f"{seconds // 3600} h" if seconds % 3600 == 0 else (
        f"{seconds // 60} min" if seconds % 60 == 0 else f"{seconds} s")


def print_tree(parent: dict[bytes, bytes], stats: dict, names: dict[bytes, str]):
    children = defaultdict(list)
    for c, p in parent.items():
        children[p].append(c)
    # Astro ids are always roots: resolution stops there, even when the same id also has rows.
    roots = sorted({p for p in parent.values() if astro_pair(p) is not None},
                   key=lambda r: label(r, names, as_frame=True))
    rows_by = lambda n: f"{stats[n]['rows']:,} rows, every {cadence(stats[n]['cadence_s'])}"

    def walk(node, prefix):
        kids = sorted(children.get(node, []), key=lambda n: label(n, names))
        for i, kid in enumerate(kids):
            last = i == len(kids) - 1
            print(f"{prefix}{'└─ ' if last else '├─ '}{label(kid, names)}  ({rows_by(kid)})")
            if astro_pair(kid) is None:
                walk(kid, prefix + ("   " if last else "│  "))

    for r in roots:
        print(f"{label(r, names, as_frame=True)}  [astro root, resolved by anise]")
        walk(r, "")
        print()

    def hops(n):
        """Rows to compose from `n` up to its astro root: a robot on a site is 2."""
        h, p = 1, parent[n]
        while astro_pair(p) is None:
            h, p = h + 1, parent[p]
        return h

    deepest = max((hops(n) for n in parent), default=0)
    print(f"{len(parent)} entities under {len(roots)} roots, deepest chain {deepest} hop(s) to a root")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("path", type=Path)
    p.add_argument("--at", help="UTC instant for the tree (default: the last epoch in the file)")
    p.add_argument("--server", help="also load the file into this server and compare its export_topology")
    args = p.parse_args()

    table, names = load(args.path)
    events, stats = events_from_rows(table)
    at_ns = (tai_ns_from_utc(datetime.fromisoformat(args.at)) if args.at
             else max(s["last"] for s in stats.values()))

    print(f"== tree at {utc(at_ns)} UTC ==\n")
    print_tree(tree_at(events, at_ns), stats, names)

    first = {}
    changes = []
    for child, parent, t in sorted(events, key=lambda e: e[2]):
        if child in first:
            changes.append((child, first[child], parent, t))
        first[child] = parent
    print(f"\n== events: {len(events)} ({len(first)} first sightings, {len(changes)} parent changes) ==")
    for child, old, new, t in changes:
        print(f"  {utc(t)}  {label(child, names)}: {label(old, names, True)} → {label(new, names, True)}")
    if not changes:
        print("  no entity changed parent")

    if args.server:
        client = SolocClient(args.server)
        print("\n" + client.action("load_ledger", {"path": str(args.path.resolve())}))
        [result] = client.flight.do_action(fl.Action("export_topology", b""))
        log = pa.ipc.open_file(result.body).read_all()
        server = set(zip(id_bytes(log.column("child_id")), id_bytes(log.column("parent_id")),
                         (np.asarray(log.column("duration_centuries"), dtype=np.int64) * CENTURY_NS
                          + np.asarray(log.column("duration_ns"), dtype=np.int64)).tolist()))
        mine = set(events)
        ok = server == mine
        print(f"{'PASS' if ok else 'FAIL'}  export_topology matches the row-derived events  "
              f"(server {len(server)}, rows {len(mine)}"
              + ("" if ok else f", only server {len(server - mine)}, only rows {len(mine - server)}") + ")")


if __name__ == "__main__":
    main()
