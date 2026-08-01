/**
 * Integration: parse the real fixture written by
 * `cargo run -p soloc-ledger --example gen_visualizer_fixture` and re-derive the
 * transform tree in TypeScript. The expected numbers mirror the generator's
 * own round-trip self-test (16 events, re-parents at T+84 h and T+120 h).
 *
 * Skipped rather than failed when the fixture is absent: it is gitignored and
 * generating it needs NAIF kernels, so a fresh checkout can still run the unit
 * suite. Everything asserted here is checked against real ephemeris output.
 */

import { describe, expect, it } from "vitest";
import { NS_PER_HOUR, taiCalendarToNs } from "../core/epoch";
import { TransformTopology } from "../core/topology";
import { fixture, hasFixture } from "../testing/fixture";

const t0 = taiCalendarToNs(2026, 8, 1);
const topology = () => TransformTopology.fromRows(fixture().data.rows);

describe.skipIf(!hasFixture)("dummy.arrows fixture", () => {
  it("loads every row of every entity", () => {
    const { data } = fixture();
    // 8 bodies every 12 h (15 samples) + Earth and Moon hourly (169 each)
    // + 3 demo entities hourly.
    expect(data.rows.length).toBe(8 * 15 + 2 * 169 + 3 * 169);
    expect(data.entities.size).toBe(13);
    expect(data.numBatches).toBe(1);
  });

  it("spans the 7-day window", () => {
    const { data } = fixture();
    expect(data.minEpochNs).toBe(t0);
    expect(data.maxEpochNs).toBe(t0 + 168n * NS_PER_HOUR);
  });

  it("resolves every id to a registered name", () => {
    const { data, idOf } = fixture();
    for (const id of data.entities.keys()) expect(data.names.label(id)).not.toBe(id);
    expect(data.names.key(idOf("Earth"))).toBe("Earth");
    expect(data.names.isAstronomical(idOf("Earth"))).toBe(true);
    expect(data.names.isAstronomical(idOf("demo:miner-1"))).toBe(false);
  });

  it("keeps native units per entity (store raw, reproject on demand)", () => {
    const { data, idOf } = fixture();
    const unitsOf = (key: string) =>
      new Set(data.entities.get(idOf(key))!.map((r) => r.unitsPos));
    expect(unitsOf("Earth")).toEqual(new Set(["km"]));
    // The miner alone spans two units: km while chasing, mm once docked.
    expect(unitsOf("demo:miner-1")).toEqual(new Set(["km", "mm"]));
  });

  it("re-derives the same 15 topology events as the Rust ledger", () => {
    expect(topology().events.length).toBe(15);
  });

  it("sees the spaceship re-parent Earth → Moon at T+84 h", () => {
    const { data, idOf } = fixture();
    const topo = topology();
    const events = topo.events.filter((e) => e.childId === idOf("demo:spaceship-1"));
    expect(events.map((e) => e.parentId)).toEqual([idOf("Earth"), idOf("Moon")]);
    expect(events[1]!.epochNs).toBe(t0 + 84n * NS_PER_HOUR);
  });

  it("sees the miner dock to the asteroid at T+120 h", () => {
    const { data, idOf } = fixture();
    const topo = topology();
    const events = topo.events.filter((e) => e.childId === idOf("demo:miner-1"));
    expect(events).toHaveLength(2);
    expect(events[1]!.parentId).toBe(idOf("demo:asteroid-1"));
    expect(events[1]!.epochNs).toBe(t0 + 120n * NS_PER_HOUR);
    // The first parent is ICRF, which has no rows and so is not an entity id.
    expect(data.names.label(events[0]!.parentId)).toBe("ICRF");
  });

  it("resolves the miner's full chain after docking", () => {
    const { data, idOf } = fixture();
    const chain = topology().chainAt(idOf("demo:miner-1"), t0 + 150n * NS_PER_HOUR);
    expect(chain.map((id) => data.names.label(id))).toEqual([
      "miner-1",
      "asteroid-1",
      "ICRF",
    ]);
  });

  it("carries covariance on some docking-phase miner rows", () => {
    const { data, idOf } = fixture();
    const miner = data.entities.get(idOf("demo:miner-1"))!;
    const withCov = miner.filter((r) => r.stateCovariance !== null);
    expect(withCov.length).toBeGreaterThan(0);
    expect(withCov.every((r) => r.stateCovariance!.length === 21)).toBe(true);
    expect(withCov.every((r) => r.unitsPos === "mm")).toBe(true);
  });

  it("decodes kinematics, and the ship's frame IS the Moon's id", () => {
    const { data, idOf } = fixture();
    const earth = data.entities.get(idOf("Earth"))!;
    expect(earth.every((r) => r.velocity !== null)).toBe(true);
    // A body is its own body-fixed frame, so the post-TLI ship is parented to
    // the Moon's own id rather than to some separate IAU_MOON anchor.
    const ship = data.entities.get(idOf("demo:spaceship-1"))!;
    expect(ship.at(-1)!.frameId).toBe(idOf("Moon"));
    expect(ship[0]!.frameId).toBe(idOf("Earth"));
  });

  it("carries real ephemeris, not a synthetic circle", () => {
    const { data, idOf } = fixture();
    // Earth is ~1 au from the SSB and moves ~29.8 km/s. Both would be wrong by
    // orders of magnitude if the almanac had silently fallen back to anything.
    const earth = data.entities.get(idOf("Earth"))![0]!;
    const r = Math.hypot(...earth.position);
    expect(r).toBeGreaterThan(1.4e8);
    expect(r).toBeLessThan(1.6e8);
    const speed = Math.hypot(...earth.velocity!) / 1000; // stored m/s
    expect(speed).toBeGreaterThan(28);
    expect(speed).toBeLessThan(31);

    // Earth's body-fixed orientation must actually turn: a full rotation a day
    // means consecutive hourly samples differ. An identity-quaternion fixture
    // would leave everything parented to it riding a planet that never rotates.
    const rows = data.entities.get(idOf("Earth"))!;
    expect(rows[0]!.quaternion).not.toEqual(rows[1]!.quaternion);
  });

  it("keeps the Earth → Moon hand-off continuous in world space", () => {
    const { data, idOf } = fixture();
    // The ship's last Earth-frame row and its first Moon-frame row are in
    // different coordinates; re-expressed against the real bodies they must
    // describe nearly the same point. 2 000 km on a 19 800 km orbit radius.
    const ship = data.entities.get(idOf("demo:spaceship-1"))!;
    const earth = data.entities.get(idOf("Earth"))!;
    const moon = data.entities.get(idOf("Moon"))!;
    const at = (rows: typeof ship, ns: bigint) => rows.find((r) => r.epochNs === ns)!;

    const lastEarthNs = t0 + 83n * NS_PER_HOUR;
    const firstMoonNs = t0 + 84n * NS_PER_HOUR;
    const a = worldKm(at(ship, lastEarthNs), at(earth, lastEarthNs));
    const b = worldKm(at(ship, firstMoonNs), at(moon, firstMoonNs));
    // Both rows sit at the capture offset from the Moon, one hour apart, so the
    // gap is the Moon's own motion (~3 700 km/h) plus the offset's rotation.
    const gap = Math.hypot(a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    expect(gap).toBeLessThan(6_000);
  });
});

/** Parent position + parent rotation applied to a child's body-fixed offset. */
function worldKm(
  child: { position: readonly number[]; quaternion: readonly number[] },
  parent: { position: readonly number[]; quaternion: readonly number[] },
): [number, number, number] {
  const [w, x, y, z] = parent.quaternion as [number, number, number, number];
  const [px, py, pz] = child.position as [number, number, number];
  // v' = q v q*, expanded.
  const tx = 2 * (y * pz - z * py);
  const ty = 2 * (z * px - x * pz);
  const tz = 2 * (x * py - y * px);
  return [
    parent.position[0]! + px + w * tx + (y * tz - z * ty),
    parent.position[1]! + py + w * ty + (z * tx - x * tz),
    parent.position[2]! + pz + w * tz + (x * ty - y * tx),
  ];
}
