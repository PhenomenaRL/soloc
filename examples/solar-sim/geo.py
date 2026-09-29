"""Body shapes, geodetic ↔ body-fixed conversion, local ENU bases and quaternions.

Positions are km in the body's IAU body-fixed frame unless a name says otherwise. Quaternions
are `[w, x, y, z]` and rotate child-frame vectors into the parent frame (step-0 finding (c)).
"""

import math
from dataclasses import dataclass

import numpy as np

from soloc_client import astronomical


@dataclass(frozen=True)
class Body:
    name: str
    frame: str            # the target_frame name do_exchange accepts
    naif: int
    a_km: float           # equatorial radius
    f: float = 0.0        # flattening; 0 for a sphere

    @property
    def frame_id(self) -> bytes:
        return astronomical(self.naif, self.naif)

    @property
    def e2(self) -> float:
        return self.f * (2 - self.f)


EARTH = Body("Earth", "IAU_EARTH", 399, 6378.137, 1 / 298.257223563)  # WGS84
MOON = Body("Moon", "IAU_MOON", 301, 1737.4)
MARS = Body("Mars", "IAU_MARS", 499, 3396.19)
SUN = Body("Sun", "IAU_SUN", 10, 695700.0)


def geodetic_to_fixed(body: Body, lat_deg: float, lon_deg: float, h_km: float = 0.0) -> np.ndarray:
    lat, lon = math.radians(lat_deg), math.radians(lon_deg)
    n = body.a_km / math.sqrt(1 - body.e2 * math.sin(lat) ** 2)
    return np.array([
        (n + h_km) * math.cos(lat) * math.cos(lon),
        (n + h_km) * math.cos(lat) * math.sin(lon),
        (n * (1 - body.e2) + h_km) * math.sin(lat),
    ])


def fixed_to_geodetic(body: Body, xyz: np.ndarray) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """`(lat_deg, lon_deg, h_km)` for an `(N, 3)` array of body-fixed km, by fixed-point iteration."""
    xyz = np.atleast_2d(xyz)
    x, y, z = xyz[:, 0], xyz[:, 1], xyz[:, 2]
    p = np.hypot(x, y)
    lon = np.arctan2(y, x)
    lat = np.arctan2(z, p * (1 - body.e2))

    def height(lat):
        n = body.a_km / np.sqrt(1 - body.e2 * np.sin(lat) ** 2)
        # p / cos(lat) blows up at the poles; there the z form is the stable one.
        with np.errstate(divide="ignore", invalid="ignore"):
            return n, np.where(np.abs(lat) < math.radians(45),
                               p / np.cos(lat) - n,
                               z / np.sin(lat) - n * (1 - body.e2))

    for _ in range(6):
        n, h = height(lat)
        lat = np.arctan2(z, p * (1 - body.e2 * n / (n + h)))
    return np.degrees(lat), np.degrees(lon), height(lat)[1]


def enu_basis(lat_deg: float, lon_deg: float) -> np.ndarray:
    """3×3 whose columns are east, north, up in body-fixed axes: maps ENU vectors to body-fixed."""
    lat, lon = math.radians(lat_deg), math.radians(lon_deg)
    sl, cl, so, co = math.sin(lat), math.cos(lat), math.sin(lon), math.cos(lon)
    return np.array([
        [-so, -sl * co, cl * co],
        [co, -sl * so, cl * so],
        [0.0, cl, sl],
    ])


def quat_from_matrix(r: np.ndarray) -> list[float]:
    """`[w, x, y, z]` of a rotation matrix (Shepperd's method), with w ≥ 0."""
    t = np.trace(r)
    if t > 0:
        s = 2 * math.sqrt(1 + t)
        q = [s / 4, (r[2, 1] - r[1, 2]) / s, (r[0, 2] - r[2, 0]) / s, (r[1, 0] - r[0, 1]) / s]
    else:
        i = int(np.argmax(np.diag(r)))
        j, k = (i + 1) % 3, (i + 2) % 3
        s = 2 * math.sqrt(1 + r[i, i] - r[j, j] - r[k, k])
        q = [0.0] * 4
        q[0] = (r[k, j] - r[j, k]) / s
        q[1 + i] = s / 4
        q[1 + j] = (r[j, i] + r[i, j]) / s
        q[1 + k] = (r[k, i] + r[i, k]) / s
    if q[0] < 0:
        q = [-c for c in q]
    return [float(c) for c in q]


def quat_yaw(yaw_rad: float) -> list[float]:
    """Rotation about the parent's z (up) axis: body x points `yaw_rad` counter-clockwise from x (east)."""
    return [math.cos(yaw_rad / 2), 0.0, 0.0, math.sin(yaw_rad / 2)]
