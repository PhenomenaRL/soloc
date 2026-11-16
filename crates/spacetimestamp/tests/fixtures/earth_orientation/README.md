# Independent Earth rotation reference

`cspice_rotations.csv` contains eight row-major position rotation matrices generated
on October 21, 2026 by **CSPICE N0067**, called through SpiceyPy 8.0.0 (NumPy 2.5.4).
Neither Soloc nor ANISE calculated these expected values. The integration test
`earth_rotations_match_independent_cspice_reference` compares both directions of
ITRF93 ↔ IAU_EARTH and ITRF93 ↔ Earth-centred inertial axes at two UTC epochs. It
checks metre/km positions, every orientation axis, and the public GPS converter.

CSPICE's [`pxform`](https://naif.jpl.nasa.gov/pub/naif/toolkit_docs/C/cspice/pxform_c.html)
maps a column position vector from the named source axes to the destination axes.
Its `J2000` axes correspond to Soloc's GCRF `(399, 1)` for these same-origin
rotations. No origin translation or SPK is involved. CSPICE's `str2et` independently
converted the UTC strings with the leap-second kernel below:

| UTC | CSPICE ET seconds past J2000 |
| --- | ---: |
| 2020-01-01T00:00:00 | 631108869.18390727 |
| 2025-01-01T12:00:00 | 789004869.18394208 |

The position tolerance is **0.02 m** at an Earth-radius input, and the unit-axis
vector tolerance is **3e-9**. These allow small numerical/time-model differences:
ANISE 0.10.6 evaluates BPC rotations with hifitime ET and IAU constants with
hifitime TDB, while CSPICE evaluates both using its ET. These are regression
tolerances, not a GPS accuracy guarantee. Both implementations use the same
underlying Earth orientation data; this checks independent evaluation and frame
composition, not independent physical measurements or the approximate
WGS84-to-ITRF93 datum assumption.

## Inputs and provenance

| Input | SHA-256 |
| --- | --- |
| Earth BPC snapshot, locally named `earth_latest_high_prec.bpc` | `f95a7fed588a6614f53ed28cee15eb9148abaf808739f94f0c62de935a82edbb` |
| Soloc's `pck11.pca` | `1be7e0a72445999cc01dd82d607201defff1f619cb528a588e4a707b5b942ed9` |
| NAIF [`pck00011.tpc`](https://naif.jpl.nasa.gov/pub/naif/generic_kernels/pck/pck00011.tpc) | `3dff7b1dbeceaa01f25467767d3fa25816051c85d162d1edf04acb310ee28bb1` |
| NAIF [`naif0012.tls`](https://naif.jpl.nasa.gov/pub/naif/generic_kernels/lsk/naif0012.tls) | `678e32bdb5a744117a467cd9601cd6b373f0e9bc9bbde1371d5eee39600a039b` |

The BPC's embedded header names `earth_000101_261029_260802.bpc`, created August 2,
2026. Both reference epochs precede its last observed datum. A newly downloaded
file named `earth_latest_high_prec.bpc` is a different snapshot; its name does not
identify its contents. The test compares numerical results without enforcing a
particular file hash, so other local snapshots can run it. A difference beyond
the tolerance fails the test and requires checking the input models; it is never
silently skipped. If an intentional data/model change requires new expected
values, regenerate them through CSPICE and record the new hashes together.
Kernels and the temporary Python tooling are not included
in the repository or required for normal offline tests.

## Run and regenerate

Run the Rust acceptance suite with explicitly supplied local kernels:

```sh
SOLOC_KERNEL_PATHS=/path/pck11.pca:/path/earth_latest_high_prec.bpc \
  cargo test -p spacetimestamp --test itrf93_transforms -- --ignored
```

To regenerate the CSV independently, install `spiceypy==8.0.0` in a temporary
Python environment and supply the BPC plus the two official NAIF text kernels.
The PCA is used only by the Rust comparison; CSPICE reads the TPC directly.
Run the following snippet with `GPS_REFERENCE_BPC`, `GPS_REFERENCE_PCK`, and
`GPS_REFERENCE_LSK` set to the respective local paths. Review the generated
values, versions and input hashes together before replacing the fixture.

```python
import os
import spiceypy as spice

assert spice.__version__ == "8.0.0"
assert spice.tkvrsn("TOOLKIT") == "CSPICE_N0067"
for variable in ["GPS_REFERENCE_PCK", "GPS_REFERENCE_LSK", "GPS_REFERENCE_BPC"]:
    spice.furnsh(os.environ[variable])
print("utc,from,to,r00,r01,r02,r10,r11,r12,r20,r21,r22")
for utc in ["2020-01-01T00:00:00", "2025-01-01T12:00:00"]:
    et = spice.str2et(utc)
    for source, target in [
        ("ITRF93", "IAU_EARTH"), ("IAU_EARTH", "ITRF93"),
        ("ITRF93", "J2000"), ("J2000", "ITRF93"),
    ]:
        matrix = spice.pxform(source, target, et)
        print(",".join([utc, source, target] + [f"{v:.17g}" for v in matrix.flat]))
spice.kclear()
```
