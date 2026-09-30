import { describe, expect, it } from "vitest";
import { asArrowIPC } from "./ipc";
import { loadLedger } from "./loader";

const encode = (s: string) => new TextEncoder().encode(s);

describe("asArrowIPC", () => {
  it("passes a real Arrow IPC file through", () => {
    const bytes = encode("ARROW1\0\0rest of the file");
    expect(asArrowIPC(bytes, "x")).toBe(bytes);
  });

  it("accepts an ArrayBuffer as well as a view", () => {
    const bytes = encode("ARROW1\0\0");
    expect(asArrowIPC(bytes.buffer as ArrayBuffer, "x")).toEqual(bytes);
  });

  it("names the cause when a dev server serves index.html instead", () => {
    // The regression this guard exists for: Vite answers a missing /data path
    // with the SPA shell and a 200, so `resp.ok` is true and Arrow reads
    // '<!do' as a 1.8 GB metadata length.
    const page = encode("<!doctype html>\n<html><head><title>soloc</title></head></html>");
    expect(() => asArrowIPC(page, "/data/dummy.arrows")).toThrow(/HTML page/);
    expect(() => asArrowIPC(page, "/data/dummy.arrows")).toThrow(/gen_visualizer_fixture/);
  });

  it("is not fooled by leading whitespace before the doctype", () => {
    expect(() => asArrowIPC(encode("\n  <HTML>"), "x")).toThrow(/HTML page/);
  });

  it("reports length for other non-Arrow payloads", () => {
    expect(() => asArrowIPC(encode("{}"), "x")).toThrow(/not an Arrow IPC file/);
    expect(() => asArrowIPC(new Uint8Array(0), "x")).toThrow(/0 bytes/);
  });
});

describe("loadLedger", () => {
  it("rejects an HTML page before Arrow can misread it as a length", () => {
    expect(() => loadLedger(encode("<!doctype html><html></html>"))).toThrow(/HTML page/);
  });
});
