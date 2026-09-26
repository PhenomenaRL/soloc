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
 * A sphere whose equirectangular texture lines up with an IAU body-fixed frame.
 *
 * `SphereGeometry` is built Y-up: its poles sit on **+Y** and its `u = 0` seam
 * on -X. A body-fixed frame is Z-up — the rotation pole is **+Z**, which is why
 * this app sets `camera.up` to `(0, 0, 1)` and why the fixture places surface
 * points at `z = R·sin(lat)`. Dropping a raw SphereGeometry into a body's node
 * therefore lays the map on its side, pointing the north pole out through the
 * equator. Harmless while a planet is a distant dot; not harmless once there is
 * a base at 5°N 20°W to look for.
 *
 * Rotating +90° about X carries +Y to +Z and fixes it, and the longitudes then
 * fall out right for a standard map with the prime meridian down the middle:
 * `u = 0.5` lands on +X (longitude 0) and `u = 0.75` on +Y (90° east). Baked
 * into the vertex data rather than set on the mesh, so nothing downstream has
 * to know the mesh carries a rotation of its own.
 */
export function bodySphereGeometry(radiusKm: number, widthSegments = 48): SphereGeometry {
  const geom = new SphereGeometry(radiusKm, widthSegments, widthSegments >> 1);
  geom.rotateX(Math.PI / 2);
  return geom;
}

/**
 * Candidate files for one texture slot, **best first**.
 *
 * Only the last name is guaranteed to exist: it is the CDN baseline. The ones
 * ahead of it are local drop-ins, so a higher-resolution map is installed by
 * putting a file in `public/textures/` — no code change, and no pretending an
 * 8192-pixel map is called `moonmap1k.jpg`.
 */
type Candidates = readonly [...string[], string];

/**
 * The drop-in ladder for one texture slot: 8k, then 4k, then 2k, then whatever
 * the CDN ships as `baseline`.
 *
 * Named by *actual* pixel size, not by the vendor's download tier — Solar
 * System Scope labels several 4096×2048 maps "8k", and a ladder that repeats
 * the vendor's claim would pick the wrong file first.
 */
const ladder = (stem: string, baseline: string): Candidates =>
  [`${stem}8k.jpg`, `${stem}4k.jpg`, `${stem}2k.jpg`, baseline] as const;

/**
 * Keyed by display name (see `scene/registry.ts`), not by prescribed id: the
 * ids are 16 opaque bytes and a hex table here would be unreadable.
 *
 * Every body takes a ladder, so a higher-resolution map is installed by putting
 * a file in `public/textures/` — no code change, and no pretending an 8192-pixel
 * map is called `moonmap1k.jpg`.
 */
export const BODY_TEXTURES: Readonly<
  Record<string, { map: Candidates; bump?: Candidates }>
> = {
  Sun: { map: ladder("sunmap", "sunmap.jpg") },
  Mercury: {
    map: ladder("mercurymap", "mercurymap.jpg"),
    bump: ladder("mercurybump", "mercurybump.jpg"),
  },
  Venus: { map: ladder("venusmap", "venusmap.jpg"), bump: ladder("venusbump", "venusbump.jpg") },
  Earth: { map: ladder("earthmap", "earthmap1k.jpg"), bump: ladder("earthbump", "earthbump1k.jpg") },
  MARS_BARYCENTER: {
    map: ladder("marsmap", "marsmap1k.jpg"),
    bump: ladder("marsbump", "marsbump1k.jpg"),
  },
  JUPITER_BARYCENTER: { map: ladder("jupitermap", "jupitermap.jpg") },
  SATURN_BARYCENTER: { map: ladder("saturnmap", "saturnmap.jpg") },
  URANUS_BARYCENTER: { map: ladder("uranusmap", "uranusmap.jpg") },
  NEPTUNE_BARYCENTER: { map: ladder("neptunemap", "neptunemap.jpg") },
  Moon: { map: ladder("moonmap", "moonmap1k.jpg"), bump: ladder("moonbump", "moonbump1k.jpg") },
};

/**
 * Saturn's rings as a single RGBA image — colour and transparency in one file,
 * which is how Solar System Scope ships them. The `threex` baseline instead
 * splits them across `saturnringcolor.jpg` and `saturnringpattern.gif`, so the
 * two are handled separately rather than laddered together.
 */
const SATURN_RING_RGBA = "saturnring8k.png";

