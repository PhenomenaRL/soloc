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
import math
from datetime import datetime
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc

from sim import DATA
from sim import scenario as sc
from sim.ephemeris import Ephemeris
from sim.geo import EARTH, GCRF, ICRF, MARS, MOON, SUN, quat_from_matrix
from sim.land import outlines
from sim.models import factory as fm
from sim.models import regatta as rg
from sim.models import wildfire as wf
from sim.roster import roster
from sim.wind import read_table
from tools.snapshot_sim import FireView, factory_rows, fuel_grid
from tools.snapshot_sim import Rows as SnapRows
from soloc_client import CENTURY_NS, SolocClient, astronomical, id_bytes, sts_field, tai_ns_from_utc
from tools.view_sim import load

THREE_JS = DATA / "three.min.js"
TEMPLATE = Path(__file__).parent / "viewer_template.html"
EPHEMERIS_STEP_S = 300
DAY_S = 86400
VENUE_REACH_KM = 12.0           # the camera distance under which the venue's boats show
VENUE_VIEW_KM = 4.0             # "Go to" distance
FIRE_REACH_KM = 15.0
FIRE_VIEW_KM = 2.5
PLANT_REACH_KM = 0.5
PLANT_VIEW_KM = 0.045
PLANT_NEAR_KM = 2e-5                # a bearing is 5 cm across: let the camera come within 2 cm
PLANT_FOLLOW_KM = 3e-4

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


def track(name: str, cat: str, t, xyz, quat, wide: bool = False, model: str | None = None) -> dict | None:
    """Quaternions are `[w, x, y, z]`, rotating the entity's body axes into its context frame.
    `wide` keeps positions as float64, for tracks far from their context's centre. `model`
    picks a shape other than the category's."""
    if len(t) == 0:
        return None
    return {"name": name, "cat": cat, "model": model or cat, "t": b64(t), "wide": wide,
            "p": b64(np.asarray(xyz).ravel(), np.float64 if wide else np.float32),
            "q": b64(np.asarray(quat).ravel())}


def venue_context(g, rows: Rows, name, wind_path: Path) -> dict:
    """The regatta venue: a site context with the water, the line and the wind as its layout."""
    pose, c, vid = Pose(rows, g.venue.id), g.course, g.venue.id
    tracks = [track(name(b), "sailboat", *on_site(rows, b.id, vid), model="motorboat" if b is g.rc else None)
              for b in (g.rc, *g.boats)]
    tracks += [track(name(m), "marker", *on_site(rows, m.id, vid), model="mark") for m in g.marks]
    tracks += [track(name(b), "marker", *on_site(rows, b.id, vid), model="buoy") for b in g.buoys]
    ctx = {"id": g.venue.spec.code.lower(), "kind": "site", "layout": "venue", "body": EARTH.name,
           "name": name(g.venue), "frame": f"{name(g.venue)} ENU",
           "p": pose.p_km.tolist(), "q": quat_from_matrix(pose.r),
           "extent": list(sc.REGATTA_WIND.extent_m), "reach": VENUE_REACH_KM, "view": VENUE_VIEW_KM,
           "water": [[g.venue.enu(a, o)[:2].round(1).tolist() for a, o in ring] for ring in sc.BASIN_WATER],
           "line": [c.rc.tolist(), c.marks["MARK-PIN"].tolist()],
           "tracks": [t for t in tracks if t]}
    if wind_path.exists():
        ctx["wind"] = wind_block(g.venue, wind_path)
    return ctx


def wind_block(venue, wind_path: Path) -> dict:
    """The venue's rows of the wind table: the grid in venue ENU, and (u, v) per time, time-major."""
    w = read_table(wind_path)
    w = w.filter(pc.equal(w["venue"], venue.name))
    t = w["t"].cast(pa.int64()).to_numpy() - int((sc.T0 - datetime(1970, 1, 1)).total_seconds())
    times = np.unique(t)
    first = t == times[0]
    xy = np.array([venue.enu(a, o)[:2] for a, o in zip(w["lat"].to_numpy()[first], w["lon"].to_numpy()[first])])
    uv = np.column_stack([w["u"].to_numpy(), w["v"].to_numpy()])
    return {"t0": float(times[0]), "step": float(times[1] - times[0]), "n": len(times),
            "xy": b64(xy.ravel()), "uv": b64(uv.ravel())}


