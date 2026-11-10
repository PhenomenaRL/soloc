"""Sweeps duck-to-duck detector designs over the recorded runs, after the fact. Each recording is
replayed through Pollen's apartment scene (every duck posed from the joint sidecar) to find, for
each moment and pair, the true range and bearing, a clear line of sight, and whether the observer's
ToF (Pollen's VL53L5CX model, its zone rays) lands on the other duck. Then, per design and noise
seed: heading drift on each duck's odometry, the design's sightings, and each duck's odometry frame
fitted into the lookout's. Seed 0 of each design goes to its own ledger (run 2's under
out/sweep/run2/), all of a run into one merged ledger; scores over every seed and run to plots.

    ../sim_study_0/.venv/bin/python sweep.py                                  # every design
    ../sim_study_0/.venv/bin/python sweep.py --designs cam-ble-base ble-base  # just these

Don't run serve.sh yourself: this starts and stops it, once per design (the server has no clear).
"""

import argparse
import math
import os
import signal
import socket
import subprocess
import time
import zlib
from collections import deque
from contextlib import contextmanager

import matplotlib
import mujoco
import numpy as np
import pyarrow as pa
import pyarrow.compute as pc

import apartment as ap
import scenario as sc
from align import Track, compose, fit, invert, yaws
from export_viewer import framed_m
from mjlab_microduck.sim.body_server import JOINT_NAMES, build_world, duck_prefix
from mjlab_microduck.sim.tof import MAX_RANGE, Tof
from record import rotate, wrap
from sim import scenario as sc0
from sim.geo import EARTH, quat_yaw
from sim.models.facility import Facility
from sim.scenario import AUTHORITY
from soloc_client import KIND_ABSTRACT, KIND_SOLOC, RowBuffer, SolocClient, positions, registry_ipc, tai_ns_from_utc
from tools.export_viewer import Rows, quat_matrix
from tools.plot_sim import SERIES, style
from tools.view_sim import load

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

SERVE = sc.SIM_STUDY_0 / "serve.sh"
SERVER, PORT = "grpc://localhost:50051", 50051
TOF_CONE_RAD = math.radians(35)       # only cast the ToF's zones when the target is near its axis
MAX_SENSE_M = 8.0
MAX_OFFSET_M = 20.0                   # no two boot frames in an 8×6 m flat are further apart
FIT_SIGMA_M, FIT_SIGMA_RAD = 0.10, math.radians(5.0)    # a pair fit's own 1σ, to be accepted
AMBIGUOUS_M, AMBIGUOUS_RAD, AMBIGUOUS_CHI2 = 0.3, math.radians(10.0), 9.0   # a rival minimum this far off and this close in χ²
GROWTH_MAX_S = 180.0                  # error-growth fit over gaps up to this
PLOT_DPI = 260                        # 2× for docs/img
SIGHTINGS_SCHEMA = pa.schema([("t_tai_ns", pa.int64()), ("observer", pa.string()), ("target", pa.string()),
                              ("range_m", pa.float64()), ("bearing_rad", pa.float64())])


def designs() -> dict[str, dict]:
    """Every design id → its settings: each baseline (`<design>-base`) and one axis varied at a time."""
    out = {f"{d}-base": dict(p, base=d, axis=None) for d, p in sc.DESIGNS.items()}
    for axis, per in sc.SWEEP_AXES.items():
        for d, (key, values, label) in per.items():
            for v in values:
                if v != sc.DESIGNS[d][key]:
                    out[f"{d}-{label}{v:g}"] = dict(sc.DESIGNS[d], **{key: v}, base=d, axis=axis)
    return out


def track_of(t, xy, yaw) -> Track:
    tr = Track.__new__(Track)
    tr.t, tr.x, tr.y, tr.yaw = t, xy[:, 0], xy[:, 1], np.unwrap(yaw)
    return tr


def drifted(t, p, yaw, sign: float, deg_per_min: float):
    """Odometry under a constant gyro bias: heading off by bias·t, each step turned by it."""
    dth = sign * math.radians(deg_per_min) / 60.0 * (t - t[0])
    dp = np.diff(p[:, :2], axis=0)
    c, s = np.cos(dth[1:]), np.sin(dth[1:])
    xy = np.vstack([p[:1, :2], p[0, :2] + np.cumsum(np.column_stack([c * dp[:, 0] - s * dp[:, 1],
                                                                     s * dp[:, 0] + c * dp[:, 1]]), axis=0)])
    return np.column_stack([xy, p[:, 2]]), yaw + dth


