"""Body ephemerides from the server's kernels.

A zero offset with identity attitude in a body's IAU frame, exchanged to an inertial frame,
comes back as the body's centre and its IAU → inertial rotation. The rows are astronomical
roots, so this reads the kernels only: it works whatever the ledger holds, and one call
carries any number of epochs.
"""

import numpy as np
import pyarrow as pa

from sim import scenario as sc
from sim.geo import ICRF, Body, Frame
from soloc_client import SolocClient, positions, sts_field, tai_ns_from_utc

DIFFERENCE_S = 30.0          # half-width of the central difference behind `state`'s velocity


class Ephemeris:
    def __init__(self, client: SolocClient):
        self.client = client
        self.t0_ns = tai_ns_from_utc(sc.T0)

    def poses(self, body: Body, t_s, target: Frame = ICRF) -> tuple[np.ndarray, np.ndarray]:
        """`(centre km (N, 3), IAU → target quaternion [w, x, y, z] (N, 4))` at each `t_s`
        (seconds since T0, any value the kernels cover)."""
        buf = self.client.buffer()
        for t in np.atleast_1d(t_s):
            buf.append(body.frame_id, body.frame_id, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0],
                       self.t0_ns + int(round(float(t) * 1e9)))
        out = self.client.exchange(pa.Table.from_batches([buf.flush()]), target.frame)
        return positions(out), sts_field(out, "quaternion").flatten().to_numpy().reshape(-1, 4)

    def centre(self, body: Body, t_s, target: Frame = ICRF) -> np.ndarray:
        return self.poses(body, t_s, target)[0]

    def state(self, body: Body, t_s, target: Frame = ICRF) -> tuple[np.ndarray, np.ndarray]:
        """`(centre km, velocity km/s)`, the velocity by central difference."""
        t = np.atleast_1d(np.asarray(t_s, dtype=float))
        p = self.centre(body, np.concatenate([t - DIFFERENCE_S, t, t + DIFFERENCE_S]), target)
        before, at, after = p[:len(t)], p[len(t):2 * len(t)], p[2 * len(t):]
        return at, (after - before) / (2 * DIFFERENCE_S)
