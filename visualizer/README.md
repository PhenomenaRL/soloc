# soloc visualizer

An Eyes-on-the-Solar-System-style front end for soloc: reads spacetimestamp
record batches (Arrow IPC — the same bytes `soloc-server` streams over Flight),
re-derives the transform tree in the browser, and renders every entity in a 3D
solar-system map with time playback.

## Quick start

```bash
# 1. Generate the ledger fixture (gitignored; required once).
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

Three demo entities are synthetic, but they hang off those real states: an
asteroid, a spaceship that performs a trans-lunar injection to where the Moon
actually is, and an asteroid miner that docks (millimetres). Two scripted
**re-parenting events** are the payload:

- `demo:spaceship-1`: Earth → Moon at T+84 h
- `demo:miner-1`: ICRF → `demo:asteroid-1` at T+120 h (docking)

Both hand-offs are continuous *in world space*: each trajectory is shaped in an
inertial frame and only then expressed in whichever parent's coordinates the row
declares, so a re-parent changes the numbers without moving the spacecraft.

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
rows, orientation included. That is what puts the post-TLI spaceship on a Moon
that really rotates, and it is why there is no frame-host table anywhere in
`src/`.

## Controls

| Input | Action |
|---|---|
| click label / list entry / tree node | fly to entity |
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

## v2 candidates (out of scope here)

- Live Arrow Flight (gRPC-web) connection to `soloc-server`
- Real body centres for Mars and the giants, given a satellite SPK
- Body-centred *inertial* frames (GCRF and friends) as first-class anchors —
  today an id with no rows resolves to the world root, which is right for ICRF
  and wrong for GCRF; nothing in the fixture uses one
- Covariance ellipsoid rendering (inspector shows raw values today)
