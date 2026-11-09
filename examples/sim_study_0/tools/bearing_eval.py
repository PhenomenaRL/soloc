"""Evaluates soloc as the store of a physics sim, against the bearing model's own output in
`bearing_truth.arrow` beside FILE: fidelity (rows vs truth, and through the frame chain to
IAU_EARTH), detection (envelope spectra of the stored captures), and cost (load, query,
current_state and ingest timings, bytes per sample). Prints a metrics table and writes
`out/plots/bearing_{orbit,capture,envelope}.png`.

Loads FILE into the server, and at the end appends one capture again to time ingest, so the
server's ledger is left with that duplicate; the other tools reload FILE themselves.

    python -m tools.bearing_eval out/sim_study_0.arrow
"""

import argparse
import time
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
import pyarrow as pa
import pyarrow.compute as pc

from sim import scenario as sc
from sim.geo import EARTH
from sim.models import bearing as br
from sim.models import factory as fm
from soloc_client import CENTURY_NS, SolocClient, epoch_tai_s, positions, sts_field, tai_ns_from_utc
from tools.merge_sim import of, read
from tools.plot_sim import INK_MUTED, SERIES, style

BAND_HZ = (200.0, 2000.0)            # around the shaft's ~500-700 Hz contact resonance
SPECTRUM_HZ = 10.0
PEAK_HALF_HZ = 0.15
CHAIN_ROWS = 100                     # capture rows per line resolved to IAU_EARTH (~70 ms each)


def epochs_ns(table: pa.Table) -> np.ndarray:
    """ns since scenario.T0."""
    c = sts_field(table, "duration_centuries").to_numpy().astype(np.int64)
    n = sts_field(table, "duration_ns").to_numpy().astype(np.int64)
    return c * CENTURY_NS + n - tai_ns_from_utc(sc.T0)


def vectors(table: pa.Table, column: str) -> np.ndarray:
    arr = table.column(column).combine_chunks()
    return pc.fill_null(arr, pa.scalar([np.nan] * 3, arr.type)).flatten().to_numpy().reshape(-1, 3)


