"""A fixed site on a body's surface. Its pose is the local ENU frame at the site, so children
(robots) report positions in site-local metres: x east, y north, z up."""

import numpy as np

from sim.geo import enu_basis, fixed_to_geodetic, geodetic_to_fixed, quat_from_matrix
from sim.models import Row
from sim.scenario import AUTHORITY, FACILITY_CADENCE_S, FACILITY_TIMESCALE, FacilitySpec
from soloc_client import KIND_SOLOC, mint


class Facility:
    def __init__(self, spec: FacilitySpec):
        self.spec = spec
        self.name = spec.name
        self.id = mint(KIND_SOLOC, AUTHORITY, spec.name)
        self.position_km = geodetic_to_fixed(spec.body, spec.lat_deg, spec.lon_deg, spec.h_km)
        self.basis = enu_basis(spec.lat_deg, spec.lon_deg)
        self.quaternion = quat_from_matrix(self.basis)

    def geodetic(self, east_m, north_m) -> tuple[np.ndarray, np.ndarray]:
        """`(lat, lon)` degrees of points on the facility's ENU plane."""
        enu = np.column_stack([np.ravel(east_m), np.ravel(north_m), np.zeros(np.size(east_m))])
        lat, lon, _ = fixed_to_geodetic(self.spec.body, self.position_km + enu @ self.basis.T / 1000)
        return lat, lon

    def enu(self, lat_deg: float, lon_deg: float, h_km: float = 0.0) -> np.ndarray:
        """`(east, north, up)` metres of a geodetic point."""
        p = geodetic_to_fixed(self.spec.body, lat_deg, lon_deg, h_km)
        return self.basis.T @ (p - self.position_km) * 1000

    def due(self, t_s: int) -> bool:
        return t_s % FACILITY_CADENCE_S == 0

    def sample(self, t_s: int) -> Row | None:
        if not self.due(t_s):
            return None
        return Row(self.spec.body.frame_id, self.position_km.tolist(), self.quaternion,
                   timescale=FACILITY_TIMESCALE, optional={"velocity": [0.0, 0.0, 0.0]})


class Spot:
    """A pad or landing point on a facility: `(east, north)` metres from its origin, on the
    facility's ENU plane."""

    def __init__(self, facility: Facility, offset_m: tuple[float, float] = (0.0, 0.0)):
        self.facility = facility
        self.body = facility.spec.body
        self.offset_km = np.array([*offset_m, 0.0]) / 1000          # in the facility's ENU frame
        self.position_km = facility.position_km + facility.basis @ self.offset_km   # body-fixed
