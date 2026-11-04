# UX study: rclone, rsync, lsyncd, rclone bisync and FileZilla (2026-11-04)

Purpose: before building the Phase 2 API and the Phase 3 web UI, record what the tools Hopsync
replaces do well and badly for a user, and turn that into requirements. Security lessons from the
same tools are in `../filezilla-rclone-lsyncd-rsync-research-paper.md` (Appendix A) and plan.md §7.
This study covers how the tools feel to use.

Sources: rclone docs (bisync, rc, GUI), the rsync man page, the lsyncd manual, FileZilla Pro docs
(toolbar, transfer queue, default file-exists action, advanced features), FileZilla trac tickets
#1371 and #5145. The FileZilla wiki refused automated access (HTTP 403).

## 1. What each tool gets right

### FileZilla: the mental model everyone already has

- **Two panes, local left and remote right**, each with a directory tree and a file list. You move
  files by dragging them between the panes. Most seedbox users learned file transfer this way.
- **Site Manager.** Saved servers with a name, host, port, user and auth type. A dropdown lists
  recent sites. There is also Quickconnect for one-off hosts. Export and import exist.
- **Transfer queue with three tabs: Queued, Failed, Successful.** Failed items can be inspected,
  re-queued or removed. You can pause and resume the whole queue ("Process Queue"). Transfers you
  start directly jump ahead of queued ones. You can choose an action to run when the queue finishes.
- **File-exists dialog.** Overwrite, overwrite if source newer, overwrite if size differs or source
  newer, resume, rename, or skip. The choice can apply once, to the whole queue, or be set as the
  default in settings. You can also change it on single queue items.
- **Message log.** The raw protocol conversation. Power users read it to find out why something
  failed.
- **Directory comparison.** Highlights files that exist on only one side or differ in size or date.
  **Synchronized browsing** moves both panes together.
- **Speed limits and live speed** in the status bar. **Filename filters** hide noise in listings.
- **Host-key prompt** on first connect, with the fingerprint and a "trust always" choice.

What hurts:

