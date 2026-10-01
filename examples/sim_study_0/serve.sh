#!/usr/bin/env bash
# Runs soloc-server with an in-memory ledger and the kernels in data/.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
k="$here/data"
for f in de440s.bsp mar099s.bsp pck11.pca; do
    [[ -s $k/$f ]] || { echo "missing $k/$f; run ./fetch_data.sh" >&2; exit 1; }
done

export SOLOC_KERNEL_PATHS="$k/de440s.bsp:$k/mar099s.bsp:$k/pck11.pca"
cd "$here/../.."
exec cargo run --release -p soloc-server