# -- the replay ------------------------------------------------------------------------------------

def replay(ducks: list[str], times: np.ndarray):
    """`(R, B, LOS, TOF)`, each `[time, observer, target]`: range (inf beyond `MAX_SENSE_M`), bearing
    in the observer's trunk frame, a clear line from its head to the target's trunk, and whether one
    of its ToF zones' rays hits the target first."""
    model = build_world(ap.SCENE, len(ducks))
    data = mujoco.MjData(model)
    side = pa.ipc.open_file(sc.JOINTS).read_all()
    t0 = tai_ns_from_utc(sc.T0)
    jid = lambda n: mujoco.mj_name2id(model, mujoco.mjtObj.mjOBJ_JOINT, n)
    duck = []
    for i, name in enumerate(ducks):
        p, rows = duck_prefix(i), side.filter(pc.equal(side["duck"], name))
        site = mujoco.mj_name2id(model, mujoco.mjtObj.mjOBJ_SITE, p + "tof")
        duck.append({"free": model.jnt_qposadr[jid(p + "trunk_base_freejoint")],
                     "slots": [(w, model.jnt_qposadr[j]) for w, n in enumerate(JOINT_NAMES) if (j := jid(p + n)) >= 0],
                     "root": mujoco.mj_name2id(model, mujoco.mjtObj.mjOBJ_BODY, p + "trunk_base"), "site": site,
                     "dirs": Tof(model, site).directions, "t": (rows["t_tai_ns"].to_numpy() - t0) / 1e9,
                     "trunk": np.array(rows["trunk"].to_pylist()), "quat": np.array(rows["quat"].to_pylist()),
                     "pos": np.array(rows["positions"].to_pylist())})
    n, k = len(times), len(ducks)
    R, B = np.full((n, k, k), np.inf), np.zeros((n, k, k))
    LOS, TOF = np.zeros((n, k, k), bool), np.zeros((n, k, k), bool)
    g1, zones = np.zeros(1, np.int32), duck[0]["dirs"].shape[0]
    gz, dz = np.zeros(zones, np.int32), np.zeros(zones)
    root_of = lambda g: model.body_rootid[model.geom_bodyid[g]]
    for ti, t in enumerate(times):
        for d in duck:
            j = min(np.searchsorted(d["t"], t), len(d["t"]) - 1)
            data.qpos[d["free"]:d["free"] + 3], data.qpos[d["free"] + 3:d["free"] + 7] = d["trunk"][j], d["quat"][j]
            for w, adr in d["slots"]:
                data.qpos[adr] = d["pos"][j, w]
        mujoco.mj_kinematics(model, data)
        for a, da in enumerate(duck):
            pa_, qa = data.xpos[da["root"]], data.xquat[da["root"]]
            yaw_a = math.atan2(2 * (qa[0] * qa[3] + qa[1] * qa[2]), 1 - 2 * (qa[2] ** 2 + qa[3] ** 2))
            head = data.site_xpos[da["site"]].copy()
            for b, db in enumerate(duck):
                if b == a:
                    continue
                pb = data.xpos[db["root"]]
                r = math.hypot(pb[0] - pa_[0], pb[1] - pa_[1])
                if r > MAX_SENSE_M:
                    continue
                R[ti, a, b], B[ti, a, b] = r, wrap(math.atan2(pb[1] - pa_[1], pb[0] - pa_[0]) - yaw_a)
                v = pb - head
                dist = float(np.linalg.norm(v))
                h = mujoco.mj_ray(model, data, head, v / dist, None, 1, da["root"], g1)
                LOS[ti, a, b] = h < 0 or h >= dist or root_of(g1[0]) == db["root"]
                if LOS[ti, a, b] and r <= MAX_RANGE and abs(B[ti, a, b]) < TOF_CONE_RAD:
                    vec = (data.site_xmat[da["site"]].reshape(3, 3) @ da["dirs"].T).T.ravel()
                    mujoco.mj_multiRay(model, data, head, vec, None, 1, da["root"], gz, dz, None, zones, MAX_RANGE)
                    TOF[ti, a, b] = any(dz[z] >= 0 and root_of(gz[z]) == db["root"] for z in range(zones))
    return R, B, LOS, TOF


