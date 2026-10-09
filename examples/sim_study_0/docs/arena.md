# Arenas: writing a strategy

In an arena, the ledger is the world model a decision maker reads while events play out. Pieces
are moved by a strategy function, which observes the world through soloc plus the venue's wind.
The defaults ship in `sim/strategies/`, so a run completes on its own. Swap one in with a flag:

```bash
python run_sim.py --regatta-policy mypkg.mymodule:my_tactician
python run_sim.py --regatta-policy tests.policies:straight_line   # a trivial example
python run_sim.py --wildfire-policy mypkg.mymodule:my_commander
python run_sim.py --wildfire-policy tests.policies:idle_crews     # never sends a crew out
```

`--regatta-policy` drives the last boat, SAIL-10, and the other nine keep the default tactician
(`sim.strategies.regatta:tactician`). Boats do not interact, so the other nine finish exactly
as before. `--wildfire-policy` replaces the incident commander
(`sim.strategies.wildfire:commander`). The run prints a result line per arena, and
`python -m tools.snapshot_sim FILE --scenario regatta|wildfire --every 5m` shows it.

Each observation carries `memory`, a dict that persists across decisions. In the regatta there
is one per boat; in the wildfire there is one for the commander.

## The decision loop

On each decision tick the driver:
1. flushes its row buffer to the server
2. reads the arena's entities back with `current_state`
3. calls the strategy, which returns commands that hold until the next decision

| Arena | Decides | From → until |
|---|---|---|
| regatta | every 10 s, once per boat still racing | the warning signal (13:55 ADT) → the last finish or the time limit (16:30 ADT) |
| wildfire | every 5 min, once for all crews | 1 h after ignition → contained |

Rows read back are as old as their entity's cadence: a boat's own pose is one row (5 s) older
than the decision.

## Regatta: `policy(obs: RegattaObservation) -> Command`

Headings are compass degrees (clockwise from true north), and wind is the direction it blows
from plus its speed. Positions are venue ENU metres (x east, y north) about the course centre.

| Field | What it is |
|---|---|
| `t_s`, `decision_s`, `memory` | now (seconds since 2026-09-01T00:00 UTC), the interval until the next decision, this boat's policy state |
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

## Wildfire: `policy(obs: WildfireObservation) -> dict`

Positions are venue ENU metres about the ignition point.

| Field | What it is |
|---|---|
| `t_s`, `decision_s`, `memory` | now, the interval until the next decision (300 s), the commander's own state |
| `crews` | the 12 crews as `Pose`s from `current_state` |
| `tasks` | per crew: `idle`, `move`, `dig`, `blocked` (its next step was refused as unsafe) or `escape` |
| `perimeter` | the fire's vertices as `Pose`s, in ring order (counter-clockwise) |
| `moving` | the names of the vertices still spreading |
| `lines` | finished trench segments, `((e, n), (e, n))`, from the trench rows |
| `ignition_s`, `icp` | when it started; the incident command post, where the crews wait |
| `safe_m`, `escape_m`, `walk_m_s`, `dig_m_h` | 30 m, 40 m, 1.2 m/s, 100 m/h |
| `wind(e, n)` | `(twd_deg, tws_m_s)` now at a point |
| `fuel(e, n)` | the R0 factor there (0 = non-burnable) |
| `spread(e, n, nx, ny, twd=None, tws=None)` | outward spread rate (m/s) along a unit normal there, optionally under another wind |
| `burning(e, n)` | inside the perimeter now |

The return value is `{crew: command}`, and crews left out keep their task. A command is one of:
- `("move", [(e, n), ...])`: walk the waypoints
- `("dig", [(e, n), ...])`: walk to the first point, then dig along the rest
- `("hold",)`: stop

A new command replaces the old one, and any piece of line being dug is finished where the crew
stands. Every 50 m of finished line becomes a trench entity.

**Rules the sim applies.**
- **Spread:** each vertex spreads along its outward normal at the rate of a wind-aligned ellipse,
  scaled by the fuel there.
- **Stopping:** a vertex stops for good at a finished trench, at non-burnable fuel, or where it
  would step into ground the fire has already burnt (fronts merging).
- **Contained:** every vertex has stopped.
- **Crew safety:** a crew never moves into burnt ground or within 30 m of a moving vertex; that
  move is refused, and the task reads `blocked`. Within 40 m it drops its work and walks away
  (`escape`).

**The default commander** (indirect attack, flanks to head):
- It plans one closed line, the convex hull of the moving vertices. Each vertex is pushed out
  50 m, plus its worst-case spread (the wind turned ±25° and gusting ×1.3) × the time until the
  line near it will be dug, × 1.2.
- It splits the line at the heel and the head. Each flank is cut into equal chunks, one per
  crew. A crew walks the line to its chunk and digs it toward the head.
- It replans only when a spreading vertex gets outside the line. A replan keeps the finished
  line: projections stop at it, and hull points next to it count as done.
- A blocked crew walks the other way round the line; a crew that had to escape gives its chunk
  up.
