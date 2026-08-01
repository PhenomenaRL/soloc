/**
 * Texture assets for celestial bodies.
 *
 * Resolution order per file: `/textures/<file>` (drop-in local override, see
 * public/textures/README.md) → jsDelivr CDN mirror of `threex.planets`.
 * Everything loads async and swaps into materials when ready, so the map is
 * fully usable (flat colors) offline or before load.
 *
 * Texture credit: James Hastings-Trew — planetpixelemporium.com (free to use
 * in projects, not for redistribution as textures) — via the long-lived
 * jeromeetienne/threex.planets repository.
 */

import {
  AdditiveBlending,
  BackSide,
  CanvasTexture,
  DoubleSide,
  Mesh,
  MeshBasicMaterial,
  MeshStandardMaterial,
  RingGeometry,
  SRGBColorSpace,
  SphereGeometry,
  Sprite,
  SpriteMaterial,
  Texture,
  TextureLoader,
} from "three";

const CDN = "https://cdn.jsdelivr.net/gh/jeromeetienne/threex.planets@master/images/";
const LOCAL = "/textures/";

const loader = new TextureLoader();
loader.setCrossOrigin("anonymous");

/**
 * Keyed by display name (see `scene/registry.ts`), not by prescribed id: the
 * ids are 16 opaque bytes and a hex table here would be unreadable.
 */
export const BODY_TEXTURES: Readonly<Record<string, { map: string; bump?: string }>> = {
  Sun: { map: "sunmap.jpg" },
  Mercury: { map: "mercurymap.jpg", bump: "mercurybump.jpg" },
  Venus: { map: "venusmap.jpg", bump: "venusbump.jpg" },
  Earth: { map: "earthmap1k.jpg", bump: "earthbump1k.jpg" },
  MARS_BARYCENTER: { map: "marsmap1k.jpg", bump: "marsbump1k.jpg" },
  JUPITER_BARYCENTER: { map: "jupitermap.jpg" },
  SATURN_BARYCENTER: { map: "saturnmap.jpg" },
  URANUS_BARYCENTER: { map: "uranusmap.jpg" },
  NEPTUNE_BARYCENTER: { map: "neptunemap.jpg" },
  Moon: { map: "moonmap1k.jpg", bump: "moonbump1k.jpg" },
};

async function resolveUrl(file: string): Promise<string> {
  try {
    const r = await fetch(LOCAL + file, { method: "HEAD" });
    const type = r.headers.get("content-type") ?? "";
    if (r.ok && !type.includes("text/html")) return LOCAL + file;
  } catch {
    /* no local override — use CDN */
  }
  return CDN + file;
}

export async function loadTexture(file: string, srgb = true): Promise<Texture> {
  const tex = await loader.loadAsync(await resolveUrl(file));
  if (srgb) tex.colorSpace = SRGBColorSpace;
  return tex;
}

/** Swaps a body's flat-color material for its texture set once loaded. */
export async function applyBodyTextures(
  mesh: Mesh,
  bodyKey: string,
  radiusKm: number,
): Promise<void> {
  const entry = BODY_TEXTURES[bodyKey];
  if (!entry) return;
  const mat = mesh.material as MeshStandardMaterial | MeshBasicMaterial;
  try {
    const [map, bump] = await Promise.all([
      loadTexture(entry.map),
      entry.bump ? loadTexture(entry.bump, false) : Promise.resolve(null),
    ]);
    mat.map = map;
    mat.color.set(0xffffff);
    if (bump && "bumpMap" in mat) {
      mat.bumpMap = bump;
      mat.bumpScale = radiusKm * 0.02;
    }
    mat.needsUpdate = true;
  } catch {
    /* offline / CDN blocked: keep the flat color */
  }
}