# -- one design --------------------------------------------------------------------------------------

def sightings(p: dict, ducks, geo, rng) -> list[tuple]:
    """`(ti, a, b, range or nan, bearing or nan, σ_range, σ_bearing)` for the design, from the replay."""
    R, B, LOS, TOF = geo
    stride, out = max(1, round(sc.REPLAY_HZ / p["hz"])), []
    for ti in range(0, R.shape[0], stride):
        for a in range(len(ducks)):
            for b in range(len(ducks)):
                r = R[ti, a, b]
                if a == b or not np.isfinite(r):
                    continue
                if p["kind"] == "ble":
                    if r <= p["max_r"]:
                        out.append((ti, a, b, max(0.05, r + rng.normal(0, p["sigma"])), math.nan, p["sigma"], math.nan))
                    continue
                if (not LOS[ti, a, b] or abs(B[ti, a, b]) > math.radians(p["fov"] / 2)
                        or rng.random() > 1 / (1 + (r / p["r50"]) ** 4)):
                    continue
                sb = math.radians(p["bearing"])
                rr, sr = math.nan, math.nan
                if p["range"] == "tof" and TOF[ti, a, b]:
                    sr = p["sigma"]
                elif p["range"] == "marker":
                    sr = max(0.01, p["k"] * r * r)
                if not math.isnan(sr):
                    rr = r + rng.normal(0, sr)
                out.append((ti, a, b, rr, B[ti, a, b] + rng.normal(0, sb), sr, sb))
    return out


def residuals(T, o) -> np.ndarray:
    """Range and bearing residuals (in σ) of a pair's sightings under `T` = lo ← hi."""
    tx, ty, th = T
    c, s = math.cos(th), math.sin(th)
    fwd = np.column_stack([c * o["tgt"][:, 0] - s * o["tgt"][:, 1] + tx, s * o["tgt"][:, 0] + c * o["tgt"][:, 1] + ty])
    dx, dy = o["tgt"][:, 0] - tx, o["tgt"][:, 1] - ty
    back = np.column_stack([c * dx + s * dy, -s * dx + c * dy])
    v = np.where(o["lo_sees"][:, None], fwd, back) - o["obs"]
    hr, hb = ~np.isnan(o["r"]), ~np.isnan(o["b"])
    rb = (np.arctan2(v[:, 1], v[:, 0]) - o["yaw"] - o["b"] + np.pi) % (2 * np.pi) - np.pi
    return np.concatenate([(np.hypot(v[:, 0], v[:, 1]) - o["r"])[hr] / o["sr"][hr], rb[hb] / o["sb"][hb]])


def gauss_newton(o, starts) -> list[tuple[float, np.ndarray, np.ndarray]]:
    """Each start's least-squares `T`: `(mean squared residual, T, Jacobian)`, best first."""
    out = []
    for T in starts:
        T = np.array(T, float)
        for _ in range(20):
            r0 = residuals(T, o)
            J = np.column_stack([(residuals(T + e, o) - r0) / 1e-6 for e in np.eye(3) * 1e-6])
            dx = np.linalg.lstsq(J, -r0, rcond=None)[0]
            T = T + dx
            T[2] = wrap(T[2])
            if np.abs(dx).max() < 1e-7:
                break
        r0 = residuals(T, o)
        J = np.column_stack([(residuals(T + e, o) - r0) / 1e-6 for e in np.eye(3) * 1e-6])
        out.append((float(np.mean(r0 ** 2)), T, J))
    return sorted(out, key=lambda x: x[0])


