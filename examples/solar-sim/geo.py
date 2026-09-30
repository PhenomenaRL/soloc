"""Body shapes, geodetic ↔ body-fixed conversion, local ENU bases and quaternions, two-body
orbits and the body spin.

Positions are km in the body's IAU body-fixed frame unless a name says otherwise. Quaternions
are `[w, x, y, z]` and rotate child-frame vectors into the parent frame (step-0 finding (c)).

"Inertial" here means the body's IAU frame frozen at `scenario.T0`; the live IAU frame spins
away from it about its z axis at the IAU prime-meridian rate. Libration and pole drift are
ignored, so the orbits wobble by tens of km on the Moon when viewed in ICRF.
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
    gm: float = 0.0       # km³/s²
    spin_deg_day: float = 0.0   # IAU prime-meridian rate W'

    @property
    def frame_id(self) -> bytes:
        return astronomical(self.naif, self.naif)

    @property
    def e2(self) -> float:
        return self.f * (2 - self.f)

    @property
    def spin_rad_s(self) -> float:
        return math.radians(self.spin_deg_day) / 86400


EARTH = Body("Earth", "IAU_EARTH", 399, 6378.137, 1 / 298.257223563,  # WGS84
             gm=398600.435436, spin_deg_day=360.9856235)
MOON = Body("Moon", "IAU_MOON", 301, 1737.4, gm=4902.800066, spin_deg_day=13.17635815)
MARS = Body("Mars", "IAU_MARS", 499, 3396.19, gm=42828.375214, spin_deg_day=350.891982443)
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


def rot_x(a: float) -> np.ndarray:
    c, s = math.cos(a), math.sin(a)
    return np.array([[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]])


def rot_z(a: float) -> np.ndarray:
    c, s = math.cos(a), math.sin(a)
    return np.array([[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]])


def spin(body: Body, t_s: float) -> np.ndarray:
    """Maps inertial vectors at `t_s` (seconds since T0) into the live body-fixed frame."""
    return rot_z(-body.spin_rad_s * t_s)


def fixed_state(body: Body, t_s: float, r: np.ndarray, v: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Inertial `(r km, v km/s)` → body-fixed, velocity relative to the rotating frame."""
    m = spin(body, t_s)
    return m @ r, m @ (v - np.cross([0.0, 0.0, body.spin_rad_s], r))


@dataclass(frozen=True)
class Kepler:
    """A two-body orbit in the body's inertial frame. Angles in degrees; `m_deg` is the mean
    anomaly at `epoch_s` (seconds since T0)."""
    body: Body
    a_km: float
    e: float
    i_deg: float
    raan_deg: float
    argp_deg: float
    m_deg: float
    epoch_s: float = 0.0

    @classmethod
    def from_altitudes(cls, body: Body, peri_alt_km: float, apo_alt_km: float, *args, **kwargs):
        rp, ra = body.a_km + peri_alt_km, body.a_km + apo_alt_km
        return cls(body, (rp + ra) / 2, (ra - rp) / (ra + rp), *args, **kwargs)

    @property
    def mean_motion(self) -> float:
        return math.sqrt(self.body.gm / self.a_km ** 3)

    @property
    def period_s(self) -> float:
        return 2 * math.pi / self.mean_motion

    @property
    def radius_range_km(self) -> tuple[float, float]:
        return self.a_km * (1 - self.e), self.a_km * (1 + self.e)

    @property
    def perifocal(self) -> np.ndarray:
        """Columns: periapsis direction, 90° ahead of it in the plane, orbit normal."""
        return (rot_z(math.radians(self.raan_deg)) @ rot_x(math.radians(self.i_deg))
                @ rot_z(math.radians(self.argp_deg)))

    @property
    def normal(self) -> np.ndarray:
        return self.perifocal[:, 2]

    def inertial(self, t_s: float) -> tuple[np.ndarray, np.ndarray]:
        n, e = self.mean_motion, self.e
        m = math.radians(self.m_deg) + n * (t_s - self.epoch_s)
        ecc = m
        for _ in range(20):
            step = (ecc - e * math.sin(ecc) - m) / (1 - e * math.cos(ecc))
            ecc -= step
            if abs(step) < 1e-13:
                break
        c, s, b = math.cos(ecc), math.sin(ecc), math.sqrt(1 - e * e)
        r_pf = self.a_km * np.array([c - e, b * s, 0.0])
        v_pf = n * self.a_km / (1 - e * c) * np.array([-s, b * c, 0.0])
        pqw = self.perifocal
        return pqw @ r_pf, pqw @ v_pf

    def fixed(self, t_s: float) -> tuple[np.ndarray, np.ndarray]:
        return fixed_state(self.body, t_s, *self.inertial(t_s))


def plane_through(direction: np.ndarray, i_deg: float, ascending: bool) -> tuple[float, float]:
    """`(raan_deg, u_deg)` of the orbit plane of inclination `i_deg` that contains the inertial
    `direction`, with `u` the argument of latitude there, on the northbound (ascending) or
    southbound pass."""
    x, y, z = direction / np.linalg.norm(direction)
    i = math.radians(i_deg)
    u = math.asin(z / math.sin(i))
    if not ascending:
        u = math.pi - u
    raan = math.atan2(y, x) - math.atan2(math.cos(i) * math.sin(u), math.cos(u))
    return math.degrees(raan) % 360, math.degrees(u) % 360


