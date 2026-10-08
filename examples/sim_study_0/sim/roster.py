"""Builds every entity in the scenario, in the order the driver appends them."""

from dataclasses import dataclass
from typing import Callable

import numpy as np

from sim import scenario as sc
from sim.ephemeris import Ephemeris
from sim.models.aircraft import aircraft
from sim.models.facility import Facility
from sim.models.probe import Probe
from sim.models.regatta import Regatta
from sim.models.robot import CargoRobot, Crawler, Robot
from sim.models.ship import ship
from sim.models.spacecraft import Spacecraft, lander, launcher, moonshot, orbiter, transfer
from sim.models.track import Track
from soloc_client import SolocClient


@dataclass
class Roster:
    facilities: list[Facility]
    spacecraft: list[Spacecraft]
    probes: list[Probe]
    robots: list[Robot]
    crawlers: list[Crawler]
    cargo: list[CargoRobot]
    aircraft: list[Track]
    ships: list[Track]
    regatta: Regatta

    @property
    def entities(self) -> list:
        """Parents before their children, so a tick's rows are appended in that order."""
        return [*self.facilities, *self.spacecraft, *self.probes, *self.robots, *self.crawlers,
                *self.cargo, *self.aircraft, *self.ships, *self.regatta.entities]

    @property
    def arenas(self) -> list:
        """Groups the driver hands decision ticks to (`decide_at`, `decide`)."""
        return [self.regatta]


def roster(seed: int, client: SolocClient, regatta_policy: Callable | None = None) -> Roster:
    """The client is for the kernels only (body ephemerides); the ledger is not read. The
    policy drives the last regatta boat (default: the tactician the others use)."""
    ephemeris = Ephemeris(client)
    facilities = [Facility(spec) for spec in sc.FACILITIES]
    site = {f.name: f for f in facilities}
    robots = [Robot(sc.robot_name(f.spec, i), f.id, sc.ROVERS[f.spec.body.name], seed, i)
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

    # The cruise craft carry their own crawlers, numbered on from the ones drawn above.
    for spec in sc.TRANSFERS:
        craft = transfer(spec, ephemeris)
        spacecraft.append(craft)
        for _ in range(spec.crawlers):
            crawlers.append(Crawler(sc.crawler_name(len(crawlers)), craft, seed))

    planes = [aircraft(sc.aircraft_name(i), seed) for i in range(sc.AIRCRAFT)]
    ships = [ship(sc.ship_name(i), sc.LANES[i % len(sc.LANES)], seed) for i in range(sc.SHIPS)]
    cargo = []
    for i, spec in enumerate(sc.MOONSHOTS):
        craft = moonshot(spec, site[spec.origin], site[spec.destination], ephemeris)
        spacecraft.append(craft)
        cargo.append(CargoRobot(sc.cargo_name(i), craft, seed))

    probes = [Probe(s, ephemeris) for s in sc.PROBES]
    return Roster(facilities, spacecraft, probes, robots, crawlers, cargo, planes, ships,
                  Regatta(seed, regatta_policy))
