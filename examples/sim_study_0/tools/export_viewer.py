"""Writes a standalone 3D viewer of a saved sim ledger: one HTML file with the tracks, the body
ephemerides, the coastlines and three.js inlined, so it opens offline by double-click.

    python -m tools.export_viewer out/sim_study_0.arrow     # → out/sim_study_0_3d.html

The viewer is one scene, nested the way the ledger's frames are:
- ICRF holds the Sun and planets on their orbits. Craft stored in ICRF are drawn about the Sun.
- GCRF is Earth-centred and does not turn. A craft's translunar leg is in there.
- Each body's IAU frame turns with the body. Its vehicles are in there, in body-fixed km as stored.
- Each site's ENU frame sits on its body at the facility's stored pose, with its robots in metres.

Tracks come from the file. Rows framed on a facility (a craft on its pad or landed) have the
facility's stored pose applied, the same composition the ledger does. Body positions and
orientations are not in the ledger at this density, so they come from a running server
(`serve.sh`): a zero offset in each body's frame is exchanged to ICRF at every epoch needed.
That gives the body's centre and its IAU → ICRF rotation from the same kernels the ledger
resolves with. The orbits come from `query_orbits`. Robots riding a craft are left out (a 2 m hull is invisible at orbit scale);
the ones that step off show on their site. A craft with rows in several frames gets one track
per frame, under one name.
"""

import argparse
import base64
import json
from pathlib import Path

import numpy as np

from sim import DATA
from sim import scenario as sc
from sim.ephemeris import Ephemeris
from sim.geo import EARTH, GCRF, ICRF, MARS, MOON, SUN, quat_from_matrix
from sim.land import outlines
from sim.roster import roster
from soloc_client import CENTURY_NS, SolocClient, astronomical, id_bytes, sts_field, tai_ns_from_utc
from tools.view_sim import load

THREE_JS = DATA / "three.min.js"
TEMPLATE = Path(__file__).parent / "viewer_template.html"
EPHEMERIS_STEP_S = 300
DAY_S = 86400

# (name, NAIF id, mean radius km, colour, draw orbit)
BODIES = (
    ("Sun", 10, 695700.0, "#ffcc66", False),
    ("Mercury", 199, 2439.7, "#9a968c", True),
    ("Venus", 299, 6051.8, "#d8c58f", True),
    ("Earth", 399, EARTH.a_km, "#2c5a8f", True),
    ("Moon", 301, MOON.a_km, "#8d8c84", True),
    ("Mars", 499, MARS.a_km, "#b5613f", True),
)
FLATTENING = {"Earth": EARTH.f}
ARC_COLOUR = "#d95926"          # the spacecraft category's
# Timeline ticks, by the key a craft's builder files the instant under.
EVENT_LABELS = {"liftoff": "liftoff", "tli": "translunar injection",
                "soi": "enters the Moon's sphere of influence", "loi": "lunar orbit insertion",
                "deorbit": "deorbit", "touchdown": "touchdown"}


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


def framed(rows: Rows, entity_id: bytes, frame_id: bytes):
    """`(t_s, xyz km, quaternion)` of the rows stored in one inertial root, as stored."""
    k = rows.of(entity_id)
    k = k[np.array([f == frame_id for f in rows.frames[k]], bool)]
    return rows.t_s[k], rows.pos[k] * rows.km[k][:, None], rows.quat[k]


def track(name: str, cat: str, t, xyz, quat, wide: bool = False) -> dict | None:
    """Quaternions are `[w, x, y, z]`, rotating the entity's body axes into its context frame.
    `wide` keeps positions as float64, for tracks far from their context's centre."""
    if len(t) == 0:
        return None
    return {"name": name, "cat": cat, "t": b64(t), "wide": wide,
            "p": b64(np.asarray(xyz).ravel(), np.float64 if wide else np.float32),
            "q": b64(np.asarray(quat).ravel())}


