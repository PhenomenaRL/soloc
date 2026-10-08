"""Entity models. Each exposes `id`, `name`, `due(t_s) -> bool` and `sample(t_s) -> Row | None`,
where `t_s` is integer seconds since `scenario.T0`; `sample` returns `None` exactly when the
entity is not due at that tick."""

from dataclasses import dataclass, field

from soloc_client import SIM_SOURCE


@dataclass
class Row:
    frame_id: bytes
    position: list[float]
    quaternion: list[float]
    units: str = "km"
    timescale: str = "TAI"
    source_id: bytes = SIM_SOURCE
    estimate: str = "SIMULATED"
    optional: dict = field(default_factory=dict)   # velocity, mass_kg, dimensions, ...
