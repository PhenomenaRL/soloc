/**
 * Epoch handling for spacetimestamp data.
 *
 * Storage convention (mirrors `spacetimestamp::ephemeris`): every row carries
 * `(duration_centuries: i16, duration_ns: u64)` — an offset from the J2000 TAI
 * reference epoch, 2000-01-01T12:00:00 TAI. One hifitime "century" is exactly
 * 36525 days. In this app an epoch is a single `bigint`: total nanoseconds
 * since J2000 TAI (ledger data is TAI-normalised on append, so no timescale
 * conversion happens here).
 */

/** Nanoseconds per hifitime century: 36525 days × 86400 s × 1e9 ns. */
export const NS_PER_CENTURY = 3_155_760_000_000_000_000n;

export const NS_PER_HOUR = 3_600_000_000_000n;
export const NS_PER_MS = 1_000_000n;

/**
 * Combines stored duration parts into nanoseconds since J2000 TAI.
 *
 * Matches `hifitime::Duration::from_parts` for the non-negative durations the
 * fixture uses (centuries ≥ 0). Negative-century durations are out of scope
 * until the visualizer needs pre-2000 data.
 */
export function partsToNs(centuries: number, ns: bigint | number): bigint {
  return BigInt(centuries) * NS_PER_CENTURY + BigInt(ns);
}

/** Inverse of {@link partsToNs}, for round-trip tests. */
export function nsToParts(epochNs: bigint): { centuries: number; ns: bigint } {
  const centuries = epochNs / NS_PER_CENTURY;
  return { centuries: Number(centuries), ns: epochNs - centuries * NS_PER_CENTURY };
}

/**
 * J2000 TAI rendered on a plain calendar: 2000-01-01 12:00:00. Used only to
 * *display* TAI epochs as calendar strings (millisecond precision); no leap
 * second or timescale conversion is implied — the label stays "TAI".
 */
const J2000_TAI_CALENDAR_MS = Date.UTC(2000, 0, 1, 12, 0, 0);

/** Formats an epoch (ns since J2000 TAI) as e.g. `2026-08-04T12:00:00 TAI`. */
export function formatTai(epochNs: bigint): string {
  const d = new Date(J2000_TAI_CALENDAR_MS + Number(epochNs / NS_PER_MS));
  return d.toISOString().replace(/\.\d{3}Z$/, " TAI");
}

/** Builds an epoch from a TAI calendar moment (inverse of {@link formatTai}). */
export function taiCalendarToNs(
  year: number,
  month1: number, // 1-based, matching how humans write dates
  day: number,
  hour = 0,
  minute = 0,
  second = 0,
): bigint {
  const ms = Date.UTC(year, month1 - 1, day, hour, minute, second) - J2000_TAI_CALENDAR_MS;
  return BigInt(ms) * NS_PER_MS;
}

/**
 * Seconds from `originNs` to `epochNs` as a JS number — safe for scrubber and
 * interpolation math, where the window is days, not centuries.
 */
export function secondsFrom(originNs: bigint, epochNs: bigint): number {
  return Number(epochNs - originNs) / 1e9;
}

/** Compare function for sorting bigint epochs. */
export function cmpNs(a: bigint, b: bigint): number {
  return a < b ? -1 : a > b ? 1 : 0;
}
