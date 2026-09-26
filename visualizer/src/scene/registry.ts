/**
 * Display metadata per entity — colors, labels, body radii, framing distances.
 * Purely cosmetic: nothing here feeds back into the data model. Radii are real
 * body radii (km) so the map is true-scale; unknown entities fall back to
 * screen-scaled markers.
 *
 * Keyed by the *name* an id resolves to through the ledger's registry, not by
 * the id itself: a prescribed id is 16 opaque bytes, and hard-coding hex here
 * would make this table unreadable and impossible to check by eye. Names are
 * unique by construction — an id is a pure function of `(kind, authority,
 * name)` — so the key is every bit as stable as the id.
 */

import type { NameBook } from "../core/identity";

export interface DisplayInfo {
  label: string;
  color: string;
  /** Real body radius in km — rendered as a sphere when present. */
  bodyRadiusKm?: number;
  /** Camera distance when focused, km. Defaults derive from radius. */
  focusDistKm?: number;
  kind: "star" | "planet" | "moon" | "asset";
}

/**
 * Mars and the giants appear under their system barycentre, which is what the
 * base DE440s kernels carry; the radius drawn is still the planet's own.
 */
const KNOWN: Record<string, DisplayInfo> = {
  Sun: { label: "Sun", color: "#ffd66b", bodyRadiusKm: 696_000, kind: "star" },
  Mercury: { label: "Mercury", color: "#b5a79b", bodyRadiusKm: 2_440, kind: "planet" },
  Venus: { label: "Venus", color: "#e6c98f", bodyRadiusKm: 6_052, kind: "planet" },
  Earth: { label: "Earth", color: "#5aa9ff", bodyRadiusKm: 6_371, kind: "planet" },
  MARS_BARYCENTER: { label: "Mars", color: "#e07b5a", bodyRadiusKm: 3_390, kind: "planet" },
  JUPITER_BARYCENTER: { label: "Jupiter", color: "#d9a87c", bodyRadiusKm: 69_911, kind: "planet" },
  SATURN_BARYCENTER: { label: "Saturn", color: "#e3cf9e", bodyRadiusKm: 58_232, kind: "planet" },
  URANUS_BARYCENTER: { label: "Uranus", color: "#9fd8e0", bodyRadiusKm: 25_362, kind: "planet" },
  NEPTUNE_BARYCENTER: { label: "Neptune", color: "#6f8fe8", bodyRadiusKm: 24_622, kind: "planet" },
  Moon: { label: "Moon", color: "#c8c8c8", bodyRadiusKm: 1_737, kind: "moon" },
  "demo:asteroid-1": {
    label: "Asteroid 1", color: "#b48ead", bodyRadiusKm: 0.04, focusDistKm: 3, kind: "asset",
  },
  "demo:moon-base-1": { label: "Moon Base", color: "#e6e6a0", focusDistKm: 20, kind: "asset" },
  "demo:rover-1": { label: "Lunar Rover", color: "#6bd68b", focusDistKm: 5, kind: "asset" },
  "demo:spaceship-1": { label: "Spaceship 1", color: "#4fc3f7", focusDistKm: 4_000, kind: "asset" },
  "demo:miner-1": { label: "Asteroid Miner", color: "#f2b56d", focusDistKm: 2, kind: "asset" },
};

/**
 * Display metadata for a prescribed id.
 *
 * An id with no table entry still renders: it falls back to whatever name the
 * registry knows, and to the hyphenated id when even that is missing.
 */
export function displayInfo(id: string, names: NameBook): DisplayInfo {
  const known = KNOWN[names.key(id)];
  if (known) return known;
  return { label: names.label(id), color: "#c9d4e8", focusDistKm: 1_000, kind: "asset" };
}

export function focusDistanceKm(id: string, names: NameBook): number {
  const d = displayInfo(id, names);
  return d.focusDistKm ?? (d.bodyRadiusKm ? d.bodyRadiusKm * 5 : 1_000);
}
