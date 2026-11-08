// The event stream from the daemon, held outside React.
//
// Twice a second a snapshot can arrive. Components subscribe to one value each (a job's progress,
// the totals, the history), and a job whose numbers did not change keeps the same object, so React
// redraws only what moved. Buttons in a job row are never replaced under the pointer.

import { useSyncExternalStore } from "react";
import { api, type Progress, type Snapshot } from "./api";

/** Two minutes at one sample a second. */
export const HISTORY_SIZE = 120;
/** Three missed heartbeats (`HEARTBEAT_EVERY` in `api.rs`). */
export const STALL_MS = 15_000;
const RETRY_MS = 2_000;
const SAMPLE_MS = 1_000;
const CLOSED = 2;

export interface Totals {
  up: number;
  down: number;
}

interface LiveSource {
  readonly readyState: number;
  close(): void;
  addEventListener(type: string, fn: (ev: { data?: unknown }) => void): void;
}

interface Options {
  open: (url: string) => LiveSource;
  /** The browser gave up on the stream: find out whether the session ended. */
  onSessionLost: () => Promise<void>;
}

const ZERO: Totals = { up: 0, down: 0 };
const EMPTY: Snapshot = { changes: -1, queue_paused: false, running: [] };

function sameProgress(a: Progress, b: Progress): boolean {
  return (
    a.bytes === b.bytes &&
    a.files === b.files &&
    a.problems === b.problems &&
    a.rate === b.rate &&
    a.total === b.total &&
    a.total_files === b.total_files &&
    a.done === b.done &&
    a.eta === b.eta &&
    a.direction === b.direction &&
    a.active.length === b.active.length &&
    a.active.every((f, i) => {
      const g = b.active[i];
      return g !== undefined && f.path === g.path && f.done === g.done && f.size === g.size;
    })
  );
}

export function createLiveStore({ open, onSessionLost }: Options) {
  const listeners = new Set<() => void>();
  let snapshot = EMPTY;
  let byJob = new Map<number, Progress>();
  let totals = ZERO;
  let history: readonly Totals[] = [];
  let connected = false;
  let source: LiveSource | null = null;
  let stallTimer: ReturnType<typeof setTimeout> | undefined;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let sampleTimer: ReturnType<typeof setInterval> | undefined;

  const notify = () => {
    for (const fn of listeners) fn();
  };

  const setConnected = (value: boolean) => {
    if (connected === value) return;
    connected = value;
    // A speed the stream can no longer vouch for is not shown.
    if (!value) {
      totals = ZERO;
      byJob = new Map();
    }
    notify();
  };

  const applySnapshot = (next: Snapshot) => {
    const jobs = new Map<number, Progress>();
    const sum = { up: 0, down: 0 };
    for (const p of next.running) {
      const old = byJob.get(p.job);
      jobs.set(p.job, old && sameProgress(old, p) ? old : p);
      if (p.direction === "push") sum.up += p.rate;
      else sum.down += p.rate;
    }
    byJob = jobs;
    if (sum.up !== totals.up || sum.down !== totals.down) totals = sum;
    snapshot = next;
    notify();
  };

  const armWatchdog = () => {
    clearTimeout(stallTimer);
    stallTimer = setTimeout(() => {
      setConnected(false);
      openStream();
    }, STALL_MS);
  };

  const alive = () => {
    setConnected(true);
    armWatchdog();
  };

  function openStream() {
    source?.close();
    clearTimeout(retryTimer);
    const es = open("/api/events");
    source = es;
    armWatchdog();
    es.addEventListener("snapshot", (ev) => {
      if (source !== es || typeof ev.data !== "string") return;
      let parsed: Snapshot;
      try {
        // The daemon that served this page sends it; see `api` in api.ts.
        parsed = JSON.parse(ev.data) as Snapshot;
      } catch {
        return;
      }
      alive();
      applySnapshot(parsed);
    });
    es.addEventListener("heartbeat", () => {
      if (source === es) alive();
    });
    es.addEventListener("error", () => {
      if (source !== es) return;
      setConnected(false);
      // While the browser is retrying by itself, the watchdog covers a retry that never lands.
      if (es.readyState !== CLOSED) return;
      void onSessionLost().finally(() => {
        if (source === es) retryTimer = setTimeout(openStream, RETRY_MS);
      });
    });
  }

  return {
    connect() {
      if (source) return;
      openStream();
      sampleTimer = setInterval(() => {
        const next = [...history, totals];
        history = next.length > HISTORY_SIZE ? next.slice(next.length - HISTORY_SIZE) : next;
        notify();
      }, SAMPLE_MS);
    },
    disconnect() {
      source?.close();
      source = null;
      clearTimeout(stallTimer);
      clearTimeout(retryTimer);
      clearInterval(sampleTimer);
      connected = false;
      snapshot = EMPTY;
      byJob = new Map();
      totals = ZERO;
      history = [];
      notify();
    },
    subscribe(fn: () => void) {
      listeners.add(fn);
      return () => {
        listeners.delete(fn);
      };
    },
    snapshot: () => snapshot,
    progressOf: (job: number) => byJob.get(job),
    totals: () => totals,
    history: () => history,
    connected: () => connected,
  };
}

export type LiveStore = ReturnType<typeof createLiveStore>;

export const live: LiveStore = createLiveStore({
  open: (url) => new EventSource(url),
  // A 401 here makes `api` show the login page.
  onSessionLost: () =>
    api("GET", "/api/settings").then(
      () => {},
      () => {},
    ),
});

/** Redraws the caller only when the value it selects changes identity. */
export function useLive<T>(select: (store: LiveStore) => T): T {
  return useSyncExternalStore(live.subscribe, () => select(live));
}
