"""Writes a standalone 3D viewer of a recorded duck ledger: one HTML file, opened offline by
double-click. Two local frames: the Old Port site (OSM map and buildings) and, on a 2nd floor
inside it, Pollen's apartment with its walls and furniture. The ducks are drawn in the apartment: MuJoCo truth, and a ghost
of where each duck's own odometry puts it (its boot frame composed with its odometry rows). The
trash is drawn where it is, and each report where soloc resolves it: the report row, stored in the
duck's odometry frame, exchanged to IAU_EARTH (through the apartment and the site) and taken back
into the apartment. On an aligned ledger,
each sighting is a short ray, and each duck has a third ghost: placed through the lookout's latest
estimate of its odometry frame. With the joint sidecar beside the ledger, the truth duck is Pollen's
own model, walking: its links posed by MuJoCo's forward kinematics from the recorded trunk pose and
joint angles. The model's meshes are CC BY-NC-SA: they go into the generated page only. The OSM
buildings round the site (`buildings.py`) stand on the map as faint volumes.

    ../sim_study_0/.venv/bin/python export_viewer.py           # out/microduck_aligned.arrow if present, else out/microduck.arrow

The Sun, planets, Moon and their orbits come from a running server (`serve.sh`), as in sim_study_0's viewer.
"""

import argparse
import base64
import json
import math
import sys
import time
import urllib.request
from io import BytesIO
from pathlib import Path

import mujoco
import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
from PIL import Image

import apartment as ap
import scenario as sc
from buildings import buildings

sys.path.insert(0, str(sc.SIM_RL / "src"))
from mjlab_microduck.sim.body_server import JOINT_NAMES, ROBOT_ONLY  # noqa: E402
from sim import scenario as sc0
from sim.ephemeris import Ephemeris
from sim.geo import EARTH, quat_from_matrix
from sim.models.facility import Facility
from soloc_client import SolocClient, astronomical, id_bytes, positions, tai_ns_from_utc
from tools.export_viewer import (BODIES, FLATTENING, THREE_JS, Pose, Rows, b64, coastlines, ephemeris,
                                 quat_matrix, track)
from tools.view_sim import load

TEMPLATE = sc.HERE / "viewer_template.html"
EPHEMERIS_STEP_S = 10
REACH_KM, VIEW_KM, NEAR_KM, FOLLOW_KM = 0.5, 0.012, 3e-5, 0.002
PAD_M = 3.0                         # floor round the ducks' tracks
RAY_S = 1.0                         # how long a sighting's ray stays drawn
RAY_Z_M = 0.1
GAIT_HZ = 25                        # link poses kept from the 50 Hz sidecar; the page interpolates
VISUAL_GROUP = 2                    # the model's visual meshes (group 3 is the collision copy)
CREASE_DEG = 35.0                   # face normals further apart than this stay a hard edge

# The site's ground: OSM raster tiles, fetched once into data/osm_tiles/ (OSM's tile policy: light
# use, a real User-Agent, cached, attributed) and embedded as one mosaic.
OSM_TILES = sc.HERE / "data" / "osm_tiles"
OSM_URL = "https://tile.openstreetmap.org/{z}/{x}/{y}.png"
OSM_AGENT = "soloc-microduck_sil-viewer/1.0 (one-off export; tiles cached locally)"
OSM_ATTRIBUTION = "Map tiles © OpenStreetMap contributors"
MAP_MIN_M = 120.0                   # the map covers at least this square
MAP_PAD_M = 20.0                    # round the ducks
MAP_TILES = 36                      # at the finest zoom that needs no more tiles than this
MAP_GRID = 9                        # the mesh the mosaic is warped onto (vertices per side)


def tile_xy(lat: float, lon: float, z: int) -> tuple[float, float]:
    """Web Mercator tile coordinates (fractional) of a point at zoom `z`."""
    n = 2 ** z
    return (lon + 180) / 360 * n, (1 - math.asinh(math.tan(math.radians(lat))) / math.pi) / 2 * n