PART_SHAPES = {"STATOR": "stator", "SHAFT": "shaft", "ROTOR": "rotor", "IR": "ring", "OR": "ring",
               "CAGE": "ring", "BALL": "ball"}


def factory_context(fac, table, ledger_rows: Rows, name) -> dict:
    """The plant: machines and belts as its floor layout; the parts as tracks in their parents'
    frames, which the page composes (ball → cage → ring → stator → B → line → plant) and turns on
    by each row's spin; the boxes composed onto the plant floor here (their parents are static)."""
    rows = factory_rows(fac, table)
    order = {p.id: k for k, p in enumerate(fac.parts)}
    first = lambda n: rows.index[n][0]

    def to_plant(part) -> tuple[np.ndarray, np.ndarray]:
        """A static part's pose on the plant floor, from its stored rows."""
        r, p = np.eye(3), np.zeros(3)
        chain = []
        while part is not None:
            chain.append(part)
            part = next((q for q in fac.parts if q.id == part.parent_id), None)
        for q in reversed(chain):
            k = first(q.name)
            p = p + r @ rows.pos3[k]
            r = r @ quat_matrix(rows.quat[k])
        return p, r

    tracks, machines = [], []
    for p in fac.parts:
        k = rows.index[p.name]
        k = k[np.isclose(rows.t_s[k] % sc.PART_CADENCE_S, 0) | np.isclose(rows.t_s[k] % sc.PART_CADENCE_S, sc.PART_CADENCE_S)]
        shape = next((s for key, s in PART_SHAPES.items() if f"-{key}" in p.name), "none")
        tr = track(name(p), "part", rows.t_s[k], rows.pos3[k], rows.quat[k], model=shape)
        tr["parent"] = order.get(p.parent_id, -1)
        if p.rotating:
            tr["w"] = b64(rows.spin[k])
        tracks.append(tr)
    for line in fac.lines:
        for key, m in line["machines"].items():
            pos, r = to_plant(m)
            lx, ly, lz = sc.MACHINE_DIMENSIONS_M[key]
            centre = pos + r @ np.array([lx / 2 if key == "B" else 0.0, 0.0, lz / 2])
            machines.append({"c": centre.tolist(), "s": [lx, ly, lz], "belt": key == "B"})
        b_pos, b_r = to_plant(line["machines"]["B"])
        c_pos, c_r = to_plant(line["machines"]["C"])
        for b in line["boxes"]:
            k = rows.index[b.name]
            on_c = np.array([f == b.c_id for f in rows.frames[k]])
            xyz = np.where(on_c[:, None], c_pos + rows.pos3[k] @ c_r.T, b_pos + rows.pos3[k] @ b_r.T)
            tracks.append(track(name(b), "box", rows.t_s[k], xyz, np.tile([1.0, 0, 0, 0], (len(k), 1))))
    pose = Pose(ledger_rows, fac.plant.id)
    ys = [l["line"].offset[1] for l in fac.lines]
    return {"id": fac.plant.spec.code.lower(), "kind": "site", "layout": "plant", "body": EARTH.name,
            "name": name(fac.plant), "frame": f"{name(fac.plant)} ENU",
            "p": pose.p_km.tolist(), "q": quat_from_matrix(pose.r),
            "extent": [-6.0, sc.MACHINE_C_M[0] + 3, min(ys) - 4, max(ys) + 4], "machines": machines,
            "reach": PLANT_REACH_KM, "view": PLANT_VIEW_KM, "near": PLANT_NEAR_KM, "follow": PLANT_FOLLOW_KM,
            "tracks": tracks}


