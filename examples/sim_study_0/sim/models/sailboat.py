"""Kinematic sailboats on a venue's ENU plane, in metres, stepped online by their `Regatta`.

A boat is docked, motoring along a route at `MOTOR_M_S`, holding head to wind (speed 0), or
sailing the heading its strategy commands at the polar speed for the wind at its position. A
tack or gybe costs its penalty in sailing time. Headings are compass degrees; rows carry ENU yaw.
"""

import math

import numpy as np

from sim.geo import quat_yaw
from sim.models import Row
from sim.scenario import (AUTHORITY, BOAT_TIMESCALE, DOCK_HEADING_DEG, GYBE_PENALTY_S, MOTOR_M_S,
                          NO_GO_DEG, POLAR_FLOOR, POLAR_GAIN, POLAR_MAX_M_S, REGATTA_CADENCE_S,
                          TACK_PENALTY_S)
from soloc_client import KIND_SOLOC, mint


def wrap180(a):
    return (np.asarray(a) + 180.0) % 360.0 - 180.0


def polar(twa_deg, tws_m_s):
    """Boat speed (m/s) at a true wind angle (either side) and true wind speed."""
    twa = np.abs(wrap180(twa_deg))
    shape = POLAR_FLOOR + (1 - POLAR_FLOOR) * np.sin(np.pi * (twa - NO_GO_DEG) / (180 - NO_GO_DEG))
    return np.where(twa < NO_GO_DEG, 0.0, np.minimum(POLAR_MAX_M_S, POLAR_GAIN * tws_m_s * shape))


def unit(heading_deg: float) -> np.ndarray:
    h = math.radians(heading_deg)
    return np.array([math.sin(h), math.cos(h)])


def bearing(p: np.ndarray, q: np.ndarray) -> float:
    """Compass bearing from `p` to `q` (ENU metres)."""
    return math.degrees(math.atan2(q[0] - p[0], q[1] - p[1])) % 360


def penalty_s(old_deg: float, new_deg: float, twd_deg: float) -> float:
    """The penalty for turning `old → new` the short way: a tack through head to wind, a gybe
    through dead downwind, nothing on the same side."""
    a0, a1 = float(wrap180(old_deg - twd_deg)), float(wrap180(new_deg - twd_deg))
    if a0 * a1 >= 0:
        return 0.0
    return TACK_PENALTY_S if abs(a0) + abs(a1) <= 180 else GYBE_PENALTY_S


class Boat:
    def __init__(self, name: str, venue_id: bytes, berth: np.ndarray, depart_s: int,
                 mass_kg: float, dimensions_m, due):
        self.name = name
        self.id = mint(KIND_SOLOC, AUTHORITY, name)
        self.venue_id = venue_id
        self.berth, self.depart_s = berth, depart_s
        self.mass_kg, self.dimensions_m = mass_kg, dimensions_m
        self._due = due
        self.group = None                 # the Regatta that steps it
        self.pos = berth.astype(float).copy()
        self.heading = DOCK_HEADING_DEG
        self.vel = np.zeros(2)
        self.mode = "docked"              # docked | motor | hold | sail
        self.route: list[np.ndarray] = []
        self.homeward = False
        self.command = None
        self.penalty_s = 0.0

    def due(self, t_s: int) -> bool:
        return self._due(t_s)

    def motor(self, route: list[np.ndarray], homeward: bool):
        self.mode, self.route, self.homeward = "motor", [np.asarray(p, float) for p in route], homeward

    def move(self, dt: float):
        """Advance the position over the step just ended."""
        if self.mode != "motor":
            self.pos = self.pos + self.vel * dt
            return
        left = MOTOR_M_S * dt
        while self.route and left > 0:
            gap = self.route[0] - self.pos
            d = float(np.linalg.norm(gap))
            if d <= left:
                self.pos, left = self.route.pop(0), left - d
            else:
                self.pos, left = self.pos + gap * (left / d), 0.0

    def steer(self, twd_deg: float, tws_m_s: float):
        """Heading and velocity for the step starting now, from the mode and the wind here."""
        if self.mode == "docked":
            self.heading, self.vel = DOCK_HEADING_DEG, np.zeros(2)
        elif self.mode == "motor":
            self.heading = bearing(self.pos, self.route[0])
            self.vel = MOTOR_M_S * unit(self.heading)
        elif self.mode == "hold":
            self.heading, self.vel = twd_deg % 360, np.zeros(2)
        else:
            cmd, self.command = self.command, None
            new = (self.heading if cmd is None else
                   (2 * twd_deg - self.heading) % 360 if cmd in ("tack", "gybe") else cmd)
            self.penalty_s += penalty_s(self.heading, new, twd_deg)
            self.heading = new
            lost = min(self.penalty_s, REGATTA_CADENCE_S)
            self.penalty_s -= lost
            speed = float(polar(new - twd_deg, tws_m_s)) * (1 - lost / REGATTA_CADENCE_S)
            self.vel = speed * unit(new)

    def sample(self, t_s: int) -> Row | None:
        if not self.due(t_s):
            return None
        self.group.advance(t_s)
        return Row(self.venue_id, [float(self.pos[0]), float(self.pos[1]), 0.0],
                   quat_yaw(math.radians(90 - self.heading)), units="m", timescale=BOAT_TIMESCALE,
                   optional={"velocity": [float(self.vel[0]), float(self.vel[1]), 0.0],
                             "mass_kg": self.mass_kg, "dimensions": list(self.dimensions_m)})
