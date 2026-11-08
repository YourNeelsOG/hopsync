// The bug this UI was rewritten to fix: the old queue rebuilt its DOM on every progress update, so a
// button could be replaced between mouse down and mouse up, and focus was lost.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, screen } from "@testing-library/react";
import { domAnimation, LazyMotion } from "motion/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Job, Progress } from "../lib/api";
import { live } from "../lib/live";
import { JobRow, QueueTotals } from "./Queue";

let emit: (data: { changes: number; queue_paused: boolean; running: Progress[] }) => void;

vi.stubGlobal(
  "EventSource",
  class {
    readyState = 1;
    close() {}
    addEventListener(type: string, fn: (ev: { data: string }) => void) {
      if (type === "snapshot") emit = (data) => fn({ data: JSON.stringify(data) });
    }
  },
);

const job: Job = {
  id: 7,
  direction: "push",
  server: 1,
  src: "Shows/big.mkv",
  dst: "big.mkv",
  existing: "skip",
  verify: false,
  state: "running",
  position: 1,
  created: 1,
  started: 1,
  finished: null,
  error: null,
  files: 0,
  bytes: 0,
  problems: 0,
};

const progress = (bytes: number, more: Partial<Progress> = {}): Progress => ({
  job: 7,
  direction: "push",
  bytes,
  files: 0,
  problems: 0,
  rate: bytes,
  total: 1000,
  total_files: null,
  done: bytes,
  eta: null,
  active: [],
  ...more,
});

function show() {
  render(
    <QueryClientProvider client={new QueryClient()}>
      <LazyMotion features={domAnimation}>
        <JobRow job={job} serverName="seedbox" before={null} />
      </LazyMotion>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {} }));
  live.connect();
});
afterEach(() => {
  cleanup();
  live.disconnect();
});

describe("a job row during a transfer", () => {
  it("calls files it left alone skipped, not problems", () => {
    show();
    act(() => emit({ changes: 1, queue_paused: false, running: [{ ...progress(100), problems: 3 }] }));
    expect(screen.getByText("3 skipped or failed")).toBeTruthy();
    expect(screen.queryByText(/problems/)).toBeNull();
  });

  it("keeps the same buttons and focus while progress updates arrive", () => {
    show();
    act(() => emit({ changes: 1, queue_paused: false, running: [progress(100)] }));
    const pause = screen.getByRole("button", { name: "Pause" });
    pause.focus();
    for (const b of [200, 300, 400]) act(() => emit({ changes: 1, queue_paused: false, running: [progress(b)] }));
    expect(screen.getByRole("button", { name: "Pause" })).toBe(pause);
    expect(document.activeElement).toBe(pause);
  });

  it("moves its bar with a transform, without layout", () => {
    show();
    act(() => emit({ changes: 1, queue_paused: false, running: [progress(500)] }));
    const fill = screen.getByRole("progressbar").firstElementChild;
    expect(fill).toBeInstanceOf(HTMLElement);
    expect(fill instanceof HTMLElement ? fill.style.transform : "").toBe("scaleX(0.5)");
    expect(fill instanceof HTMLElement ? fill.style.width : "x").toBe("");
  });

  it("shows hostile text from the server as text", () => {
    render(
      <QueryClientProvider client={new QueryClient()}>
        <LazyMotion features={domAnimation}>
          <JobRow job={{ ...job, state: "failed", error: "<img src=x onerror=alert(1)>" }} serverName="<b>x</b>" before={null} />
        </LazyMotion>
      </QueryClientProvider>,
    );
    expect(screen.getByText("<img src=x onerror=alert(1)>")).toBeTruthy();
    expect(document.querySelector("img")).toBeNull();
    expect(document.querySelector("b")).toBeNull();
  });
});

describe("progress numbers", () => {
  const MiB = 1 << 20;
  // The amount is an animated number followed by " of <total>": match the whole line of text.
  const whole = (text: string) => (_: string, el: Element | null) => el?.tagName === "SPAN" && el.textContent === text;

  it("shows bytes landed of the total, files of the total, and the server's ETA", () => {
    show();
    act(() =>
      emit({
        changes: 1,
        queue_paused: false,
        running: [progress(300 * MiB, { done: 250 * MiB, total: 1000 * MiB, files: 3, total_files: 12, eta: 125 })],
      }),
    );
    const fill = screen.getByRole("progressbar", { name: "Progress" }).firstElementChild;
    expect(fill instanceof HTMLElement ? fill.style.transform : "").toBe("scaleX(0.25)");
    expect(screen.getByText(whole("250 MiB of 1000 MiB"))).toBeTruthy();
    expect(screen.getByText("25%")).toBeTruthy();
    expect(screen.getByText("3 of 12 files")).toBeTruthy();
    expect(screen.getByText("2m 5s left")).toBeTruthy();
  });

  it("gives each file in flight its own bar", () => {
    show();
    act(() =>
      emit({
        changes: 1,
        queue_paused: false,
        running: [
          progress(10, {
            active: [
              { path: "Show/E01.mkv", done: 3 * MiB, size: 4 * MiB },
              { path: "Show/E02.mkv", done: 0, size: 4 * MiB },
            ],
          }),
        ],
      }),
    );
    const e01 = screen.getByRole("progressbar", { name: "Show/E01.mkv" });
    expect(e01.getAttribute("aria-valuenow")).toBe(String(3 * MiB));
    expect(e01.firstElementChild instanceof HTMLElement ? e01.firstElementChild.style.transform : "").toBe("scaleX(0.75)");
    expect(screen.getByRole("progressbar", { name: "Show/E02.mkv" })).toBeTruthy();
  });

  it("totals every running job in one line", () => {
    render(<QueueTotals />);
    act(() =>
      emit({
        changes: 1,
        queue_paused: false,
        running: [
          progress(0, { job: 1, done: 100 * MiB, total: 400 * MiB, files: 1, total_files: 4, eta: 60, rate: 2 * MiB }),
          progress(0, { job: 2, done: 100 * MiB, total: 600 * MiB, files: 2, total_files: 6, eta: 300, rate: 3 * MiB }),
        ],
      }),
    );
    expect(screen.getByText(whole("200 MiB of 1000 MiB"))).toBeTruthy();
    expect(screen.getByText("20%")).toBeTruthy();
    expect(screen.getByText("3 of 10 files")).toBeTruthy();
    expect(screen.getByText("5m 0s left")).toBeTruthy();
  });
});

