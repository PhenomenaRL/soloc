/**
 * Row-derived transform topology — a TypeScript mini-port of
 * `spacetimestamp::topology::TransformTree`.
 *
 * Same semantics, simpler machinery: rows are grouped per prescribed id, sorted
 * by epoch (stable, so equal epochs keep row order — first row wins, matching
 * the Rust tie-break), and walked in order, emitting a `TopologyEvent` at every
 * parent change. Frame validation and cycle detection are deliberately omitted:
 * the browser reads fixtures that a real `Ledger` already validated on append.
 *
 * Terminal frames are recognised structurally rather than by name: an id this
 * tree has never seen a row for is an anchor the ledger cannot pose (ICRF and
 * friends), and the chain stops there. Under prescribed ids a body *is* its own
 * body-fixed frame, so `IAU_MOON` is the Moon's own id and resolves against the
 * Moon's own rows — there is no host table to consult, and nothing left that a
 * name-shaped test could tell apart.
 */

import { cmpNs } from "./epoch";

export interface PoseRowLike {
  entityId: string;
  frameId: string;
  epochNs: bigint;
}

/** A single parent change: `childId` became parented to `parentId` at `epochNs`. */
export interface TopologyEvent {
  childId: string;
  parentId: string;
  epochNs: bigint;
}

export class TransformTopology {
  /** Every parent change, in (childId, epoch) order — mirrors `IngestOutcome.events`. */
  readonly events: TopologyEvent[] = [];

  private latest = new Map<string, { parent: string; sinceNs: bigint }>();

  static fromRows(rows: readonly PoseRowLike[]): TransformTopology {
    const t = new TransformTopology();
    t.ingest(rows);
    return t;
  }

  /** Derives parent-change events from `rows` and applies them to the tree. */
  ingest(rows: readonly PoseRowLike[]): TopologyEvent[] {
    const byId = new Map<string, PoseRowLike[]>();
    for (const r of rows) {
      const list = byId.get(r.entityId);
      if (list) list.push(r);
      else byId.set(r.entityId, [r]);
    }

    const emitted: TopologyEvent[] = [];
    for (const id of [...byId.keys()].sort()) {
      // Array.prototype.sort is stable: equal epochs keep batch order.
      const sorted = byId.get(id)!.slice().sort((a, b) => cmpNs(a.epochNs, b.epochNs));

      // Start each id from the parent it had *before* this ingest.
      let running = this.latest.get(id)?.parent;
      for (const row of sorted) {
        if (row.frameId === running) continue;
        running = row.frameId;
        const event = { childId: id, parentId: row.frameId, epochNs: row.epochNs };
        emitted.push(event);
        this.events.push(event);
        this.latest.set(id, { parent: row.frameId, sinceNs: row.epochNs });
      }
    }
    return emitted;
  }

  /** All entity ids with a known parent. */
  ids(): string[] {
    return [...this.latest.keys()];
  }

  /**
   * `true` if this tree has rows posing `id` — i.e. it is an entity, not a
   * terminal anchor. The structural replacement for the old "does the name look
   * like a URI" test.
   */
  isPosed(id: string): boolean {
    return this.latest.has(id);
  }

  /** `childId`'s current parent, ignoring history. */
  currentParent(childId: string): string | undefined {
    return this.latest.get(childId)?.parent;
  }

  /** `childId`'s parent as of `epochNs`, or `undefined` if it had none yet. */
  parentAt(childId: string, epochNs: bigint): string | undefined {
    const l = this.latest.get(childId);
    if (!l) return undefined;
    if (l.sinceNs <= epochNs) return l.parent;
    for (let i = this.events.length - 1; i >= 0; i--) {
      const e = this.events[i]!;
      if (e.childId === childId && e.epochNs <= epochNs) return e.parentId;
    }
    return undefined;
  }

  /**
   * Chain of node ids from `entityId` up to its terminal frame as of `epochNs`.
   * First element is `entityId`; the last is a terminal anchor id (or the
   * deepest resolvable node). Guarded against cycles for defensiveness even
   * though validated fixtures cannot contain one.
   */
  chainAt(entityId: string, epochNs: bigint): string[] {
    const chain = [entityId];
    const seen = new Set(chain);
    let node = entityId;
    for (;;) {
      const parent = this.parentAt(node, epochNs);
      if (parent === undefined || seen.has(parent)) break;
      chain.push(parent);
      if (!this.latest.has(parent)) break; // terminal anchor: nothing poses it
      seen.add(parent);
      node = parent;
    }
    return chain;
  }

  /** Distinct terminal frames (non-entity parents) present in the tree right now. */
  roots(): string[] {
    const roots = new Set<string>();
    for (const { parent } of this.latest.values()) {
      if (!this.latest.has(parent)) roots.add(parent);
    }
    return [...roots].sort();
  }
}
