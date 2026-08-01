/**
 * Arrow IPC → typed rows for the soloc entity schema.
 *
 * Reads real `RecordBatch`es (the same bytes `soloc-server` would stream over
 * Arrow Flight) with `apache-arrow`, unwraps the nested `spacetimestamp`
 * struct column, and normalises epochs to a single bigint (ns since J2000
 * TAI). Honours "store raw, reproject on demand": positions/quaternions/units
 * are surfaced exactly as stored.
 *
 * Two column encodings need decoding on the way in, and neither loses anything:
 *
 * - **Identity columns** (`entity_id`, `frame_id`, `source_id`) are
 *   `FixedSizeBinary(16)` prescribed ids. They become their canonical
 *   hyphenated strings (see `core/identity.ts`) and stay the key for
 *   everything downstream. Names are a separate, optional lookup.
 * - **Vocabulary columns** (`units_pos`, `timescale_id`, `estimate_type`) are
 *   `UInt8` codes whose decode table travels in the field's own Arrow
 *   extension metadata. Reading the table off the schema rather than hard-coding
 *   it means a vocabulary gaining a member does not need a front-end release.
 */

import { tableFromIPC, type Field } from "apache-arrow";
import { partsToNs } from "../core/epoch";
import { NameBook, idToString } from "../core/identity";
import { asArrowIPC } from "./ipc";

export interface EntityRow {
  /** Canonical hyphenated prescribed id — the key for this entity everywhere. */
  entityId: string;
  // spacetimestamp struct fields, verbatim
  frameId: string;
  unitsPos: string;
  timescaleId: string;
  sourceId: string;
  estimateType: string;
  position: [number, number, number];
  quaternion: [number, number, number, number]; // [w, x, y, z]
  epochNs: bigint; // duration_centuries + duration_ns collapsed
  positionCovariance: number[] | null; // upper-triangle 6
  orientationCovariance: number[] | null; // upper-triangle 6
  // entity-schema extras
  velocity: [number, number, number] | null;
  angularVelocity: [number, number, number] | null;
  acceleration: [number, number, number] | null;
  massKg: number | null;
  stateCovariance: number[] | null; // upper-triangle 21
  dimensionsM: [number, number, number] | null;
  rowIndex: number;
}

export interface LedgerData {
  rows: EntityRow[];
  /** Rows grouped per entity, sorted by epoch ascending. */
  entities: Map<string, EntityRow[]>;
  /** Display names for the ids above. Empty when no registry was supplied. */
  names: NameBook;
  numBatches: number;
  /** Data window, from the rows themselves. */
  minEpochNs: bigint;
  maxEpochNs: bigint;
}

/** Converts an Arrow FixedSizeList cell (or null) to a plain number array. */
function nums(cell: unknown): number[] | null {
  if (cell == null) return null;
  const arr = cell as { toArray?: () => ArrayLike<number> };
  return Array.from(typeof arr.toArray === "function" ? arr.toArray() : (cell as ArrayLike<number>));
}

function vec3(cell: unknown): [number, number, number] | null {
  const a = nums(cell);
  return a === null ? null : [a[0] ?? 0, a[1] ?? 0, a[2] ?? 0];
}

const EXTENSION_METADATA_KEY = "ARROW:extension:metadata";

/**
 * Canonical decode tables, used only when a field arrives without its metadata.
 *
 * Mirrors `spacetimestamp::vocabulary`. Index 0 is the reserved `-` placeholder,
 * so a stored code indexes straight in. Vocabularies are append-only by
 * contract, which is what makes a stale copy safe: it can fall short of a newer
 * writer, never disagree with it.
 */
const FALLBACK_VOCABULARIES: Readonly<Record<string, readonly string[]>> = {
  units_pos: ["-", "km", "m", "cm", "mm", "au", "in", "ft", "mi", "nmi"],
  timescale_id: [
    "-", "TAI", "TT", "ET", "TDB", "UTC", "GPST", "GST", "BDT", "QZSST", "TCG", "TCB", "TL", "TCL",
  ],
  estimate_type: ["-", "MEASURED", "ESTIMATED", "SIMULATED"],
};

/** The decode table for one vocabulary column, preferring the file's own metadata. */
function vocabularyTable(field: Field | undefined, name: string): readonly string[] {
  const declared = field?.metadata?.get(EXTENSION_METADATA_KEY);
  if (declared) return declared.split(",");
  const fallback = FALLBACK_VOCABULARIES[name];
  if (!fallback) throw new Error(`no decode table for vocabulary column '${name}'`);
  return fallback;
}

