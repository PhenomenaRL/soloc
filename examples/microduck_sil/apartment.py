"""Pollen's apartment scene, compiled from their MJCF: an occupancy grid with an A* planner for the
ducks' patrols, line of sight by MuJoCo ray casts, and its geoms for the viewer.

    ../sim_study_0/.venv/bin/python apartment.py            # grid stats and a sample route
"""

import heapq
import math
import sys
from functools import cache

import mujoco
import numpy as np

import scenario as sc

sys.path.insert(0, str(sc.SIM_RL / "src"))
from mjlab_microduck.sim.body_server import SCENES  # noqa: E402

SCENE = SCENES / f"scene_{sc.SCENE}.xml"
CELL_M = 0.05
INFLATE_M = 0.2                       # duck half-width plus margin
HIT_TOP_MIN_M, HIT_BOTTOM_MAX_M = 0.02, 0.3     # what a 25 cm duck can walk into
SKIP = ("floor", "rug", "stair", "lm_")
PARKED = (50.0, 50.0, 0.1)            # the scene's own duck, out of the way of rays


@cache
def world() -> tuple[mujoco.MjModel, mujoco.MjData]:
    """The scene with its duck parked far off, posed once."""
    model = mujoco.MjModel.from_xml_path(str(SCENE))
    data = mujoco.MjData(model)
    data.qpos[:3] = PARKED
    mujoco.mj_forward(model, data)
    return model, data


def geom_name(model, g: int) -> str:
    return mujoco.mj_id2name(model, mujoco.mjtObj.mjOBJ_GEOM, g) or ""


def is_duck(model, g: int) -> bool:
    root = model.body_rootid[model.geom_bodyid[g]]
    return mujoco.mj_id2name(model, mujoco.mjtObj.mjOBJ_BODY, root) == "trunk_base"


def half_height(model, g: int) -> float:
    t, s = model.geom_type[g], model.geom_size[g]
    if t in (mujoco.mjtGeom.mjGEOM_BOX, mujoco.mjtGeom.mjGEOM_ELLIPSOID):
        return s[2]
    if t == mujoco.mjtGeom.mjGEOM_CYLINDER:
        return s[1]
    if t == mujoco.mjtGeom.mjGEOM_CAPSULE:
        return s[1] + s[0]
    return s[0]


def obstacles() -> list[tuple[str, int]]:
    """`(name, geom)` of everything a duck can walk into: colliding, not floor-like, at duck height."""
    model, data = world()
    out = []
    for g in range(model.ngeom):
        n = geom_name(model, g)
        if (model.geom_type[g] == mujoco.mjtGeom.mjGEOM_PLANE or not model.geom_contype[g]
                or any(s in n for s in SKIP) or is_duck(model, g)):
            continue
        z, h = data.geom_xpos[g][2], half_height(model, g)
        if z + h > HIT_TOP_MIN_M and z - h < HIT_BOTTOM_MAX_M:
            out.append((n, g))
    return out


class Grid:
    """Occupancy over the apartment, obstacles inflated by `INFLATE_M`, plus the stairwell no-go box."""

    def __init__(self):
        model, data = world()
        self.x0, self.y0 = -4.1, -3.1
        self.nx, self.ny = int(8.2 / CELL_M), int(6.2 / CELL_M)
        xs = self.x0 + (np.arange(self.nx) + 0.5) * CELL_M
        ys = self.y0 + (np.arange(self.ny) + 0.5) * CELL_M
        X, Y = np.meshgrid(xs, ys, indexing="ij")
        self.blocked = np.zeros((self.nx, self.ny), bool)
        for _, g in obstacles():
            p, s, r = data.geom_xpos[g], model.geom_size[g], data.geom_xmat[g].reshape(3, 3)
            lx = (X - p[0]) * r[0, 0] + (Y - p[1]) * r[1, 0]
            ly = (X - p[0]) * r[0, 1] + (Y - p[1]) * r[1, 1]
            if model.geom_type[g] == mujoco.mjtGeom.mjGEOM_BOX:
                dx, dy = np.maximum(np.abs(lx) - s[0], 0), np.maximum(np.abs(ly) - s[1], 0)
                self.blocked |= np.hypot(dx, dy) < INFLATE_M
            else:
                self.blocked |= np.hypot(lx, ly) < s[0] + INFLATE_M
        x0, x1, y0, y1 = sc.NO_GO
        self.blocked |= (X > x0) & (X < x1) & (Y > y0) & (Y < y1)

    def cell(self, x: float, y: float) -> tuple[int, int]:
        return (min(self.nx - 1, max(0, int((x - self.x0) / CELL_M))),
                min(self.ny - 1, max(0, int((y - self.y0) / CELL_M))))

    def centre(self, c: tuple[int, int]) -> tuple[float, float]:
        return self.x0 + (c[0] + 0.5) * CELL_M, self.y0 + (c[1] + 0.5) * CELL_M

    def free_near(self, c: tuple[int, int]) -> tuple[int, int]:
        """The nearest free cell to `c` (itself if free)."""
        if not self.blocked[c]:
            return c
        free = np.argwhere(~self.blocked)
        return tuple(free[np.argmin(((free - c) ** 2).sum(1))])

    def visible(self, a: tuple[int, int], b: tuple[int, int]) -> bool:
        n = max(abs(b[0] - a[0]), abs(b[1] - a[1]), 1)
        for k in range(n + 1):
            if self.blocked[round(a[0] + (b[0] - a[0]) * k / n), round(a[1] + (b[1] - a[1]) * k / n)]:
                return False
        return True

    def plan(self, start, goal) -> list[tuple[float, float]]:
        """Waypoints (apartment m) from `start` to `goal`, 8-connected A*, shortcut by line of sight.
        Empty if unreachable. A blocked start or goal snaps to its nearest free cell."""
        s, g = self.free_near(self.cell(*start)), self.free_near(self.cell(*goal))
        came, cost, todo = {s: None}, {s: 0.0}, [(0.0, s)]
        while todo:
            _, c = heapq.heappop(todo)
            if c == g:
                break
            for dx in (-1, 0, 1):
                for dy in (-1, 0, 1):
                    n = (c[0] + dx, c[1] + dy)
                    if (dx or dy) and 0 <= n[0] < self.nx and 0 <= n[1] < self.ny and not self.blocked[n]:
                        k = cost[c] + math.hypot(dx, dy)
                        if k < cost.get(n, math.inf):
                            cost[n], came[n] = k, c
                            heapq.heappush(todo, (k + math.hypot(g[0] - n[0], g[1] - n[1]), n))
        if g not in came:
            return []
        path = [g]
        while came[path[-1]] is not None:
            path.append(came[path[-1]])
        path.reverse()
        out, i = [], 0
        while i < len(path) - 1:
            j = len(path) - 1
            while j > i + 1 and not self.visible(path[i], path[j]):
                j -= 1
            out.append(self.centre(path[j]))
            i = j
        return out


