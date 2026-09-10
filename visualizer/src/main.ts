/**
 * soloc visualizer — Eyes-on-the-Solar-System-style map over spacetimestamp
 * record batches. Phase 4: time playback with live transform-tree changes.
 */

import { loadLedger, type LedgerData } from "./arrow/loader";
import { TransformTopology } from "./core/topology";
import { Viewer } from "./scene/viewer";
import { buildHud } from "./ui/hud";
import { buildInspector } from "./ui/inspector";
import { buildTimeControls, DEFAULT_SPEED } from "./ui/timeControls";
import { buildTreePanel } from "./ui/treePanel";

async function main(): Promise<void> {
  const canvasHost = document.getElementById("canvas-host")!;
  const labelLayer = document.getElementById("labels")!;
  const panel = document.getElementById("panel")!;
  const status = document.getElementById("status")!;
  const timebar = document.getElementById("timebar")!;

  const resp = await fetch("/data/dummy.arrows");
  if (!resp.ok) {
    status.textContent =
      "Could not fetch /data/dummy.arrows — run: cargo run -p soloc-ledger --example gen_visualizer_fixture";
    return;
  }

  // The name registry `Ledger::save_ipc` writes beside the ledger. Display-only,
  // so a missing sibling degrades to hyphenated ids rather than failing the load.
  const namesResp = await fetch("/data/dummy.arrows.names.arrow");
  const namesBytes = namesResp.ok ? await namesResp.arrayBuffer() : undefined;

  // A dev server answers a missing path with index.html and a 200, so `resp.ok`
  // above cannot catch an absent fixture — `loadLedger` checks the Arrow magic
  // and says so. Put that sentence where the user is already looking.
  let data: LedgerData;
  try {
    data = loadLedger(await resp.arrayBuffer(), namesBytes);
  } catch (e) {
    status.textContent = e instanceof Error ? e.message : String(e);
    throw e;
  }
  const topo = TransformTopology.fromRows(data.rows);
  const viewer = new Viewer(canvasHost, labelLayer, data, topo);
  const updateParents = buildHud(panel, status, data, topo, viewer);
  const treePanel = buildTreePanel(document.getElementById("treepanel")!, data, topo, viewer);
  const inspector = buildInspector(document.getElementById("inspector")!, viewer);

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

  // ---- toolbar: transform-tree layer toggle + whole-system view -------------
  const treeBtn = document.getElementById("btn-tree")!;
  const overviewBtn = document.getElementById("btn-overview")!;
  const toggleTree = () => {
    viewer.setTreeLayer(!viewer.treeLayerOn);
    treeBtn.classList.toggle("active", viewer.treeLayerOn);
  };
  const panelBtn = document.getElementById("btn-panel")!;
  const rightcol = document.getElementById("rightcol")!;
  const togglePanels = () => {
    const hidden = rightcol.style.display === "none";
    rightcol.style.display = hidden ? "" : "none";
    panelBtn.classList.toggle("active", hidden);
  };
  treeBtn.addEventListener("click", toggleTree);
  panelBtn.addEventListener("click", togglePanels);
  overviewBtn.addEventListener("click", () => viewer.overview());
  window.addEventListener("keydown", (ev) => {
    if (ev.target instanceof HTMLInputElement || ev.target instanceof HTMLSelectElement) return;
    if (ev.code === "KeyT") toggleTree();
    if (ev.code === "KeyP") togglePanels();
    if (ev.code === "KeyO") viewer.overview();
  });

  viewer.onBeforeFrame = (dt) => {
    if (!playing) return;
    const next = t + BigInt(Math.round(dt * nsPerSec));
    if (next >= data.maxEpochNs) {
      setTime(data.maxEpochNs, { pause: true });
    } else {
      setTime(next);
    }
  };

  setTime(data.minEpochNs);
}

main().catch((err) => {
  document.getElementById("status")!.textContent =
    `error: ${err instanceof Error ? err.message : String(err)}`;
});
