"""Natural Earth 1:50m land polygons (data/ne_50m_land.geojson, from fetch_data.sh):
a point-in-land test for the ship checks and outlines for the plots.

At this scale the Suez and Panama canals are land, so `CANALS` names the boxes a lane may cross
them in; `on_land` ignores points inside those boxes.
"""

from functools import cache
import json

import numpy as np
from matplotlib.path import Path as MplPath

from sim import DATA

LAND = DATA / "ne_50m_land.geojson"

# (lat_min, lat_max, lon_min, lon_max)
CANALS = {
    "Suez": (29.85, 31.35, 32.15, 32.70),
    "Panama": (8.85, 9.45, -80.00, -79.45),
}


@cache
def polygons() -> list[tuple[MplPath, list[MplPath], tuple[float, float, float, float]]]:
    """`(exterior, holes, (lon_min, lon_max, lat_min, lat_max))` per polygon, in lon/lat."""
    out = []
    for feature in json.loads(LAND.read_text())["features"]:
        geom = feature["geometry"]
        parts = [geom["coordinates"]] if geom["type"] == "Polygon" else geom["coordinates"]
        for rings in parts:
            ext = np.array(rings[0])
            out.append((MplPath(ext), [MplPath(np.array(r)) for r in rings[1:]],
                        (ext[:, 0].min(), ext[:, 0].max(), ext[:, 1].min(), ext[:, 1].max())))
    return out


def in_canal(lat: np.ndarray, lon: np.ndarray) -> np.ndarray:
    lat, lon = np.asarray(lat), np.asarray(lon)
    hit = np.zeros(lat.shape, bool)
    for la0, la1, lo0, lo1 in CANALS.values():
        hit |= (lat >= la0) & (lat <= la1) & (lon >= lo0) & (lon <= lo1)
    return hit


def on_land(lat, lon) -> np.ndarray:
    """Boolean mask: which `(lat, lon)` degree pairs fall on land outside the canal boxes."""
    lat, lon = np.atleast_1d(lat).astype(float), np.atleast_1d(lon).astype(float)
    pts = np.column_stack([lon, lat])
    land = np.zeros(len(pts), bool)
    for ext, holes, (lo0, lo1, la0, la1) in polygons():
        near = (lon >= lo0) & (lon <= lo1) & (lat >= la0) & (lat <= la1) & ~land
        if not near.any():
            continue
        k = np.flatnonzero(near)
        inside = ext.contains_points(pts[k])
        for hole in holes:
            inside &= ~hole.contains_points(pts[k])
        land[k[inside]] = True
    return land & ~in_canal(lat, lon)


def outlines():
    """Every ring as an `(N, 2)` lon/lat array, for drawing."""
    return [p.vertices for ext, holes, _ in polygons() for p in (ext, *holes)]
