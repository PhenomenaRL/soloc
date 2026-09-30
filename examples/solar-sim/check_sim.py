"""Reloads a saved sim ledger into the server and checks it. One PASS/FAIL line per check;
exits non-zero on any FAIL. Only fleets present in the file are checked.

Loading replaces the server's in-memory ledger, so this can run right after run_sim.py.
"""

import argparse
import math
import sys
from collections import Counter
from datetime import timedelta
from pathlib import Path

import numpy as np
import pyarrow as pa

import scenario as sc
from geo import fixed_to_geodetic
from models.robot import HULL_REACH_M
from models.spacecraft import Orbit
from run_sim import roster
from soloc_client import (CENTURY_NS, SolocClient, id_bytes, matches, positions, sts_field,
                          tai_ns_from_utc, vocabulary)
from topo_sim import events_from_rows

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


def utc(t_s: float) -> str:
    return (sc.T0 + timedelta(seconds=t_s)).strftime("%m-%d %H:%M:%S")


class Data:
    """The reloaded rows, indexed per entity."""

    def __init__(self, client: SolocClient):
        self.client = client
        self.rows = client.query_all()
        self.ids = np.array(id_bytes(self.rows.column("entity_id")), dtype=object)
        self.frames = np.array(id_bytes(sts_field(self.rows, "frame_id")), dtype=object)
        self.t0_ns = tai_ns_from_utc(sc.T0)
        self.t_ns = epochs_ns(self.rows)
        self.t_s = (self.t_ns - self.t0_ns) / 1e9
        self.pos = positions(self.rows)
        code: dict[bytes, int] = {}
        codes = np.fromiter((code.setdefault(i, len(code)) for i in self.ids), np.int64, len(self.ids))
        self.index = {i: np.flatnonzero(codes == c) for i, c in code.items()}

    def of(self, entity_id: bytes, t0_s=-math.inf, t1_s=math.inf) -> np.ndarray:
        """Row indices of an entity with `t0_s <= t < t1_s`, in stored order."""
        idx = self.index.get(entity_id, np.array([], np.int64))
        return idx[(self.t_s[idx] >= t0_s) & (self.t_s[idx] < t1_s)]

    def at(self, entity_id: bytes, t_s: float) -> int:
        [k] = self.of(entity_id, t_s, t_s + 1e-6)
        return int(k)

    def resolve(self, idx, frame: str) -> np.ndarray:
        """Positions (km) of the given rows, exchanged into `frame` through the ledger."""
        return positions(self.client.exchange(self.rows.take(pa.array(np.asarray(idx))), frame))


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

    world = roster(args.seed)
    body_by_id = {b.frame_id: b for b in sc.SNAPSHOT_BODIES}
    fleet_models = {"facilities": world.facilities, "spacecraft": world.spacecraft,
                    "robots": world.robots, "crawlers": world.crawlers}
    by_id = {e.id: e for e in world.entities}

    d = Data(client)
    present = set(d.index)
    fleets = {name: {e.id for e in members if e.id in present} for name, members in fleet_models.items()}
    fleets["bodies"] = {i for i in body_by_id if i in present}
    print(f"      {d.rows.num_rows:,} rows; fleets present: "
          + ", ".join(f"{k} {len(v)}" for k, v in fleets.items() if v))

    # -- roster ---------------------------------------------------------------------------
    state = client.current_state()
    state_ids = set(id_bytes(state.column("entity_id")))
    members = {**{k: {e.id for e in v} for k, v in fleet_models.items()}, "bodies": set(body_by_id)}
    for name, ids in members.items():
        if fleets[name]:
            got = len(state_ids & ids)
            check(f"current_state has all {len(ids)} {name}", got == len(ids), f"{got}")
    unknown = state_ids - set(by_id) - set(body_by_id)
    check("current_state has no unknown entities", not unknown, f"{len(unknown)} unknown")

    names = client.names()
    unnamed = [i for i in present if i not in body_by_id and i not in names]
    check("names registry reloaded for every sim entity", not unnamed, f"{len(names)} names")

    # -- vocabulary -----------------------------------------------------------------------
    sts_type = client.schema.field("spacetimestamp").type
    units = {v: k for k, v in vocabulary(sts_type.field("units_pos")).items()}
    scales = {v: k for k, v in vocabulary(sts_type.field("timescale_id")).items()}
    unit_counts = Counter(units[u] for u in sts_field(d.rows, "units_pos").to_numpy())
    check("only km/m stored", set(unit_counts) <= {"km", "m"}, dict(unit_counts))
    scale_counts = Counter(scales[s] for s in sts_field(d.rows, "timescale_id").to_numpy())
    check("every row normalised to TAI", set(scale_counts) == {"TAI"}, dict(scale_counts))

    # -- schedule -------------------------------------------------------------------------
    grid = range(0, sc.DURATION_S + 1, sc.BASE_TICK_S)
    wrong_count, off_schedule = [], []
    for i in present:
        if i in body_by_id:
            expected = np.arange(0, sc.DURATION_S + 1, sc.SNAPSHOT_S)
        else:
            expected = np.array([t for t in grid if by_id[i].due(t)])
        got = np.sort(d.t_s[d.index[i]])
        if len(got) != len(expected):
            wrong_count.append(i)
        elif np.any(got != expected):
            off_schedule.append(i)
    check("rows per entity match its schedule (t0 → t_end inclusive)", not wrong_count,
          f"{len(wrong_count)} off" if wrong_count else f"{len(present)} entities")
    check("every epoch sits exactly on its entity's schedule", not off_schedule,
          f"{len(off_schedule)} off")

    # Zero-order hold: a row framed on a moving entity resolves through that entity's latest
    # pose at or before it, which is only exact when the parent has a row at the same epoch.
    craft_ids = {c.id for c in world.spacecraft}
    on_craft = np.flatnonzero([f in craft_ids for f in d.frames])
    if len(on_craft):
        parent_epochs = {c: set(d.t_ns[d.index[c]].tolist()) for c in craft_ids if c in d.index}
        missing = sum(int(d.t_ns[k]) not in parent_epochs.get(d.frames[k], ()) for k in on_craft)
        check("every row on a spacecraft shares its epoch with a row of that spacecraft",
              missing == 0, f"{len(on_craft):,} rows, {missing} without a parent row")

    if fleets["robots"]:
        check_robots(d, world, fleets)
    if fleets["spacecraft"]:
        check_spacecraft(d, world)
    if fleets["crawlers"]:
        check_crawlers(d, world)
    if fleets["spacecraft"] or fleets["crawlers"]:
        check_topology(d, world)

    print(f"\n{results.count(True)}/{len(results)} passed")
    sys.exit(0 if all(results) else 1)


