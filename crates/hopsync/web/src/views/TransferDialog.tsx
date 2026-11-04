// The one place every transfer choice is made: policy, verify, and a dry-run preview before queueing.
// Nothing asks a question once a job runs.

import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { Dialog, DialogButtons, Field, VirtualRows } from "../components/ui";
import { api, type Direction, type DryRun, type Existing, type JobSpec, message } from "../lib/api";
import { bytes, joinPath } from "../lib/format";
import { useSettings } from "../lib/hooks";
import { live } from "../lib/live";
import { cn, toast } from "../lib/util";

export const POLICIES: { value: Existing; label: string; hint: string }[] = [
  { value: "skip", label: "Skip existing files", hint: "Leave files that already exist at the destination alone." },
  { value: "update", label: "Update changed files", hint: "Replace files whose size or date differ, but keep ones that are newer at the destination." },
  { value: "overwrite", label: "Overwrite", hint: "Replace every existing file." },
  { value: "rename", label: "Keep both", hint: 'Write the new file beside the old one as "name (1).ext".' },
];

type Result = { spec: JobSpec; plan: DryRun } | { spec: JobSpec; error: string };

const OUTCOME_LABEL: Record<string, string> = { create: "new", replace: "replace", unchanged: "unchanged", skipped: "skip", failed: "fail" };
const OUTCOME_STYLE: Record<string, string> = { create: "text-ok", replace: "text-warn", unchanged: "text-muted", skipped: "text-muted", failed: "text-bad", error: "text-bad" };

interface PreviewRow {
  path: string;
  outcome: string;
  size: string;
  detail: string;
}

function rowsOf(results: Result[]): PreviewRow[] {
  const rows: PreviewRow[] = [];
  for (const r of results) {
    if ("error" in r) {
      rows.push({ path: r.spec.src, outcome: "error", size: "", detail: r.error });
      continue;
    }
    for (const it of r.plan.items) {
      const path = results.length > 1 ? joinPath(r.spec.src, it.path) : it.path;
      const copied = it.outcome === "create" || it.outcome === "replace";
      const explained = it.outcome === "skipped" || it.outcome === "failed";
      rows.push({ path: path || r.spec.src, outcome: it.outcome, size: copied ? bytes(it.bytes) : "", detail: explained ? it.detail : "" });
    }
  }
  return rows;
}

function summary(results: Result[]): string {
  let files = 0;
  let size = 0;
  let problems = 0;
  const counts: Record<string, number> = {};
  for (const r of results) {
    if ("error" in r) continue;
    files += r.plan.files;
    size += r.plan.bytes;
    problems += r.plan.problems;
    for (const it of r.plan.items) counts[it.outcome] = (counts[it.outcome] ?? 0) + 1;
  }
  return [
    `${files} file${files === 1 ? "" : "s"} to copy (${bytes(size)})`,
    counts.replace ? `${counts.replace} replaced` : null,
    counts.skipped ? `${counts.skipped} skipped` : null,
    counts.unchanged ? `${counts.unchanged} unchanged` : null,
    problems && !counts.skipped ? `${problems} problems` : null,
  ]
    .filter(Boolean)
    .join(" · ");
}

function Preview({ results }: { results: Result[] }) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const rows = useMemo(() => rowsOf(results), [results]);
  const truncated = results.some((r) => "plan" in r && r.plan.truncated);
  return (
    <div className="mt-3">
      <p className="font-medium tabular-nums">{summary(results)}</p>
      {rows.length ? (
        <div ref={scrollRef} className="scroller mt-2 rounded-md border border-line" style={{ height: Math.min(rows.length * 28 + 2, 280) }}>
          <VirtualRows
            count={rows.length}
            rowHeight={28}
            scrollRef={scrollRef}
            renderRow={(i) => {
              const r = rows[i];
              if (!r) return null;
              return (
                <div role="row" className="grid h-7 grid-cols-[minmax(0,2fr)_70px_70px_minmax(0,1fr)] items-center gap-2 border-b border-line px-2 text-xs">
                  <span role="cell" className="truncate" title={r.path}>
                    {r.path}
                  </span>
                  <span role="cell" className={OUTCOME_STYLE[r.outcome]}>
                    {OUTCOME_LABEL[r.outcome] ?? r.outcome}
                  </span>
                  <span role="cell" className="text-right tabular-nums">
                    {r.size}
                  </span>
                  <span role="cell" className="truncate text-muted" title={r.detail}>
                    {r.detail}
                  </span>
                </div>
              );
            }}
          />
        </div>
      ) : (
        <p className="mt-1 text-muted">Nothing to do.</p>
      )}
      {truncated && <p className="mt-1 text-xs text-muted">Only the first 10,000 entries are listed. The totals cover everything.</p>}
    </div>
  );
}

