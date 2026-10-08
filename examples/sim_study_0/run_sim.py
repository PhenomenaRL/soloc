"""The driver: ticks the scenario on a 5 s grid, appends one batch per 10 min of sim time,
snapshots the bodies hourly, and saves the ledger.

Run against a freshly started serve.sh (empty ledger). Stops on the first append failure.
"""

import argparse
import sys
import time
from pathlib import Path

import pyarrow.flight as fl

from sim import scenario as sc
from sim.roster import roster
from soloc_client import KIND_ABSTRACT, KIND_SOLOC, SolocClient, registry_ipc, tai_ns_from_utc


class Sim:
    def __init__(self, client: SolocClient, seed: int):
        self.client = client
        self.entities = roster(seed, client).entities
        self.t0_ns = tai_ns_from_utc(sc.T0)
        self.buffer = client.buffer()
        self.batches = self.rows = self.snapshots = 0

    def register_names(self):
        bindings = [(KIND_ABSTRACT, sc.AUTHORITY, "kinematic_sim_v1"),
                    (KIND_ABSTRACT, sc.HORIZONS_AUTHORITY, sc.HORIZONS_SOURCE)]
        bindings += [(KIND_SOLOC, sc.AUTHORITY, e.name) for e in self.entities]
        print(self.client.action("import_names", registry_ipc(bindings)))

    def step(self, t_s: int):
        tai_ns = self.t0_ns + t_s * 10**9
        if t_s % sc.SNAPSHOT_S == 0:
            self.client.snapshot([b.frame_id for b in sc.SNAPSHOT_BODIES], tai_ns)
            self.snapshots += len(sc.SNAPSHOT_BODIES)
        for e in self.entities:
            row = e.sample(t_s)
            if row is not None:
                self.buffer.append(e.id, row.frame_id, row.position, row.quaternion, tai_ns,
                                   units=row.units, timescale=row.timescale,
                                   source_id=row.source_id, estimate=row.estimate, **row.optional)

    def flush(self, t_s: int):
        n = len(self.buffer)
        if n:
            self.client.put(self.buffer.flush())
            self.batches += 1
            self.rows += n
        h, m = divmod(t_s // 60, 60)
        print(f"\rbatch {self.batches:4d}  sim {h:02d}:{m:02d}  +{n:5d} rows  "
              f"{self.rows:9,d} entity rows  {self.snapshots:4d} snapshot rows", end="", flush=True)

    def run(self):
        self.register_names()
        for t_s in range(0, sc.DURATION_S + 1, sc.BASE_TICK_S):
            self.step(t_s)
            if (t_s + sc.BASE_TICK_S) % sc.BATCH_S == 0 or t_s == sc.DURATION_S:
                self.flush(t_s)
        print()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--server", default="grpc://localhost:50051")
    p.add_argument("--out", default="out/sim_study_0.arrow")
    p.add_argument("--seed", type=int, default=sc.SEED)
    args = p.parse_args()

    client = SolocClient(args.server)
    if client.query_all().num_rows:
        sys.exit("ledger is not empty; restart serve.sh first")

    sim = Sim(client, args.seed)
    started = time.monotonic()
    try:
        sim.run()
    except fl.FlightError as e:
        print()
        sys.exit(f"append failed: {e}")

    # The server resolves the path from its own working directory, so send it absolute.
    out = Path(args.out).resolve()
    out.parent.mkdir(parents=True, exist_ok=True)
    print(client.action("save_ledger", {"path": str(out)}))
    print(f"{len(sim.entities)} entities, {sim.rows:,} entity rows + {sim.snapshots} snapshot rows "
          f"in {time.monotonic() - started:.0f} s")


if __name__ == "__main__":
    main()