# -- robots -----------------------------------------------------------------------------------


def check_robots(d: Data, world, fleets):
    fac_by_id = {f.id: f for f in world.facilities}
    robot_by_id = {r.id: r for r in world.robots}
    idx = np.concatenate([d.index[i] for i in fleets["robots"]])
    wrong_host = int(sum(d.frames[k] != robot_by_id[d.ids[k]].host_id for k in idx))
    check("every robot row is framed on its facility", wrong_host == 0, f"{wrong_host} wrong")

    worst_xy = float(np.abs(d.pos[idx, :2]).max())
    worst_z = float(np.abs(d.pos[idx, 2]).max())
    check("robots stay inside their site area",
          worst_xy <= sc.SITE_HALF_WIDTH_M and worst_z == 0.0,
          f"max |x|,|y| = {worst_xy:.1f} m of {sc.SITE_HALF_WIDTH_M:.0f}, max |z| = {worst_z} m")

    # Resolving goes through the ledger at historical epochs, one frame-chain scan per
    # (frame, epoch), so only a few epochs are resolved rather than every row.
    by_facility: dict[bytes, list[int]] = {}
    for h in SPOT_CHECK_HOURS:
        for i in fleets["robots"]:
            by_facility.setdefault(robot_by_id[i].host_id, []).append(d.at(i, h * 3600))
    by_body: dict = {}
    for fid, ks in by_facility.items():
        by_body.setdefault(fac_by_id[fid].spec.body, []).extend(ks)

    snap = snapshots(d)
    for body, ks in sorted(by_body.items(), key=lambda kv: kv[0].name):
        fixed = d.resolve(ks, body.frame)
        _, _, h_km = fixed_to_geodetic(body, fixed)
        worst_h = float(np.abs(h_km).max() * 1000)
        r = np.linalg.norm(fixed, axis=1)
        check(f"{body.name} robots at ground level ({len(ks)} rows via {body.frame})",
              worst_h <= GROUND_TOLERANCE_M,
              f"|r| {r.min():,.3f}–{r.max():,.3f} km, max |h| = {worst_h * 1000:.1f} mm")

        site = np.array([fac_by_id[robot_by_id[d.ids[k]].host_id].position_km for k in ks])
        worst_d = float(np.linalg.norm(fixed - site, axis=1).max() * 1000)
        check(f"{body.name} robots within their site radius",
              worst_d <= sc.SITE_HALF_WIDTH_M * math.sqrt(2),
              f"max {worst_d:.1f} m from the facility origin")

        if snap:
            icrf = d.resolve(ks, "ICRF")
            err = [abs(np.linalg.norm(p - snap[(body.name, round(d.t_s[k]))]) - np.linalg.norm(f)) * 1e6
                   for p, k, f in zip(icrf, ks, fixed)]
            check(f"{body.name} robots in ICRF sit on the snapshot body's surface",
                  max(err) <= GROUND_TOLERANCE_M * 1000, f"max {max(err):.1f} mm off")


