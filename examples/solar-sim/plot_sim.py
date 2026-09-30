"""Sanity-check plots of a saved sim ledger, one PNG per figure into out/plots/.

Reloads the file into the server first, so it can run right after run_sim.py.
"""

import argparse
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
import pyarrow as pa

import scenario as sc
from geo import EARTH, MARS, MOON, GreatCircle, fixed_to_geodetic
from land import CANALS, outlines
from run_sim import roster
from soloc_client import (CENTURY_NS, SolocClient, id_bytes, matches, positions, sts_field,
                          tai_ns_from_utc)

TRACK = "#2a78d6"
MARK = "#eb6834"
INK_MUTED = "#6b6a63"
GRID = "#e6e5df"
BODY_FILL = "#d9d8d2"
SERIES = ("#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4")   # categorical slots 1-5


def style(ax):
    ax.grid(color=GRID, linewidth=0.5)
    ax.tick_params(labelsize=7, colors=INK_MUTED)


def plot_site_robots(facility, robots, rows, ids, frames, out_dir: Path) -> Path:
    """Robot tracks in the site's ENU frame (the frame they are stored in), one panel each, over
    the site layout. `robots` may include a disembarked crawler; only its rows framed on this
    facility are drawn."""
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
    fig.suptitle(f"{facility.name}: robot rows over 3 days, site ENU (small dots = 30 s rows, "
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
    fig.suptitle("Spacecraft over 3 days, body-centred with ICRF axes "
                 "(top: x–y, bottom: x–z; grey disc = body)", fontsize=11)
    path = out_dir / "orbits.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_altitudes(client, rows, ids, t_s, spacecraft, out_dir: Path) -> Path | None:
    """Altitude against time around each launch and landing, resolved to the body frame through
    the ledger (pad and landed rows resolve through their facility)."""
    shows = []
    for c in spacecraft:
        if "liftoff" in c.events:
            shows.append((c, c.events["liftoff"], "liftoff", [("insertion", c.events["insertion"])]))
        if "touchdown" in c.events:
            shows.append((c, c.events["deorbit"], "deorbit",
                          [("perilune", c.events["perilune"]), ("touchdown", c.events["touchdown"])]))
    shows = [s for s in shows if s[0].id in set(ids)]
    if not shows:
        return None
    fig, axes = plt.subplots(1, len(shows), figsize=(5 * len(shows), 4), layout="constrained")
    for ax, (craft, t_ref, ref_name, marks) in zip(np.atleast_1d(axes), shows):
        t_end = marks[-1][1]
        mask = matches(ids, craft.id) & (t_s >= t_ref - 600) & (t_s <= t_end + 1200)
        window = rows.filter(pa.array(mask))
        fixed = positions(client.exchange(window, craft.body.frame))
        _, _, h = fixed_to_geodetic(craft.body, fixed)
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
        ax.set_title(f"{craft.name} ({craft.facility.name})", fontsize=10)
        ax.set_xlabel(f"minutes from {ref_name}", fontsize=8)
        ax.set_ylabel(f"altitude above {craft.body.name} (km)", fontsize=8)
        style(ax)
    fig.suptitle("Launches and landing: altitude resolved to the body frame", fontsize=11)
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
    """Stored tracks over 3 days on a lon/lat map with the land polygons: one hue for every
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
    """Stored altitude over the 3 days, one panel per aircraft."""
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
    fig.suptitle("Aircraft altitude over 3 days (flat at field elevation = parked)", fontsize=11)
    path = out_dir / "flight_altitudes.png"
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
    world = roster(args.seed)
    for f in world.facilities:
        mine = [r for r in world.robots if r.host_id == f.id and r.id in present]
        mine += [c for c in world.crawlers if c.disembark and c.disembark[0] is f and c.id in present]
        if mine:
            print(plot_site_robots(f, mine, rows, ids, frames, out_dir))
    if present & {c.id for c in world.spacecraft}:
        print(plot_orbits(client, rows, ids, frames, world.spacecraft, out_dir))
        print(plot_altitudes(client, rows, ids, t_s, world.spacecraft, out_dir))
    planes = [v for v in world.aircraft if v.id in present]
    if planes:
        print(plot_tracks(rows, ids, t_s, planes, "Aircraft tracks over 3 days (dot = position at T0)",
                          out_dir, "aircraft.png"))
        print(plot_flight_altitudes(rows, ids, t_s, planes, out_dir))
    ships = [v for v in world.ships if v.id in present]
    if ships:
        print(plot_tracks(rows, ids, t_s, ships, "Ship tracks over 3 days (dot = position at T0; "
                          "dotted = full lanes; boxes = canal exemptions)", out_dir, "ships.png",
                          routes=[GreatCircle(lane.waypoints) for lane in sc.LANES], canals=True))


if __name__ == "__main__":
    main()
