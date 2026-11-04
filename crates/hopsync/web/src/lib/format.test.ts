import { describe, expect, it } from "vitest";
import { bytes, duration, joinPath, parentPath, parseSize, rate, sizeInput } from "./format";

describe("bytes", () => {
  it("uses binary units with precision that shrinks as digits grow", () => {
    expect(bytes(0)).toBe("0 B");
    expect(bytes(1023)).toBe("1023 B");
    expect(bytes(1536)).toBe("1.50 KiB");
    expect(bytes(15 * 1024 * 1024)).toBe("15.0 MiB");
    expect(bytes(150 * 1024 ** 3)).toBe("150 GiB");
  });
});

describe("rate", () => {
  it("is empty when nothing is moving", () => {
    expect(rate(0)).toBe("");
    expect(rate(2048)).toBe("2.00 KiB/s");
  });
});

describe("duration", () => {
  it("picks the two largest useful units", () => {
    expect(duration(59.4)).toBe("59s");
    expect(duration(61)).toBe("1m 1s");
    expect(duration(3700)).toBe("1h 1m");
  });
  it("is empty for values that make no sense", () => {
    expect(duration(Number.POSITIVE_INFINITY)).toBe("");
    expect(duration(-1)).toBe("");
  });
});

describe("parseSize", () => {
  it("reads the forms people type", () => {
    expect(parseSize("0")).toBe(0);
    expect(parseSize("10G")).toBe(10 * 1024 ** 3);
    expect(parseSize(" 500 MiB ")).toBe(500 * 1024 ** 2);
    expect(parseSize("1.5k")).toBe(1536);
  });
  it("refuses anything else", () => {
    expect(parseSize("ten")).toBeNull();
    expect(parseSize("-1G")).toBeNull();
    expect(parseSize("")).toBeNull();
  });
  it("reads back what sizeInput writes", () => {
    expect(sizeInput(0)).toBe("0");
    expect(parseSize(sizeInput(20 * 1024 ** 3))).toBe(20 * 1024 ** 3);
  });
});

describe("paths", () => {
  it("joins and splits relative paths", () => {
    expect(joinPath("", "a")).toBe("a");
    expect(joinPath("a/b", "c")).toBe("a/b/c");
    expect(parentPath("a/b/c")).toBe("a/b");
    expect(parentPath("a")).toBe("");
  });
});
