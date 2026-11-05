"""Trivial arena policies, to show a strategy swap changes the result.

    python run_sim.py --regatta-policy tests.policies:straight_line
    python run_sim.py --wildfire-policy tests.policies:idle_crews
"""

import numpy as np

from sim.models.sailboat import bearing, wrap180
from sim.scenario import NO_GO_DEG
from sim.strategies import Command, RegattaObservation


def straight_line(obs: RegattaObservation) -> Command:
    """Straight at the next mark (the line's centre to start and finish), pinching 1° off the
    no-go edge when the mark is upwind. No start timing, no tactics."""
    me = obs.boats[obs.me]
    if obs.next_mark in ("START", "FINISH"):
        target = (obs.boats[obs.line[0]].xy + obs.marks[obs.line[1]].xy) / 2
    elif obs.next_mark == "W":
        target = obs.marks[obs.windward].xy
    else:
        target = min((obs.marks[n].xy for n in obs.gate), key=lambda m: float(np.linalg.norm(m - me.xy)))
    to = bearing(me.xy, target)
    twd, _ = obs.wind(me.east_m, me.north_m)
    a = float(wrap180(to - twd))
    if abs(a) >= NO_GO_DEG + 1:
        return to
    return (twd + np.copysign(NO_GO_DEG + 1, a if a else 1.0)) % 360


def idle_crews(obs) -> dict:
    """Never sends a crew out: the fire burns unchecked."""
    return {}
