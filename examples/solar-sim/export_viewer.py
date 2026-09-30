"""Writes a standalone 3D viewer of a saved sim ledger: one HTML file with the tracks, the body
ephemerides, the coastlines and three.js inlined, so it opens offline by double-click.

    python export_viewer.py out/solar_sim.arrow             # → out/solar_sim_3d.html

The viewer is one scene, nested the way the ledger's frames are:
- ICRF holds the Sun and planets on their orbits.
- Each body's IAU frame turns with the body. Its vehicles are in there, in body-fixed km as stored.
- Each site's ENU frame sits on its body at the facility's stored pose, with its robots in metres.

Tracks come from the file. Rows framed on a facility (a craft on its pad or landed) have the
facility's stored pose applied, the same composition the ledger does. Body positions and
orientations are not in the ledger at this density, so they come from a running server
(`serve.sh`): a zero offset in each body's frame is exchanged to ICRF at every epoch needed.
That gives the body's centre and its IAU → ICRF rotation from the same kernels the ledger
resolves with. Crawlers on orbit are left out (a 2 m hull is invisible at orbit scale); the
one that disembarks shows on its site.
"""

import argparse
import base64
import json
from pathlib import Path

import numpy as np
import pyarrow as pa

import scenario as sc
from geo import EARTH, MARS, MOON, quat_from_matrix
from land import outlines
from run_sim import roster
from soloc_client import (CENTURY_NS, SolocClient, astronomical, id_bytes, positions, sts_field,
                          tai_ns_from_utc)
from view_sim import load

HERE = Path(__file__).parent
THREE_JS = HERE / "kernels" / "three.min.js"
TEMPLATE = HERE / "viewer_template.html"
EPHEMERIS_STEP_S = 300
DAY_S = 86400

# (name, NAIF id, mean radius km, colour, [orbit period days, orbit sample step days, centre])
BODIES = (
    ("Sun", 10, 695700.0, "#ffcc66", None),
    ("Mercury", 199, 2439.7, "#9a968c", (88.0, 0.5, None)),
    ("Venus", 299, 6051.8, "#d8c58f", (224.7, 1.0, None)),
    ("Earth", 399, EARTH.a_km, "#2c5a8f", (365.3, 1.0, None)),
    ("Moon", 301, MOON.a_km, "#8d8c84", (27.32, 2 / 24, "Earth")),
    ("Mars", 499, MARS.a_km, "#b5613f", (687.0, 2.0, None)),
)
FLATTENING = {"Earth": EARTH.f}


def b64(a, dtype=np.float32) -> str:
    return base64.b64encode(np.asarray(a, dtype=dtype).tobytes()).decode()


def quat_matrix(q) -> np.ndarray:
    w, x, y, z = q
    return np.array([
        [1 - 2 * (y * y + z * z), 2 * (x * y - w * z), 2 * (x * z + w * y)],
        [2 * (x * y + w * z), 1 - 2 * (x * x + z * z), 2 * (y * z - w * x)],
        [2 * (x * z - w * y), 2 * (y * z + w * x), 1 - 2 * (x * x + y * y)],
    ])


class Rows:
    def __init__(self, table):
        self.ids = np.array(id_bytes(table.column("entity_id")), dtype=object)
        self.frames = np.array(id_bytes(sts_field(table, "frame_id")), dtype=object)
        t_ns = (sts_field(table, "duration_centuries").to_numpy().astype(np.int64) * CENTURY_NS
                + sts_field(table, "duration_ns").to_numpy().astype(np.int64))
        self.t_s = (t_ns - tai_ns_from_utc(sc.T0)) / 1e9
        self.pos = sts_field(table, "position").flatten().to_numpy().reshape(-1, 3)
        self.quat = sts_field(table, "quaternion").flatten().to_numpy().reshape(-1, 4)
        units = sts_field(table, "units_pos").to_numpy()
        tokens = table.schema.field("spacetimestamp").type.field("units_pos").metadata[
            b"ARROW:extension:metadata"].decode().split(",")
        self.km = np.where(np.array(tokens)[units] == "m", 1e-3, 1.0)   # km per stored unit
        code: dict[bytes, int] = {}
        codes = np.fromiter((code.setdefault(i, len(code)) for i in self.ids), np.int64, len(self.ids))
        self.index = {}
        for i, c in code.items():
            k = np.flatnonzero(codes == c)
            self.index[i] = k[np.argsort(self.t_s[k], kind="stable")]

    def of(self, entity_id: bytes) -> np.ndarray:
        return self.index.get(entity_id, np.array([], np.int64))


