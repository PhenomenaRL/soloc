/**
 * Prescribed ids in the browser — a TypeScript reading of
 * `spacetimestamp::identity`.
 *
 * Every identity column (`entity_id`, `frame_id`, `source_id`, and both
 * topology columns) is a `FixedSizeBinary(16)` carrying the `arrow.uuid`
 * extension name. Those 16 bytes are the identity; a *name* is a separate,
 * display-only lookup that the ledger ships beside the data as
 * `<fixture>.names.arrow`.
 *
 * That split is deliberate on the Rust side and it is honoured here: the whole
 * front end keys on the id string returned by {@link idToString}, and a
 * {@link NameBook} is consulted only when something is about to be rendered
 * for a human. Nothing computes over a name.
 *
 * Two kinds of id exist in a fixture:
 *
 * - **astronomical** (kind `0x0`) — self-describing. The bytes embed anise's
 *   `(ephemeris_id, orientation_id)` pair rather than a hash, so `Earth` and
 *   `IAU_EARTH` are one id and a body *is* its own body-fixed frame. This is
 *   why the rover's `IAU_MOON` frame resolves against the Moon's own rows with
 *   no host table in between.
 * - **soloc** (`0x1`) / **abstract** (`0x2`) — a SHA-256 over
 *   `(kind, authority, common_name)`. Nothing about the name is recoverable
 *   from the bytes, so these are the ids the registry file exists for.
 */

import { tableFromIPC } from "apache-arrow";

import { asArrowIPC } from "../arrow/ipc";

/** Every prescribed id is exactly 16 bytes. */
export const ID_BYTES = 16;

export const KIND_ASTRO = 0x0;
export const KIND_SOLOC = 0x1;
export const KIND_ABSTRACT = 0x2;

/** The authority astronomical names are registered under. */
export const ASTRO_AUTHORITY = "astro";

/** A registered display name for one id. */
export interface NameEntry {
  authority: string;
  commonName: string;
  kind: number;
}

/** Reads the 16 bytes behind an Arrow `FixedSizeBinary(16)` cell. */
function idBytes(cell: unknown): Uint8Array {
  if (cell instanceof Uint8Array) return cell;
  if (ArrayBuffer.isView(cell)) {
    const v = cell as ArrayBufferView;
    return new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
  }
  if (Array.isArray(cell)) return Uint8Array.from(cell as number[]);
  throw new Error("id column cell is not FixedSizeBinary(16) bytes");
}

/**
 * Canonical hyphenated form (`8-4-4-4-12`) of an id cell.
 *
 * Byte-for-byte what `PrescribedId::to_hyphenated` prints, so an id shown in
 * the inspector can be pasted straight into a Rust-side query. This string is
 * the front end's primary key for everything: map keys, `frameId` comparisons,
 * scene-graph node names.
 */
