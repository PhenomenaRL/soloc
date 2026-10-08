# Checks

## check_sim.py

`python check_sim.py out/sim_study_0.arrow` loads the file into the server (replacing its
ledger), prints one PASS/FAIL line per check and exits non-zero on any FAIL. It checks only the
fleets present in the file.

The checks need the full 5-day history. If the loaded ledger starts after t0, because the file
was recorded under a small `memory_limit` or the server's limit evicted part of it on load,
`check_sim` prints one FAIL naming where the history starts and stops. See
[memory_limit.md](memory_limit.md).

| Check | What it confirms |
|---|---|
| roster | `current_state` holds every facility, spacecraft, probe, robot, crawler, aircraft, ship, regatta entity and body, and nothing else; the regatta marks are absent, since they were lifted hours before the window ends |
| names | the `.names.arrow` sibling reloaded a name for every sim entity |
| units / timescale | only `km`/`m` stored, every row normalised to TAI |
| schedule | each entity's stored epochs are exactly its model's schedule |
| zero-order hold | every row framed on a spacecraft shares its epoch with a row of that spacecraft |
| robot frames | every robot row is framed on its own facility |
| site area | every stored robot position inside the site square, at z = 0 |
| ground level | robot rows at every 24 h resolved through the facility to IAU_EARTH/IAU_MOON sit on the surface and near the site |
| ICRF | the same rows in ICRF sit at the body's radius from its snapshot position |
| orbits | per orbit, stored radii inside Kepler's periapsis–apoapsis range and the nodal period (timed from z crossings) equal to Kepler's; GEO instead hangs still in IAU_EARTH |
| launches | pad rows at the pad's offset on the facility and resolving onto the pad; the ascent climbs monotonically |
| landings | the descent stays above the ground; after touchdown the lander resolves onto its landing point |
| Moon flight | each leg is on its frame (IAU_EARTH, GCRF, IAU_MOON); resolved to GCRF, the first row after each hand-off continues the curve of the three rows before it; the approach bottoms out at the capture altitude and the orbit holds it |
| cargo | CARGO-01 is framed on KSC, the ship and Shackleton in turn; resolved through the ledger it is on the ground at both ends and within 1 m of the ship at 9 epochs in between |
| Mars transfer | the arc leaves Earth's centre and, propagated 330 days, meets Mars's; the stored rows, made Sun-centred, keep their energy and angular momentum |
| Parker Solar Probe | the stored rows equal the Horizons table; its distance from the Sun agrees with the Sun's snapshot rows; perihelion falls inside the window at 9.8–9.9 solar radii |
| crawlers | on the hull in the stored frame, and within hull reach of the host once both are resolved to the host's frame |
| disembark | the crawler is framed on the facility afterwards, at ground level inside the site |
| topology | the parent changes are exactly the 10 that the models' phases predict |
| aircraft | every row between the lowest field and cruise altitude; at rest only on an airfield; 900 km/h (±1%) ground speed at cruise altitude |
| ships | at sea level; 22-40 km/h (±1%) under way; at rest only at a berth; every row on water |
| regatta frame | every boat, mark and buoy row is framed on the venue at z = 0, and sits on water (`scenario.BASIN_WATER`); 6 boat rows resolved to IAU_EARTH land on the venue's ENU plane |
| race | each boat's stored rows, replayed through the referee, start after the gun, round W, gate, W, gate, W in order and finish |
| polar | while sailing, every row's speed is at most the polar speed for the wind at its position and time (`sim/wind.py`), along its heading |
| docks | outside race day the boats and RC-BOAT sit at rest in their berths, and all are back by 18:00 ADT |
| RC boat, marks | RC-BOAT holds the line's starboard end from the warning to the last finish; marks and buoys never move |

Resolving rows through the ledger at historical epochs scans for the parent's pose once per
`(frame, epoch)`. The resolution checks therefore run on a few epochs, and the per-row checks
run on the stored values.

## tests/smoke_test.py

`python -m tests.smoke_test` checks the server contract the sim relies on. It needs an empty ledger,
so restart `serve.sh` first.

| Check | What it confirms |
|---|---|
| `get_schema` | the client builds batches against the server's own schema |
| mint vectors | Python ids match `frozen_mint_vectors` in `identity.rs` |
| append per timescale | TAI, UTC, GPST and TT rows are stored as the same TAI instant |
| (a) epoch base | the JSON `epoch_tai_s` counts from J1900 |
| (b) Mars | body 499 resolves (it comes from `mar099s.bsp`; `de440s` lacks it) |
| (c) quaternion | `[w,x,y,z]` rotates child-frame vectors into the parent frame |
| round trip | `save_ledger` → `load_ledger` keeps every row and writes the `.names.arrow` sibling |
