/**
 * Native-unit handling. Positions stay in each row's own `units_pos` in the
 * data; the scene works in kilometres, converting at read time — the
 * view-layer analogue of "store raw, reproject on demand".
 */

export const KM_PER_UNIT: Readonly<Record<string, number>> = {
  km: 1,
  m: 1e-3,
  cm: 1e-5,
  mm: 1e-6,
  au: 1.495978707e8,
};

export function unitToKm(units: string): number {
  const f = KM_PER_UNIT[units.toLowerCase()];
  if (f === undefined) throw new Error(`unknown units_pos '${units}'`);
  return f;
}

export function posToKm(
  p: readonly [number, number, number],
  units: string,
): [number, number, number] {
  const f = unitToKm(units);
  return [p[0] * f, p[1] * f, p[2] * f];
}
