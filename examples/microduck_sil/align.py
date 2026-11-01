"""Adds duck-to-duck sightings to a recorded run and fits each duck's odometry frame into the
lookout's, then scores "where is duck b in duck a's frame?" against truth.

    ../sim_study_0/serve.sh                                   # a fresh server, in another terminal
    ../sim_study_0/.venv/bin/python align.py                  # out/microduck.arrow → out/microduck_aligned.arrow

Sightings are synthetic, from truth plus noise: a duck sees another inside its camera's view and
its ToF range, and BLE names who it is. Each is stored in the observer's odometry frame. A pair's
sightings, both ways, pair one physical point in two odometry frames, so a 2-D rigid fit gives one
frame in the other. Every `REFIT_S` the fits so far are chained onto the lookout and written as
`ESTIMATED` rows. Truth for the score goes through soloc: the ducks' truth rows exchanged to
IAU_EARTH, then taken into the observer's trunk frame.
"""

import argparse
import math
from collections import deque

import numpy as np

import scenario as sc
from export_viewer import framed_m
from record import rotate, wrap
from sim import scenario as sc0
from sim.geo import EARTH, quat_yaw
from soloc_client import SolocClient, positions, registry_ipc, sts_field, tai_ns_from_utc
from tools.export_viewer import Rows, quat_matrix


def yaws(q: np.ndarray) -> np.ndarray:
    return np.arctan2(2 * (q[:, 0] * q[:, 3] + q[:, 1] * q[:, 2]), 1 - 2 * (q[:, 2] ** 2 + q[:, 3] ** 2))


class Track:
    """A 2-D pose track `(x, y, yaw)`, interpolated in time."""

    def __init__(self, t, pos, quat):
        self.t, self.x, self.y, self.yaw = t, pos[:, 0], pos[:, 1], np.unwrap(yaws(quat))

    def at(self, t: float) -> tuple[float, float, float]:
        return (float(np.interp(t, self.t, self.x)), float(np.interp(t, self.t, self.y)),
                float(np.interp(t, self.t, self.yaw)))


# 2-D rigid transforms `(tx, ty, theta)`: a point p in the inner frame is R(theta) p + t outside.
def compose(a, b):
    x, y = rotate(b[0], b[1], a[2])
    return a[0] + x, a[1] + y, wrap(a[2] + b[2])


def invert(a):
    x, y = rotate(-a[0], -a[1], -a[2])
    return x, y, -a[2]