@cache
def grid() -> Grid:
    return Grid()


def clear_line(a, b, z: float = sc.SIGHT_Z_M) -> bool:
    """No wall or furniture between apartment points `a` and `b`, at height `z`, by a MuJoCo ray."""
    model, data = world()
    d = np.array([b[0] - a[0], b[1] - a[1], 0.0])
    n = float(np.linalg.norm(d))
    if n < 1e-6:
        return True
    hit = np.zeros(1, np.int32)
    dist = mujoco.mj_ray(model, data, np.array([a[0], a[1], z]), d / n, None, 1, -1, hit)
    return dist < 0 or dist >= n


def colour(model, g: int) -> np.ndarray:
    """A geom's RGB: its material's texture averaged (the floors and walls are textured), else the
    material's or geom's own colour."""
    mat = model.geom_matid[g]
    if mat < 0:
        return model.geom_rgba[g][:3]
    tex = model.mat_texid[mat][mujoco.mjtTextureRole.mjTEXROLE_RGB]
    if tex >= 0:
        a, n, c = model.tex_adr[tex], model.tex_width[tex] * model.tex_height[tex], model.tex_nchannel[tex]
        px = model.tex_data[a:a + n * c].reshape(-1, c)[:, :3]
        return px.mean(0) / 255 * model.mat_rgba[mat][:3]
    return model.mat_rgba[mat][:3]


def viewer_geoms() -> list[dict]:
    """The scene's visible geoms for the page: `{kind, s, p, q, c, wall}` in apartment metres."""
    model, data = world()
    kinds = {mujoco.mjtGeom.mjGEOM_BOX: "box", mujoco.mjtGeom.mjGEOM_CYLINDER: "cylinder",
             mujoco.mjtGeom.mjGEOM_SPHERE: "sphere", mujoco.mjtGeom.mjGEOM_CAPSULE: "capsule",
             mujoco.mjtGeom.mjGEOM_ELLIPSOID: "ellipsoid"}
    out = []
    for g in range(model.ngeom):
        kind = kinds.get(int(model.geom_type[g]))
        if kind is None or is_duck(model, g) or geom_name(model, g).startswith("lm_"):
            continue
        rgb = colour(model, g)
        q = np.zeros(4)
        mujoco.mju_mat2Quat(q, data.geom_xmat[g])
        n = geom_name(model, g)
        out.append({"kind": kind, "s": np.round(model.geom_size[g], 4).tolist(),
                    "p": np.round(data.geom_xpos[g], 4).tolist(), "q": np.round(q, 5).tolist(),
                    "c": "#%02x%02x%02x" % tuple(int(255 * v) for v in rgb),
                    "wall": n.startswith("w") and kind == "box"})
    return out


def main():
    gr = grid()
    print(f"{len(obstacles())} obstacles; grid {gr.nx}×{gr.ny} at {CELL_M * 100:.0f} cm, "
          f"{gr.blocked.mean() * 100:.0f}% blocked (inflated {INFLATE_M} m)")
    for duck, pts in sc.PATROLS.items():
        start = (0.0, sc.DUCKS.index(duck) * 0.5)
        legs = [gr.plan(a, b) for a, b in zip([start] + pts, pts)]
        print(f"  {duck}: {sum(map(len, legs))} waypoints over {len(pts)} patrol points"
              + ("" if all(legs) else f"; UNREACHABLE: {[p for p, l in zip(pts, legs) if not l]}"))
    print("trash visible from the corridor line:", [t for t, p in sc.TRASH.items() if clear_line((0.0, 0.0), p)])
    print(f"{len(viewer_geoms())} geoms for the viewer")


if __name__ == "__main__":
    main()