class Pose:
    """A facility's stored pose (position km, rotation site ENU → body-fixed); it is static."""

    def __init__(self, rows: Rows, facility_id: bytes):
        k = rows.of(facility_id)[0]
        self.p_km, self.r = rows.pos[k], quat_matrix(rows.quat[k])


def in_body(rows: Rows, entity_id: bytes, body_frame: bytes, poses: dict):
    """`(t_s, xyz km, quaternion)` in the body's frame: body-framed rows as stored,
    facility-framed rows composed through the facility's pose."""
    t, xyz, quat = [], [], []
    for k in rows.of(entity_id):
        f = rows.frames[k]
        local_km = rows.pos[k] * rows.km[k]
        if f == body_frame:
            p, q = local_km, rows.quat[k]
        elif f in poses:
            p = poses[f].p_km + poses[f].r @ local_km
            q = quat_from_matrix(poses[f].r @ quat_matrix(rows.quat[k]))
        else:
            continue
        t.append(rows.t_s[k])
        xyz.append(p)
        quat.append(q)
    return np.array(t), np.array(xyz).reshape(-1, 3), np.array(quat).reshape(-1, 4)


def on_site(rows: Rows, entity_id: bytes, facility_id: bytes):
    """`(t_s, xyz m, quaternion)` of the rows framed on the facility (already site ENU)."""
    k = rows.of(entity_id)
    k = k[np.array([f == facility_id for f in rows.frames[k]], bool)]
    return rows.t_s[k], rows.pos[k] * rows.km[k][:, None] * 1000, rows.quat[k]


def track(name: str, cat: str, t, xyz, quat) -> dict | None:
    """Quaternions are `[w, x, y, z]`, rotating the entity's body axes into its context frame."""
    if len(t) == 0:
        return None
    return {"name": name, "cat": cat, "t": b64(t), "p": b64(np.asarray(xyz).ravel()),
            "q": b64(np.asarray(quat).ravel())}


def ephemeris(client: SolocClient, naif: int, t_s: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """The body's centre (ICRF km) and IAU → ICRF rotation `[w, x, y, z]` at each `t_s`: a zero
    offset with identity attitude in the body frame, exchanged to ICRF by the server."""
    frame = astronomical(naif, naif)
    buf = client.buffer()
    t0 = tai_ns_from_utc(sc.T0)
    for t in t_s:
        buf.append(frame, frame, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], t0 + int(round(t)) * 10**9)
    out = client.exchange(pa.Table.from_batches([buf.flush()]), "ICRF")
    q = sts_field(out, "quaternion").flatten().to_numpy().reshape(-1, 4).copy()
    for i in range(1, len(q)):          # neighbours on the same hemisphere, so slerp takes the short way
        if q[i] @ q[i - 1] < 0:
            q[i] = -q[i]
    return positions(out), q