def fit_pair(o):
    """`T` = lo ← hi from a pair's sightings, or None when too few, too clustered or inconsistent."""
    n = len(o["obs"])
    hi_pts = np.where(o["lo_sees"][:, None], o["tgt"], o["obs"])
    if n < sc.MIN_SIGHTINGS or 2 * np.linalg.norm(hi_pts - hi_pts.mean(0), axis=1).max() < sc.MIN_SPREAD_M:
        return None
    if len(residuals((0.0, 0.0, 0.0), o)) < 6:
        return None
    full = ~np.isnan(o["r"]) & ~np.isnan(o["b"])
    starts = []
    if full.sum() >= sc.MIN_SIGHTINGS:
        seen = o["obs"][full] + o["r"][full, None] * np.column_stack([np.cos(o["yaw"][full] + o["b"][full]),
                                                                      np.sin(o["yaw"][full] + o["b"][full])])
        lo = np.where(o["lo_sees"][full, None], seen, o["tgt"][full])
        hi = np.where(o["lo_sees"][full, None], o["tgt"][full], seen)
        starts.append(fit(lo, hi))
    for th in np.linspace(-math.pi, math.pi, 8, endpoint=False):
        c, s = math.cos(th), math.sin(th)
        lo = np.where(o["lo_sees"][:, None], o["obs"], o["tgt"])
        starts.append((*(lo.mean(0) - np.array([[c, -s], [s, c]]) @ hi_pts.mean(0)), th))
    sols = gauss_newton(o, starts)
    cost, T, J = sols[0]
    if cost > 9.0 or math.hypot(T[0], T[1]) > MAX_OFFSET_M:
        return None
    # Determined, not just consistent: few bearings from nearly one place fit many offsets equally
    # well. The fit's own 1σ (residuals are in σ) must be small, and no other start may land on a
    # clearly different answer that fits about as well.
    cov = np.linalg.pinv(J.T @ J)
    if (math.sqrt(max(np.linalg.eigvalsh(cov[:2, :2]))) > FIT_SIGMA_M or math.sqrt(cov[2, 2]) > FIT_SIGMA_RAD):
        return None
    n = len(J)
    for c2, T2, _ in sols[1:]:
        far = math.hypot(T2[0] - T[0], T2[1] - T[1]) > AMBIGUOUS_M or abs(wrap(T2[2] - T[2])) > AMBIGUOUS_RAD
        if far and (c2 - cost) * n < AMBIGUOUS_CHI2:
            return None
    # Bearings alone leave the scale free: a fit can slide off to where every bearing still matches.
    # Its implied ranges must stay within what any of these sensors could have seen.
    c, s = math.cos(T[2]), math.sin(T[2])
    fwd = o["tgt"] @ np.array([[c, s], [-s, c]]) + T[:2]
    back = (o["tgt"] - T[:2]) @ np.array([[c, -s], [s, c]])
    v = np.where(o["lo_sees"][:, None], fwd, back) - o["obs"]
    return tuple(T) if np.hypot(v[:, 0], v[:, 1]).max() <= MAX_SENSE_M else None


def estimate(ducks, sights, times, odom) -> dict[str, list]:
    """Per duck, `[(t, T lookout ← duck)]`: every `REFIT_S` each pair fitted on its sightings so far,
    chained breadth first onto the lookout."""
    pairs = {}
    for ti, a, b, r, br, sr, sb in sights:
        t = times[ti]
        lo, hi = min(a, b), max(a, b)
        oa, ob = odom[ducks[a]].at(t), odom[ducks[b]].at(t)
        pairs.setdefault((lo, hi), []).append((t, *oa, ob[0], ob[1], a == lo, r, br, sr, sb))
    pairs = {k: np.array(v, float) for k, v in pairs.items()}
    look = ducks.index(sc.LOOKOUT)
    history = {d: [] for d in ducks if d != sc.LOOKOUT}
    for tr in np.arange(sc.REFIT_S, times[-1] + 1e-9, sc.REFIT_S):
        edges = {}
        for (lo, hi), v in pairs.items():
            v = v[v[:, 0] <= tr]
            if not len(v):
                continue
            o = {"obs": v[:, 1:3], "yaw": v[:, 3], "tgt": v[:, 4:6], "lo_sees": v[:, 6] > 0.5,
                 "r": v[:, 7], "b": v[:, 8], "sr": v[:, 9], "sb": v[:, 10]}
            T = fit_pair(o)
            if T is not None:
                edges[(lo, hi)], edges[(hi, lo)] = T, invert(T)
        est, queue = {look: (0.0, 0.0, 0.0)}, deque([look])
        while queue:
            cur = queue.popleft()
            for (a, b), T in edges.items():
                if a == cur and b not in est:
                    est[b] = compose(est[cur], T)
                    queue.append(b)
        for d, T in est.items():
            if d != look:
                history[ducks[d]].append((float(tr), T))
    return history


