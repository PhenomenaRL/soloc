"""OSM buildings round the site: footprints fetched once from Overpass into data/osm_buildings.json,
in the site's ENU metres, with a height each. Used by the viewer (and, from B2, the MuJoCo scene).

    ../sim_study_0/.venv/bin/python buildings.py           # fetch (if needed) and list them
"""

import json
import re
import urllib.parse
import urllib.request

import scenario as sc
from sim.models.facility import Facility

CACHE = sc.HERE / "data" / "osm_buildings.json"
OVERPASS_URL = "https://overpass-api.de/api/interpreter"
AGENT = "soloc-microduck_sil/1.0 (one-off fetch; cached locally)"
RADIUS_M = 80.0
LEVEL_M = 3.3
DEFAULT_H_M, DEFAULT_SPREAD_M = 9.0, 4.0     # untagged buildings: 9–13 m, fixed per OSM id


def query(spec) -> str:
    return f"[out:json][timeout:90];way(around:{RADIUS_M:g},{spec.lat_deg},{spec.lon_deg})[building];out tags geom;"


def fetch(spec) -> list[dict]:
    """The Overpass reply's ways, from the cache if it holds this query, else fetched into it."""
    q = query(spec)
    if CACHE.exists():
        cached = json.loads(CACHE.read_text())
        if cached.get("query") == q:
            return cached["elements"]
    req = urllib.request.Request(OVERPASS_URL, data=urllib.parse.urlencode({"data": q}).encode(),
                                 headers={"User-Agent": AGENT})
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            reply = json.loads(r.read())
    except (OSError, ValueError) as e:
        raise SystemExit(f"Overpass fetch failed ({e}); it is often busy, try again in a minute") from e
    CACHE.parent.mkdir(parents=True, exist_ok=True)
    CACHE.write_text(json.dumps({"query": q, "elements": reply["elements"]}))
    return reply["elements"]


def number(tag: str | None) -> float | None:
    m = re.match(r"\s*([0-9.]+)", tag or "")
    return float(m.group(1)) if m else None


def height_m(osm_id: int, tags: dict) -> float:
    if (h := number(tags.get("height"))) is not None:
        return h
    if (lv := number(tags.get("building:levels"))) is not None:
        return lv * LEVEL_M
    return DEFAULT_H_M + DEFAULT_SPREAD_M * ((osm_id * 2654435761) % 1000) / 1000


def buildings(site: Facility) -> list[dict]:
    """`{name, osm_id, ring [[e, n], …] (open), h}` per building, ring in site ENU metres."""
    out = []
    for el in fetch(site.spec):
        pts = el.get("geometry") or []
        ring = [site.enu(p["lat"], p["lon"], site.spec.h_km)[:2].round(3).tolist() for p in pts]
        if len(ring) > 1 and ring[0] == ring[-1]:
            ring = ring[:-1]
        if len(ring) >= 3:
            out.append({"name": f"bldg-{el['id']}", "osm_id": el["id"], "ring": ring,
                        "h": round(height_m(el["id"], el.get("tags", {})), 2)})
    return out


def main():
    bs = buildings(Facility(sc.SITE))
    for b in sorted(bs, key=lambda b: min(e * e + n * n for e, n in b["ring"])):
        d = min((e * e + n * n) ** 0.5 for e, n in b["ring"])
        print(f"  {b['name']:<16} nearest corner {d:5.1f} m  h {b['h']:5.1f} m  {len(b['ring'])} corners")
    print(f"{len(bs)} buildings within {RADIUS_M:g} m of {sc.SITE.name} ({CACHE})")


if __name__ == "__main__":
    main()
