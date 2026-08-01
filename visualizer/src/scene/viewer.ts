/**
 * The Eyes-on-the-Solar-System-style 3D view.
 *
 * True-scale bodies with always-visible screen markers and labels, orbit
 * guide lines, click-to-focus with a short fly-to. Precision strategy:
 * Three.js matrices are float64 in JS, so we keep everything in km and
 * re-root the world each frame so the *focused* entity sits at the origin —
 * GPU float32 error then lives far from the camera. Combined with a
 * logarithmic depth buffer this holds from Neptune (4.5e9 km) down to a
 * docked miner posed in millimetres.
 */

import {
  AmbientLight,
  BoxGeometry,
  BufferAttribute,
  BufferGeometry,
  Color,
  ConeGeometry,
  CylinderGeometry,
  Group,
  DynamicDrawUsage,
  Line,
  LineBasicMaterial,
  LineLoop,
  LineSegments,
  Mesh,
  MeshBasicMaterial,
  MeshStandardMaterial,
  OctahedronGeometry,
  PerspectiveCamera,
  PointLight,
  Scene,
  SphereGeometry,
  Vector3,
  WebGLRenderer,
} from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import type { LedgerData } from "../arrow/loader";
import type { NameBook } from "../core/identity";
import type { TransformTopology } from "../core/topology";
import { buildTimelines, type EntityTimeline, type PoseState } from "../core/timeline";
import { WorldResolver } from "../core/worldResolve";
import { posToKm } from "./units";
import type { EntityNode } from "./sceneGraph";
import { displayInfo, focusDistanceKm } from "./registry";
import { buildSceneGraph, type SceneGraph } from "./sceneGraph";
import { guideLine } from "./orbits";
import {
  applyBodyTextures,
  makeEarthClouds,
  makeSaturnRings,
  makeSunGlow,
} from "./textures";

interface LabelEntry {
  id: string;
  el: HTMLElement;
  anchor: Vector3;
}

export class Viewer {
  readonly graph: SceneGraph;
  /** Display names for the ids in `graph` — handed to the UI panels too. */
  readonly names: NameBook;
  focusedId: string | null = null;
  onFocus: ((id: string) => void) | null = null;
  /** Called once per frame with elapsed seconds — drives the playback clock. */
  onBeforeFrame: ((dtSec: number) => void) | null = null;

  private timelines: Map<string, EntityTimeline>;
  private resolver!: WorldResolver;
  private lastFrameMs = performance.now();

  private scene = new Scene();
  private camera: PerspectiveCamera;
  private renderer: WebGLRenderer;
  private controls: OrbitControls;
  private labels: LabelEntry[] = [];
  private labelLayer: HTMLElement;
  private markerMeshes: { mesh: Mesh; node: Group }[] = [];
  private flyFrom: Vector3 | null = null;
  private flyTo: Vector3 | null = null;
  private flyT = 1;
  private tmp = new Vector3();
  private tmp2 = new Vector3();
  /** [0]: entity→entity edges (bright); [1]: entity→frame-anchor edges (dim). */
  private treeLines: LineSegments[] = [];
  treeLayerOn = true;

