"""Situation pictures of an arena scenario at chosen instants, drawn from a saved ledger and
`out/wind.arrow`: what a decision maker sees. One PNG per instant into `out/snapshots/`.

Each entity is drawn at its latest row at or before the instant; boats trail their last 5 min,
crews their last 30. The fire's perimeter is rebuilt from its vertex rows (`FireReplay`) and
drawn over the fuel map from `out/fuel.arrow`.

    python -m tools.snapshot_sim out/sim_study_0.arrow --scenario regatta --at 2026-09-05T17:20:00
    python -m tools.snapshot_sim out/sim_study_0.arrow --scenario regatta --every 10m
    python -m tools.snapshot_sim out/sim_study_0.arrow --scenario wildfire --every 2h
"""

import argparse
import re
from datetime import datetime, timedelta
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
import pyarrow as pa
import pyarrow.compute as pc

from sim import scenario as sc
from sim.models import regatta as rg
from sim.models import wildfire as wf
from sim.wind import read_table
from soloc_client import CENTURY_NS, id_bytes, matches, positions, sts_field, tai_ns_from_utc
from tools.plot_sim import INK_MUTED, MARK, SERIES, TRACK, WATER, style
from tools.view_sim import load

TAIL_S = 300
ADT = timedelta(hours=-3)


class Rows:
    """The rows framed on one venue, as arrays, with each entity's rows in time order."""

    def __init__(self, table: pa.Table, names: dict[bytes, str], venue_id: bytes, t0_ns: int):
        table = table.filter(pa.array(matches(id_bytes(sts_field(table, "frame_id")), venue_id)))
        t_ns = (sts_field(table, "duration_centuries").to_numpy().astype(np.int64) * CENTURY_NS
                + sts_field(table, "duration_ns").to_numpy().astype(np.int64))
        self.t_s = (t_ns - t0_ns) / 1e9
        self.names = np.array([names.get(i, i.hex()) for i in id_bytes(table.column("entity_id"))])
        self.pos = positions(table)[:, :2]
        q = sts_field(table, "quaternion").flatten().to_numpy().reshape(-1, 4)
        self.heading = (90 - np.degrees(2 * np.arctan2(q[:, 3], q[:, 0]))) % 360
        v = table.column("velocity").combine_chunks().flatten().to_numpy().reshape(-1, 3)
        self.speed = np.hypot(v[:, 0], v[:, 1])
        self.length = np.array([d[0] if d else np.nan for d in table.column("dimensions").to_pylist()])
        self.index = {}
        for n in np.unique(self.names):
            k = np.flatnonzero(self.names == n)
            self.index[n] = k[np.argsort(self.t_s[k], kind="stable")]

    def upto(self, name: str, t_s: float) -> np.ndarray:
        k = self.index.get(name, np.array([], np.int64))
        return k[self.t_s[k] <= t_s]


class FireView:
    """The wildfire as stored: the replayed perimeter, and each trench as `(born, a, b)`."""

    def __init__(self, w: wf.Wildfire, rows: Rows):
        vertex = np.array([n.startswith("FIRE-V") for n in rows.names])
        self.replay = wf.FireReplay(rows.t_s[vertex], rows.names[vertex], rows.pos[vertex])
        self.lines = []
        for tr in w.trenches:
            k = rows.index.get(tr.name)
            if k is None:
                continue
            j = k[0]
            h = np.radians(rows.heading[j])
            half = rows.length[j] / 2 * np.array([np.sin(h), np.cos(h)])
            self.lines.append((rows.t_s[j], rows.pos[j] - half, rows.pos[j] + half))
        self.end = max(self.replay.stop.values())     # inf if never contained

    def perimeter(self, t_s: float) -> np.ndarray:
        ring = self.replay.ring_at(t_s)
        return self.replay.at(ring, t_s) if ring else np.empty((0, 2))

    def area_ha(self, t_s: float) -> float:
        xy = self.perimeter(t_s)
        return float(abs(np.dot(xy[:, 0], np.roll(xy[:, 1], -1)) - np.dot(xy[:, 1], np.roll(xy[:, 0], -1))) / 2e4)

    def moving(self, t_s: float) -> np.ndarray:
        names = [n for n in self.replay.ring_at(t_s) if self.replay.moving(n, t_s)]
        return self.replay.at(names, t_s) if names else np.empty((0, 2))


def fuel_grid(path: Path, venue) -> np.ndarray:
    """The venue's `r0_factor` on the fuel grid, north-major, as `(rows, cols)`."""
    table = read_table(path)
    table = table.filter(pc.equal(table["venue"], venue.name))
    e0, e1, n0, n1 = sc.FIRE_WIND.extent_m
    cols = len(np.arange(e0, e1 + 1e-6, sc.FUEL_GRID_M))
    return table["r0_factor"].to_numpy().reshape(-1, cols)


