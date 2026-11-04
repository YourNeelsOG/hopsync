# Phase 2 plan: service, store, API

Status: started 2026-11-04 (owner: "Start it", steps run back to back); steps 1 to 10 done.
Security review: `docs/phase2/security-review.md`.
Design: `plan.md` §5, §7 and §8. UX requirements: `docs/research/ux-study.md` (U1 to U21).
Scope (plan.md §9, Phase 2): `hopsync serve`, SQLite store, secret vault, host-key pinning, job
scheduler, JSON API with live progress, Unix-socket CLI, systemd units (system and user).
Out of scope: the web UI itself (Phase 3), TLS, the audit log UI and fuzzing (Phase 4), mirror/sync
(later), bandwidth limits (U12, should; only if time allows).

## Decisions this plan depends on

All are in the plan.md Decision log (2026-11-04).

| # | Need | Choice |
|---|---|---|
| E1 | HTTP server | `axum` 0.8, `http1` only, no HTTP/2 |
| E2 | Live progress | Server-Sent Events; a `watch` channel per job, latest snapshot wins |
| E3 | Password hash | `argon2` 0.6, argon2id m = 19 MiB, t = 2, p = 1 |
| E4 | Secret vault | `aws-lc-rs` ChaCha20-Poly1305, key file `vault.key` (0600) outside the database, associated data binds each secret to its row |
| E5 | Sessions and CSRF | Hand-written: random token, SHA-256 stored, HttpOnly SameSite=Strict cookie, Origin check on every state change |
| E6 | Local CLI | Same router on a Unix socket in a 0700 directory, `SO_PEERCRED` uid check |
| E7 | Migrations | `PRAGMA user_version`, one transaction per step, newer database refused |

## Module layout

Still one crate. New modules:

| Module | Owns |
|---|---|
| `store` | `hopsync.db`: schema, migrations, typed queries for servers, host keys, jobs, items, events, settings, admin, sessions |
| `vault` | key file creation (0600, never overwritten), seal and open with bound associated data |
| `auth` | setup token, password hashing, sessions, login throttling, CSRF checks |
| `jobs` | scheduler: queue order, concurrency, run, pause, cancel, retry, restart recovery, progress snapshots |
| `api` | axum router: JSON handlers, SSE stream, auth middleware |
| `daemon` | `hopsync serve`: data directory, listeners (TCP and Unix socket), shutdown |
| `client` | CLI side of the Unix socket |

## Data directory

`--data-dir`, else `$XDG_DATA_HOME/hopsync`, else `~/.local/share/hopsync`, created 0700:
`hopsync.db` (store), `journal.sqlite` (resume journal), `vault.key`, `known_hosts` (the Phase 1
file, now managed by the daemon), `hopsync.lock` (one daemon per directory) and `setup-token`
(until setup is done). Generated keys live sealed in the store. Runtime:
`$XDG_RUNTIME_DIR/hopsync/hopsync.sock`, else `<data-dir>/run/hopsync.sock`.

## Edge cases (plan.md §8, plus the service ones)

- Restart mid-job: jobs that were running go back to queued and resume from the journal.
- Two jobs writing the same destination: refused when queued (same server and destination path).
- Server deleted while a job uses it: refused while jobs reference it.
- Host key changed: the job fails with the key-change error, and the server is marked blocked until
  the user re-trusts it. It is never accepted automatically.
- `vault.key` missing while the database holds secrets: the daemon refuses to start and says why. It
  never makes a new key, which would orphan every secret.
- Database written by a newer Hopsync: refused.
- Disk full writing the store: the request fails; running transfers are unaffected.
- Many browsers watching: snapshots are shared, never queued per client.
- A slow client on SSE: the latest snapshot replaces older ones.
- Login brute force: exponential delay per source address and a global cap.
- Setup token: printed once to stderr, valid until setup completes, compared in constant time.
- Clock changes: session expiry uses wall time; a jump only shortens or lengthens sessions.
- Unix socket left over from a crash: removed only if nothing is listening on it.
- Paths from the API: parsed into `RelPath` / `RemotePath` at the boundary, under the server's
  configured root.

## Steps

Each step leaves the tree green (fmt, clippy `-D warnings`, tests, `cargo deny`, `cargo audit`).
Production code starts with a failing test.

### 1. Engine: cancellation, dry run, per-item results
- **Change:** `TransferOptions.cancel` (a `watch` flag the workers and producer race against, so a
  cancel ends the transfer like an abort but keeps parts and journal). `TreeReport.items`: one entry
  per file with its outcome. A planning-only entry point that walks the tree and applies the policy
  with no data moved.
- **Test:** cancel mid-tree stops within a second, and a second run resumes. The dry run reports
  the same outcomes the real run then produces. Items list every file.

### 2. Store
- **Change:** `store` with migrations and typed accessors. The resume journal keeps its own file
  in the data directory (Decision log: its write rate should not contend with the API).
- **Test:** fresh database migrates; a reopen is a no-op; a newer `user_version` is refused; CRUD
  round trips.

### 3. Vault
- **Change:** `vault`: create or load the key, seal and open.
- **Test:** round trip; a ciphertext moved to another row fails to open; a wrong key fails; the key
  file is 0600 and never overwritten; a missing key with existing secrets is an error.

### 4. Auth
- **Change:** setup token, argon2id hashing, sessions in the store, throttling, CSRF check.
- **Test:** no login before setup; a wrong token cannot set up; the password hash is never in any
  response or log; sessions expire; cross-origin POSTs are refused; throttling delays repeats.

### 5. Servers and host keys
- **Change:** server CRUD; probe (connects, returns the offered key and fingerprint, trusts
  nothing); trust (pins the key); a changed key marks the server blocked. Engine auth from an
  in-memory key (vault) as well as a key file.
- **Test:** against the sshd fixture: probe, trust, list; an impostor key blocks the server.

### 6. Job scheduler
- **Change:** `jobs`: queue in the store, concurrency limit, run through the engine, snapshots,
  pause, resume, cancel, retry, restart recovery, items and events saved.
- **Test:** jobs run in order; a cancelled job is resumable; a restarted scheduler resumes running
  jobs; a duplicate destination is refused.

### 7. API and SSE
- **Change:** `api` router: setup, login, logout, servers, probe, trust, listings, jobs, queue,
  settings, `/api/events` (SSE). `hopsync serve` binds loopback by default and warns loudly on a
  non-loopback plain-HTTP bind.
- **Test:** HTTP-level tests: auth required on every route except setup and login; the full flow
  from setup to a finished job; SSE delivers snapshots.

### 8. Unix socket and CLI client
- **Change:** the router on a Unix socket with the peer uid check; `hopsync jobs`, `hopsync add`
  (queue a transfer), `hopsync servers`, `hopsync cancel`; the standalone `push`/`pull` stay.
- **Test:** the CLI drives a running daemon; a connection from another uid is refused (checked
  with the uid rule as a unit, since tests cannot switch users).

### 9. Key generation and install
- **Change:** generate an ed25519 key per server, sealed in the vault; install it into the remote
  `~/.ssh/authorized_keys` over SFTP with a one-time password that is never stored.
- **Test:** against the fixture with password auth enabled: after install, key auth works and the
  password is in no table or log.

### 10. systemd units, docs, security review
- **Change:** `contrib/systemd/hopsync.service` (system, hardened) and the user unit; README
  service section; `docs/phase2/security-review.md`.
- **Test:** `systemd-analyze verify` on the units; the full test suite twice.
