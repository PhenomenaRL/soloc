"""The driver: ticks the scenario on a 5 s grid, appends one batch per 10 min of sim time,
snapshots the bodies hourly, and saves the ledger.

Run against a freshly started serve.sh (empty ledger). Stops on the first append failure.
"""

import argparse
import sys
import time
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import pyarrow.flight as fl

import scenario as sc
from models.aircraft import aircraft
from models.facility import Facility
from models.robot import Crawler, Robot
from models.ship import ship
from models.spacecraft import Spacecraft, lander, launcher, orbiter
from models.track import Track
from soloc_client import KIND_ABSTRACT, KIND_SOLOC, SolocClient, registry_ipc, tai_ns_from_utc


@dataclass
class Roster:
    facilities: list[Facility]
    spacecraft: list[Spacecraft]
    robots: list[Robot]
    crawlers: list[Crawler]
    aircraft: list[Track]
    ships: list[Track]

    @property
    def entities(self) -> list:
        """Parents before their children, so a tick's rows are appended in that order."""
        return [*self.facilities, *self.spacecraft, *self.robots, *self.crawlers,
                *self.aircraft, *self.ships]


def roster(seed: int) -> Roster:
    facilities = [Facility(spec) for spec in sc.FACILITIES]
    site = {f.name: f for f in facilities}
    robots = [Robot(sc.robot_name(f.spec, i), f.id, sc.ROVERS[f.spec.body.name], seed)
              for f in facilities for i in range(sc.ROBOTS_PER_FACILITY)]

    landers = [lander(s, site[s.facility]) for s in sc.LANDERS]
    hosts = [*(orbiter(s) for s in sc.ORBITERS), *landers]
    spacecraft = [*hosts, *(launcher(s, site[s.facility]) for s in sc.LAUNCHES)]

    # Crawlers go to random hosts (stream 0; robots use their name hashes), with every lander
    # carrying at least one: its first crawler is the one that disembarks.
    picks = np.random.default_rng([seed, 0]).integers(len(hosts), size=sc.CRAWLERS)
    for k, craft in enumerate(landers):
        if hosts.index(craft) not in picks:
            picks[k] = hosts.index(craft)
    crawlers = []
    for i, pick in enumerate(picks):
        host = hosts[pick]
        host.cadence_s = sc.HOST_CADENCE_S
        disembark = None
        if host in landers and not any(c.host is host for c in crawlers):
            disembark = (host.facility, host.events["touchdown"] + sc.DISEMBARK_AFTER_S,
                         sc.ROVERS[host.facility.spec.body.name])
        crawlers.append(Crawler(sc.crawler_name(i), host, seed, disembark))

    planes = [aircraft(sc.aircraft_name(i), seed) for i in range(sc.AIRCRAFT)]
    ships = [ship(sc.ship_name(i), sc.LANES[i % len(sc.LANES)], seed) for i in range(sc.SHIPS)]
    return Roster(facilities, spacecraft, robots, crawlers, planes, ships)


class Sim:
    def __init__(self, client: SolocClient, seed: int):
        self.client = client
        self.entities = roster(seed).entities
        self.t0_ns = tai_ns_from_utc(sc.T0)
        self.buffer = client.buffer()
        self.batches = self.rows = self.snapshots = 0

    def register_names(self):
        bindings = [(KIND_ABSTRACT, sc.AUTHORITY, "kinematic_sim_v1")]
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
                                   units=row.units, timescale=row.timescale, **row.optional)

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
    p.add_argument("--out", default="out/solar_sim.arrow")
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