- The Failed and Successful tabs are cleared on exit, by design (#1371). After a restart you cannot
  tell what happened overnight.
- The file-exists dialog is modal. It stops an unattended queue until someone clicks it.
- Passwords were stored obfuscated (F1). The master password came late and is opt-in.
- Everything runs on the desktop. Nothing can run headless on a seedbox.
- One connection per file. Big files over long links are slow.

### rclone (CLI, rc API, web GUI)

- **Structured live stats** (`core/stats`): bytes, total, speed, ETA, error count, and a
  "transferring" list with per-file progress. The web GUI and every third-party UI are built on
  these fields.
- **Async jobs** (`job/status`, `job/list`, `job/stop`) and stats groups per job. A UI can start a
  job and follow it without holding the HTTP request open.
- **History of the last 100 transfers** (`core/transferred`), each with size, error and time.
- **`--dry-run` everywhere**, and global **`--bwlimit`** that can be changed live.
- **Web GUI pages:** Dashboard, Explorer (two-pane), Remotes, Mounts, Serves, Settings. It binds
  to localhost by default and warns against running without auth.

What hurts: the GUI is a separate bundle fetched at runtime. The rc API includes command-running
endpoints, which led to the R1 and R8 incidents. Remote config is a long, jargon-heavy wizard.
Progress text is dense and built for terminals.

### rsync

- **`--info=progress2`**: one overall line (bytes, percent, speed, ETA) instead of one line per
  file. This works well for large trees.
- **`--itemize-changes`**: one line per file saying what happened and why (new, size changed, time
  changed, skipped). It answers "what did it actually do".
- **`--dry-run`** with the same output, so the preview and the real run look identical.
- **`--partial` / `--partial-dir`**, **`--update`**, **`--checksum`**, **`--bwlimit`**,
  **`--stats`**, and **`-h`** human-readable sizes.
- **`--delete` is opt-in**, with `--max-delete` as a brake.

What hurts: no UI, no queue and no persistence. A dropped SSH session means starting the command
again by hand.

### lsyncd

- **Declarative config** (a Lua table): source, target, `delay` (batching window),
  `maxProcesses`, excludes.
- **An `init` sync at startup**, so the target catches up before the watch starts. The manual
  calls `init = false` dangerous.
- **`delete = true | false | 'startup' | 'running'`**: deletions are a named, visible mode, not a
  side effect.
- **`statusFile`**: a plain file a human can read to see what the daemon is doing.

What hurts: the config can run arbitrary Lua and shell commands (L1, L4). There is no UI, and
errors end up in syslog.

### rclone bisync and `--resync` ("resync")

- **The first run must be `--resync`.** The tool refuses to guess a baseline. A `--resync` is also
  required after a filter change or a critical error, and the message says why.
- **Safety brakes:** `--max-delete` (default 50 %), "all files changed" and "path empty" checks
  that abort instead of propagating a wipe, and `--check-access` sentinel files.
- **Conflicts are explicit:** `--conflict-resolve` (newer, older, larger, smaller, path1, path2),
  `--conflict-loser` (rename or delete), and `--conflict-suffix`.
- **Lock files**, plus `--recover` and `--resilient` to continue after interruptions. Exit codes
  separate "retry" from "needs a human".

What hurts: the recovery story needs a manual-page read the first time anything goes wrong.

## 2. Requirements for Hopsync

Priority: **must** for v1 (Phases 2 and 3), **should** for v1 if cheap, **later** once mirror/sync
exists. Each row names the API surface Phase 2 has to provide.

| # | Requirement | From | Priority | API needed (Phase 2) |
|---|---|---|---|---|
| U1 | Two-pane browser: "this box" on the left, any saved server on the right; tree plus list, sizes human-readable, sort by name/size/date. Drag a selection across to queue a job. | FileZilla | must | `GET /api/servers/{id}/list?path=`, `GET /api/local/list?path=`, confined to roots |
| U2 | Servers page (Site Manager): name, host, port, user, auth (generated key, existing key file, stored password), roots. Edit, test connection, remove. No secrets in any export. | FileZilla, rclone | must | `servers` CRUD, `POST /api/servers/{id}/test` |
| U3 | Add-server flow shows the host-key fingerprint and waits for an explicit confirm. A changed key shows old and new fingerprints and blocks the server until re-confirmed. | FileZilla, Appendix A | must | `POST /api/servers/probe` returns the offered key; `POST /api/servers/{id}/trust` pins it |
| U4 | Key install: generate a per-server ed25519 key and install it with a one-time password that is never stored. | plan.md §7 | must | `POST /api/servers/{id}/install-key` (the password lives only in the request) |
| U5 | Queue with tabs **Active / Queued / Failed / Done** that **persist across restarts** (FileZilla's biggest gap). Failed items show the error and offer Retry and Remove. | FileZilla | must | `jobs` table; `GET /api/jobs?state=`; `POST /api/jobs/{id}/retry\|pause\|resume\|cancel`, `DELETE` |
| U6 | Pause and resume the whole queue. Jobs run in queue order with a concurrency limit, and the user can reorder them. | FileZilla | must | `POST /api/queue/pause\|resume`, `PATCH /api/jobs/{id}` (position) |
| U7 | The existing-file policy is chosen **when the job is created** (default from settings), never in a modal mid-run. Options: skip, update, overwrite, rename. Resume is always on. | FileZilla, rsync `--update` | must | `policy` field on job creation; maps to engine `Existing` |
| U8 | **Dry-run preview** before a big job: counts and bytes of new, skipped, replaced and renamed files, plus problems (symlinks, odd names), item by item. | rsync `-n -i`, bisync | must | `POST /api/jobs?dry_run=true` returns the plan without transferring |
| U9 | Live progress per job: bytes, total, percent, speed, ETA, lanes in use, files done/total, current files with their own progress. One overall bar first (progress2), details on expand. | rclone core/stats, rsync | must | WebSocket stream of job snapshots, at most a few per second |
| U10 | Per-job result list: every file with its outcome (sent, skipped and why, failed and why, verified). Kept with the job. | rsync `-i`, rclone core/transferred | must | `GET /api/jobs/{id}/items` |
| U11 | Per-job event log (connects, lane changes, retries, errors), escaped and with secrets redacted. The useful part of FileZilla's message log, without the raw protocol. | FileZilla | should | `GET /api/jobs/{id}/events`; also on the WebSocket |
| U12 | Global bandwidth limit, changeable live, and an optional per-job limit. | rclone, rsync, FileZilla | should | settings plus job field; needs an engine rate limiter (not in Phase 1) |
| U13 | Verify toggle per job, default from settings (plan.md §11 q1). | rsync `--checksum`, rclone | must | `verify` field on job creation |
| U14 | Minimum free space per server, shown on the server card with current free space. | Hopsync Phase 1 | must | server field `min_free`; `statvfs@` on probe |
| U15 | Directory comparison: mark entries present on one side only, or differing in size or mtime. | FileZilla | should | the client can compute it from two listings; no new endpoint |
| U16 | Exclude filters per job (glob), shown in the preview. | rsync, lsyncd, FileZilla | later | |
| U17 | Mirror/sync jobs: a first-run baseline (resync), deletions as an explicit named mode, a max-delete brake, an empty-source abort, a conflict policy, and a dry run required before the first real run. | bisync, lsyncd | later | |
| U18 | Notification or action when the queue drains. | FileZilla | later | |
| U19 | First start: a setup page asking for the admin password, unlocked by a one-time setup token printed to the log. No default password. Login rate-limited. | rclone GUI lesson, plan.md §7 | must | `POST /api/setup`, `POST /api/login`, `POST /api/logout` |
| U20 | Human-readable everything: sizes in IEC units, speeds, ETAs, relative times. Errors in plain words first, details on expand. | rsync `-h` | must | the API sends raw numbers; the UI formats them |
| U21 | Recent paths per server, so a repeat transfer is two clicks. | FileZilla recent sites | should | stored per server |

## 3. Decisions taken from this study

- **No modal prompts during a job.** Every question (existing files, trust, space) is answered
  before the job starts or fails the item with a clear reason. Hopsync runs headless; a job that
  waits for a click overnight is a failed job.
- **History is durable.** The queue, results and events are in SQLite and survive restarts. Old
  jobs are pruned by age or count (a setting), not on exit.
- **Preview and run share one code path.** A dry run is the planner without the transfer, as with
  rsync, so the preview cannot drift from what the run does.
- **Progress comes as snapshots, not raw events.** The engine publishes the current state of a job
  (as rclone's core/stats does) at a bounded rate. A slow browser drops intermediate snapshots
  instead of building up a backlog.
- **Mirror/sync is not in v1** (plan.md non-goals). Its brakes (U17) are recorded now so the
  later design starts from them.

## 4. Phase 2 engine gaps this study exposes

1. **Progress reporting.** `engine::run` returns a report at the end. U9 needs a live snapshot
   (bytes moved, current files, lanes) readable while it runs.
2. **Cancellation and pause.** Jobs must stop cleanly on request and resume later from the
   journal. Pause is cancel plus resume.
3. **Dry-run planning.** The tree walk and policy decisions without any transfer (U8).
4. **Per-item results** collected into a list the store can save (U10). `TreeReport` holds counts
   and problems today.
5. **Bandwidth limit** (U12): a shared token bucket across lanes. Should, not must.
