"""A fixed site on a body's surface. Its pose is the local ENU frame at the site, so children
(robots) report positions in site-local metres: x east, y north, z up."""

from geo import enu_basis, geodetic_to_fixed, quat_from_matrix
from models import Row
from scenario import AUTHORITY, FACILITY_CADENCE_S, FACILITY_TIMESCALE, FacilitySpec
from soloc_client import KIND_SOLOC, mint


class Facility:
    def __init__(self, spec: FacilitySpec):
        self.spec = spec
        self.name = spec.name
        self.id = mint(KIND_SOLOC, AUTHORITY, spec.name)
        self.position_km = geodetic_to_fixed(spec.body, spec.lat_deg, spec.lon_deg, spec.h_km)
        self.quaternion = quat_from_matrix(enu_basis(spec.lat_deg, spec.lon_deg))

    def sample(self, t_s: int) -> Row | None:
        if t_s % FACILITY_CADENCE_S:
            return None
        return Row(self.spec.body.frame_id, self.position_km.tolist(), self.quaternion,
                   timescale=FACILITY_TIMESCALE, optional={"velocity": [0.0, 0.0, 0.0]})