def tile_latlon(x: float, y: float, z: int) -> tuple[float, float]:
    n = 2 ** z
    return math.degrees(math.atan(math.sinh(math.pi * (1 - 2 * y / n)))), x / n * 360 - 180


def osm_tile(z: int, x: int, y: int) -> Path:
    """One OSM tile, from the cache or fetched into it."""
    path = OSM_TILES / f"{z}/{x}/{y}.png"
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        req = urllib.request.Request(OSM_URL.format(z=z, x=x, y=y), headers={"User-Agent": OSM_AGENT})
        with urllib.request.urlopen(req, timeout=30) as r:
            path.write_bytes(r.read())
        time.sleep(0.1)
    return path


def osm_ground(site, extent) -> dict | None:
    """The OSM map round the ducks: a mosaic of tiles at the finest zoom that covers it in
    `MAP_TILES`, as a JPEG, and the site-frame (m) positions of a `MAP_GRID`² mesh over it, so
    the page warps the mosaic onto the site's ground exactly. `None` if a tile can't be had."""
    x0, x1, y0, y1 = extent
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    half = max((x1 - x0) / 2 + MAP_PAD_M, (y1 - y0) / 2 + MAP_PAD_M, MAP_MIN_M / 2)
    lat, lon = site.geodetic([cx - half, cx + half, cx - half, cx + half], [cy - half, cy - half, cy + half, cy + half])
    for z in range(19, 9, -1):
        xs, ys = zip(*(tile_xy(a, b, z) for a, b in zip(lat, lon)))
        tx, ty = range(int(min(xs)), int(max(xs)) + 1), range(int(min(ys)), int(max(ys)) + 1)
        if len(tx) * len(ty) <= MAP_TILES:
            break
    try:
        mosaic = Image.new("RGB", (256 * len(tx), 256 * len(ty)))
        for i, x in enumerate(tx):
            for j, y in enumerate(ty):
                mosaic.paste(Image.open(osm_tile(z, x, y)).convert("RGB"), (256 * i, 256 * j))
    except OSError as e:
        print(f"  no OSM map for {site.name}: {e}")
        return None
    buf = BytesIO()
    mosaic.save(buf, "JPEG", quality=80)
    xy = []
    for j in range(MAP_GRID):                 # rows from the mosaic's top (north)
        for i in range(MAP_GRID):
            a, b = tile_latlon(tx[0] + i / (MAP_GRID - 1) * len(tx), ty[0] + j / (MAP_GRID - 1) * len(ty), z)
            xy.append(site.enu(a, b, site.spec.h_km)[:2])
    print(f"  OSM map {site.name}: zoom {z}, {len(tx)}×{len(ty)} tiles")
    return {"img": "data:image/jpeg;base64," + base64.b64encode(buf.getvalue()).decode(), "n": MAP_GRID,
            "xy": b64(np.ravel(xy)), "half": half}


def framed_m(rows: Rows, entity_id: bytes, frame_id: bytes):
    """`(t_s, xyz m, quaternion)` of one entity's rows in one frame."""
    k = rows.of(entity_id)
    k = k[np.array([f == frame_id for f in rows.frames[k]], bool)]
    return rows.t_s[k], rows.pos[k] * rows.km[k][:, None] * 1000, rows.quat[k]


