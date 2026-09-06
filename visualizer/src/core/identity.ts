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

  /** What a human should read: the registered common name, else the id itself. */
  label(id: string): string {
    return this.entries.get(id)?.commonName ?? id;
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
    if (!e) return id;
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
