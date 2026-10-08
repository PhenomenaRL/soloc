#!/usr/bin/env bash
# Downloads the kernels serve.sh loads into data/, skipping any already present, plus the
# Natural Earth 1:50m land polygons (public domain) that the ship checks and plots use, and the
# three.js build (MIT) that tools/export_viewer.py inlines into the standalone 3D viewer.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p data

NAIF=https://naif.jpl.nasa.gov/pub/naif/generic_kernels
for url in \
    "$NAIF/spk/planets/de440s.bsp" \
    "$NAIF/spk/satellites/mar099s.bsp" \
    "http://public-data.nyxspace.com/anise/v0.10/pck11.pca" \
    "https://raw.githubusercontent.com/nvkelso/natural-earth-vector/master/geojson/ne_50m_land.geojson" \
    "https://cdnjs.cloudflare.com/ajax/libs/three.js/r128/three.min.js"; do
    f="data/$(basename "$url")"
    if [[ ! -s $f ]]; then
        echo "fetching $url"
        curl -fL --retry 3 -o "$f.part" "$url"
        mv "$f.part" "$f"
    fi
done
ls -lh data