def draw_fuel(ax, fuel: np.ndarray):
    """Fuel as grey lightness by R0 factor (darker burns faster); the river (0) as water."""
    e0, e1, n0, n1 = sc.FIRE_WIND.extent_m
    h = sc.FUEL_GRID_M / 2
    grey = np.clip(1 - fuel / 2.4, 0, 1)
    rgb = np.dstack([grey * 0.92 + 0.05] * 3)
    rgb[fuel == 0] = matplotlib.colors.to_rgb(WATER)
    ax.imshow(rgb, origin="lower", extent=(e0 - h, e1 + h, n0 - h, n1 + h), interpolation="nearest", zorder=0)


def wind_at(table: pa.Table, venue, t_s: float) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """The venue's latest wind grid at or before `t_s`, as venue ENU `(xy, u, v)`."""
    table = table.filter(pc.equal(table["venue"], venue.name))
    t = (table["t"].cast(pa.int64()).to_numpy() - int((sc.T0 - datetime(1970, 1, 1)).total_seconds()))
    sel = t[t <= t_s]
    if not len(sel):
        return np.empty((0, 2)), np.empty(0), np.empty(0)
    rows = table.filter(pa.array(t == sel.max()))
    lat, lon = rows["lat"].to_numpy(), rows["lon"].to_numpy()
    xy = np.array([venue.enu(a, o)[:2] for a, o in zip(lat, lon)])
    return xy, rows["u"].to_numpy(), rows["v"].to_numpy()


def standings(g: rg.Regatta, rows: Rows, t_s: float) -> list[str]:
    out = []
    for b in g.boats:
        k = rows.upto(b.name, t_s)
        r, _ = rg.replay(g.course, rows.t_s[k], rows.pos[k])
        nxt = r.next_mark
        if r.finish_s is not None:
            key, note = (0, r.finish_s), f"finished {timedelta(seconds=round(r.finish_s - rg.GUN_S))}"
        else:
            marks = {"W": ["MARK-W"], "GATE": ["MARK-GATE-1", "MARK-GATE-2"]}.get(nxt, [])
            to = min((np.linalg.norm(rows.pos[k[-1]] - g.course.marks[m]) for m in marks),
                     default=abs(g.course.above(rows.pos[k[-1]])) if len(k) else 0.0)
            leg = max(r.leg, 0)
            key = (1, -leg, to)
            note = f"leg {leg + 1}/{len(g.course.legs)} → {nxt} {to:,.0f} m, {rows.speed[k[-1]]:.1f} m/s"
            if r.ocs and r.start_s is None:
                note += ", OCS"
        out.append((key, f"{b.name}  {note}"))
    return [f"{i + 1:2d}. {s}" for i, (_, s) in enumerate(sorted(out))]


