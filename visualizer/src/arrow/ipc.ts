/**
 * The one guard between a fetch and `tableFromIPC`.
 *
 * A dev server answers an unknown path with `index.html` and a **200**, so a
 * missing fixture does not arrive as a failed request — it arrives as a page.
 * Handed to Arrow that page parses as a length prefix: `<!do` little-endian is
 * 1 868 833 084, and the reader asks for 1.8 GB of metadata before giving up.
 * Checking the magic first turns that into a sentence that names the cause.
 */

/** Every Arrow IPC file (not stream) begins and ends with these six bytes. */
const MAGIC = "ARROW1";

function looksLikeHtml(bytes: Uint8Array): boolean {
  const head = new TextDecoder().decode(bytes.subarray(0, 64)).trimStart().toLowerCase();
  return head.startsWith("<!doctype") || head.startsWith("<html");
}

/**
 * Returns `bytes` as a `Uint8Array`, having checked it really is an Arrow IPC
 * file. `what` names the file in the error, e.g. `"/data/dummy.arrows"`.
 */
export function asArrowIPC(bytes: ArrayBuffer | Uint8Array, what: string): Uint8Array {
  const u8 = bytes instanceof ArrayBuffer ? new Uint8Array(bytes) : bytes;

  if (u8.length >= MAGIC.length) {
    const magic = new TextDecoder().decode(u8.subarray(0, MAGIC.length));
    if (magic === MAGIC) return u8;
  }

  if (looksLikeHtml(u8)) {
    throw new Error(
      `${what} came back as an HTML page, not Arrow — the dev server served ` +
        "index.html because the file is not there. Generate the fixture: " +
        "cargo run -p soloc-ledger --example gen_visualizer_fixture",
    );
  }
  throw new Error(
    `${what} is not an Arrow IPC file (no '${MAGIC}' magic, ${u8.length} bytes). ` +
      "Regenerate it: cargo run -p soloc-ledger --example gen_visualizer_fixture",
  );
}
