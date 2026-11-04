// The bug this UI was rewritten to fix: the old queue rebuilt its DOM on every progress update, so a
// button could be replaced between mouse down and mouse up, and focus was lost.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, screen } from "@testing-library/react";
import { domAnimation, LazyMotion } from "motion/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Job, Progress } from "../lib/api";
import { live } from "../lib/live";
import { JobRow } from "./Queue";

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

const progress = (bytes: number): Progress => ({ job: 7, direction: "push", bytes, files: 0, problems: 0, rate: bytes, total: 1000 });

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
