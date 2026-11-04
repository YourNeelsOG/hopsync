# Phase 3 plan: web UI

Status: started 2026-11-04 (owner: "Start Phase 3"); steps 1 to 8 done. Security review:
`docs/phase3/security-review.md`.
Requirements: `docs/research/ux-study.md` (U1 to U21). API: Phase 2 (`crates/hopsync/src/api.rs`).
Stack: plain JavaScript modules and CSS in `crates/hopsync/web/`, embedded in the binary (Decision
log 2026-11-04). Out of scope: TLS (Phase 4), mirror/sync (later), bandwidth limits (U12).

## Screens

| Screen | Covers | API |
|---|---|---|
| Setup | U19: token from the data directory, password twice | `POST /api/setup` |
| Login | U19 | `POST /api/login` |
| Transfers | U1 two panes (this box left, a server right), U7 policy per job, U8 preview, U13 verify, U15 compare, U21 recent paths; queue panel below with U5 tabs, U6 queue pause and reorder, U9 live progress | `local/list`, `servers/{id}/list`, `jobs`, `jobs/dry-run`, `events` (SSE) |
| Job detail | U10 per-file results, U11 events | `jobs/{id}`, `items`, `events` |
| Servers | U2 add, edit, remove; U3 fingerprint confirm and changed-key flow; U4 key install; U14 min free | `servers`, `probe`, `trust`, `install-key`, `public-key` |
| Settings | password change, default policy and verify, local min free, audit log | `password`, `settings`, `audit` |

## Rules for the UI code

- Every string from the API reaches the page through `textContent` or attribute setters, never
  `innerHTML` (server-chosen names are hostile input).
- No inline script, style attribute or event attribute: the CSP forbids them.
- No modal question while a job runs (UX study §3): every choice is made before queueing.
- Sizes in IEC units, speeds, ETAs and relative times are formatted in the UI; the API sends raw
  numbers (U20).
- Browser storage only for conveniences (last path per server, active tab), wrapped in try/catch.

## Steps

1. Serve embedded assets: `/`, `/app.js`, `/app.css`, `/icon.svg`, with content types, the
   existing security headers, and the loopback Host check. Test over HTTP.
2. Settings API: `GET` and `PUT /api/settings` for `default_existing`, `default_verify`,
   `local_min_free`, validated. Test.
3. Shell, API client, setup and login.
4. Servers screen with the trust and key-install flows.
5. Transfers screen: two panes, selection, compare, preview, queue.
6. Queue panel with live progress, job detail, settings screen.
7. End to end in the built-in browser against a real daemon and SSH server; fix what it finds.
8. Security review of the UI, docs, final gate.
