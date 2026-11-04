// One job: where it went, what happened to every file, and why anything was skipped or failed.

import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { ArrowLeft } from "lucide-react";
import { useMemo, useRef, useState } from "react";
import { StateBadge, VirtualRows } from "../components/ui";
import { api, type Job, type JobEvent, type JobItem } from "../lib/api";
import { bytes, stamp } from "../lib/format";
import { useServers } from "../lib/hooks";
import { JobLive } from "./Queue";
import { POLICIES } from "./TransferDialog";

const PAGE = 500;
const FILTERS: [string, string][] = [
  ["", "All files"],
  ["sent", "Copied"],
  ["skipped", "Skipped"],
  ["failed", "Failed"],
  ["unchanged", "Unchanged"],
];
const OUTCOME_STYLE: Record<string, string> = { sent: "text-ok", skipped: "text-muted", unchanged: "text-muted", failed: "text-bad" };

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <>
      <dt className="text-muted">{label}</dt>
      <dd className="min-w-0 break-words">{children}</dd>
    </>
  );
}

function Files({ job }: { job: Job }) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [filter, setFilter] = useState("");
  const pages = useInfiniteQuery({
    queryKey: ["job-items", job.id, job.state],
    initialPageParam: 0,
    queryFn: ({ pageParam }) => api<JobItem[]>("GET", `/api/jobs/${job.id}/items?offset=${pageParam}&limit=${PAGE}`),
    getNextPageParam: (last, all) => (last.length === PAGE ? all.reduce((n, p) => n + p.length, 0) : undefined),
  });
  const items = useMemo(() => {
    const all = pages.data?.pages.flat() ?? [];
    return filter ? all.filter((i) => i.outcome === filter) : all;
  }, [pages.data, filter]);

  return (
    <div className="card p-4">
      <div className="mb-2 flex items-center justify-between gap-3">
        <h2>Files</h2>
        <select aria-label="Show" className="input h-7 w-auto" value={filter} onChange={(e) => setFilter(e.target.value)}>
          {FILTERS.map(([v, l]) => (
            <option key={v} value={v}>
              {l}
            </option>
          ))}
        </select>
      </div>
      {items.length === 0 && !pages.isPending && <p className="text-muted">Nothing to show.</p>}
      {items.length > 0 && (
        <div ref={scrollRef} className="scroller rounded-md border border-line" style={{ height: Math.min(items.length * 30 + 2, 480) }}>
          <VirtualRows
            count={items.length}
            rowHeight={30}
            scrollRef={scrollRef}
            renderRow={(i) => {
              const it = items[i];
              if (!it) return null;
              return (
                <div role="row" className="grid h-[30px] grid-cols-[minmax(0,2fr)_72px_72px_minmax(0,1fr)] items-center gap-2 border-b border-line px-2 text-xs">
                  <span role="cell" className="truncate" title={it.path}>
                    {it.path}
                  </span>
                  <span role="cell" className={OUTCOME_STYLE[it.outcome]}>
                    {it.outcome}
                  </span>
                  <span role="cell" className="text-right tabular-nums">
                    {it.outcome === "sent" ? bytes(it.bytes) : ""}
                  </span>
                  <span role="cell" className="truncate text-muted" title={it.detail}>
                    {it.outcome === "sent" ? "" : it.detail}
                  </span>
                </div>
              );
            }}
          />
        </div>
      )}
      {pages.hasNextPage && (
        <button type="button" className="btn mt-2" onClick={() => void pages.fetchNextPage()}>
          Load more
        </button>
      )}
    </div>
  );
}

export function JobPage({ id }: { id: number }) {
  const { data: jobs } = useQuery<Job[]>({ queryKey: ["jobs"], enabled: false });
  const fromList = jobs?.find((j) => j.id === id);
  const stateKey = fromList?.state;
  const query = useQuery({ queryKey: ["job", id, stateKey], queryFn: () => api<Job>("GET", `/api/jobs/${id}`) });
  const events = useQuery({ queryKey: ["job-events", id, stateKey], queryFn: () => api<JobEvent[]>("GET", `/api/jobs/${id}/events`) });
  const { data: servers = [] } = useServers();
  const job = query.data;

  if (query.isError) {
    return (
      <div className="card p-4">
        <p className="text-bad">No such job.</p>
        <a className="link mt-2 block" href="#/transfers">
          Back to Transfers
        </a>
      </div>
    );
  }
  if (!job) return <div aria-busy="true" className="skeleton h-40" />;

  const server = servers.find((s) => s.id === job.server)?.name ?? `server ${job.server}`;
  return (
    <div className="mx-auto flex max-w-5xl flex-col gap-3">
      <div className="card p-4">
        <a href="#/transfers" className="inline-flex items-center gap-1 text-xs text-muted hover:text-text">
          <ArrowLeft aria-hidden className="size-3" /> Transfers
        </a>
        <h1 className="mt-1 mb-3">
          Job {job.id}: {job.direction === "push" ? "upload" : "download"}
        </h1>
        <dl className="grid grid-cols-[max-content_minmax(0,1fr)] gap-x-4 gap-y-1.5">
          <Fact label="From">{job.direction === "push" ? `this box:/${job.src}` : `${server}:/${job.src}`}</Fact>
          <Fact label="To">{job.direction === "push" ? `${server}:/${job.dst}` : `this box:/${job.dst}`}</Fact>
          <Fact label="State">
            <StateBadge state={job.state} />
          </Fact>
          {job.state === "running" && (
            <Fact label="Live">
              <JobLive id={job.id} />
            </Fact>
          )}
          <Fact label="Existing files">{POLICIES.find((p) => p.value === job.existing)?.label ?? job.existing}</Fact>
          <Fact label="Verify">{job.verify ? "yes" : "no"}</Fact>
          <Fact label="Result">
            {job.files} files, {bytes(job.bytes)}, {job.problems} skipped or failed
          </Fact>
          {job.error && (
            <Fact label="Error">
              <span className="text-bad">{job.error}</span>
            </Fact>
          )}
        </dl>
      </div>
      <div className="card p-4">
        <h2 className="mb-2">Events</h2>
        {events.data?.length ? (
          <ol className="space-y-1">
            {events.data.map((e, i) => (
              <li key={`${e.at}-${i}`} className="break-words">
                <span className="mr-2 text-xs text-muted tabular-nums">{stamp(e.at)}</span>
                {e.text}
              </li>
            ))}
          </ol>
        ) : (
          <p className="text-muted">None.</p>
        )}
      </div>
      <Files job={job} />
    </div>
  );
}
