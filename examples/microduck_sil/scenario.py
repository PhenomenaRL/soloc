"""The microduck_sil scene: the Old Port site, Pollen's apartment placed in a building there, the
epoch, each duck's synthetic boot heading and its patrol. Shared by the recorder and the viewer."""

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
ALIGNED = OUT / "microduck_aligned.arrow"     # the recording plus sightings and alignment estimates

SITE = FacilitySpec("Place Jacques-Cartier", "PJC", EARTH, 45.5075, -73.5536)
# Pollen's apartment scene (run with DUCK_SIM_SCENE=apartment) is the MuJoCo world. Its frame, the
# `apartment` entity, sits in the site's ENU: on the 2nd floor of the block at the site point,
# turned to the building's walls.
SCENE = "apartment"
APARTMENT = "apartment"
APARTMENT_ENU_M = (0.0, 0.0, 3.3)
APARTMENT_YAW_DEG = -29.5
T0 = datetime(2026, 10, 29, 16, 0)   # UTC, noon in Montreal
RATE_HZ = 10

LETTERS = "abcd"
DUCKS = [f"duck-{c}" for c in LETTERS]

# Synthetic: the twin's odom yaw is world yaw (its IMU is MuJoCo's own quaternion), where a real
# duck's heading starts wherever it booted. Each duck's odom is turned by this.
BOOT_YAW_DEG = {"duck-a": 0.0, "duck-b": 70.0, "duck-c": -120.0, "duck-d": 160.0}

# Patrols (apartment m), cycled from the spawn line; the planner routes between points round
# furniture and through doorways. a: kitchen + living, b: bedroom + office, c: bathroom, d: corridor.
_KITCHEN_DOOR = [(-3.3, 0.7), (-2.45, 0.7), (-2.45, -0.5)]
PATROLS = {
    "duck-a": [(-0.4, 2.0), (-1.4, 2.0), (-1.6, 2.3), (-3.2, 2.3), *_KITCHEN_DOOR, (-1.5, -1.4), (-2.0, -2.4),
               (-1.5, -1.4), *_KITCHEN_DOOR[::-1], (-3.2, 2.3), (-1.6, 2.3), (-1.4, 2.0)],
    "duck-b": [(0.2, 1.8), (1.2, 1.8), (1.2, 2.2), (1.2, 1.8), (0.2, 1.8), (0.2, 0.45), (1.4, 0.45), (2.6, 0.0),
               (1.4, 0.45), (0.2, 0.45)],
    "duck-c": [(-0.75, 0.0), (-0.75, -2.1), (0.2, -2.1), (1.2, -2.1), (2.2, -2.0), (2.8, -1.5), (1.2, -2.1),
               (0.2, -2.1), (-0.75, -2.1), (-0.75, 0.0)],
    "duck-d": [(0.0, 2.6), (0.0, 0.2), (-0.75, -0.2), (-0.75, -2.4), (-0.75, -0.2), (0.0, 0.2)],
}
NO_GO = (-0.55, 0.15, -1.55, -0.55)   # x0, x1, y0, y1: the stairwell, a drop the ToF can't see
WAYPOINT_M = 0.15                     # a waypoint is reached this close, by the duck's own reckoning
REPLAN_S = 2.0

TRUTH_SOURCE_NAME = "mujoco_twin"

# The mission: virtual trash in the rooms (apartment m; not in MuJoCo), spotted by a synthetic
# detector from truth plus noise when in view, range and line of sight, walked to and pointed out.
TRASH = {
    "trash-1": (-3.3, 1.5), "trash-2": (-0.5, 2.6), "trash-3": (-2.4, -2.3), "trash-4": (-1.6, 0.0),
    "trash-5": (1.3, 1.5), "trash-6": (2.2, -0.3), "trash-7": (1.8, -1.6), "trash-8": (2.6, -2.6),
}
SEED = 7
DETECT_RANGE_M = 3.0
FOV_DEG = 45.0                        # the head camera's forward field of view
SIGHT_Z_M = 0.1                       # line-of-sight rays at about camera height
RANGE_SIGMA_M = 0.05
BEARING_SIGMA_DEG = 2.0
GOTO_VX = 0.4                         # velstand walks ~1/3 of the commanded vx in the twin
FACING_RAD = 0.4                      # walk only when the target is this close to dead ahead
STOP_M = 0.35                         # stop this far short of the item, by odometry
POINT_S = 3.0
GOTO_TIMEOUT_S = 60.0
# Bumps, from the twin's own VL53L5CX (body op `tof`): the centre zones' nearest valid return.
TOF_ROWS, TOF_COLS = (3, 4), (2, 3, 4, 5)
TOF_STOP_MM = 150
TOF_VALID = 5

# Duck-to-duck sightings (synthetic: camera bearing inside the FOV, ToF range, BLE identity) and the
# alignment of each duck's odometry frame into the lookout's, fitted from them.
LOOKOUT = "duck-d"
SIGHT_RANGE_M = 4.0                   # the VL53L5CX's range
SIGHT_HZ = 2
REFIT_S = 10.0
MIN_SIGHTINGS = 3
MIN_SPREAD_M = 0.5
EVAL_EVERY_S = 15.0                   # "b in a's frame" samples, truth via soloc exchange

