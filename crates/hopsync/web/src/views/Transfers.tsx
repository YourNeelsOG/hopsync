// The main page: two panes, the transfer buttons between them, and the queue below.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowDown, ArrowLeft, ArrowRight, ArrowUp, ChevronUp, Download, File as FileIcon, FileQuestion, Folder, GitCompareArrows, Link2, RefreshCw, Upload } from "lucide-react";
import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRowHeight, VirtualRows } from "../components/ui";
import { api, ApiError, uploadFile, type Direction, type Entry, type EntryKind, type JobSpec, type Server } from "../lib/api";
import { compareMark, indexByName, type Mark, sortEntries } from "../lib/files";
import { bytes, joinPath, parentPath, shortStamp, stamp } from "../lib/format";
import { useServers } from "../lib/hooks";
import { cn, prefs, toast } from "../lib/util";
import { Queue } from "./Queue";
import { SpeedStrip } from "./SpeedStrip";
import { TransferDialog } from "./TransferDialog";

type Side = "left" | "right";
const DRAG_TYPE = "application/x-hopsync";

interface PaneState {
  server: number | null;
  path: string;
  setPath: (path: string) => void;
  selected: ReadonlySet<string>;
  setSelected: (next: ReadonlySet<string>) => void;
  entries: Entry[] | undefined;
  loading: boolean;
  error: string;
  refresh: () => void;
}

function usePane(kind: "local" | "remote", server: number | null): PaneState {
  const key = kind === "local" ? "path.local" : `path.${server}`;
  const [path, setPathState] = useState(() => prefs.get(key, ""));
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const client = useQueryClient();

  // A different server starts from that server's remembered folder.
  const [lastServer, setLastServer] = useState(server);
  if (server !== lastServer) {
    setLastServer(server);
    setPathState(prefs.get(key, ""));
    setSelected(new Set());
  }

  const setPath = useCallback((next: string) => {
    setPathState(next);
    setSelected(new Set());
  }, []);

  const url =
    kind === "local"
      ? `/api/local/list?path=${encodeURIComponent(path)}`
      : `/api/servers/${server}/list?path=${encodeURIComponent(path)}`;
  const enabled = kind === "local" || server !== null;
  const query = useQuery({
    queryKey: ["list", kind, server, path],
    queryFn: () => api<Entry[]>("GET", url),
    enabled,
  });

  useEffect(() => {
    if (query.data) prefs.set(key, path);
  }, [query.data, key, path]);

  // The remembered folder is gone: fall back to the root once.
  const gone = query.error instanceof ApiError && query.error.status === 404 && path !== "";
  useEffect(() => {
    if (gone) setPath("");
  }, [gone, setPath]);

  // A failed listing may have blocked the server (changed host key).
  const failed = kind === "remote" && query.isError;
  useEffect(() => {
    if (failed) void client.invalidateQueries({ queryKey: ["servers"] });
  }, [failed, client]);

  return {
    server,
    path,
    setPath,
    selected,
    setSelected,
    entries: query.data,
    loading: enabled && query.isPending,
    error: query.error ? query.error.message : "",
    refresh: () => void query.refetch(),
  };
}

const KIND_ICON: Record<EntryKind, typeof Folder> = { dir: Folder, file: FileIcon, symlink: Link2, other: FileQuestion };

const MARK_TITLE: Record<Mark, string> = {
  only: "Only on this side",
  differs: "Differs from the other side",
  same: "Same on both sides",
};

const MARK_ROW: Record<Mark, string> = {
  only: "bg-only shadow-[inset_3px_0_var(--ok)]",
  differs: "bg-differs shadow-[inset_3px_0_var(--warn)]",
  same: "",
};

function rowTitle(entry: Entry, mark: Mark | null): string {
  if (entry.valid) return mark ? `${entry.name}. ${MARK_TITLE[mark]}` : entry.name;
  if (entry.kind === "symlink") return "Symlinks are skipped, never followed";
  return "Hopsync cannot transfer this entry (special file or unsafe name)";
}