def snapshots(d: Data) -> dict:
    """`(body name, whole seconds since T0)` → ICRF km, from the hourly snapshots."""
    out = {}
    for b in sc.SNAPSHOT_BODIES:
        for k in d.index.get(b.frame_id, []):
            out[(b.name, round(d.t_s[k]))] = d.pos[k]
    return out


# -- spacecraft -------------------------------------------------------------------------------


def nodal_period_s(t_s: np.ndarray, z: np.ndarray) -> float | None:
    """Mean time between ascending-node crossings (z going − → +), linearly interpolated.
    z is the same in the frozen inertial frame and the live IAU frame (the spin is about z)."""
    k = np.flatnonzero((z[:-1] < 0) & (z[1:] >= 0))
    if len(k) < 2:
        return None
    crossings = t_s[k] - z[k] * (t_s[k + 1] - t_s[k]) / (z[k + 1] - z[k])
    return float(np.diff(crossings).mean())


def check_spacecraft(d: Data, world):
    for craft in world.spacecraft:
        if craft.id not in d.index:
            continue
        ends = [*craft.starts[1:], math.inf]
        for phase, end in zip(craft.phases, ends):
            # The coast after a deorbit is half an ellipse; only full orbits are checked here.
            if isinstance(phase, Orbit) and phase.cadence_s is None:
                check_orbit(d, craft, phase, end)
        if "liftoff" in craft.events:
            check_launch(d, craft)
        if "touchdown" in craft.events:
            check_landing(d, craft)