def duck_model(model: mujoco.MjModel) -> list[list[dict]]:
    """Per body (1…), its visual meshes in the body's frame, one per colour, at full detail."""
    parts: dict[tuple[int, tuple], list] = {}
    for g in range(model.ngeom):
        if model.geom_group[g] != VISUAL_GROUP or model.geom_type[g] != mujoco.mjtGeom.mjGEOM_MESH:
            continue
        mid = model.geom_dataid[g]
        v = model.mesh_vert[model.mesh_vertadr[mid]:model.mesh_vertadr[mid] + model.mesh_vertnum[mid]]
        f = model.mesh_face[model.mesh_faceadr[mid]:model.mesh_faceadr[mid] + model.mesh_facenum[mid]]
        r = np.zeros(9)
        mujoco.mju_quat2Mat(r, model.geom_quat[g])
        v = v @ r.reshape(3, 3).T + model.geom_pos[g]
        mat = model.geom_matid[g]
        rgba = model.mat_rgba[mat] if mat >= 0 else model.geom_rgba[g]
        parts.setdefault((int(model.geom_bodyid[g]), tuple(np.round(rgba[:3], 3))), []).append((v, f))
    bodies = [[] for _ in range(model.nbody - 1)]
    faces = 0
    for (body, rgb), pieces in parts.items():
        offsets = np.cumsum([0] + [len(v) for v, _ in pieces])
        v = np.vstack([v for v, _ in pieces])
        f = np.vstack([f + o for (_, f), o in zip(pieces, offsets)])
        v, n, f = crease_normals(v, f, CREASE_DEG)
        faces += len(f)
        bodies[body - 1].append({"color": "#%02x%02x%02x" % tuple(int(255 * c) for c in rgb),
                                 "v": b64(v.ravel()), "n": b64(n.ravel()),
                                 "i": b64(f.ravel(), np.uint32)})
    print(f"  duck model: {model.nbody - 1} links, {faces} faces")
    return bodies


def crease_normals(v: np.ndarray, f: np.ndarray, crease_deg: float):
    """Smooth vertex normals that keep hard edges: each face corner averages (area-weighted) the
    normals of the faces at its vertex within `crease_deg` of its own face; a vertex whose corners
    disagree is split. Returns (vertices, normals, faces)."""
    fn = np.cross(v[f[:, 1]] - v[f[:, 0]], v[f[:, 2]] - v[f[:, 0]])
    unit = fn / np.maximum(np.linalg.norm(fn, axis=1), 1e-30)[:, None]
    vid, fid = f.ravel(), np.repeat(np.arange(len(f)), 3)
    order = np.argsort(vid, kind="stable")
    count = np.bincount(vid, minlength=len(v))
    size = count[vid[order]]                                        # group size per sorted corner
    start = np.repeat(np.cumsum(count) - count, count)              # group start per sorted corner
    # every (corner, corner of the same vertex) pair, in sorted order
    a = np.repeat(np.arange(len(order)), size)
    b = start[a] + (np.arange(len(a)) - np.repeat(np.cumsum(size) - size, size))
    ca, cb = order[a], order[b]
    keep = np.einsum("ij,ij->i", unit[fid[ca]], unit[fid[cb]]) >= math.cos(math.radians(crease_deg))
    n = np.zeros((len(vid), 3))
    np.add.at(n, ca[keep], fn[fid[cb[keep]]])
    n /= np.maximum(np.linalg.norm(n, axis=1), 1e-30)[:, None]
    key = np.column_stack([vid, np.round(n * 1000).astype(np.int64)])
    _, first, inv = np.unique(key, axis=0, return_index=True, return_inverse=True)
    return v[vid[first]], n[first], inv.ravel().reshape(-1, 3)


def gait(model: mujoco.MjModel, joints_path: Path) -> list[dict]:
    """Per duck, its links' poses in the site frame at `GAIT_HZ`: the sidecar's trunk pose and
    joint angles set on the model, then MuJoCo's forward kinematics."""
    table = pa.ipc.open_file(joints_path).read_all()
    data = mujoco.MjData(model)
    free = model.jnt_qposadr[mujoco.mj_name2id(model, mujoco.mjtObj.mjOBJ_JOINT, "trunk_base_freejoint")]
    slots = [(w, model.jnt_qposadr[j]) for w, n in enumerate(JOINT_NAMES)
             if (j := mujoco.mj_name2id(model, mujoco.mjtObj.mjOBJ_JOINT, n)) >= 0]
    t0 = tai_ns_from_utc(sc.T0)
    every = sc.JOINT_HZ // GAIT_HZ
    out = []
    for duck in sc.DUCKS:
        rows = table.filter(pc.equal(table["duck"], duck))
        if not rows.num_rows:
            continue
        t = (rows["t_tai_ns"].to_numpy()[::every] - t0) / 1e9
        trunk = np.array(rows["trunk"].to_pylist())[::every]
        quat = np.array(rows["quat"].to_pylist())[::every]
        wire = np.array(rows["positions"].to_pylist())[::every]
        p, q = np.zeros((len(t), model.nbody - 1, 3)), np.zeros((len(t), model.nbody - 1, 4))
        for i in range(len(t)):
            data.qpos[free:free + 3], data.qpos[free + 3:free + 7] = trunk[i], quat[i]
            for w, adr in slots:
                data.qpos[adr] = wire[i, w]
            mujoco.mj_kinematics(model, data)
            p[i], q[i] = data.xpos[1:], data.xquat[1:]
        out.append({"name": duck, "t": b64(t), "p": b64(p.ravel()), "q": b64(q.ravel())})
    return out


