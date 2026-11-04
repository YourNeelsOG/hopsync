// The live readout above the panes: total speeds, the last two minutes, and the queue switch.

import { useQuery } from "@tanstack/react-query";
import { ArrowDown, ArrowUp, Pause, Play } from "lucide-react";
import { useId, useMemo, useState } from "react";
import { AnimatedNumber } from "../components/ui";
import { api, type Job, message } from "../lib/api";
import { bytes } from "../lib/format";
import { HISTORY_SIZE, type Totals, useLive } from "../lib/live";
import { cn, toast } from "../lib/util";

const formatRate = (n: number) => `${bytes(n)}/s`;

function Readout({ label, value, icon, swatch }: { label: string; value: number; icon: React.ReactNode; swatch: string }) {
  return (
    <div className="min-w-0">
      <div className="flex items-center gap-1.5 text-xs text-muted">
        <span aria-hidden className={cn("size-2 rounded-full", swatch)} />
        {icon}
        {label}
      </div>
      <AnimatedNumber value={value} format={formatRate} className="block font-mono text-lg leading-6 font-medium whitespace-nowrap" />
    </div>
  );
}

function path(points: readonly number[], max: number): string {
  // Newest on the right; the x axis always spans the full two minutes.
  const start = HISTORY_SIZE - points.length;
  return points
    .map((v, i) => `${i ? "L" : "M"}${(((start + i) / (HISTORY_SIZE - 1)) * 100).toFixed(2)} ${(37 - (v / max) * 34).toFixed(2)}`)
    .join("");
}

function SpeedGraph() {
  const history = useLive((s) => s.history());
  const [hover, setHover] = useState<number | null>(null);
  const titleId = useId();
  const { down, up, max } = useMemo(() => {
    const d = history.map((h) => h.down);
    const u = history.map((h) => h.up);
    return { down: d, up: u, max: Math.max(1024 * 1024, ...d, ...u) };
  }, [history]);

  const sample: Totals | undefined = hover === null ? undefined : history[hover];
  const hoverX = hover === null ? 0 : ((HISTORY_SIZE - history.length + hover) / (HISTORY_SIZE - 1)) * 100;

  return (
    <div
      className="relative h-10 min-w-0 flex-1"
      onPointerMove={(e) => {
        const box = e.currentTarget.getBoundingClientRect();
        const slot = Math.round(((e.clientX - box.left) / box.width) * (HISTORY_SIZE - 1));
        const i = slot - (HISTORY_SIZE - history.length);
        setHover(i >= 0 && i < history.length ? i : null);
      }}
      onPointerLeave={() => setHover(null)}
    >
      <svg viewBox="0 0 100 40" preserveAspectRatio="none" role="img" aria-labelledby={titleId} className="size-full overflow-visible">
        <title id={titleId}>Download and upload speed over the last two minutes. The peak is {formatRate(max)}.</title>
        <line x1="0" x2="100" y1="37" y2="37" stroke="var(--line)" strokeWidth="1" vectorEffect="non-scaling-stroke" />
        {history.length > 1 && (
          <>
            <path d={path(down, max)} fill="none" stroke="var(--series-down)" strokeWidth="2" strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
            <path d={path(up, max)} fill="none" stroke="var(--series-up)" strokeWidth="2" strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
          </>
        )}
        {sample && <line x1={hoverX} x2={hoverX} y1="0" y2="37" stroke="var(--line-strong)" strokeWidth="1" vectorEffect="non-scaling-stroke" />}
      </svg>
      {sample && hover !== null && (
        <div
          role="status"
          className="card pointer-events-none absolute top-full z-20 mt-1 bg-raised px-2 py-1 text-xs whitespace-nowrap shadow-lg tabular-nums"
          style={hoverX > 60 ? { right: `${100 - hoverX}%` } : { left: `${hoverX}%` }}
        >
          <div className="text-muted">{history.length - 1 - hover} s ago</div>
          <div>Download {formatRate(sample.down)}</div>
          <div>Upload {formatRate(sample.up)}</div>
        </div>
      )}
    </div>
  );
}

function QueueSwitch() {
  const paused = useLive((s) => s.snapshot().queue_paused);
  const running = useLive((s) => s.snapshot().running.length);
  const { data: jobs } = useQuery<Job[]>({ queryKey: ["jobs"], enabled: false });
  const waiting = jobs?.filter((j) => j.state === "queued").length ?? 0;
  const toggle = async () => {
    try {
      await api("POST", paused ? "/api/queue/resume" : "/api/queue/pause", {});
    } catch (e) {
      toast(message(e));
    }
  };
  return (
    <div className="flex items-center gap-3">
      <div className="text-xs text-muted tabular-nums">
        <div>{running} running</div>
        <div>{waiting} waiting</div>
      </div>
      <button
        type="button"
        id="queue-toggle"
        className={cn("btn", paused && "btn-warn")}
        title={paused ? "No new jobs start until you resume. Running jobs finish." : "Stop starting new jobs. Running jobs finish."}
        onClick={() => void toggle()}
      >
        {paused ? <Play aria-hidden className="size-3.5" /> : <Pause aria-hidden className="size-3.5" />}
        {paused ? "Resume queue" : "Pause queue"}
      </button>
    </div>
  );
}

export function SpeedStrip() {
  const totals = useLive((s) => s.totals());
  return (
    <section aria-label="Live speed" className="card flex flex-wrap items-center gap-x-6 gap-y-2 px-4 py-2.5">
      <div className="flex gap-6">
        <Readout label="Download" value={totals.down} swatch="bg-series-down" icon={<ArrowDown aria-hidden className="size-3" />} />
        <Readout label="Upload" value={totals.up} swatch="bg-series-up" icon={<ArrowUp aria-hidden className="size-3" />} />
      </div>
      <div className="order-3 flex w-full min-w-40 sm:order-none sm:w-auto sm:flex-1">
        <SpeedGraph />
      </div>
      <QueueSwitch />
    </section>
  );
}
