/**
 * Shared access to the generated ledger fixture for integration tests.
 *
 * The fixture is gitignored and generating it needs NAIF kernels, so a fresh
 * checkout has no copy. Tests that need it declare `describe.skipIf(!hasFixture)`
 * and call {@link fixture} *inside* their assertions — `skipIf` still runs the
 * describe body, so nothing may be loaded at collection time.
 */

import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { loadLedger, type LedgerData } from "../arrow/loader";

const here = dirname(fileURLToPath(import.meta.url));

/** `<repo>/visualizer/public/data/dummy.arrows`. */
export const FIXTURE_PATH = resolve(here, "../../public/data/dummy.arrows");

/** The name registry `Ledger::save_ipc` writes beside it. */
export const NAMES_PATH = `${FIXTURE_PATH}.names.arrow`;

/** Whether both files are present. */
export const hasFixture = existsSync(FIXTURE_PATH) && existsSync(NAMES_PATH);

export interface Fixture {
  data: LedgerData;
  /** The prescribed id filed under a display key — how a test names an entity. */
  idOf(key: string): string;
}

let cached: Fixture | undefined;

/** Loads (once) and returns the fixture. Throws if it is not on disk. */
export function fixture(): Fixture {
  if (cached) return cached;
  if (!hasFixture) {
    throw new Error(
      `fixture missing at ${FIXTURE_PATH} — ` +
        "run: cargo run -p soloc-ledger --example gen_visualizer_fixture",
    );
  }
  const data = loadLedger(readFileSync(FIXTURE_PATH), readFileSync(NAMES_PATH));
  const idOf = (key: string): string => {
    for (const id of data.entities.keys()) if (data.names.key(id) === key) return id;
    throw new Error(`no entity named '${key}' in the fixture`);
  };
  cached = { data, idOf };
  return cached;
}