/** One listing row. Memoized: selecting a file redraws that row, not the folder. */
const FileRow = memo(function FileRow({
  entry,
  selected,
  mark,
  onToggle,
  onOpen,
  onDragFrom,
}: {
  entry: Entry;
  selected: boolean;
  mark: Mark | null;
  onToggle: (name: string) => void;
  onOpen: (name: string) => void;
  onDragFrom: (name: string, ev: React.DragEvent) => void;
}) {
  const Icon = KIND_ICON[entry.kind];
  return (
    <div
      role="row"
      aria-selected={selected}
      title={rowTitle(entry, mark)}
      draggable={entry.valid}
      onDragStart={(ev) => onDragFrom(entry.name, ev)}
      onClick={(ev) => {
        if (!entry.valid || (ev.target instanceof Element && ev.target.closest("button, input, a"))) return;
        onToggle(entry.name);
      }}
      className={cn(
        "file-grid row-height border-b border-line select-none",
        entry.valid ? "hover:bg-hover" : "text-muted italic",
        mark && MARK_ROW[mark],
        selected && "bg-selected hover:bg-selected",
      )}
    >
      <span role="cell" className="flex">
        <input type="checkbox" aria-label={`Select ${entry.name}`} checked={selected} disabled={!entry.valid} onChange={() => onToggle(entry.name)} />
      </span>
      <span role="cell" className="flex min-w-0 items-center gap-2">
        <Icon aria-hidden className={cn("size-3.5 shrink-0 text-muted", entry.kind === "dir" && "text-warn")} />
        {entry.kind === "dir" && entry.valid ? (
          <button type="button" className="link truncate" onClick={() => onOpen(entry.name)}>
            {entry.name}
          </button>
        ) : (
          <span className="truncate">{entry.name}</span>
        )}
      </span>
      <span role="cell" className="text-right text-xs whitespace-nowrap tabular-nums">
        {entry.kind === "file" ? bytes(entry.size) : ""}
      </span>
      <span role="cell" className="col-modified truncate text-xs text-muted tabular-nums" title={stamp(entry.mtime)}>
        {shortStamp(entry.mtime)}
      </span>
    </div>
  );
});

