#!/usr/bin/env bash
# Downloads the kernels serve.sh loads into kernels/, skipping any already present.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p kernels

NAIF=https://naif.jpl.nasa.gov/pub/naif/generic_kernels
for url in \
    "$NAIF/spk/planets/de440s.bsp" \
    "$NAIF/spk/satellites/mar099s.bsp" \
    "http://public-data.nyxspace.com/anise/v0.10/pck11.pca"; do
    f="kernels/$(basename "$url")"
    if [[ ! -s $f ]]; then
        echo "fetching $url"
        curl -fL --retry 3 -o "$f.part" "$url"
        mv "$f.part" "$f"
    fi
done
ls -lh kernels
