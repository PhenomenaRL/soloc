/** Minimal quaternion math on `[w, x, y, z]` tuples (hifitime/soloc convention). */

export type Quat = [number, number, number, number];
export type Vec3 = [number, number, number];

export function mulQ(a: Quat, b: Quat): Quat {
  const [aw, ax, ay, az] = a;
  const [bw, bx, by, bz] = b;
  return [
    aw * bw - ax * bx - ay * by - az * bz,
    aw * bx + ax * bw + ay * bz - az * by,
    aw * by - ax * bz + ay * bw + az * bx,
    aw * bz + ax * by - ay * bx + az * bw,
  ];
}

export function conjQ(q: Quat): Quat {
  return [q[0], -q[1], -q[2], -q[3]];
}

/** Rotates `v` by unit quaternion `q`. */
export function rotateVec(q: Quat, v: Vec3): Vec3 {
  const [w, x, y, z] = q;
  // t = 2 * (q.xyz × v); v' = v + w*t + q.xyz × t
  const tx = 2 * (y * v[2] - z * v[1]);
  const ty = 2 * (z * v[0] - x * v[2]);
  const tz = 2 * (x * v[1] - y * v[0]);
  return [
    v[0] + w * tx + (y * tz - z * ty),
    v[1] + w * ty + (z * tx - x * tz),
    v[2] + w * tz + (x * ty - y * tx),
  ];
}