def estimate_tracks(rows: Rows, site_id: bytes) -> list[dict]:
    """Each duck as the lookout places it: the lookout's boot frame ∘ its latest estimate of the
    duck's odometry frame ∘ the duck's odometry rows."""
    look = sc.entity(sc.odom_name(sc.LOOKOUT))
    _, boot_p, boot_q = framed_m(rows, look, site_id)
    if not len(boot_p):
        return []
    rb, out = quat_matrix(boot_q[0]), []
    for duck in sc.DUCKS:
        te, pe, qe = framed_m(rows, sc.entity(sc.estimate_name(duck)), look)
        if duck == sc.LOOKOUT or not len(te):
            continue
        t, pos, quat = framed_m(rows, sc.entity(duck), sc.entity(sc.odom_name(duck)))
        keep = t >= te[0]
        t, pos, quat = t[keep], pos[keep], quat[keep]
        k = np.searchsorted(te, t, side="right") - 1
        p_site, q_site = [], []
        for i in range(len(t)):
            re = quat_matrix(qe[k[i]])
            r = rb @ re @ quat_matrix(quat[i])
            p_site.append(boot_p[0] + rb @ (pe[k[i]] + re @ pos[i]))
            q_site.append(quat_from_matrix(r))
        out.append(track(f"{duck} (via {sc.LOOKOUT}'s estimate)", "estimate", t, np.array(p_site), np.array(q_site),
                         model="duck"))
    return out


