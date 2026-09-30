"""Downloads each probe's state vectors for the sim window from JPL Horizons into kernels/.

    python fetch_horizons.py            # skips tables already there; --force refetches

Barycentric ICRF position and velocity (km, km/s), one row per `PROBE_CADENCE_S` on the UTC
minute. Run it again after changing the window in scenario.py.
"""

import argparse
import urllib.parse
import urllib.request
from pathlib import Path

import scenario as sc
from models.probe import covers, load_horizons

API = "https://ssd.jpl.nasa.gov/api/horizons.api"
KERNELS = Path(__file__).parent / "kernels"


def fetch(spec: sc.ProbeSpec) -> str:
    quoted = {
        "COMMAND": spec.horizons_id,
        "OBJ_DATA": "NO",
        "EPHEM_TYPE": "VECTORS",
        "CENTER": "500@0",               # the solar-system barycentre
        "REF_PLANE": "FRAME",            # ICRF axes, not the ecliptic
        "VEC_TABLE": "2",                # position and velocity
        "CSV_FORMAT": "YES",
        "OUT_UNITS": "KM-S",
        "TIME_TYPE": "UT",               # UTC since 1962
        "START_TIME": sc.T0.strftime("%Y-%m-%d %H:%M"),
        "STOP_TIME": sc.T_END.strftime("%Y-%m-%d %H:%M"),
        "STEP_SIZE": f"{sc.PROBE_CADENCE_S // 60} min",
    }
    query = {"format": "text", **{k: f"'{v}'" for k, v in quoted.items()}}
    with urllib.request.urlopen(f"{API}?{urllib.parse.urlencode(query)}", timeout=120) as r:
        return r.read().decode()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--force", action="store_true", help="refetch tables that already cover the window")
    args = p.parse_args()

    KERNELS.mkdir(exist_ok=True)
    for spec in sc.PROBES:
        path = KERNELS / sc.horizons_file(spec)
        if path.exists() and not args.force and covers(load_horizons(path)[0]):
            print(f"{path.name}: already covers the window")
            continue
        print(f"fetching {spec.name} ({spec.horizons_id}) from Horizons")
        text = fetch(spec)
        if "$$SOE" not in text:
            raise SystemExit(f"Horizons returned no table:\n{text}")
        path.write_text(text)
        t_s, _, _ = load_horizons(path)
        print(f"{path.name}: {len(t_s):,} rows, {path.stat().st_size / 1e6:.1f} MB")


if __name__ == "__main__":
    main()
