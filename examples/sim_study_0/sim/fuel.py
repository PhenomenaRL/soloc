"""The fire venue's fuel map: hand-drawn zones (`scenario.FUEL_FACTORS`) in venue ENU metres,
each scaling the no-wind spread rate R0. `write_table` grids it for `out/fuel.arrow`.
"""

from pathlib import Path

import numpy as np
import pyarrow as pa

from sim import scenario as sc
from sim.models.facility import Facility
from sim.wind import grid

CLASSES = tuple(sc.FUEL_FACTORS)
FACTORS = np.array([sc.FUEL_FACTORS[c] for c in CLASSES])


def polyline_distance(east, north, line: np.ndarray) -> np.ndarray:
    """Distance (m) from each point to the polyline `(K, 2)`."""
    p = np.stack([np.asarray(east, float), np.asarray(north, float)], -1)[..., None, :]
    a, ab = line[:-1], np.diff(line, axis=0)
    t = np.clip(((p - a) * ab).sum(-1) / (ab * ab).sum(-1), 0.0, 1.0)
    return np.linalg.norm(a + t[..., None] * ab - p, axis=-1).min(-1)


class Fuel:
    def __init__(self, venue: Facility):
        self.venue = venue
        self.river = np.array([venue.enu(*p)[:2] for p in sc.SQUAMISH_RIVER])
        self.corridor = np.array([venue.enu(*p)[:2] for p in sc.POWER_LINE])

    def classes(self, east, north) -> np.ndarray:
        """Index into `CLASSES` per point."""
        e, n = np.asarray(east, float), np.asarray(north, float)
        river = polyline_distance(e, n, self.river)
        x0, x1, y0, y1 = sc.SLASH_M
        out = np.full(e.shape, CLASSES.index("conifer"))
        out = np.where((e >= x0) & (e <= x1) & (n >= y0) & (n <= y1), CLASSES.index("slash"), out)
        out = np.where(polyline_distance(e, n, self.corridor) <= sc.CORRIDOR_HALF_WIDTH_M,
                       CLASSES.index("corridor"), out)
        out = np.where(river <= sc.RIPARIAN_M, CLASSES.index("riparian"), out)
        return np.where(river <= sc.RIVER_HALF_WIDTH_M, CLASSES.index("river"), out)

    def factor(self, east, north) -> np.ndarray:
        return FACTORS[self.classes(east, north)]


FUEL_SCHEMA = pa.schema([
    ("venue", pa.string()),
    ("lat", pa.float64()),
    ("lon", pa.float64()),
    ("class", pa.dictionary(pa.int8(), pa.string())),
    ("r0_factor", pa.float64()),
])


def write_table(path: Path, fuels: list[Fuel]) -> int:
    """Every venue's fuel on a `FUEL_GRID_M` grid over its wind extent. Returns the row count."""
    batches = []
    for f in fuels:
        east, north = grid(sc.FIRE_WIND.extent_m, sc.FUEL_GRID_M)
        lat, lon = f.venue.geodetic(east, north)
        k = f.classes(east, north)
        batches.append(pa.record_batch([
            pa.array([f.venue.name] * len(east)), pa.array(lat), pa.array(lon),
            pa.DictionaryArray.from_arrays(pa.array(k, pa.int8()), pa.array(CLASSES)),
            pa.array(FACTORS[k])], schema=FUEL_SCHEMA))
    table = pa.Table.from_batches(batches, schema=FUEL_SCHEMA)
    with pa.ipc.new_file(path, FUEL_SCHEMA) as out:
        out.write_table(table)
    return table.num_rows
