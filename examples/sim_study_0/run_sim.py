"""The driver: ticks the scenario on a 5 s grid, appends one batch per 10 min of sim time,
snapshots the bodies hourly, and saves the ledger. On an arena's decision tick it flushes first,
so the arena's policies read the latest rows back through `current_state`.

Run against a freshly started serve.sh (empty ledger). Stops on the first append failure.
"""

import argparse
import sys
import time
from pathlib import Path

import pyarrow.flight as fl

from sim import scenario as sc
from sim.roster import roster
from sim.strategies import load
from sim.fuel import write_table as write_fuel
from sim.wind import write_table
from soloc_client import KIND_ABSTRACT, KIND_SOLOC, SolocClient, registry_ipc, tai_ns_from_utc


class Sim:
    def __init__(self, client: SolocClient, seed: int, regatta_policy=None, wildfire_policy=None):
        self.client = client
        self.world = roster(seed, client, regatta_policy, wildfire_policy)
        self.entities = self.world.entities
        # An entity with `samples(t_s) -> [(offset_ns, row)]` may report several rows per tick.
        self.sub = [getattr(e, "samples", None) for e in self.entities]
        self.t0_ns = tai_ns_from_utc(sc.T0)
        self.buffer = client.buffer()
        self.batches = self.rows = self.snapshots = 0
        self.seen: set[bytes] = set()                # entities with rows (the fire pools fill only partly)

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
        for e, sub in zip(self.entities, self.sub):
            if sub is None:
                row = e.sample(t_s)
                if row is not None:
                    self.append(e, row, tai_ns)
            else:
                for offset_ns, row in sub(t_s):
                    self.append(e, row, tai_ns + offset_ns)

    def append(self, e, row, tai_ns: int):
        self.seen.add(e.id)
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
            deciding = [a for a in self.world.arenas if a.decide_at(t_s)]
            if deciding:
                self.flush(t_s - sc.BASE_TICK_S)
                for arena in deciding:
                    arena.decide(self.client, t_s)
            self.step(t_s)
            if (t_s + sc.BASE_TICK_S) % sc.BATCH_S == 0 or t_s == sc.DURATION_S:
                self.flush(t_s)
        print()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--server", default="grpc://localhost:50051")
    p.add_argument("--out", default="out/sim_study_0.arrow")
    p.add_argument("--seed", type=int, default=sc.SEED)
    p.add_argument("--regatta-policy", metavar="MOD:FN",
                   help="strategy for the last regatta boat (see docs/arena.md)")
    p.add_argument("--wildfire-policy", metavar="MOD:FN",
                   help="the wildfire's incident commander (see docs/arena.md)")
    args = p.parse_args()

    policies = [load(spec) if spec else None for spec in (args.regatta_policy, args.wildfire_policy)]
    client = SolocClient(args.server)
    if client.query_all().num_rows:
        sys.exit("ledger is not empty; restart serve.sh first")

    sim = Sim(client, args.seed, *policies)
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
    wind_path = out.with_name("wind.arrow")
    n = write_table(wind_path, [a.wind for a in sim.world.arenas])
    print(f"{n:,} wind samples → {wind_path}")
    fuel_path = out.with_name("fuel.arrow")
    n = write_fuel(fuel_path, [sim.world.wildfire.fuel])
    print(f"{n:,} fuel cells → {fuel_path}")
    print(f"{len(sim.seen)} entities, {sim.rows:,} entity rows + {sim.snapshots} snapshot rows "
          f"in {time.monotonic() - started:.0f} s")
    for arena in sim.world.arenas:
        print(arena.result())
    print(sim.world.factory.result())


if __name__ == "__main__":
    main()
