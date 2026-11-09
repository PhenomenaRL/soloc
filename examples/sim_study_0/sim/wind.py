"""Analytic wind, one field per venue (`scenario.WindSpec`): a mean wind whose direction shifts
sinusoidally and whose speed builds linearly, scaled up by Gaussian puffs that drift with the
mean wind. Positions are venue ENU metres, time is seconds since T0, and directions are compass
degrees the wind blows from.

`write_table` samples every field onto a grid for `out/wind.arrow`.
"""

import math
from pathlib import Path

import numpy as np
import pyarrow as pa

from sim import scenario as sc
from sim.models.facility import Facility
from sim.models.robot import own_rng


class Wind:
    def __init__(self, spec: sc.WindSpec, venue: Facility, seed: int):
        self.spec, self.venue = spec, venue
        self.on_s, self.off_s = sc.seconds(spec.on), sc.seconds(spec.off)
        rng = own_rng(seed, f"wind/{spec.venue}")
        self.phase = rng.uniform(0, 2 * math.pi)
        e0, e1, n0, n1 = spec.extent_m
        self.lo = np.array([e0, n0])
        self.size = np.array([e1 - e0, n1 - n0])
        self.puff0 = self.lo + rng.random((spec.puffs, 2)) * self.size
        d = math.radians(spec.twd_deg)
        self.drift = -spec.tws_m_s * np.array([math.sin(d), math.cos(d)])   # m/s, downwind

    def _clip(self, t_s: float) -> float:
        return min(max(t_s, self.on_s), self.off_s)

    def mean(self, t_s: float) -> tuple[float, float]:
        """`(twd_deg, tws_m_s)` without the puffs."""
        s, t = self.spec, self._clip(t_s)
        twd = s.twd_deg + s.shift_deg * math.sin(2 * math.pi * t / s.shift_period_s + self.phase)
        return twd % 360, s.tws_m_s + s.build_m_s_h * (t - self.on_s) / 3600

    def puffs(self, t_s: float) -> np.ndarray:
        """`(K, 2)` puff centres, wrapped into the extent."""
        return self.lo + (self.puff0 - self.lo + self.drift * (self._clip(t_s) - self.on_s)) % self.size

    def at(self, east_m, north_m, t_s: float) -> tuple[np.ndarray, np.ndarray]:
        """`(twd_deg, tws_m_s)` at venue ENU points (any matching shapes)."""
        twd, tws = self.mean(t_s)
        e, n = np.asarray(east_m, float), np.asarray(north_m, float)
        c = self.puffs(t_s)
        d2 = (e[..., None] - c[:, 0]) ** 2 + (n[..., None] - c[:, 1]) ** 2
        gain = 1 + self.spec.puff_gain * np.exp(-d2 / (2 * self.spec.puff_radius_m ** 2)).sum(-1)
        return np.full(e.shape, twd), tws * gain

    def uv(self, east_m, north_m, t_s: float) -> tuple[np.ndarray, np.ndarray]:
        """`(u east, v north)` m/s, the way the air moves."""
        twd, tws = self.at(east_m, north_m, t_s)
        d = np.radians(twd)
        return -tws * np.sin(d), -tws * np.cos(d)


def grid(extent_m, step_m: float) -> tuple[np.ndarray, np.ndarray]:
    """Flat `(east, north)` of a grid over `(east lo, hi, north lo, hi)`, north-major."""
    e0, e1, n0, n1 = extent_m
    east, north = np.meshgrid(np.arange(e0, e1 + 1e-6, step_m), np.arange(n0, n1 + 1e-6, step_m))
    return east.ravel(), north.ravel()


WIND_SCHEMA = pa.schema([
    ("venue", pa.string()),
    ("t", pa.timestamp("s", tz="UTC")),
    ("lat", pa.float64()),
    ("lon", pa.float64()),
    ("u", pa.float64()),
    ("v", pa.float64()),
])


def write_table(path: Path, winds: list[Wind]) -> int:
    """Every field on its `grid_m` grid every `table_s` over its `[on, off]`. Returns the row
    count."""
    batches = []
    for w in winds:
        east, north = grid(w.spec.extent_m, w.spec.grid_m)
        lat, lon = w.venue.geodetic(east, north)
        for t_s in range(w.on_s, w.off_s + 1, w.spec.table_s):
            u, v = w.uv(east, north, t_s)
            t = np.datetime64(sc.T0, "s") + np.timedelta64(t_s, "s")
            batches.append(pa.record_batch([
                pa.array([w.spec.venue] * len(east)), pa.array(np.full(len(east), t)),
                pa.array(lat), pa.array(lon), pa.array(u), pa.array(v)], schema=WIND_SCHEMA))
    table = pa.Table.from_batches(batches, schema=WIND_SCHEMA)
    with pa.ipc.new_file(path, WIND_SCHEMA) as f:
        f.write_table(table)
    return table.num_rows


def read_table(path: Path) -> pa.Table:
    return pa.ipc.open_file(path).read_all()