def draw_regatta(g: rg.Regatta, rows: Rows, wind: pa.Table, t_s: float, out_dir: Path) -> Path:
    fig, (whole, near, panel) = plt.subplots(1, 3, figsize=(17, 6.5), layout="constrained",
                                             gridspec_kw={"width_ratios": [1, 1.6, 0.9]})
    rings = [np.array([g.venue.enu(a, o)[:2] for a, o in ring]) for ring in sc.BASIN_WATER]
    xy, u, v = wind_at(wind, g.venue, t_s)
    c = g.course
    for ax in (whole, near):
        for ring in rings:
            ax.fill(ring[:, 0], ring[:, 1], color=WATER, linewidth=0)
        if len(xy):
            ax.quiver(xy[:, 0], xy[:, 1], u, v, color=INK_MUTED, alpha=0.6, width=0.003,
                      scale=60 if ax is whole else 40)
        ax.plot(*np.column_stack([c.rc, c.marks["MARK-PIN"]]), "--", color=INK_MUTED, linewidth=0.8)
        for m in g.marks:
            k = rows.upto(m.name, t_s)
            if len(k) and t_s - rows.t_s[k[-1]] < sc.MARK_CADENCE_S:
                ax.plot(*rows.pos[k[-1]], "o", color=MARK, markersize=5)
        for b in g.buoys:
            k = rows.upto(b.name, t_s)
            if len(k):
                ax.plot(*rows.pos[k[-1]], "D", color=SERIES[2], markersize=5)
        for i, b in enumerate((g.rc, *g.boats)):
            k = rows.upto(b.name, t_s)
            if not len(k):
                continue
            tail = k[rows.t_s[k] >= t_s - TAIL_S]
            colour = INK_MUTED if b is g.rc else TRACK
            ax.plot(rows.pos[tail, 0], rows.pos[tail, 1], "-", color=colour, linewidth=0.6)
            marker = "s" if b is g.rc else (3, 0, -rows.heading[k[-1]])
            ax.plot(*rows.pos[k[-1]], marker=marker, color=colour, markersize=7 if ax is near else 4)
            if ax is near and b is not g.rc:
                ax.annotate(b.name[-2:], rows.pos[k[-1]], fontsize=6, xytext=(4, 4),
                            textcoords="offset points")
        ax.set_aspect("equal")
        style(ax)
    e0, e1, n0, n1 = sc.REGATTA_WIND.extent_m
    whole.set_xlim(e0, e1)
    whole.set_ylim(n0, n1)
    whole.set_title(f"{g.venue.name}, venue ENU (m)", fontsize=9)
    pts = np.array([c.rc, *c.marks.values(), *c.staging])
    lo, hi = pts.min(0) - 400, pts.max(0) + 400
    near.set_xlim(lo[0], hi[0])
    near.set_ylim(lo[1], hi[1])
    near.set_title("course (5 min tails, ▲ heading, ■ RC boat, ● marks, ◆ met buoys)", fontsize=9)

    when = sc.T0 + timedelta(seconds=t_s)
    lines = [f"{when:%Y-%m-%d %H:%M:%S} UTC", f"{when + ADT:%H:%M:%S} ADT",
             f"gun {sc.REGATTA_GUN + ADT:%H:%M} ADT", ""]
    if len(xy):
        for b in g.buoys:
            k = int(np.argmin(np.linalg.norm(xy - b.xy, axis=1)))
            tws = float(np.hypot(u[k], v[k]))
            twd = float(np.degrees(np.arctan2(-u[k], -v[k])) % 360)
            lines.append(f"{b.name}  {twd:5.1f}° {tws:4.1f} m/s")
        lines.append("")
    lines += standings(g, rows, t_s)
    panel.axis("off")
    panel.text(0, 1, "\n".join(lines), va="top", family="monospace", fontsize=8)
    path = out_dir / f"regatta_{when:%Y%m%dT%H%M%S}.png"
    fig.savefig(path, dpi=110)
    plt.close(fig)
    return path


FIRE_RED = "#e34948"                 # the status "critical" red: the fire is a hazard, not a series
PDT = timedelta(hours=-7)


