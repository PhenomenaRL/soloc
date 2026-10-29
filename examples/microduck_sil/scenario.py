"""The microduck_sil scene: the Old Port site, the epoch, each duck's synthetic boot heading and its
search route. Shared by the recorder and the viewer."""

import math
import sys
from datetime import datetime
from pathlib import Path

HERE = Path(__file__).resolve().parent
SIM_STUDY_0 = HERE.parent / "sim_study_0"
sys.path.insert(0, str(SIM_STUDY_0))

from sim.geo import EARTH  # noqa: E402
from sim.scenario import AUTHORITY, FacilitySpec  # noqa: E402
from soloc_client import KIND_ABSTRACT, KIND_SOLOC, mint  # noqa: E402

UPSTREAM = HERE / "upstream"
DUCK_SIM = UPSTREAM / "microduck" / "scripts" / "duck-sim"
SIM_RL = UPSTREAM / "microduck_rl"
STATE = Path.home() / ".cache" / "duck-sim"
VELSTAND = STATE / "policies" / "current" / "velstand.onnx"
BODY_PORT = 7801                     # duck i's body is BODY_PORT + i
OUT = HERE / "out"
LEDGER = OUT / "microduck.arrow"
JOINTS = OUT / "microduck.joints.arrow"       # 50 Hz trunk pose + joint angles, for the gait viewer
JOINT_HZ = 50

# MuJoCo +x/+y/+z are the site's east/north/up; the scene origin is this point.
SITE = FacilitySpec("Place Jacques-Cartier", "PJC", EARTH, 45.5075, -73.5536)
T0 = datetime(2026, 10, 29, 16, 0)   # UTC, noon in Montreal
RATE_HZ = 10

LETTERS = "abcd"
DUCKS = [f"duck-{c}" for c in LETTERS]

# Synthetic: the twin's odom yaw is world yaw (its IMU is MuJoCo's own quaternion), where a real
# duck's heading starts wherever it booted. Each duck's odom is turned by this.
BOOT_YAW_DEG = {"duck-a": 0.0, "duck-b": 70.0, "duck-c": -120.0, "duck-d": 160.0}

# Search routes, `(vx m/s, vyaw rad/s, seconds)`, repeated. vx stays above the walk deadband (0.2).
ROUTES = {
    "duck-a": [(0.0, -0.8, 2), (0.3, 0.0, 8), (0.0, 1.0, 3), (0.3, 0.2, 10), (0.0, -1.0, 3)],
    "duck-b": [(0.3, 0.0, 6), (0.3, 0.4, 16), (0.0, 1.0, 2)],
    "duck-c": [(0.0, 0.8, 2), (0.3, 0.0, 8), (0.0, -1.0, 3), (0.3, -0.2, 10), (0.0, 1.0, 3)],
    "duck-d": [(0.0, 1.0, 4), (0.3, 0.0, 3), (0.0, -1.0, 4)],
}

TRUTH_SOURCE_NAME = "mujoco_twin"

# The mission: virtual trash on the square (site ENU m; not in MuJoCo), spotted by a synthetic
# detector from truth plus noise, walked to on the duck's own odometry, and pointed out.
TRASH = {
    "trash-1": (1.5, -1.2), "trash-2": (3.0, 0.3), "trash-3": (-1.5, 2.5), "trash-4": (2.5, 2.8),
    "trash-5": (-2.0, -1.5), "trash-6": (5.0, -2.5), "trash-7": (0.5, 4.5), "trash-8": (-3.5, 0.8),
}
SEED = 7
DETECT_RANGE_M = 3.0
FOV_DEG = 45.0                        # the head camera's forward field of view
RANGE_SIGMA_M = 0.05
BEARING_SIGMA_DEG = 2.0
GOTO_VX = 0.4                         # velstand walks ~1/3 of the commanded vx in the twin
FACING_RAD = 0.4                      # walk only when the target is this close to dead ahead
STOP_M = 0.35                         # stop this far short of the item, by odometry
POINT_S = 3.0
GOTO_TIMEOUT_S = 45.0                 # 3 m at ~0.1 m/s, plus turning


def odom_name(duck: str) -> str:
    return f"{duck}/odom"


def source_name(duck: str) -> str:
    return f"{duck}/robotd"


def entity(name: str) -> bytes:
    return mint(KIND_SOLOC, AUTHORITY, name)


def source(name: str) -> bytes:
    return mint(KIND_ABSTRACT, AUTHORITY, name)


def bindings(ducks: list[str]) -> list[tuple[int, str, str]]:
    """`(kind, authority, name)` for `import_names`."""
    out = [(KIND_SOLOC, AUTHORITY, SITE.name), (KIND_ABSTRACT, AUTHORITY, TRUTH_SOURCE_NAME),
           (KIND_ABSTRACT, AUTHORITY, "kinematic_sim_v1")]     # the reused Facility's row source
    out += [(KIND_SOLOC, AUTHORITY, t) for t in TRASH]
    for d in ducks:
        out += [(KIND_SOLOC, AUTHORITY, d), (KIND_SOLOC, AUTHORITY, odom_name(d)),
                (KIND_ABSTRACT, AUTHORITY, source_name(d))]
    return out


def command(duck: str, t_s: float) -> tuple[float, float]:
    """`(vx, vyaw)` of the duck's route at `t_s` into the run."""
    route = ROUTES[duck]
    t = t_s % sum(s for _, _, s in route)
    for vx, vyaw, s in route:
        if t < s:
            return vx, vyaw
        t -= s
    return 0.0, 0.0


def boot_yaw(duck: str) -> float:
    return math.radians(BOOT_YAW_DEG[duck])