/** `true` if `file` is present in `public/textures/`. */
async function hasLocal(file: string): Promise<boolean> {
  try {
    const r = await fetch(LOCAL + file, { method: "HEAD" });
    // A dev server answers a missing path with index.html and a 200, so the
    // status alone proves nothing — the content type is what settles it.
    return r.ok && !(r.headers.get("content-type") ?? "").includes("text/html");
  } catch {
    return false;
  }
}

/**
 * Every URL worth trying for a texture slot, best first.
 *
 * Local drop-ins that exist, in the order they were listed, then the CDN copy
 * of the baseline name as the last resort.
 */
async function candidateUrls(files: Candidates): Promise<string[]> {
  // Probed together: every body carries a ladder now, so this runs some fifty
  // times at startup and sequential round-trips would be felt.
  const present = await Promise.all(files.map(hasLocal));
  const urls = files.filter((_, i) => present[i]!).map((file) => LOCAL + file);
  urls.push(CDN + files[files.length - 1]);
  return urls;
}

/**
 * Loads the best texture available for a slot, falling past any that fail.
 *
 * Existing is not the same as *working*: a half-finished download is a file of
 * the right name that decodes to nothing, and trusting the filename there costs
 * the whole body its texture — the colour map and the bump map are awaited
 * together, so one bad file blanks both. Trying each in turn and taking the
 * first that actually decodes means a broken drop-in degrades to the CDN map
 * rather than to a flat grey sphere.
 */
export async function loadTexture(files: Candidates | string, srgb = true): Promise<Texture> {
  const urls = await candidateUrls(typeof files === "string" ? [files] : files);
  let lastError: unknown;
  for (const url of urls) {
    try {
      const tex = await loader.loadAsync(url);
      if (srgb) tex.colorSpace = SRGBColorSpace;
      // Grazing angles are the whole point near a surface: without anisotropic
      // filtering an 8k map viewed along the ground blurs back to worse than 1k.
      tex.anisotropy = maxAnisotropy;
      return tex;
    } catch (e) {
      lastError = e;
    }
  }
  throw lastError ?? new Error(`no texture could be loaded from ${urls.join(", ")}`);
}

/**
 * Anisotropy cap for the running GPU, set once by the viewer.
 *
 * Kept as a module-level value rather than threaded through every call site:
 * it is a property of the device, identical for every texture, and known only
 * after a renderer exists.
 */
let maxAnisotropy = 1;

export function setMaxAnisotropy(n: number): void {
  maxAnisotropy = Math.max(1, n);
}

/**
 * How strongly a bump map perturbs the surface normal.
 *
 * Unitless, and three.js calibrates it around 1 — it is *not* a height in any
 * physical unit. Scaling it by the body radius (as this once did) hands the
 * Moon 34.7 and Jupiter 1398, which stamps the bump map's every JPEG artifact
 * into a crater and destroys the shading. Invisible while a planet is a distant
 * dot; the first thing you see once you fly down to one. Taste value — turn it
 * up for more relief, but keep it near 1.
 */
const BUMP_SCALE = 0.5;

/** Swaps a body's flat-color material for its texture set once loaded. */
export async function applyBodyTextures(mesh: Mesh, bodyKey: string): Promise<void> {
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
      mat.bumpScale = BUMP_SCALE;
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
    // One RGBA file carries its own transparency; the two-file baseline needs
    // the alpha supplied separately as an alphaMap.
    const mat = (await hasLocal(SATURN_RING_RGBA))
      ? new MeshStandardMaterial({
          map: await loadTexture([SATURN_RING_RGBA]),
          transparent: true,
          side: DoubleSide,
          roughness: 1,
        })
      : new MeshStandardMaterial({
          map: await loadTexture("saturnringcolor.jpg"),
          alphaMap: await loadTexture("saturnringpattern.gif", false),
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
    return new Mesh(bodySphereGeometry(radiusKm * 1.006), mat);
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
    return new Mesh(bodySphereGeometry(radiusKm), mat);
  } catch {
    return null;
  }
}

/** One `<img>` load, resolving only if the bytes actually decode. */
function decodeImage(url: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.crossOrigin = "anonymous";
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error(`failed to load ${url}`));
    img.src = url;
  });
}

/** Same candidate-and-fall-through contract as {@link loadTexture}. */
async function loadImage(file: string): Promise<HTMLImageElement> {
  let lastError: unknown;
  for (const url of await candidateUrls([file])) {
    try {
      return await decodeImage(url);
    } catch (e) {
      lastError = e;
    }
  }
  throw lastError ?? new Error(`failed to load ${file}`);
}
