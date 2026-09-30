"""Probes on real ephemerides: rows are taken straight from a JPL Horizons table.

Position and velocity are Horizons' barycentric ICRF vectors, stored as they come (no
interpolation: the table is on the probe's cadence). The rows are tagged ESTIMATED with
Horizons as their source, not SIMULATED. Attitude is modelled, not from the mission's
attitude kernels: body z points at the Sun's centre (for Parker, the heat shield).
"""

from datetime import datetime
from pathlib import Path

import numpy as np

from geo import ICRF, SUN, lvlh, quat_from_matrix
from models import Row
from scenario import (AUTHORITY, DURATION_S, HORIZONS_AUTHORITY, HORIZONS_SOURCE, PROBE_CADENCE_S,
                      PROBE_TIMESCALE, T0, ProbeSpec, horizons_file)
from soloc_client import KIND_ABSTRACT, KIND_SOLOC, mint

KERNELS = Path(__file__).parent.parent / "kernels"
HORIZONS_ID = mint(KIND_ABSTRACT, HORIZONS_AUTHORITY, HORIZONS_SOURCE)


def load_horizons(path: Path) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """`(t_s since T0, position km (N, 3), velocity km/s (N, 3))` from a Horizons CSV vector
    table written by fetch_horizons.py."""
    t_s, states = [], []
    lines = iter(path.read_text().splitlines())
    for line in lines:
        if line.startswith("$$SOE"):
            break
    for line in lines:
        if line.startswith("$$EOE"):
            break
        cells = [c.strip() for c in line.split(",")]
        at = datetime.strptime(cells[1].removeprefix("A.D. "), "%Y-%b-%d %H:%M:%S.%f")
        t_s.append((at - T0).total_seconds())
        states.append([float(c) for c in cells[2:8]])
    states = np.array(states).reshape(-1, 6)
    return np.array(t_s), states[:, :3], states[:, 3:]


def schedule() -> np.ndarray:
    return np.arange(0, DURATION_S + 1, PROBE_CADENCE_S, dtype=float)


def covers(t_s: np.ndarray) -> bool:
    """Whether a table has a row on every tick of the probe schedule."""
    return bool(np.isin(schedule(), t_s).all())


class Probe:
    def __init__(self, spec: ProbeSpec, ephemeris):
        self.name = spec.name
        self.id = mint(KIND_SOLOC, AUTHORITY, spec.name)
        self.spec = spec
        path = KERNELS / horizons_file(spec)
        if not path.exists() or not covers(load_horizons(path)[0]):
            raise SystemExit(f"{path} is missing or does not cover the window; "
                             "run `python fetch_horizons.py`")
        t_s, p, v = load_horizons(path)
        keep = np.isin(t_s, schedule())
        self.t_s, self.p, self.v = t_s[keep], p[keep], v[keep]
        self.row_of = {int(t): k for k, t in enumerate(self.t_s)}
        self.sun_p, self.sun_v = ephemeris.state(SUN, self.t_s)

    def due(self, t_s: int) -> bool:
        return t_s % PROBE_CADENCE_S == 0

    def state(self, t_s: int) -> tuple[np.ndarray, np.ndarray]:
        """The table's barycentric ICRF `(km, km/s)` at a scheduled tick."""
        k = self.row_of[t_s]
        return self.p[k], self.v[k]

    def sample(self, t_s: int) -> Row | None:
        if not self.due(t_s):
            return None
        k = self.row_of[t_s]
        r, v = self.p[k] - self.sun_p[k], self.v[k] - self.sun_v[k]      # heliocentric
        attitude = lvlh(np.cross(r, v), r)
        return Row(ICRF.frame_id, self.p[k].tolist(), quat_from_matrix(attitude),
                   units="km", timescale=PROBE_TIMESCALE,
                   source_id=HORIZONS_ID, estimate="ESTIMATED",
                   optional={"velocity": (self.v[k] * 1000).tolist(),
                             "mass_kg": self.spec.mass_kg,
                             "dimensions": list(self.spec.dimensions_m)})