function Pane({
  side,
  pane,
  title,
  other,
  compare,
  servers,
  onServer,
  onDropFrom,
}: {
  side: Side;
  pane: PaneState;
  title: string;
  other: PaneState;
  compare: boolean;
  servers?: Server[];
  onServer?: (id: number) => void;
  onDropFrom: (from: Side) => void;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const rowHeight = useRowHeight();
  const [dropping, setDropping] = useState(false);
  const { path, setPath, selected, setSelected, entries } = pane;

  const sorted = useMemo(() => sortEntries(entries ?? []), [entries]);
  const twins = useMemo(() => (compare && other.entries && !other.error ? indexByName(other.entries) : null), [compare, other.entries, other.error]);

  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  const toggle = useCallback(
    (name: string) => {
      const next = new Set(selectedRef.current);
      if (!next.delete(name)) next.add(name);
      setSelected(next);
    },
    [setSelected],
  );
  const pathRef = useRef(path);
  pathRef.current = path;
  const open = useCallback((name: string) => setPath(joinPath(pathRef.current, name)), [setPath]);
  const dragFrom = useCallback(
    (name: string, ev: React.DragEvent) => {
      if (!selectedRef.current.has(name)) setSelected(new Set([name]));
      ev.dataTransfer.setData(DRAG_TYPE, side);
      ev.dataTransfer.effectAllowed = "copy";
    },
    [setSelected, side],
  );

  const parts = path ? path.split("/") : [];
  const canList = servers === undefined || pane.server !== null;
  const browserFiles = !servers;
  const [uploading, setUploading] = useState("");
  const fileInput = useRef<HTMLInputElement>(null);

  const sendFiles = async (files: File[]) => {
    for (const file of files) {
      const dest = joinPath(path, file.name);
      const progress = (done: number) => setUploading(`${Math.floor((done / Math.max(file.size, 1)) * 100)}% ${file.name}`);
      try {
        try {
          await uploadFile(dest, file, false, progress);
        } catch (e) {
          if (!(e instanceof ApiError && e.status === 409 && e.message.includes("already exists"))) throw e;
          if (!window.confirm(`${file.name} is already in this folder. Replace it?`)) continue;
          await uploadFile(dest, file, true, progress);
        }
        toast(`Uploaded ${file.name}.`, "ok");
      } catch (e) {
        toast(`${file.name}: ${e instanceof Error ? e.message : "upload failed"}`);
      }
      pane.refresh();
    }
    setUploading("");
  };

  const saveSelected = () => {
    const chosen = sorted.filter((e) => selected.has(e.name));
    const files = chosen.filter((e) => e.kind === "file");
    if (files.length < chosen.length) toast("Only files can be saved to this computer, not folders.");
    for (const e of files) {
      const a = document.createElement("a");
      a.href = `/api/local/file?path=${encodeURIComponent(joinPath(path, e.name))}`;
      a.download = e.name;
      a.click();
    }
  };

  return (
    <div id={`pane-${side}`} className="file-pane card flex min-h-[320px] min-w-0 flex-col overflow-hidden lg:min-h-0" aria-label={title} role="region">
      <div className="flex h-11 shrink-0 items-center gap-2 border-b border-line px-2.5">
        {servers ? (
          servers.length ? (
            <select aria-label="Server" className="input h-7 min-w-0 flex-1 font-medium" value={pane.server ?? ""} onChange={(e) => onServer?.(Number(e.target.value))}>
              {servers.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.blocked ? `${s.name} (blocked)` : s.name}
                </option>
              ))}
            </select>
          ) : (
            <strong className="flex-1">No server</strong>
          )
        ) : (
          <strong className="flex-1 px-1 whitespace-nowrap">{title}</strong>
        )}
        <span className="min-w-0 truncate text-xs whitespace-nowrap text-muted tabular-nums">{uploading || (selected.size ? `${selected.size} selected` : "")}</span>
        {browserFiles && (
          <>
            <input
              ref={fileInput}
              type="file"
              multiple
              hidden
              aria-label="Files to upload"
              onChange={(ev) => {
                const files = [...(ev.target.files ?? [])];
                ev.target.value = "";
                void sendFiles(files);
              }}
            />
            <button type="button" className="btn btn-ghost btn-icon" aria-label="Upload from this computer" title="Upload from this computer" disabled={uploading !== ""} onClick={() => fileInput.current?.click()}>
              <Upload aria-hidden className="size-3.5" />
            </button>
            <button type="button" className="btn btn-ghost btn-icon" aria-label="Save to this computer" title="Save to this computer" disabled={selected.size === 0} onClick={saveSelected}>
              <Download aria-hidden className="size-3.5" />
            </button>
          </>
        )}
        <button type="button" className="btn btn-ghost btn-icon" aria-label="Up one folder" title="Up one folder" disabled={!path} onClick={() => setPath(parentPath(path))}>
          <ChevronUp aria-hidden className="size-4" />
        </button>
        <button type="button" className="btn btn-ghost btn-icon" aria-label="Refresh" title="Refresh" disabled={!canList} onClick={pane.refresh}>
          <RefreshCw aria-hidden className="size-3.5" />
        </button>
      </div>
      {canList && (
        <nav aria-label="Folder" className="flex min-h-7 shrink-0 flex-wrap items-center gap-x-1 border-b border-line px-3 py-1 font-mono text-xs">
          <button type="button" className="link" onClick={() => setPath("")}>
            {servers ? "server root" : "root"}
          </button>
          {parts.map((part, i) => (
            <span key={parts.slice(0, i + 1).join("/")} className="flex min-w-0 items-center gap-1">
              <span className="text-muted">/</span>
              <button type="button" className="link break-all" onClick={() => setPath(parts.slice(0, i + 1).join("/"))}>
                {part}
              </button>
            </span>
          ))}
        </nav>
      )}
      <div
        ref={scrollRef}
        role="table"
        aria-rowcount={sorted.length}
        className={cn("scroller relative min-h-0 flex-1", dropping && "outline-2 -outline-offset-4 outline-accent outline-dashed")}
        onDragOver={(ev) => {
          if (ev.dataTransfer.types.includes(DRAG_TYPE)) {
            ev.preventDefault();
            setDropping(true);
          }
        }}
        onDragLeave={() => setDropping(false)}
        onDrop={(ev) => {
          setDropping(false);
          const from = ev.dataTransfer.getData(DRAG_TYPE);
          if ((from === "left" || from === "right") && from !== side) {
            ev.preventDefault();
            onDropFrom(from);
          }
        }}
      >
        <div role="row" className="file-grid sticky top-0 z-10 h-7 border-b border-line bg-surface text-[11px] font-medium text-muted">
          <span role="columnheader" aria-label="Selected" />
          <span role="columnheader">Name</span>
          <span role="columnheader" className="text-right">
            Size
          </span>
          <span role="columnheader" className="col-modified">
            Modified
          </span>
        </div>
        {pane.loading && (
          <div aria-busy="true" className="space-y-2 p-3">
            {[70, 50, 60, 40].map((w) => (
              <div key={w} className="skeleton h-4" style={{ width: `${w}%` }} />
            ))}
          </div>
        )}
        {pane.error && <p className="p-3 text-bad">{pane.error}</p>}
        {!canList && (
          <p className="p-3 text-muted">
            No servers yet.{" "}
            <a className="link" href="#/servers">
              Add a server
            </a>
          </p>
        )}
        {entries && !sorted.length && <p className="p-3 text-muted">Empty folder</p>}
        {sorted.length > 0 && (
          <VirtualRows
            count={sorted.length}
            rowHeight={rowHeight}
            scrollRef={scrollRef}
            headerHeight={28}
            renderRow={(i) => {
              const e = sorted[i];
              if (!e) return null;
              return <FileRow entry={e} selected={selected.has(e.name)} mark={compareMark(e, twins)} onToggle={toggle} onOpen={open} onDragFrom={dragFrom} />;
            }}
          />
        )}
      </div>
    </div>
  );
}

