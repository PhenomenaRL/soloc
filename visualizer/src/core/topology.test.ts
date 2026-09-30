import { describe, expect, it } from "vitest";
import { TransformTopology } from "./topology";

const row = (entityId: string, frameId: string, epochNs: bigint) => ({ entityId, frameId, epochNs });

describe("TransformTopology.isPosed", () => {
  // Terminal frames are recognised structurally: an id with no rows is an
  // anchor the ledger cannot pose. Under prescribed ids a body is its own
  // body-fixed frame, so there is nothing a name-shaped test could tell apart.
  it("separates posed entities from terminal anchors", () => {
    const t = TransformTopology.fromRows([
      row("demo:rover", "moon", 100n),
      row("moon", "ICRF", 100n),
    ]);
    expect(t.isPosed("demo:rover")).toBe(true);
    expect(t.isPosed("moon")).toBe(true);
    expect(t.isPosed("ICRF")).toBe(false);
  });

  it("stops a chain at the first unposed parent", () => {
    const t = TransformTopology.fromRows([
      row("demo:rover", "moon", 100n),
      row("moon", "ICRF", 100n),
    ]);
    expect(t.chainAt("demo:rover", 100n)).toEqual(["demo:rover", "moon", "ICRF"]);
    expect(t.roots()).toEqual(["ICRF"]);
  });
});

describe("TransformTopology.ingest", () => {
  // Mirrors Rust test_first_sighting_creates_edges.
  it("first sighting creates one event per entity", () => {
    const t = TransformTopology.fromRows([
      row("demo:truck", "IAU_EARTH", 100n),
      row("demo:sat", "ICRF", 100n),
    ]);
    expect(t.events).toHaveLength(2);
    expect(t.currentParent("demo:truck")).toBe("IAU_EARTH");
    expect(t.currentParent("demo:sat")).toBe("ICRF");
  });

  // Mirrors Rust test_steady_state_emits_no_events.
  it("steady state emits no events on later ingests", () => {
    const t = TransformTopology.fromRows([row("demo:sat", "ICRF", 100n)]);
    const emitted = t.ingest([row("demo:sat", "ICRF", 200n), row("demo:sat", "ICRF", 300n)]);
    expect(emitted).toHaveLength(0);
    expect(t.events).toHaveLength(1);
  });

  it("captures a multi-hop re-parent inside one ingest, at the right epochs", () => {
    const t = TransformTopology.fromRows([
      row("demo:ship", "naif:399", 100n),
      row("demo:ship", "naif:399", 200n),
      row("demo:ship", "naif:301", 300n),
      row("demo:ship", "IAU_MOON", 400n),
    ]);
    expect(t.events.map((e) => [e.parentId, e.epochNs])).toEqual([
      ["naif:399", 100n],
      ["naif:301", 300n],
      ["IAU_MOON", 400n],
    ]);
  });

  it("sorts rows by epoch before walking (batch order is not trusted)", () => {
    const t = TransformTopology.fromRows([
      row("demo:ship", "naif:301", 300n), // out of order on purpose
      row("demo:ship", "naif:399", 100n),
    ]);
    expect(t.events.map((e) => e.parentId)).toEqual(["naif:399", "naif:301"]);
    expect(t.currentParent("demo:ship")).toBe("naif:301");
  });
});

describe("historical queries", () => {
  const t = TransformTopology.fromRows([
    row("demo:ship", "naif:399", 100n),
    row("demo:ship", "naif:301", 300n),
    row("naif:301", "naif:399", 100n),
    row("naif:399", "ICRF", 100n),
  ]);

  it("parentAt replays history", () => {
    expect(t.parentAt("demo:ship", 50n)).toBeUndefined();
    expect(t.parentAt("demo:ship", 100n)).toBe("naif:399");
    expect(t.parentAt("demo:ship", 299n)).toBe("naif:399");
    expect(t.parentAt("demo:ship", 300n)).toBe("naif:301");
    expect(t.parentAt("demo:ship", 999n)).toBe("naif:301");
  });

  it("chainAt walks to the astronomical root and changes with time", () => {
    expect(t.chainAt("demo:ship", 200n)).toEqual(["demo:ship", "naif:399", "ICRF"]);
    expect(t.chainAt("demo:ship", 400n)).toEqual(["demo:ship", "naif:301", "naif:399", "ICRF"]);
  });

  it("roots reports terminal frames only", () => {
    expect(t.roots()).toEqual(["ICRF"]);
  });
});
