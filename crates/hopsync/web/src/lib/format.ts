// Text for raw numbers and relative paths. The API sends bytes and seconds; people read these.

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"] as const;

export function bytes(n: number): string {
  let i = 0;
  let v = n;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i += 1;
  }
  if (i === 0) return `${Math.round(v)} B`;
  return `${v.toFixed(v < 10 ? 2 : v < 100 ? 1 : 0)} ${UNITS[i]}`;
}

export function rate(n: number): string {
  return n ? `${bytes(n)}/s` : "";
}

export function duration(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) return "";
  const s = Math.round(secs);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

export function when(epoch: number | null): string {
  if (!epoch) return "";
  const d = new Date(epoch * 1000);
  const ago = (Date.now() - d.getTime()) / 1000;
  if (ago >= 0 && ago < 60) return "just now";
  if (ago >= 0 && ago < 3600) return `${Math.floor(ago / 60)} min ago`;
  if (ago >= 0 && ago < 86400) return `${Math.floor(ago / 3600)} h ago`;
  return d.toLocaleString();
}

const SHORT_STAMP = new Intl.DateTimeFormat(undefined, { dateStyle: "short", timeStyle: "short" });

/** Fits a file list's date column. The full time is in `stamp`. */
export function shortStamp(epoch: number | null): string {
  return epoch ? SHORT_STAMP.format(new Date(epoch * 1000)) : "";
}

export function stamp(epoch: number | null): string {
  return epoch ? new Date(epoch * 1000).toLocaleString() : "";
}

const POWERS: Record<string, number> = { "": 0, k: 1, m: 2, g: 3, t: 4, p: 5 };

/** "10G", "500 MiB", "0" to bytes; null if unreadable. */
export function parseSize(s: string): number | null {
  const m = /^\s*(\d+(?:\.\d+)?)\s*([kmgtp]?)(?:i?b)?\s*$/i.exec(s);
  if (!m) return null;
  const power = POWERS[(m[2] ?? "").toLowerCase()] ?? 0;
  return Math.round(Number(m[1]) * 1024 ** power);
}

/** The shortest text for a size field that parseSize reads back exactly. */
export function sizeInput(n: number): string {
  for (const [letter, power] of [["P", 5], ["T", 4], ["G", 3], ["M", 2], ["K", 1]] as const) {
    if (n > 0 && n % 1024 ** power === 0) return `${n / 1024 ** power}${letter}`;
  }
  return String(n);
}

export function joinPath(dir: string, name: string): string {
  return dir ? `${dir}/${name}` : name;
}

export function parentPath(dir: string): string {
  const i = dir.lastIndexOf("/");
  return i < 0 ? "" : dir.slice(0, i);
}