def ephemeris(eph: Ephemeris, naif: int, t_s: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """The body's centre (ICRF km) and IAU → ICRF rotation `[w, x, y, z]` at each `t_s`."""
    p, q = eph.poses_of(astronomical(naif, naif), t_s)
    q = q.copy()
    for i in range(1, len(q)):          # neighbours on the same hemisphere, so slerp takes the short way
        if q[i] @ q[i - 1] < 0:
            q[i] = -q[i]
    return p, q


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
    client = SolocClient(args.server)
    world = roster(args.seed, client)
    name = lambda e: names.get(e.id, e.name)
    poses = {f.id: Pose(rows, f.id) for f in world.facilities if len(rows.of(f.id))}

    # -- bodies: the window at EPHEMERIS_STEP_S, and one orbit around the window's middle --------
    grid = np.arange(0, sc.DURATION_S + 1, EPHEMERIS_STEP_S, dtype=float)
    mid = sc.DURATION_S / 2
    eph = Ephemeris(client)
    bodies = []
    for body_name, naif, radius, colour, _ in BODIES:
        pos, q = ephemeris(eph, naif, grid)
        bodies.append({"name": body_name, "radius": radius, "color": colour,
                       "flattening": FLATTENING.get(body_name, 0.0),
                       "p": b64(pos.ravel(), np.float64), "q": b64(q.ravel())})
        print(f"  ephemeris {body_name}")
    by_id = {astronomical(naif, naif): body_name for body_name, naif, *_ in BODIES}
    table = client.orbits([astronomical(naif, naif) for _, naif, *_, orbit in BODIES if orbit],
                          tai_ns_from_utc(sc.T0) + int(mid) * 10**9)
    orbits = [{"name": by_id[b], "centre": by_id[c], "p": b64(np.array(path).ravel())}
              for b, c, path in zip(id_bytes(table.column("body_id")),
                                    id_bytes(table.column("centre_id")),
                                    table.column("path_km").to_pylist())]

    # -- contexts: each body's fixed frame, and each site -------------------------------------
    contexts = []
    # A craft has a track in every context it has rows in: SELENE-1 is in Earth's, GCRF's and
    # the Moon's in turn.
    for body in (EARTH, MOON, MARS):
        on_body = {f.id: poses[f.id] for f in world.facilities if f.spec.body is body and f.id in poses}
        members = [("spacecraft", c) for c in world.spacecraft]
        if body is EARTH:
            members += [("aircraft", a) for a in world.aircraft] + [("ship", s) for s in world.ships]
        tracks = [track(name(e), cat, *in_body(rows, e.id, body.frame_id, on_body)) for cat, e in members]
        tracks = [t for t in tracks if t]
        if tracks:
            contexts.append({"id": body.name.lower(), "kind": "body", "body": body.name,
                             "name": body.name, "frame": body.frame, "tracks": tracks})

    # -- inertial roots: GCRF about Earth as stored; ICRF rows shown about the Sun, since the
    #    barycentre is not a place anything orbits ------------------------------------------
    deep = [*world.spacecraft, *world.probes]
    tracks = [track(name(e), "spacecraft", *framed(rows, e.id, GCRF.frame_id), wide=True) for e in deep]
    if any(tracks):
        contexts.append({"id": "gcrf", "kind": "inertial", "body": EARTH.name, "name": "GCRF",
                         "frame": "GCRF", "tracks": [t for t in tracks if t]})
    tracks = []
    for e in deep:
        t, xyz, quat = framed(rows, e.id, ICRF.frame_id)
        if len(t):
            tracks.append(track(name(e), "spacecraft", t, xyz - eph.centre(SUN, t), quat, wide=True))
    if tracks:
        contexts.append({"id": "icrf", "kind": "inertial", "body": SUN.name, "name": "ICRF",
                         "frame": "ICRF axes, Sun-centred", "tracks": tracks})

    arcs = []
    for c in world.spacecraft:
        if "arrival" in c.events:
            ts = np.arange(c.events["departure"], c.events["arrival"] + 1, DAY_S, dtype=float)
            path = eph.centre(SUN, ts) + np.array([c.phases[0].conic.state(t)[0] for t in ts])
            arcs.append({"name": name(c), "color": ARC_COLOUR, "p": b64(path.ravel())})

    for f in world.facilities:
        if f.id not in poses:
            continue
        tracks = [track(name(r), r.role, *on_site(rows, r.id, f.id))
                  for r in world.robots if r.host_id == f.id]
        tracks += [track(name(c), "crawler", *on_site(rows, c.id, f.id))
                   for c in world.crawlers if c.disembark and c.disembark[0] is f]
        tracks += [track(name(c), "logistics", *on_site(rows, c.id, f.id))
                   for c in world.cargo if f in (c.origin, c.destination)]
        contexts.append({"id": f.spec.code.lower(), "kind": "site", "body": f.spec.body.name,
                         "name": name(f), "frame": f"{name(f)} ENU",
                         "p": poses[f.id].p_km.tolist(), "q": quat_from_matrix(poses[f.id].r),
                         "half": sc.SITE_HALF_WIDTH_M, "roads": list(sc.SITE_ROADS_M),
                         "depot": list(sc.DEPOT_M), "tracks": [t for t in tracks if t]})

    events = []
    for c in world.spacecraft:
        for key, label in EVENT_LABELS.items():
            if key in c.events:
                events.append({"t": c.events[key], "label": f"{name(c)} {label}"})
    for c in world.crawlers:
        if c.disembark:
            events.append({"t": c.disembark[1], "label": f"{name(c)} disembarks"})
    for c in world.cargo:
        events.append({"t": c.t_board, "label": f"{name(c)} boards {name(c.ship)}"})
        events.append({"t": c.t_off, "label": f"{name(c)} disembarks"})
    for p in world.probes:
        k = int(np.argmin(np.linalg.norm(p.p - p.sun_p, axis=1)))
        if 0 < k < len(p.t_s) - 1:
            events.append({"t": float(p.t_s[k]), "label": f"{name(p)} perihelion"})

    data = {"t0": sc.T0.isoformat() + "Z", "duration": sc.DURATION_S, "source": args.path.name,
            "ephemerisStep": EPHEMERIS_STEP_S, "bodies": bodies, "orbits": orbits, "arcs": arcs,
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
