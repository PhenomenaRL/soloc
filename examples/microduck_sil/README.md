# microduck_sil

Pollen Robotics' microduck digital twin, four ducks on a trash-spotting mission in their apartment
scene, recorded into soloc; then a sweep of duck-to-duck detector designs over the recordings.
Run everything from this directory. Pollen's repos are cloned, unpatched, into `upstream/`.

## Once

    ./fetch_upstream.sh
    (cd upstream/microduck_rl && uv sync)
    ../sim_study_0/fetch_data.sh          # if sim_study_0's venv and kernels aren't set up yet

## Record a run (300 s)

    export DUCK_SIM_RL=$PWD/upstream/microduck_rl DUCK_SIM_DUCKS=4 DUCK_SIM_VIEWER=0 DUCK_SIM_SCENE=apartment
    upstream/microduck/scripts/duck-sim down && upstream/microduck/scripts/duck-sim
    ../sim_study_0/serve.sh                                                    # another terminal, fresh
    ../sim_study_0/.venv/bin/python record.py --ducks 4 --seconds 300          # --run 2: the variant

## Sightings, alignment and the 3D viewer

    ../sim_study_0/serve.sh                                                    # restart it, fresh
    ../sim_study_0/.venv/bin/python align.py                                   # --run 2 likewise
    ../sim_study_0/.venv/bin/python export_viewer.py                           # → out/microduck_aligned_3d.html

## Detector sweep (stop serve.sh first; it starts its own)

    ../sim_study_0/.venv/bin/python sweep.py                                   # every design, every run
    ../sim_study_0/.venv/bin/python sweep.py --designs cam-ble-base ble-base   # a few

Outputs: `out/sweep/<design>.arrow` (run 2 under `out/sweep/run2/`), merged `out/sweep.arrow` and
`out/sweep_run2.arrow`, plots `out/sweep_*.png`.

## Helpers

    ../sim_study_0/.venv/bin/python apartment.py     # the planner's grid and patrol routes
    ../sim_study_0/.venv/bin/python buildings.py     # the OSM buildings round the site
