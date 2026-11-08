// The daemon's JSON API: types that mirror the Rust structs, and one request function.
//
// Text in these types comes from servers Hopsync does not trust. React renders it as text; nothing
// in this UI turns it into markup.

export type EntryKind = "file" | "dir" | "symlink" | "other";

export interface Entry {
  name: string;
  kind: EntryKind;
  size: number;
  mtime: number | null;
  valid: boolean;
}

export interface Server {
  id: number;
  name: string;
  host: string;
  port: number;
  user: string;
  root: string;
  key_file: string | null;
  key_secret: number | null;
  min_free: number;
  blocked: string | null;
  created: number;
}

export interface ServerSpec {
  name: string;
  host: string;
  port: number;
  user: string;
  root: string;
  min_free: number;
  key_file?: string;
}

export type Direction = "push" | "pull";
export type JobState = "queued" | "running" | "paused" | "done" | "failed" | "cancelled";
export type Existing = "skip" | "update" | "overwrite" | "rename";

export interface JobSpec {
  direction: Direction;
  server: number;
  src: string;
  dst: string;
  existing: Existing;
  verify: boolean;
}

export interface Job extends JobSpec {
  id: number;
  state: JobState;
  position: number;
  created: number;
  started: number | null;
  finished: number | null;
  error: string | null;
  files: number;
  bytes: number;
  problems: number;
}

export interface JobItem {
  path: string;
  outcome: string;
  bytes: number;
  detail: string;
}

export interface JobEvent {
  at: number;
  text: string;
}

export interface DryRun {
  files: number;
  bytes: number;
  problems: number;
  items: JobItem[];
  truncated: boolean;
}

export type Probe = { fingerprint: string; algorithm: string } & (
  | { status: "trusted" }
  | { status: "unknown" }
  | { status: "changed"; pinned: string[] }
);

export interface Settings {
  default_existing: Existing;
  default_verify: boolean;
  local_min_free: number;
}

export interface AuditRow {
  at: number;
  event: string;
  detail: string;
}

export interface FileProgress {
  path: string;
  done: number;
  size: number;
}

export interface Progress {
  job: number;
  direction: Direction;
  /** Bytes moved, resent pieces included; drives the speed. */
  bytes: number;
  files: number;
  problems: number;
  rate: number;
  /** Bytes the job will send; null until its counting walk finishes. */
  total: number | null;
  total_files: number | null;
  /** Bytes of the job's files that have landed, resumed bytes included. */
  done: number;
  /** Seconds left, from the daemon's ten-second speed average. */
  eta: number | null;
  /** Files in flight (at most 16). */
  active: FileProgress[];
}

export interface Snapshot {
  changes: number;
  queue_paused: boolean;
  running: Progress[];
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

export type AuthKind = "login" | "setup";

let authRequired: (kind: AuthKind) => void = () => {};

/** Sets who hears that the daemon wants a login or the first-run setup. */
export function onAuthRequired(fn: (kind: AuthKind) => void) {
  authRequired = fn;
}

function errorText(data: unknown): string | null {
  if (typeof data === "object" && data !== null && "error" in data && typeof data.error === "string") {
    return data.error;
  }
  return null;
}

/**
 * One request. The `T` is trusted, not checked: the only peer is the daemon that served this page
 * (same origin, CSP), and these types mirror its structs.
 */
/** The file route of this box (`server` null) or a connected server. */
export function fileRoute(server: number | null): string {
  return server === null ? "/api/local/file" : `/api/servers/${server}/file`;
}

/** Sends a file from this computer to `path` under a root folder. XHR, because fetch reports no upload progress. */
export function uploadFile(server: number | null, path: string, file: File, overwrite: boolean, onProgress: (sent: number) => void): Promise<void> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    const query = new URLSearchParams({ path, overwrite: String(overwrite) });
    xhr.open("PUT", `${fileRoute(server)}?${query.toString()}`);
    xhr.setRequestHeader("Content-Type", "application/octet-stream");
    xhr.upload.onprogress = (ev) => onProgress(ev.loaded);
    xhr.onerror = () => reject(new ApiError(0, "Hopsync is not reachable. Is the daemon running?"));
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve();
        return;
      }
      let text = `failed with status ${xhr.status}`;
      try {
        const data: unknown = JSON.parse(xhr.responseText);
        if (data && typeof data === "object" && "error" in data && typeof data.error === "string") text = data.error;
      } catch {
        // Not JSON: keep the status.
      }
      reject(new ApiError(xhr.status, text));
    };
    xhr.send(file);
  });
}

export async function api<T>(method: string, path: string, body?: unknown): Promise<T> {
  const init: RequestInit = { method, credentials: "same-origin" };
  if (body !== undefined) {
    init.headers = { "Content-Type": "application/json" };
    init.body = JSON.stringify(body);
  }
  let resp: Response;
  try {
    resp = await fetch(path, init);
  } catch {
    throw new ApiError(0, "Hopsync is not reachable. Is the daemon running?");
  }
  const text = await resp.text();
  let data: unknown = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = null;
    }
  }
  if (!resp.ok) {
    const msg = errorText(data) ?? `${resp.status} ${resp.statusText}`;
    if (resp.status === 401 && !path.startsWith("/api/login")) authRequired("login");
    if (resp.status === 403 && msg === "setup is required") authRequired("setup");
    throw new ApiError(resp.status, msg);
  }
  return data as T;
}

export function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