export function idToString(cell: unknown): string {
  const b = idBytes(cell);
  if (b.length !== ID_BYTES) {
    throw new Error(`prescribed id must be ${ID_BYTES} bytes, got ${b.length}`);
  }
  let hex = "";
  for (const byte of b) hex += byte.toString(16).padStart(2, "0");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** The kind nibble: the low half of byte 6. */
export function kindOfIdString(id: string): number {
  // Byte 6 is the first byte of the third hyphen-separated group.
  const group = id.split("-")[2] ?? "";
  return Number.parseInt(group.slice(0, 2) || "0", 16) & 0x0f;
}

/**
 * Mirrors `spacetimestamp::ephemeris::ASTRO_FRAMES` (Rust) — the canonical
 * `(ephemeris_id, orientation_id)` → name table for astronomical ids.
 *
 * Used only as a *fallback* when an astronomical id has no registry entry: the
 * pair is baked straight into the id's own bytes (see {@link astroFrameOf}),
 * so a name can still be recovered even when whatever wrote the ledger forgot
 * to call the equivalent of `register_name` for it. First match wins, same as
 * the Rust `frame_name()` this is a reading of — that is what makes a
 * body-fixed pair like `(399, 399)` resolve to the bare `"Earth"` rather than
 * `"IAU_EARTH"`.
 */
const ASTRO_FRAMES: ReadonlyArray<readonly [string, number, number]> = [
  ["ICRF", 0, 1],
  ["J2000", 0, 1],
  ["SSB", 0, 1],
  ["GCRF", 399, 1],
  ["EME2000", 399, 1],
  ["EMB", 3, 1],
  ["MERCURY_BARYCENTER", 1, 1],
  ["VENUS_BARYCENTER", 2, 1],
  ["MARS_BARYCENTER", 4, 1],
  ["JUPITER_BARYCENTER", 5, 1],
  ["SATURN_BARYCENTER", 6, 1],
  ["URANUS_BARYCENTER", 7, 1],
  ["NEPTUNE_BARYCENTER", 8, 1],
  ["PLUTO_BARYCENTER", 9, 1],
  ["Sun", 10, 10],
  ["Mercury", 199, 199],
  ["Venus", 299, 299],
  ["Earth", 399, 399],
  ["Moon", 301, 301],
  ["Mars", 499, 499],
  ["Jupiter", 599, 599],
  ["Saturn", 699, 699],
  ["Uranus", 799, 799],
  ["Neptune", 899, 899],
  ["Pluto", 999, 1],
  ["Phobos", 401, 1],
  ["Deimos", 402, 1],
  ["Io", 501, 1],
  ["Europa", 502, 1],
  ["Ganymede", 503, 1],
  ["Callisto", 504, 1],
  ["Titan", 606, 1],
  ["Enceladus", 602, 1],
  ["IAU_SUN", 10, 10],
  ["IAU_MERCURY", 199, 199],
  ["IAU_VENUS", 299, 299],
  ["IAU_EARTH", 399, 399],
  ["IAU_MOON", 301, 301],
  ["IAU_MARS", 499, 499],
  ["IAU_JUPITER", 599, 599],
  ["IAU_SATURN", 699, 699],
  ["IAU_NEPTUNE", 899, 899],
  ["IAU_URANUS", 799, 799],
  ["IAU_PLUTO", 999, 999],
  ["IAU_CHARON", 901, 901],
  ["IAU_PHOBOS", 401, 401],
  ["IAU_DEIMOS", 402, 402],
  ["IAU_IO", 501, 501],
  ["IAU_EUROPA", 502, 502],
  ["IAU_GANYMEDE", 503, 503],
  ["IAU_CALLISTO", 504, 504],
  ["IAU_MIMAS", 601, 601],
  ["IAU_ENCELADUS", 602, 602],
  ["IAU_TETHYS", 603, 603],
  ["IAU_DIONE", 604, 604],
  ["IAU_RHEA", 605, 605],
  ["IAU_TITAN", 606, 606],
  ["IAU_IAPETUS", 608, 608],
  ["IAU_ARIEL", 701, 701],
  ["IAU_UMBRIEL", 702, 702],
  ["IAU_TITANIA", 703, 703],
  ["IAU_OBERON", 704, 704],
  ["IAU_MIRANDA", 705, 705],
  ["IAU_TRITON", 801, 801],
];

/**
 * Reverse of `PrescribedId::astronomical` — recovers `(ephemeris_id,
 * orientation_id)` straight from an id's own bytes, no registry lookup
 * needed. `null` if `id` isn't a well-formed 16-byte id.
 */
export function astroFrameOf(id: string): { ephemerisId: number; orientationId: number } | null {
  const hex = id.replace(/-/g, "");
  if (hex.length !== ID_BYTES * 2) return null;
  // bytes[0..4] and bytes[9..13], big-endian i32 — see `PrescribedId::astronomical`.
  const ephemerisId = Number.parseInt(hex.slice(0, 8), 16) | 0;
  const orientationId = Number.parseInt(hex.slice(18, 26), 16) | 0;
  return { ephemerisId, orientationId };
}

/** The canonical frame name for `(ephemerisId, orientationId)`, or `undefined` if unrecognised. */
export function astroFrameName(ephemerisId: number, orientationId: number): string | undefined {
  for (const [name, e, o] of ASTRO_FRAMES) {
    if (e === ephemerisId && o === orientationId) return name;
  }
  return undefined;
}

/**
 * Display names for prescribed ids — the browser half of
 * `spacetimestamp::identity::NameRegistry`.
 *
 * Display-only, exactly as on the Rust side. An id with no entry is still a
 * first-class citizen everywhere; it just renders as its hyphenated self.
 */
export class NameBook {
  private readonly entries = new Map<string, NameEntry>();

  /** Parses a `<fixture>.names.arrow` payload. */
  static fromIPC(bytes: ArrayBuffer | Uint8Array): NameBook {
    const book = new NameBook();
    const table = tableFromIPC(asArrowIPC(bytes, "the name registry"));
    const ids = table.getChild("prescribed_id");
    const authority = table.getChild("authority");
    const commonName = table.getChild("common_name");
    const kind = table.getChild("kind");
    if (!ids || !authority || !commonName || !kind) {
      throw new Error(
        "not a soloc name registry: expected prescribed_id, authority, common_name, kind",
      );
    }
    for (let i = 0; i < table.numRows; i++) {
      book.entries.set(idToString(ids.get(i)), {
        authority: String(authority.get(i)),
        commonName: String(commonName.get(i)),
        kind: Number(kind.get(i)),
      });
    }
    return book;
  }

  /** An empty book — every id renders as its hyphenated form. */
  static empty(): NameBook {
    return new NameBook();
  }

  /** A book built from bindings already in hand, for tests and synthetic data. */
  static fromEntries(entries: Iterable<readonly [string, NameEntry]>): NameBook {
    const book = new NameBook();
    for (const [id, entry] of entries) book.entries.set(id, entry);
    return book;
  }

  get size(): number {
    return this.entries.size;
  }

  entry(id: string): NameEntry | undefined {
    return this.entries.get(id);
  }

  /**
   * Recovers a display name straight from an unregistered astronomical id's
   * own bytes — the `(ephemeris_id, orientation_id)` pair baked in by
   * `PrescribedId::astronomical`, decoded the same way
   * `spacetimestamp::ephemeris::frame_name` would. Only consulted when
   * nothing was registered for `id`; a real registry entry always wins.
   */
  private fallbackAstroName(id: string): string | undefined {
    if (kindOfIdString(id) !== KIND_ASTRO) return undefined;
    const frame = astroFrameOf(id);
    return frame ? astroFrameName(frame.ephemerisId, frame.orientationId) : undefined;
  }

  /**
   * What a human should read: the registered common name, else a name
   * recovered straight from the id's own bytes for an unregistered
   * astronomical body, else the id itself.
   */
  label(id: string): string {
    const e = this.entries.get(id);
    if (e) return e.commonName;
    return this.fallbackAstroName(id) ?? id;
  }

  /**
   * The stable key display metadata is filed under (see `scene/registry.ts`).
   *
   * Astronomical names are already globally unique — there is exactly one
   * `Earth` — so they key on the bare name. Everything else is namespaced by
   * its authority, which is the half of a minted id that keeps two
   * organisations' `rover-1` apart.
   */
  key(id: string): string {
    const e = this.entries.get(id);
    if (!e) return this.fallbackAstroName(id) ?? id;
    return e.kind === KIND_ASTRO ? e.commonName : `${e.authority}:${e.commonName}`;
  }

  /** The id's kind, preferring the registry's own column over the byte nibble. */
  kind(id: string): number {
    return this.entries.get(id)?.kind ?? kindOfIdString(id);
  }

  /** `true` for an astronomical id — a body or a reference frame, never an asset. */
  isAstronomical(id: string): boolean {
    return this.kind(id) === KIND_ASTRO;
  }
}
