/**
 * Raw-row inspector: the exact stored values behind the focused entity's
 * current pose — native frame, native units, covariances and all. Deliberately
 * un-converted ("store raw, reproject on demand" made visible).
 *
 * Identity columns show the id *and* the name it resolves to, in that order:
 * the 16 bytes are what the row actually stores and what a Rust-side query
 * takes, and the name is the courtesy.
 */

import { formatTai } from "../core/epoch";
import { displayInfo } from "../scene/registry";
import type { Viewer } from "../scene/viewer";

export interface Inspector {
  update(): void;
}

const fmt = (n: number): string => {
  if (Number.isInteger(n) && Math.abs(n) < 1e15) return String(n);
  const p = n.toPrecision(6);
  return p.includes("e") ? p : String(Number.parseFloat(p));
};

const vec = (v: readonly number[] | null, suffix = ""): string | null =>
  v === null ? null : `[${v.map(fmt).join(", ")}]${suffix ? " " + suffix : ""}`;

export function buildInspector(container: HTMLElement, viewer: Viewer): Inspector {
  container.replaceChildren();
  const title = document.createElement("div");
  title.className = "side-title";
  title.textContent = "row inspector";
  const body = document.createElement("div");
  body.className = "insp-body";
  container.append(title, body);

  let lastKey = "";

  function update(): void {
    const id = viewer.focusedId;
    const node = id ? viewer.graph.nodes.get(id) : undefined;
    if (!node) {
      body.textContent = "click an entity";
      lastKey = "";
      return;
    }
    const row = node.row;
    const key = `${id}:${row.rowIndex}`;
    if (key === lastKey) return; // same driving row — no DOM churn during playback
    lastKey = key;

    const names = viewer.names;
    const named = (rowId: string): string => {
      const label = names.label(rowId);
      return label === rowId ? rowId : `${rowId}  (${label})`;
    };

    const kv: [string, string | null][] = [
      ["entity_id", named(row.entityId)],
      ["frame_id", named(row.frameId)],
      ["epoch", formatTai(row.epochNs)],
      ["position", vec(row.position, row.unitsPos)],
      ["units_pos", row.unitsPos],
      ["quaternion", vec(row.quaternion, "[w x y z]")],
      ["timescale_id", row.timescaleId],
      ["source_id", named(row.sourceId)],
      ["estimate_type", row.estimateType],
      ["velocity", vec(row.velocity)],
      ["angular_velocity", vec(row.angularVelocity)],
      ["acceleration", vec(row.acceleration)],
      ["mass_kg", row.massKg === null ? null : fmt(row.massKg)],
      ["dimensions", vec(row.dimensionsM, "m")],
      ["position_cov", vec(row.positionCovariance, "(upper-△ 6)")],
      ["orientation_cov", vec(row.orientationCovariance, "(upper-△ 6)")],
      ["state_cov", row.stateCovariance ? `21 values, σ_pos ≈ ${fmt(Math.sqrt(Math.abs(row.stateCovariance[0] ?? 0)))} ${row.unitsPos}` : null],
      ["row #", String(row.rowIndex)],
    ];

    body.textContent = "";
    const head = document.createElement("div");
    head.className = "insp-head";
    const info = displayInfo(row.entityId, names);
    head.innerHTML = `<span class="dot" style="background:${info.color}"></span>${info.label}`;
    body.append(head);
    for (const [k, v] of kv) {
      if (v === null) continue;
      const line = document.createElement("div");
      line.className = "insp-line";
      line.innerHTML = `<span class="k">${k}</span><span class="v"></span>`;
      line.querySelector<HTMLElement>(".v")!.textContent = v;
      body.append(line);
    }
  }

  update();
  return { update };
}
