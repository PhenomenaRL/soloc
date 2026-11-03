"""Records Pollen's microduck twin into soloc on a trash-spotting mission in their apartment scene:
each duck's MuJoCo truth in the `apartment` frame (itself posed in the Old Port site), its own
odometry in its own boot frame, and its trash reports in that frame. Each duck patrols its rooms,
walks to trash it spots and points it out. The trunk pose and joint angles also go to a 50 Hz
sidecar for the gait viewer.

    ../sim_study_0/serve.sh                                  # in another terminal
    DUCK_SIM_RL=$PWD/upstream/microduck_rl DUCK_SIM_DUCKS=4 DUCK_SIM_VIEWER=0 DUCK_SIM_SCENE=apartment upstream/microduck/scripts/duck-sim
    ../sim_study_0/.venv/bin/python record.py --ducks 4 --seconds 300
"""

import argparse
import json
import math
import os
import socket
import subprocess
import threading
import time

import numpy as np
import pyarrow as pa

import apartment as ap
import scenario as sc
from sim.geo import quat_yaw
from sim.models.facility import Facility
from soloc_client import SolocClient, registry_ipc, tai_ns_from_utc


def yaw_of(q) -> float:
    w, x, y, z = q
    return math.atan2(2 * (w * z + x * y), 1 - 2 * (y * y + z * z))


def rotate(x: float, y: float, a: float) -> tuple[float, float]:
    c, s = math.cos(a), math.sin(a)
    return c * x - s * y, s * x + c * y


def wrap(a: float) -> float:
    return (a + math.pi) % (2 * math.pi) - math.pi


JOINTS_SCHEMA = pa.schema([("t_tai_ns", pa.int64()), ("duck", pa.string()),
                           ("trunk", pa.list_(pa.float64(), 3)), ("quat", pa.list_(pa.float64(), 4)),
                           ("positions", pa.list_(pa.float64(), 15))])


class Sampler(threading.Thread):
    """Reads one duck's body at `JOINT_HZ` on its own connection: trunk pose and joint angles."""

    def __init__(self, index: int, tai, stop: threading.Event):
        super().__init__(daemon=True)
        self.name_, self.tai, self.stop = sc.DUCKS[index], tai, stop
        self.body = socket.create_connection(("127.0.0.1", sc.BODY_PORT + index)).makefile("rw")
        self.body.write(json.dumps({"op": "hello", "protocol": 1, "joints": 15}) + "\n")
        self.body.flush()
        self.body.readline()
        self.rows = []

    def run(self):
        period, start, k = 1.0 / sc.JOINT_HZ, time.monotonic(), 0
        while not self.stop.is_set():
            self.body.write('{"op":"read"}\n')
            self.body.flush()
            r = json.loads(self.body.readline())
            self.rows.append((self.tai(time.monotonic_ns()), self.name_, r["trunk"], r["imu"]["quat"], r["positions"]))
            k += 1
            time.sleep(max(0.0, start + k * period - time.monotonic()))


