// Listing order and the compare marks between two panes.

import type { Entry } from "./api";

export type Mark = "only" | "differs" | "same";

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

/** Folders first, then by name with numbers in numeric order. Returns a new array. */
export function sortEntries(entries: readonly Entry[]): Entry[] {
  return [...entries].sort((a, b) => {
    if ((a.kind === "dir") !== (b.kind === "dir")) return a.kind === "dir" ? -1 : 1;
    return collator.compare(a.name, b.name);
  });
}

export function indexByName(entries: readonly Entry[]): Map<string, Entry> {
  return new Map(entries.map((e) => [e.name, e]));
}

/** How `entry` relates to the other pane. A lookup per row keeps big folders linear in size. */
export function compareMark(entry: Entry, other: ReadonlyMap<string, Entry> | null): Mark | null {
  if (!other) return null;
  const twin = other.get(entry.name);
  if (!twin) return "only";
  if (entry.kind !== twin.kind) return "differs";
  if (entry.kind !== "file") return "same";
  if (entry.size !== twin.size) return "differs";
  // SFTP servers may omit a date, and a missing one proves nothing.
  if (entry.mtime && twin.mtime && entry.mtime !== twin.mtime) return "differs";
  return "same";
}