def coastlines() -> str:
    """Rings as lon/lat degree pairs, every 3rd vertex, NaN-separated."""
    parts = []
    for ring in outlines():
        r = ring[::3] if len(ring) > 30 else ring
        parts.append(np.vstack([r, r[:1]]))
        parts.append([[np.nan, np.nan]])
    return b64(np.vstack(parts).ravel())


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("path", type=Path)
    p.add_argument("--out", type=Path, help="default: <path stem>_3d.html beside the ledger")
    p.add_argument("--server", default="grpc://localhost:50051")
    p.add_argument("--seed", type=int, default=sc.SEED)
    args = p.parse_args()

    table, names = load(args.path)
    rows = Rows(table)
    world = roster(args.seed)
    name = lambda e: names.get(e.id, e.name)
    poses = {f.id: Pose(rows, f.id) for f in world.facilities if len(rows.of(f.id))}
    client = SolocClient(args.server)

    # -- bodies: the window at EPHEMERIS_STEP_S, and one orbit around the window's middle --------
    grid = np.arange(0, sc.DURATION_S + 1, EPHEMERIS_STEP_S, dtype=float)
    mid = sc.DURATION_S / 2
    bodies, orbits = [], []
    for body_name, naif, radius, colour, orbit in BODIES:
        pos, q = ephemeris(client, naif, grid)
        bodies.append({"name": body_name, "radius": radius, "color": colour,
                       "flattening": FLATTENING.get(body_name, 0.0),
                       "p": b64(pos.ravel(), np.float64), "q": b64(q.ravel())})
        if orbit:
            period_d, step_d, centre = orbit
            ts = mid + np.arange(-period_d / 2, period_d / 2 + step_d, step_d) * DAY_S
            path, _ = ephemeris(client, naif, ts)
            if centre:
                path = path - ephemeris(client, dict((b[0], b[1]) for b in BODIES)[centre], ts)[0]
            orbits.append({"name": body_name, "centre": centre, "p": b64(path.ravel())})
        print(f"  ephemeris {body_name}")

    # -- contexts: each body's fixed frame, and each site -------------------------------------
    contexts = []
    for body in (EARTH, MOON, MARS):
        members = [("spacecraft", c) for c in world.spacecraft if c.body is body]
        if body is EARTH:
            members += [("aircraft", a) for a in world.aircraft] + [("ship", s) for s in world.ships]
        tracks = [track(name(e), cat, *in_body(rows, e.id, body.frame_id, poses)) for cat, e in members]
        tracks = [t for t in tracks if t]
        if tracks:
            contexts.append({"id": body.name.lower(), "kind": "body", "body": body.name,
                             "name": body.name, "frame": body.frame, "tracks": tracks})
    for f in world.facilities:
        if f.id not in poses:
            continue
        tracks = [track(name(r), r.role, *on_site(rows, r.id, f.id))
                  for r in world.robots if r.host_id == f.id]
        tracks += [track(name(c), "crawler", *on_site(rows, c.id, f.id))
                   for c in world.crawlers if c.disembark and c.disembark[0] is f]
        contexts.append({"id": f.spec.code.lower(), "kind": "site", "body": f.spec.body.name,
                         "name": name(f), "frame": f"{name(f)} ENU",
                         "p": poses[f.id].p_km.tolist(), "q": quat_from_matrix(poses[f.id].r),
                         "half": sc.SITE_HALF_WIDTH_M, "roads": list(sc.SITE_ROADS_M),
                         "depot": list(sc.DEPOT_M), "tracks": [t for t in tracks if t]})

    events = []
    for c in world.spacecraft:
        for key in ("liftoff", "deorbit", "touchdown"):
            if key in c.events:
                events.append({"t": c.events[key], "label": f"{name(c)} {key}"})
    for c in world.crawlers:
        if c.disembark:
            events.append({"t": c.disembark[1], "label": f"{name(c)} disembarks"})

    data = {"t0": sc.T0.isoformat() + "Z", "duration": sc.DURATION_S, "source": args.path.name,
            "ephemerisStep": EPHEMERIS_STEP_S, "bodies": bodies, "orbits": orbits,
            "coast": coastlines(), "contexts": contexts,
            "events": sorted(events, key=lambda e: e["t"])}
    html = (TEMPLATE.read_text()
            .replace("/*THREE_JS*/", THREE_JS.read_text())
            .replace("/*DATA_JSON*/", json.dumps(data, separators=(",", ":"))))
    out = args.out or args.path.with_name(args.path.name.removesuffix(".arrow") + "_3d.html")
    out.write_text(html)
    n = sum(len(c["tracks"]) for c in contexts)
    print(f"{out}  ({out.stat().st_size / 1e6:.1f} MB, {len(bodies)} bodies, {n} tracks)")


if __name__ == "__main__":
    main()
