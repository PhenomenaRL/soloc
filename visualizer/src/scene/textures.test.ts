import { describe, expect, it } from "vitest";
import { bodySphereGeometry } from "./textures";

/**
 * The vertex whose UV is nearest `(u, v)`, as `[x, y, z]` on a unit sphere.
 *
 * Reading the geometry back is the only honest way to pin this down: the claim
 * is about where a *pixel of the map* ends up in body-fixed axes, and that is
 * exactly the UV → position relation.
 */
function atUV(u: number, v: number): [number, number, number] {
  const geom = bodySphereGeometry(1, 64);
  const pos = geom.getAttribute("position");
  const uv = geom.getAttribute("uv");
  let best = 0;
  let bestD = Infinity;
  for (let i = 0; i < uv.count; i++) {
    const d = Math.hypot(uv.getX(i) - u, uv.getY(i) - v);
    if (d < bestD) {
      bestD = d;
      best = i;
    }
  }
  return [pos.getX(best), pos.getY(best), pos.getZ(best)];
}

const near = (got: readonly number[], want: readonly number[]) => {
  got.forEach((g, i) => expect(g).toBeCloseTo(want[i]!, 5));
};

describe("bodySphereGeometry maps an equirectangular texture to a body-fixed frame", () => {
  it("puts the top of the map on the +Z rotation pole, not +Y", () => {
    // The bug this exists to prevent: a raw SphereGeometry poles on +Y, which
    // in a Z-up body frame points the north pole out through the equator.
    near(atUV(0.5, 1), [0, 0, 1]);
    near(atUV(0.5, 0), [0, 0, -1]);
  });

  it("puts the prime meridian at the middle of the map, on +X", () => {
    near(atUV(0.5, 0.5), [1, 0, 0]);
  });

  it("runs longitude east with u, so 90° E lands on +Y", () => {
    // Right-handed about +Z: lon 0 = +X, lon 90° E = +Y. A map with longitude
    // increasing left-to-right therefore has u = 0.75 on +Y.
    near(atUV(0.75, 0.5), [0, 1, 0]);
    near(atUV(0.25, 0.5), [0, -1, 0]);
  });

  it("scales to the requested radius", () => {
    const geom = bodySphereGeometry(1737.4, 16);
    const pos = geom.getAttribute("position");
    expect(Math.hypot(pos.getX(0), pos.getY(0), pos.getZ(0))).toBeCloseTo(1737.4, 3);
  });
});
