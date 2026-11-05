"""The default incident commander: indirect attack, flanks to head.

It plans a closed line around where the fire will be when each stretch of line is finished:
the convex hull of the moving vertices, each pushed out along its normal by `OFFSET_M` plus its
worst-case spread rate (the wind shifted by up to `SHIFT_DEG` and gusting by `GUST`) × the time
until the line near it is dug (times `SAFETY`), but no further than the
first finished trench in the way. Stopped vertices stay where they are. Hull points within
`COVER_M` of a finished trench are already done; the rest form arcs to dig. A full loop splits
at the heel (upwind end) and the head into two flanks; crews share the arcs by length, each cut
into equal chunks, and dig from the heel end toward the head. The dig times and the hull are
iterated together a few times.

A crew walks the hull to its chunk, then digs it. A blocked crew is sent round the other way;
one that had to escape gives its chunk up. The commander replans (at most every `REPLAN_S`)
when a moving vertex gets outside the planned line.
"""

import numpy as np
from matplotlib.path import Path as MplPath

from sim.strategies import WildfireObservation

OFFSET_M = 50.0
SAFETY = 1.2
SHIFT_DEG = 25.0                     # plan for the worst of the wind turning this far either way
GUST = 1.3                           # and blowing this much harder
REPLAN_S = 900
STEP_M = 10.0
COVER_M = 15.0
OVERLAP = 2                          # arcs run this many points into finished line at each end
ITERATIONS = 8