def fit(pa: np.ndarray, pb: np.ndarray):
    """The rigid transform taking points `pb` (frame B) onto `pa` (frame A), least squares."""
    ca, cb = pa.mean(0), pb.mean(0)
    a, b = pa - ca, pb - cb
    th = math.atan2(np.sum(b[:, 0] * a[:, 1] - b[:, 1] * a[:, 0]), np.sum(b[:, 0] * a[:, 0] + b[:, 1] * a[:, 1]))
    x, y = rotate(cb[0], cb[1], th)
    return ca[0] - x, ca[1] - y, th


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--server", default="grpc://localhost:50051")
    args = p.parse_args()

    client = SolocClient(args.server)
    if not client.is_empty():
        raise SystemExit("the server's ledger is not empty; restart serve.sh")
    print(client.action("load_ledger", {"path": str(sc.LEDGER)}))
    table = client.query_all()
    rows = Rows(table)
    rows.t_s = rows.t_s - (tai_ns_from_utc(sc.T0) - tai_ns_from_utc(sc0.T0)) / 1e9   # Rows counts from sim_study_0's T0
    site = sc.entity(sc.SITE.name)
    ducks = [d for d in sc.DUCKS if len(rows.of(sc.entity(d)))]
    if sc.LOOKOUT not in ducks:
        raise SystemExit(f"the run has no {sc.LOOKOUT}, the lookout")
    print(client.action("import_names", registry_ipc(sc.bindings(ducks))))
    truth, odom, boot = {}, {}, {}
    for d in ducks:
        did, oid = sc.entity(d), sc.entity(sc.odom_name(d))
        truth[d] = Track(*framed_m(rows, did, site))
        odom[d] = Track(*framed_m(rows, did, oid))
        _, bp, bq = framed_m(rows, oid, site)
        boot[d] = (bp[0, 0], bp[0, 1], float(yaws(bq)[0]))
    duration = float(min(tr.t[-1] for tr in truth.values()))
    t0_tai = tai_ns_from_utc(sc.T0)
    tai = lambda t: t0_tai + int(round(t * 1e9))

    # -- sightings: each pairs one point in the observer's odometry frame with the target's own ----
    rng = np.random.default_rng(sc.SEED + 1)
    buf = client.buffer()
    pairs = {}                         # (A, B) → [(t, point in A's odom frame, same point in B's)]
    half_fov = math.radians(sc.FOV_DEG / 2)
    for t in np.arange(0.0, duration, 1.0 / sc.SIGHT_HZ):
        for a in ducks:
            ax, ay, ayaw = truth[a].at(t)
            for b in ducks:
                if b == a:
                    continue
                bx, by, _ = truth[b].at(t)
                r = math.hypot(bx - ax, by - ay)
                beta = wrap(math.atan2(by - ay, bx - ax) - ayaw)
                if r > sc.SIGHT_RANGE_M or abs(beta) > half_fov:
                    continue
                r += rng.normal(0, sc.RANGE_SIGMA_M)
                beta += rng.normal(0, math.radians(sc.BEARING_SIGMA_DEG))
                ox, oy, oyaw = odom[a].at(t)
                seen = (ox + r * math.cos(oyaw + beta), oy + r * math.sin(oyaw + beta))
                buf.append(sc.entity(b), sc.entity(sc.odom_name(a)), [*seen, 0.0], [1.0, 0.0, 0.0, 0.0], tai(t),
                           units="m", source_id=sc.source(sc.source_name(a)), estimate="MEASURED")
                own = odom[b].at(t)[:2]
                key, pt = ((a, b), (t, seen, own)) if a < b else ((b, a), (t, own, seen))
                pairs.setdefault(key, []).append(pt)
    n_sight = len(buf)
    print(f"{n_sight} sightings: " + ", ".join(f"{a[-1]}–{b[-1]} {len(v)}" for (a, b), v in sorted(pairs.items())))

    # -- refits, chained onto the lookout ----------------------------------------------------------
    history = {d: [] for d in ducks if d != sc.LOOKOUT}     # [(t, T lookout ← d)]
    for tr in np.arange(sc.REFIT_S, duration + 1e-9, sc.REFIT_S):
        edges = {}
        for (a, b), pts in pairs.items():
            pts = [x for x in pts if x[0] <= tr]
            if len(pts) < sc.MIN_SIGHTINGS:
                continue
            pa, pb = np.array([x[1] for x in pts]), np.array([x[2] for x in pts])
            if 2 * np.linalg.norm(pa - pa.mean(0), axis=1).max() < sc.MIN_SPREAD_M:
                continue
            edges[(a, b)] = fit(pa, pb)                     # T a ← b
            edges[(b, a)] = invert(edges[(a, b)])
        est, queue = {sc.LOOKOUT: (0.0, 0.0, 0.0)}, deque([sc.LOOKOUT])
        while queue:                                         # breadth first: direct links win
            cur = queue.popleft()
            for (a, b), T in edges.items():
                if a == cur and b not in est:
                    est[b] = compose(est[cur], T)
                    queue.append(b)
        for d, T in est.items():
            if d == sc.LOOKOUT:
                continue
            history[d].append((tr, T))
            buf.append(sc.entity(sc.estimate_name(d)), sc.entity(sc.odom_name(sc.LOOKOUT)), [T[0], T[1], 0.0],
                       quat_yaw(T[2]), tai(tr), units="m", source_id=sc.source(sc.source_name(sc.LOOKOUT)),
                       estimate="ESTIMATED")
    print(f"{len(buf) - n_sight} alignment estimates")
    client.put(buf.flush())

    # -- the final alignment against truth (lookout's boot frame⁻¹ ∘ the duck's) -------------------
    for d, hist in history.items():
        if not hist:
            print(f"  {d}: never aligned (too few sightings, or too little spread)")
            continue
        t_first, (tx, ty, th) = hist[0][0], hist[-1][1]
        gx, gy, gth = compose(invert(boot[sc.LOOKOUT]), boot[d])
        print(f"  {d} in {sc.LOOKOUT}'s odometry frame: first at {t_first:.0f} s; final error "
              f"{math.hypot(tx - gx, ty - gy) * 100:.1f} cm, {math.degrees(wrap(th - gth)):+.2f}°")

    # -- "where is b in a's frame?": estimate vs truth through soloc --------------------------------
    times = np.arange(sc.EVAL_EVERY_S, duration, sc.EVAL_EVERY_S)
    picks = {}
    for d in ducks:
        k = rows.of(sc.entity(d))
        k = k[np.array([f == site for f in rows.frames[k]], bool)]
        picks[d] = k[np.clip(np.searchsorted(rows.t_s[k], times), 0, len(k) - 1)]
    order = [d for d in ducks]
    out = client.exchange(table.take(np.concatenate([picks[d] for d in order])), EARTH.frame)
    p_km = positions(out).reshape(len(order), len(times), 3)
    q = sts_field(out, "quaternion").flatten().to_numpy().reshape(len(order), len(times), 4)
    latest = lambda d, t: (0.0, 0.0, 0.0) if d == sc.LOOKOUT else next(
        (T for tt, T in reversed(history[d]) if tt <= t), None)
    print(f"b in a's trunk frame, every {sc.EVAL_EVERY_S:.0f} s ({len(order) * len(times)} truth rows exchanged):")
    for i, a in enumerate(order):
        for j, b in enumerate(order):
            if a == b:
                continue
            errs = []
            for n, t in enumerate(times):
                ta, tb = latest(a, t), latest(b, t)
                if ta is None or tb is None:
                    continue
                oa, ob = odom[a].at(t), odom[b].at(t)
                est = compose(invert(oa), compose(compose(invert(ta), tb), (ob[0], ob[1], 0.0)))[:2]
                rel = quat_matrix(q[i, n]).T @ (p_km[j, n] - p_km[i, n]) * 1000
                errs.append(math.hypot(est[0] - rel[0], est[1] - rel[1]))
            if errs:
                print(f"  {b} in {a}'s frame: {len(errs)} samples, median {np.median(errs) * 100:.1f} cm, "
                      f"max {max(errs) * 100:.1f} cm")
            else:
                print(f"  {b} in {a}'s frame: no estimate")
    print(client.action("save_ledger", {"path": str(sc.ALIGNED)}))


if __name__ == "__main__":
    main()