export function TransferDialog({
  specs,
  direction,
  label,
  to,
  onClose,
  onQueued,
}: {
  specs: JobSpec[];
  direction: Direction;
  label: string;
  to: string;
  onClose: () => void;
  onQueued: (queuePaused: boolean) => void;
}) {
  const { data: settings } = useSettings();
  const client = useQueryClient();
  const [policy, setPolicy] = useState<Existing | null>(null);
  const [verify, setVerify] = useState<boolean | null>(null);
  const [results, setResults] = useState<Result[] | null>(null);
  const [queueing, setQueueing] = useState(false);

  const existing = policy ?? settings?.default_existing ?? "skip";
  const verifying = verify ?? settings?.default_verify ?? false;

  useEffect(() => {
    let stale = false;
    setResults(null);
    void (async () => {
      const out: Result[] = [];
      for (const s of specs) {
        try {
          out.push({ spec: s, plan: await api<DryRun>("POST", "/api/jobs/dry-run", { ...s, existing, verify: verifying }) });
        } catch (e) {
          out.push({ spec: s, error: message(e) });
        }
        if (stale) return;
      }
      setResults(out);
    })();
    return () => {
      stale = true;
    };
  }, [specs, existing, verifying]);

  const queue = async () => {
    setQueueing(true);
    let queued = 0;
    for (const s of specs) {
      try {
        await api("POST", "/api/jobs", { ...s, existing, verify: verifying });
        queued += 1;
      } catch (e) {
        toast(message(e));
      }
    }
    if (queued) {
      toast(`Queued ${queued} job${queued === 1 ? "" : "s"}.`, "ok");
      onQueued(live.snapshot().queue_paused);
      await client.invalidateQueries({ queryKey: ["jobs"] });
    }
    onClose();
  };

  const allFailed = results?.every((r) => "error" in r) ?? true;

  return (
    <Dialog wide title={`${direction === "push" ? "Upload" : "Download"} ${label}`} onClose={onClose}>
      <p className="break-all text-muted">To {to}</p>
      <div className="mt-1 grid items-end gap-x-4 sm:grid-cols-2">
        <Field label="If a file exists">
          <select name="existing" className="input" value={existing} onChange={(e) => setPolicy(POLICIES.find((p) => p.value === e.target.value)?.value ?? "skip")}>
            {POLICIES.map((p) => (
              <option key={p.value} value={p.value}>
                {p.label}
              </option>
            ))}
          </select>
        </Field>
        <label className="my-2.5 flex min-h-8 items-center gap-2">
          <input type="checkbox" name="verify" checked={verifying} onChange={(e) => setVerify(e.target.checked)} />
          Verify after copying (reads everything back)
        </label>
      </div>
      <small className="text-xs text-muted">{POLICIES.find((p) => p.value === existing)?.hint}</small>
      {results ? (
        <Preview results={results} />
      ) : (
        <p aria-busy="true" className={cn("mt-3 text-muted")}>
          Checking what would happen…
        </p>
      )}
      <DialogButtons>
        <button type="button" className="btn btn-ghost" onClick={onClose}>
          Cancel
        </button>
        <button type="button" className="btn btn-primary" disabled={allFailed || queueing} onClick={() => void queue()}>
          Queue
        </button>
      </DialogButtons>
    </Dialog>
  );
}
