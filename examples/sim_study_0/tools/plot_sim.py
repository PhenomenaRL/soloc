"""Sanity-check plots of a saved sim ledger, one PNG per figure into out/plots/.

Reloads the file into the server first, so it can run right after run_sim.py.
"""

import argparse
import math
from datetime import timedelta
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
import pyarrow as pa

from sim import scenario as sc
from sim.ephemeris import Ephemeris
from sim.geo import EARTH, GCRF, MARS, MOON, SUN, GreatCircle, fixed_to_geodetic
from sim.land import CANALS, outlines
from sim.models import factory as fm
from sim.models import regatta as rg
from sim.models import wildfire as wf
from sim.models.sailboat import polar
from sim.roster import roster
from soloc_client import (CENTURY_NS, SolocClient, id_bytes, matches, positions, sts_field,
                          tai_ns_from_utc)

TRACK = "#2a78d6"
MARK = "#eb6834"
INK_MUTED = "#6b6a63"
GRID = "#e6e5df"
BODY_FILL = "#d9d8d2"
WATER = "#dcebf5"
SERIES = ("#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4")   # categorical slots 1-5
DAYS = sc.DURATION_S // 86400


def style(ax):
    ax.grid(color=GRID, linewidth=0.5)
    ax.tick_params(labelsize=7, colors=INK_MUTED)


def plot_site_robots(facility, robots, rows, ids, frames, out_dir: Path) -> Path:
    """Robot tracks in the site's ENU frame (the frame they are stored in), one panel each, over
    the site layout. `robots` may include a disembarked crawler or a cargo robot; only their
    rows framed on this facility are drawn."""
    cols = 5 if len(robots) <= 10 else 6
    fig, axes = plt.subplots(2, cols, figsize=(3.2 * cols, 6.8), sharex=True, sharey=True,
                             layout="constrained")
    w = sc.SITE_HALF_WIDTH_M
    r0, r1 = sc.SITE_ROADS_M[0], sc.SITE_ROADS_M[-1]
    for ax in axes.flat[len(robots):]:
        ax.set_visible(False)
    for ax, robot in zip(axes.flat, robots):
        for c in sc.SITE_ROADS_M:
            ax.plot([r0, r1], [c, c], color=GRID, linewidth=2.5, zorder=0)
            ax.plot([c, c], [r0, r1], color=GRID, linewidth=2.5, zorder=0)
        mask = matches(ids, robot.id) & matches(frames, facility.id)
        xyz = positions(rows.filter(pa.array(mask)))
        # Dots, not a line: at 30 s rows an Earth robot moves up to 45 m between samples, and
        # joining them would cut every corner differently on every lap.
        ax.plot(xyz[:, 0], xyz[:, 1], ".", color=TRACK, markersize=1.2)
        ax.plot(*xyz[0, :2], "o", color=MARK, markersize=5)
        ax.add_patch(plt.Rectangle((-w, -w), 2 * w, 2 * w, fill=False, linestyle="--",
                                   edgecolor=INK_MUTED, linewidth=0.8))
        role = getattr(robot, "role", None) or f"survey, off {robot.host.name}"
        ax.set_title(f"{robot.name} ({role})", fontsize=9)
        ax.set_aspect("equal")
        ax.set_xlim(-1.1 * w, 1.1 * w)
        ax.set_ylim(-1.1 * w, 1.1 * w)
        style(ax)
    for ax in axes[-1]:
        ax.set_xlabel("east (m)", fontsize=8)
    for ax in axes[:, 0]:
        ax.set_ylabel("north (m)", fontsize=8)
    fig.suptitle(f"{facility.name}: robot rows over {DAYS} days, site ENU (small dots = 30 s rows, "
                 f"large dot = first row on the site, dashed = site area, grey = roads)", fontsize=11)
    path = out_dir / f"robots_{facility.spec.code.lower()}.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def epochs_ns(table: pa.Table) -> np.ndarray:
    c = sts_field(table, "duration_centuries").to_numpy().astype(np.int64)
    return c * CENTURY_NS + sts_field(table, "duration_ns").to_numpy().astype(np.int64)


def body_centred_icrf(client: SolocClient, rows: pa.Table, body) -> np.ndarray:
    """Positions (km) of rows framed on `body`'s IAU frame, relative to the body's centre with
    ICRF axes. The centre comes from exchanging a zero offset in the body frame at each row's
    epoch, so it is exact at every epoch rather than interpolated from the hourly snapshots."""
    centre = client.buffer()
    for t in epochs_ns(rows).tolist():
        centre.append(body.frame_id, body.frame_id, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], t)
    centre = pa.Table.from_batches([centre.flush()])
    return positions(client.exchange(rows, "ICRF")) - positions(client.exchange(centre, "ICRF"))


