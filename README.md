<p align="center"><img src="docs/assets/logo.svg" alt="" width="96" height="96"></p>

# Hopsync

Fast, careful file transfer between SSH hosts: seedbox to seedbox, or any SSH box to any SSH box.
Hopsync speaks plain SFTP, so the server needs only a standard OpenSSH account.

**Status: Phases 0 to 3 done: transfer engine, daemon with job queue and JSON API, and the web
UI.** Packaging and TLS are Phase 4. See [plan.md](plan.md). Nothing is released yet.

## What it does

- **Parallel lanes.** One transfer opens several SSH connections and splits big files into chunks
  across them. That gets past the per-connection window limit that holds `scp` and `sftp` to a few
  MB/s on long links. Lanes start at 4 and are added while each one still raises throughput.
- **Resume.** Interrupted files continue from their last durable chunk, after a crash or `kill -9`
  too.
- **Safe by default.** Host keys are pinned, and a changed key is a hard failure. Existing files are
  skipped unless you choose a policy. Data goes to `name.hopsync-part` and is renamed into place
  only when complete.
- **Light on the box.** Memory is bounded. The page cache is dropped after streaming, so a
  seedbox's cache stays free for its own work. There are no temporary copies.
- **Optional verify.** `--verify` reads every chunk back and compares xxh3-128 hashes before the
  rename.

## Build

Requires Rust (the version is pinned in `rust-toolchain.toml`) on Linux 5.6 or later.

```bash
cargo build --release
```

## Use

Pin the server's host key once. Compare the fingerprint with the one your provider shows, or pass
it with `--fingerprint SHA256:...`:

```bash
target/release/hopsync hosts trust seedbox.example.com
```

Copy a file or a directory tree:

```bash
target/release/hopsync push -i ~/.ssh/id_ed25519 ./Movies user@seedbox.example.com:downloads/
```

```bash
target/release/hopsync pull user@seedbox.example.com:downloads/Show ./
```

Useful options (`hopsync push --help` lists them all):

| Option | Effect |
|---|---|
| `--existing skip\|update\|overwrite\|rename` | What to do with files already at the destination. `update` replaces files whose size or mtime differ, but keeps ones that are newer at the destination. |
| `--verify` | Read everything back and compare it before the rename. |
| `-l N` / `--max-lanes N` | Fix the lane count, or cap the automatic one (default 4 rising to 16). |
| `--min-free 10G` | Refuse files that would leave less free space than this at the destination. |
| `--cache keep` | Leave transferred data in the page cache (the default drops it). |
| `--no-resume` | Do not record or use the resume journal. |

## Run it as a service

`hopsync serve` runs the daemon: a job queue that survives restarts, a JSON API with live
progress (Server-Sent Events), and a socket for the commands below. It listens on
`127.0.0.1:7890` and keeps its data in `~/.local/share/hopsync`.

```bash
target/release/hopsync serve --root ~/downloads
```

Open `http://127.0.0.1:7890/` in a browser. On first start there is no password: the log names a
file holding a one-time setup token, and the page asks for it once to set the admin password. The
token is never written to the log. To reach a daemon on a remote box, forward the port over SSH
(`ssh -L 7890:127.0.0.1:7890 box`) rather than listening on a public address.

The web UI has two panes, this box and a server: browse both, compare them, select files and
folders, and copy them either way. Before anything is queued it shows what would happen
(new, replaced, skipped) for the policy you pick. Below the panes, the queue shows live progress,
speed and time left; jobs can be paused, resumed, cancelled, retried and reordered, and each has a
page listing every file and why it was skipped or failed.

The same binary drives a running daemon over its socket, with no login (the socket only accepts
your own user):

```bash
target/release/hopsync servers
```

```bash
target/release/hopsync queue push seedbox Show/Season.1 Show/Season.1 --dry-run
```

```bash
target/release/hopsync jobs
```

`hopsync job pause|resume|cancel|show|rm <id>` controls one job. A paused, failed or restarted job
resumes from its journal. Units for systemd (a hardened system unit and a user unit for shared
seedboxes) are in [contrib/systemd](contrib/systemd).

Servers are added in the web UI (or the API). A server's host key must be confirmed by fingerprint
before first use, and a changed key blocks the server until you review it. Hopsync can generate a
per-server ed25519 key (sealed in its vault) and install it with the account password once, so no
password is stored.

## Numbers

Release build. Medians of 3 runs on loopback with tmpfs on both sides: 512 MiB file, 2,000 files
of 0.5 to 4.5 KiB. `benches/bench.py` reproduces them; details are in
[docs/phase1/benchmarks.md](docs/phase1/benchmarks.md).

| Scenario | hopsync | scp | sftp |
|---|---|---|---|
| Push one 512 MiB file | **734 MB/s** | 526 MB/s | 527 MB/s |
| Pull one 512 MiB file | **1057 MB/s** | 573 MB/s | 568 MB/s |
| Push 2,000 small files | **0.30 s** | 0.65 s | 0.52 s |
| Pull 2,000 small files | **0.28 s** | 0.67 s | 0.66 s |
| Peak client memory (big file) | 40 MiB | 12 MiB | 13 MiB |

Over a real 200 ms link to a shared seedbox (256 MiB file, 300 small files, 2 runs):

| Scenario | hopsync | scp | sftp |
|---|---|---|---|
| Push one 256 MiB file | **11.7 MB/s** | 5.9 MB/s | 6.1 MB/s |
| Pull one 256 MiB file | **15.7 MB/s** | 7.9 MB/s | 7.5 MB/s |
| Push 300 small files | **39 s** | 249 s | 188 s |
| Pull 300 small files | **31 s** | 250 s | 252 s |

There the gap comes from parallel lanes and from keeping many small files in flight against the
round-trip time.

## Security

Hopsync handles credentials for every server it reaches, so security is a design requirement (see
[CLAUDE.md](CLAUDE.md) §3). Host keys are pinned, local paths cannot leave their root (`openat2`
with `RESOLVE_BENEATH`), nothing is ever run through a shell, and text a server sends is escaped
before it reaches your terminal. The daemon requires a login from first start (no default
password), seals stored keys with a key kept outside its database, refuses cross-origin and
DNS-rebinding requests, and runs no commands. The web UI loads nothing from other sites, runs
no inline script, and shows server-supplied names only as text. Reviews:
[Phase 1](docs/phase1/security-review.md), [Phase 2](docs/phase2/security-review.md),
[Phase 3](docs/phase3/security-review.md).

## License

MIT OR Apache-2.0, at your option.