def check_orbit(d: Data, craft, phase: Orbit, end_s: float):
    k = phase.kepler
    idx = d.of(craft.id, phase.start_s, end_s)
    idx = idx[np.argsort(d.t_s[idx])]
    framed = bool(matches(d.frames[idx], k.body.frame_id).all())
    r = np.linalg.norm(d.pos[idx], axis=1)
    rp, ra = k.radius_range_km
    radius_ok = r.min() >= rp - 1e-3 and r.max() <= ra + 1e-3
    detail = (f"{len(idx):,} rows on {k.body.frame}, |r| {r.min():,.3f}–{r.max():,.3f} km "
              f"of Kepler {rp:,.3f}–{ra:,.3f}")
    if k.i_deg == 0:
        # Equatorial: no node to time. A synchronous one should hang still in the body frame.
        drift = float(np.linalg.norm(d.pos[idx] - d.pos[idx[0]], axis=1).max())
        check(f"{craft.name} orbit", framed and radius_ok and drift < 1.0,
              f"{detail}, drifts {drift * 1000:.1f} m in {k.body.frame} over {utc(phase.start_s)} → end")
        return
    period = nodal_period_s(d.t_s[idx], d.pos[idx, 2])
    period_ok = period is not None and abs(period - k.period_s) < 1.0
    check(f"{craft.name} orbit", framed and radius_ok and period_ok,
          f"{detail}, nodal period "
          + (f"{period / 60:.3f} min of Kepler {k.period_s / 60:.3f}" if period else "unmeasured"))


def check_launch(d: Data, craft):
    t_lift, t_ins = craft.events["liftoff"], craft.events["insertion"]
    fac, body = craft.facility, craft.facility.spec.body
    pad = d.of(craft.id, -math.inf, t_lift)
    after = d.of(craft.id, t_lift)
    framed = (matches(d.frames[pad], fac.id).all() and np.all(d.pos[pad] == 0)
              and matches(d.frames[after], body.frame_id).all())
    spot = [d.at(craft.id, 0), int(pad[np.argmax(d.t_s[pad])])]
    miss = float(np.linalg.norm(d.resolve(spot, body.frame) - fac.position_km, axis=1).max() * 1e6)
    check(f"{craft.name} on {fac.name} until liftoff {utc(t_lift)}, then on {body.frame}",
          framed and miss <= 1.0,
          f"{len(pad)} pad rows at the facility origin, resolved within {miss:.3f} mm of the pad")

    ascent = d.of(craft.id, t_lift, t_ins + 1)
    ascent = ascent[np.argsort(d.t_s[ascent])]
    _, _, h = fixed_to_geodetic(body, d.pos[ascent])
    check(f"{craft.name} ascent climbs monotonically", bool(np.all(np.diff(h) >= 0)),
          f"{len(ascent)} rows over {(t_ins - t_lift) / 60:.0f} min, {h[0] * 1000:.1f} m → {h[-1]:.1f} km")


def check_landing(d: Data, craft):
    t_deorbit, t_down = craft.events["deorbit"], craft.events["touchdown"]
    fac, body = craft.facility, craft.facility.spec.body
    descent = d.of(craft.id, t_deorbit, t_down)
    alt = np.linalg.norm(d.pos[descent], axis=1) - np.linalg.norm(fac.position_km)
    check(f"{craft.name} descent stays above {fac.name}'s ground level",
          bool(matches(d.frames[descent], body.frame_id).all()) and alt.min() >= -1e-6,
          f"{len(descent)} rows {utc(t_deorbit)} → {utc(t_down)}, min {alt.min() * 1000:.3f} m")

    landed = d.of(craft.id, t_down)
    spot = [d.at(craft.id, t) for t in (t_down, t_down + 3600, sc.DURATION_S)]
    miss = float(np.linalg.norm(d.resolve(spot, body.frame) - fac.position_km, axis=1).max() * 1000)
    check(f"{craft.name} on {fac.name} from touchdown {utc(t_down)}",
          bool(matches(d.frames[landed], fac.id).all()) and miss <= GROUND_TOLERANCE_M,
          f"{len(landed):,} rows, resolved within {miss * 1000:.3f} mm of the site at 3 epochs")


# -- crawlers ---------------------------------------------------------------------------------