def plot_orbits(client, rows, ids, frames, spacecraft, out_dir: Path) -> Path:
    """Every spacecraft row framed on a body's IAU frame, body-centred with ICRF axes, projected
    onto the ICRF x–y and x–z planes. Pad and landed rows are framed on facilities and left out."""
    bodies = (EARTH, MOON, MARS)
    fig, axes = plt.subplots(2, 3, figsize=(15, 10), layout="constrained")
    for col, body in enumerate(bodies):
        crafts = [c for c in spacecraft if c.body is body and c.id in set(ids)]
        extent = body.a_km
        for slot, craft in enumerate(crafts):
            mask = matches(ids, craft.id) & matches(frames, body.frame_id)
            xyz = body_centred_icrf(client, rows.filter(pa.array(mask)), body)
            extent = max(extent, float(np.abs(xyz).max()))
            for row, (a, b) in enumerate(((0, 1), (0, 2))):
                axes[row, col].plot(xyz[:, a], xyz[:, b], color=SERIES[slot], linewidth=0.6,
                                    label=craft.name)
        for row, label in enumerate(("y", "z")):
            ax = axes[row, col]
            ax.add_patch(plt.Circle((0, 0), body.a_km, color=BODY_FILL, zorder=0))
            lim = 1.08 * extent
            ax.set_xlim(-lim, lim)
            ax.set_ylim(-lim, lim)
            ax.set_aspect("equal")
            ax.set_xlabel("ICRF x (km)", fontsize=8)
            ax.set_ylabel(f"ICRF {label} (km)", fontsize=8)
            style(ax)
        axes[0, col].set_title(f"{body.name}-centred", fontsize=10)
        legend = axes[0, col].legend(fontsize=8, frameon=False, loc="upper right")
        for line in legend.get_lines():
            line.set_linewidth(2)
    fig.suptitle(f"Spacecraft over {DAYS} days, body-centred with ICRF axes "
                 "(top: x–y, bottom: x–z; grey disc = body)", fontsize=11)
    path = out_dir / "orbits.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_heliocentric(client, rows, ids, t_s, probes, out_dir: Path) -> Path:
    """Each probe's stored ICRF rows, Sun-centred with ICRF axes (the Sun's centre from the
    kernels at every row's epoch), on the x–y and x–z planes. The Sun is drawn to scale."""
    fig, axes = plt.subplots(len(probes), 2, figsize=(11, 5.2 * len(probes)), layout="constrained",
                             squeeze=False)
    for row, probe in zip(axes, probes):
        mask = matches(ids, probe.id)
        order = np.argsort(t_s[mask])
        t = t_s[mask][order]
        xyz = (positions(rows.filter(pa.array(mask)))[order] - Ephemeris(client).centre(SUN, t)) / 1e6
        r = np.linalg.norm(xyz, axis=1)
        k = int(np.argmin(r))
        lim = 1.08 * float(np.abs(xyz).max())
        for ax, (a, b), label in zip(row, ((0, 1), (0, 2)), ("y", "z")):
            ax.add_patch(plt.Circle((0, 0), SUN.a_km / 1e6, color=SERIES[3], zorder=0))
            ax.plot(xyz[:, a], xyz[:, b], color=TRACK, linewidth=1.2)
            ax.plot(*xyz[0, [a, b]], "o", color=TRACK, markersize=5)
            ax.plot(*xyz[k, [a, b]], "o", color=MARK, markersize=5)
            ax.annotate("T0", xyz[0, [a, b]], textcoords="offset points", xytext=(6, 6),
                        fontsize=8, color=INK_MUTED)
            if 0 < k < len(r) - 1:
                when = (sc.T0 + timedelta(seconds=float(t[k]))).strftime("%m-%d %H:%M")
                ax.annotate(f"perihelion {when} UTC\n{r[k] * 1e6 / SUN.a_km:.2f} solar radii",
                            xyz[k, [a, b]], textcoords="offset points", xytext=(-8, 8),
                            ha="right", fontsize=8, color=INK_MUTED)
            ax.set_xlim(-lim, lim)
            ax.set_ylim(-lim, lim)
            ax.set_aspect("equal")
            ax.set_xlabel("ICRF x (million km)", fontsize=8)
            ax.set_ylabel(f"ICRF {label} (million km)", fontsize=8)
            style(ax)
        row[0].set_title(probe.name, fontsize=10, loc="left")
    fig.suptitle(f"Probes over {DAYS} days, Sun-centred with ICRF axes "
                 "(left: x–y, right: x–z; yellow disc = the Sun, to scale)", fontsize=11)
    path = out_dir / "heliocentric.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_cislunar(client, rows, ids, t_s, craft, out_dir: Path) -> Path:
    """A moonshot resolved to GCRF through the ledger, whatever frame each row is stored in.
    Left: Earth-centred, the whole flight with the Moon's path. Right: Moon-centred (GCRF axes),
    zoomed on the capture, the orbit and the descent."""
    eph, ev = Ephemeris(client), craft.events
    mask = matches(ids, craft.id) & (t_s >= ev["liftoff"]) & (t_s <= ev["touchdown"])
    order = np.argsort(t_s[mask])
    t = t_s[mask][order]
    xyz = positions(client.exchange(rows.filter(pa.array(mask)), GCRF.frame))[order]
    grid = np.arange(0, sc.DURATION_S + 1, 1800.0)
    moon_path = eph.centre(MOON, grid, GCRF)
    near = t >= ev["soi"]
    rel = xyz[near] - eph.centre(MOON, t[near], GCRF)
    at = lambda when: int(np.searchsorted(t, when))
    marks = (("TLI", ev["tli"]), ("sphere of influence", ev["soi"]), ("touchdown", ev["touchdown"]))

    fig, (wide, zoom) = plt.subplots(1, 2, figsize=(13, 6.2), layout="constrained")
    wide.add_patch(plt.Circle((0, 0), EARTH.a_km / 1e3, color=BODY_FILL, zorder=0))
    wide.plot(moon_path[:, 0] / 1e3, moon_path[:, 1] / 1e3, color=INK_MUTED, linewidth=0.8,
              linestyle="--", label=f"Moon, {DAYS} days")
    wide.plot(xyz[:, 0] / 1e3, xyz[:, 1] / 1e3, color=TRACK, linewidth=1.2, label=craft.name)
    for (name, when), (offset, side) in zip(marks, (((8, -12), "left"), ((10, -10), "left"), ((-8, 8), "right"))):
        p = xyz[min(at(when), len(t) - 1), :2] / 1e3
        wide.plot(*p, "o", color=MARK, markersize=5)
        wide.annotate(f"{name}\n{(sc.T0 + timedelta(seconds=float(when))):%m-%d %H:%M}", p,
                      textcoords="offset points", xytext=offset, ha=side, fontsize=8, color=INK_MUTED)
    lim = 1.1 * float(np.abs(moon_path[:, :2]).max()) / 1e3
    wide.set_xlim(-lim, lim)
    wide.set_ylim(-lim, lim)
    wide.set_title("Earth-centred (grey disc = Earth, to scale)", fontsize=10, loc="left")
    wide.legend(fontsize=8, frameon=False, loc="lower left")

    # Seen face-on: the two directions that span the rows near the Moon (the orbit plane).
    lim = 3.2 * MOON.a_km
    close = rel[np.linalg.norm(rel, axis=1) < lim]
    in_plane = np.linalg.svd(close, full_matrices=False)[2][:2]
    flat = rel @ in_plane.T / 1e3
    zoom.add_patch(plt.Circle((0, 0), MOON.a_km / 1e3, color=BODY_FILL, zorder=0))
    zoom.plot(flat[:, 0], flat[:, 1], color=TRACK, linewidth=0.9)
    zoom.set_xlim(-lim / 1e3, lim / 1e3)
    zoom.set_ylim(-lim / 1e3, lim / 1e3)
    zoom.set_title("Moon-centred, in the orbit plane: approach, capture, orbit, descent",
                   fontsize=10, loc="left")
    wide.set_xlabel("GCRF x (thousand km)", fontsize=8)
    wide.set_ylabel("GCRF y (thousand km)", fontsize=8)
    zoom.set_xlabel("in-plane axis 1 (thousand km; grey disc = Moon)", fontsize=8)
    zoom.set_ylabel("in-plane axis 2 (thousand km)", fontsize=8)
    for ax in (wide, zoom):
        ax.set_aspect("equal")
        style(ax)
    fig.suptitle(f"{craft.name}: every stored row from liftoff to touchdown, resolved to GCRF",
                 fontsize=11)
    path = out_dir / f"cislunar_{craft.name.lower()}.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_transfer(client, rows, ids, t_s, craft, out_dir: Path) -> Path:
    """An interplanetary cruise, Sun-centred with ICRF axes: the planets' paths from departure
    to arrival (from the kernels), the whole arc (from the model), and the stored rows on it."""
    eph, spec, conic = Ephemeris(client), craft.spec, craft.phases[0].conic
    t_dep, t_arr = craft.events["departure"], craft.events["arrival"]
    days = np.arange(t_dep, t_arr + 1, 86400.0)
    sun = eph.centre(SUN, days)
    paths = [(b.name, (eph.centre(b, days) - sun) / 1e6) for b in (spec.origin, spec.target)]
    arc = np.array([conic.state(t)[0] for t in days]) / 1e6
    mask = matches(ids, craft.id)
    order = np.argsort(t_s[mask])[::120]                                   # hourly is plenty
    t = t_s[mask][order]
    stored = (positions(rows.filter(pa.array(mask)))[order] - eph.centre(SUN, t)) / 1e6

    fig, axes = plt.subplots(1, 2, figsize=(12, 6), layout="constrained")
    lim = 1.08 * max(float(np.abs(p).max()) for _, p in paths)
    for ax, (a, b), label in zip(axes, ((0, 1), (0, 2)), ("y", "z")):
        ax.plot(0, 0, "o", color=SERIES[3], markersize=7)
        for slot, (name, p) in zip((2, 1), paths):
            ax.plot(p[:, a], p[:, b], color=SERIES[slot], linewidth=1.0, label=f"{name}, same dates")
            ax.plot(*p[0, [a, b]], "o", color=SERIES[slot], markersize=4)
            ax.plot(*p[-1, [a, b]], "o", color=SERIES[slot], markersize=4)
        ax.plot(arc[:, a], arc[:, b], color=INK_MUTED, linewidth=0.8, linestyle="--",
                label="transfer arc (model)")
        ax.plot(stored[:, a], stored[:, b], color=TRACK, linewidth=3, solid_capstyle="round",
                label=f"stored rows, {DAYS} days")
        ax.set_xlim(-lim, lim)
        ax.set_ylim(-lim, lim)
        ax.set_aspect("equal")
        ax.set_xlabel("ICRF x (million km)", fontsize=8)
        ax.set_ylabel(f"ICRF {label} (million km)", fontsize=8)
        style(ax)
    for name, p, when, offset in ((f"departs {spec.origin.name}", paths[0][1][0], spec.depart, (10, -22)),
                                  (f"reaches {spec.target.name}", paths[1][1][-1], spec.arrive, (8, 8))):
        axes[0].annotate(f"{name}\n{when:%Y-%m-%d}", p[[0, 1]], textcoords="offset points",
                         xytext=offset, fontsize=8, color=INK_MUTED)
    axes[0].legend(fontsize=8, frameon=False, loc="lower left")
    fig.suptitle(f"{craft.name}: Sun-centred with ICRF axes (left: x–y, right: x–z; "
                 "yellow dot = the Sun)", fontsize=11)
    path = out_dir / f"transfer_{craft.name.lower()}.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_altitudes(client, rows, ids, t_s, spacecraft, out_dir: Path) -> Path | None:
    """Altitude against time around each launch and landing, resolved to the body frame through
    the ledger (pad and landed rows resolve through their facility)."""
    shows = []
    for c in spacecraft:
        if "liftoff" in c.events:
            shows.append((c, c.launch_spot, c.events["liftoff"], "liftoff",
                          [("insertion", c.events["insertion"])]))
        if "touchdown" in c.events:
            shows.append((c, c.landing_spot, c.events["deorbit"], "deorbit",
                          [("perilune", c.events["perilune"]), ("touchdown", c.events["touchdown"])]))
    shows = [s for s in shows if s[0].id in set(ids)]
    if not shows:
        return None
    fig, axes = plt.subplots(1, len(shows), figsize=(5 * len(shows), 4), layout="constrained")
    for ax, (craft, place, t_ref, ref_name, marks) in zip(np.atleast_1d(axes), shows):
        t_end = marks[-1][1]
        mask = matches(ids, craft.id) & (t_s >= t_ref - 600) & (t_s <= t_end + 1200)
        window = rows.filter(pa.array(mask))
        fixed = positions(client.exchange(window, place.body.frame))
        _, _, h = fixed_to_geodetic(place.body, fixed)
        minutes = (t_s[mask] - t_ref) / 60
        order = np.argsort(minutes)
        ax.plot(minutes[order], h[order], color=TRACK, linewidth=1.2)
        ax.set_ylim(top=1.3 * float(h.max()))   # headroom for the event labels
        # Event labels step down the axis so close events (perilune, touchdown) don't collide.
        for j, (name, t) in enumerate([(ref_name, t_ref), *marks]):
            m = (t - t_ref) / 60
            ax.axvline(m, color=INK_MUTED, linestyle="--", linewidth=0.8)
            ax.text(m, 0.97 - 0.07 * j, f" {name}", fontsize=8, color=INK_MUTED, va="top",
                    transform=ax.get_xaxis_transform())
        ax.set_title(f"{craft.name} ({place.facility.name})", fontsize=10)
        ax.set_xlabel(f"minutes from {ref_name}", fontsize=8)
        ax.set_ylabel(f"altitude above {place.body.name} (km)", fontsize=8)
        style(ax)
    fig.suptitle("Launches and landings: altitude resolved to the body frame", fontsize=11)
    path = out_dir / "altitudes.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def _geodetic(rows: pa.Table) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    return fixed_to_geodetic(EARTH, positions(rows))


