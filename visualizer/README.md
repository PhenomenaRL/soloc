# soloc visualizer

An Eyes-on-the-Solar-System-style front end for soloc: reads spacetimestamp
record batches (Arrow IPC — the same bytes `soloc-server` streams over Flight),
re-derives the transform tree in the browser, and renders every entity in a 3D
solar-system map with time playback.

## Quick start

```bash
# 1. Generate the demo ledger fixture (gitignored; optional — see
#    "Supplying your own ledger" below for other ways in).
#    Downloads DE440s + PCK on first run (~150 MB, cached), or point
#    SOLOC_KERNEL_PATHS at kernels you already have.
cargo run -p soloc-ledger --example gen_visualizer_fixture

# 2. Run the app
cd visualizer
npm install
npm run dev     # → http://localhost:5173

# Tests (unit; the integration suites skip when the fixture is absent)
npm test
```

## Supplying your own ledger

The app never requires the demo fixture specifically — anything that
`loadLedger` can parse (a real `entity_id` + `spacetimestamp` Arrow IPC file,
the same bytes `soloc-server` streams over Flight) works. Three ways in, all
wired up in `main.ts`:

- **Drag and drop** a `.arrows` file anywhere on the page. Drop its
  `<name>.arrows.names.arrow` sibling alongside it (multi-file drop) to get
  resolved names instead of hyphenated ids.
- **"⇪ load ledger"** in the toolbar opens a file picker for the same thing.
- **`?src=<url>`** on the page URL fetches a ledger from anywhere reachable
  over HTTP(S) — e.g. `http://localhost:5173/?src=https://example.com/my.arrows`.
  The `<url>.names.arrow` sibling is fetched automatically, best-effort. The
  URL needs CORS enabled for cross-origin hosts; same-origin (e.g. served
  alongside the app, or via a dev-server proxy) needs nothing extra.

With none of those, it falls back to `/data/dummy.arrows` — the generated
fixture from step 1 above.

Loading a new ledger tears down and rebuilds the whole scene in place (no
page reload), so it also works mid-session — drop a different file to swap
datasets without losing your window state... other than the playback clock,
which resets to the new ledger's own time window.

Not yet wired up: a live Arrow Flight (gRPC-web) connection straight to
`soloc-server`, which would skip the flat-file step entirely for data backed
by `soloc-server`'s own `file://`/`s3://`/`gs://`/`az://` storage. See
"v2 candidates" below.

## What you're looking at

A 7-day window, 2026-08-01 → 08-08 TAI, written through a real `Ledger` —
validated, TAI-normalised, topology-checked.

The **celestial bodies are real**: the Sun, the eight planets and the Moon come
from `celestial_snapshot` against a NAIF almanac, so their positions,
velocities, body-fixed orientations, angular velocities and masses are whatever
anise resolves. The Sun therefore traces its own small orbit about the
barycentre instead of sitting at the origin, and Earth turns once a day under
whatever is parented to it.

Each body is anchored wherever the base DE440s kernels actually put one. The
Sun, Mercury, Venus, Earth and the Moon have body centres, so they carry real
IAU body-fixed orientations. **Mars and the giants** appear at their **system
barycentre** — DE440s carries no body centre for them, and a real one would need
a satellite SPK (`mar097.bsp`, `jup365.bsp`, …). The offset is the moons' share
of system mass: tens of metres for Mars, a few hundred kilometres for the
giants, invisible at any zoom the map offers.

Five demo entities are synthetic, but they hang off those real states: an
asteroid, a surveyed Moon base with a rover driving away from it (metres), a
spaceship that performs a trans-lunar injection to where the Moon actually is,
and an asteroid miner that docks (millimetres). Two scripted **re-parenting
events** are the payload:

- `demo:spaceship-1`: Earth → Moon at T+84 h
- `demo:miner-1`: ICRF → `demo:asteroid-1` at T+120 h (docking)

Both hand-offs are continuous *in world space*: each trajectory is shaped in an
inertial frame and only then expressed in whichever parent's coordinates the row
declares, so a re-parent changes the numbers without moving the spacecraft.

The base is where the tree gets deep: `rover → base → Moon → ICRF`. It never
moves in the Moon's frame — a fixed installation is exactly the case where
storing raw costs nothing — and it carries a real **local-level** orientation,
east/north/up. So the rover's rows are plain metres from the front door, and it
takes the base's orientation *and* the Moon's to turn them into an ICRF
position. That is the whole argument for a transform tree in one entity.