export function TransfersPage() {
  const { data: servers } = useServers();
  const [chosen, setChosen] = useState<number | null>(() => prefs.get("server", null));
  const list = servers ?? [];
  const usable = list.filter((s) => !s.blocked);
  // The remembered server may be gone or blocked.
  const server = (usable.find((s) => s.id === chosen) ?? usable[0] ?? list[0])?.id ?? null;

  const left = usePane("local", null);
  const right = usePane("remote", server);
  const [compare, setCompare] = useState(() => prefs.get("compare", false));
  const [queueTab, setQueueTab] = useState(() => prefs.get("queueTab", "active"));
  const [transfer, setTransfer] = useState<{ specs: JobSpec[]; direction: Direction; label: string; to: string } | null>(null);

  const start = (direction: Direction) => {
    const from = direction === "push" ? left : right;
    const to = direction === "push" ? right : left;
    if (server === null) {
      toast("Add a server first.");
      return;
    }
    // A file that vanished in a refresh is no longer on offer.
    const present = new Set(from.entries?.map((e) => e.name));
    const names = [...from.selected].filter((n) => present.has(n));
    if (!names.length) {
      toast(`Select files or folders on the ${direction === "push" ? "left" : "right"} first.`);
      return;
    }
    const serverName = list.find((s) => s.id === server)?.name ?? `server ${server}`;
    setTransfer({
      direction,
      label: names.length === 1 ? (names[0] ?? "") : `${names.length} items`,
      to: direction === "push" ? `${serverName}:/${to.path}` : `this box:/${to.path}`,
      specs: names.map((n) => ({
        direction,
        server,
        src: joinPath(from.path, n),
        dst: joinPath(to.path, n),
        existing: "skip",
        verify: false,
      })),
    });
  };

  const uploadable = left.selected.size > 0 && server !== null;
  const downloadable = right.selected.size > 0;

  return (
    <div className="flex flex-col gap-3 lg:h-full">
      <SpeedStrip />
      <section aria-label="Files" className="grid min-h-0 gap-3 lg:flex-[3] lg:grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)]">
        <Pane side="left" pane={left} title="This box" other={right} compare={compare} onDropFrom={() => {}} />
        <div className="flex flex-row flex-wrap items-center justify-center gap-2 lg:flex-col">
          <button type="button" id="push-btn" className="btn btn-primary" disabled={!uploadable} title={uploadable ? "Copy the selection on the left to the folder on the right" : "Select files on the left first"} onClick={() => start("push")}>
            Upload
            <ArrowRight aria-hidden className="hidden size-3.5 lg:block" />
            <ArrowDown aria-hidden className="size-3.5 lg:hidden" />
          </button>
          <button type="button" id="pull-btn" className="btn btn-primary" disabled={!downloadable} title={downloadable ? "Copy the selection on the right to the folder on the left" : "Select files on the right first"} onClick={() => start("pull")}>
            <ArrowLeft aria-hidden className="hidden size-3.5 lg:block" />
            <ArrowUp aria-hidden className="size-3.5 lg:hidden" />
            Download
          </button>
          <label className="flex min-h-7 cursor-pointer items-center gap-1.5 text-xs text-muted" title="Mark files that exist on only one side, or differ in size or date">
            <input
              type="checkbox"
              checked={compare}
              onChange={(e) => {
                setCompare(e.target.checked);
                prefs.set("compare", e.target.checked);
              }}
            />
            <GitCompareArrows aria-hidden className="size-3.5" />
            Compare
          </label>
        </div>
        <Pane
          side="right"
          pane={right}
          title="Server"
          other={left}
          compare={compare}
          servers={list}
          onServer={(id) => {
            setChosen(id);
            prefs.set("server", id);
          }}
          onDropFrom={() => start("push")}
        />
      </section>
      <Queue tab={queueTab} onTab={(t) => { setQueueTab(t); prefs.set("queueTab", t); }} servers={list} />
      {transfer && (
        <TransferDialog
          {...transfer}
          onClose={() => setTransfer(null)}
          onQueued={(paused) => {
            (transfer.direction === "push" ? left : right).setSelected(new Set());
            const tab = paused ? "queued" : "active";
            setQueueTab(tab);
            prefs.set("queueTab", tab);
          }}
        />
      )}
    </div>
  );
}