def _unwrapped(lon: np.ndarray, lat: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Breaks a track where it crosses the antimeridian, so it isn't drawn across the map."""
    jump = np.flatnonzero(np.abs(np.diff(lon)) > 180) + 1
    return np.insert(lon, jump, np.nan), np.insert(lat, jump, np.nan)


def plot_tracks(rows, ids, t_s, fleet, title: str, out_dir: Path, filename: str,
                routes=(), canals: bool = False) -> Path:
    """Stored tracks over the window on a lon/lat map with the land polygons: one hue for every
    track (there are more vehicles than categorical slots), a dot and name at each T0 position.
    `routes` (GreatCircles) are drawn faintly underneath, and `canals` outlines the canal boxes."""
    fig, ax = plt.subplots(figsize=(16, 6.8), layout="constrained")
    for ring in outlines():
        ax.fill(ring[:, 0], ring[:, 1], color=BODY_FILL, linewidth=0)
    for route in routes:
        lat, lon = np.array([route.at(s) for s in np.arange(0, route.length_km, 20.0)]).T
        ax.plot(*_unwrapped(lon, lat), color=INK_MUTED, linewidth=0.6, linestyle=":")
    for la0, la1, lo0, lo1 in CANALS.values() if canals else ():
        ax.add_patch(plt.Rectangle((lo0, la0), lo1 - lo0, la1 - la0, fill=False,
                                   edgecolor=INK_MUTED, linewidth=0.6))
    for v in fleet:
        mask = matches(ids, v.id)
        lat, lon, _ = _geodetic(rows.filter(pa.array(mask)))
        order = np.argsort(t_s[mask])
        lat, lon = lat[order], lon[order]
        ax.plot(*_unwrapped(lon, lat), color=TRACK, linewidth=0.9)
        ax.plot(lon[0], lat[0], "o", color=MARK, markersize=4)
        ax.annotate(v.name, (lon[0], lat[0]), xytext=(4, 3), textcoords="offset points",
                    fontsize=7, color="#3d3c37")
    ax.set_xlim(-180, 180)
    ax.set_ylim(-60, 75)
    ax.set_aspect("equal")
    ax.set_xlabel("longitude (°)", fontsize=8)
    ax.set_ylabel("latitude (°)", fontsize=8)
    style(ax)
    ax.set_title(title, fontsize=11)
    path = out_dir / filename
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_flight_altitudes(rows, ids, t_s, fleet, out_dir: Path) -> Path:
    """Stored altitude over the window, one panel per aircraft."""
    cols = 5
    fig, axes = plt.subplots(2, cols, figsize=(3.2 * cols, 5.6), sharex=True, sharey=True,
                             layout="constrained")
    for ax, v in zip(axes.flat, fleet):
        mask = matches(ids, v.id)
        _, _, h = _geodetic(rows.filter(pa.array(mask)))
        order = np.argsort(t_s[mask])
        ax.plot(t_s[mask][order] / 3600, h[order], color=TRACK, linewidth=1.0)
        ax.set_title(v.name, fontsize=9)
        style(ax)
    for ax in axes[-1]:
        ax.set_xlabel("hours from T0", fontsize=8)
    for ax in axes[:, 0]:
        ax.set_ylabel("altitude (km)", fontsize=8)
    fig.suptitle(f"Aircraft altitude over {DAYS} days (flat at field elevation = parked)", fontsize=11)
    path = out_dir / "flight_altitudes.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def regatta_rows(g, table: pa.Table):
    # Imported here: snapshot_sim imports this module's style.
    from tools.snapshot_sim import Rows
    return Rows(table, g.names, g.venue.id, tai_ns_from_utc(sc.T0))


def plot_regatta(g, table: pa.Table, out_dir: Path) -> Path:
    """The race in venue ENU, as stored: each boat from the warning to its finish over the
    course, and the whole race day (docks, motor route) over the venue. The fleet sails the
    default tactician; SAIL-10 sails whatever `--regatta-policy` named."""
    rows, c = regatta_rows(g, table), g.course
    rings = [np.array([g.venue.enu(a, o)[:2] for a, o in ring]) for ring in sc.BASIN_WATER]
    fig, (near, whole) = plt.subplots(1, 2, figsize=(15, 7.2), layout="constrained",
                                      gridspec_kw={"width_ratios": [1.6, 1]})
    finish = {}
    for ax in (near, whole):
        for ring in rings:
            ax.fill(ring[:, 0], ring[:, 1], color=WATER, linewidth=0)
        for b in g.boats:
            k = rows.index[b.name]
            if ax is near:
                r, end = rg.replay(c, rows.t_s[k], rows.pos[k])
                finish[b.name] = r.finish_s
                k = k[(rows.t_s[k] >= rg.WARNING_S) & (rows.t_s[k] < end)]
            else:
                k = k[(rows.t_s[k] >= rg.ON_S) & (rows.t_s[k] <= rg.OFF_S)]
            mine = b is g.boats[-1]
            ax.plot(rows.pos[k, 0], rows.pos[k, 1], color=MARK if mine else TRACK,
                    linewidth=1.4 if mine else 0.7, alpha=1.0 if mine else 0.55, zorder=3 if mine else 2,
                    label=None if b.name not in ("SAIL-01", g.boats[-1].name) else
                    f"{b.name} (--regatta-policy)" if mine else "fleet (default tactician)")
        ax.plot(*np.column_stack([c.rc, c.marks["MARK-PIN"]]), "--", color=INK_MUTED, linewidth=0.9)
        marks = np.array(list(c.marks.values()))
        ax.plot(marks[:, 0], marks[:, 1], "o", color="#3d3c37", markersize=5, zorder=4)
        ax.plot(*c.rc, "s", color="#3d3c37", markersize=6, zorder=4)
        ax.set_aspect("equal")
        ax.set_xlabel("east (m)", fontsize=8)
        ax.set_ylabel("north (m)", fontsize=8)
        style(ax)
    for name, xy in (("W", c.marks["MARK-W"]), ("gate", c.marks["MARK-GATE-2"]), ("pin", c.marks["MARK-PIN"]),
                     ("RC", c.rc)):
        near.annotate(name, xy, xytext=(6, -10), textcoords="offset points", fontsize=8, color="#3d3c37")
    done = sorted((t, n) for n, t in finish.items() if t is not None)
    if done:
        t, n = done[0]
        k = rows.upto(n, t + sc.REGATTA_CADENCE_S)[-1]
        near.annotate(f"{n} first, {timedelta(seconds=round(t - rg.GUN_S))}", rows.pos[k], xytext=(8, 12),
                      textcoords="offset points", fontsize=8, color="#3d3c37")
    pts = np.array([c.rc, *c.marks.values(), *c.staging])
    lo, hi = pts.min(0) - 700, pts.max(0) + 700
    near.set_xlim(lo[0], hi[0])
    near.set_ylim(lo[1], hi[1])
    near.legend(fontsize=8, loc="lower right")
    near.set_title("from the warning signal to each boat's finish (■ RC boat, ● marks, dashed = line)", fontsize=10)
    e0, e1, n0, n1 = sc.REGATTA_WIND.extent_m
    whole.set_xlim(e0, e1)
    whole.set_ylim(n0, n1)
    whole.set_title("race day 13:00–18:00 ADT, docks to course", fontsize=10)
    fig.suptitle(f"{g.venue.name} regatta, {sc.REGATTA_GUN:%Y-%m-%d}: stored rows in venue ENU", fontsize=11)
    path = out_dir / "regatta.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_regatta_speed(g, table: pa.Table, out_dir: Path) -> Path:
    """Each boat's stored speed while sailing against the polar bound for the wind at its row,
    one panel per boat; dips below the bound are tacks, gybes and luffs."""
    rows, c = regatta_rows(g, table), g.course
    fig, axes = plt.subplots(2, 5, figsize=(16, 5.8), sharex=True, sharey=True, layout="constrained")
    for ax, b in zip(axes.flat, g.boats):
        k = rows.index[b.name]
        r, end = rg.replay(c, rows.t_s[k], rows.pos[k])
        k = k[(rows.t_s[k] >= rg.WARNING_S) & (rows.t_s[k] < end)]
        twd = np.empty(len(k))
        tws = np.empty(len(k))
        for j, i in enumerate(k):
            twd[j], tws[j] = g.wind.at(rows.pos[i, 0], rows.pos[i, 1], float(rows.t_s[i]))
        minutes = (rows.t_s[k] - rg.GUN_S) / 60
        for _, t in r.roundings:
            ax.axvline((t - rg.GUN_S) / 60, color=GRID, linewidth=0.8)
        ax.plot(minutes, rows.speed[k], color=MARK if b is g.boats[-1] else TRACK, linewidth=1.0,
                label="stored speed")
        ax.plot(minutes, polar(rows.heading[k] - twd, tws), color="#3d3c37", linewidth=0.8,
                linestyle="--", label="polar bound")
        note = f"finished {timedelta(seconds=round(r.finish_s - rg.GUN_S))}" if r.finish_s else "DNF"
        ax.set_title(f"{b.name}, {note}", fontsize=9)
        style(ax)
    for ax in axes[-1]:
        ax.set_xlabel("minutes from the gun", fontsize=8)
    for ax in axes[:, 0]:
        ax.set_ylabel("speed (m/s)", fontsize=8)
    axes.flat[0].legend(fontsize=7, loc="lower right")
    fig.suptitle("Regatta boat speed while sailing vs the polar for the wind at each row (drops = tack, "
                 "gybe or luff; grey verticals = roundings; SAIL-10 in orange sails --regatta-policy)", fontsize=11)
    path = out_dir / "regatta_speed.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def fire_rows(w, table: pa.Table):
    # Imported here: snapshot_sim imports this module's style.
    from tools.snapshot_sim import FireView, Rows
    rows = Rows(table, w.names, w.venue.id, tai_ns_from_utc(sc.T0))
    return rows, FireView(w, rows)


def plot_wildfire(w, table: pa.Table, fuel_path: Path, out_dir: Path) -> Path:
    """The fire's growth as stored: the perimeter every 2 h from ignition (darker = later) over
    the fuel map, the finished line coloured by when each piece was finished, the crews' tracks."""
    from tools.snapshot_sim import FIRE_RED, draw_fuel, fuel_grid
    rows, fire = fire_rows(w, table)
    end = min(fire.end, sc.DURATION_S)
    fig, ax = plt.subplots(figsize=(11, 9), layout="constrained")
    draw_fuel(ax, fuel_grid(fuel_path, w.venue))
    hours = np.arange(2, (end - wf.IGNITION_S) / 3600 + 1e-9, 2.0)
    reds = matplotlib.cm.ScalarMappable(matplotlib.colors.Normalize(-hours[-1] * 0.3, hours[-1]),
                                        matplotlib.colors.LinearSegmentedColormap.from_list("fire", ["#ffffff", FIRE_RED, "#7a1515"]))
    for h in hours:
        ring = fire.perimeter(wf.IGNITION_S + h * 3600)
        ax.plot(*np.vstack([ring, ring[:1]]).T, color=reds.to_rgba(h), linewidth=1.0)
    fig.colorbar(reds, ax=ax, shrink=0.6, label="perimeter (hours after ignition)",
                 boundaries=np.linspace(0, hours[-1], 50), ticks=hours[1::2])
    for c in w.crews:
        k = rows.index.get(c.name, np.array([], int))
        k = k[(rows.t_s[k] >= wf.IGNITION_S) & (rows.t_s[k] <= end)]
        ax.plot(rows.pos[k, 0], rows.pos[k, 1], color=INK_MUTED, linewidth=0.5, alpha=0.7)
    if fire.lines:
        born = np.array([b for b, _, _ in fire.lines])
        segs = [np.array([a, b]) for _, a, b in fire.lines]
        lc = matplotlib.collections.LineCollection(segs, cmap="Blues", linewidths=2.5,
                                                   norm=matplotlib.colors.Normalize((born.min() - wf.IGNITION_S) / 3600 - 1,
                                                                                    (born.max() - wf.IGNITION_S) / 3600))
        lc.set_array((born - wf.IGNITION_S) / 3600)
        ax.add_collection(lc)
        fig.colorbar(lc, ax=ax, shrink=0.6, label="line finished (hours after ignition)")
    ax.plot(*sc.ICP_M, "^", color="#3d3c37", markersize=8)
    ax.annotate("ICP", sc.ICP_M, xytext=(6, -3), textcoords="offset points", fontsize=8)
    ring = fire.perimeter(end)
    pts = np.vstack([ring, *[np.array([a, b]) for _, a, b in fire.lines], [sc.ICP_M]])
    lo, hi = pts.min(0) - 200, pts.max(0) + 200
    ax.set_xlim(lo[0], hi[0])
    ax.set_ylim(lo[1], hi[1])
    ax.set_aspect("equal")
    ax.set_xlabel("east (m)", fontsize=8)
    ax.set_ylabel("north (m)", fontsize=8)
    style(ax)
    state = (f"contained {timedelta(seconds=round(fire.end - wf.IGNITION_S))} after ignition"
             if math.isfinite(fire.end) else "not contained")
    ax.set_title(f"{w.venue.name}: {state}, {fire.area_ha(end):.1f} ha\nperimeter every 2 h (red), finished "
                 "line (blue), crew tracks (grey); fuel in grey by R0 factor (darker burns faster), river in light blue",
                 fontsize=10)
    path = out_dir / "wildfire.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_wildfire_crews(w, table: pa.Table, out_dir: Path) -> Path:
    """Each crew's distance to the nearest spreading vertex while any spreads, one panel each,
    against the rule's 30 m and the 40 m at which a crew walks away."""
    rows, fire = fire_rows(w, table)
    end = min(fire.end, sc.DURATION_S)
    ks = {c.name: rows.index[c.name] for c in w.crews}
    ticks = np.unique(np.concatenate([rows.t_s[k] for k in ks.values()]))
    ticks = ticks[(ticks >= wf.IGNITION_S) & (ticks < end)]
    dist = {n: [] for n in ks}
    for t in ticks:
        moving = fire.moving(t)
        for n, k in ks.items():
            j = k[np.searchsorted(rows.t_s[k], t, side="right") - 1]
            dist[n].append(np.linalg.norm(moving - rows.pos[j], axis=1).min() if len(moving) else np.nan)
    cols = 4
    fig, axes = plt.subplots(len(ks) // cols, cols, figsize=(16, 2.6 * len(ks) // cols), sharex=True,
                             sharey=True, layout="constrained")
    for ax, (n, d) in zip(axes.flat, dist.items()):
        h = (ticks - wf.IGNITION_S) / 3600
        ax.plot(h, d, color=TRACK, linewidth=0.9)
        ax.axhline(sc.SAFE_M, color=MARK, linewidth=0.8, linestyle="--")
        ax.axhline(sc.ESCAPE_M, color=INK_MUTED, linewidth=0.8, linestyle=":")
        ax.set_yscale("log")
        ax.set_title(f"{n}, closest {np.nanmin(d):.0f} m", fontsize=9)
        style(ax)
    for ax in axes[-1]:
        ax.set_xlabel("hours after ignition", fontsize=8)
    for ax in axes[:, 0]:
        ax.set_ylabel("to nearest front (m)", fontsize=8)
    fig.suptitle(f"Wildfire crews: distance to the nearest spreading vertex (orange dashed = the {sc.SAFE_M:.0f} m rule, "
                 f"grey dotted = {sc.ESCAPE_M:.0f} m, where a crew walks away)", fontsize=11)
    path = out_dir / "wildfire_crews.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def cest_hours(t_s):
    """Seconds since T0 as CEST hours on 2026-09-02."""
    from tools.snapshot_sim import CEST
    return (np.asarray(t_s) + CEST.total_seconds()) / 3600 - 24


def plot_factory_boxes(fac, table: pa.Table, out_dir: Path) -> Path:
    """Per line, boxes spawned and delivered over the shift (top), and the belt speed from the
    shaft's stored angular_velocity × the pulley radius (bottom)."""
    from tools.snapshot_sim import factory_rows
    rows = factory_rows(fac, table)
    fig, (count, speed) = plt.subplots(2, 1, figsize=(13, 6.5), sharex=True, layout="constrained",
                                       gridspec_kw={"height_ratios": [2, 1]})
    for k, line in enumerate(fac.lines):
        colour = SERIES[k]
        born = np.sort([b.spawn_s for b in line["boxes"]])
        done = np.sort([rows.t_s[rows.index[b.name][-1]] for b in line["boxes"]
                        if rows.frames[rows.index[b.name][-1]] == b.c_id])
        count.step(cest_hours(born), np.arange(1, len(born) + 1), where="post", color=colour, linewidth=1.0,
                   label=f"{line['name']} spawned on A")
        count.step(cest_hours(done), np.arange(1, len(done) + 1), where="post", color=colour, linewidth=1.0,
                   linestyle="--", label=f"{line['name']} delivered to C")
        k_shaft = rows.index[line["shaft"].name]
        t = rows.t_s[k_shaft]
        shift = (t >= fm.SHIFT_S[0] - 600) & (t <= fm.SHIFT_S[1] + 600)
        speed.plot(cest_hours(t[shift]), rows.spin[k_shaft][shift] * sc.PULLEY_R_M, color=colour, linewidth=1.0,
                   label=line["name"])
    count.set_ylabel("boxes", fontsize=8)
    count.legend(fontsize=7, ncols=3)
    count.set_title(f"{fac.plant.name}: boxes over the shift (solid = spawned, dashed = delivered; they "
                    "overlap at this scale, a box spends 16 s on the belt)", fontsize=10)
    speed.set_ylabel("belt speed (m/s)", fontsize=8)
    speed.set_xlabel("hours, CEST, 2026-09-02", fontsize=8)
    speed.set_title("belt speed = shaft angular_velocity × pulley radius: start, break 10:00–10:30, stop", fontsize=10)
    for ax in (count, speed):
        style(ax)
    path = out_dir / "factory_boxes.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_factory_bearing(fac, client: SolocClient, table: pa.Table, out_dir: Path) -> Path:
    """The burst on line 1: the shaft's, a cage's and a ball's stored angles against the analytic
    ones over its first 2 s (left), and bearing 1's balls resolved through the ledger to
    IAU_EARTH at one burst instant, seen along the shaft (right)."""
    from tools.snapshot_sim import factory_rows
    rows = factory_rows(fac, table)
    line = fac.lines[0]
    bear = line["bearings"][0]
    fig, (turns, cut) = plt.subplots(1, 2, figsize=(15, 6), layout="constrained",
                                     gridspec_kw={"width_ratios": [1.6, 1]})
    t0 = fm.BURST_S[0]
    fine = np.linspace(t0, t0 + 2, 801)
    for colour, part, label in ((SERIES[0], line["shaft"], "shaft"), (SERIES[1], bear["cage"], "cage 1"),
                                (SERIES[2], bear["balls"][0], "ball 1 (spin in the cage)")):
        k = rows.index[part.name]
        k = k[(rows.t_s[k] >= t0) & (rows.t_s[k] <= t0 + 2)]
        stored = np.unwrap(rows.turn[k])
        want = part.spin * (fm.angle(rows.t_s[k]) - fm.angle(t0))
        stored = stored - stored[0] + want[0]
        turns.plot(fine - t0, part.spin * (fm.angle(fine) - fm.angle(t0)) / (2 * np.pi), color=colour,
                   linewidth=0.8, label=f"{label}: spin × shaft angle")
        turns.plot(rows.t_s[k] - t0, stored / (2 * np.pi), "o", color=colour, markersize=3,
                   label=f"{label}: stored, unwrapped at {sc.BURST_HZ} Hz")
    turns.set_xlabel(f"seconds from the burst's start ({sc.BURST[0]:%H:%M} UTC)", fontsize=8)
    turns.set_ylabel("turns", fontsize=8)
    turns.legend(fontsize=7)
    turns.set_title("burst rows against the rolling-bearing kinematics (0° contact, pure rolling)", fontsize=10)
    style(turns)

    when = t0 + 7 * fm.SUB_NS / 1e9
    pick = [int(rows.index[ball.name][np.argmin(np.abs(rows.t_s[rows.index[ball.name]] - when))])
            for ball in bear["balls"]]
    rings = [int(rows.index[b["outer"].name][0]) for b in line["bearings"]]
    r = positions(client.exchange(rows.table.take(pa.array(rings)), EARTH.frame)) * 1000
    balls = positions(client.exchange(rows.table.take(pa.array(pick)), EARTH.frame)) * 1000
    axis = (r[1] - r[0]) / np.linalg.norm(r[1] - r[0])
    centre = r[0] if np.linalg.norm(balls[0] - r[0]) < np.linalg.norm(balls[0] - r[1]) else r[1]
    e1 = np.cross(axis, [0.0, 0.0, 1.0])
    e1 /= np.linalg.norm(e1)
    e2 = np.cross(axis, e1)
    xy = np.column_stack([(balls - centre) @ e1, (balls - centre) @ e2]) * 1000      # mm
    for rad in (sc.RING_DIMENSIONS_M["outer"][1] / 2, (sc.PITCH_D_M + sc.BALL_D_M) / 2,
                (sc.PITCH_D_M - sc.BALL_D_M) / 2):
        cut.add_patch(plt.Circle((0, 0), rad * 1000, fill=False, color=INK_MUTED, linewidth=0.8))
    cut.add_patch(plt.Circle((0, 0), sc.PITCH_D_M / 2 * 1000, fill=False, linestyle=":", color=INK_MUTED))
    for p in xy:
        cut.add_patch(plt.Circle(p, sc.BALL_D_M / 2 * 1000, color=SERIES[2], alpha=0.8))
    radial = np.linalg.norm(xy, axis=1)
    cut.set_xlim(-30, 30)
    cut.set_ylim(-30, 30)
    cut.set_aspect("equal")
    cut.set_xlabel("mm", fontsize=8)
    style(cut)
    cut.set_title(f"{line['name']} bearing 1, its 8 balls resolved through 7 frames to {EARTH.frame} at "
                  f"+{7 * fm.SUB_NS / 1e9:.2f} s\nball centres {radial.min():.4f}–{radial.max():.4f} mm "
                  f"from the axis (pitch radius {sc.PITCH_D_M / 2 * 1000:.4f})", fontsize=9)
    path = out_dir / "factory_bearing.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_factory_bearing_analysis(fac, table: pa.Table, out_dir: Path) -> Path:
    """Line 1 bearing 1 from its stored rows: speeds and their ratios to the shaft, the defect
    frequencies and cumulative cycles over the shift (top four), and the burst's orientation with
    the speed differenced from it against the stored angular_velocity (bottom)."""
    from tools.snapshot_sim import factory_rows
    rows = factory_rows(fac, table)
    line = fac.lines[0]
    bear = line["bearings"][0]
    k = {"shaft": rows.index[line["shaft"].name], "cage": rows.index[bear["cage"].name],
         "ball": rows.index[bear["balls"][0].name]}
    t = rows.t_s[k["shaft"]]
    assert all(np.array_equal(rows.t_s[v], t) for v in k.values())
    w = {key: rows.spin[v] for key, v in k.items()}                 # rad/s about x
    window = (t >= fm.SHIFT_S[0] - 600) & (t <= fm.SHIFT_S[1] + 600)
    turning = window & (w["shaft"] > 0.05 * fm.OMEGA)
    h = cest_hours(t[window])
    labels = {"shaft": "shaft", "cage": "cage 1", "ball": "ball 1, spin in the cage"}
    colours = {"shaft": SERIES[0], "cage": SERIES[1], "ball": SERIES[2]}
    fig, ((speeds, ratios), (freqs, cycles), (orient, rate)) = plt.subplots(3, 2, figsize=(15, 12),
                                                                            layout="constrained")
    rpm = 60 / (2 * np.pi)

    for key in k:
        speeds.plot(h, w[key][window] * rpm, color=colours[key], linewidth=1.0, label=labels[key])
    speeds.set_ylabel("rpm", fontsize=8)
    speeds.set_title("stored angular_velocity about each part's x (ball: relative to the cage)", fontsize=10)

    spread = []
    for key, want in (("cage", fm.CAGE_RATIO), ("ball", fm.BALL_RATIO)):
        ratio = w[key][turning] / w["shaft"][turning]
        spread.append(np.max(np.abs(ratio - want)))
        ratios.plot(cest_hours(t[turning]), ratio, ".", color=colours[key], markersize=2,
                    label=f"{key} / shaft, stored")
        ratios.axhline(want, color=colours[key], linestyle="--", linewidth=0.8,
                       label=f"{key}: pure rolling {want:.4f}")
    ratios.set_ylabel("speed ratio", fontsize=8)
    ratios.set_title(f"ratios to the shaft while it turns (> 5% of {sc.SHAFT_HZ * 60:.0f} rpm): slip would "
                     f"pull the cage off its line\nmax |stored − pure rolling| {max(spread):.1e}", fontsize=10)

    f_s, ftf = w["shaft"] / (2 * np.pi), w["cage"] / (2 * np.pi)
    bands = (("FTF (cage)", ftf, SERIES[1]), ("BSF", np.abs(w["ball"]) / (2 * np.pi), SERIES[2]),
             ("BPFO", sc.BALLS * ftf, SERIES[3]), ("BPFI", sc.BALLS * (f_s - ftf), SERIES[4]))
    freqs.plot(h, f_s[window], color=SERIES[0], linewidth=1.0, label="shaft (1×)")
    for label, f, colour in bands:
        order = np.median(f[turning] / f_s[turning])
        freqs.plot(h, f[window], color=colour, linewidth=1.0, label=f"{label} = {order:.3f}× shaft")
    freqs.set_ylim(-0.25, 6.5)
    freqs.set_ylabel("Hz", fontsize=8)
    freqs.set_title(f"defect frequencies from the stored speeds ({sc.BALLS} balls, "
                    f"{sc.CONTACT_DEG:.0f}° contact)", fontsize=10)

    cum = lambda y: np.concatenate([[0.0], np.cumsum(np.diff(t) * (y[1:] + y[:-1]) / 2)]) / (2 * np.pi)
    turns = fm.angle(t) / (2 * np.pi)
    err = []
    for label, got, want, colour in (
            ("shaft revolutions", cum(w["shaft"]), turns, SERIES[0]),
            ("outer-race ball passes", sc.BALLS * cum(w["cage"]), sc.BALLS * fm.CAGE_RATIO * turns, SERIES[3]),
            ("inner-race ball passes", sc.BALLS * cum(w["shaft"] - w["cage"]),
             sc.BALLS * (1 - fm.CAGE_RATIO) * turns, SERIES[4])):
        err.append(np.max(np.abs(got - want)))
        cycles.plot(h, got[window] / 1000, color=colour, linewidth=1.2, label=f"{label}: {got[-1]:,.0f}")
        cycles.plot(h, want[window] / 1000, color=INK_MUTED, linewidth=0.6, linestyle="--")
    cycles.set_ylabel("thousands", fontsize=8)
    cycles.set_title(f"cumulative cycles, stored speed integrated (trapezoid) against the analytic angle "
                     f"(grey dashed)\nmax |difference| {max(err):.1e} cycles", fontsize=10)

    for ax in (speeds, ratios, freqs, cycles):
        ax.set_xlim(h[0], h[-1])
        ax.set_xlabel("hours, CEST, 2026-09-02", fontsize=8)
        ax.legend(fontsize=7, ncols=3 if ax is freqs else 1, loc="upper left" if ax is freqs else "best")
        style(ax)

    t0 = fm.BURST_S[0]
    burst = (t >= t0) & (t < fm.BURST_S[1])
    shown = (t[burst] - t0) <= 3
    worst = 0.0
    for key in k:
        kb = k[key][burst]
        orient.plot(t[burst][shown] - t0, np.degrees(rows.turn[kb][shown]) % 360, "o", color=colours[key],
                    markersize=2.5, label=labels[key])
        derived = np.diff(np.unwrap(rows.turn[kb])) / np.diff(t[burst])
        worst = max(worst, np.max(np.abs(derived - w[key][burst][1:])) * rpm)
        mid = (t[burst][1:] + t[burst][:-1]) / 2 - t0
        rate.plot(mid[shown[1:]], derived[shown[1:]] * rpm, "o", color=colours[key], markersize=2.5,
                  label=f"{labels[key]}: from successive quaternions")
        rate.plot(t[burst][shown] - t0, w[key][burst][shown] * rpm, color=colours[key], linewidth=0.8,
                  label=f"{labels[key]}: stored angular_velocity")
    orient.set_ylabel("angle about x (deg)", fontsize=8)
    orient.set_title(f"orientation from the stored quaternion at {sc.BURST_HZ} Hz (at the {sc.PART_CADENCE_S} s "
                     f"shift cadence a {sc.SHAFT_HZ:.0f} Hz shaft aliases to a constant)", fontsize=10)
    rate.set_ylabel("rpm", fontsize=8)
    rate.set_title(f"speed differenced from orientation against the stored rate\nmax |difference| over the "
                   f"10 min burst {worst:.1e} rpm", fontsize=10)
    for ax in (orient, rate):
        ax.set_xlabel(f"seconds from the burst's start ({sc.BURST[0]:%H:%M} UTC)", fontsize=8)
        ax.legend(fontsize=7)
        style(ax)
    fig.suptitle(f"{line['name']} bearing 1 (6205-size deep-groove ball bearing): kinematics from the stored rows",
                 fontsize=11)
    path = out_dir / "factory_bearing_analysis.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def main():
    p = argparse.ArgumentParser()
    p.add_argument("path")
    p.add_argument("--server", default="grpc://localhost:50051")
    p.add_argument("--seed", type=int, default=sc.SEED)
    args = p.parse_args()

    client = SolocClient(args.server)
    path = Path(args.path).resolve()
    print(client.action("load_ledger", {"path": str(path)}))
    rows = client.query_all()
    ids = np.array(id_bytes(rows.column("entity_id")), dtype=object)
    frames = np.array(id_bytes(sts_field(rows, "frame_id")), dtype=object)
    t_s = (epochs_ns(rows) - tai_ns_from_utc(sc.T0)) / 1e9
    present = set(ids)

    out_dir = path.parent / "plots"
    out_dir.mkdir(exist_ok=True)
    world = roster(args.seed, client)
    for f in world.facilities:
        mine = [r for r in world.robots if r.host_id == f.id and r.id in present]
        mine += [c for c in world.crawlers if c.disembark and c.disembark[0] is f and c.id in present]
        mine += [c for c in world.cargo if f in (c.origin, c.destination) and c.id in present]
        if mine:
            print(plot_site_robots(f, mine, rows, ids, frames, out_dir))
    if present & {c.id for c in world.spacecraft}:
        print(plot_orbits(client, rows, ids, frames, world.spacecraft, out_dir))
        print(plot_altitudes(client, rows, ids, t_s, world.spacecraft, out_dir))
    for craft in world.spacecraft:
        if "arrival" in craft.events and craft.id in present:
            print(plot_transfer(client, rows, ids, t_s, craft, out_dir))
        if "tli" in craft.events and craft.id in present:
            print(plot_cislunar(client, rows, ids, t_s, craft, out_dir))
    probes = [p for p in world.probes if p.id in present]
    if probes:
        print(plot_heliocentric(client, rows, ids, t_s, probes, out_dir))
    planes = [v for v in world.aircraft if v.id in present]
    if planes:
        print(plot_tracks(rows, ids, t_s, planes, f"Aircraft tracks over {DAYS} days (dot = position at T0)",
                          out_dir, "aircraft.png"))
        print(plot_flight_altitudes(rows, ids, t_s, planes, out_dir))
    ships = [v for v in world.ships if v.id in present]
    if ships:
        print(plot_tracks(rows, ids, t_s, ships, f"Ship tracks over {DAYS} days (dot = position at T0; "
                          "dotted = full lanes; boxes = canal exemptions)", out_dir, "ships.png",
                          routes=[GreatCircle(lane.waypoints) for lane in sc.LANES], canals=True))
    g = world.regatta
    if present & {b.id for b in g.boats}:
        print(plot_regatta(g, rows, out_dir))
        print(plot_regatta_speed(g, rows, out_dir))
    w = world.wildfire
    if present & {c.id for c in w.crews}:
        print(plot_wildfire(w, rows, path.with_name("fuel.arrow"), out_dir))
        print(plot_wildfire_crews(w, rows, out_dir))
    fac = world.factory
    if present & {p.id for p in fac.parts}:
        print(plot_factory_boxes(fac, rows, out_dir))
        print(plot_factory_bearing(fac, client, rows, out_dir))
        print(plot_factory_bearing_analysis(fac, rows, out_dir))


if __name__ == "__main__":
    main()