class Searcher:
    """One duck's mission: SEARCH along its patrol, GOTO a spotted item, POINT at it and report
    where its odometry puts it. Items are claimed team-wide. It navigates on its own odometry placed
    by its true boot pose (a stand-in for Pollen's map localization), routed by the apartment's grid
    planner, and stops on the twin's ToF when something is dead ahead."""

    claimed: set[str] = set()

    def __init__(self, duck: "Duck", rng: np.random.Generator):
        self.duck, self.rng = duck, rng
        self.mode, self.since, self.item, self.target = "SEARCH", 0.0, None, None
        self.patrol, self.leg, self.path, self.planned = sc.PATROLS[duck.name], 0, [], -math.inf

    def placed(self, odom) -> tuple[float, float, float]:
        """`(x, y, yaw)` in the apartment: the duck's odometry through its boot pose."""
        x, y, yaw = odom
        bx, by, _, a = self.duck.boot
        dx, dy = rotate(x, y, a)
        return bx + dx, by + dy, yaw + a

    def drive(self, t: float, here, goal, tof_mm: float) -> tuple[float, float]:
        """`(vx, vyaw)` toward `goal` along the planned path; replans every `REPLAN_S` and on a bump."""
        if not self.path or t - self.planned > sc.REPLAN_S:
            self.path, self.planned = ap.grid().plan(here[:2], goal), t
        while self.path and math.hypot(self.path[0][0] - here[0], self.path[0][1] - here[1]) < sc.WAYPOINT_M:
            self.path.pop(0)
        aim = self.path[0] if self.path else goal
        err = wrap(math.atan2(aim[1] - here[1], aim[0] - here[0]) - here[2])
        if tof_mm < sc.TOF_STOP_MM and abs(err) < sc.FACING_RAD:
            self.planned = -math.inf
            return 0.0, 1.0
        return (sc.GOTO_VX if abs(err) < sc.FACING_RAD else 0.0, max(-1.0, min(1.0, 2 * err)))

    def measure(self, pos, tyaw: float, item: str) -> tuple[float, float]:
        """Range and bearing (trunk frame) of an item, from truth plus noise."""
        ex, ny = sc.TRASH[item]
        r = math.hypot(ex - pos[0], ny - pos[1])
        b = wrap(math.atan2(ny - pos[1], ex - pos[0]) - tyaw)
        return (r + self.rng.normal(0, sc.RANGE_SIGMA_M),
                b + self.rng.normal(0, math.radians(sc.BEARING_SIGMA_DEG)))

    def spot(self, pos, tyaw: float) -> str | None:
        """The nearest unclaimed item in the camera's view and range, with nothing in between."""
        seen = []
        for item, (ex, ny) in sc.TRASH.items():
            r = math.hypot(ex - pos[0], ny - pos[1])
            b = wrap(math.atan2(ny - pos[1], ex - pos[0]) - tyaw)
            if (item not in Searcher.claimed and r <= sc.DETECT_RANGE_M and abs(b) <= math.radians(sc.FOV_DEG / 2)
                    and ap.clear_line(pos, (ex, ny))):
                seen.append((r, item))
        return min(seen)[1] if seen else None

    def step(self, t: float, pos, tyaw: float, odom, tof_mm: float) -> tuple[tuple[float, float], tuple | None]:
        """`((vx, vyaw), report)`; a report is `(item, [x, y, 0] in the duck's odom frame)`."""
        if self.duck.boot is None:
            return (0.0, 0.0), None
        x, y, yaw = odom
        here = self.placed(odom)
        name = self.duck.name
        if self.mode == "SEARCH":
            item = self.spot(pos, tyaw)
            if item is None:
                goal = self.patrol[self.leg]
                if math.hypot(goal[0] - here[0], goal[1] - here[1]) < 2 * sc.WAYPOINT_M:
                    self.leg, self.path = (self.leg + 1) % len(self.patrol), []
                    goal = self.patrol[self.leg]
                return self.drive(t, here, goal, tof_mm), None
            r, b = self.measure(pos, tyaw, item)
            seen = self.placed((x + r * math.cos(yaw + b), y + r * math.sin(yaw + b), 0.0))
            self.item, self.target, self.path = item, seen[:2], []
            Searcher.claimed.add(item)
            self.mode, self.since = "GOTO", t
            print(f"  {t:6.1f} s  {name} spots {item} at {r:.2f} m")
        if self.mode == "GOTO":
            if math.hypot(self.target[0] - here[0], self.target[1] - here[1]) < sc.STOP_M:
                r, b = self.measure(pos, tyaw, self.item)
                self.mode, self.since, self.path = "POINT", t, []
                print(f"  {t:6.1f} s  {name} points out {self.item}")
                return (0.0, 0.0), (self.item, [x + r * math.cos(yaw + b), y + r * math.sin(yaw + b), 0.0])
            if t - self.since > sc.GOTO_TIMEOUT_S:
                Searcher.claimed.discard(self.item)
                self.mode, self.since, self.path = "SEARCH", t, []
                print(f"  {t:6.1f} s  {name} gives up on {self.item}")
                return (0.0, 0.0), None
            return self.drive(t, here, self.target, tof_mm), None
        if t - self.since > sc.POINT_S:
            self.mode, self.since = "SEARCH", t
        return (0.0, 0.0), None


