import type { LedgerData } from "../arrow/loader";
import { astroFrameOf, kindOfIdString, KIND_ASTRO } from "./identity";

function isEarthFrame(id: string, orientation: number): boolean {
  const frame = astroFrameOf(id);
  return kindOfIdString(id) === KIND_ASTRO &&
    frame?.ephemerisId === 399 && frame.orientationId === orientation;
}

/**
 * Select the explicit anchor for an Earth-centred scene. GPX exports already
 * validate this contract in the loader. Ordinary EntitySchema exports can carry
 * it too: a GPS observation plus an ITRF93 → GCRF snapshot needs no GPX columns.
 *
 * Keep inference narrow: every observation must be directly under ITRF93 and
 * covered by finite, unit-rotation, zero-translation anchors under an unposed
 * GCRF root. Do not reinterpret a solar-system ledger or infer geometry from a
 * display name. This affects presentation only; it never changes stored rows.
 */
export function earthFrameFor(data: LedgerData): string | undefined {
  if (data.gpxPlayback) return data.gpxPlayback.earthFrameId;
  for (const [id, anchors] of data.entities) {
    if (!isEarthFrame(id, 3000) || anchors.length === 0) continue;
    if (anchors.some((row, i) =>
      !isEarthFrame(row.frameId, 1) || data.entities.has(row.frameId) ||
      row.position.some((n) => n !== 0) ||
      !row.quaternion.every(Number.isFinite) ||
      Math.abs(Math.hypot(...row.quaternion) - 1) > 1e-6 ||
      (i > 0 && (row.epochNs <= anchors[i - 1]!.epochNs ||
        row.epochNs - anchors[i - 1]!.epochNs > 300_000_000_000n)),
    )) continue;
    const observations = data.rows.filter((row) => row.entityId !== id);
    if (observations.length === 0 || observations.some((row) =>
      row.frameId !== id || row.epochNs < anchors[0]!.epochNs ||
      row.epochNs > anchors[anchors.length - 1]!.epochNs,
    )) continue;
    return id;
  }
  return undefined;
}
