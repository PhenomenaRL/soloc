"""Checks that two saved ledgers hold the same rows and names, whatever their batch layout:
e.g. a merge of separate `--scenario` runs against the monolithic run. No server needed.

    python -m tests.compare_ledgers out/sim_study_0.arrow out/mono/sim_study_0.arrow
"""

import argparse
import sys
from pathlib import Path

import pyarrow.compute as pc

from tools.merge_sim import by_epoch, ids
from tools.view_sim import label, load

SLICE_ROWS = 65536


def first_difference(a, b) -> int:
    for lo in range(0, a.num_rows, SLICE_ROWS):
        if not a.slice(lo, SLICE_ROWS).equals(b.slice(lo, SLICE_ROWS)):
            return next(lo + k for k in range(SLICE_ROWS)
                        if not a.slice(lo + k, 1).equals(b.slice(lo + k, 1)))
    return -1


def main():
    p = argparse.ArgumentParser()
    p.add_argument("a", type=Path)
    p.add_argument("b", type=Path)
    args = p.parse_args()

    (a, names_a), (b, names_b) = load(args.a), load(args.b)
    ok = True
    only = set(names_a.items()) ^ set(names_b.items())
    print(f"names: {len(names_a):,} vs {len(names_b):,}, {len(only)} differ")
    ok &= not only

    ids_a, ids_b = (set(pc.unique(ids(t)).to_pylist()) for t in (a, b))
    for side, extra in ((args.a, ids_a - ids_b), (args.b, ids_b - ids_a)):
        if extra:
            ok = False
            print(f"only in {side}: {len(extra)} entities, e.g. {label(next(iter(extra)), names_a | names_b)}")
    print(f"rows: {a.num_rows:,} vs {b.num_rows:,}")
    if a.num_rows == b.num_rows and not (ids_a ^ ids_b):
        a, b = by_epoch(a), by_epoch(b)
        k = first_difference(a, b)
        if k >= 0:
            ok = False
            eid = ids(a.slice(k, 1))[0].as_py()
            print(f"first differing row {k:,} (sorted by entity and epoch), entity {label(eid, names_a)}")
    else:
        ok = False
    print("IDENTICAL" if ok else "DIFFERENT")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
