"""Writes a standalone 3D viewer of a recorded duck ledger: one HTML file, opened offline by
double-click. The ducks are drawn on the Old Port site over its OSM map: MuJoCo truth, and a ghost
of where each duck's own odometry puts it (its boot frame composed with its odometry rows). The
trash is drawn where it is, and each report where soloc resolves it: the report row, stored in the
duck's odometry frame, exchanged to IAU_EARTH and taken into the site's ENU.

    ../sim_study_0/.venv/bin/python export_viewer.py           # out/microduck.arrow → out/microduck_3d.html

The Sun and Earth come from a running server (`serve.sh`), as in sim_study_0's viewer.
"""

import argparse
import base64
import json
import math
import time
import urllib.request
from io import BytesIO
from pathlib import Path

import numpy as np
from PIL import Image

import scenario as sc
from sim import scenario as sc0
from sim.ephemeris import Ephemeris
from sim.geo import EARTH, quat_from_matrix
from sim.models.facility import Facility
from soloc_client import SolocClient, positions, tai_ns_from_utc
from tools.export_viewer import THREE_JS, Pose, Rows, b64, coastlines, ephemeris, quat_matrix, track
from tools.view_sim import load

TEMPLATE = sc.HERE / "viewer_template.html"
EPHEMERIS_STEP_S = 10
REACH_KM, VIEW_KM, NEAR_KM, FOLLOW_KM = 0.5, 0.012, 2e-4, 0.002
PAD_M = 3.0                         # floor round the ducks' tracks

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


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("path", type=Path, nargs="?", default=sc.LEDGER)
    p.add_argument("--out", type=Path, help="default: <path stem>_3d.html beside the ledger")
    p.add_argument("--server", default="grpc://localhost:50051")
    args = p.parse_args()

    ledger, names = load(args.path)
    rows = Rows(ledger)
    rows.t_s = rows.t_s - (tai_ns_from_utc(sc.T0) - tai_ns_from_utc(sc0.T0)) / 1e9   # Rows counts from sim_study_0's T0
    site = Facility(sc.SITE)
    pose = Pose(rows, site.id)
    duration = float(rows.t_s.max())

    tracks, xy = [], []
    for duck in sc.DUCKS:
        did, oid = sc.entity(duck), sc.entity(sc.odom_name(duck))
        t, pos, quat = framed_m(rows, did, site.id)
        if not len(t):
            continue
        xy.append(pos[:, :2])
        tracks.append(track(duck, "duck", t, pos, quat, model="duck"))
        _, boot_p, boot_q = framed_m(rows, oid, site.id)
        r = quat_matrix(boot_q[0])
        t, pos, quat = framed_m(rows, did, oid)
        belief = boot_p[0] + pos @ r.T
        tracks.append(track(f"{duck} (odometry)", "belief", t, belief,
                            [quat_from_matrix(r @ quat_matrix(q)) for q in quat], model="duck"))
        tracks.append(track(sc.odom_name(duck), "frame", [0.0, duration], np.tile(boot_p[0], (2, 1)),
                            np.tile(boot_q[0], (2, 1))))
    client = SolocClient(args.server)
    odom_of = {sc.entity(sc.odom_name(d)): d for d in sc.DUCKS}
    events = []
    for item in sc.TRASH:
        tid = sc.entity(item)
        _, pos, _ = framed_m(rows, tid, site.id)
        tracks.append(track(item, "trash", [0.0, duration], np.tile(pos[0], (2, 1)), np.tile([1.0, 0, 0, 0], (2, 1))))
        tracks[-1]["solid"] = {"s": [0.15, 0.15, 0.1], "c": [0.0, 0.0, 0.05]}
        xy.append(pos[:, :2])
        k = rows.of(tid)
        k = k[np.array([f in odom_of for f in rows.frames[k]], bool)]
        if not len(k):
            continue
        out = client.exchange(ledger.take(k), EARTH.frame)
        enu = (positions(out) - site.position_km) @ site.basis * 1000
        for i, kk in enumerate(k):
            duck, t = odom_of[rows.frames[kk]], float(rows.t_s[kk])
            tracks.append(track(f"{item} as {duck} reported it", "report", [t, duration], np.tile(enu[i], (2, 1)),
                                np.tile([1.0, 0, 0, 0], (2, 1))))
            tracks[-1]["solid"] = {"s": [0.04, 0.04, 0.4], "c": [0.0, 0.0, 0.2]}
            events.append({"t": t, "label": f"{duck} points out {item}"})
            print(f"  {item} reported by {duck}: {np.linalg.norm(enu[i, :2] - pos[0, :2]) * 100:.1f} cm from the item")
    xy = np.vstack(xy)
    extent = [float(xy[:, 0].min() - PAD_M), float(xy[:, 0].max() + PAD_M),
              float(xy[:, 1].min() - PAD_M), float(xy[:, 1].max() + PAD_M)]
    ctx = {"id": sc.SITE.code.lower(), "kind": "site", "layout": "plant", "body": EARTH.name,
           "name": names.get(site.id, site.name), "frame": f"{site.name} ENU",
           "p": pose.p_km.tolist(), "q": quat_from_matrix(pose.r), "extent": extent, "machines": [],
           "reach": REACH_KM, "view": VIEW_KM, "near": NEAR_KM, "follow": FOLLOW_KM, "tracks": tracks}
    ground = osm_ground(site, extent)
    if ground:
        ctx["map"] = ground

    eph = Ephemeris(client)
    eph.t0_ns = tai_ns_from_utc(sc.T0)
    grid = np.arange(0, duration + EPHEMERIS_STEP_S, EPHEMERIS_STEP_S, dtype=float)
    bodies = []
    for body_name, naif, radius, colour in (("Sun", 10, 695700.0, "#ffcc66"), ("Earth", 399, EARTH.a_km, "#2c5a8f")):
        pos, q = ephemeris(eph, naif, grid)
        bodies.append({"name": body_name, "radius": radius, "color": colour,
                       "flattening": EARTH.f if body_name == "Earth" else 0.0,
                       "p": b64(pos.ravel(), np.float64), "q": b64(q.ravel())})

    data = {"t0": sc.T0.isoformat() + "Z", "duration": duration, "source": args.path.name,
            "ephemerisStep": EPHEMERIS_STEP_S, "bodies": bodies, "orbits": [], "arcs": [],
            "coast": coastlines(), "contexts": [ctx], "events": sorted(events, key=lambda e: e["t"]),
            "attribution": OSM_ATTRIBUTION if ground else ""}
    html = (TEMPLATE.read_text()
            .replace("/*THREE_JS*/", THREE_JS.read_text())
            .replace("/*DATA_JSON*/", json.dumps(data, separators=(",", ":"))))
    out = args.out or args.path.with_name(args.path.name.removesuffix(".arrow") + "_3d.html")
    out.write_text(html)
    print(f"{out}  ({out.stat().st_size / 1e6:.1f} MB, {len(tracks)} tracks)")


if __name__ == "__main__":
    main()
