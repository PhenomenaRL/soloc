import { readFileSync } from "node:fs";
import { tableFromIPC, tableToIPC } from "apache-arrow";
import { describe, expect, it } from "vitest";
import { loadLedger, type EntityRow, type LedgerData } from "../arrow/loader";
import { earthFrameFor } from "./earthFrame";

const bytes = readFileSync(new URL("../../public/data/halifax-gpx.arrows", import.meta.url));

/** One real GPS conversion and its Earth anchor, using ordinary EntitySchema. */
function singleFix(): LedgerData {
  const table = tableFromIPC(bytes);
  table.schema.metadata.delete("soloc:gpx_playback");
  table.schema.metadata.delete("soloc:gpx_earth_frame");
  const standard = table.select(table.schema.fields.map((f) => f.name).filter((n) => !n.startsWith("gpx_")));
  return loadLedger(tableToIPC(standard.slice(0, 1).concat(standard.slice(5, 6)), "file"));
}

function changed(data: LedgerData, rows: EntityRow[]): LedgerData {
  const entities = new Map<string, EntityRow[]>();
  for (const row of rows) entities.set(row.entityId, [...(entities.get(row.entityId) ?? []), row]);
  return { ...data, rows, entities };
}

describe("explicit Earth frame presentation", () => {
  it("recognizes a standard single-GPS export without inventing GPX provenance", () => {
    const data = singleFix();
    expect(data.rows).toHaveLength(2);
    expect(data.gpxPlayback).toBeUndefined();
    expect(data.rows.every((row) => row.gpx === undefined)).toBe(true);
    expect(earthFrameFor(data)).toBe(data.rows[0]!.frameId);
  });

  it("preserves the validated GPX playback profile", () => {
    const data = loadLedger(bytes);
    expect(earthFrameFor(data)).toBe(data.gpxPlayback!.earthFrameId);
  });

  it("requires a real anchor and rejects translation, invalid rotation, or missing epoch coverage", () => {
    const data = singleFix();
    const [point, anchor] = data.rows as [EntityRow, EntityRow];
    expect(earthFrameFor(changed(data, [point]))).toBeUndefined();
    for (const patch of [
      { position: [1, 0, 0] },
      { quaternion: [0, 0, 0, 0] },
      { quaternion: [NaN, 0, 0, 0] },
      { epochNs: anchor.epochNs + 1n },
    ] as Partial<EntityRow>[]) {
      expect(earthFrameFor(changed(data, [point, { ...anchor, ...patch }]))).toBeUndefined();
    }
  });

  it("does not reinterpret a solar-system scene or a similarly named frame", () => {
    const data = singleFix();
    const [point, anchor] = data.rows as [EntityRow, EntityRow];
    const earthBody = "0000018f-0000-8000-8000-00018f000000"; // IAU_EARTH, not ITRF93
    expect(earthFrameFor(changed(data, [
      { ...point, frameId: earthBody }, { ...anchor, entityId: earthBody },
    ]))).toBeUndefined();
    // A posed GCRF participates in a wider transform tree, not this profile.
    expect(earthFrameFor(changed(data, [point, anchor,
      { ...anchor, entityId: anchor.frameId, frameId: "00000000-0000-8000-8000-000001000000" },
    ]))).toBeUndefined();
  });
});