/** Saturn's rings: real proportions, radial UVs, alpha from the ring pattern. */
export async function makeSaturnRings(radiusKm: number): Promise<Mesh | null> {
  try {
    const inner = radiusKm * 1.24;
    const outer = radiusKm * 2.27;
    const geom = new RingGeometry(inner, outer, 192, 1);
    // Remap UVs so u runs along the radius (RingGeometry's default is planar).
    const pos = geom.attributes.position!;
    const uv = geom.attributes.uv!;
    for (let i = 0; i < pos.count; i++) {
      const r = Math.hypot(pos.getX(i), pos.getY(i));
      uv.setXY(i, (r - inner) / (outer - inner), 0.5);
    }
    const [map, alpha] = await Promise.all([
      loadTexture("saturnringcolor.jpg"),
      loadTexture("saturnringpattern.gif", false),
    ]);
    const mat = new MeshStandardMaterial({
      map,
      alphaMap: alpha,
      transparent: true,
      side: DoubleSide,
      roughness: 1,
    });
    return new Mesh(geom, mat);
  } catch {
    return null;
  }
}

/** Semi-transparent cloud shell for Earth (color map + inverted transparency map). */
export async function makeEarthClouds(radiusKm: number): Promise<Mesh | null> {
  try {
    const [mapImg, transImg] = await Promise.all([
      loadImage("earthcloudmap.jpg"),
      loadImage("earthcloudmaptrans.jpg"),
    ]);
    const canvas = document.createElement("canvas");
    canvas.width = mapImg.width;
    canvas.height = mapImg.height;
    const ctx = canvas.getContext("2d")!;
    ctx.drawImage(mapImg, 0, 0);
    const rgb = ctx.getImageData(0, 0, canvas.width, canvas.height);
    ctx.drawImage(transImg, 0, 0, canvas.width, canvas.height);
    const trans = ctx.getImageData(0, 0, canvas.width, canvas.height);
    for (let i = 0; i < rgb.data.length; i += 4) {
      rgb.data[i + 3] = 255 - trans.data[i]!; // white in trans map = transparent
    }
    ctx.putImageData(rgb, 0, 0);
    const tex = new CanvasTexture(canvas);
    tex.colorSpace = SRGBColorSpace;
    const mat = new MeshStandardMaterial({ map: tex, transparent: true, roughness: 1 });
    return new Mesh(new SphereGeometry(radiusKm * 1.006, 48, 24), mat);
  } catch {
    return null;
  }
}

/** Procedural additive glow sprite for the Sun (no asset needed). */
export function makeSunGlow(radiusKm: number): Sprite {
  const size = 256;
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = size;
  const ctx = canvas.getContext("2d")!;
  const g = ctx.createRadialGradient(size / 2, size / 2, 0, size / 2, size / 2, size / 2);
  g.addColorStop(0, "rgba(255, 240, 200, 0.55)");
  g.addColorStop(0.35, "rgba(255, 205, 110, 0.22)");
  g.addColorStop(1, "rgba(255, 180, 80, 0)");
  ctx.fillStyle = g;
  ctx.fillRect(0, 0, size, size);
  const sprite = new Sprite(
    new SpriteMaterial({ map: new CanvasTexture(canvas), blending: AdditiveBlending, depthWrite: false }),
  );
  sprite.scale.setScalar(radiusKm * 8);
  return sprite;
}

/** Milky Way background sphere; resolves to null if the asset can't load. */
export async function makeStarfieldSphere(radiusKm: number): Promise<Mesh | null> {
  try {
    const tex = await loadTexture("galaxy_starfield.png");
    const mat = new MeshBasicMaterial({ map: tex, side: BackSide, depthWrite: false });
    return new Mesh(new SphereGeometry(radiusKm, 48, 24), mat);
  } catch {
    return null;
  }
}

function loadImage(file: string): Promise<HTMLImageElement> {
  return resolveUrl(file).then(
    (url) =>
      new Promise((resolvePromise, reject) => {
        const img = new Image();
        img.crossOrigin = "anonymous";
        img.onload = () => resolvePromise(img);
        img.onerror = () => reject(new Error(`failed to load ${url}`));
        img.src = url;
      }),
  );
}
