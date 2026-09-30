import { describe, expect, it } from "vitest";
import type { EntityRow } from "../arrow/loader";
import { countAtOrBefore, guideLine } from "./orbits";

const HOUR = 3_600_000_000_000n;

/** A pose row; ids are opaque strings here, as everywhere below the loader. */
function row(over: Partial<EntityRow>): EntityRow {
  return {
    entityId: "ship",
    frameId: "earth",
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

describe("countAtOrBefore", () => {
  const epochs = [0n, HOUR, 2n * HOUR, 3n * HOUR];

  it("counts only what has already happened", () => {
    expect(countAtOrBefore(epochs, 0n)).toBe(1);
    expect(countAtOrBefore(epochs, 2n * HOUR)).toBe(3);
    expect(countAtOrBefore(epochs, 3n * HOUR)).toBe(4);
  });

  it("includes a sample exactly at t, and counts partial hours down", () => {
    expect(countAtOrBefore(epochs, HOUR - 1n)).toBe(1);
    expect(countAtOrBefore(epochs, HOUR)).toBe(2);
  });

  it("is empty before the first sample and saturated after the last", () => {
    expect(countAtOrBefore(epochs, -1n)).toBe(0);
    expect(countAtOrBefore(epochs, 99n * HOUR)).toBe(4);
    expect(countAtOrBefore([], 5n)).toBe(0);
  });
});

describe("guideLine", () => {
  // A path that grows in radius is a trail, not a fitted circle.
  const spiral = [0, 1, 2, 3].map((h) =>
    row({ epochNs: BigInt(h) * HOUR, position: [1000 + 500 * h, 0, 0] }),
  );

  it("carries one epoch per point, in order", () => {
    const g = guideLine(spiral, "earth")!;
    expect(g.kind).toBe("trail");
    expect(g.epochs).toEqual([0n, HOUR, 2n * HOUR, 3n * HOUR]);
    expect(g.points.length).toBe(4 * 3);
  });

  it("reports the first sample epoch in this frame as sinceNs", () => {
    // The spaceship case: rows in the Moon's frame only start at the re-parent,
    // so the lunar path describes nothing before T+84 h.
    const rows = [
      ...spiral,
      row({ epochNs: 84n * HOUR, frameId: "moon", position: [19800, 0, 0] }),
      row({ epochNs: 85n * HOUR, frameId: "moon", position: [18000, 4000, 0] }),
    ];
    expect(guideLine(rows, "earth")!.sinceNs).toBe(0n);
    expect(guideLine(rows, "moon")!.sinceNs).toBe(84n * HOUR);
  });

  it("ignores samples expressed in a different frame", () => {
    const rows = [
      ...spiral,
      row({ epochNs: 84n * HOUR, frameId: "moon", position: [19800, 0, 0] }),
    ];
    expect(guideLine(rows, "earth")!.epochs).toHaveLength(4);
  });

  it("needs two samples in the frame to draw anything", () => {
    expect(guideLine([spiral[0]!], "earth")).toBeNull();
    expect(guideLine(spiral, "mars")).toBeNull();
  });

  it("fits a circle through near-constant radius, with no per-point epochs", () => {
    const orbit = [0, 1, 2, 3, 4].map((h) => {
      const th = (h * Math.PI) / 12;
      return row({
        epochNs: BigInt(h) * HOUR,
        position: [7000 * Math.cos(th), 7000 * Math.sin(th), 0],
      });
    });
    const g = guideLine(orbit, "earth")!;
    expect(g.kind).toBe("circle");
    expect(g.epochs).toBeUndefined(); // interpolated, so it reveals all at once
    expect(g.sinceNs).toBe(0n);
  });

  it("draws no path for a fixed installation that never moves", () => {
    // The moon base: identical rows in the Moon's frame. Constant radius would
    // otherwise read as a perfect orbit and get fitted to a ring around it.
    const base = [0, 1, 2, 3].map((h) =>
      row({ epochNs: BigInt(h) * HOUR, position: [1737.4, 0, 0] }),
    );
    expect(guideLine(base, "earth")).toBeNull();
  });

  it("still draws a body that barely moves relative to its distance", () => {
    // Neptune covers ~0.01% of its orbit in a week; that is a path, not a fixed
    // point, and the stationary test must not swallow it.
    const crawl = [0, 1, 2, 3].map((h) =>
      row({ epochNs: BigInt(h) * HOUR, position: [4.5e9, 3.3e5 * h, 0] }),
    );
    expect(guideLine(crawl, "earth")).not.toBeNull();
  });

  it("returns nothing for an entity sitting at its frame origin", () => {
    const atOrigin = [0, 1].map((h) => row({ epochNs: BigInt(h) * HOUR }));
    expect(guideLine(atOrigin, "earth")).toBeNull();
  });
});
