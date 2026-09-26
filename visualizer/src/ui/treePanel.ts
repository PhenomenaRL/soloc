/**
 * 2D transform-tree panel: an indented tree (SVG) driven by `parentAt(id, t)`,
 * so scrubbing time restructures it live — rows slide to their new parent and
 * the changed edge flashes. Complements the in-scene 3D edge layer: same tree,
 * schematic view. Anchors the ledger cannot pose (ICRF) are squares; entities
 * are colored dots — and since a body is its own body-fixed frame under
 * prescribed ids, IAU_MOON shows up as the Moon's own dot rather than a
 * separate square.
 */

import type { LedgerData } from "../arrow/loader";
import type { TransformTopology } from "../core/topology";
import { displayInfo } from "../scene/registry";
import type { Viewer } from "../scene/viewer";

const SVG_NS = "http://www.w3.org/2000/svg";
const ROW_H = 20;
const INDENT = 15;
const PAD = 8;

interface NodeVis {
  id: string;
  isEntity: boolean;
  g: SVGGElement;
  edge: SVGPathElement;
  text: SVGTextElement;
  x: number;
  y: number;
  tx: number;
  ty: number;
  parent: string | null;
}

export interface TreePanel {
  update(t: bigint): void;
  setFocused(id: string | null): void;
}

export function buildTreePanel(
  container: HTMLElement,
  data: LedgerData,
  topo: TransformTopology,
  viewer: Viewer,
): TreePanel {
  container.replaceChildren();
  const title = document.createElement("div");
  title.className = "side-title";
  title.textContent = "transform tree";
  container.append(title);

  const names = data.names;
  const entityIds = [...data.entities.keys()];
  const anchors = new Set<string>();
  for (const e of topo.events) if (!data.entities.has(e.parentId)) anchors.add(e.parentId);
  const allIds = [...anchors, ...entityIds];

  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("width", "100%");
  svg.setAttribute("height", String(allIds.length * ROW_H + PAD * 2));
  container.append(svg);
  const edgeLayer = document.createElementNS(SVG_NS, "g");
  const nodeLayer = document.createElementNS(SVG_NS, "g");
  svg.append(edgeLayer, nodeLayer);

  const label = (id: string) =>
    data.entities.has(id) ? displayInfo(id, names).label : names.label(id);

  const nodes = new Map<string, NodeVis>();
  for (const id of allIds) {
    const isEntity = data.entities.has(id);
    const g = document.createElementNS(SVG_NS, "g");
    g.classList.add("tp-node");

    const shape = isEntity
      ? document.createElementNS(SVG_NS, "circle")
      : document.createElementNS(SVG_NS, "rect");
    if (isEntity) {
      shape.setAttribute("r", "4");
      shape.setAttribute("cx", "6");
      shape.setAttribute("cy", String(ROW_H / 2));
      shape.setAttribute("fill", displayInfo(id, names).color);
    } else {
      shape.setAttribute("width", "7");
      shape.setAttribute("height", "7");
      shape.setAttribute("x", "2.5");
      shape.setAttribute("y", String(ROW_H / 2 - 3.5));
      shape.classList.add("tp-anchor");
    }

    const text = document.createElementNS(SVG_NS, "text");
    text.setAttribute("x", "15");
    text.setAttribute("y", String(ROW_H / 2 + 3.5));
    text.textContent = label(id);
    text.classList.add(isEntity ? "tp-label" : "tp-label-anchor");

    g.append(shape, text);
    if (isEntity) {
      g.style.cursor = "pointer";
      g.addEventListener("click", () => viewer.focus(id));
    }
    nodeLayer.append(g);

    const edge = document.createElementNS(SVG_NS, "path");
    edge.classList.add("tp-edge");
    edgeLayer.append(edge);

    nodes.set(id, { id, isEntity, g, edge, text, x: PAD, y: PAD, tx: PAD, ty: PAD, parent: null });
  }

  // An anchor has no rows, so nothing can give it a parent: it is always a root.
  const parentOf = (id: string, t: bigint): string | null =>
    nodes.get(id)!.isEntity ? (topo.parentAt(id, t) ?? null) : null;

  function update(t: bigint): void {
    const children = new Map<string | null, string[]>();
    for (const id of allIds) {
      const p = parentOf(id, t);
      const key = p !== null && nodes.has(p) ? p : null;
      (children.get(key) ?? children.set(key, []).get(key)!).push(id);

      const vis = nodes.get(id)!;
      if (vis.parent !== key) {
        if (vis.parent !== null) {
          // Restart the flash animation on a genuine re-parent (not first layout).
          vis.edge.classList.remove("flash");
          void vis.edge.getBoundingClientRect();
          vis.edge.classList.add("flash");
        }
        vis.parent = key;
      }
    }
    for (const list of children.values()) list.sort((a, b) => label(a).localeCompare(label(b)));

    let index = 0;
    const walk = (id: string, depth: number): void => {
      const vis = nodes.get(id)!;
      vis.tx = PAD + depth * INDENT;
      vis.ty = PAD + index * ROW_H;
      index++;
      for (const child of children.get(id) ?? []) walk(child, depth + 1);
    };
    // ICRF first, then the rest alphabetically by what the reader actually sees.
    const roots = (children.get(null) ?? []).sort((a, b) => {
      const [la, lb] = [label(a), label(b)];
      return la === "ICRF" ? -1 : lb === "ICRF" ? 1 : la.localeCompare(lb);
    });
    for (const root of roots) walk(root, 0);
  }

  function setFocused(id: string | null): void {
    for (const vis of nodes.values()) vis.g.classList.toggle("focused", vis.id === id);
  }

  // Animation loop: rows glide to their targets; elbow guides track them.
  (function animate(): void {
    for (const vis of nodes.values()) {
      vis.x += (vis.tx - vis.x) * 0.18;
      vis.y += (vis.ty - vis.y) * 0.18;
      vis.g.setAttribute("transform", `translate(${vis.x.toFixed(1)}, ${vis.y.toFixed(1)})`);
      const parent = vis.parent ? nodes.get(vis.parent) : undefined;
      if (parent) {
        const px = parent.x + 6;
        const py = parent.y + ROW_H / 2 + 5;
        const cy = vis.y + ROW_H / 2;
        vis.edge.setAttribute("d", `M ${px.toFixed(1)} ${py.toFixed(1)} V ${cy.toFixed(1)} H ${(vis.x + 1).toFixed(1)}`);
      } else {
        vis.edge.setAttribute("d", "");
      }
    }
    requestAnimationFrame(animate);
  })();

  return { update, setFocused };
}