def score(ducks, sights, times, truth, odom, history) -> dict:
    """"b in a's trunk frame", estimated vs true, every second for every ordered pair; and the error
    against how long the target has gone unseen."""
    last = {d: [] for d in range(len(ducks))}
    for ti, a, b, *_ in sights:
        last[a].append(times[ti])
        last[b].append(times[ti])
    last = {d: np.array(sorted(v)) for d, v in last.items()}
    latest = lambda d, t: (0.0, 0.0, 0.0) if d == sc.LOOKOUT else next(
        (T for tt, T in reversed(history[d]) if tt <= t), None)
    errs, gaps, total = [], [], 0
    for t in np.arange(0.0, times[-1], 1.0):
        for i, a in enumerate(ducks):
            for j, b in enumerate(ducks):
                if a == b:
                    continue
                total += 1
                ta, tb = latest(a, t), latest(b, t)
                if ta is None or tb is None:
                    continue
                oa, ob = odom[a].at(t), odom[b].at(t)
                est = compose(invert(oa), compose(compose(invert(ta), tb), (ob[0], ob[1], 0.0)))[:2]
                xa, ya, yawa = truth[a].at(t)
                xb, yb, _ = truth[b].at(t)
                rel = rotate(xb - xa, yb - ya, -yawa)
                errs.append(math.hypot(est[0] - rel[0], est[1] - rel[1]))
                k = np.searchsorted(last[j], t, side="right")
                gaps.append(t - last[j][k - 1] if k else math.inf)
    errs, gaps = np.array(errs), np.array(gaps)
    firsts = [h[0][0] if h else math.inf for h in history.values()]
    # Staleness as growth: error against time since the target was last seen, a straight line fitted
    # over the gaps seen; how long until it passes `STALE_M`.
    k = np.isfinite(gaps) & (gaps <= GROWTH_MAX_S)
    growth, e0 = (np.polyfit(gaps[k] / 60, errs[k], 1) if k.sum() > 10 and np.ptp(gaps[k]) > 1 else (math.nan, math.nan))
    resight = (max(0.0, sc.STALE_M - e0) / growth * 60 if growth > 0 else math.inf) if math.isfinite(growth) else math.nan
    return {"median": float(np.median(errs)) if len(errs) else math.nan,
            "p95": float(np.percentile(errs, 95)) if len(errs) else math.nan,
            "coverage": len(errs) / total, "aligned_at": max(firsts), "growth": float(growth), "resight_s": float(resight),
            "n_sight": len(sights), "errs": errs, "gaps": gaps}


def aggregate(per: list[dict]) -> dict:
    """One design's scores over its seeds and runs: medians, with the spread across them."""
    pick = lambda key: np.array([p[key] for p in per], float)
    out = {"n_sight": float(np.median(pick("n_sight"))), "coverage": float(np.mean(pick("coverage"))),
           "aligned_at": float(np.median(pick("aligned_at")))}
    for key in ("median", "p95", "growth", "resight_s"):
        v = pick(key)
        v = v[~np.isnan(v)]
        out[key] = float(np.median(v)) if len(v) else math.nan
        out[key + "_lo"], out[key + "_hi"] = (float(np.percentile(v, 10)), float(np.percentile(v, 90))) if len(v) else (math.nan, math.nan)
    out["errs"] = np.concatenate([p["errs"] for p in per])
    out["gaps"] = np.concatenate([p["gaps"] for p in per])
    return out


# -- ledgers -------------------------------------------------------------------------------------------

