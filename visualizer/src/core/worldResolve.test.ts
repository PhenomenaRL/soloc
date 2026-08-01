/**
 * Ids are opaque strings to the resolver, so these tests use readable
 * placeholders where a real fixture would carry hyphenated prescribed ids.
 */

import { describe, expect, it } from "vitest";
import type { EntityRow } from "../arrow/loader";
import { EntityTimeline } from "./timeline";
import { WorldResolver } from "./worldResolve";

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

function timelines(spec: Record<string, Partial<EntityRow>[]>): Map<string, EntityTimeline> {
  const m = new Map<string, EntityTimeline>();
  for (const [id, rows] of Object.entries(spec)) {
    m.set(id, new EntityTimeline(rows.map((r) => makeRow({ entityId: id, ...r }))));
  }
  return m;
}

describe("WorldResolver.worldPoseAt", () => {
  it("resolves chains at the requested epoch, not 'now'", () => {
    const r = new WorldResolver(
      timelines({
        "demo:ast": [
          { epochNs: 0n, position: [0, 0, 0] },
          { epochNs: 1000n, position: [72_000, 0, 0] }, // fast parent
        ],
      }),
    );
    expect(r.worldPoseAt("demo:ast", 0n).p).toEqual([0, 0, 0]);
    expect(r.worldPoseAt("demo:ast", 500n).p).toEqual([36_000, 0, 0]);
    expect(r.worldPoseAt("demo:ast", 1000n).p).toEqual([72_000, 0, 0]);
  });

  it("composes nested chains (moon → earth → ICRF)", () => {
    const r = new WorldResolver(
      timelines({
        earth: [{ epochNs: 0n, position: [1000, 0, 0] }],
        moon: [{ epochNs: 0n, frameId: "earth", position: [0, 384, 0] }],
      }),
    );
    expect(r.worldPoseAt("moon", 0n).p).toEqual([1000, 384, 0]);
  });

  it("resolves a body-fixed frame through the body's own rows", () => {
    // Under prescribed ids IAU_MOON *is* the Moon's id, so a rover parented to
    // the body-fixed frame picks up the body's orientation with no host table.
    const yaw90: [number, number, number, number] = [Math.SQRT1_2, 0, 0, Math.SQRT1_2];
    const r = new WorldResolver(
      timelines({
        moon: [{ epochNs: 0n, position: [1000, 0, 0], quaternion: yaw90 }],
        rover: [{ epochNs: 0n, frameId: "moon", position: [1737, 0, 0] }],
      }),
    );
    const p = r.worldPoseAt("rover", 0n).p;
    expect(p[0]).toBeCloseTo(1000); // +x in the rotating frame → +y in world
    expect(p[1]).toBeCloseTo(1737);
  });

  it("treats an id nothing poses as a terminal anchor", () => {
    const r = new WorldResolver(timelines({}));
    expect(r.worldPoseAt("ICRF", 0n)).toEqual({ p: [0, 0, 0], q: [1, 0, 0, 0] });
  });

  it("applies parent orientation to child offsets", () => {
    const yaw90: [number, number, number, number] = [Math.SQRT1_2, 0, 0, Math.SQRT1_2];
    const r = new WorldResolver(
      timelines({
        "demo:ship": [{ epochNs: 0n, position: [100, 0, 0], quaternion: yaw90 }],
        "demo:cam": [{ epochNs: 0n, frameId: "demo:ship", position: [10, 0, 0] }],
      }),
    );
    const p = r.worldPoseAt("demo:cam", 0n).p;
    expect(p[0]).toBeCloseTo(100); // +x rotated 90° about z → +y
    expect(p[1]).toBeCloseTo(10);
  });
});

describe("WorldResolver.reexpress — the miner hand-off case", () => {
  // Parent races along +x at 72 km/tick; child chases 136 km ahead on +x.
  const r = new WorldResolver(
    timelines({
      "demo:ast": [
        { epochNs: 0n, position: [0, 0, 0] },
        { epochNs: 1000n, position: [72_000, 0, 0] },
      ],
    }),
  );

  it("computes the child's offset relative to the parent AT the old epoch", () => {
    // Child's last ICRF row: parent(600) + 136 on x.
    const parentAt600 = 43_200;
    const rel = r.reexpress([parentAt600 + 136, 0, 0], "ICRF", "demo:ast", 600n);
    expect(rel[0]).toBeCloseTo(136);
    expect(rel[1]).toBeCloseTo(0);
  });

  it("is the identity for same-frame re-expression", () => {
    expect(r.reexpress([1, 2, 3], "ICRF", "ICRF", 0n)).toEqual([1, 2, 3]);
  });
});
