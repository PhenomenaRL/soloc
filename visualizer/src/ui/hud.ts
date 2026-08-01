/**
 * HUD: entity list panel (grouped anchors vs. assets) and a status line.
 * Clicking an entry flies the camera to that entity.
 */

import type { LedgerData } from "../arrow/loader";
import type { TransformTopology } from "../core/topology";
import { formatTai } from "../core/epoch";
import { displayInfo } from "../scene/registry";
import type { Viewer } from "../scene/viewer";

export function buildHud(
  panel: HTMLElement,
  status: HTMLElement,
  data: LedgerData,
  topo: TransformTopology,
  viewer: Viewer,
): (t: bigint) => void {
  const names = data.names;
  // Astronomical ids are bodies and reference frames; everything else is an
  // asset someone minted. The id's own kind nibble says which — no name test.
  const groups: [string, (id: string) => boolean][] = [
    ["Celestial anchors", (id) => names.isAstronomical(id)],
    ["Assets", (id) => !names.isAstronomical(id)],
  ];

  const rows = new Map<string, HTMLElement>();
  const parentSpans = new Map<string, HTMLElement>();
  for (const [title, match] of groups) {
    const h = document.createElement("div");
    h.className = "hud-group";
    h.textContent = title;
    panel.append(h);

    for (const id of [...data.entities.keys()].sort()) {
      if (!match(id)) continue;
      const info = displayInfo(id, names);
      const el = document.createElement("div");
      el.className = "hud-entity";
      el.innerHTML =
        `<span class="dot" style="background:${info.color}"></span>` +
        `<span class="name">${info.label}</span>` +
        `<span class="parent"></span>`;
      el.addEventListener("click", () => viewer.focus(id));
      panel.append(el);
      rows.set(id, el);
      parentSpans.set(id, el.querySelector(".parent")!);
    }
  }

  viewer.onFocus = (id) => {
    for (const [rid, el] of rows) el.classList.toggle("focused", rid === id);
  };

  status.textContent =
    `${data.rows.length} rows · ${data.entities.size} entities · ` +
    `${topo.events.length} topology events · window ` +
    `${formatTai(data.minEpochNs)} → ${formatTai(data.maxEpochNs)} · ` +
    `textures © planetpixelemporium.com`;

  // Live parent column: flashes when the transform tree changes under playback.
  return (t: bigint) => {
    for (const [id, span] of parentSpans) {
      const parent = topo.parentAt(id, t);
      const text = `← ${parent === undefined ? "—" : displayInfo(parent, names).label}`;
      if (span.textContent !== text) {
        span.textContent = text;
        span.classList.remove("changed");
        void span.offsetWidth; // restart the flash animation
        span.classList.add("changed");
      }
    }
  };
}
