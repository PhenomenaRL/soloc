#!/usr/bin/env bash
# Clones Pollen Robotics' microduck (robotd, duck-sim) and microduck_rl (MuJoCo twin, policies) into
# upstream/ at pinned commits, skipping any clone already present. Nothing here is committed.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p upstream

clone() {  # name url commit
    if [ -d "upstream/$1" ]; then
        echo "upstream/$1 present, skipping"
        return
    fi
    git clone --filter=blob:none "$2" "upstream/$1"
    git -C "upstream/$1" checkout --detach "$3"
}

clone microduck https://github.com/pollen-robotics/microduck a360b3843bb49fd48a635b39eabcb835d9fffecb
clone microduck_rl https://github.com/pollen-robotics/microduck_rl bae39f5900b318f7e87fc485c30a31213cfa14eb
