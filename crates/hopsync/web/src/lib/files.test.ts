import { describe, expect, it } from "vitest";
import type { Entry } from "./api";
import { compareMark, indexByName, sortEntries } from "./files";

const file = (name: string, size = 1, mtime: number | null = 100): Entry => ({
  name,
  kind: "file",
  size,
  mtime,
  valid: true,
});
const dir = (name: string): Entry => ({ name, kind: "dir", size: 0, mtime: 100, valid: true });

describe("sortEntries", () => {
  it("puts folders first and sorts numbers as numbers", () => {
    const sorted = sortEntries([file("ep10"), dir("z"), file("ep2"), dir("a")]);
    expect(sorted.map((e) => e.name)).toEqual(["a", "z", "ep2", "ep10"]);
  });
  it("leaves its input alone", () => {
    const input = [file("b"), file("a")];
    sortEntries(input);
    expect(input[0]?.name).toBe("b");
  });
});

describe("compareMark", () => {
  const other = indexByName([file("same"), file("big", 2), file("old", 1, 50), dir("kind"), file("nodate", 1, null)]);

  it("is only for a name missing on the other side", () => {
    expect(compareMark(file("new"), other)).toBe("only");
  });
  it("marks a different size, date or kind", () => {
    expect(compareMark(file("big"), other)).toBe("differs");
    expect(compareMark(file("old"), other)).toBe("differs");
    expect(compareMark(file("kind"), other)).toBe("differs");
  });
  it("marks a match, and a missing date never counts as a difference", () => {
    expect(compareMark(file("same"), other)).toBe("same");
    expect(compareMark(file("nodate"), other)).toBe("same");
  });
  it("does nothing without a loaded other side", () => {
    expect(compareMark(file("same"), null)).toBeNull();
  });
  it("compares two 100,000 entry folders fast enough to stay interactive", () => {
    const left = Array.from({ length: 100_000 }, (_, i) => file(`f${i}`));
    const right = indexByName(Array.from({ length: 100_000 }, (_, i) => file(`f${i + 50_000}`)));
    const start = performance.now();
    const marks = left.map((e) => compareMark(e, right));
    expect(performance.now() - start).toBeLessThan(200);
    expect(marks.filter((m) => m === "only")).toHaveLength(50_000);
  });
});