def check_crawlers(d: Data, world):
    on_hull = [(c, d.of(c.id, -math.inf, c.disembark[1] if c.disembark else math.inf))
               for c in world.crawlers if c.id in d.index]
    framed = all(matches(d.frames[idx], c.host.id).all() for c, idx in on_hull)
    reach = max(float(np.linalg.norm(d.pos[idx], axis=1).max()) for _, idx in on_hull)
    check("crawlers framed on their host, on its hull", framed and reach <= HULL_REACH_M + 1e-9,
          f"max {reach:.3f} m from the host origin, hull reach {HULL_REACH_M:.3f} m")

    # Resolve crawler rows and their host's rows at the same epochs into the host's body frame.
    pairs: dict = {}
    for c, _ in on_hull:
        body = c.host.body
        for h in SPOT_CHECK_HOURS:
            t = h * 3600
            if c.disembark and t >= c.disembark[1]:
                continue
            pairs.setdefault(body, []).append((d.at(c.id, t), d.at(c.host.id, t)))
    worst, n = 0.0, 0
    for body, ks in pairs.items():
        crawler = d.resolve([a for a, _ in ks], body.frame)
        host = d.resolve([b for _, b in ks], body.frame)
        worst = max(worst, float(np.linalg.norm(crawler - host, axis=1).max() * 1000))
        n += len(ks)
    check("crawlers resolve to within hull reach of their host", worst <= HULL_REACH_M + 1e-3,
          f"{n} rows at {'/'.join(map(str, SPOT_CHECK_HOURS))} h, max {worst:.3f} m")

    for c in world.crawlers:
        if not c.disembark or c.id not in d.index:
            continue
        fac, t_off = c.disembark[0], c.disembark[1]
        body = fac.spec.body
        ashore = d.of(c.id, t_off)
        xy = float(np.abs(d.pos[ashore, :2]).max())
        framed = bool(matches(d.frames[ashore], fac.id).all()) and np.all(d.pos[ashore, 2] == 0)
        spot = [d.at(c.id, t) for t in (t_off, sc.DURATION_S)]
        fixed = d.resolve(spot, body.frame)
        _, _, h = fixed_to_geodetic(body, fixed)
        dist = float(np.linalg.norm(fixed - fac.position_km, axis=1).max() * 1000)
        check(f"{c.name} disembarks onto {fac.name} at {utc(t_off)} and roves there",
              framed and xy <= sc.SITE_HALF_WIDTH_M and float(np.abs(h).max()) * 1000 <= GROUND_TOLERANCE_M
              and dist <= sc.SITE_HALF_WIDTH_M * math.sqrt(2),
              f"{len(ashore):,} rows, max |x|,|y| {xy:.1f} m; resolved via {fac.name} to "
              f"|h| ≤ {float(np.abs(h).max()) * 1e6:.1f} mm, ≤ {dist:.1f} m from the site")


# -- topology ---------------------------------------------------------------------------------


def check_topology(d: Data, world):
    events, _ = events_from_rows(d.rows)
    seen: dict[bytes, bytes] = {}
    changes = set()
    for child, parent, t in sorted(events, key=lambda e: e[2]):
        if child in seen:
            changes.add((child, seen[child], parent, t))
        seen[child] = parent

    s_ns = lambda t_s: d.t0_ns + int(round(t_s)) * 10**9
    expected = {}   # (child, old parent, new parent, TAI ns) → kind
    for craft in world.spacecraft:
        fac = craft.facility
        if "liftoff" in craft.events:
            expected[(craft.id, fac.id, craft.body.frame_id, s_ns(craft.events["liftoff"]))] = "launch"
        if "touchdown" in craft.events:
            expected[(craft.id, craft.body.frame_id, fac.id, s_ns(craft.events["touchdown"]))] = "landing"
    for c in world.crawlers:
        if c.disembark:
            expected[(c.id, c.host.id, c.disembark[0].id, s_ns(c.disembark[1]))] = "disembark"
    kinds = Counter(kind for e, kind in expected.items() if e[0] in d.index)
    expected = {e for e in expected if e[0] in d.index}
    check("parent changes are exactly the launches, landings and disembarks",
          changes == expected,
          ", ".join(f"{n} {k}" for k, n in sorted(kinds.items()))
          + ("" if changes == expected else
             f"; only in rows {len(changes - expected)}, only expected {len(expected - changes)}"))


if __name__ == "__main__":
    main()
