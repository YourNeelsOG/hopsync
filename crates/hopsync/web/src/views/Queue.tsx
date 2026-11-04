// The job queue. A row's frame and buttons come from the job list, which changes rarely. Its live
// numbers are a separate component subscribed to that job alone, so a progress update never replaces
// a button under the pointer.

import * as Tabs from "@radix-ui/react-tabs";
import { ArrowDown, ArrowUp, ChevronUp } from "lucide-react";
import { AnimatePresence, m } from "motion/react";
import { memo } from "react";
import { AnimatedNumber, StateBadge } from "../components/ui";
import type { Job, Server } from "../lib/api";
import { bytes, duration, when } from "../lib/format";
import { useJobAction, useJobs } from "../lib/hooks";
import { useLive } from "../lib/live";

const TABS: { id: string; label: string; empty: string; pick: (j: Job) => boolean }[] = [
  { id: "active", label: "Active", empty: "Nothing is running.", pick: (j) => j.state === "running" },
  { id: "queued", label: "Queued", empty: "The queue is empty.", pick: (j) => j.state === "queued" || j.state === "paused" },
  { id: "failed", label: "Failed", empty: "No failed or cancelled jobs.", pick: (j) => j.state === "failed" || j.state === "cancelled" },
  { id: "done", label: "Done", empty: "No finished jobs yet.", pick: (j) => j.state === "done" },
];

const formatBytes = (n: number) => bytes(Math.round(n));
const formatRate = (n: number) => (n >= 1 ? `${bytes(Math.round(n))}/s` : "starting");

/** Live numbers for one running job. */
export function JobLive({ id }: { id: number }) {
  const p = useLive((s) => s.progressOf(id));
  if (!p) {
    return (
      <div className="flex items-center gap-2">
        <StateBadge state="running" />
        <span className="text-xs text-muted">Starting</span>
      </div>
    );
  }
  const share = p.total ? Math.min(p.bytes / p.total, 1) : null;
  const eta = p.total && p.rate ? duration((p.total - p.bytes) / p.rate) : "";
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <div
        className="bar-track"
        role="progressbar"
        aria-label="Progress"
        aria-valuemin={0}
        aria-valuemax={p.total ?? undefined}
        aria-valuenow={p.total ? Math.min(p.bytes, p.total) : undefined}
      >
        {share === null ? <div className="bar-fill bar-unknown" /> : <div className="bar-fill" style={{ transform: `scaleX(${share})` }} />}
      </div>
      <div className="flex flex-wrap items-baseline gap-x-2 text-xs text-muted tabular-nums">
        <span data-testid="job-rate" className="font-mono font-medium text-text">
          <AnimatedNumber value={p.rate} format={formatRate} />
        </span>
        <span>
          <AnimatedNumber value={p.bytes} format={formatBytes} />
          {p.total ? ` of ${bytes(p.total)}` : ""}
        </span>
        {eta && <span>{eta} left</span>}
        <span>
          {p.files} file{p.files === 1 ? "" : "s"}
        </span>
        {p.problems > 0 && <span className="text-warn">{p.problems} problems</span>}
      </div>
    </div>
  );
}

function finishedDetail(j: Job): string {
  if (j.state === "done") return `${j.files} files, ${bytes(j.bytes)}${j.problems ? `, ${j.problems} skipped or failed` : ""} · ${when(j.finished)}`;
  if (j.state === "failed") return j.error ?? "";
  if (j.state === "cancelled") return when(j.finished);
  return "";
}

