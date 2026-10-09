# Bearing dynamics

The factory's 6 bearings (two 6205-size deep-groove ball bearings per line) run a lumped
dynamics model, `sim/models/bearing.py`. The question it answers is how well soloc serves a
physics simulation: as the store of its states, the engine that composes them through a frame
tree, and the place an analysis reads them back from. The model integrates in numpy; soloc
does no integration.

## The model

A rigid shaft (shaft, rotor and both inner rings: 8.1 kg, 0.036 kg·m² transverse) sits on two
bearings at x = ∓0.15 m in the stator frame. Each bearing station has a radial displacement
u_k in the stator's y-z plane, and together the two stations carry the shaft's translation and
tilt. Ball j of bearing k sits at

    θ_kj = cage angle + 2πj/8 + ε_kj (pocket wander)

and carries Q = K·δ^1.5 when δ > 0, with

    δ = u_k · (cos θ, sin θ) − c_r/2 + Δd_kj − spall depth.

| Parameter | Value |
|---|---|
| K (ball on both races, in series) | 9.41×10⁹ N/m^1.5, from Brewe & Hamrock's approximations for the 6205 (groove conformity 0.52, steel E 208 GPa, ν 0.3); inner 2.60×10¹⁰, outer 2.73×10¹⁰ |
| Load | 300 N belt pull toward the belt at the shaft's +x end (0.2 m), plus the shaft's weight. Bearing 2 carries ~350 N; bearing 1 carries ~64 N the other way |
| Damping | 1,000 N·s/m per station and axis (ζ ≈ 3 % at the loaded mode) |
| Housing | rigid (the stator) |
| Contact resonance | ~470–670 Hz, from the loaded radial stiffness (3–7×10⁷ N/m per bearing) |
| Defect lines at 60 rpm | FTF 0.397, BSF 2.32, BPFO 3.18, 2×BSF 4.64, BPFI 4.82 Hz |

The cage turns at 0.397 × the shaft (1 − its slip), and each ball spins at −2.32 × the shaft
(1 − its own slip). Slip and wander are functions of the shaft's angle, not time, so they hold
still while the line stops. Wander is a sum of three seeded sines per ball.

Over the shift, the shaft is in quasi-static equilibrium at every row epoch, solved by Newton
to below 10⁻¹⁰ N. In the eight captures (10 s each at 04:15, 05:15, 06:15, 07:15, 08:45,
09:45, 10:45 and 11:45 UTC), RK4 integrates it at 20 µs, starting from equilibrium 0.2 s
early. The model writes every step to `bearing_truth.arrow` (12M rows), and the shaft rows
take every 10th step (5 kHz).

### Wear, by line

Each ball's diameter error, slip and wander phases come from its name's seed.

| Line | Ball Δd σ | Clearance c_r | Slip | Wander | Spall |
|---|---|---|---|---|---|
| STY-L1 | 0.1 µm | 10 µm | 0.5 % | ±0.05° | – |
| STY-L2 | 2 µm | 20 µm | 2 % | ±0.5° | – |
| STY-L3 | 2 µm | 20 µm | 2 % | ±0.5° | 1.5 mm × 20 µm on ball 1 of bearing 2 |

### In the ledger

- **Shaft:** position = its centre's µm displacement in the stator, quaternion = tilt ∘ spin.
  Its capture rows add velocity and `acceleration`, the quantity an accelerometer reads.
- **Cage:** its slipped angle, with that speed in `angular_velocity`.
- **Ball:** offset in its cage. The ball sits on its pocket's wandered angle at the pitch radius
  when unloaded; when loaded it is seated on the outer race, at dm/2 + c_r/4 − Δd/2 − (Q/K_o)^⅔.
  It spins with its slip, and `acceleration` holds its contact load over its mass (the schema
  has no force column).
- **Inner rings:** these ride the shaft, so their displacement comes from the frame tree, with
  no rows of their own.

## Results

From `python -m tools.bearing_eval` on the factory ledger (2.2M rows):

| Measure | Result |
|---|---|
| Capture rows vs truth | 0 error (position and acceleration, bit for bit) |
| Shaft offset recovered via IAU_EARTH, 7 hops, km | 0.5 nm max error, against float64's 0.91 nm spacing at Earth radius |
| Cage slip read back from `angular_velocity` | equal to the model's for all 6 cages |
| Envelope at 2×BSF (slipped, 4.54 Hz) | L3 6.7× the floor; L1 1.3×, L2 1.0× |
| Capture RMS / kurtosis | L1 0.0004 m/s² / 32, L2 0.020 / 319, L3 1.77 / 578 |
| `load_ledger` | 7–9 s |
| Query one 10 s capture (time range) | 2.5 s, 150k rows |
| `current_state` of the 3 shafts | 50–60 ms |
| Exchange past capture rows to IAU_EARTH | **82 ms per row** |
| Ingest | 156–290k rows/s |
| Bytes per capture sample | 490 B in the ledger, 194 B in the truth table, 40 B raw (t, y, z, a_y, a_z) |

What the spectra show:
- **L1:** the healthy ball-pass ripple at BPFO and its harmonics.
- **L2:** irregular impacts from its oversized balls.
- **L3:** the classic ball-spall pattern. Impacts come only while the spalled ball crosses the
  load zone, once per cage turn, so FTF dominates with 2×BSF and its sidebands beside it.

All three are read from rows that went through the server and back.

## Verdict

Where soloc fits:
- **Storing states.** It stores them losslessly at any rate the schema can carry: 5 kHz shaft
  rows round-trip bit for bit.
- **Composing.** The frame tree gives every derived pose for free: the inner rings ride the
  shaft, and the balls ride their cage. µm motions survive 7 hops to an Earth-fixed frame at
  sub-nm precision, even in km.
- **Analysis.** The rows support real condition-monitoring analysis. Slip, orbits and the spall
  signature all come back out of the ledger.
- **Ingest.** At 150–290k rows/s, ingest is 10–20× what three 5 kHz channels need in real time.

Where it doesn't:
- **No integration.** soloc stores states; the physics runs in numpy. The run's cost moved to
  Python: the factory part went from 26 s to 319 s, about 140 s of it integration and the rest
  building 1.2M capture rows one at a time.
- **Resolving past rows.** At ~80 ms per row on a 2.2M-row ledger, resolving every capture row
  to Earth would take ~27 h. Composing in the shaft's own frame is free; across the tree, at
  history, it is the bottleneck.
- **Row overhead.** A ledger row costs ~12× a raw sample, because every row carries the full
  spacetimestamp, ids, quaternion and optional columns.
- **Querying.** A time-range query can't also filter by id, so pulling one channel's capture
  brings back everything in the window.
- **Schema.** The schema has no force or generic channel column, so contact loads ride in
  `acceleration` and the full truth lives in a side table.

## Simplifications

- The housing is rigid. There is no pedestal or sensor resonance, so captures read the shaft's
  own acceleration.
- Balls have no mass in the contact balance (quasi-static balls, the usual lumped model). They
  have no gyroscopic or centrifugal load, no EHL film and no traction model. Slip is imposed, not
  solved.
- The drive is stiff: the shaft speed is prescribed, so bearing friction never slows the belt.
- Wear is fixed for the shift; nothing grows with cycles.
- Each spall contact is a sharp step in depth, so it rings the resonance at entry and exit.
