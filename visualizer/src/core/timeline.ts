/**
 * Per-entity pose timelines with interpolation.
 *
 * Between two samples in the *same* frame and units we lerp position and
 * slerp orientation. Across a frame or unit change (a re-parent) we never
 * interpolate — the entity holds its last pose until the first row in the new
 * frame takes effect, matching the ledger's convention that a parent change
 * happens exactly at the epoch of the first row expressed in the new frame.
 */

import type { EntityRow } from "../arrow/loader";
import { posToKm } from "../scene/units";

export interface PoseState {
  /** Frame the pose is expressed in (drives scene-graph attachment). */
  frame: string;
  positionKm: [number, number, number];
  /** `[w, x, y, z]`, normalised. */
  quaternion: [number, number, number, number];
  /** The sample this state is based on (for the inspector). */
  row: EntityRow;
  /** 0 = exactly at `row`; (0,1) = interpolating toward the next sample. */
  alpha: number;
  /**
   * Set when the next sample lives in a different frame (or units): a frame
   * hand-off is in progress. `positionKm`/`frame` still describe the held
   * old-frame pose; a renderer that can resolve both frames should glide in
   * world space from this pose toward `nextRow` using `alpha`.
   */
  crossFrame?: boolean;
  nextRow?: EntityRow;
}

export class EntityTimeline {
  constructor(private readonly rows: readonly EntityRow[]) {
    if (rows.length === 0) throw new Error("EntityTimeline needs at least one row");
  }

  /** Pose at `t` (ns since J2000 TAI), clamped to the sample range. */
  at(t: bigint): PoseState {
    const rows = this.rows;
    if (t <= rows[0]!.epochNs) return hold(rows[0]!);

    // Binary search: last index with epochNs <= t.
    let lo = 0;
    let hi = rows.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (rows[mid]!.epochNs <= t) lo = mid;
      else hi = mid - 1;
    }
    const a = rows[lo]!;
    const b = rows[lo + 1];

    if (!b) return hold(a); // end of window

    const span = Number(b.epochNs - a.epochNs);
    const alpha = span <= 0 ? 0 : Math.min(1, Math.max(0, Number(t - a.epochNs) / span));

    // Frame or unit change ahead: report the hand-off instead of blending
    // coordinates from two different frames.
    if (b.frameId !== a.frameId || b.unitsPos !== a.unitsPos) {
      return {
        ...hold(a),
        alpha,
        crossFrame: true,
        nextRow: b,
        quaternion: slerp(a.quaternion, b.quaternion, alpha),
      };
    }
    const pa = posToKm(a.position, a.unitsPos);
    const pb = posToKm(b.position, b.unitsPos);
    return {
      frame: a.frameId,
      positionKm: [
        pa[0] + (pb[0] - pa[0]) * alpha,
        pa[1] + (pb[1] - pa[1]) * alpha,
        pa[2] + (pb[2] - pa[2]) * alpha,
      ],
      quaternion: slerp(a.quaternion, b.quaternion, alpha),
      row: a,
      alpha,
    };
  }
}

export function buildTimelines(
  entities: Map<string, EntityRow[]>,
): Map<string, EntityTimeline> {
  const m = new Map<string, EntityTimeline>();
  for (const [id, rows] of entities) m.set(id, new EntityTimeline(rows));
  return m;
}

function hold(row: EntityRow): PoseState {
  return {
    frame: row.frameId,
    positionKm: posToKm(row.position, row.unitsPos),
    quaternion: normalize(row.quaternion),
    row,
    alpha: 0,
  };
}

type Q = [number, number, number, number]; // [w, x, y, z]

function normalize(q: Q): Q {
  const n = Math.hypot(q[0], q[1], q[2], q[3]) || 1;
  return [q[0] / n, q[1] / n, q[2] / n, q[3] / n];
}

/** Standard quaternion slerp with shortest-path sign flip and nlerp fallback. */
export function slerp(qa: Q, qb: Q, t: number): Q {
  const a = normalize(qa);
  let b = normalize(qb);
  let dot = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
  if (dot < 0) {
    b = [-b[0], -b[1], -b[2], -b[3]];
    dot = -dot;
  }
  if (dot > 0.9995) {
    // Nearly parallel: lerp + renormalise.
    return normalize([
      a[0] + (b[0] - a[0]) * t,
      a[1] + (b[1] - a[1]) * t,
      a[2] + (b[2] - a[2]) * t,
      a[3] + (b[3] - a[3]) * t,
    ]);
  }
  const theta = Math.acos(dot);
  const s = Math.sin(theta);
  const wa = Math.sin((1 - t) * theta) / s;
  const wb = Math.sin(t * theta) / s;
  return [
    a[0] * wa + b[0] * wb,
    a[1] * wa + b[1] * wb,
    a[2] * wa + b[2] * wb,
    a[3] * wa + b[3] * wb,
  ];
}