def hull(points: np.ndarray) -> np.ndarray:
    """Convex hull, counter-clockwise (monotone chain)."""
    pts = sorted(map(tuple, points))
    cross = lambda o, a, b: (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    lower, upper = [], []
    for p in pts:
        while len(lower) >= 2 and cross(lower[-2], lower[-1], p) <= 0:
            lower.pop()
        lower.append(p)
    for p in reversed(pts):
        while len(upper) >= 2 and cross(upper[-2], upper[-1], p) <= 0:
            upper.pop()
        upper.append(p)
    return np.array(lower[:-1] + upper[:-1])


def resample(loop: np.ndarray, step: float) -> np.ndarray:
    """A closed loop as points every `step` metres (not repeating the first)."""
    closed = np.vstack([loop, loop[:1]])
    s = np.concatenate([[0], np.cumsum(np.linalg.norm(np.diff(closed, axis=0), axis=1))])
    at = np.arange(0, s[-1], step)
    return np.column_stack([np.interp(at, s, closed[:, 0]), np.interp(at, s, closed[:, 1])])


def cumulative(path: np.ndarray) -> np.ndarray:
    return np.concatenate([[0], np.cumsum(np.linalg.norm(np.diff(path, axis=0), axis=1))])


def covered(loop: np.ndarray, lines) -> np.ndarray:
    """Loop points within `COVER_M` of a finished trench."""
    if not lines:
        return np.zeros(len(loop), bool)
    a = np.array([l[0] for l in lines])
    e = np.array([l[1] for l in lines]) - a
    t = np.clip(((loop[:, None] - a[None]) * e[None]).sum(-1) / np.maximum((e * e).sum(-1), 1e-9)[None], 0, 1)
    return np.linalg.norm(a[None] + t[..., None] * e[None] - loop[:, None], axis=-1).min(1) <= COVER_M


def arcs(loop: np.ndarray, todo: np.ndarray, down: np.ndarray) -> list[list[int]]:
    """Index runs of the loop still to dig, each ordered from its heel end. With nothing done
    yet, the two flanks from the heel to the head."""
    n = len(loop)
    if todo.all():
        heel, head = int(np.argmin(loop @ down)), int(np.argmax(loop @ down))
        return [[(heel + k) % n for k in range((head - heel) % n + 1)],
                [(heel - k) % n for k in range((heel - head) % n + 1)]]
    start = int(np.flatnonzero(~todo)[0])
    runs, run = [], []
    for k in range(1, n + 1):
        i = (start + k) % n
        if todo[i]:
            run.append(i)
        elif run:
            runs.append(run)
            run = []
    out = []
    for run in runs:
        run = ([(run[0] - k) % n for k in range(OVERLAP, 0, -1)] + run
               + [(run[-1] + k) % n for k in range(1, OVERLAP + 1)])
        out.append(run if loop[run[0]] @ down <= loop[run[-1]] @ down else run[::-1])
    return out


def chunks_for(loop, runs, crews: int) -> list[list[int]]:
    """Crews shared out over the runs by length (at least one each, longest first), each run cut
    into equal consecutive chunks of loop indices."""
    if not runs:
        return []
    lengths = [cumulative(loop[r])[-1] for r in runs]
    order = sorted(range(len(runs)), key=lambda k: -lengths[k])[:crews]
    share = {k: 1 for k in order}
    for _ in range(crews - len(order)):
        k = max(order, key=lambda k: lengths[k] / share[k])
        share[k] += 1
    out = []
    for k in order:
        run = runs[k]
        s = cumulative(loop[run])
        cuts = np.searchsorted(s, np.linspace(0, s[-1], share[k] + 1))
        cuts[-1] = len(run) - 1
        out += [run[cuts[j]:max(cuts[j + 1], cuts[j] + 1) + 1] for j in range(share[k])]
    return out


def clip(p: np.ndarray, d: np.ndarray, lines) -> np.ndarray:
    """Per ray `p → p + d`, the fraction at the first finished trench it crosses (1 if none)."""
    if not lines:
        return np.ones(len(p))
    a = np.array([l[0] for l in lines])
    e = np.array([l[1] for l in lines]) - a
    cross = lambda u, v: u[..., 0] * v[..., 1] - u[..., 1] * v[..., 0]
    ap = a[None] - p[:, None]
    den = cross(d[:, None], e[None])
    with np.errstate(divide="ignore", invalid="ignore"):
        s, u = cross(ap, e[None]) / den, cross(ap, d[:, None]) / den
    return np.where((den != 0) & (s >= 0) & (s <= 1) & (u >= 0) & (u <= 1), s, 1.0).min(1)


def assign(chunks, names, at, loop) -> dict[str, int]:
    """Each chunk to the nearest free crew, nearest pairs first."""
    pairs = sorted((float(np.linalg.norm(loop[c[0]] - at[i])), i, k) for k, c in enumerate(chunks) for i in range(len(names)))
    out, used = {}, set()
    for _, i, k in pairs:
        if names[i] not in out and k not in used:
            out[names[i]] = k
            used.add(k)
    return out


def plan(obs: WildfireObservation) -> dict:
    p = np.array([v.xy for v in obs.perimeter])
    tangent = np.roll(p, -1, 0) - np.roll(p, 1, 0)
    normal = np.column_stack([tangent[:, 1], -tangent[:, 0]])
    normal /= np.linalg.norm(normal, axis=1, keepdims=True)
    moving = np.array([v.name in obs.moving for v in obs.perimeter])
    rate = np.zeros(len(p))
    for k in np.flatnonzero(moving):
        twd, tws = obs.wind(*p[k])
        rate[k] = max(obs.spread(*p[k], *normal[k], twd=twd + s, tws=tws * GUST)
                      for s in (-SHIFT_DEG, 0.0, SHIFT_DEG))
    names = sorted(obs.crews)
    at = np.array([obs.crews[c].xy for c in names])
    twd, _ = obs.wind(*p.mean(0))
    down = -np.array([np.sin(np.radians(twd)), np.cos(np.radians(twd))])
    dig_m_s = obs.dig_m_h / 3600

    lead_s = np.full(len(p), cumulative(np.vstack([p, p[:1]]))[-1] / (len(names) * dig_m_s))
    for _ in range(ITERATIONS):
        d = normal * np.where(moving, rate * lead_s + OFFSET_M, 0.0)[:, None]
        q = p + d * clip(p, d, obs.lines)[:, None]
        loop = resample(hull(q), STEP_M)
        runs = arcs(loop, ~covered(loop, obs.lines), down)
        chunks = chunks_for(loop, runs, len(names))
        jobs = assign(chunks, names, at, loop)
        done = np.zeros(len(loop))                   # seconds until each loop point is dug
        for name, k in jobs.items():
            c = chunks[k]
            walk = float(np.linalg.norm(loop[c[0]] - at[names.index(name)])) / obs.walk_m_s
            done[c] = np.maximum(done[c], walk + cumulative(loop[c]) / dig_m_s)
        near = np.argmin(np.linalg.norm(q[:, None] - loop[None], axis=-1), axis=1)
        lead_s = 0.5 * lead_s + 0.5 * SAFETY * done[near]

    plans = {name: {"dig": loop[chunks[k]].tolist(), "stage": "new"} for name, k in jobs.items()}
    return {"t": obs.t_s, "loop": loop.tolist(), "jobs": plans}


def route(loop: np.ndarray, start: np.ndarray, to: np.ndarray, shorter: bool = True) -> list:
    """From `start` to the nearest point of the loop, then along it to `to`, the shorter way
    round (or the longer)."""
    n = len(loop)
    i = int(np.argmin(np.linalg.norm(loop - start, axis=1)))
    j = int(np.argmin(np.linalg.norm(loop - to, axis=1)))
    ahead, behind = (j - i) % n, (i - j) % n
    forward = (ahead <= behind) == shorter
    idx = [(i + k) % n for k in range(ahead + 1)] if forward else [(i - k) % n for k in range(behind + 1)]
    return [*loop[idx[::5]].tolist(), list(to)]


def breached(obs: WildfireObservation, p: dict) -> bool:
    line = MplPath(np.array(p["loop"]))
    return any(not line.contains_point(v.xy) for v in obs.perimeter if v.name in obs.moving)


def commander(obs: WildfireObservation) -> dict:
    m = obs.memory
    p = m.get("plan")
    if p is None or (obs.t_s - p["t"] >= REPLAN_S and breached(obs, p)):
        p = m["plan"] = plan(obs)
    loop = np.array(p["loop"])
    cmds = {}
    for name in obs.crews:
        job = p["jobs"].get(name)
        task, at = obs.tasks[name], obs.crews[name].xy
        if job is None or job["stage"] == "done":
            if task not in ("idle", "escape"):
                cmds[name] = ("hold",)
        elif task == "escape":
            job["stage"] = "done"
        elif job["stage"] == "new":
            cmds[name], job["stage"] = ("move", route(loop, at, job["dig"][0])), "walking"
        elif job["stage"] == "walking" and task == "blocked":
            job["round"] = not job.get("round", False)
            cmds[name] = ("move", route(loop, at, job["dig"][0], shorter=not job["round"]))
        elif job["stage"] == "walking" and task == "idle":
            cmds[name], job["stage"] = ("dig", job["dig"]), "digging"
        elif job["stage"] == "digging" and task in ("idle", "blocked"):
            job["stage"] = "done"
            if task == "blocked":
                cmds[name] = ("hold",)
    return cmds
