/**
 * Playback bar: play/pause, speed, scrubber with re-parent event ticks, and
 * the current TAI clock. Owns no state — main.ts holds the clock and calls
 * `update()`; user input flows back through the callbacks.
 */

import { formatTai } from "../core/epoch";
import type { TopologyEvent } from "../core/topology";

const SLIDER_STEPS = 20_000;

export interface TimeControlsOptions {
  minNs: bigint;
  maxNs: bigint;
  events: readonly TopologyEvent[];
  onScrub: (t: bigint) => void;
  onPlayToggle: () => void;
  onSpeedChange: (nsPerRealSecond: number) => void;
}

export interface TimeControls {
  update(t: bigint, playing: boolean): void;
  /** Removes the window-level space-bar listener this instance installed. */
  destroy(): void;
}

export const SPEEDS: { label: string; nsPerSec: number }[] = [
  { label: "1 min/s", nsPerSec: 60e9 },
  { label: "1 h/s", nsPerSec: 3_600e9 },
  { label: "6 h/s", nsPerSec: 21_600e9 },
  { label: "1 d/s", nsPerSec: 86_400e9 },
];
export const DEFAULT_SPEED = SPEEDS[2]!;

export function buildTimeControls(bar: HTMLElement, opts: TimeControlsOptions): TimeControls {
  bar.replaceChildren();
  const span = Number(opts.maxNs - opts.minNs);

  const playBtn = document.createElement("button");
  playBtn.className = "tc-play";
  playBtn.textContent = "▶";
  playBtn.title = "play / pause (space)";
  playBtn.addEventListener("click", () => opts.onPlayToggle());

  const speedSel = document.createElement("select");
  speedSel.className = "tc-speed";
  for (const s of SPEEDS) {
    const o = document.createElement("option");
    o.value = String(s.nsPerSec);
    o.textContent = s.label;
    if (s === DEFAULT_SPEED) o.selected = true;
    speedSel.append(o);
  }
  speedSel.addEventListener("change", () => opts.onSpeedChange(Number(speedSel.value)));

  const track = document.createElement("div");
  track.className = "tc-track";
  const slider = document.createElement("input");
  slider.type = "range";
  slider.min = "0";
  slider.max = String(SLIDER_STEPS);
  slider.value = "0";
  slider.addEventListener("input", () => {
    const frac = Number(slider.value) / SLIDER_STEPS;
    opts.onScrub(opts.minNs + BigInt(Math.round(frac * span)));
  });
  track.append(slider);

  // Tick marks at re-parent events (skip the first-sighting burst at t0).
  for (const e of opts.events) {
    if (e.epochNs <= opts.minNs) continue;
    const frac = Number(e.epochNs - opts.minNs) / span;
    const tick = document.createElement("div");
    tick.className = "tc-tick";
    tick.style.left = `${(frac * 100).toFixed(2)}%`;
    tick.title = `${e.childId} ← ${e.parentId}\n${formatTai(e.epochNs)}`;
    track.append(tick);
  }

  const clock = document.createElement("div");
  clock.className = "tc-clock";

  bar.append(playBtn, speedSel, track, clock);

  const onKeydown = (ev: KeyboardEvent): void => {
    if (ev.code === "Space" && !(ev.target instanceof HTMLInputElement)) {
      ev.preventDefault();
      opts.onPlayToggle();
    }
  };
  window.addEventListener("keydown", onKeydown);

  return {
    destroy() {
      window.removeEventListener("keydown", onKeydown);
    },
    update(t, playing) {
      playBtn.textContent = playing ? "⏸" : "▶";
      const frac = span <= 0 ? 0 : Number(t - opts.minNs) / span;
      slider.value = String(Math.round(frac * SLIDER_STEPS));
      clock.textContent = formatTai(t);
      for (const tickEl of track.querySelectorAll<HTMLElement>(".tc-tick")) {
        tickEl.classList.toggle(
          "passed",
          Number(slider.value) / SLIDER_STEPS >= parseFloat(tickEl.style.left) / 100,
        );
      }
    },
  };
}
