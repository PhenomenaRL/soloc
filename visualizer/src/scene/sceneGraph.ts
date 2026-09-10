/**
 * Scene graph = transform tree, 1:1.
 *
 * Every entity gets a `THREE.Group` attached to its parent's group; a frame the
 * ledger has no rows for is terminal and maps to the world root. A body-fixed
 * frame needs no host lookup — under prescribed ids it *is* the body's own id,
 * so it finds the body's group by the ordinary path. Poses are set from each
 * entity's row in its *native* frame
 * and units, converted to km at the node — nothing is ever flattened to a
 * global frame, mirroring soloc's "store raw, reproject on demand".
 *
 * Renderer-free on purpose: meshes, lines, and labels are decorated on top by
 * the viewer, so this module is unit-testable in Node.
 */

import { Group, Quaternion } from "three";
import type { EntityRow } from "../arrow/loader";
import type { TransformTopology } from "../core/topology";
import { posToKm } from "./units";

export interface EntityNode {
  id: string;
  group: Group;
  /** The row currently posing this node. */
  row: EntityRow;
  /** Chain of ids up to the terminal frame, at the posed epoch. */
  chain: string[];
}

export interface SceneGraph {
  /** World root — the inertial frame (ICRF and friends). */
  root: Group;
  nodes: Map<string, EntityNode>;
  /** Resolves a frame id to its group; anything unposed lands on the root. */
  frameGroup(frame: string): Group;
  /** Re-pose one entity from a row (used by playback later). */
  pose(id: string, row: EntityRow): void;
  /** Re-attach an entity to a new parent frame (topology change during playback). */
  reparent(id: string, parentFrame: string): void;
}

export function buildSceneGraph(
  entities: Map<string, EntityRow[]>,
  topo: TransformTopology,
  epochNs: bigint,
): SceneGraph {
  const root = new Group();
  root.name = "ICRF";
  const nodes = new Map<string, EntityNode>();

  /** Latest row at or before `epochNs`; falls back to the first row. */
  function rowAt(id: string): EntityRow | undefined {
    const rows = entities.get(id);
    if (!rows || rows.length === 0) return undefined;
    let candidate = rows[0]!;
    for (const r of rows) {
      if (r.epochNs > epochNs) break; // rows are epoch-sorted
      candidate = r;
    }
    return candidate;
  }

  function ensureNode(id: string): EntityNode | undefined {
    const existing = nodes.get(id);
    if (existing) return existing;

    const row = rowAt(id);
    if (!row) return undefined;

    const parent = topo.parentAt(id, epochNs) ?? row.frameId;
    // An unposed parent is a terminal anchor (ICRF, …): the world root.
    const parentGroup = ensureNode(parent)?.group ?? root;

    const group = new Group();
    group.name = id;
    applyPose(group, row);
    parentGroup.add(group);

    const node: EntityNode = { id, group, row, chain: topo.chainAt(id, epochNs) };
    nodes.set(id, node);
    return node;
  }

  for (const id of [...entities.keys()].sort()) ensureNode(id);

  const frameGroup = (frame: string): Group => nodes.get(frame)?.group ?? root;

  return {
    root,
    nodes,
    frameGroup,
    pose(id, row) {
      const node = nodes.get(id);
      if (!node) return;
      applyPose(node.group, row);
      node.row = row;
    },
    reparent(id, parentFrame) {
      const node = nodes.get(id);
      if (!node) return;
      const target = frameGroup(parentFrame);
      if (node.group.parent !== target) target.add(node.group); // caller re-poses next
      node.chain = [id, parentFrame];
    },
  };
}

function applyPose(group: Group, row: EntityRow): void {
  const [x, y, z] = posToKm(row.position, row.unitsPos);
  group.position.set(x, y, z);
  const [w, qx, qy, qz] = row.quaternion;
  group.quaternion.copy(new Quaternion(qx, qy, qz, w).normalize());
}
