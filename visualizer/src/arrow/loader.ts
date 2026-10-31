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
import { NameBook, idToString, astroFrameOf, kindOfIdString, KIND_ASTRO } from "../core/identity";
import { asArrowIPC } from "./ipc";

export interface EntityRow {
  /** Original GPX order/context. Present only in a validated playback export. */
  gpx?: GpxProvenance;
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

export interface GpxProvenance {
  source: string;
  object: string;
  segment: bigint | null;
  point: bigint;
}

export interface LedgerData {
  /** The explicitly posed terrestrial anchor used for the WGS84 surface. */
  gpxPlayback?: { earthFrameId: string };
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
  const gpxFields = ["gpx_source", "gpx_object", "gpx_segment", "gpx_point"] as const;
  const gpxColumns = gpxFields.map((name) => table.getChild(name));
  const playbackVersion = table.schema.metadata.get("soloc:gpx_playback");
  const earthFrameId = table.schema.metadata.get("soloc:gpx_earth_frame");
  if (playbackVersion !== undefined || earthFrameId !== undefined || gpxColumns.some(Boolean)) {
    if (playbackVersion !== "1" || !earthFrameId || gpxColumns.some((c) => !c)) {
      throw new Error("GPX playback needs version 1 metadata, an Earth frame, and all four provenance columns");
    }
    const earth = astroFrameOf(earthFrameId);
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(earthFrameId) ||
        kindOfIdString(earthFrameId) !== KIND_ASTRO || earth?.ephemerisId !== 399 || earth.orientationId !== 3000) {
      throw new Error("GPX playback Earth frame must be ITRF93 (399, 3000)");
    }
    const expected = ["Utf8", "Utf8", "Uint64", "Uint64"];
    gpxColumns.forEach((column, i) => {
      if (column!.type.toString() !== expected[i]) {
        throw new Error(`GPX playback column ${gpxFields[i]} must be ${expected[i]}`);
      }
    });
  }

  const fields = stsFields(table.schema.fields);
  const decodeUnits = decoder(fields.get("units_pos"), "units_pos");
  const decodeTimescale = decoder(fields.get("timescale_id"), "timescale_id");
  const decodeEstimate = decoder(fields.get("estimate_type"), "estimate_type");

  const rows: EntityRow[] = [];
  for (let i = 0; i < table.numRows; i++) {
    // StructRow proxy: fields by schema name.
    const s = sts.get(i) as Record<string, unknown> | null;
    if (s === null) {
      if (playbackVersion) throw new Error(`row ${i}: GPX playback cannot contain a null pose`);
      continue;
    }

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
    if (playbackVersion && (!pos.every(Number.isFinite) || !quat.every(Number.isFinite) ||
        Math.abs(Math.hypot(...quat) - 1) > 1e-6)) {
      throw new Error(`row ${i}: GPX playback requires finite positions and unit quaternions`);
    }

    const row: EntityRow = {
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
    };
    if (playbackVersion) {
      const [source, object, segment, point] = gpxColumns.map((c) => c!.get(i));
      if ([source, object, segment, point].some((v) => v !== null)) {
        if (typeof source !== "string" || typeof object !== "string" || !/^(track|route):[1-9][0-9]*$/.test(object) ||
            typeof point !== "bigint" || point <= 0n ||
            (object.startsWith("track:") ? typeof segment !== "bigint" || segment <= 0n : segment !== null)) {
          throw new Error(`row ${i}: incomplete or invalid GPX provenance`);
        }
        row.gpx = { source, object, segment: segment as bigint | null, point };
        if (row.frameId !== earthFrameId) throw new Error(`row ${i}: GPX observation is not in the exported Earth frame`);
      }
    }
    rows.push(row);
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
  for (const row of rows) {
    const frame = astroFrameOf(row.frameId);
    if (kindOfIdString(row.frameId) === KIND_ASTRO && frame?.ephemerisId === 399 &&
        frame.orientationId === 3000 && !entities.has(row.frameId)) {
      throw new Error("ITRF93 has no pose timeline. Generate a GPX playback export with Earth-frame anchors; raw GPX ingestion IPC is not a positioned scene.");
    }
  }

  const names = namesBytes ? NameBook.fromIPC(namesBytes) : NameBook.empty();

  if (playbackVersion) {
    const observations = rows.filter((row) => row.gpx);
    const anchors = entities.get(earthFrameId!) ?? [];
    if (observations.length === 0 || anchors.length === 0) {
      throw new Error("GPX playback requires observations and a posed Earth frame");
    }
    for (const entityRows of entities.values()) {
      if (entityRows.some((row) => row.gpx) && entityRows.some((row) => !row.gpx)) {
        throw new Error("GPX playback cannot mix observations and anchor rows for one entity");
      }
    }
    for (const row of rows) {
      if (row.gpx) {
        if (row.entityId === earthFrameId) {
          throw new Error("GPX playback cannot mix observations and anchor rows for one entity");
        }
      } else {
        const parent = astroFrameOf(row.frameId);
        if (row.entityId !== earthFrameId || kindOfIdString(row.frameId) !== KIND_ASTRO ||
            parent?.ephemerisId !== 399 || parent.orientationId !== 1 ||
            row.position.some((n) => n !== 0) || entities.has(row.frameId)) {
          throw new Error("GPX playback needs zero-translation ITRF93 poses under an unposed GCRF root");
        }
      }
    }
    for (let i = 1; i < anchors.length; i++) {
      if (anchors[i]!.epochNs <= anchors[i - 1]!.epochNs) throw new Error("GPX Earth anchor epochs must be unique");
      if (anchors[i]!.epochNs - anchors[i - 1]!.epochNs > 300_000_000_000n) {
        throw new Error("GPX Earth anchor samples must be no more than 300 seconds apart");
      }
    }
    if (observations.some((row) => row.epochNs < anchors[0]!.epochNs || row.epochNs > anchors[anchors.length - 1]!.epochNs)) {
      throw new Error("GPX Earth anchor timeline does not cover every observation");
    }
  }

  return { rows, entities, names, numBatches: table.batches.length, minEpochNs, maxEpochNs,
    ...(playbackVersion ? { gpxPlayback: { earthFrameId: earthFrameId! } } : {}) };
}
