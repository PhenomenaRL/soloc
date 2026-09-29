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
from run_sim import roster
from soloc_client import SolocClient, id_bytes, positions

TRACK = "#2a78d6"
MARK = "#eb6834"
INK_MUTED = "#6b6a63"


def plot_site_robots(facility, robots, rows, ids, out_dir: Path) -> Path:
    """Robot tracks in the site's ENU frame (the frame they are stored in), one panel each."""
    cols = 5
    fig, axes = plt.subplots(2, cols, figsize=(3.2 * cols, 6.8), sharex=True, sharey=True,
                             layout="constrained")
    w = sc.SITE_HALF_WIDTH_M
    for ax, robot in zip(axes.flat, robots):
        xyz = positions(rows.filter(pa.array(ids == robot.id)))
        ax.plot(xyz[:, 0], xyz[:, 1], color=TRACK, linewidth=0.8)
        ax.plot(*xyz[0, :2], "o", color=MARK, markersize=5)
        ax.add_patch(plt.Rectangle((-w, -w), 2 * w, 2 * w, fill=False, linestyle="--",
                                   edgecolor=INK_MUTED, linewidth=0.8))
        ax.set_title(robot.name, fontsize=9)
        ax.set_aspect("equal")
        ax.set_xlim(-1.1 * w, 1.1 * w)
        ax.set_ylim(-1.1 * w, 1.1 * w)
        ax.grid(color="#e6e5df", linewidth=0.5)
        ax.tick_params(labelsize=7, colors=INK_MUTED)
    for ax in axes[-1]:
        ax.set_xlabel("east (m)", fontsize=8)
    for ax in axes[:, 0]:
        ax.set_ylabel("north (m)", fontsize=8)
    fig.suptitle(f"{facility.name}: robot tracks over 3 days, site ENU "
                 f"(dot = t0, dashed = site area)", fontsize=11)
    path = out_dir / f"robots_{facility.spec.code.lower()}.png"
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
    present = set(ids)

    out_dir = path.parent / "plots"
    out_dir.mkdir(exist_ok=True)
    facilities, robots = roster(args.seed)
    for f in facilities:
        mine = [r for r in robots if r.host_id == f.id and r.id in present]
        if mine:
            print(plot_site_robots(f, mine, rows, ids, out_dir))


if __name__ == "__main__":
    main()
