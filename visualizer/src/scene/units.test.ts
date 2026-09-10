import { describe, expect, it } from "vitest";
import { posToKm, unitToKm } from "./units";

describe("native unit conversion", () => {
  it("maps supported units to km", () => {
    expect(unitToKm("km")).toBe(1);
    expect(unitToKm("m")).toBe(1e-3);
    expect(unitToKm("mm")).toBe(1e-6);
  });

  it("rejects unknown units instead of guessing", () => {
    expect(() => unitToKm("furlong")).toThrow(/unknown units_pos/);
  });

  it("converts positions in place", () => {
    expect(posToKm([1_737_400, 0, -500], "m")).toEqual([1_737.4, 0, -0.5]);
  });
});
