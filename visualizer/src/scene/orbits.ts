/**
 * Orbit/trajectory guide lines, derived from the data itself.
 *
 * If an entity's samples (in its current parent frame) sit at near-constant
 * radius, we render a full orbit circle fitted through them (Eyes-style
 * context even though the window covers a sliver of the orbit). Otherwise we
 * render the sampled trajectory as a polyline (spirals, chases, surface
 * drives).
 *
 * Every line carries the epochs behind it so the viewer can reveal it as
 * playback advances rather than drawing the whole window at t0. Without that,
 * a re-parented entity shows a path it has not flown yet — the spaceship's
 * lunar orbit hanging off the Moon for three days before it launches.
 */

import type { EntityRow } from "../arrow/loader";
import { posToKm } from "./units";

export interface GuideLine {
  kind: "circle" | "trail";
  /** Flat xyz triples, km, in the parent frame. */
  points: Float32Array;
  /**
   * Epoch of each point, in the same order — trails only, where points are
   * one-per-sample. A fitted circle interpolates a whole orbit and has no
   * per-point time, so it is revealed all at once instead.
   */
  epochs?: bigint[];
  /**
   * Epoch of the entity's first sample in this frame. The line describes
   * nothing before this: the entity was somewhere else, in other coordinates.
   */
  sinceNs: bigint;
}

const CIRCLE_SEGMENTS = 192;

export function guideLine(rows: readonly EntityRow[], parentFrame: string): GuideLine | null {
  // Only samples expressed in the current parent frame — a re-parented entity's
  // older rows live in a different frame and would be nonsense here.
  const inFrame = rows.filter((r) => r.frameId === parentFrame);
  if (inFrame.length < 2) return null;

  // Rows arrive epoch-sorted from the loader, so the polyline is already in
  // time order and `epochs[i]` lines up with point `i`.
  const epochs = inFrame.map((r) => r.epochNs);
  const sinceNs = epochs[0]!;

  const pts = inFrame.map((r) => posToKm(r.position, r.unitsPos));
  const radii = pts.map(([x, y, z]) => Math.hypot(x, y, z));
  const mean = radii.reduce((a, b) => a + b, 0) / radii.length;
  if (mean === 0) return null; // an entity sitting at its frame origin

  // A fixed installation — a moon base, a ground station — never moves in its
  // parent's frame, so it has no path to draw. Without this it would read as
  // perfectly circular and get fitted to a full orbit ring around its parent.
  const a = pts[0]!;
  const span = Math.max(...pts.map((p) => Math.hypot(p[0] - a[0], p[1] - a[1], p[2] - a[2])));
  if (span < mean * 1e-6) return null;

  const maxDev = Math.max(...radii.map((r) => Math.abs(r - mean)));
  if (maxDev / mean < 0.02) return fitCircle(pts, mean, sinceNs) ?? trail(pts, epochs, sinceNs);
  return trail(pts, epochs, sinceNs);
}

function trail(
  pts: [number, number, number][],
  epochs: bigint[],
  sinceNs: bigint,
): GuideLine {
  const points = new Float32Array(pts.length * 3);
  pts.forEach(([x, y, z], i) => points.set([x, y, z], i * 3));
  return { kind: "trail", points, epochs, sinceNs };
}

/** Full circle through near-circular samples: basis from two well-separated samples. */
function fitCircle(
  pts: [number, number, number][],
  radius: number,
  sinceNs: bigint,
): GuideLine | null {
  const a = pts[0]!;
  // Pick the sample most orthogonal to `a` for a stable normal.
  let b = pts[pts.length - 1]!;
  let bestCross = -1;
  for (const p of pts) {
    const c = cross(a, p);
    const m = Math.hypot(...c);
    if (m > bestCross) {
      bestCross = m;
      b = p;
    }
  }
  const n = cross(a, b);
  const nLen = Math.hypot(...n);
  if (nLen / (radius * radius) < 1e-6) return null; // samples nearly collinear
  const w = scale(n, 1 / nLen);
  const u = scale(a, 1 / Math.hypot(...a));
  const v = cross(w, u);

  const points = new Float32Array((CIRCLE_SEGMENTS + 1) * 3);
  for (let i = 0; i <= CIRCLE_SEGMENTS; i++) {
    const th = (i / CIRCLE_SEGMENTS) * Math.PI * 2;
    const c = Math.cos(th) * radius;
    const s = Math.sin(th) * radius;
    points.set(
      [u[0] * c + v[0] * s, u[1] * c + v[1] * s, u[2] * c + v[2] * s],
      i * 3,
    );
  }
  return { kind: "circle", points, sinceNs };
}

type V3 = [number, number, number];
const cross = (a: V3, b: V3): V3 => [
  a[1] * b[2] - a[2] * b[1],
  a[2] * b[0] - a[0] * b[2],
  a[0] * b[1] - a[1] * b[0],
];
const scale = (a: V3, s: number): V3 => [a[0] * s, a[1] * s, a[2] * s];

/**
 * How many leading epochs are at or before `t` — the number of trail points to
 * draw at that moment.
 *
 * Binary search over an ascending array: this runs for every guide line on
 * every scrub, and a trail carries one point per sample.
 */
export function countAtOrBefore(epochs: readonly bigint[], t: bigint): number {
  if (epochs.length === 0 || t < epochs[0]!) return 0;
  let lo = 0;
  let hi = epochs.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (epochs[mid]! <= t) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}