def sighting_rays(rows: Rows, site_id: bytes) -> list[dict]:
    """Each sighting as a segment from the observer (truth) to where it saw the target, taken into
    the site through the observer's boot frame. Per observer, sorted by time, for the page to draw
    a window."""
    odom_of = {sc.entity(sc.odom_name(d)): d for d in sc.DUCKS}
    segs = {d: [] for d in sc.DUCKS}
    for target in sc.DUCKS:
        k = rows.of(sc.entity(target))
        k = k[np.array([f in odom_of and odom_of[f] != target for f in rows.frames[k]], bool)]
        for kk in k:
            observer, t = odom_of[rows.frames[kk]], float(rows.t_s[kk])
            _, bp, bq = framed_m(rows, rows.frames[kk], site_id)
            end = bp[0] + quat_matrix(bq[0]) @ (rows.pos[kk] * rows.km[kk] * 1000)
            ot, opos, _ = framed_m(rows, sc.entity(observer), site_id)
            start = [np.interp(t, ot, opos[:, 0]), np.interp(t, ot, opos[:, 1])]
            segs[observer].append((t, *start, end[0], end[1]))
    out = []
    for observer, ss in segs.items():
        if ss:
            s = np.array(sorted(ss))
            out.append({"observer": observer, "t": s[:, 0].tolist(), "p": b64(s[:, 1:].ravel()),
                        "z": RAY_Z_M, "life": RAY_S})
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("path", type=Path, nargs="?", help="default: the run's aligned ledger if present, else its recording")
    p.add_argument("--out", type=Path, help="default: <path stem>_3d.html beside the ledger")
    p.add_argument("--server", default="grpc://localhost:50051")
    p.add_argument("--run", type=int, default=1, help="which recording (see scenario.set_run)")
    args = p.parse_args()
    sc.set_run(args.run)
    args.path = args.path or (sc.ALIGNED if sc.ALIGNED.exists() else sc.LEDGER)

    ledger, names = load(args.path)
    rows = Rows(ledger)
    rows.t_s = rows.t_s - (tai_ns_from_utc(sc.T0) - tai_ns_from_utc(sc0.T0)) / 1e9   # Rows counts from sim_study_0's T0
    site = Facility(sc.SITE)
    pose = Pose(rows, site.id)
    apt = sc.entity(sc.APARTMENT)
    _, apt_p, apt_q = framed_m(rows, apt, site.id)          # the apartment's pose in the site, from the ledger
    apt_t, apt_r = apt_p[0], quat_matrix(apt_q[0])
    duration = float(rows.t_s.max())

    real = sc.JOINTS.exists()           # the walking model draws the truth duck; its track keeps the label
    tracks, xy = [], []
    for duck in sc.DUCKS:
        did, oid = sc.entity(duck), sc.entity(sc.odom_name(duck))
        t, pos, quat = framed_m(rows, did, apt)
        if not len(t):
            continue
        xy.append(pos[:, :2])
        tracks.append(track(duck, "duck", t, pos, quat, model="real" if real else "duck"))
        _, boot_p, boot_q = framed_m(rows, oid, apt)
        r = quat_matrix(boot_q[0])
        t, pos, quat = framed_m(rows, did, oid)
        belief = boot_p[0] + pos @ r.T
        tracks.append(track(f"{duck} (odometry)", "belief", t, belief,
                            [quat_from_matrix(r @ quat_matrix(q)) for q in quat], model="duck"))
        tracks.append(track(sc.odom_name(duck), "frame", [0.0, duration], np.tile(boot_p[0], (2, 1)),
                            np.tile(boot_q[0], (2, 1))))
    tracks += estimate_tracks(rows, apt)
    rays = sighting_rays(rows, apt)
    client = SolocClient(args.server)
    odom_of = {sc.entity(sc.odom_name(d)): d for d in sc.DUCKS}
    events = []
    for item in sc.TRASH:
        tid = sc.entity(item)
        _, pos, _ = framed_m(rows, tid, apt)
        tracks.append(track(item, "trash", [0.0, duration], np.tile(pos[0], (2, 1)), np.tile([1.0, 0, 0, 0], (2, 1))))
        tracks[-1]["solid"] = {"s": [0.15, 0.15, 0.1], "c": [0.0, 0.0, 0.05]}
        xy.append(pos[:, :2])
        k = rows.of(tid)
        k = k[np.array([f in odom_of for f in rows.frames[k]], bool)]
        if not len(k):
            continue
        out = client.exchange(ledger.take(k), EARTH.frame)
        enu = ((positions(out) - site.position_km) @ site.basis * 1000 - apt_t) @ apt_r   # back into the apartment
        for i, kk in enumerate(k):
            duck, t = odom_of[rows.frames[kk]], float(rows.t_s[kk])
            tracks.append(track(f"{item} as {duck} reported it", "report", [t, duration], np.tile(enu[i], (2, 1)),
                                np.tile([1.0, 0, 0, 0], (2, 1))))
            tracks[-1]["solid"] = {"s": [0.04, 0.04, 0.4], "c": [0.0, 0.0, 0.2]}
            events.append({"t": t, "label": f"{duck} points out {item}"})
            print(f"  {item} reported by {duck}: {np.linalg.norm(enu[i, :2] - pos[0, :2]) * 100:.1f} cm from the item")
    # Two local frames: the site (OSM map and buildings) and, inside it, the apartment (Pollen's
    # scene, the ducks and the trash), each a context placed on the Earth.
    corners = np.array([[x, y, 0.0] for x in (-4.0, 4.0) for y in (-3.0, 3.0)]) @ apt_r.T + apt_t
    extent = [float(corners[:, 0].min() - PAD_M), float(corners[:, 0].max() + PAD_M),
              float(corners[:, 1].min() - PAD_M), float(corners[:, 1].max() + PAD_M)]
    common = {"kind": "site", "layout": "plant", "body": EARTH.name, "machines": [],
              "reach": REACH_KM, "view": VIEW_KM, "near": NEAR_KM, "follow": FOLLOW_KM}
    site_ctx = {**common, "id": sc.SITE.code.lower(), "name": names.get(site.id, site.name),
                "frame": f"{site.name} ENU", "p": pose.p_km.tolist(), "q": quat_from_matrix(pose.r),
                "extent": extent, "tracks": []}
    xy = np.vstack(xy)
    apt_ctx = {**common, "id": "apt", "name": names.get(apt, sc.APARTMENT), "frame": "apartment (Pollen's scene)",
               "p": (pose.p_km + pose.r @ apt_t / 1000).tolist(), "q": quat_from_matrix(pose.r @ apt_r),
               "extent": [float(xy[:, 0].min() - PAD_M), float(xy[:, 0].max() + PAD_M),
                          float(xy[:, 1].min() - PAD_M), float(xy[:, 1].max() + PAD_M)],
               "noFloor": True, "room": ap.viewer_geoms(), "tracks": tracks,
               "fov": {"deg": sc.FOV_DEG, "range": sc.SIGHT_RANGE_M, "z": sc.SIGHT_Z_M}}
    if rays:
        apt_ctx["rays"] = rays
    if real:
        model = mujoco.MjModel.from_xml_path(str(ROBOT_ONLY))
        apt_ctx["articulated"] = {"bodies": duck_model(model), "ducks": gait(model, sc.JOINTS)}
    ground = osm_ground(site, extent)
    if ground:
        site_ctx["map"] = ground
        site_ctx["buildings"] = [{"name": b["name"], "ring": b["ring"], "h": b["h"]} for b in buildings(site)]

    eph = Ephemeris(client)
    eph.t0_ns = tai_ns_from_utc(sc.T0)
    grid = np.arange(0, duration + EPHEMERIS_STEP_S, EPHEMERIS_STEP_S, dtype=float)
    # sim_study_0's bodies and their orbits, so the page zooms in from the inner solar system
    bodies = []
    for body_name, naif, radius, colour, _ in BODIES:
        pos, q = ephemeris(eph, naif, grid)
        bodies.append({"name": body_name, "radius": radius, "color": colour,
                       "flattening": FLATTENING.get(body_name, 0.0),
                       "p": b64(pos.ravel(), np.float64), "q": b64(q.ravel())})
    by_id = {astronomical(naif, naif): body_name for body_name, naif, *_ in BODIES}
    table = client.orbits([astronomical(naif, naif) for _, naif, *_, orbit in BODIES if orbit],
                          tai_ns_from_utc(sc.T0) + int(duration / 2) * 10**9)
    orbits = [{"name": by_id[b], "centre": by_id[c], "p": b64(np.array(path).ravel())}
              for b, c, path in zip(id_bytes(table.column("body_id")), id_bytes(table.column("centre_id")),
                                    table.column("path_km").to_pylist())]

    data = {"t0": sc.T0.isoformat() + "Z", "duration": duration, "source": args.path.name,
            "ephemerisStep": EPHEMERIS_STEP_S, "bodies": bodies, "orbits": orbits, "arcs": [],
            "coast": coastlines(), "contexts": [site_ctx, apt_ctx], "events": sorted(events, key=lambda e: e["t"]),
            "attribution": OSM_ATTRIBUTION if ground else ""}
    html = (TEMPLATE.read_text()
            .replace("/*THREE_JS*/", THREE_JS.read_text())
            .replace("/*DATA_JSON*/", json.dumps(data, separators=(",", ":"))))
    out = args.out or args.path.with_name(args.path.name.removesuffix(".arrow") + "_3d.html")
    out.write_text(html)
    print(f"{out}  ({out.stat().st_size / 1e6:.1f} MB, {len(tracks)} tracks)")


if __name__ == "__main__":
    main()
