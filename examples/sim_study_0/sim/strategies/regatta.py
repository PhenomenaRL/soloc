"""The default tactician: VMG-optimal angles from the polar, the lifted tack or gybe (with
hysteresis) toward the next mark, straight there once it is fetchable, marks left to port.

Start: luff below the line until the time to the line at close-hauled VMG matches the time to
the gun, and luff early enough that the projected position at the next decision stays
`LINE_STANDOFF_M` below it; if over at the gun, run back below.

Its own pose is a tick old, so it is first carried forward to now along its velocity.
"""

import math

import numpy as np

from sim.models.sailboat import bearing, unit, wrap180
from sim.strategies import Command, RegattaObservation

MARGIN_DEG = 3.0                     # sail this far above the VMG angle, clear of the no-go edge
HYSTERESIS_DEG = 10.0                # change tack only when the other is this much closer
ROUNDING_OFFSET_M = 15.0             # aim this far off a mark, on the side that leaves it to port
LINE_STANDOFF_M = 20.0
START_SLACK_S = 10.0


def vmg_angles(polar, tws: float) -> tuple[float, float]:
    """True wind angles of the best upwind and downwind VMG."""
    twa = np.arange(30.0, 180.5, 0.5)
    vmg = polar(twa, tws) * np.cos(np.radians(twa))
    return float(twa[np.argmax(vmg)]), float(twa[np.argmin(vmg)])


def steer(heading: float, to: float, twd: float, up: float, down: float) -> float:
    """Heading toward bearing `to`: straight if it is between the VMG angles, else the tack (or
    gybe) closer to it, keeping the current one unless the other is clearly closer."""
    a = abs(float(wrap180(to - twd)))
    if up <= a <= down:
        return to
    off = up if a < up else down
    options = [(twd + off) % 360, (twd - off) % 360]
    gaps = [abs(float(wrap180(h - to))) for h in options]
    side = 0 if wrap180(heading - twd) >= 0 else 1
    other = 1 - side
    return options[other] if gaps[other] + HYSTERESIS_DEG < gaps[side] else options[side]


def tactician(obs: RegattaObservation) -> Command:
    me = obs.boats[obs.me]
    v = np.array(me.velocity)
    p = me.xy + v * (obs.t_s - me.t_s)
    twd, tws = obs.wind(*p)
    up, down = vmg_angles(obs.polar, tws)
    up += MARGIN_DEG
    axis, right = unit(obs.course_axis_deg), unit(obs.course_axis_deg + 90)
    rc, pin = obs.boats[obs.line[0]].xy, obs.marks[obs.line[1]].xy
    centre = (rc + pin) / 2
    above = float((p - centre) @ axis)

    if obs.next_mark == "START":
        half = float(np.linalg.norm(rc - pin)) / 2 - LINE_STANDOFF_M
        target = centre + np.clip((p - centre) @ right, -half, half) * right + 30 * axis
        if obs.t_s < obs.gun_s:
            vmg = float(obs.polar(up, tws)) * math.cos(math.radians(up))
            late = -above / max(vmg, 1e-6) + START_SLACK_S >= obs.gun_s - obs.t_s
            ahead = above + float(v @ axis) * obs.decision_s
            if ahead > -LINE_STANDOFF_M or not late:
                return twd
        elif above > 0:
            return (twd + 180) % 360
        return steer(me.heading_deg, bearing(p, target), twd, up, down)

    if obs.next_mark == "W":
        target = obs.marks[obs.windward].xy + ROUNDING_OFFSET_M * right
    elif obs.next_mark == "GATE":
        a, b = (obs.marks[n].xy for n in obs.gate)
        mark = min((a, b), key=lambda m: float(np.linalg.norm(m - p)))
        mid = (a + b) / 2
        target = mark + ROUNDING_OFFSET_M * (mid - mark) / float(np.linalg.norm(mid - mark))
    else:
        target = centre
    return steer(me.heading_deg, bearing(p, target), twd, up, down)
