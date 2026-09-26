/**
 * soloc visualizer — Eyes-on-the-Solar-System-style map over spacetimestamp
 * record batches. Phase 4: time playback with live transform-tree changes.
 * Phase 5: bring-your-own ledger — a `?src=` URL, a dropped/picked file, or
 * the generated fixture, whichever applies.
 */

import { loadLedger, type LedgerData } from "./arrow/loader";
import { TransformTopology } from "./core/topology";
import { Viewer } from "./scene/viewer";
import { buildHud } from "./ui/hud";
import { buildInspector } from "./ui/inspector";
import { buildTimeControls, DEFAULT_SPEED, type TimeControls } from "./ui/timeControls";
import { buildTreePanel } from "./ui/treePanel";

const canvasHost = document.getElementById("canvas-host")!;
const labelLayer = document.getElementById("labels")!;
const panel = document.getElementById("panel")!;
const status = document.getElementById("status")!;
const timebar = document.getElementById("timebar")!;
const treepanel = document.getElementById("treepanel")!;
const inspectorEl = document.getElementById("inspector")!;
const dropzone = document.getElementById("dropzone")!;
const fileInput = document.getElementById("file-input") as HTMLInputElement;

/** The ledger currently on screen, so a fresh load can tear it down first. */
let current: { viewer: Viewer; controls: TimeControls } | null = null;

/**
 * Parses one ledger (plus optional names sibling) and rebuilds the whole app
 * around it, tearing down whatever was on screen first so repeated loads —
 * a new drop, a different file picked — don't leak WebGL contexts or stack
 * duplicate listeners. Each UI builder (`buildHud` and friends) clears its own
 * container on entry, so this only has to handle what it owns directly: the
 * previous `Viewer` and the previous `TimeControls`.
 *
 * `label` is cosmetic — it names the source in the page title.
 */
async function boot(
  bytes: ArrayBuffer | Uint8Array,
  namesBytes: ArrayBuffer | Uint8Array | undefined,
  label: string,
): Promise<void> {
  let data: LedgerData;
  try {
    data = loadLedger(bytes, namesBytes);
  } catch (e) {
    status.textContent = e instanceof Error ? e.message : String(e);
    throw e;
  }

  current?.viewer.dispose();
  current?.controls.destroy();
  current = null;

  const topo = TransformTopology.fromRows(data.rows);
  const viewer = new Viewer(canvasHost, labelLayer, data, topo);
  const updateParents = buildHud(panel, status, data, topo, viewer);
  const treePanel = buildTreePanel(treepanel, data, topo, viewer);
  const inspector = buildInspector(inspectorEl, viewer);
  document.title = `soloc visualizer — ${label}`;

  // Compose focus listeners (hud installed its own in buildHud).
  const hudFocus = viewer.onFocus;
  viewer.onFocus = (id) => {
    hudFocus?.(id);
    treePanel.setFocused(id);
    inspector.update();
  };
  treePanel.setFocused(viewer.focusedId);

  // ---- playback clock (owned here; viewer just renders whatever t we set) ----
  let t = data.minEpochNs;
  let playing = false;
  let nsPerSec = DEFAULT_SPEED.nsPerSec;

  const controls = buildTimeControls(timebar, {
    minNs: data.minEpochNs,
    maxNs: data.maxEpochNs,
    events: topo.events,
    onScrub: (nt) => setTime(nt, { pause: true }),
    onPlayToggle: () => {
      // Replay from the start when hitting play at the end of the window.
      if (!playing && t >= data.maxEpochNs) t = data.minEpochNs;
      playing = !playing;
      controls.update(t, playing);
    },
    onSpeedChange: (s) => {
      nsPerSec = s;
    },
  });

  function setTime(nt: bigint, opts: { pause?: boolean } = {}): void {
    t = nt < data.minEpochNs ? data.minEpochNs : nt > data.maxEpochNs ? data.maxEpochNs : nt;
    if (opts.pause) playing = false;
    viewer.setTime(t);
    updateParents(t);
    treePanel.update(t);
    inspector.update();
    controls.update(t, playing);
  }

  viewer.onBeforeFrame = (dt) => {
    if (!playing) return;
    const next = t + BigInt(Math.round(dt * nsPerSec));
    if (next >= data.maxEpochNs) {
      setTime(data.maxEpochNs, { pause: true });
    } else {
      setTime(next);
    }
  };

  current = { viewer, controls };
  setTime(data.minEpochNs);
}

