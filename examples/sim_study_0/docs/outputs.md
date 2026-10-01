# Inspecting a run

## Plots

`python -m tools.plot_sim out/sim_study_0.arrow` (~100 s) writes to `out/plots/`:

| File | Shows |
|---|---|
| `robots_{ksc,and,jsc,shk}.png` | each site's robot rows in site ENU over the road grid, one panel per robot titled with its role; Shackleton's includes CRAWLER-01 after it disembarks, and KSC's and Shackleton's include CARGO-01 |
| `orbits.png` | every orbiter body-centred with ICRF axes (x–y and x–z) around Earth, Moon and Mars |
| `altitudes.png` | altitude around each launch and landing, resolved through the ledger |
| `cislunar_selene-1.png` | SELENE-1 from liftoff to touchdown, resolved to GCRF: Earth-centred with the Moon's path, and Moon-centred in the orbit plane |
| `transfer_mars-transfer-1.png` | the Mars transfer Sun-centred: Earth's and Mars's paths over the same dates, the whole arc, and the stored 5 days on it |
| `heliocentric.png` | Parker Solar Probe Sun-centred, with perihelion marked and the Sun to scale |
| `aircraft.png` | aircraft tracks over the land polygons |
| `flight_altitudes.png` | each aircraft's altitude over the 5 days |
| `ships.png` | ship tracks over the land polygons, with every full lane dotted and the canal boxes outlined |

## 3D viewer

`python -m tools.export_viewer out/sim_study_0.arrow` turns a saved ledger into one standalone
HTML file, `out/sim_study_0_3d.html` (~43 MB). The tracks, the body ephemerides, the Earth
coastlines and three.js are inlined into `tools/viewer_template.html`, so the page opens offline by double-click
in any WebGL browser. Building it needs a running server.

It is one scene, nested the way the ledger's frames are:
- **ICRF:** the Sun, Mercury, Venus, Earth, the Moon and Mars, at their true positions, with a
  full orbit drawn for each (the Moon's around Earth). Lighting comes from the Sun. Parker
  Solar Probe and MARS-TRANSFER-1 are here, with the whole Mars arc dashed.
- **GCRF:** Earth-centred and not turning. SELENE-1's translunar leg is here.
- **Each body's IAU frame:** turns with the body. Its spacecraft, aircraft and ships are in there,
  in body-fixed km as stored.
- **Each site's ENU frame:** sits on its body at the facility's stored pose, with its robots in
  metres over the road grid.

Body positions and orientations come from the server: a zero offset in each body frame is
exchanged to ICRF, at 5 min steps over the window and along each orbit. That is the same
resolution the ledger does. Rows framed on a facility (a craft on its pad or landed) are composed
through the facility's stored pose.

**Navigating:**
- "Go to" flies the camera to the solar system, a body or a site; "Follow" tracks any vehicle or
  robot. A craft that changes frame is followed across them: SELENE-1 from its pad, round the
  Earth, across GCRF and down to the Moon.
- Vehicles, trails and labels appear as you close in on their body, and robots on their site.
  Craft in GCRF or ICRF show from any distance.
- The camera can turn with the body or stay fixed in ICRF. Trails can be drawn body-fixed or
  non-rotating.
- Floating-origin rendering keeps it precise from 4 AU down to a metre.

Every entity is a small model with its body axes (x red, y green, z blue) at its interpolated
pose; models keep a constant size on screen. The controls:
- a time slider with ticks at the launches, burns, hand-offs, touchdowns, boardings and
  disembarks, and at Parker's perihelion
- play at 1 min/s up to 3 h/s
- trails from 15 min to the whole track
- labels, a hover readout (position in the entity's own frame), and a clickable legend that
  hides a category

Robots riding a craft are left out (crawlers on a hull, CARGO-01 in flight), since a 2 m hull
is invisible at orbit scale; following CARGO-01 holds at the pad until it steps off. On Earth sites
the survey tracks look jagged: a robot moves up to 45 m between its 30 s rows, and the survey
rows are 10 m apart, so straight lines between samples cut the corners.

## Rows: tools/view_sim.py

`tools.view_sim` prints a saved ledger as a table straight from the file (no server). Ids show as
names, vocabulary codes as tokens and epochs as UTC; columns that are empty in every shown row
are left out.

```bash
python -m tools.view_sim out/sim_study_0.arrow                    # first 20 rows
python -m tools.view_sim out/sim_study_0.arrow --entity AND-R03 --limit 10
python -m tools.view_sim out/sim_study_0.arrow --entity shackleton --tail --limit 5
python -m tools.view_sim out/sim_study_0.arrow --summary          # one line per entity: rows, time span, frames
python -m tools.view_sim out/sim_study_0.arrow --schema
```

`--entity` matches any part of a name, case-insensitively, and can be repeated.

## Topology: tools/topo_sim.py

`tools.topo_sim` summarises the frame topology: the parent/child tree at an instant (astro frames
as roots, each entity with its row count and cadence), then every parent change over the run.

```bash
python -m tools.topo_sim out/sim_study_0.arrow                    # tree at the last epoch + events
python -m tools.topo_sim out/sim_study_0.arrow --at 2026-09-02T12:00:00   # tree at a UTC instant
python -m tools.topo_sim out/sim_study_0.arrow --server grpc://localhost:50051
```

It derives topology from the rows as the ledger does: an entity's parent is the `frame_id` of
its latest row at or before t. `--server` also loads the file into a running server and checks
that its `export_topology` log matches the row-derived events exactly.

## Reload from Rust

The file is a plain soloc ledger. The server's `load_ledger` action, which `check_sim.py` uses,
is exactly this call:

```rust
use std::path::Path;
use soloc_ledger::ledger::Ledger;

let ledger = Ledger::load_ipc(Path::new("examples/sim_study_0/out/sim_study_0.arrow"), "entity_id")?;
```

`load_ipc` also picks up the `.names.arrow` sibling next to the file, if present.