def envelope_spectrum(a: np.ndarray, hz: float) -> tuple[np.ndarray, np.ndarray]:
    """Band-pass around the resonance, Hilbert envelope, then its amplitude spectrum."""
    n = len(a)
    spec = np.fft.fft(a - a.mean())
    band = np.abs(np.fft.fftfreq(n, 1 / hz))
    spec[(band < BAND_HZ[0]) | (band > BAND_HZ[1])] = 0
    # The analytic signal: negative frequencies dropped, positive doubled.
    h = np.zeros(n)
    h[0] = 1
    h[1:(n + 1) // 2] = 2
    if n % 2 == 0:
        h[n // 2] = 1
    env = np.abs(np.fft.ifft(spec * h))
    return np.fft.rfftfreq(n, 1 / hz), np.abs(np.fft.rfft(env - env.mean())) * 2 / n


def peak(f: np.ndarray, amp: np.ndarray, at: float) -> tuple[float, float]:
    """(amplitude near `at`, its ratio to the median floor below SPECTRUM_HZ)."""
    near = np.abs(f - at) <= PEAK_HALF_HZ
    floor = np.median(amp[(f > 0.2) & (f < SPECTRUM_HZ)])
    a = float(amp[near].max())
    return a, a / floor


def kurtosis(x: np.ndarray) -> float:
    x = x - x.mean()
    return float((x ** 4).mean() / (x ** 2).mean() ** 2)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("path", type=Path)
    p.add_argument("--server", default="grpc://localhost:50051")
    args = p.parse_args()

    path = args.path.resolve()
    truth_path = path.with_name("bearing_truth.arrow")
    fac = fm.Factory()
    lines = fac.lines
    shafts = {l["shaft"].id: l for l in lines}
    metrics: list[tuple[str, str]] = []

    # -- rows, straight from the file ------------------------------------------------------------
    table = read(path)
    rows = table.filter(of(table, set(shafts)))
    ids = rows.column("entity_id").combine_chunks().storage.to_pylist()
    t_ns = epochs_ns(rows)
    pos, acc = positions(rows), vectors(rows, "acceleration")
    truth = read(truth_path)
    step_ns = round(sc.CAPTURE_STEP_S * 1e9)

    per_line = {}
    pos_err = acc_err = 0.0
    for l in lines:
        mine = np.flatnonzero([i == l["shaft"].id for i in ids])
        mine = mine[np.argsort(t_ns[mine])]
        cap = mine[[l["shaft"].capturing(t / 1e9) for t in t_ns[mine]]]
        tr = truth.filter(pc.equal(truth["line"], l["name"]))
        t_tr = tr["t_ns"].to_numpy()
        idx = np.searchsorted(t_tr, t_ns[cap])
        assert np.array_equal(t_tr[idx], t_ns[cap]), f"{l['name']}: capture rows off the truth steps"
        pos_err = max(pos_err, float(np.abs(pos[cap, 1] - tr["y"].to_numpy()[idx]).max()),
                      float(np.abs(pos[cap, 2] - tr["z"].to_numpy()[idx]).max()))
        acc_err = max(acc_err, float(np.abs(acc[cap, 1] - tr["acc_y"].to_numpy()[idx]).max()))
        per_line[l["name"]] = {"rows": mine, "cap": cap, "truth": tr}
    metrics += [("capture rows vs truth, position", f"{pos_err:.1e} m (max |error|)"),
                ("capture rows vs truth, acceleration", f"{acc_err:.1e} m/s²")]

    # -- the server: load, query, resolve, ingest ------------------------------------------------
    client = SolocClient(args.server)
    t = time.monotonic()
    client.action("load_ledger", {"path": str(path)})
    metrics.append(("load_ledger", f"{time.monotonic() - t:.1f} s ({table.num_rows:,} rows)"))

    start = fm.CAPTURES_S[2]
    t0_tai = tai_ns_from_utc(sc.T0)
    t = time.monotonic()
    window = client._get({"query_type": "filter", "time_range_tai_s": [
        epoch_tai_s(t0_tai + start * 10**9), epoch_tai_s(t0_tai + (start + sc.CAPTURE_S) * 10**9 - 1)]})
    q_s = time.monotonic() - t
    got = window.filter(of(window, set(shafts))).num_rows
    metrics.append((f"query one {sc.CAPTURE_S} s capture by time range",
                    f"{q_s * 1000:.0f} ms, {window.num_rows:,} rows in range, {got:,} shaft rows"))
    t = time.monotonic()
    client.current_state(list(shafts))
    metrics.append(("current_state of the 3 shafts", f"{(time.monotonic() - t) * 1000:.0f} ms"))

    # Resolve capture rows through 7 frames to IAU_EARTH; the shaft's radial offset from the
    # stator's origin, recovered there, is compared with the truth.
    chain_err, chain_s, chain_n = 0.0, 0.0, 0
    for l in lines:
        d = per_line[l["name"]]
        pick = d["cap"][:: max(1, len(d["cap"]) // CHAIN_ROWS)][:CHAIN_ROWS]
        stator = np.flatnonzero(of(table, {l["stator"].id}).to_numpy(zero_copy_only=False))[:1]
        t = time.monotonic()
        resolved = positions(client.exchange(rows.take(pa.array(pick)), EARTH.frame))
        chain_s, chain_n = chain_s + time.monotonic() - t, chain_n + len(pick)
        origin = positions(client.exchange(table.take(pa.array(stator)), EARTH.frame))[0]
        r = np.linalg.norm(resolved - origin, axis=1) * 1e3                 # km → m
        want = np.hypot(pos[pick, 1], pos[pick, 2])
        chain_err = max(chain_err, float(np.abs(r - want).max()))
    ulp = np.spacing(np.linalg.norm(origin)) * 1e3
    metrics.append((f"shaft offset recovered via {EARTH.frame} (7 hops, km)",
                    f"{chain_err * 1e9:.1f} nm max error; float64 spacing at Earth radius {ulp * 1e9:.2f} nm"))
    metrics.append((f"exchange of past capture rows to {EARTH.frame}",
                    f"{chain_s / chain_n * 1000:.0f} ms per row ({chain_n} rows)"))

    # Bytes per capture sample: in the ledger (every column) against the truth table's row.
    cap_rows = rows.take(pa.array(per_line[lines[0]["name"]]["cap"]))
    metrics.append(("bytes per capture sample", f"ledger row {cap_rows.nbytes / cap_rows.num_rows:.0f} B, "
                    f"truth row {truth.nbytes / truth.num_rows:.0f} B, raw t+y+z+a_y+a_z 40 B"))
    t = time.monotonic()
    for batch in cap_rows.to_batches():
        client.put(batch)
    ingest = time.monotonic() - t
    metrics.append((f"ingest {lines[0]['name']}'s {len(fm.CAPTURES_S)} captures again",
                    f"{cap_rows.num_rows / ingest:,.0f} rows/s "
                    f"({cap_rows.num_rows:,} rows in {ingest:.2f} s)"))

    # -- detection from the stored captures ------------------------------------------------------
    spectra, stats = {}, {}
    n_cap = sc.CAPTURE_S * sc.CAPTURE_HZ
    for l in lines:
        d = per_line[l["name"]]
        a = acc[d["cap"], 1].reshape(len(fm.CAPTURES_S), n_cap)
        f, amps = zip(*(envelope_spectrum(x, sc.CAPTURE_HZ) for x in a))
        spectra[l["name"]] = (f[0], np.mean(amps, axis=0))
        stats[l["name"]] = (np.sqrt((a ** 2).mean(axis=1)), np.array([kurtosis(x) for x in a]))
    m3 = lines[2]["model"]
    kb, kj = (i - 1 for i in sc.WEAR[2].spall)
    two_bsf = 2 * abs(br.BALL_RATIO) * (1 - m3.spin_slip[kb, kj]) * sc.SHAFT_HZ
    for l in lines:
        f, amp = spectra[l["name"]]
        a, snr = peak(f, amp, two_bsf)
        rms, kurt = stats[l["name"]]
        metrics.append((f"{l['name']} envelope at 2×BSF {two_bsf:.2f} Hz", f"{a:.2e} m/s², {snr:.1f}× the floor"))
        metrics.append((f"{l['name']} capture RMS / kurtosis", f"{rms.mean():.4f} m/s² / {kurt.mean():.1f} "
                        f"(range {rms.min():.4f}-{rms.max():.4f})"))

    # Cage slip read back from the cage rows' angular_velocity against the shaft's.
    cage_ids = {b["cage"].id: (l, kc) for l in lines for kc, b in enumerate(l["bearings"])}
    cages = table.filter(of(table, set(cage_ids)))
    w = vectors(cages, "angular_velocity")[:, 0]
    cid = cages.column("entity_id").combine_chunks().storage.to_pylist()
    tc = epochs_ns(cages) / 1e9
    turning = fm.omega(tc) > 0.99 * fm.OMEGA
    for cage, (l, kc) in cage_ids.items():
        k = np.array([i == cage for i in cid]) & turning
        slip = 1 - np.median(w[k] / fm.omega(tc[k])) / fm.CAGE_RATIO
        metrics.append((f"{l['name']} cage {kc + 1} slip, from rows", f"{slip:.3%} (model {l['model'].cage_slip[kc]:.3%})"))

    width = max(len(k) for k, _ in metrics)
    for k, v in metrics:
        print(f"{k.ljust(width)}  {v}")

    out_dir = Path("out/plots")
    out_dir.mkdir(parents=True, exist_ok=True)
    print(plot_orbit(lines, per_line, pos, t_ns, out_dir))
    print(plot_capture(lines, per_line, acc, t_ns, out_dir))
    print(plot_envelope(lines, spectra, two_bsf, out_dir))


def plot_orbit(lines, per_line, pos, t_ns, out_dir: Path) -> Path:
    """Each shaft centre in its stator, µm, one panel per line on one scale: one capture's orbit
    and the quasi-static rows."""
    fig, axes = plt.subplots(1, len(lines), figsize=(15, 6), sharex=True, sharey=True, layout="constrained")
    start = fm.CAPTURES_S[2]
    for ax, colour, l in zip(axes, SERIES, lines):
        d = per_line[l["name"]]
        quasi = np.setdiff1d(d["rows"], d["cap"])
        ax.plot(pos[quasi, 1] * 1e6, pos[quasi, 2] * 1e6, ".", color=INK_MUTED, markersize=2, alpha=0.3,
                label="quasi-static rows, whole shift")
        k = d["cap"][(t_ns[d["cap"]] >= start * 10**9) & (t_ns[d["cap"]] < (start + sc.CAPTURE_S) * 10**9)]
        ax.plot(pos[k, 1] * 1e6, pos[k, 2] * 1e6, color=colour, linewidth=0.8,
                label=f"{sc.CAPTURES[2]:%H:%M} UTC capture, {sc.CAPTURE_HZ} Hz")
        w = l["model"].wear
        ax.set_title(f"{l['name']}: c_r {w.clearance_m * 1e6:.0f} µm, ball σ {w.ball_sigma_m * 1e6:.1f} µm"
                     + (", spalled ball" if w.spall else ""), fontsize=9)
        ax.set_aspect("equal")
        ax.set_xlabel("y in the stator, µm (toward the belt)", fontsize=8)
        style(ax)
    axes[0].set_ylabel("z in the stator, µm (up)", fontsize=8)
    axes[0].legend(fontsize=7, loc="lower left")
    fig.suptitle("shaft centre in its stator, read back from the ledger", fontsize=10)
    path = out_dir / "bearing_orbit.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_capture(lines, per_line, acc, t_ns, out_dir: Path) -> Path:
    """The first 3 s of one capture's stored acceleration per line, L3's spall passes ticked."""
    start, shown = fm.CAPTURES_S[2], 3.0
    fig, axes = plt.subplots(len(lines), 1, figsize=(13, 8), sharex=True, layout="constrained")
    for ax, colour, l in zip(axes, SERIES, lines):
        k = per_line[l["name"]]["cap"]
        t = t_ns[k] / 1e9 - start
        sel = (t >= 0) & (t < shown)
        ax.plot(t[sel], acc[k[sel], 1], color=colour, linewidth=0.6)
        ax.set_ylabel(f"{l['name']}\na_y, m/s²", fontsize=8)
        style(ax)
        m = l["model"]
        if m.wear.spall:
            kb, kj = (i - 1 for i in m.wear.spall)
            fine = start + np.arange(0, shown, 1e-4)
            depth = m.spall_depth(fm.angle(fine))[:, kb, kj] > 0
            edges = fine[1:][np.diff(depth.astype(int)) == 1] - start
            for e in edges:
                ax.axvline(e, color=INK_MUTED, linewidth=0.6, linestyle=":")
            ax.text(0.995, 0.92, "dotted: the spalled ball meets a race", transform=ax.transAxes,
                    ha="right", fontsize=7, color=INK_MUTED)
    axes[-1].set_xlabel(f"seconds from {sc.CAPTURES[2]:%H:%M} UTC ({sc.CAPTURE_HZ} Hz rows)", fontsize=8)
    fig.suptitle("stored shaft acceleration (y) per line, each on its own scale: ball-pass stiffness ripple, "
                 "and L3's spall impacts inside the load zone", fontsize=10)
    path = out_dir / "bearing_capture.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


def plot_envelope(lines, spectra, two_bsf: float, out_dir: Path) -> Path:
    """Envelope spectra averaged over every capture, with the defect lines."""
    marks = {"FTF": br.FTF, "BSF": br.BSF, "BPFO": br.BPFO, "2×BSF": two_bsf, "BPFI": br.BPFI}
    fig, axes = plt.subplots(len(lines), 1, figsize=(13, 8), sharex=True, layout="constrained")
    for ax, colour, l in zip(axes, SERIES, lines):
        f, amp = spectra[l["name"]]
        sel = f <= SPECTRUM_HZ
        ax.plot(f[sel], amp[sel], color=colour, linewidth=1.0)
        for name, hz in marks.items():
            ax.axvline(hz, color=INK_MUTED, linewidth=0.6, linestyle=":")
            if ax is axes[0]:
                ax.text(hz, 1.02, name, transform=ax.get_xaxis_transform(), ha="center", fontsize=7, color=INK_MUTED)
        ax.set_ylabel(f"{l['name']}\nm/s²", fontsize=8)
        style(ax)
    axes[-1].set_xlabel(f"Hz (envelope of {BAND_HZ[0]:.0f}-{BAND_HZ[1]:.0f} Hz band; "
                        f"{1 / sc.CAPTURE_S:.1f} Hz resolution, {len(fm.CAPTURES_S)} captures averaged)", fontsize=8)
    fig.suptitle("envelope spectra of the stored captures, each line on its own scale", fontsize=10)
    path = out_dir / "bearing_envelope.png"
    fig.savefig(path, dpi=120)
    plt.close(fig)
    return path


if __name__ == "__main__":
    main()
