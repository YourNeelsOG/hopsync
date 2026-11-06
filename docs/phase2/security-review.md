# Phase 2 security review (2026-11-04)

Scope: everything Phase 2 added: `store`, `vault`, `auth`, `service`, `api`, `daemon`, `client`,
the `serve`, `jobs`, `queue`, `job` and `servers` commands, the engine's cancel, dry-run and
per-item hooks, password authentication in `ssh`, `sftp::chmod`, and the systemd units. Checked
against CLAUDE.md §3, plan.md §7 and the threat model "Hopsync holds SSH credentials for many
servers; the web UI is reachable by a browser that also visits hostile pages; any remote may be
hostile".
Reviewer: the implementing agent, so this review is not independent. A second reviewer should
check it before a public release, as for Phase 1.

## Method

1. Walk the CLAUDE.md §3 rules and plan.md §7 controls; cite the test that enforces each.
2. For every route, ask: who can call it (network, local socket, before setup), and what can a
   cross-site page, a rebinding page, a stolen database, or a hostile server make it do.
3. Every finding gets a failing test before the fix.
4. Run the daemon under the hardening options of the systemd units to check they do not break it.

## CLAUDE.md §3 and plan.md §7 checklist

| Rule | Status | Where it is enforced |
|---|---|---|
| No plaintext secrets stored | Holds | Generated SSH keys are sealed with ChaCha20-Poly1305 under `vault.key` (0600, outside the database), bound to their row and purpose (`vault` tests). The admin password is an argon2id hash. Sessions are stored as SHA-256 of the token. `a_generated_key_is_sealed_and_works_once_installed` scans the database file for a PEM header. |
| Never log secrets | Holds | The setup token goes to a 0600 file, not the log (finding P1). Install passwords live in `Zeroizing` buffers for one connection; `KeySource`'s `Debug` prints `<redacted>`. Internal errors are logged without request data; API errors carry no stack or SQL. |
| Host keys verified and pinned; a change is a hard failure | Holds | A server is unusable until `trust` with the fingerprint the user saw; the key is fetched again and must match. A changed key fails the job and blocks the server; replacing the pin needs `replace: true` and is audited (`a_changed_host_key_blocks_the_server_until_replaced`). |
| Paths confined to roots; no path reaches a shell | Holds | Every API path goes through `service::parse_rel` (no `..`, no control characters) and is joined under the local root (`openat2` `RESOLVE_BENEATH`) or the server root. `api.rs` traversal test returns 400. Key installation only writes files named `authorized_keys` or `authorized_keys2`. |
| No endpoint, option or plugin executes commands | Holds | No `std::process::Command` in the crate. The API has no command, script or hook route. |
| Web UI auth from first start; no default password | Holds | Every route but `health`, `setup` and `login` needs a session (`nothing_works_before_setup_and_everything_needs_a_session`). Setup needs the one-time token from the data directory. |
| argon2id, HttpOnly + SameSite=Strict cookies, CSRF, rate limiting | Holds | `auth` tests; cookie flags asserted in `cross_origin_and_form_posts_are_refused`. Login and setup are throttled per address after 3 failures, doubling up to 5 minutes; one argon2 hash runs at a time. |
| Strict CSP; UI assets never fetched | Holds for the API | `default-src 'self'; frame-ancestors 'none'` plus nosniff, no-referrer, DENY framing, `no-store` on every response. There is no UI bundle yet (Phase 3). |
| Loud warning on plain HTTP off loopback | Holds | `daemon::serve` prints it; checked by hand. |
| Supply chain | Holds | New crates: axum 0.8.9 (no HTTP/2), hyper client, serde, serde_json, futures-util, argon2 0.6.0; aws-lc-rs and zeroize were already in the tree. `cargo deny check`: advisories, bans, licenses and sources ok. `cargo audit`: no vulnerabilities in 261 crates. |
| Local privilege; systemd hardening | Holds | System unit: dedicated user, `ProtectSystem=strict`, `ProtectHome`, empty capability set, `@system-service` syscall filter, `MemoryDenyWriteExecute`. The daemon ran under the user-manager subset of these and served requests (Phase 2 step 10). |
| Audit log | Holds (no UI yet) | Setup, logins (and failures), password change, servers added, changed and removed, host keys trusted, replaced and changed, keys installed, restarts. Append-only through the API. |