  constructor(
    private container: HTMLElement,
    labelLayer: HTMLElement,
    data: LedgerData,
    topo: TransformTopology,
  ) {
    this.labelLayer = labelLayer;
    this.names = data.names;
    this.graph = buildSceneGraph(data.entities, topo, data.minEpochNs);
    this.timelines = buildTimelines(data.entities);
    this.resolver = new WorldResolver(this.timelines);
    this.scene.add(this.graph.root);
    this.setTime(data.minEpochNs);

    this.camera = new PerspectiveCamera(50, 1, 1e-4, 5e10);
    this.camera.up.set(0, 0, 1); // dummy orbits live near the xy-plane
    this.renderer = new WebGLRenderer({ antialias: true, logarithmicDepthBuffer: true });
    this.renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));
    container.append(this.renderer.domElement);

    this.controls = new OrbitControls(this.camera, this.renderer.domElement);
    this.controls.enableDamping = true;
    this.controls.dampingFactor = 0.08;

    this.scene.add(new AmbientLight(0x404860, 1.2));
    this.decorate(data, topo);
    this.buildTreeLines();

    window.addEventListener("resize", () => this.resize());
    this.resize();

    this.overview(); // whole-system opening shot
    this.renderer.setAnimationLoop(() => this.tick());
  }

  /** Poses every entity at `t` (ns since J2000 TAI), re-parenting live. */
  setTime(epochNs: bigint): void {
    const handoffs: { node: EntityNode; state: PoseState }[] = [];
    for (const [id, timeline] of this.timelines) {
      const node = this.graph.nodes.get(id);
      if (!node) continue;
      const state = timeline.at(epochNs);
      const target = this.graph.frameGroup(state.frame);
      if (node.group.parent !== target) {
        target.add(node.group);
        node.chain = [id, state.frame];
      }
      node.group.position.set(...state.positionKm);
      const [w, x, y, z] = state.quaternion;
      node.group.quaternion.set(x, y, z, w);
      node.row = state.row;
      if (state.crossFrame && state.nextRow) handoffs.push({ node, state });
    }

    // Frame hand-offs glide *relative to the incoming parent*: the old pose is
    // re-expressed in the new frame at the old sample's epoch, then that local
    // offset interpolates to the first new-frame row — so the entity rides
    // along with a fast-moving new parent (pursuit) instead of lerping through
    // absolute space while the parent runs away. Attachment (the tree) still
    // flips exactly at the event epoch — this only shapes motion between rows.
    if (handoffs.length > 0) {
      this.graph.root.updateMatrixWorld(true);
      for (const { node, state } of handoffs) {
        const next = state.nextRow!;
        const relA = this.resolver.reexpress(
          state.positionKm,
          state.frame,
          next.frameId,
          state.row.epochNs,
        );
        const relB = posToKm(next.position, next.unitsPos);
        const a = state.alpha;
        this.tmp.set(
          relA[0] + (relB[0] - relA[0]) * a,
          relA[1] + (relB[1] - relA[1]) * a,
          relA[2] + (relB[2] - relA[2]) * a,
        );
        const oldFrame = this.graph.frameGroup(state.frame);
        const newFrame = this.graph.frameGroup(next.frameId);
        newFrame.localToWorld(this.tmp); // ride the new parent at display time
        node.group.position.copy(oldFrame.worldToLocal(this.tmp));
      }
    }
  }

  /** Zoom out to the whole-system view (Sun focus, all orbits in frame). */
  overview(): void {
    const sun = this.idNamed("Sun");
    if (sun) this.focus(sun, 4.2e9);
  }

  /** The id whose display key is `key`, if this ledger carries one. */
  private idNamed(key: string): string | undefined {
    for (const id of this.graph.nodes.keys()) {
      if (this.names.key(id) === key) return id;
    }
    return undefined;
  }

  /** Show or hide the live transform-tree edge layer. */
  setTreeLayer(on: boolean): void {
    this.treeLayerOn = on;
    for (const lines of this.treeLines) lines.visible = on;
  }

  /** Fly the camera to `id`. */
  focus(id: string, distanceKm = focusDistanceKm(id, this.names)): void {
    if (!this.graph.nodes.has(id)) return;
    this.focusedId = id; // per-frame re-rooting in tick() puts it at the origin
    this.controls.target.set(0, 0, 0);

    // Preserve viewing direction, animate the distance.
    const dir = this.camera.position.clone().normalize();
    if (!Number.isFinite(dir.lengthSq()) || dir.lengthSq() < 1e-12) dir.set(0.4, -1, 0.45).normalize();
    this.flyFrom = this.camera.position.clone();
    this.flyTo = dir.multiplyScalar(distanceKm);
    this.flyT = 0;
    this.controls.minDistance = Math.max(distanceKm * 1e-3, 1e-4);
    this.onFocus?.(id);
  }

  private tick(): void {
    const now = performance.now();
    const dt = Math.min((now - this.lastFrameMs) / 1000, 0.25); // clamp tab-switch jumps
    this.lastFrameMs = now;
    this.onBeforeFrame?.(dt);

    // Re-root the world each frame so the (possibly moving) focus target stays
    // at the origin — GPU float32 error lives far from the camera.
    const focusNode = this.focusedId ? this.graph.nodes.get(this.focusedId) : undefined;
    if (focusNode) {
      this.graph.root.position.set(0, 0, 0);
      this.graph.root.updateMatrixWorld(true);
      focusNode.group.getWorldPosition(this.tmp);
      this.graph.root.position.copy(this.tmp).negate();
    }

    if (this.flyTo && this.flyFrom && this.flyT < 1) {
      this.flyT = Math.min(1, this.flyT + 0.025);
      const e = 1 - Math.pow(1 - this.flyT, 3); // ease-out cubic
      this.camera.position.lerpVectors(this.flyFrom, this.flyTo, e);
    }
    this.controls.update();

    // Screen-constant scale for marker meshes (assets without a real radius).
    for (const { mesh, node } of this.markerMeshes) {
      const d = this.camera.position.distanceTo(node.getWorldPosition(this.tmp));
      mesh.scale.setScalar(Math.max(d * 0.006, 1e-6));
    }

    this.updateTreeLines();
    this.renderer.render(this.scene, this.camera);
    this.updateLabels();
  }

  private decorate(data: LedgerData, topo: TransformTopology): void {
    for (const [id, node] of this.graph.nodes) {
      const key = this.names.key(id);
      const info = displayInfo(id, this.names);
      const color = new Color(info.color);

      if (info.bodyRadiusKm) {
        const r = info.bodyRadiusKm;
        const mat =
          info.kind === "star"
            ? new MeshBasicMaterial({ color })
            : new MeshStandardMaterial({ color, roughness: 0.9, metalness: 0 });
        const body = new Mesh(new SphereGeometry(r, 48, 24), mat);
        node.group.add(body);
        void applyBodyTextures(body, key, r);

        if (info.kind === "star") {
          node.group.add(new PointLight(0xfff2d5, 2.5, 0, 0));
          node.group.add(makeSunGlow(r));
        }
        if (key === "SATURN_BARYCENTER") {
          void makeSaturnRings(r).then((rings) => rings && node.group.add(rings));
        }
        if (key === "Earth") {
          void makeEarthClouds(r).then((clouds) => clouds && node.group.add(clouds));
        }
      } else {
        // Screen-scaled marker for assets; shape hints at what it is.
        const geom =
          key.includes("spaceship") ? new ConeGeometry(0.45, 1.5, 12)
          : key.includes("miner") ? new CylinderGeometry(0.5, 0.5, 0.9, 6)
          : new OctahedronGeometry(1);
        const mesh = new Mesh(geom, new MeshBasicMaterial({ color }));
        node.group.add(mesh);
        this.markerMeshes.push({ mesh, node: node.group });
      }

      // Orbit / trajectory guide lines — one per frame the entity has rows in,
      // each attached to that frame's group (a re-parented entity keeps both
      // its Earth-frame spiral and its Moon-frame capture arc, like Eyes).
      const rows = data.entities.get(id);
      if (rows) {
        for (const frame of new Set(rows.map((r) => r.frameId))) {
          const guide = guideLine(rows, frame);
          if (!guide) continue;
          const geom = new BufferGeometry();
          geom.setAttribute("position", new BufferAttribute(guide.points, 3));
          const mat = new LineBasicMaterial({
            color,
            transparent: true,
            opacity: guide.kind === "circle" ? 0.28 : 0.55,
          });
          const line = guide.kind === "circle" ? new LineLoop(geom, mat) : new Line(geom, mat);
          this.graph.frameGroup(frame).add(line);
        }
      }

      // HTML label + dot (always visible, like Eyes).
      const el = document.createElement("div");
      el.className = "map-label";
      el.innerHTML = `<span class="dot" style="background:${info.color}"></span>${info.label}`;
      el.addEventListener("click", () => this.focus(id));
      this.labelLayer.append(el);
      this.labels.push({ id, el, anchor: new Vector3() });
    }
  }

  private updateLabels(): void {
    const w = this.container.clientWidth;
    const h = this.container.clientHeight;
    for (const label of this.labels) {
      const node = this.graph.nodes.get(label.id)!;
      node.group.getWorldPosition(label.anchor);
      const v = label.anchor.project(this.camera);
      const behind = v.z > 1 || v.z < -1;
      // Hide a child label when it overlaps its parent from far away (Moon vs Earth).
      if (behind) {
        label.el.style.display = "none";
        continue;
      }
      label.el.style.display = "";
      label.el.style.transform = `translate(${((v.x + 1) / 2) * w}px, ${((1 - v.y) / 2) * h}px)`;
      label.el.classList.toggle("focused", label.id === this.focusedId);
    }
  }

  /**
   * The parentage layer: one line segment per entity, child → parent origin,
   * in world space. Rebuilt every frame from the *actual* scene-graph
   * attachment, so a re-parent during playback visibly re-routes its edge.
   */
  private buildTreeLines(): void {
    const styles = [
      { color: 0x4fc3f7, opacity: 0.75 }, // entity → parent entity
      { color: 0x55627e, opacity: 0.3 }, // entity → astronomical frame anchor
    ];
    this.treeLines = styles.map(({ color, opacity }) => {
      const geom = new BufferGeometry();
      const attr = new BufferAttribute(new Float32Array(this.graph.nodes.size * 6), 3);
      attr.setUsage(DynamicDrawUsage);
      geom.setAttribute("position", attr);
      const lines = new LineSegments(
        geom,
        new LineBasicMaterial({ color, transparent: true, opacity }),
      );
      lines.frustumCulled = false;
      lines.visible = this.treeLayerOn;
      this.scene.add(lines);
      return lines;
    });
  }

  private updateTreeLines(): void {
    if (!this.treeLayerOn) return;
    this.graph.root.updateMatrixWorld(true);
    const attrs = this.treeLines.map(
      (l) => l.geometry.getAttribute("position") as BufferAttribute,
    );
    const counts = [0, 0];
    for (const node of this.graph.nodes.values()) {
      const parent = node.group.parent ?? this.graph.root;
      // Edge kind from the actual attachment: another entity's group → kinship
      // edge (bright); the root or any non-entity group → frame-anchor spoke (dim).
      const kind = this.graph.nodes.has(parent.name) ? 0 : 1;
      const attr = attrs[kind]!;
      node.group.getWorldPosition(this.tmp);
      parent.getWorldPosition(this.tmp2);
      attr.setXYZ(counts[kind]!++, this.tmp.x, this.tmp.y, this.tmp.z);
      attr.setXYZ(counts[kind]!++, this.tmp2.x, this.tmp2.y, this.tmp2.z);
    }
    this.treeLines.forEach((lines, k) => {
      lines.geometry.setDrawRange(0, counts[k]!);
      attrs[k]!.needsUpdate = true;
    });
  }

  private resize(): void {
    const w = this.container.clientWidth;
    const h = this.container.clientHeight;
    this.camera.aspect = w / h;
    this.camera.updateProjectionMatrix();
    this.renderer.setSize(w, h);
  }
}