class Duck:
    """One duck: its body's TCP port for truth, and robotd's socket for odometry and commands."""

    def __init__(self, index: int):
        self.name = sc.DUCKS[index]
        self.id, self.odom_id = sc.entity(self.name), sc.entity(sc.odom_name(self.name))
        self.source = sc.source(sc.source_name(self.name))
        self.psi = sc.boot_yaw(self.name)
        self.body = socket.create_connection(("127.0.0.1", sc.BODY_PORT + index)).makefile("rw")
        self._call({"op": "hello", "protocol": 1, "joints": 15})
        sock = str(sc.STATE / f"{self.name}.sock")
        sub = socket.socket(socket.AF_UNIX)
        sub.connect(sock)
        self.sub = sub.makefile("rw")
        self.sub.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "robot.subscribe", "params": {}}) + "\n")
        self.sub.flush()
        mv = socket.socket(socket.AF_UNIX)
        mv.connect(sock)
        self.mv = mv.makefile("w")
        self.state, self.fallen, self.boot = None, False, None
        threading.Thread(target=self._pump, daemon=True).start()

    def _call(self, req: dict) -> dict:
        self.body.write(json.dumps(req) + "\n")
        self.body.flush()
        return json.loads(self.body.readline())

    def _pump(self):
        for line in self.sub:
            m = json.loads(line)
            if m.get("method") == "robot.state":
                self.state = m["params"]

    def truth(self) -> tuple[list[float], list[float], int]:
        """Trunk position (m) and attitude `[w, x, y, z]` in the site frame, stamped on arrival."""
        r = self._call({"op": "read"})
        return r["trunk"], r["imu"]["quat"], time.monotonic_ns()

    def odom(self) -> tuple[float, float, float, float, int]:
        """`(x, y, z, yaw, t_ns)`, turned by the synthetic boot heading."""
        s = self.state
        p, yaw = s["odom"]["position"], s["odom"]["yaw"]
        x, y = rotate(p[0], p[1], self.psi)
        return x, y, p[2], yaw + self.psi, s["t_ns"]

    def tof(self) -> float:
        """The nearest valid return (mm) in the ToF's centre zones; 4 m if none."""
        r = self._call({"op": "tof"})
        near = [r["distance_mm"][i * r["cols"] + j] for i in sc.TOF_ROWS for j in sc.TOF_COLS
                if r["status"][i * r["cols"] + j] == sc.TOF_VALID]
        return min(near, default=4000.0)

    def move(self, vx: float, vyaw: float):
        self.mv.write(json.dumps({"jsonrpc": "2.0", "method": "robot.move",
                                  "params": {"vx": vx, "vy": 0.0, "vyaw": vyaw}}) + "\n")
        self.mv.flush()