# The detector sweep (`sweep.py`): four ways one duck could sense another, each varied one axis at a
# time around its baseline. Camera detection falls off as 1 / (1 + (r / r50)^4).
DESIGNS = {
    "cam-tof-ble": {"kind": "cam", "range": "tof", "r50": 2.0, "fov": 45.0, "hz": 2.0, "bearing": 2.0,
                    "sigma": 0.05, "drift": 0.5},
    "cam-ble": {"kind": "cam", "range": None, "r50": 2.0, "fov": 45.0, "hz": 2.0, "bearing": 2.0, "drift": 0.5},
    "ble": {"kind": "ble", "max_r": 8.0, "hz": 1.0, "sigma": 1.0, "drift": 0.5},
    "marker": {"kind": "cam", "range": "marker", "r50": 2.0, "fov": 45.0, "hz": 2.0, "bearing": 2.0,
               "k": 0.02, "drift": 0.5},
}
# axis → {design: (key, values, id label)}; values equal to the baseline are its `-base` run.
_CAMS = ("cam-tof-ble", "cam-ble", "marker")
SWEEP_AXES = {
    "range": {d: ("r50", (1.0, 2.0, 3.0), "range") for d in _CAMS},
    "fov": {d: ("fov", (45.0, 90.0, 160.0), "fov") for d in _CAMS},
    "rate": {**{d: ("hz", (0.5, 2.0, 10.0), "hz") for d in _CAMS}, "ble": ("hz", (0.5, 1.0, 5.0), "hz")},
    "noise": {"cam-tof-ble": ("bearing", (1.0, 2.0, 5.0), "bearing"), "cam-ble": ("bearing", (1.0, 2.0, 5.0), "bearing"),
              "marker": ("k", (0.01, 0.02, 0.05), "k"), "ble": ("sigma", (0.5, 1.0, 2.0), "sigma")},
    "drift": {d: ("drift", (0.0, 0.5, 2.0, 5.0), "drift") for d in DESIGNS},
}
DRIFT_SIGN = {"duck-a": 1.0, "duck-b": -1.0, "duck-c": 1.0, "duck-d": -1.0}   # gyro bias direction per duck
REPLAY_HZ = 10
STALE_M = 0.25
UNSEEN_BINS_S = (0.0, 5.0, 15.0, 30.0, 60.0, math.inf)
SWEEP = OUT / "sweep"
SWEEP_MERGED = OUT / "sweep.arrow"


# Recordings: run 1 is the scenario above; run 2 walks every patrol in reverse with other synthetic
# boot headings, for different encounters on the same mission.
RUN_BOOT_YAW_DEG = {2: {"duck-a": 110.0, "duck-b": -40.0, "duck-c": 150.0, "duck-d": -90.0}}


_RUN1 = {"LEDGER": LEDGER, "JOINTS": JOINTS, "ALIGNED": ALIGNED, "PATROLS": PATROLS, "BOOT_YAW_DEG": BOOT_YAW_DEG,
         "SWEEP": SWEEP, "SWEEP_MERGED": SWEEP_MERGED}
RUNS = (1, 2)
SEEDS = 10                            # noise seeds per design per run in the sweep (ledgers for seed 0)


def set_run(run: int):
    """Point the scenario at recording `run`: its file names, and for run 2 its variant."""
    global LEDGER, JOINTS, ALIGNED, PATROLS, BOOT_YAW_DEG, SWEEP, SWEEP_MERGED
    globals().update(_RUN1)
    if run == 1:
        return
    tag = f"microduck_run{run}"
    LEDGER, JOINTS, ALIGNED = OUT / f"{tag}.arrow", OUT / f"{tag}.joints.arrow", OUT / f"{tag}_aligned.arrow"
    SWEEP, SWEEP_MERGED = OUT / "sweep" / f"run{run}", OUT / f"sweep_run{run}.arrow"
    PATROLS = {d: pts[::-1] for d, pts in PATROLS.items()}
    BOOT_YAW_DEG = RUN_BOOT_YAW_DEG[run]


def odom_name(duck: str) -> str:
    return f"{duck}/odom"


def estimate_name(duck: str) -> str:
    """`duck`'s odometry frame as the lookout estimates it, posed in the lookout's odometry frame."""
    return f"{duck}/odom~{LOOKOUT}"


def source_name(duck: str) -> str:
    return f"{duck}/robotd"


def entity(name: str) -> bytes:
    return mint(KIND_SOLOC, AUTHORITY, name)


def source(name: str) -> bytes:
    return mint(KIND_ABSTRACT, AUTHORITY, name)


def bindings(ducks: list[str]) -> list[tuple[int, str, str]]:
    """`(kind, authority, name)` for `import_names`."""
    out = [(KIND_SOLOC, AUTHORITY, SITE.name), (KIND_SOLOC, AUTHORITY, APARTMENT),
           (KIND_ABSTRACT, AUTHORITY, TRUTH_SOURCE_NAME),
           (KIND_ABSTRACT, AUTHORITY, "kinematic_sim_v1")]     # the reused Facility's row source
    out += [(KIND_SOLOC, AUTHORITY, t) for t in TRASH]
    out += [(KIND_SOLOC, AUTHORITY, estimate_name(d)) for d in ducks if d != LOOKOUT]
    for d in ducks:
        out += [(KIND_SOLOC, AUTHORITY, d), (KIND_SOLOC, AUTHORITY, odom_name(d)),
                (KIND_ABSTRACT, AUTHORITY, source_name(d))]
    return out


def boot_yaw(duck: str) -> float:
    return math.radians(BOOT_YAW_DEG[duck])
