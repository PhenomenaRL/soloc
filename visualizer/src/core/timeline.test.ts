import { describe, expect, it } from "vitest";
import type { EntityRow } from "../arrow/loader";
import { EntityTimeline, slerp } from "./timeline";

function makeRow(over: Partial<EntityRow>): EntityRow {
  return {
    entityId: "demo:x",
    frameId: "ICRF",
    unitsPos: "km",
    timescaleId: "TAI",
    sourceId: "test",
    estimateType: "MEASURED",
    position: [0, 0, 0],
    quaternion: [1, 0, 0, 0],
    epochNs: 0n,
    positionCovariance: null,
    orientationCovariance: null,
    velocity: null,
    angularVelocity: null,
    acceleration: null,
    massKg: null,
    stateCovariance: null,
    dimensionsM: null,
    rowIndex: 0,
    ...over,
  };
}

describe("EntityTimeline interpolation", () => {
  const tl = new EntityTimeline([
    makeRow({ epochNs: 0n, position: [0, 0, 0] }),
    makeRow({ epochNs: 1000n, position: [10, -20, 4] }),
  ]);

  it("lerps position at the midpoint", () => {
    const s = tl.at(500n);
    expect(s.positionKm).toEqual([5, -10, 2]);
    expect(s.alpha).toBe(0.5);
    expect(s.frame).toBe("ICRF");
  });

  it("clamps before the first and after the last sample", () => {
    expect(tl.at(-5n).positionKm).toEqual([0, 0, 0]);
    expect(tl.at(99999n).positionKm).toEqual([10, -20, 4]);
  });

  it("converts native units before lerping", () => {
    const m = new EntityTimeline([
      makeRow({ epochNs: 0n, position: [1000, 0, 0], unitsPos: "m" }),
      makeRow({ epochNs: 100n, position: [3000, 0, 0], unitsPos: "m" }),
    ]);
    expect(m.at(50n).positionKm[0]).toBeCloseTo(2); // metres → km
  });
});

describe("EntityTimeline across a re-parent", () => {
  const tl = new EntityTimeline([
    makeRow({ epochNs: 0n, frameId: "naif:399", position: [100, 0, 0] }),
    makeRow({ epochNs: 100n, frameId: "naif:399", position: [200, 0, 0] }),
    makeRow({ epochNs: 200n, frameId: "naif:301", position: [7, 7, 7] }),
  ]);

  it("reports a hand-off instead of blending coordinates across frames", () => {
    const s = tl.at(150n);
    expect(s.frame).toBe("naif:399"); // held old-frame pose…
    expect(s.positionKm).toEqual([200, 0, 0]);
    expect(s.crossFrame).toBe(true); // …but flagged, with glide progress
    expect(s.alpha).toBe(0.5);
    expect(s.nextRow?.frameId).toBe("naif:301");
  });

  it("switches frame exactly at the first row in the new frame", () => {
    expect(tl.at(199n).frame).toBe("naif:399");
    const s = tl.at(200n);
    expect(s.frame).toBe("naif:301");
    expect(s.positionKm).toEqual([7, 7, 7]);
    expect(s.crossFrame).toBeUndefined();
  });

  it("treats a unit change as a hand-off too (no km↔mm blend)", () => {
    const u = new EntityTimeline([
      makeRow({ epochNs: 0n, position: [10, 0, 0], unitsPos: "km" }),
      makeRow({ epochNs: 100n, position: [5_000_000, 0, 0], unitsPos: "mm" }),
    ]);
    const s = u.at(50n);
    expect(s.positionKm).toEqual([10, 0, 0]); // held, not blended
    expect(s.crossFrame).toBe(true);
  });
});

describe("slerp", () => {
  const halfTurnZ: [number, number, number, number] = [0, 0, 0, 1]; // 180° about z
  it("hits the quarter-turn midpoint", () => {
    const mid = slerp([1, 0, 0, 0], halfTurnZ, 0.5); // → 90° about z
    expect(mid[0]).toBeCloseTo(Math.SQRT1_2);
    expect(mid[3]).toBeCloseTo(Math.SQRT1_2);
  });

  it("returns unit quaternions", () => {
    const q = slerp([1, 0, 0, 0], [0.5, 0.5, 0.5, 0.5], 0.3);
    expect(Math.hypot(...q)).toBeCloseTo(1);
  });

  it("takes the short way around (sign flip)", () => {
    const q = slerp([1, 0, 0, 0], [-1, 0, 0, 0], 0.5); // same rotation, opposite sign
    expect(Math.abs(q[0])).toBeCloseTo(1);
  });
});