export const JobRow = memo(function JobRow({ job: j, serverName, before }: { job: Job; serverName: string; before: number | null }) {
  const action = useJobAction();
  const act = (label: string, path: string, title?: string) => (
    <button type="button" className="btn btn-ghost btn-sm" title={title} onClick={() => action.mutate({ path: `/api/jobs/${j.id}/${path}` })}>
      {label}
    </button>
  );
  const final = j.state === "done" || j.state === "failed" || j.state === "cancelled";
  const Arrow = j.direction === "push" ? ArrowUp : ArrowDown;
  return (
    <div data-job={j.id} className="grid items-center gap-x-4 gap-y-1.5 border-b border-line px-3 py-2 last:border-b-0 md:grid-cols-[minmax(0,2fr)_minmax(0,2fr)_auto]">
      <div className="flex min-w-0 items-center gap-2">
        <Arrow aria-label={j.direction === "push" ? "Upload" : "Download"} className="size-3.5 shrink-0 text-muted" />
        <div className="min-w-0">
          <a href={`#/job/${j.id}`} className="block truncate font-medium hover:underline" title={j.direction === "push" ? j.src : j.dst}>
            {j.direction === "push" ? j.src : j.dst}
          </a>
          <div className="truncate text-xs text-muted" title={`${serverName}:/${j.direction === "push" ? j.dst : j.src}`}>
            {serverName}:/{j.direction === "push" ? j.dst : j.src}
          </div>
        </div>
      </div>
      <div className="min-w-0">
        {j.state === "running" ? (
          <JobLive id={j.id} />
        ) : (
          <div className="flex min-w-0 items-center gap-2 text-xs">
            <StateBadge state={j.state} />
            <span className={j.state === "failed" ? "break-words text-bad" : "truncate text-muted"}>{finishedDetail(j)}</span>
          </div>
        )}
      </div>
      <div className="flex flex-wrap justify-end gap-1">
        {(j.state === "running" || j.state === "queued") && act("Pause", "pause", "Stop for now. It resumes where it left off.")}
        {j.state === "paused" && act("Resume", "resume")}
        {(j.state === "failed" || j.state === "cancelled") && act("Retry", "resume", "Queue again. Finished parts are not copied again.")}
        {!final && act("Cancel", "cancel")}
        {j.state === "queued" && before !== null && (
          <button type="button" className="btn btn-ghost btn-icon" aria-label="Move up" title="Move up" onClick={() => action.mutate({ path: `/api/jobs/${j.id}/move`, body: { before } })}>
            <ChevronUp aria-hidden className="size-4" />
          </button>
        )}
        {final && (
          <button type="button" className="btn btn-ghost btn-sm" onClick={() => action.mutate({ method: "DELETE", path: `/api/jobs/${j.id}` })}>
            Remove
          </button>
        )}
      </div>
    </div>
  );
});

export function Queue({ tab, onTab, servers }: { tab: string; onTab: (t: string) => void; servers: Server[] }) {
  const { data: jobs = [] } = useJobs();
  const clear = useJobAction();
  const current = TABS.find((t) => t.id === tab) ?? TABS[0];
  if (!current) return null;
  const shown = jobs.filter(current.pick);
  if (current.id === "done" || current.id === "failed") shown.reverse();
  const nameOf = (id: number) => servers.find((s) => s.id === id)?.name ?? `server ${id}`;

  return (
    <Tabs.Root id="queue" value={current.id} onValueChange={onTab} className="card flex min-h-40 flex-col overflow-hidden lg:min-h-0 lg:flex-[2]">
      <div className="flex items-center gap-1 border-b border-line px-2">
        <Tabs.List aria-label="Queue" className="flex overflow-x-auto">
          {TABS.map((t) => {
            const n = jobs.filter(t.pick).length;
            return (
              <Tabs.Trigger
                key={t.id}
                value={t.id}
                className="border-b-2 border-transparent px-3 py-2 font-medium whitespace-nowrap text-muted transition-colors duration-100 hover:text-text data-[state=active]:border-accent data-[state=active]:text-text"
              >
                {t.label}
                {n > 0 && <span className="ml-1.5 text-xs text-muted tabular-nums">{n}</span>}
              </Tabs.Trigger>
            );
          })}
        </Tabs.List>
        {current.id === "done" && shown.length > 0 && (
          <button
            type="button"
            className="btn btn-ghost btn-sm ml-auto"
            onClick={() => {
              for (const j of shown) clear.mutate({ method: "DELETE", path: `/api/jobs/${j.id}` });
            }}
          >
            Clear done
          </button>
        )}
      </div>
      <Tabs.Content value={current.id} tabIndex={-1} className="scroller min-h-0 flex-1">
        {shown.length === 0 && <p className="p-3 text-muted">{current.empty}</p>}
        <AnimatePresence initial={false}>
          {shown.map((j, i) => (
            <m.div key={j.id} initial={{ opacity: 0, y: 4 }} animate={{ opacity: 1, y: 0 }} exit={{ opacity: 0 }} transition={{ duration: 0.15, ease: "easeOut" }}>
              <JobRow job={j} serverName={nameOf(j.server)} before={shown[i - 1]?.id ?? null} />
            </m.div>
          ))}
        </AnimatePresence>
      </Tabs.Content>
    </Tabs.Root>
  );
}