Paths are drawn **only up to the displayed epoch**. A trail grows one sample at
a time as it is flown, and no line is drawn at all for a frame the entity has
not entered yet — otherwise the spaceship's lunar orbit hangs off the Moon,
riding along with it, for three days before launch. A fitted orbit circle has no
per-point time (it interpolates a whole orbit from a sliver of samples), so it
appears whole the moment the entity enters that frame.

Non-astronomical entities are drawn as their three **body axes** rather than a
shape. The `dimensions` column is not modelled yet, but orientation is real and
worth seeing — and it is what shows the rover turning with the Moon beneath it.

## Identity

Every identity column (`entity_id`, `frame_id`, `source_id`) is a
`FixedSizeBinary(16)` prescribed id, not a string. The front end keys on the
canonical hyphenated form of those bytes, and looks a *name* up only when
something is about to be rendered — from `dummy.arrows.names.arrow`, the
display-only registry `Ledger::save_ipc` writes beside the ledger. A fixture
without that sibling still loads; ids simply render as themselves.

One consequence is worth knowing before reading the code: an astronomical id
embeds anise's `(ephemeris_id, orientation_id)` pair, so **a body is its own
body-fixed frame**. `IAU_MOON` is not a separate anchor that has to be mapped
onto a host body — it *is* the Moon's id, and resolves through the Moon's own
rows, orientation included. That is what puts the rover on a Moon that really
rotates, and it is why there is no frame-host table anywhere in `src/`.

## Controls

| Input | Action |
|---|---|
| click label / list entry / tree node | fly to entity |
| checkbox in the entity list | show / hide that entity (children keep rendering) |
| space | play / pause |
| `T` | toggle 3D transform-tree edges (cyan = entity→entity, gray = frame anchor) |
| `P` | toggle tree panel + row inspector |
| `O` | zoom out to the whole solar system |
| scrubber ticks | re-parent events (hover for details) |

## Architecture

```
src/core/identity.ts      prescribed ids → hyphenated strings; the name registry (display only)
src/arrow/loader.ts       Arrow IPC → typed rows (ids decoded, vocabulary codes decoded from
                          field metadata, epochs → bigint ns since J2000 TAI)
src/core/epoch.ts         (duration_centuries, duration_ns) ↔ bigint; TAI calendar display
src/core/topology.ts      string-keyed port of TransformTree; parentAt/chainAt history queries
src/core/timeline.ts      per-entity interpolation; frame hand-offs flagged, never blended across frames
src/core/worldResolve.ts  world pose of any frame at any epoch (used to glide hand-offs relative to the new parent)
src/scene/sceneGraph.ts   Object3D tree mirroring the transform tree; native units converted per node
src/scene/orbits.ts       guide lines from the samples themselves; epochs per point so a path reveals as it is flown
src/scene/registry.ts     display metadata, keyed by resolved name rather than by opaque id
src/scene/viewer.ts       renderer: float-precision via per-frame re-rooting on the focused entity
src/ui/                   entity list, playback bar, animated tree panel, raw-row inspector
```

Design commitments, mirroring soloc itself: rows are consumed **raw** (native
frame, native units — conversion happens per scene node at read time); the
scene graph *is* the transform tree (a re-parent is literally an `Object3D`
re-attachment at the event epoch); interpolation across a re-parent is done
relative to the incoming parent frame, never by blending coordinates from two
frames.

Planet textures load from CDN with local override — see
`public/textures/README.md`. Fully offline the map still works (flat colors).
The Moon and Earth accept a resolution ladder (`moonmap8k.jpg` → `4k` → `2k` →
the 1k CDN baseline), so a higher-resolution map is installed by dropping a file
in, not by editing code; a missing drop-in degrades to the map that shipped
rather than to a blank body. Textures are loaded at the GPU's maximum
anisotropy, which is what keeps a surface sharp when viewed along it — the case
that matters once you are down at the rover.

Body spheres are built by `bodySphereGeometry`, which rotates the mesh so an
equirectangular map lands the way a body-fixed frame expects: north pole on
**+Z**, prime meridian down the middle of the image on **+X**, longitude running
east with `u`. Three.js builds spheres Y-up, so a raw `SphereGeometry` lays every
map on its side — invisible while a planet is a dot, obvious once there is a
base at 5°N 20°W to look for. `scene/textures.test.ts` pins the convention by
reading the UVs back off the geometry.

## v2 candidates (out of scope here)

- Live Arrow Flight (gRPC-web) connection to `soloc-server`
- Real body centres for Mars and the giants, given a satellite SPK
- Body-centred *inertial* frames (GCRF and friends) as first-class anchors —
  today an id with no rows resolves to the world root, which is right for ICRF
  and wrong for GCRF; nothing in the fixture uses one
- Covariance ellipsoid rendering (inspector shows raw values today)
