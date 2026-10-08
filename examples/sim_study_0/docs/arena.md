# Arenas: writing a strategy

In an arena, the ledger is the world model a decision maker reads while events play out. Pieces
are moved by a strategy function, which observes the world through soloc plus the venue's wind.
The defaults ship in `sim/strategies/`, so a run completes on its own. Swap one in with a flag:

```bash
python run_sim.py --regatta-policy mypkg.mymodule:my_tactician
python run_sim.py --regatta-policy tests.policies:straight_line   # a trivial example
```

The flag drives the last boat, SAIL-10, and the other nine keep the default tactician
(`sim.strategies.regatta:tactician`). Boats do not interact, so the other nine finish exactly
as before. The run prints the finishing order, and
`python -m tools.snapshot_sim FILE --scenario regatta --every 5m` shows the race.

## The decision loop

From the warning signal (13:55 ADT) until the last boat finishes or the time limit (16:30 ADT),
the driver runs this loop every 10 s:
1. It flushes its row buffer to the server.
2. It reads `current_state` for the RC boat, the boats and the marks.
3. It calls the strategy once per boat still racing.

The command holds until the next decision. Rows are 5 s apart, so a boat's own pose is one row
(5 s) older than the decision.

## Regatta: `policy(obs: RegattaObservation) -> Command`

Headings are compass degrees (clockwise from true north), and wind is the direction it blows
from plus its speed. Positions are venue ENU metres (x east, y north) about the course centre.

| Field | What it is |
|---|---|
| `t_s`, `decision_s` | now (seconds since 2026-09-01T00:00 UTC) and the interval until the next decision |
| `me` | this boat's name, a key of `boats` |
| `boats` | every racing boat and `RC-BOAT`: `Pose(name, t_s, east_m, north_m, heading_deg, speed_m_s, velocity)`, decoded from `current_state` |
| `marks` | the laid marks as `Pose`s: `MARK-W`, `MARK-GATE-1`, `MARK-GATE-2`, `MARK-PIN` |
| `next_mark`, `legs_done`, `legs` | `START`, `W`, `GATE` or `FINISH`; the course is `W, GATE, W, GATE, W, FINISH` |
| `started`, `ocs` | started yet; over the line at the gun (must dip below and cross again) |
| `gun_s`, `time_limit_s` | 14:00 ADT; boats still racing at 16:30 ADT are DNF |
| `course_axis_deg`, `line`, `gate`, `windward` | bearing from the line to `MARK-W`; the names of the line ends (RC boat, pin), the gate marks and the windward mark |
| `wind(east, north)` | `(twd_deg, tws_m_s)` now at a point |
| `polar(twa_deg, tws)` | boat speed in m/s; 0 inside 40° of the wind |

The command is one of:
- a compass heading
- `"tack"` or `"gybe"`, which mirror the heading about the wind
- `None`, which keeps the current heading

Any other return value stops the run with a `TypeError`. Pointing inside the no-go zone stops the
boat. That is a luff, and the default uses it to wait on the start line.

**Rules the referee applies.**
- **Start:** the first upward crossing of the line between the RC boat and the pin after the gun.
- **Rounding:** passing within 30 m of the mark (for the gate, either mark), in course order.
- **Finish:** crossing the line downward after the last windward mark.
- **Penalties:** a turn through head to wind costs 8 s of sailing and one through dead downwind
  costs 5 s.

Before the warning signal and after finishing, the boat motors on its own.

**The default tactician:**
- sails 3° above the best upwind VMG angle and at the best downwind one
- picks the tack or gybe whose heading is closer to the mark, changing only when the other is
  10° closer
- heads straight for the mark once it is fetchable, leaving marks to port
- at the start, luffs below the line and times its run from the close-hauled VMG, projecting its
  own pose forward by the observation lag