// ---- toolbar: transform-tree layer toggle + whole-system view + panels ----
// Registered once, against `current`, so they keep working across reloads.
const treeBtn = document.getElementById("btn-tree")!;
const overviewBtn = document.getElementById("btn-overview")!;
const toggleTree = (): void => {
  if (!current) return;
  current.viewer.setTreeLayer(!current.viewer.treeLayerOn);
  treeBtn.classList.toggle("active", current.viewer.treeLayerOn);
};
const panelBtn = document.getElementById("btn-panel")!;
const rightcol = document.getElementById("rightcol")!;
const togglePanels = (): void => {
  const hidden = rightcol.style.display === "none";
  rightcol.style.display = hidden ? "" : "none";
  panelBtn.classList.toggle("active", hidden);
};
treeBtn.addEventListener("click", toggleTree);
panelBtn.addEventListener("click", togglePanels);
overviewBtn.addEventListener("click", () => current?.viewer.overview());
window.addEventListener("keydown", (ev) => {
  if (ev.target instanceof HTMLInputElement || ev.target instanceof HTMLSelectElement) return;
  if (ev.code === "KeyT") toggleTree();
  if (ev.code === "KeyP") togglePanels();
  if (ev.code === "KeyO") current?.viewer.overview();
});

// ---- bring your own ledger: file picker + drag-and-drop --------------------

const NAMES_RE = /\.names\.arrow$/i;

/** Picks the ledger + optional names sibling out of a drop/pick, by filename convention. */
function loadFromFiles(files: FileList | File[]): void {
  const list = Array.from(files);
  const namesFile = list.find((f) => NAMES_RE.test(f.name));
  const ledgerFile = list.find((f) => f !== namesFile);
  if (!ledgerFile) {
    status.textContent = "Drop a ledger (.arrows) file — optionally with its .names.arrow sibling.";
    return;
  }
  status.textContent = `loading ${ledgerFile.name}…`;
  void Promise.all([ledgerFile.arrayBuffer(), namesFile?.arrayBuffer()])
    .then(([bytes, namesBytes]) => boot(bytes, namesBytes, ledgerFile.name))
    .catch((e) => {
      status.textContent = e instanceof Error ? e.message : String(e);
    });
}

document.getElementById("btn-load")!.addEventListener("click", () => fileInput.click());
fileInput.addEventListener("change", () => {
  if (fileInput.files?.length) loadFromFiles(fileInput.files);
  fileInput.value = ""; // so picking the same file twice still fires `change`
});

// Drag-and-drop anywhere on the page. `dragenter`/`dragleave` fire on every
// element the pointer crosses, so a depth counter is what keeps the overlay
// from flickering as the pointer moves over child elements.
let dragDepth = 0;
window.addEventListener("dragover", (ev) => ev.preventDefault());
window.addEventListener("dragenter", (ev) => {
  ev.preventDefault();
  dragDepth++;
  dropzone.classList.add("active");
});
window.addEventListener("dragleave", (ev) => {
  ev.preventDefault();
  dragDepth = Math.max(0, dragDepth - 1);
  if (dragDepth === 0) dropzone.classList.remove("active");
});
window.addEventListener("drop", (ev) => {
  ev.preventDefault();
  dragDepth = 0;
  dropzone.classList.remove("active");
  if (ev.dataTransfer?.files.length) loadFromFiles(ev.dataTransfer.files);
});

// ---- initial load: ?src=<url>, else the generated fixture -----------------

async function loadInitial(): Promise<void> {
  const src = new URLSearchParams(location.search).get("src");
  const ledgerUrl = src ?? "/data/dummy.arrows";
  const label = src ?? "dummy.arrows";

  let resp: Response;
  try {
    resp = await fetch(ledgerUrl);
  } catch (e) {
    status.textContent =
      `could not fetch ${ledgerUrl}: ${e instanceof Error ? e.message : String(e)} ` +
      "(check the URL, and that it allows cross-origin requests) — " +
      'drop a .arrows file below instead, or use "Load ledger…".';
    return;
  }
  if (!resp.ok) {
    status.textContent = src
      ? `${src} returned ${resp.status} — drop a .arrows file below, or use "Load ledger…".`
      : "No ledger loaded — drop a .arrows file anywhere on this page, use \"Load ledger…\", " +
        "or run: cargo run -p soloc-ledger --example gen_visualizer_fixture";
    return;
  }

  // The name registry `Ledger::save_ipc` writes beside the ledger, `<path>.names.arrow`.
  // Display-only, so a missing sibling degrades to hyphenated ids rather than failing the load.
  const namesResp = await fetch(`${ledgerUrl}.names.arrow`);
  const namesBytes = namesResp.ok ? await namesResp.arrayBuffer() : undefined;

  // A dev server answers a missing path with index.html and a 200, so `resp.ok`
  // above cannot catch an absent fixture — `loadLedger` checks the Arrow magic
  // and says so. Put that sentence where the user is already looking.
  await boot(await resp.arrayBuffer(), namesBytes, label);
}

loadInitial().catch((err) => {
  status.textContent = err instanceof Error ? err.message : String(err);
});
