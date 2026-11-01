import { describe, expect, it, vi } from "vitest";
import { BufferGeometry, Mesh, MeshStandardMaterial, Texture, TextureLoader } from "three";
import { applyBodyTextures, bodySphereGeometry, earthEllipsoidGeometry, WGS84_A_KM, WGS84_B_KM } from "./textures";

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

describe("WGS84 Earth geometry", () => {
  const geometry = earthEllipsoidGeometry(64);
  function earthAt(u: number, v: number): number[] {
    const uv = geometry.getAttribute("uv");
    const position = geometry.getAttribute("position");
    for (let i = 0; i < uv.count; i++) {
      if (uv.getX(i) === u && uv.getY(i) === v) return [position.getX(i), position.getY(i), position.getZ(i)];
    }
    throw new Error("test UV is not a mesh vertex");
  }

  it("uses the actual equatorial and polar radii in kilometres", () => {
    expect(earthAt(0.5, 0.5)[0]).toBeCloseTo(WGS84_A_KM, 3);
    expect(earthAt(0.5 + 0.5 / 64, 1)[2]).toBeCloseTo(WGS84_B_KM, 3);
    expect(earthAt(0.75, 0.5)[1]).toBeCloseTo(WGS84_A_KM, 3);
  });

  it("puts 45° geodetic north at the GPS ellipsoid coordinate, not a scaled sphere latitude", () => {
    const p = earthAt(0.5, 0.75);
    // WGS84 reference: geodetic latitude45°, longitude0°, ellipsoidal height0.
    expect(p[0]).toBeCloseTo(4517.590878849, 3);
    expect(p[2]).toBeCloseTo(4487.348408866, 3);
    expect(p[0]! ** 2 / WGS84_A_KM ** 2 + p[2]! ** 2 / WGS84_B_KM ** 2).toBeCloseTo(1, 6);
  });
});

it("shows a local Earth map while optional relief is pending and keeps it when relief fails", async () => {
  const map = new Texture();
  const material = new MeshStandardMaterial({ color: 0x5aa9ff });
  const mesh = new Mesh(new BufferGeometry(), material);
  let rejectBump!: (reason: Error) => void;
  const bump = new Promise<Texture>((_resolve, reject) => { rejectBump = reject; });
  vi.stubGlobal("fetch", vi.fn(async (url: string) => new Response(null, {
    status: url === "/textures/earthmap8k.jpg" ? 200 : 404,
    headers: { "content-type": "image/jpeg" },
  })));
  vi.spyOn(TextureLoader.prototype, "loadAsync").mockImplementation((url) => {
    if (url === "/textures/earthmap8k.jpg") return Promise.resolve(map);
    if (url.includes("earthbump")) return bump;
    return Promise.reject(new Error("unavailable texture"));
  });

  try {
    const loading = applyBodyTextures(mesh, "Earth");
    await vi.waitFor(() => expect(material.map).toBe(map));
    expect(material.color.getHex()).toBe(0xffffff);
    expect(material.bumpMap).toBeNull();
    rejectBump(new Error("CDN blocked"));
    await loading;
    expect(material.map).toBe(map);
    expect(material.bumpMap).toBeNull();
  } finally {
    rejectBump(new Error("test cleanup"));
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    mesh.geometry.dispose();
    material.dispose();
    map.dispose();
  }
});
