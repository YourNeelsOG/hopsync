import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Snapshot } from "./api";
import { createLiveStore, HISTORY_SIZE, STALL_MS } from "./live";

class FakeSource {
  static instances: FakeSource[] = [];
  readyState = 1;
  closed = false;
  handlers = new Map<string, ((ev: { data?: string }) => void)[]>();
  constructor(readonly url: string) {
    FakeSource.instances.push(this);
  }
  addEventListener(type: string, fn: (ev: { data?: string }) => void) {
    this.handlers.set(type, [...(this.handlers.get(type) ?? []), fn]);
  }
  close() {
    this.closed = true;
    this.readyState = 2;
  }
  emit(type: string, data?: unknown) {
    for (const fn of this.handlers.get(type) ?? []) {
      fn({ data: data === undefined ? undefined : JSON.stringify(data) });
    }
  }
}

const snap = (over: Partial<Snapshot> = {}): Snapshot => ({
  changes: 1,
  queue_paused: false,
  running: [],
  ...over,
});
const progress = (job: number, direction: "push" | "pull", rate: number, bytes = 10) => ({
  job,
  direction,
  bytes,
  files: 0,
  problems: 0,
  rate,
  total: 100,
});

function start() {
  const onSessionLost = vi.fn(async () => {});
  const store = createLiveStore({ open: (url) => new FakeSource(url), onSessionLost });
  store.connect();
  const source = FakeSource.instances.at(-1);
  if (!source) throw new Error("no source");
  return { store, source, onSessionLost };
}

beforeEach(() => {
  FakeSource.instances = [];
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("live store", () => {
  it("adds up speeds by direction", () => {
    const { store, source } = start();
    source.emit("snapshot", snap({ running: [progress(1, "push", 10), progress(2, "pull", 5), progress(3, "push", 1)] }));
    expect(store.totals()).toEqual({ up: 11, down: 5 });
    store.disconnect();
  });

  it("keeps the same object for a job whose numbers did not change, so its row does not redraw", () => {
    const { store, source } = start();
    source.emit("snapshot", snap({ running: [progress(1, "push", 10), progress(2, "push", 10)] }));
    const one = store.progressOf(1);
    const two = store.progressOf(2);
    source.emit("snapshot", snap({ running: [progress(1, "push", 10), progress(2, "push", 99)] }));
    expect(store.progressOf(1)).toBe(one);
    expect(store.progressOf(2)).not.toBe(two);
    expect(store.progressOf(2)?.rate).toBe(99);
    store.disconnect();
  });

  it("notifies only the subscribers whose selected value changed", () => {
    const { store, source } = start();
    source.emit("snapshot", snap({ running: [progress(1, "push", 10)] }));
    const listener = vi.fn();
    const off = store.subscribe(listener);
    source.emit("snapshot", snap({ running: [progress(1, "push", 11)] }));
    expect(listener).toHaveBeenCalledTimes(1);
    off();
    store.disconnect();
  });

  it("ignores a malformed snapshot and keeps the last good one", () => {
    const { store, source } = start();
    source.emit("snapshot", snap({ queue_paused: true }));
    for (const fn of source.handlers.get("snapshot") ?? []) fn({ data: "{not json" });
    expect(store.snapshot().queue_paused).toBe(true);
    store.disconnect();
  });

  it("samples one point a second into a history of bounded size", () => {
    const { store, source } = start();
    source.emit("snapshot", snap({ running: [progress(1, "pull", 7)] }));
    vi.advanceTimersByTime(1000);
    expect(store.history().at(-1)).toEqual({ up: 0, down: 7 });
    vi.advanceTimersByTime((HISTORY_SIZE + 50) * 1000);
    expect(store.history()).toHaveLength(HISTORY_SIZE);
    store.disconnect();
  });

  it("drops to zero speed rather than showing a stale number once the stream is lost", () => {
    const { store, source } = start();
    source.emit("snapshot", snap({ running: [progress(1, "pull", 7)] }));
    vi.advanceTimersByTime(STALL_MS + 1);
    expect(store.connected()).toBe(false);
    expect(store.totals()).toEqual({ up: 0, down: 0 });
    store.disconnect();
  });

  it("reopens a stream that went silent, and heartbeats count as life", () => {
    const { store, source } = start();
    vi.advanceTimersByTime(STALL_MS - 1000);
    source.emit("heartbeat", 1);
    vi.advanceTimersByTime(STALL_MS - 1000);
    expect(source.closed).toBe(false);
    expect(FakeSource.instances).toHaveLength(1);
    vi.advanceTimersByTime(1001);
    expect(source.closed).toBe(true);
    expect(FakeSource.instances).toHaveLength(2);
    store.disconnect();
  });

  it("asks about the session when the browser gives up on the stream", async () => {
    const { store, source, onSessionLost } = start();
    source.close();
    source.emit("error");
    await vi.advanceTimersByTimeAsync(0);
    expect(onSessionLost).toHaveBeenCalledTimes(1);
    store.disconnect();
  });

  it("stops everything on disconnect", () => {
    const { store, source } = start();
    store.disconnect();
    expect(source.closed).toBe(true);
    vi.advanceTimersByTime(STALL_MS * 3);
    expect(FakeSource.instances).toHaveLength(1);
  });
});