## Findings, all fixed

### P1. The setup token was written to the log (medium)

The first draft printed the one-time setup token to stderr, which the system unit sends to the
journal. Anyone who can read the journal (group `systemd-journal`, `adm` on some distributions)
could finish setup and choose the admin password before the owner.

Fix: the token is written to `<data dir>/setup-token` (0600, replaced on each start before setup,
deleted when setup succeeds). The log names the file, never the token.

Test: `nothing_works_before_setup_and_everything_needs_a_session` checks the file's mode and
contents and that setup removes it.

### P2. A second daemon on the same data directory corrupted the first one's queue (high, data integrity)

Found by the manual smoke test. `hopsync serve` opened the store, which puts running jobs back in
the queue, before it noticed that the socket was taken. The first daemon then saw its own running
jobs as queued and could start them a second time: two transfers into the same part files.

Fix: `Service::open` takes an exclusive `flock` on `hopsync.lock` before touching anything, and
`daemon::run` binds nothing before that.

Test: `a_second_daemon_on_the_same_data_dir_is_refused` (failed before the fix: the second open
succeeded).

### P3. DNS rebinding reached the API (medium)

A page on `evil.example` whose DNS answer changes to 127.0.0.1 sends requests whose `Origin` and
`Host` both say `evil.example`, so the same-origin check passed. The session cookie is not sent
(it belongs to 127.0.0.1), but the page could still drive the login endpoint (password guessing,
slowed only by the throttle) and read `health`.

Fix: on a loopback listener every request's `Host` must be a loopback name (127.0.0.0/8, ::1 or
`localhost`); anything else gets 421.

Test: `a_rebound_hostname_is_refused_on_a_loopback_listener` and `api::tests::loopback_hosts`.

### P4. Unbounded listings in the daemon (low, availability)

The engine's per-directory cap (10 million names, security review F3 of Phase 1) is sized for the
CLI. In the daemon, a hostile server could make one browse request hold hundreds of megabytes and
build a reply of the same size.

Fix: browse listings stop at 100,000 entries and transfer listings at 1 million
(`service::BROWSE_MAX_ENTRIES`, `TRANSFER_MAX_ENTRIES`). Over the cap the request fails with
"listing too long". The cut-off itself is the engine's, covered by `an_endless_listing_is_cut_off`.

### P5. A too-long socket path failed with a cryptic error (low, usability)

`<data dir>/run/hopsync.sock` can exceed the 107-byte limit for Unix socket paths with a deep
data directory. Linux then reports "path must be shorter than SUN_LEN".

Fix: `daemon::check_socket_path` refuses such paths up front and says to pass `--socket`.

Test: `daemon::tests::a_too_long_socket_path_says_what_to_do`.

## Accepted risks (documented, not fixed)

- **Plain HTTP.** Until Phase 4 adds TLS, the UI is plain HTTP. It binds loopback by default and
  warns otherwise; `--secure-cookies` is for use behind a TLS proxy.
- **The local socket skips login.** Anyone running as the daemon's user can drive it. That user
  can already read `vault.key` and the database, so a password would add nothing.
- **Root on the box.** Root, or anyone who can read the data directory, gets every sealed secret:
  the vault key sits beside the database. The master-passphrase mode (plan.md §7) is still to do.
- **Session lifetime.** 12 hours idle, 7 days absolute, with no "log out everywhere" button except
  changing the password.
- **Throttle by address.** Many source addresses can each try 3 passwords freely before their
  delays start; with argon2id at about 50 ms per attempt and one at a time, that is at most about
  20 guesses a second in total. A strong password is still required (12 characters minimum).
- **Password installs untested end to end.** Closed on 2026-11-06: see the
  [logins and releases review](../phase4/security-review.md).
- **Job history grows.** Items and events stay until their job is deleted. Pruning by age is a
  Phase 3 setting (UX study §3).
- **No independent review yet.** See above.