def draw_wildfire(w: wf.Wildfire, rows: Rows, fire: FireView, fuel: np.ndarray, wind: pa.Table,
                  t_s: float, out_dir: Path) -> Path:
    fig, (whole, near, panel) = plt.subplots(1, 3, figsize=(17, 6.5), layout="constrained",
                                             gridspec_kw={"width_ratios": [1, 1.6, 0.9]})
    xy, u, v = wind_at(wind, w.venue, t_s)
    ring = fire.perimeter(t_s)
    for ax in (whole, near):
        draw_fuel(ax, fuel)
        if len(xy):
            ax.quiver(xy[:, 0], xy[:, 1], u, v, color=INK_MUTED, alpha=0.5, width=0.003,
                      scale=50 if ax is whole else 35)
        if len(ring):
            ax.fill(ring[:, 0], ring[:, 1], color=FIRE_RED, alpha=0.25, linewidth=0)
            ax.plot(*np.vstack([ring, ring[:1]]).T, color=FIRE_RED, linewidth=1.2)
        for born, a, b in fire.lines:
            if born <= t_s:
                ax.plot([a[0], b[0]], [a[1], b[1]], color="#3d3c37", linewidth=2.0, solid_capstyle="butt")
        ax.plot(*sc.ICP_M, "^", color="#3d3c37", markersize=7)
        for c in w.crews:
            k = rows.upto(c.name, t_s)
            if not len(k):
                continue
            tail = k[rows.t_s[k] >= t_s - 1800]
            ax.plot(rows.pos[tail, 0], rows.pos[tail, 1], "-", color=TRACK, linewidth=0.6)
            ax.plot(*rows.pos[k[-1]], "s", color=TRACK, markersize=5 if ax is near else 3)
            if ax is near:
                ax.annotate(c.name.split("-")[1], rows.pos[k[-1]], fontsize=6, xytext=(3, 3),
                            textcoords="offset points")
        ax.set_aspect("equal")
        style(ax)
    e0, e1, n0, n1 = sc.FIRE_WIND.extent_m
    whole.set_xlim(e0, e1)
    whole.set_ylim(n0, n1)
    whole.set_title(f"{w.venue.name}, venue ENU (m); grey = fuel (darker burns faster)", fontsize=9)
    pts = np.vstack([ring if len(ring) else np.zeros((1, 2)), *[np.array([a, b]) for _, a, b in fire.lines], [sc.ICP_M]])
    lo, hi = pts.min(0) - 250, pts.max(0) + 250
    near.set_xlim(lo[0], hi[0])
    near.set_ylim(lo[1], hi[1])
    near.set_title("fire (red), finished line (dark), crews with 30 min tails (blue ■), ICP ▲", fontsize=9)

    when = sc.T0 + timedelta(seconds=t_s)
    since = t_s - wf.IGNITION_S
    line_km = sum(float(np.linalg.norm(b - a)) for born, a, b in fire.lines if born <= t_s) / 1000
    moving = fire.moving(t_s)
    lines = [f"{when:%Y-%m-%d %H:%M:%S} UTC", f"{when + PDT:%H:%M:%S} PDT",
             f"ignition {sc.FIRE_IGNITION + PDT:%m-%d %H:%M} PDT" + (f", +{timedelta(seconds=round(since))}" if since >= 0 else ""),
             "",
             f"burnt     {fire.area_ha(t_s):8.1f} ha",
             f"perimeter {len(ring):5d} vertices, {len(moving)} spreading",
             f"line      {line_km:8.2f} km finished",
             ("contained " + f"{timedelta(seconds=round(fire.end - wf.IGNITION_S))} after ignition"
              if t_s >= fire.end else "not contained"),
             ""]
    if len(xy):
        k = int(np.argmin(np.linalg.norm(xy - np.array(sc.ICP_M), axis=1)))
        lines.append(f"wind at ICP {np.degrees(np.arctan2(-u[k], -v[k])) % 360:5.1f}° {np.hypot(u[k], v[k]):4.1f} m/s")
        lines.append("")
    for c in w.crews:
        k = rows.upto(c.name, t_s)
        if len(k):
            p = rows.pos[k[-1]]
            gap = f"{np.linalg.norm(moving - p, axis=1).min():7.0f} m" if len(moving) else "   none"
            lines.append(f"{c.name:8s} {rows.speed[k[-1]]:4.2f} m/s, nearest spreading front {gap}")
    panel.axis("off")
    panel.text(0, 1, "\n".join(lines), va="top", family="monospace", fontsize=8)
    path = out_dir / f"wildfire_{when:%Y%m%dT%H%M%S}.png"
    fig.savefig(path, dpi=110)
    plt.close(fig)
    return path


def parse_every(text: str) -> int:
    m = re.fullmatch(r"(\d+)([smh]?)", text)
    if not m:
        raise argparse.ArgumentTypeError(f"{text!r} is not like 30s, 10m or 1h")
    return int(m[1]) * {"": 1, "s": 1, "m": 60, "h": 3600}[m[2]]


SCENARIOS = {"regatta": (rg.ON_S, rg.OFF_S), "wildfire": (wf.IGNITION_S, None)}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("path")
    p.add_argument("--scenario", choices=sorted(SCENARIOS), required=True)
    when = p.add_mutually_exclusive_group(required=True)
    when.add_argument("--at", type=datetime.fromisoformat, help="UTC, e.g. 2026-09-05T17:20:00")
    when.add_argument("--every", type=parse_every,
                      help="over the scenario's window (the wildfire's: ignition to contained), e.g. 10m")
    p.add_argument("--wind", default=None, help="wind table (default: wind.arrow beside FILE)")
    p.add_argument("--out", default="out/snapshots")
    args = p.parse_args()

    path = Path(args.path)
    table, names = load(path)
    wind = read_table(Path(args.wind) if args.wind else path.with_name("wind.arrow"))
    t0_ns = tai_ns_from_utc(sc.T0)
    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)
    if args.scenario == "regatta":
        g = rg.Regatta(sc.SEED)
        rows = Rows(table, names, g.venue.id, t0_ns)
        draw = lambda t_s: draw_regatta(g, rows, wind, t_s, out_dir)
        on, off = SCENARIOS["regatta"]
    else:
        w = wf.Wildfire(sc.SEED)
        rows = Rows(table, names, w.venue.id, t0_ns)
        fire = FireView(w, rows)
        fuel = fuel_grid(path.with_name("fuel.arrow"), w.venue)
        draw = lambda t_s: draw_wildfire(w, rows, fire, fuel, wind, t_s, out_dir)
        on, off = wf.IGNITION_S, int(min(fire.end, sc.DURATION_S))
    times = [sc.seconds(args.at)] if args.at else range(on, off + 1, args.every)
    for t_s in times:
        print(draw(t_s))


if __name__ == "__main__":
    main()