@contextmanager
def server():
    """A fresh soloc-server from `serve.sh`, stopped (its whole process group) on the way out."""
    proc = subprocess.Popen([str(SERVE)], start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(600):
            with socket.socket() as s:
                if s.connect_ex(("127.0.0.1", PORT)) == 0:
                    break
            time.sleep(0.2)
        else:
            raise SystemExit("soloc-server did not come up")
        yield SolocClient(SERVER)
    finally:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait()
        for _ in range(100):
            with socket.socket() as s:
                if s.connect_ex(("127.0.0.1", PORT)) != 0:
                    break
            time.sleep(0.1)


def design_names(did: str, ducks) -> list[tuple[int, str, str]]:
    out = []
    for d in ducks:
        out += [(KIND_SOLOC, AUTHORITY, f"{sc.odom_name(d)}@{did}"), (KIND_ABSTRACT, AUTHORITY, f"{sc.source_name(d)}@{did}")]
        if d != sc.LOOKOUT:
            out.append((KIND_SOLOC, AUTHORITY, f"{sc.estimate_name(d)}@{did}"))
    return out


def design_rows(schema, did, ducks, times, odom_raw, boot, sights, history):
    """The design's ledger rows: its own boot frames (as recorded), drifted odometry, full-point
    sightings and estimates; and its partial sightings as a side table."""
    t0 = tai_ns_from_utc(sc.T0)
    tai = lambda t: t0 + int(round(t * 1e9))
    buf, truth_src = RowBuffer(schema), sc.source(sc.TRUTH_SOURCE_NAME)
    frame = {d: sc.entity(f"{sc.odom_name(d)}@{did}") for d in ducks}
    src = {d: sc.source(f"{sc.source_name(d)}@{did}") for d in ducks}
    apt = sc.entity(sc.APARTMENT)
    for d in ducks:
        bt, bp, bq = boot[d]
        buf.append(frame[d], apt, bp.tolist(), bq.tolist(), tai(bt), units="m", source_id=truth_src, estimate="SIMULATED")
        t, p, yaw, _ = odom_raw[d]
        for k in range(len(t)):
            buf.append(sc.entity(d), frame[d], p[k].tolist(), quat_yaw(yaw[k]), tai(t[k]), units="m",
                       source_id=src[d], estimate="MEASURED")
    partial = []
    for ti, a, b, r, br, *_ in sights:
        t, obs, tgt = times[ti], ducks[a], ducks[b]
        if math.isnan(r) or math.isnan(br):
            partial.append((tai(t), obs, tgt, None if math.isnan(r) else r, None if math.isnan(br) else br))
            continue
        ox, oy, oyaw = odom_raw[obs][3].at(t)
        buf.append(sc.entity(tgt), frame[obs], [ox + r * math.cos(oyaw + br), oy + r * math.sin(oyaw + br), 0.0],
                   [1.0, 0.0, 0.0, 0.0], tai(t), units="m", source_id=src[obs], estimate="MEASURED")
    for d, hist in history.items():
        for tr, T in hist:
            buf.append(sc.entity(f"{sc.estimate_name(d)}@{did}"), frame[sc.LOOKOUT], [T[0], T[1], 0.0], quat_yaw(T[2]),
                       tai(tr), units="m", source_id=src[sc.LOOKOUT], estimate="ESTIMATED")
    side = pa.Table.from_pylist([dict(zip(SIGHTINGS_SCHEMA.names, r)) for r in partial], SIGHTINGS_SCHEMA)
    return buf.flush(), side


def exchange_check(client, table, rows, ducks, site: Facility) -> float:
    """soloc's chain (apartment → site → IAU_EARTH) against the same truth composed here, metres."""
    apt = sc.entity(sc.APARTMENT)
    _, ap_p, ap_q = framed_m(rows, apt, site.id)
    r_apt = quat_matrix(ap_q[0])
    worst = 0.0
    for d in ducks:
        k = rows.of(sc.entity(d))
        k = k[np.array([f == apt for f in rows.frames[k]], bool)][::300]
        out = client.exchange(table.take(k), EARTH.frame)
        local = rows.pos[k] * rows.km[k][:, None] * 1000
        mine = site.position_km + (local @ r_apt.T + ap_p[0]) @ site.basis.T / 1000
        worst = max(worst, float(np.abs(positions(out) - mine).max() * 1000))
    return worst


# -- plots -----------------------------------------------------------------------------------------------

def plots(results: dict, specs: dict):
    labels = {"range": "camera 50% detection range (m)", "fov": "field of view (°)", "rate": "sighting rate (Hz)",
              "noise": "noise level (low / baseline / high)", "drift": "heading drift (°/min)"}
    colour = {d: SERIES[i] for i, d in enumerate(sc.DESIGNS)}
    out = []
    for axis, per in sc.SWEEP_AXES.items():
        fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(10, 4))
        for d, (key, values, _) in per.items():
            if sc.DESIGNS[d]["kind"] != "cam":     # BLE alone rarely has an estimate (see the table)
                continue
            xs, rs = [], []
            for i, v in enumerate(values):
                did = f"{d}-base" if v == sc.DESIGNS[d][key] else f"{d}-{per[d][2]}{v:g}"
                if did in results:
                    xs.append(i if axis == "noise" else v)
                    rs.append(results[did])
            if not xs:
                continue
            col = lambda key: np.array([r[key] for r in rs])
            ax1.plot(xs, col("median") * 100, "o-", color=colour[d], label=f"{d} median")
            ax1.fill_between(xs, col("median_lo") * 100, col("median_hi") * 100, color=colour[d], alpha=0.15)
            ax1.plot(xs, col("p95") * 100, "o--", color=colour[d], alpha=0.6, label=f"{d} p95")
            # Re-sight comes from a line fitted over gaps up to GROWTH_MAX_S; longer is extrapolation,
            # so it is drawn at the cap with ▲.
            resight = col("resight_s")
            over = resight > GROWTH_MAX_S
            shown = np.minimum(resight, GROWTH_MAX_S)
            ax2.plot(xs, shown, "-", color=colour[d], label=d)
            ax2.plot(np.array(xs)[~over], shown[~over], "o", color=colour[d])
            ax2.plot(np.array(xs)[over], shown[over], "^", color=colour[d], markersize=8)
        for ax in (ax1, ax2):
            style(ax)
            ax.set_xlabel(labels[axis])
        if axis == "noise":
            for ax in (ax1, ax2):
                ax.set_xticks([0, 1, 2], ["low", "baseline", "high"])
        ax1.set_ylabel('"b in a\'s frame" error (cm)')
        ax2.set_ylabel(f"re-sight within (s) to stay ≤ {sc.STALE_M * 100:.0f} cm")
        ax2.axhline(GROWTH_MAX_S, color="#6b6a63", lw=1, ls=":")
        ax2.annotate(f"▲ ≥ {GROWTH_MAX_S / 60:.0f} min: longer than any gap seen", (0.02, GROWTH_MAX_S),
                     xycoords=("axes fraction", "data"), xytext=(0, 4), textcoords="offset points",
                     fontsize=7, color="#6b6a63")
        ax2.set_ylim(0, GROWTH_MAX_S * 1.15)
        ax1.legend(fontsize=7)
        ax2.legend(fontsize=7)
        fig.tight_layout()
        path = sc.OUT / f"sweep_{axis}.png"
        fig.savefig(path, dpi=PLOT_DPI)
        plt.close(fig)
        out.append(path)
    fig, ax = plt.subplots(figsize=(6, 4))
    bins = sc.UNSEEN_BINS_S
    names = [f"{lo:g}–{hi:g} s" if math.isfinite(hi) else f"≥ {lo:g} s" for lo, hi in zip(bins, bins[1:])]
    for i, d in enumerate(sc.DESIGNS):
        for did, ls in ((f"{d}-base", "-"), (f"{d}-drift5", ":")):
            if did not in results:
                continue
            e, g = results[did]["errs"], results[did]["gaps"]
            med = [np.median(e[(g >= lo) & (g < hi)]) * 100 if ((g >= lo) & (g < hi)).any() else np.nan
                   for lo, hi in zip(bins, bins[1:])]
            ax.plot(range(len(names)), med, "o" + ls, color=SERIES[i], label=did)
    ax.axhline(sc.STALE_M * 100, color="#6b6a63", lw=1, ls=":")
    style(ax)
    ax.set_xticks(range(len(names)), names)
    ax.set_xlabel("time since the target was last seen by anyone")
    ax.set_ylabel("median error, all seeds and runs (cm)")
    ax.legend(fontsize=8)
    fig.tight_layout()
    path = sc.OUT / "sweep_staleness.png"
    fig.savefig(path, dpi=PLOT_DPI)
    plt.close(fig)
    return out + [path]


