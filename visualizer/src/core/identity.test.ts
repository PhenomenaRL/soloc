import { describe, expect, it } from "vitest";
import { KIND_ABSTRACT, KIND_ASTRO, KIND_SOLOC, NameBook, idToString, kindOfIdString } from "./identity";

/** The bytes `PrescribedId::astronomical(ephemeris, orientation)` writes. */
function astroBytes(ephemeris: number, orientation: number): Uint8Array {
  const b = new Uint8Array(16);
  new DataView(b.buffer).setInt32(0, ephemeris, false); // big-endian
  b[6] = 0x80 | KIND_ASTRO; // UUID version 8, kind in the low nibble
  b[8] = 0x80; // RFC 9562 variant
  new DataView(b.buffer).setInt32(9, orientation, false);
  return b;
}

describe("idToString", () => {
  it("formats the canonical hyphenated 8-4-4-4-12 form", () => {
    const b = Uint8Array.from([
      0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x80, 0x07,
      0x80, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    ]);
    expect(idToString(b)).toBe("00010203-0405-8007-8009-0a0b0c0d0e0f");
  });

  it("rejects anything that is not 16 bytes", () => {
    expect(() => idToString(Uint8Array.from([1, 2, 3]))).toThrow(/16 bytes/);
  });

  it("round-trips an astronomical id's embedded frame pair", () => {
    // Earth is (399, 399): 0x18f big-endian in bytes 0..4 and 9..13.
    expect(idToString(astroBytes(399, 399))).toBe("0000018f-0000-8000-8000-00018f000000");
  });
});

describe("kindOfIdString", () => {
  it("reads the low nibble of byte 6", () => {
    expect(kindOfIdString(idToString(astroBytes(399, 399)))).toBe(KIND_ASTRO);
    expect(kindOfIdString("00000000-0000-8100-8000-000000000000")).toBe(KIND_SOLOC);
    expect(kindOfIdString("00000000-0000-8200-8000-000000000000")).toBe(KIND_ABSTRACT);
  });
});

describe("NameBook", () => {
  const earth = idToString(astroBytes(399, 399));
  const rover = "11111111-1111-8100-8000-111111111111";
  const unknown = "22222222-2222-8100-8000-222222222222";

  // Built by hand rather than parsed from IPC: the shape under test is the
  // lookup contract, not Arrow decoding (the fixture test covers that).
  const book = NameBook.fromEntries([
    [earth, { authority: "astro", commonName: "Earth", kind: KIND_ASTRO }],
    [rover, { authority: "demo", commonName: "rover-1", kind: KIND_SOLOC }],
  ]);

  it("keys astronomical names bare and everything else by authority", () => {
    expect(book.key(earth)).toBe("Earth");
    expect(book.key(rover)).toBe("demo:rover-1");
  });

  it("falls back to the id itself when nothing is registered", () => {
    expect(book.label(unknown)).toBe(unknown);
    expect(book.key(unknown)).toBe(unknown);
  });

  it("classifies an unregistered id from its own bytes", () => {
    expect(book.isAstronomical(earth)).toBe(true);
    expect(book.isAstronomical(unknown)).toBe(false);
    expect(book.isAstronomical(idToString(astroBytes(0, 1)))).toBe(true); // ICRF
  });
});