/** Builds a code → token decoder for one vocabulary column. */
function decoder(field: Field | undefined, name: string): (code: unknown) => string {
  const table = vocabularyTable(field, name);
  return (code) => {
    if (typeof code === "string") {
      // Vocabularies used to be dictionary-encoded strings. A file still shaped
      // that way predates the UInt8 encoding and needs regenerating, which is a
      // far more useful thing to say than "code NaN".
      throw new Error(
        `${name} is a string ('${code}'), not a vocabulary code — this fixture predates ` +
          "the current schema. Regenerate it: cargo run -p soloc-ledger --example gen_visualizer_fixture",
      );
    }
    const i = Number(code);
    const token = table[i];
    if (token === undefined || i === 0) {
      throw new Error(`invalid ${name} code ${i}, expected 1..=${table.length - 1}`);
    }
    return token;
  };
}

/** The `spacetimestamp` struct's own child fields, by name. */
function stsFields(fields: readonly Field[]): Map<string, Field> {
  const sts = fields.find((f) => f.name === "spacetimestamp");
  const children = (sts?.type as { children?: Field[] } | undefined)?.children ?? [];
  return new Map(children.map((f) => [f.name, f]));
}

/**
 * Parses an entity ledger, optionally with the name registry written beside it.
 *
 * `namesBytes` is the `<fixture>.names.arrow` sibling `Ledger::save_ipc` writes.
 * It is optional on purpose: names are display-only, so a caller that just
 * wants geometry can skip the second fetch and still get a working scene.
 */
export function loadLedger(
  bytes: ArrayBuffer | Uint8Array,
  namesBytes?: ArrayBuffer | Uint8Array,
): LedgerData {
  const table = tableFromIPC(asArrowIPC(bytes, "the ledger"));

  const ids = table.getChild("entity_id");
  const sts = table.getChild("spacetimestamp");
  if (!ids || !sts) {
    throw new Error(
      "not a soloc entity ledger: missing 'entity_id' or 'spacetimestamp' column",
    );
  }
  const velocity = table.getChild("velocity");
  const angularVelocity = table.getChild("angular_velocity");
  const acceleration = table.getChild("acceleration");
  const massKg = table.getChild("mass_kg");
  const stateCovariance = table.getChild("state_covariance");
  const dimensions = table.getChild("dimensions");

  const fields = stsFields(table.schema.fields);
  const decodeUnits = decoder(fields.get("units_pos"), "units_pos");
  const decodeTimescale = decoder(fields.get("timescale_id"), "timescale_id");
  const decodeEstimate = decoder(fields.get("estimate_type"), "estimate_type");

  const rows: EntityRow[] = [];
  for (let i = 0; i < table.numRows; i++) {
    // StructRow proxy: fields by schema name.
    const s = sts.get(i) as Record<string, unknown> | null;
    if (s === null) continue;

    const timescale = decodeTimescale(s["timescale_id"]);
    if (timescale !== "TAI") {
      // Ledger::append normalises to TAI before storing; anything else means
      // the file bypassed the ledger. Surface it rather than misinterpreting.
      throw new Error(`row ${i}: expected TAI-normalised data, got timescale '${timescale}'`);
    }

    const pos = vec3(s["position"]);
    const quat = nums(s["quaternion"]);
    if (!pos || !quat || quat.length !== 4) {
      throw new Error(`row ${i}: malformed position/quaternion`);
    }

    rows.push({
      entityId: idToString(ids.get(i)),
      frameId: idToString(s["frame_id"]),
      unitsPos: decodeUnits(s["units_pos"]),
      timescaleId: timescale,
      sourceId: idToString(s["source_id"]),
      estimateType: decodeEstimate(s["estimate_type"]),
      position: pos,
      quaternion: [quat[0]!, quat[1]!, quat[2]!, quat[3]!],
      epochNs: partsToNs(Number(s["duration_centuries"]), s["duration_ns"] as bigint),
      positionCovariance: nums(s["position_covariance"]),
      orientationCovariance: nums(s["orientation_covariance"]),
      velocity: vec3(velocity?.get(i)),
      angularVelocity: vec3(angularVelocity?.get(i)),
      acceleration: vec3(acceleration?.get(i)),
      massKg: (massKg?.get(i) as number | null) ?? null,
      stateCovariance: nums(stateCovariance?.get(i)),
      dimensionsM: vec3(dimensions?.get(i)),
      rowIndex: i,
    });
  }

  const entities = new Map<string, EntityRow[]>();
  for (const r of rows) {
    const list = entities.get(r.entityId);
    if (list) list.push(r);
    else entities.set(r.entityId, [r]);
  }
  let minEpochNs = rows.length ? rows[0]!.epochNs : 0n;
  let maxEpochNs = minEpochNs;
  for (const r of rows) {
    if (r.epochNs < minEpochNs) minEpochNs = r.epochNs;
    if (r.epochNs > maxEpochNs) maxEpochNs = r.epochNs;
  }
  for (const list of entities.values()) {
    list.sort((a, b) => (a.epochNs < b.epochNs ? -1 : a.epochNs > b.epochNs ? 1 : 0));
  }

  const names = namesBytes ? NameBook.fromIPC(namesBytes) : NameBook.empty();

  return { rows, entities, names, numBatches: table.batches.length, minEpochNs, maxEpochNs };
}