# -- main ------------------------------------------------------------------------------------------------

def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--designs", nargs="*", help="design ids (default: all); `-base` ids are the baselines")
    args = p.parse_args()
    specs = designs()
    chosen = args.designs or list(specs)
    if unknown := [d for d in chosen if d not in specs]:
        raise SystemExit(f"unknown designs {unknown}; known: {', '.join(specs)}")
    with socket.socket() as s:
        if s.connect_ex(("127.0.0.1", PORT)) == 0:
            raise SystemExit("a server is already on port 50051; stop serve.sh, this starts its own")

    per = {did: [] for did in chosen}
    site, schema = Facility(sc.SITE), None
    for run in sc.RUNS:
        sc.set_run(run)
        if not sc.LEDGER.exists():
            print(f"run {run}: no {sc.LEDGER.name}, skipped")
            continue
        table, _ = load(sc.LEDGER)
        rows = Rows(table)
        rows.t_s = rows.t_s - (tai_ns_from_utc(sc.T0) - tai_ns_from_utc(sc0.T0)) / 1e9   # Rows counts from sim_study_0's T0
        apt = sc.entity(sc.APARTMENT)
        ducks = [d for d in sc.DUCKS if len(rows.of(sc.entity(d)))]
        truth, odom, boot = {}, {}, {}
        for d in ducks:
            truth[d] = Track(*framed_m(rows, sc.entity(d), apt))
            t, pos, q = framed_m(rows, sc.entity(d), sc.entity(sc.odom_name(d)))
            odom[d] = (t, pos, yaws(q))
            bt, bp, bq = framed_m(rows, sc.entity(sc.odom_name(d)), apt)
            boot[d] = (bt[0], bp[0], bq[0])
        duration = float(min(tr.t[-1] for tr in truth.values()))
        times = np.arange(0.0, duration, 1.0 / sc.REPLAY_HZ)
        start = time.monotonic()
        geo = replay(ducks, times)
        print(f"run {run}: replayed {len(times)} instants × {len(ducks) * (len(ducks) - 1)} pairs in "
              f"{time.monotonic() - start:.0f} s: {geo[2][np.isfinite(geo[0])].mean() * 100:.0f}% of pairs within "
              f"{MAX_SENSE_M:g} m in clear line, {geo[3].sum()} ToF hits on a duck")
        sc.SWEEP.mkdir(parents=True, exist_ok=True)
        batches, names = [], sc.bindings(ducks)
        for did in chosen:
            spec = specs[did]
            tracks, raw = {}, {}
            for d in ducks:
                t, pos, yaw = odom[d]
                dp, dyaw = drifted(t, pos, yaw, sc.DRIFT_SIGN[d], spec["drift"])
                tracks[d] = track_of(t, dp, dyaw)
                raw[d] = (t, dp, dyaw, tracks[d])
            for seed in range(sc.SEEDS):
                rng = np.random.default_rng(zlib.crc32(f"{did}/{run}/{seed}".encode()))
                sights = sightings(spec, ducks, geo, rng)
                history = estimate(ducks, sights, times, tracks)
                per[did].append(score(ducks, sights, times, truth, tracks, history))
                if seed == 0:
                    keep = (sights, history)
            # The ledgers carry seed 0: the design's frames, odometry, sightings and estimates.
            sights, history = keep
            with server() as client:
                client.action("load_ledger", {"path": str(sc.LEDGER)})
                if schema is None:
                    schema = client.schema
                if did == chosen[0]:
                    worst = exchange_check(client, table, rows, ducks, site)
                    print(f"  soloc chain (apartment → site → IAU_EARTH) vs truth composed here: {worst * 1000:.3f} mm worst")
                batch, side = design_rows(schema, did, ducks, times, raw, boot, sights, history)
                client.action("import_names", registry_ipc(names + design_names(did, ducks)))
                client.put(batch)
                client.action("save_ledger", {"path": str(sc.SWEEP / f"{did}.arrow")})
            if side.num_rows:
                with pa.ipc.new_file(sc.SWEEP / f"{did}.sightings.arrow", SIGHTINGS_SCHEMA) as w:
                    w.write_table(side)
            batches.append(batch)
            names += design_names(did, ducks)
            print(f"  run {run} {did} done")
        with server() as client:
            client.action("load_ledger", {"path": str(sc.LEDGER)})
            client.action("import_names", registry_ipc(names))
            for b in batches:
                client.put(b)
            print(client.action("save_ledger", {"path": str(sc.SWEEP_MERGED)}))
    sc.set_run(1)
    results = {did: aggregate(v) for did, v in per.items() if v}
    print(f"\n{'design':24s} sightings  median (10–90%)        p95 (10–90%)          coverage  aligned  growth   re-sight")
    for did, r in results.items():
        print(f"{did:24s} {r['n_sight']:9.0f}  {r['median'] * 100:5.1f} ({r['median_lo'] * 100:4.1f}–{r['median_hi'] * 100:5.1f}) cm"
              f"  {r['p95'] * 100:5.1f} ({r['p95_lo'] * 100:5.1f}–{r['p95_hi'] * 100:6.1f}) cm  {r['coverage'] * 100:4.0f}%"
              f"  {r['aligned_at']:5.0f} s  {r['growth'] * 100:5.1f} cm/min  {r['resight_s']:6.0f} s")
    for path in plots(results, specs):
        print(path)


if __name__ == "__main__":
    main()
