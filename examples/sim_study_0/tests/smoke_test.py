"""Step 0: checks the server contract the sim relies on and resolves the plan's unknowns.

Run against a freshly started serve.sh (empty ledger). Exits non-zero on any FAIL.
"""

import argparse
import math
import struct
import sys
from datetime import datetime
from pathlib import Path

from soloc_client import (KIND_ABSTRACT, KIND_SOLOC, SolocClient, astronomical, entity_ids,
                          from_parts, id_bytes, mint, tai_ns_from_utc, wire)

HERE = Path(__file__).resolve().parent.parent
J1900_TO_J2000_NS = 3_155_716_800 * 10**9
DAY_S = 86400
SUN, EARTH, MOON, MARS = (astronomical(n, n) for n in (10, 399, 301, 499))

results: list[bool] = []


def check(name: str, ok: bool, detail: str = ""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  ({detail})" if detail else ""))


def finding(text: str):
    print(f"      finding: {text}")


def sts(table, row: int = 0) -> dict:
    return table.column("spacetimestamp")[row].as_py()


def stored_tai_ns(table, entity_id: bytes) -> list[int]:
    return [from_parts(sts(table, i)["duration_centuries"], sts(table, i)["duration_ns"])
            for i, e in enumerate(entity_ids(table)) if e == entity_id]


def spk_segments(path: Path) -> set[tuple[int, int]]:
    """(target, center) of every segment in an SPK file, read from its DAF summary records."""
    with open(path, "rb") as f:
        head = f.read(1024)
        nd, ni = struct.unpack("<ii", head[8:16])
        rec = struct.unpack("<i", head[76:80])[0]
        size = nd + (ni + 1) // 2
        pairs = set()
        while rec:
            f.seek((rec - 1) * 1024)
            block = f.read(1024)
            nxt, _, nsum = struct.unpack("<ddd", block[:24])
            for i in range(int(nsum)):
                off = 24 + i * size * 8 + nd * 8
                ints = struct.unpack(f"<{ni}i", block[off:off + ni * 4])
                pairs.add((ints[0], ints[1]))
            rec = int(nxt)
    return pairs


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--server", default="grpc://localhost:50051")
    args = p.parse_args()

    client = SolocClient(args.server)
    if client.query_all().num_rows:
        sys.exit("ledger is not empty; restart serve.sh first")

    # -- schema and ids --------------------------------------------------------------------
    names = client.schema.names
    check("get_schema", names[:2] == ["entity_id", "spacetimestamp"], ", ".join(names))

    vectors = [
        (mint(KIND_SOLOC, "acme.com", "truck_A"), "70591c8b9b2b81edb6314748930a1713"),
        (mint(KIND_SOLOC, "acme.com", "cam"), "f5d55ca2261a81a0965c1100b928f306"),
        (astronomical(0, 1), "00000000000080008000000001000000"),
        (astronomical(399, 399), "0000018f00008000800000018f000000"),
        (astronomical(399, 1), "0000018f000080008000000001000000"),
        (astronomical(606, 1), "0000025e000080008000000001000000"),
        (mint(KIND_ABSTRACT, "acme.com", "pipeline_v3"), "6af4052e183f82d2b617092fb6da6fa5"),
    ]
    bad = [want for got, want in vectors if got.hex() != want]
    check("mint matches frozen_mint_vectors", not bad, f"mismatched: {bad}" if bad else "")

    t0 = tai_ns_from_utc(datetime(2026, 9, 1))

    # -- one row per timescale; append must store each as TAI at the same instant -----------
    ts_ids = {ts: mint(KIND_SOLOC, "sim.soloc", f"smoke_ts_{ts}") for ts in ("TAI", "UTC", "GPST", "TT")}
    for ts, eid in ts_ids.items():
        buf = client.buffer()
        buf.append(eid, EARTH, [6378.137, 0, 0], [1, 0, 0, 0], t0, timescale=ts)
        client.put(buf.flush())
    state = client.current_state(list(ts_ids.values()))
    tai_code = client.buffer().codes["timescale_id"]["TAI"]
    for ts, eid in ts_ids.items():
        stored = stored_tai_ns(state, eid)
        i = entity_ids(state).index(eid)
        ok = stored == [t0] and sts(state, i)["timescale_id"] == tai_code
        check(f"append {ts} row stored as TAI", ok,
              "" if ok else f"off by {(stored[0] - t0) / 1e9:+.3f} s")

    # -- (a) epoch base of the JSON epoch_tai_s ---------------------------------------------
    client.action("append_snapshot", {"bodies": [wire(EARTH)], "epoch_tai_s": t0 / 1e9})
    diff = stored_tai_ns(client.query_all(), EARTH)[0] - t0
    if abs(diff) < 10**6:
        base, snap_offset_s = "J2000", 0
    elif abs(diff + J1900_TO_J2000_NS) < 10**6:
        base, snap_offset_s = "J1900", J1900_TO_J2000_NS // 10**9
    else:
        base, snap_offset_s = None, 0
    check("(a) epoch_tai_s base identified", base is not None, f"stored - sent = {diff / 1e9:+.3f} s")
    if base:
        finding(f"epoch_tai_s is TAI seconds since {base}; send t_J2000_s + {snap_offset_s}")

    # -- (b) Mars 499 --------------------------------------------------------------------
    for bsp in sorted((HERE / "data").glob("*.bsp")):
        mars = sorted(s for s in spk_segments(bsp) if 499 in s)
        finding(f"{bsp.name} Mars 499 segments (target, center): {mars or 'none'}")
    try:
        msg = client.action("append_snapshot", {"bodies": [wire(EARTH), wire(MARS)],
                                                "epoch_tai_s": t0 / 1e9 + snap_offset_s})
        check("(b) Earth + Mars snapshot at t0", True, msg)
    except Exception as e:
        check("(b) Earth + Mars snapshot at t0", False, str(e))

    # -- query_orbits: each body about its NAIF parent, nothing stored -------------------------
    n = client.query_all().num_rows
    try:
        orbits = client.orbits([EARTH, MOON], t0)
        centres = id_bytes(orbits.column("centre_id"))
        days = [p / DAY_S for p in orbits.column("period_s").to_pylist()]
        ok = (centres == [SUN, EARTH] and abs(days[0] - 365.25) < 1 and abs(days[1] - 27.3) < 1
              and client.query_all().num_rows == n)
        check("query_orbits Earth about the Sun, the Moon about Earth", ok,
              f"periods {days[0]:.2f} d, {days[1]:.2f} d")
    except Exception as e:
        check("query_orbits Earth about the Sun, the Moon about Earth", False,
              str(e).splitlines()[0])

    # -- (c) quaternion convention: parent rotated +90° about z, child 1 km along its x -----
    site = mint(KIND_SOLOC, "sim.soloc", "smoke_site")
    rover = mint(KIND_SOLOC, "sim.soloc", "smoke_rover")
    c45 = math.cos(math.pi / 4)
    buf = client.buffer()
    buf.append(site, EARTH, [7000, 0, 0], [c45, 0, 0, c45], t0)
    client.put(buf.flush())
    buf.append(rover, site, [1000, 0, 0], [1, 0, 0, 0], t0, units="m")
    client.put(buf.flush())
    try:
        out = client.exchange(client.current_state([rover]), "IAU_EARTH")
        pos, quat = sts(out)["position"], sts(out)["quaternion"]
    except Exception as e:
        check("(c) do_exchange", False, str(e).splitlines()[0])
        pos, quat = [], []
    near = lambda a, b: len(a) == len(b) and all(abs(x - y) < 1e-6 for x, y in zip(a, b))
    if near(pos, [7000, 1, 0]):
        conv = "q rotates child-frame vectors into the parent frame (child→parent)"
    elif near(pos, [7000, -1, 0]):
        conv = "q rotates parent-frame vectors into the child frame (parent→child)"
    else:
        conv = None
    check("(c) quaternion convention identified", conv is not None, f"rover in IAU_EARTH = {pos} km")
    if conv:
        finding(conv)
    check("(c) composed quaternion is the parent's", near([abs(x) for x in quat], [c45, 0, 0, c45]),
          f"{quat}")

    # -- save_ledger -> load_ledger round trip ---------------------------------------------
    path = HERE / "out" / "smoke.arrow"
    path.parent.mkdir(exist_ok=True)
    n = client.query_all().num_rows
    print("      " + client.action("save_ledger", {"path": str(path)}))
    print("      " + client.action("load_ledger", {"path": str(path)}))
    names_file = path.with_name(path.name + ".names.arrow")
    check("save_ledger -> load_ledger round trip", client.query_all().num_rows == n and names_file.exists(),
          f"{n} rows, names sibling {'present' if names_file.exists() else 'missing'}")

    print(f"\n{results.count(True)}/{len(results)} passed")
    sys.exit(0 if all(results) else 1)


if __name__ == "__main__":
    main()
