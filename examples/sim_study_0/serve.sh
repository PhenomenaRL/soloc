#!/usr/bin/env bash
# Runs soloc-server with an in-memory ledger, the kernels in data/, and config.toml beside this
# script (unless $SOLOC_CONFIG already names another file).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
k="$here/data"
for f in de440s.bsp mar099s.bsp pck11.pca; do
    [[ -s $k/$f ]] || { echo "missing $k/$f; run ./fetch_data.sh" >&2; exit 1; }
done

export SOLOC_KERNEL_PATHS="$k/de440s.bsp:$k/mar099s.bsp:$k/pck11.pca"
# Absolute, because the server runs from the repo root (the cd below).
export SOLOC_CONFIG="$(realpath -m "${SOLOC_CONFIG:-$here/config.toml}")"
cd "$here/../.."
exec cargo run --release -p soloc-server
