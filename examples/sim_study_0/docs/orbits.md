# Orbits for your own front end

`query_orbits` gives each body's osculating orbit at an epoch: the Keplerian elements and a path
sampled from the kernels over one period, relative to the body's centre, in ICRF axes. It covers kernel bodies 
(the Sun's planets and their moons), not ledger entities.

## Ask for them

```python
from datetime import datetime

import numpy as np
import pyarrow as pa

from soloc_client import SolocClient, astronomical, id_bytes, positions, tai_ns_from_utc

client = SolocClient("grpc://localhost:50051")
EARTH, MOON, MARS = (astronomical(n, n) for n in (399, 301, 499))
t = tai_ns_from_utc(datetime(2026, 9, 3, 12))

orbits = client.orbits([EARTH, MOON, MARS], t)   # centres default to Sun, Earth, Sun
```

`centres=[...]` names other centres; `samples=N` sets the points per path (default 361, at most
100 000). From another language, send `do_action("query_orbits", body)` with:

```json
{"orbits": [{"body": {"ephemeris_id": 399, "orientation_id": 399}},
            {"body": {"ephemeris_id": 301, "orientation_id": 301},
             "centre": {"ephemeris_id": 399, "orientation_id": 399}}],
 "epoch_tai_s": 3997425637.0, "samples": 361}
```

`epoch_tai_s` is TAI seconds since J1900 (`epoch_tai_s(tai_ns)` converts). The reply is one Arrow
IPC file, one row per request.

## What comes back

| Column | Type | Meaning |
|---|---|---|
| `body_id`, `centre_id` | 16-byte id | the body, and what its elements and path are relative to |
| `duration_centuries`, `duration_ns` | int16, uint64 | the epoch, TAI since J2000 (`from_parts`) |
| `sma_km`, `ecc` | float, km and – | semi-major axis, eccentricity |
| `inc_deg`, `raan_deg`, `aop_deg` | float, ° | inclination, node, argument of periapsis; ICRF (equatorial) axes |
| `ta_deg` | float, ° | true anomaly at the epoch |
| `period_s` | float, s | two-body period, μ = GM_centre + GM_body |
| `path_dt_s` | list of float, s | each sample's offset from the epoch, −P/2 to +P/2 |
| `path_km` | list of `[x, y, z]`, km | the body relative to its centre, ICRF axes |

## Draw them

A path is relative to its centre, so a point on it in an ICRF scene is the centre's position plus
the path point. Get a centre's position the way the viewer does, by exchanging a zero offset in the
body's own frame to ICRF:

```python
def centre_icrf(body_id: bytes, tai_ns: int) -> np.ndarray:
    buf = client.buffer()
    buf.append(body_id, body_id, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], tai_ns)
    return positions(client.exchange(pa.Table.from_batches([buf.flush()]), "ICRF"))[0]

for centre, path in zip(id_bytes(orbits.column("centre_id")), orbits.column("path_km").to_pylist()):
    line = centre_icrf(centre, t) + np.array(path)      # (361, 3) km in ICRF, ready to draw
```

In a scene graph, attach the line to the centre's node instead and draw `path_km` as is; it then
moves with the centre. One exchange call takes any number of epochs (`sim/ephemeris.py` does this
for a whole window).

## Things to know

- **Mars and its moons need a Mars SPK** (`mar099s.bsp`); `de440s.bsp` holds only the Mars
  barycentre.
- **The path is the kernels', not an ellipse.** Planets close to within a fraction of a percent;
  the Sun's pull leaves the Moon's path several percent of `a` open after one period (8–10% at
  the epochs tried).
- **Straight segments cut inside the curve.** At 361 samples the gap is up to about 8,700 km for
  Mars, invisible at solar-system scale. Raise `samples` for close-ups.
- **`path_dt_s` makes the path a trajectory.** Interpolate it at `T − epoch` to animate a body
  along its orbit.
- **The angles are equatorial.** Earth about the Sun shows `inc_deg` ≈ 23.4°, not the ecliptic 0°.

## Worked example

`tools/export_viewer.py` makes one `client.orbits(...)` call for every body and passes each path
with its centre's name; `tools/viewer_template.html` attaches each line to its centre's node.