def lvlh(normal: np.ndarray, r: np.ndarray) -> np.ndarray:
    """Nadir-pointing LVLH axes as columns, in the frame of `normal` and `r`: z to nadir,
    y against the orbit normal, x completing the triad (the ram direction on a circular orbit)."""
    up = r / np.linalg.norm(r)
    h = normal - (normal @ up) * up
    y = -h / np.linalg.norm(h)
    z = -up
    return np.column_stack([np.cross(y, z), y, z])


def frd(lat_deg: float, lon_deg: float, heading_rad: float, pitch_rad: float = 0.0) -> np.ndarray:
    """Vehicle axes as columns in body-fixed axes: x forward along `heading` (clockwise from
    north) climbing at `pitch`, y to the right, z down (forward-right-down, as aircraft and
    ships use)."""
    sh, ch, sp, cp = math.sin(heading_rad), math.cos(heading_rad), math.sin(pitch_rad), math.cos(pitch_rad)
    fwd = np.array([sh * cp, ch * cp, sp])
    right = np.array([ch, -sh, 0.0])
    return enu_basis(lat_deg, lon_deg) @ np.column_stack([fwd, right, np.cross(fwd, right)])


TRACK_RADIUS_KM = 6371.0   # the sphere great-circle track lengths are measured on


def _unit(lat_deg: float, lon_deg: float) -> np.ndarray:
    lat, lon = math.radians(lat_deg), math.radians(lon_deg)
    return np.array([math.cos(lat) * math.cos(lon), math.cos(lat) * math.sin(lon), math.sin(lat)])


class GreatCircle:
    """A polyline of great-circle arcs through `(lat, lon)` waypoints (degrees), measured on a
    sphere of `TRACK_RADIUS_KM`. The latitudes it returns are placed on the WGS84 ellipsoid as
    geodetic ones, which is how the waypoints were read off a map in the first place."""

    def __init__(self, waypoints):
        self.waypoints = tuple(waypoints)
        self.u = [_unit(*w) for w in self.waypoints]
        arcs = [math.acos(float(np.clip(a @ b, -1, 1))) for a, b in zip(self.u, self.u[1:])]
        self.cum_km = np.concatenate([[0.0], np.cumsum(arcs)]) * TRACK_RADIUS_KM

    @property
    def length_km(self) -> float:
        return float(self.cum_km[-1])

    def reversed(self) -> "GreatCircle":
        return GreatCircle(self.waypoints[::-1])

    def at(self, s_km: float) -> tuple[float, float]:
        """`(lat, lon)` at `s_km` along the track, clamped to its ends."""
        s = min(max(s_km, 0.0), self.length_km)
        k = min(int(np.searchsorted(self.cum_km, s, side="right")) - 1, len(self.u) - 2)
        a, b = self.u[k], self.u[k + 1]
        omega = (self.cum_km[k + 1] - self.cum_km[k]) / TRACK_RADIUS_KM
        f = (s - self.cum_km[k]) / (self.cum_km[k + 1] - self.cum_km[k])
        u = (math.sin((1 - f) * omega) * a + math.sin(f * omega) * b) / math.sin(omega)
        return math.degrees(math.asin(u[2])), math.degrees(math.atan2(u[1], u[0]))


def _hermite(s: float) -> tuple[np.ndarray, np.ndarray]:
    """Cubic Hermite basis `(h00, h10, h01, h11)` and its derivative in `s`."""
    s2, s3 = s * s, s * s * s
    return (np.array([2 * s3 - 3 * s2 + 1, s3 - 2 * s2 + s, -2 * s3 + 3 * s2, s3 - s2]),
            np.array([6 * s2 - 6 * s, 3 * s2 - 4 * s + 1, -6 * s2 + 6 * s, 3 * s2 - 2 * s]))


class RadialHermite:
    """A path from state `(p0, v0)` to `(p1, v1)` over `duration_s`, matching both ends in
    position and velocity. Radius and direction are interpolated separately (cubic Hermite each),
    so the altitude profile is the radius cubic alone and the path never cuts under the surface
    the way a Cartesian spline between two points on a sphere does. km and km/s."""

    def __init__(self, p0, v0, p1, v1, duration_s: float):
        self.duration_s = duration_s
        r0, r1 = np.linalg.norm(p0), np.linalg.norm(p1)
        u0, u1 = p0 / r0, p1 / r1
        self.r = np.array([r0, (v0 @ u0) * duration_s, r1, (v1 @ u1) * duration_s])
        self.u = np.array([u0, (v0 - (v0 @ u0) * u0) / r0 * duration_s,
                           u1, (v1 - (v1 @ u1) * u1) / r1 * duration_s])

    def state(self, tau_s: float) -> tuple[np.ndarray, np.ndarray]:
        h, dh = _hermite(tau_s / self.duration_s)
        r, dr = h @ self.r, dh @ self.r / self.duration_s
        w, dw = h @ self.u, dh @ self.u / self.duration_s
        n = np.linalg.norm(w)
        u = w / n
        du = (dw - (u @ dw) * u) / n
        return r * u, dr * u + r * du
