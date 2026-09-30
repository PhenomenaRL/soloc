/**
 * Evaluates world poses at arbitrary epochs straight from the timelines —
 * independent of whatever instant the scene graph currently displays. Used to
 * re-express a pose from one frame into another *at a specific epoch*, which
 * is what makes frame hand-offs render as pursuit rather than teleportation.
 */

import type { EntityTimeline } from "./timeline";
import { conjQ, mulQ, rotateVec, type Quat, type Vec3 } from "./quat";

export interface FramePose {
  p: Vec3;
  q: Quat;
}

const IDENTITY: FramePose = { p: [0, 0, 0], q: [1, 0, 0, 0] };

export class WorldResolver {
  constructor(private readonly timelines: Map<string, EntityTimeline>) {}

  /**
   * World pose of the frame `id` names at `epoch`.
   *
   * A prescribed id that nothing in the ledger poses is a terminal anchor
   * (ICRF and friends) and resolves to identity. A body-fixed frame needs no
   * special case: it *is* its body's id, so it resolves through the body's own
   * rows — orientation included, which is what puts the rover on a Moon that
   * really turns.
   */
  worldPoseAt(id: string, epoch: bigint, depth = 0): FramePose {
    if (depth > 16) return IDENTITY; // cycle guard; validated data never hits it
    const tl = this.timelines.get(id);
    if (!tl) return IDENTITY;
    const st = tl.at(epoch); // hand-offs resolve to the held pose — fine here
    const parent = this.worldPoseAt(st.frame, epoch, depth + 1);
    return {
      p: add(parent.p, rotateVec(parent.q, st.positionKm)),
      q: mulQ(parent.q, st.quaternion),
    };
  }

  /** Re-expresses a point (km, in `fromFrame`) into `toFrame` coordinates at `epoch`. */
  reexpress(pointKm: Vec3, fromFrame: string, toFrame: string, epoch: bigint): Vec3 {
    if (fromFrame === toFrame) return pointKm;
    const a = this.worldPoseAt(fromFrame, epoch);
    const b = this.worldPoseAt(toFrame, epoch);
    const world = add(a.p, rotateVec(a.q, pointKm));
    return rotateVec(conjQ(b.q), sub(world, b.p));
  }
}

const add = (a: Vec3, b: Vec3): Vec3 => [a[0] + b[0], a[1] + b[1], a[2] + b[2]];
const sub = (a: Vec3, b: Vec3): Vec3 => [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