def load_velstand(duck: str, count: int):
    env = os.environ | {"DUCK_SIM_RL": str(sc.SIM_RL), "DUCK_SIM_DUCKS": str(count), "DUCK_SIM_DUCK": duck}
    out = subprocess.run([str(sc.DUCK_SIM), "ctl", "policy", "load", "walk", str(sc.VELSTAND)],
                         env=env, capture_output=True, text=True)
    if out.returncode:
        raise SystemExit(f"{duck}: policy load failed\n{out.stdout}{out.stderr}")
    print(f"  {duck}: {out.stdout.splitlines()[0]}")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--ducks", type=int, default=4)
    p.add_argument("--seconds", type=float, default=300)
    p.add_argument("--server", default="grpc://localhost:50051")
    p.add_argument("--run", type=int, default=1, help="1: out/microduck.arrow; 2: reversed patrols, other boot yaws")
    args = p.parse_args()
    sc.set_run(args.run)

    client = SolocClient(args.server)
    if not client.is_empty():
        raise SystemExit("the server's ledger is not empty; restart serve.sh")
    names = [sc.DUCKS[i] for i in range(args.ducks)]
    print(client.action("import_names", registry_ipc(sc.bindings(names))))
    for d in names:
        load_velstand(d, args.ducks)
    time.sleep(2.0)
    ducks = [Duck(i) for i in range(args.ducks)]
    while any(d.state is None for d in ducks):
        time.sleep(0.05)

    site = Facility(sc.SITE)
    t0_tai = tai_ns_from_utc(sc.T0)
    mono0 = time.monotonic_ns()
    tai = lambda mono: t0_tai + mono - mono0
    buf = client.buffer()
    row = site.sample(0)
    buf.append(site.id, row.frame_id, row.position, row.quaternion, t0_tai, units=row.units,
               timescale=row.timescale, source_id=row.source_id, estimate=row.estimate, **row.optional)
    truth_src = sc.source(sc.TRUTH_SOURCE_NAME)
    apt = sc.entity(sc.APARTMENT)
    buf.append(apt, site.id, list(sc.APARTMENT_ENU_M), quat_yaw(math.radians(sc.APARTMENT_YAW_DEG)), t0_tai,
               units="m", source_id=truth_src, estimate="SIMULATED")
    for item, (e, n) in sc.TRASH.items():
        buf.append(sc.entity(item), apt, [e, n, 0.0], [1.0, 0.0, 0.0, 0.0], t0_tai, units="m",
                   source_id=truth_src, estimate="SIMULATED")
    ap.grid()                           # build the planner before the clock starts

    rng = np.random.default_rng(sc.SEED)
    searchers = [Searcher(d, rng) for d in ducks]
    reported = []
    stop = threading.Event()
    samplers = [Sampler(i, tai, stop) for i in range(args.ducks)]
    for s in samplers:
        s.start()
    period = 1.0 / sc.RATE_HZ
    start = time.monotonic()
    tick = 0
    try:
        while (t := time.monotonic() - start) < args.seconds:
            for d, m in zip(ducks, searchers):
                pos, q, mono = d.truth()
                x, y, z, yaw, t_ns = d.odom()
                cmd, report = m.step(t, pos, yaw_of(q), (x, y, yaw), d.tof())
                d.move(*cmd)
                if report:
                    reported.append(report[0])
                    buf.append(sc.entity(report[0]), d.odom_id, report[1], [1.0, 0.0, 0.0, 0.0], tai(t_ns),
                               units="m", source_id=d.source, estimate="MEASURED")
                if d.boot is None:
                    # truth(t₀) ∘ odom′(t₀)⁻¹: the boot frame's pose in the apartment.
                    a = yaw_of(q) - yaw
                    bx, by = rotate(x, y, a)
                    d.boot = (pos[0] - bx, pos[1] - by, pos[2] - z, a)
                    buf.append(d.odom_id, apt, d.boot[:3], quat_yaw(a), tai(mono), units="m",
                               source_id=truth_src, estimate="SIMULATED")
                buf.append(d.id, apt, pos, q, tai(mono), units="m", source_id=truth_src, estimate="SIMULATED")
                buf.append(d.id, d.odom_id, [x, y, z], quat_yaw(yaw), tai(t_ns), units="m",
                           source_id=d.source, estimate="MEASURED")
                fallen = d.state["safety"]["fallen"]
                if fallen != d.fallen:
                    print(f"  {t:6.1f} s  {d.name} {'fell' if fallen else 'is up again'}")
                    d.fallen = fallen
            tick += 1
            if tick % sc.RATE_HZ == 0:
                client.put(buf.flush())
            time.sleep(max(0.0, start + tick * period - time.monotonic()))
    finally:
        stop.set()
        for d in ducks:
            d.move(0.0, 0.0)
        if len(buf):
            client.put(buf.flush())
    sc.OUT.mkdir(exist_ok=True)
    print(client.action("save_ledger", {"path": str(sc.LEDGER)}))
    for s in samplers:
        s.join()
    joints = pa.Table.from_pylist([dict(zip(JOINTS_SCHEMA.names, r)) for s in samplers for r in s.rows], JOINTS_SCHEMA)
    with pa.ipc.new_file(sc.JOINTS, JOINTS_SCHEMA) as w:
        w.write_table(joints)
    print(f"{sc.JOINTS}  ({joints.num_rows} samples)")
    print(f"pointed out {len(reported)} of {len(sc.TRASH)} items")


if __name__ == "__main__":
    main()
