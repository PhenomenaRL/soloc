"""Arena strategies: what a policy observes, what it may command, and loading one by `mod:fn`.

A policy observes the world through soloc (`current_state` rows, decoded into `Pose`s) plus the
venue's wind, and returns a command. See docs/arena.md.
"""

import importlib
import math
from dataclasses import dataclass
from typing import Callable, Literal

import numpy as np
import pyarrow as pa

from soloc_client import CENTURY_NS, id_bytes, positions, sts_field


@dataclass(frozen=True)
class Pose:
    """An entity's latest row, on the venue's ENU plane."""
    name: str
    t_s: float                       # the row's epoch, seconds since T0
    east_m: float
    north_m: float
    heading_deg: float               # compass, from the row's yaw
    speed_m_s: float
    velocity: tuple[float, float]    # (east, north) m/s

    @property
    def xy(self) -> np.ndarray:
        return np.array([self.east_m, self.north_m])


def decode_state(table: pa.Table, names: dict[bytes, str], t0_ns: int) -> dict[str, Pose]:
    """`current_state` rows → `Pose` by name. Rows must be in metres on the venue frame."""
    ids = id_bytes(table.column("entity_id"))
    pos = positions(table)
    q = sts_field(table, "quaternion").flatten().to_numpy().reshape(-1, 4)
    v = table.column("velocity").combine_chunks().flatten().to_numpy().reshape(-1, 3)
    t_ns = (sts_field(table, "duration_centuries").to_numpy().astype(np.int64) * CENTURY_NS
            + sts_field(table, "duration_ns").to_numpy().astype(np.int64))
    out = {}
    for k, i in enumerate(ids):
        yaw = 2 * math.atan2(q[k, 3], q[k, 0])
        out[names[i]] = Pose(names[i], (int(t_ns[k]) - t0_ns) / 1e9, float(pos[k, 0]),
                             float(pos[k, 1]), (90 - math.degrees(yaw)) % 360,
                             float(math.hypot(v[k, 0], v[k, 1])), (float(v[k, 0]), float(v[k, 1])))
    return out


Command = float | Literal["tack", "gybe"] | None


@dataclass(frozen=True)
class RegattaObservation:
    """One boat's view at a decision tick. `boats` and `marks` come from `current_state`, so
    they are the ledger's latest rows (one 5 s tick old). `wind(east, north)` and `polar(twa,
    tws)` are evaluated now."""
    t_s: int
    decision_s: int                  # the command holds until the next decision, this much later
    me: str
    boats: dict[str, Pose]           # every racing boat and the RC boat
    marks: dict[str, Pose]           # the course marks while laid
    next_mark: str                   # START, W, GATE or FINISH
    legs_done: int
    legs: tuple[str, ...]
    started: bool
    ocs: bool                        # over the line at the gun; must dip back and start again
    gun_s: int
    time_limit_s: int
    course_axis_deg: float           # compass bearing from the line to the windward mark
    line: tuple[str, str]            # (RC boat, pin) names: the start and finish line ends
    gate: tuple[str, str]
    windward: str
    wind: Callable[[float, float], tuple[float, float]]   # → (twd_deg from, tws_m_s)
    polar: Callable[[float, float], float]                # (twa_deg, tws) → boat speed m/s


def load(spec: str) -> Callable:
    """`package.module:function` → the function."""
    mod, sep, fn = spec.partition(":")
    if not sep or not mod or not fn:
        raise ValueError(f"policy {spec!r} is not 'module:function'")
    policy = getattr(importlib.import_module(mod), fn)
    if not callable(policy):
        raise TypeError(f"policy {spec!r} is not callable")
    return policy


def regatta_command(cmd, policy: Callable) -> Command:
    """A heading in compass degrees, `"tack"`, `"gybe"` or `None` (hold the current heading)."""
    if cmd is None or cmd in ("tack", "gybe"):
        return cmd
    if (isinstance(cmd, (int, float, np.floating, np.integer)) and not isinstance(cmd, bool)
            and math.isfinite(cmd)):
        return float(cmd) % 360
    raise TypeError(f"{getattr(policy, '__name__', policy)} returned {cmd!r}; "
                    "expected a compass heading, 'tack', 'gybe' or None")
