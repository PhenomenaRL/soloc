import { describe, expect, it } from "vitest";
import type { EntityRow } from "../arrow/loader";
import { EntityTimeline, gpxContinuousRuns, slerp } from "./timeline";

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

describe("GPX observation coverage", () => {
  function point(index: number, epoch: bigint, segment = 1n): EntityRow {
    return makeRow({ rowIndex: index, epochNs: epoch, position: [index, 0, 0],
      gpx: { source: "route.gpx", object: "track:1", segment, point: BigInt(index + 1) } });
  }

  it("interpolates consecutive source points and hides gaps and uncovered times", () => {
    const rows = [point(0, 0n), point(1, 10n), point(2, 20n, 2n), point(3, 30n, 2n)];
    const timeline = new EntityTimeline(rows);
    expect(timeline.sampleAt(5n)?.positionKm).toEqual([0.5, 0, 0]);
    for (const epoch of [-1n, 11n, 15n, 19n, 31n]) expect(timeline.sampleAt(epoch)).toBeNull();
    for (const epoch of [0n, 10n, 20n, 30n]) expect(timeline.sampleAt(epoch)).not.toBeNull();
    expect(gpxContinuousRuns(rows).map((run) => run.length)).toEqual([2, 2]);
  });

  it("does not interpolate across skipped points, objects or source files", () => {
    const a = point(0, 0n);
    const b = point(1, 10n);
    for (const gpx of [
      { ...b.gpx!, point: 3n },
      { ...b.gpx!, object: "track:2" },
      { ...b.gpx!, source: "other.gpx" },
    ]) expect(new EntityTimeline([a, { ...b, gpx }]).sampleAt(5n)).toBeNull();
  });

  it("isolates duplicate epochs and picks the last file row only at that instant", () => {
    const rows = [point(0, 0n), point(1, 10n), point(2, 10n), point(3, 20n)];
    const timeline = new EntityTimeline(rows);
    expect(timeline.sampleAt(5n)).toBeNull();
    expect(timeline.sampleAt(15n)).toBeNull();
    expect(timeline.sampleAt(10n)?.row.rowIndex).toBe(2);
    expect(gpxContinuousRuns(rows).map((run) => run.length)).toEqual([1, 1, 1, 1]);
    expect(new EntityTimeline([point(0, 0n), point(1, 0n)]).sampleAt(0n)?.row.rowIndex).toBe(1);
  });

  it("does not join reordered timestamps after sorting the timeline", () => {
    const rows = [point(1, 0n), point(0, 10n), point(2, 20n)];
    const timeline = new EntityTimeline(rows);
    expect(timeline.sampleAt(5n)).toBeNull();
    expect(timeline.sampleAt(15n)).toBeNull();
    expect(gpxContinuousRuns(rows)).toHaveLength(3);
  });
});
