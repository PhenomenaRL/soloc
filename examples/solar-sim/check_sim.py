"""Reloads a saved sim ledger into the server and checks it. One PASS/FAIL line per check;
exits non-zero on any FAIL. Only fleets present in the file are checked.

Loading replaces the server's in-memory ledger, so this can run right after run_sim.py.
"""

import argparse
import math
import sys
from collections import Counter
from pathlib import Path

import numpy as np
import pyarrow as pa

import scenario as sc
from geo import fixed_to_geodetic
from run_sim import roster
from soloc_client import (CENTURY_NS, SolocClient, id_bytes, positions, sts_field,
                          tai_ns_from_utc, vocabulary)

GROUND_TOLERANCE_M = 1.0
SPOT_CHECK_HOURS = (0, 24, 48, 72)

results: list[bool] = []


def check(name: str, ok: bool, detail: str = ""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  ({detail})" if detail else ""))


def epochs_ns(table: pa.Table) -> np.ndarray:
    """Stored TAI ns since J2000. The sim window is inside the first century, so int64 holds it."""
    c = sts_field(table, "duration_centuries").to_numpy().astype(np.int64)
    return c * CENTURY_NS + sts_field(table, "duration_ns").to_numpy().astype(np.int64)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("path")
    p.add_argument("--server", default="grpc://localhost:50051")
    p.add_argument("--seed", type=int, default=sc.SEED)
    args = p.parse_args()

    client = SolocClient(args.server)
    path = Path(args.path).resolve()
    try:
        msg = client.action("load_ledger", {"path": str(path)})
        check("load_ledger", True, msg)
    except Exception as e:
        check("load_ledger", False, str(e).splitlines()[0])
        sys.exit(1)

    facilities, robots = roster(args.seed)
    fac_by_id = {f.id: f for f in facilities}
    robot_by_id = {r.id: r for r in robots}
    body_by_id = {b.frame_id: b for b in sc.SNAPSHOT_BODIES}

    rows = client.query_all()
    ids = np.array(id_bytes(rows.column("entity_id")), dtype=object)
    t_ns = epochs_ns(rows)
    t0_ns = tai_ns_from_utc(sc.T0)
    present = set(ids)
    fleets = {name: {i for i in members if i in present} for name, members in
              (("facilities", fac_by_id), ("robots", robot_by_id), ("bodies", body_by_id))}
    print(f"      {rows.num_rows:,} rows; fleets present: "
          + ", ".join(f"{k} {len(v)}" for k, v in fleets.items() if v))

    # -- roster ---------------------------------------------------------------------------
    state = client.current_state()
    state_ids = set(id_bytes(state.column("entity_id")))
    for name, members in (("facilities", fac_by_id), ("robots", robot_by_id), ("bodies", body_by_id)):
        if fleets[name]:
            got = len(state_ids & set(members))
            check(f"current_state has all {len(members)} {name}", got == len(members), f"{got}")
    unknown = state_ids - set(fac_by_id) - set(robot_by_id) - set(body_by_id)
    check("current_state has no unknown entities", not unknown, f"{len(unknown)} unknown")

    names = client.names()
    unnamed = [i for i in present if i not in body_by_id and i not in names]
    check("names registry reloaded for every sim entity", not unnamed, f"{len(names)} names")

    # -- vocabulary -----------------------------------------------------------------------
    sts_type = client.schema.field("spacetimestamp").type
    units = {v: k for k, v in vocabulary(sts_type.field("units_pos")).items()}
    scales = {v: k for k, v in vocabulary(sts_type.field("timescale_id")).items()}
    unit_counts = Counter(units[u] for u in sts_field(rows, "units_pos").to_numpy())
    check("only km/m stored", set(unit_counts) <= {"km", "m"}, dict(unit_counts))
    scale_counts = Counter(scales[s] for s in sts_field(rows, "timescale_id").to_numpy())
    check("every row normalised to TAI", set(scale_counts) == {"TAI"}, dict(scale_counts))

    # -- cadence --------------------------------------------------------------------------
    per_entity = Counter(ids)
    expected = {**{i: sc.FACILITY_CADENCE_S for i in fac_by_id},
                **{i: sc.ROBOT_CADENCE_S for i in robot_by_id},
                **{i: sc.SNAPSHOT_S for i in body_by_id}}
    bad = [i for i in present if i in expected
           and per_entity[i] != sc.DURATION_S // expected[i] + 1]
    check("rows per entity match cadence (t0 → t_end inclusive)", not bad,
          f"{len(bad)} off" if bad else "facilities/bodies 73, robots 8,641")
    offgrid = [i for i in present if i in expected and i not in body_by_id
               and np.any((t_ns[ids == i] - t0_ns) % (expected[i] * 10**9))]
    check("entity epochs sit exactly on their cadence grid", not offgrid, f"{len(offgrid)} off")

    # -- robots ---------------------------------------------------------------------------
    if fleets["robots"]:
        is_robot = np.array([i in robot_by_id for i in ids])
        r_rows = rows.filter(pa.array(is_robot))
        r_ids = ids[is_robot]
        frames = np.array(id_bytes(sts_field(r_rows, "frame_id")), dtype=object)
        wrong_host = int(sum(f != robot_by_id[i].host_id for i, f in zip(r_ids, frames)))
        check("every robot row is framed on its facility", wrong_host == 0, f"{wrong_host} wrong")

        pos = positions(r_rows)
        worst_xy = float(np.abs(pos[:, :2]).max())
        worst_z = float(np.abs(pos[:, 2]).max())
        check("robots stay inside their site area",
              worst_xy <= sc.SITE_HALF_WIDTH_M and worst_z == 0.0,
              f"max |x|,|y| = {worst_xy:.1f} m of {sc.SITE_HALF_WIDTH_M:.0f}, max |z| = {worst_z} m")

        spot_check_robots(client, rows, ids, t_ns, t0_ns, r_rows, r_ids, fac_by_id, robot_by_id,
                          body_by_id, fleets)

    print(f"\n{results.count(True)}/{len(results)} passed")
    sys.exit(0 if all(results) else 1)


def spot_check_robots(client, rows, ids, t_ns, t0_ns, r_rows, r_ids, fac_by_id, robot_by_id,
                      body_by_id, fleets):
    """Resolves robot rows at a few epochs through their facility to the body, and to ICRF."""
    spot_ns = [t0_ns + h * 3600 * 10**9 for h in SPOT_CHECK_HOURS]
    r_t = epochs_ns(r_rows)
    by_body: dict[str, list[int]] = {}
    for k, (i, t) in enumerate(zip(r_ids, r_t)):
        if t in spot_ns:
            body = fac_by_id[robot_by_id[i].host_id].spec.body
            by_body.setdefault(body.name, []).append(k)

    snap = {}   # (body, rounded second) -> ICRF km, from the hourly snapshots
    if fleets["bodies"]:
        b_mask = np.array([i in body_by_id for i in ids])
        b_pos = positions(rows.filter(pa.array(b_mask)))
        for i, t, xyz in zip(ids[b_mask], t_ns[b_mask], b_pos):
            snap[(body_by_id[i].name, round(t / 1e9))] = xyz

    for body_name, idx in sorted(by_body.items()):
        body = next(b for b in sc.SNAPSHOT_BODIES if b.name == body_name)
        sample = r_rows.take(pa.array(idx))
        s_ids, s_t = r_ids[idx], r_t[idx]

        fixed = positions(client.exchange(sample, body.frame))
        _, _, h_km = fixed_to_geodetic(body, fixed)
        worst_h = float(np.abs(h_km).max() * 1000)
        check(f"{body_name} robots at ground level ({len(idx)} rows via {body.frame})",
              worst_h <= GROUND_TOLERANCE_M,
              f"|r| {np.linalg.norm(fixed, axis=1).min():,.3f}–"
              f"{np.linalg.norm(fixed, axis=1).max():,.3f} km, max |h| = {worst_h * 1000:.1f} mm")

        site = np.array([fac_by_id[robot_by_id[i].host_id].position_km for i in s_ids])
        worst_d = float(np.linalg.norm(fixed - site, axis=1).max() * 1000)
        check(f"{body_name} robots within their site radius", worst_d <= sc.SITE_HALF_WIDTH_M * math.sqrt(2),
              f"max {worst_d:.1f} m from the facility origin")

        if not fleets["bodies"]:
            continue
        icrf = positions(client.exchange(sample, "ICRF"))
        err = [abs(np.linalg.norm(p - snap[(body_name, round(t / 1e9))]) - np.linalg.norm(f)) * 1e6
               for p, t, f in zip(icrf, s_t, fixed)]
        check(f"{body_name} robots in ICRF sit on the snapshot body's surface",
              max(err) <= GROUND_TOLERANCE_M * 1000, f"max {max(err):.1f} mm off")


if __name__ == "__main__":
    main()
