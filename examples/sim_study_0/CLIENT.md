# The soloc Python client

`soloc_client.py` is how Python talks to a soloc ledger. soloc has no Python bindings, so the
client goes through [`soloc-server`](../../crates/soloc-server/) over Arrow Flight: it mints
ids, encodes epochs, builds batches against the server's own schema, and wraps the Flight
calls. It is one file on top of `pyarrow` and `numpy`; copy it into your project.

The full API, the vocabularies and the raw wire contract are in
[docs/client_reference.md](docs/client_reference.md).

## 1. Start a server

From `examples/sim_study_0/`:

```bash
./fetch_data.sh     # kernels into data/, once (the client needs de440s, mar099s, pck11)
./serve.sh          # builds and runs soloc-server on 0.0.0.0:50051, empty in-memory ledger
```

`serve.sh` is equivalent to this, which runs from anywhere in the repo with your own kernels:

```bash
SOLOC_KERNEL_PATHS=/path/de440s.bsp:/path/mar099s.bsp:/path/pck11.pca \
  cargo run --release -p soloc-server
```

For a bind address, a persistent ledger file or an object store, see the
[server README](../../crates/soloc-server/README.md).

## 2. Install

```bash
python3 -m venv .venv
.venv/bin/pip install pyarrow numpy
```

## 3. First program

A site on the Earth's surface and a rover that drives across it:

```python
from datetime import datetime
from pathlib import Path

from soloc_client import (KIND_ABSTRACT, KIND_SOLOC, SolocClient, astronomical, mint,
                          positions, registry_ipc, tai_ns_from_utc)

client = SolocClient("grpc://localhost:50051")

# Ids. Yours are minted from (authority, name); an astronomical frame from its NAIF pair.
AUTHORITY = "example.org"
IAU_EARTH = astronomical(399, 399)
site = mint(KIND_SOLOC, AUTHORITY, "test_range")
rover = mint(KIND_SOLOC, AUTHORITY, "rover_1")
source = mint(KIND_ABSTRACT, AUTHORITY, "rover_odometry")

# Names, so that tools can show "rover_1" and not 16 bytes. Optional.
client.action("import_names", registry_ipc([
    (KIND_SOLOC, AUTHORITY, "test_range"),
    (KIND_SOLOC, AUTHORITY, "rover_1"),
    (KIND_ABSTRACT, AUTHORITY, "rover_odometry"),
]))

# The site is framed on the Earth, in km.
t0 = tai_ns_from_utc(datetime(2026, 9, 1, 12, 0, 0))
buf = client.buffer()
buf.append(site, IAU_EARTH, [6378.137, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], t0,
           source_id=source, estimate="MEASURED")
client.put(buf.flush())

# The rover is framed on the site, in metres, one row per second.
for k in range(5):
    buf.append(rover, site, [10.0 * k, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], t0 + k * 10**9,
               units="m", source_id=source, estimate="MEASURED",
               velocity=[10.0, 0.0, 0.0], mass_kg=180.0)
client.put(buf.flush())

# Where is the rover now, and where is that on the Earth?
now = client.current_state([rover])
print(positions(now)[0])                                   # [40. 0. 0.]            m, on the site
print(positions(client.exchange(now, "IAU_EARTH"))[0])     # [6378.177 0. 0.]       km
print(positions(client.exchange(now, "GCRF", units="m"))[0])

# Keep it. The server resolves the path from its own working directory, so send it absolute.
print(client.action("save_ledger", {"path": str(Path("quickstart.arrow").resolve())}))
```

## The model in brief

- **A row is one measurement:** `entity_id`, `frame_id`, `position` (in `units`),
  `quaternion` `[w, x, y, z]` rotating the entity's frame into its parent, and an epoch.
- **Frames:** `frame_id` is an astronomical frame (`astronomical(ephemeris_id,
  orientation_id)`, e.g. `(399, 399)` IAU_EARTH, `(399, 1)` GCRF, `(0, 1)` ICRF) or another
  entity. A frame exists once a row names it; an entity changes parent by writing a row with a
  different `frame_id`. A row framed on an entity resolves through that entity's latest pose at
  or before it.
- **Time:** `append` takes integer TAI nanoseconds since J2000 (`tai_ns_from_utc`); the
  `timescale` argument only tags the row, and the server stores everything as TAI.

## Things to know

- **Compare ids with `matches`.** numpy's `==` on bytes drops trailing zero bytes, and
  astronomical ids end in them.
- **Ids come back as `uuid.UUID`.** `entity_ids` and `id_bytes` give `bytes`.
- **`save_ledger` / `load_ledger` paths are the server's**, relative to its working directory.
- **The JSON epochs count from J1900.** Use `epoch_tai_s(tai_ns)` to convert.
- **`RowBuffer.append` defaults** `source_id` and `estimate` to the simulation's; set your own.
