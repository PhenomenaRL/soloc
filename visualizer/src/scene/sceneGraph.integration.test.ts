/**
 * Scene graph structure against the real fixture: the Object3D hierarchy must
 * mirror the transform tree, with native units converted per node.
 *
 * Skipped rather than failed when the fixture is absent — see the note in
 * `arrow/loader.integration.test.ts`.
 */

import { Vector3 } from "three";
import { describe, expect, it } from "vitest";
import { TransformTopology } from "../core/topology";
import { fixture, hasFixture } from "../testing/fixture";
import { buildSceneGraph, type SceneGraph } from "./sceneGraph";
import { guideLine } from "./orbits";

/** The graph at the fixture's last epoch, plus the lookups a test needs. */
function scene(): {
  graph: SceneGraph;
  idOf: (key: string) => string;
  worldPos: (key: string) => Vector3;
} {
  const { data, idOf } = fixture();
  const topo = TransformTopology.fromRows(data.rows);
  const graph = buildSceneGraph(data.entities, topo, data.maxEpochNs);
  const worldPos = (key: string) => {
    graph.root.updateMatrixWorld(true);
    return graph.nodes.get(idOf(key))!.group.getWorldPosition(new Vector3());
  };
  return { graph, idOf, worldPos };
}

describe.skipIf(!hasFixture)("scene graph mirrors the transform tree", () => {
  it("creates a node per entity", () => {
    expect(scene().graph.nodes.size).toBe(15);
  });

  it("parents groups exactly like the topology (latest epoch)", () => {
    const { graph, idOf } = scene();
    const parentName = (key: string) => graph.nodes.get(idOf(key))!.group.parent?.name;
    // Snapshot rows are all expressed in ICRF, so every body hangs off the root.
    expect(parentName("Earth")).toBe("ICRF");
    expect(parentName("Moon")).toBe("ICRF");
    // Entity → entity edges: the two scripted re-parents, at the last epoch.
    expect(parentName("demo:spaceship-1")).toBe(idOf("Moon")); // post re-parent
    expect(parentName("demo:miner-1")).toBe(idOf("demo:asteroid-1")); // docked
    // Three deep: the rover hangs off the base, which hangs off the Moon.
    expect(parentName("demo:moon-base-1")).toBe(idOf("Moon"));
    expect(parentName("demo:rover-1")).toBe(idOf("demo:moon-base-1"));
  });

  it("places the Moon a real lunar distance from Earth in world space", () => {
    const { worldPos } = scene();
    const d = worldPos("Moon").distanceTo(worldPos("Earth"));
    expect(d).toBeGreaterThan(3.5e5); // perigee ≈ 356 500 km
    expect(d).toBeLessThan(4.1e5); // apogee ≈ 406 700 km
  });

  it("converts native units at the node: base sits at lunar radius in km", () => {
    const { worldPos } = scene();
    const d = worldPos("demo:moon-base-1").distanceTo(worldPos("Moon"));
    expect(d).toBeCloseTo(1_737.4, 0); // metres in data → km in scene
  });

  it("composes two nested frames: the rover ends up on the surface too", () => {
    const { worldPos } = scene();
    // Local-level east/north/up, through the base's orientation, through the
    // Moon's. The curvature drop in the fixture keeps the wheels on the ground.
    const d = worldPos("demo:rover-1").distanceTo(worldPos("Moon"));
    expect(d).toBeCloseTo(1_737.4, 0);
    // ...and it really has driven away from the base — ~12 km over the window.
    const fromBase = worldPos("demo:rover-1").distanceTo(worldPos("demo:moon-base-1"));
    expect(fromBase).toBeGreaterThan(10);
    expect(fromBase).toBeLessThan(14);
  });

  it("docked miner is within ~100 m of the asteroid (mm rows)", () => {
    const { worldPos } = scene();
    const d = worldPos("demo:miner-1").distanceTo(worldPos("demo:asteroid-1"));
    expect(d).toBeGreaterThan(0.05);
    expect(d).toBeLessThan(0.3);
  });

  it("reparent() re-attaches a group", () => {
    const { graph, idOf } = scene();
    graph.reparent(idOf("demo:spaceship-1"), idOf("Earth"));
    expect(graph.nodes.get(idOf("demo:spaceship-1"))!.group.parent?.name).toBe(idOf("Earth"));
  });
});

describe.skipIf(!hasFixture)("guide lines derived from data", () => {
  it("planets get full orbit circles", () => {
    const { data, idOf } = fixture();
    const icrf = data.entities.get(idOf("Earth"))![0]!.frameId;
    expect(guideLine(data.entities.get(idOf("Earth"))!, icrf)!.kind).toBe("circle");
  });

  it("the spaceship spiral is a trail, not a circle", () => {
    const { data, idOf } = fixture();
    const ship = guideLine(data.entities.get(idOf("demo:spaceship-1"))!, idOf("Moon"))!;
    expect(ship.kind).toBe("trail");
  });

  it("the Sun orbits the barycentre rather than sitting at the origin", () => {
    const { data, idOf } = fixture();
    const icrf = data.entities.get(idOf("Earth"))![0]!.frameId;
    // Real ephemeris is solar-system-barycentric, so the Sun has a small orbit
    // of its own — a few hundred thousand km, driven mostly by Jupiter. The old
    // synthetic fixture pinned it at the origin and drew nothing.
    const sun = data.entities.get(idOf("Sun"))!;
    expect(guideLine(sun, icrf)).not.toBeNull();
    const r = Math.hypot(...sun[0]!.position);
    expect(r).toBeGreaterThan(0);
    expect(r).toBeLessThan(3e6); // comfortably inside 0.02 au
  });
});
