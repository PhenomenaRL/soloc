import { describe, expect, it } from "vitest";
import {
  NS_PER_CENTURY,
  NS_PER_HOUR,
  cmpNs,
  formatTai,
  nsToParts,
  partsToNs,
  secondsFrom,
  taiCalendarToNs,
} from "./epoch";

describe("epoch parts", () => {
  it("collapses (centuries, ns) like hifitime Duration::from_parts", () => {
    expect(partsToNs(0, 42n)).toBe(42n);
    expect(partsToNs(1, 0n)).toBe(NS_PER_CENTURY);
    expect(partsToNs(2, 7n)).toBe(2n * NS_PER_CENTURY + 7n);
  });

  it("round-trips through nsToParts", () => {
    for (const ns of [0n, 42n, NS_PER_CENTURY - 1n, NS_PER_CENTURY + 5n]) {
      const p = nsToParts(ns);
      expect(partsToNs(p.centuries, p.ns)).toBe(ns);
    }
  });
});

describe("TAI calendar display", () => {
  it("renders J2000 itself", () => {
    expect(formatTai(0n)).toBe("2000-01-01T12:00:00 TAI");
  });

  it("matches the fixture window start printed by the Rust generator", () => {
    const t0 = taiCalendarToNs(2026, 8, 1);
    expect(formatTai(t0)).toBe("2026-08-01T00:00:00 TAI");
  });

  it("is the inverse of taiCalendarToNs at second precision", () => {
    const ns = taiCalendarToNs(2026, 8, 4, 12, 0, 0);
    expect(formatTai(ns)).toBe("2026-08-04T12:00:00 TAI");
    expect(ns - taiCalendarToNs(2026, 8, 1)).toBe(84n * NS_PER_HOUR);
  });
});

describe("epoch math helpers", () => {
  it("secondsFrom is window-relative and float-safe", () => {
    const t0 = taiCalendarToNs(2026, 8, 1);
    expect(secondsFrom(t0, t0 + NS_PER_HOUR)).toBe(3600);
    expect(secondsFrom(t0 + NS_PER_HOUR, t0)).toBe(-3600);
  });

  it("cmpNs orders bigints", () => {
    expect([3n, 1n, 2n].sort(cmpNs)).toEqual([1n, 2n, 3n]);
  });
});