def fire_context(w, table, rows: Rows, name, wind_path: Path, fuel_path: Path) -> tuple[dict, list]:
    """The wildfire venue: the fuel map as its ground, the perimeter as a loop through the vertex
    tracks in replayed ring order, trenches appearing as they are finished, the crews, the wind.
    Also returns its timeline events."""
    pose, vid = Pose(rows, w.venue.id), w.venue.id
    view = FireView(w, SnapRows(table, w.names, vid, tai_ns_from_utc(sc.T0)))
    vertices = [v for v in w.vertices if len(rows.of(v.id))]
    order = {v.name: k for k, v in enumerate(vertices)}
    tracks = [track(name(c), "crew", *on_site(rows, c.id, vid)) for c in w.crews]
    tracks += [track(name(v), "fire", *on_site(rows, v.id, vid), model="none") for v in vertices]
    lines = sorted(view.lines, key=lambda l: l[0])
    ctx = {"id": w.venue.spec.code.lower(), "kind": "site", "layout": "venue", "body": EARTH.name,
           "name": name(w.venue), "frame": f"{name(w.venue)} ENU",
           "p": pose.p_km.tolist(), "q": quat_from_matrix(pose.r),
           "extent": list(sc.FIRE_WIND.extent_m), "reach": FIRE_REACH_KM, "view": FIRE_VIEW_KM,
           # Ring order over time, as indices into this context's tracks (the crews come first).
           "rings": [{"t": t, "i": [len(w.crews) + order[n] for n in ring]} for t, ring in view.replay.history],
           "trenches": {"t": [float(b) for b, _, _ in lines], "p": b64(np.array([[*a, *b] for _, a, b in lines]).ravel())},
           "tracks": [t for t in tracks if t]}
    if fuel_path.exists():
        fuel = fuel_grid(fuel_path, w.venue)
        ctx["fuel"] = {"step": sc.FUEL_GRID_M, "cols": fuel.shape[1], "f": b64(fuel.ravel())}
    if wind_path.exists():
        ctx["wind"] = wind_block(w.venue, wind_path)
    events = [{"t": wf.IGNITION_S, "label": "wildfire ignition"},
              {"t": wf.DISPATCH_S, "label": "wildfire: first crew orders"}]
    if lines:
        events.append({"t": float(lines[-1][0]), "label": "wildfire: last line finished"})
    if math.isfinite(view.end):
        events.append({"t": view.end, "label": "wildfire contained"})
    return ctx, events


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

    ledger, names = load(args.path)
    rows = Rows(ledger)
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
    g = world.regatta
    if len(rows.of(g.venue.id)):
        contexts.append(venue_context(g, rows, name, args.path.with_name("wind.arrow")))
    events = []
    w = world.wildfire
    if len(rows.of(w.venue.id)):
        ctx, fire_events = fire_context(w, ledger, rows, name, args.path.with_name("wind.arrow"),
                                        args.path.with_name("fuel.arrow"))
        contexts.append(ctx)
        events += fire_events
    fac = world.factory
    if len(rows.of(fac.plant.id)):
        contexts.append(factory_context(fac, ledger, rows, name))
        events += [{"t": float(fm.SHIFT_S[0]), "label": "factory shift starts"},
                   {"t": float(fm.BURST_S[0]), "label": f"factory {sc.BURST_HZ} Hz burst"},
                   {"t": float(fm.SHIFT_S[1]), "label": "factory shift ends"}]

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
    if len(rows.of(g.venue.id)):
        finishes = []
        for b in g.boats:
            k = rows.of(b.id)
            r, _ = rg.replay(g.course, rows.t_s[k], rows.pos[k, :2])
            if r.finish_s is not None:
                finishes.append(r.finish_s)
        events += [{"t": rg.LAID_S[0], "label": "regatta marks laid"},
                   {"t": rg.WARNING_S, "label": "regatta warning signal"},
                   {"t": rg.GUN_S, "label": "regatta start gun"},
                   {"t": rg.LAID_S[1], "label": "regatta marks lifted"}]
        if finishes:
            events += [{"t": min(finishes), "label": "regatta first finish"},
                       {"t": max(finishes), "label": "regatta last finish"}]

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
