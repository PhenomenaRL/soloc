"""Entity models. Each exposes `id`, `name`, and `sample(t_s) -> Row | None`, where `t_s` is
integer seconds since `scenario.T0`; `None` means the entity reports nothing at that tick."""

from dataclasses import dataclass, field


@dataclass
class Row:
    frame_id: bytes
    position: list[float]
    quaternion: list[float]
    units: str = "km"
    timescale: str = "TAI"
    optional: dict = field(default_factory=dict)   # velocity, mass_kg, dimensions, ...
