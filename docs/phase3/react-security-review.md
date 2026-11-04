# React web UI security review (2026-11-04)

Scope: the move of the web UI to React (`crates/hopsync/web/`), how the daemon now builds and serves
it (`crates/hopsync/build.rs`, `crates/hopsync/src/api.rs`), and the heartbeat added to the event
stream. It carries the [Phase 3 review](security-review.md) forward and uses the same threat model:
Hopsync holds SSH credentials for many servers, the browser that shows the UI also visits hostile
pages, and any remote may be hostile, including the names of the files it lists.

Reviewer: the implementing agent, so this review is not independent. A second reviewer should check
it before a public release.

## Method

1. Read the injection behaviour of each candidate UI package before adopting it. Packages that add
   `<style>` elements at runtime conflict with the CSP, so they were rejected (Decision log).
2. Run every flow in a real browser against a real daemon and a real local sshd
   (`web/e2e/e2e.py`). The daemon runs with an empty `PATH`, to prove that the UI comes from the
   binary alone. The run fails on any CSP violation.
3. Write a failing test first for each finding in the daemon.

## Checklist

| Rule | Status | Where it is enforced |
|---|---|---|
| Hostile names cannot inject markup or script | Holds | React renders API text as text. `ui_source_keeps_the_safety_rules` fails on `dangerouslySetInnerHTML`, `innerHTML` and similar in our source. The fixture's file `we<b>ird & "name".txt` is checked in the browser. |
| Strict CSP holds | Holds | The policy is still `default-src 'self'`, with no `unsafe-inline` or `unsafe-eval`. The browser run logs no violation. `shipped_assets_keep_the_safety_rules` checks the built page, CSS and scripts. |
| No code or assets fetched at runtime | Holds | `build.rs` compiles every asset into the binary, including the fonts. Source and page checks refuse external URLs and dynamic `import()`. `assetsInlineLimit: 0` keeps assets out of `data:` URLs. |
| Unexpected files cannot ship | Holds | `build.rs` panics on a file type it does not expect. |
| Cached assets hold no data | Holds | Only files under `/assets/` with a content hash in the name are cached (`public, max-age=31536000, immutable`). They are the same static build for every visitor. API responses and the page stay `no-store`. |
| Auth from first start | Holds | Unchanged. Assets are public and hold no data. Every data call is an authenticated API request. |
| Sessions end everywhere they are used | Holds after R1 | See below. |
| CSRF, Host and Origin checks | Holds | Unchanged. The UI uses `fetch` with JSON bodies from the same origin. The Vite dev proxy exists only in `pnpm dev`, never in the binary. |
| Secrets stay out of browser storage | Holds | `localStorage` holds preferences only (theme, folders, chosen server, compare toggle, queue tab). |
| Gzip does not change content for a browser that refuses it | Holds | `gzip_needs_the_browser_to_accept_it`. |

## Changes to existing checks

These change earlier safeguards, so CLAUDE.md §5 requires a written reason.

- **`crates/hopsync/tests/ui_syntax.rs` is deleted.** It ran `node --check` on `web/app.js`, and that
  file no longer exists. `tsc --noEmit` is stricter, and `pnpm build` runs it, so a syntax error
  cannot reach a built UI.
- **The byte rules moved from shipped files to source.** A React bundle holds React's own internals,
  including the word `innerHTML`, so the old check on shipped bytes cannot pass on any React build.
  The rule on `ui_source_keeps_the_safety_rules` now covers the code we write. What can still be
  checked on shipped bytes stays in `shipped_assets_keep_the_safety_rules`. The browser run covers
  the rest. This check is weaker for third-party code, which the supply chain rules below address.
- **The inline style rule changed.** The old rule banned every inline style. CSP blocks `style`
  attributes in markup and `setAttribute("style", ...)`, and both stay banned. Writes through the
  CSSOM (`element.style.x`), which React uses for its `style` prop and motion uses for animation, are
  allowed by CSP. The virtual rows and progress bars depend on them.
- **The "no npm packages" decision is reversed** by owner choice. See the supply chain section.

## Supply chain

The UI adds an npm tree of 205 packages at build time. None of them runs on a server, but their code
ships inside the binary and runs in the admin's browser, next to an authenticated session.

| Control | Setting |
|---|---|
| Exact versions and a committed lockfile | `save-exact=true`, `pnpm-lock.yaml`, `pnpm install --frozen-lockfile` |
| No install scripts | `ignore-scripts=true` in `.npmrc` |
| Delay against freshly compromised releases | `minimumReleaseAge: 20160` (14 days), for direct and indirect packages |
| Known vulnerabilities | `pnpm audit` |
| Few runtime packages | React, TanStack Query and Virtual, motion, lucide-react, Radix Tabs, two font packages |
| `dist/` is not committed | A built artifact in git could differ from its source without anyone noticing. |
| A release cannot silently lack the UI | `HOPSYNC_REQUIRE_UI=1` makes a missing UI a build error. |

## Findings, all fixed

### R1. An idle event stream outlived its session (low, information exposure)

**What:** Phase 3 made streams re-check the session before each snapshot. With no job activity no
snapshot is sent, so a revoked session's stream stayed open indefinitely. It delivered nothing
sensitive while idle, but would have held the connection, and the UI did not learn that the session
had ended.
**Found:** while adding the heartbeat.
**Fix:** the stream wakes every 5 seconds when idle, re-checks its session, and ends if the session
is gone. The check does not refresh the idle time.
**Test:** `an_idle_event_stream_ends_when_its_session_does` failed before the fix.

### R2. A stalled stream could not be detected by the UI (low, availability)

**What:** the earlier keep-alive sent SSE comments, which browsers do not expose to scripts. A silent
proxy or a dead connection looked the same as an idle queue, so speeds could freeze with no sign.
**Fix:** a named `heartbeat` event replaces the keep-alive. The UI reopens the stream after 15 seconds
of silence.
**Tests:** `an_idle_event_stream_sends_heartbeats_the_browser_can_see`, and the stream recovery step in
the browser run.

## Accepted risks

- **Third-party code in the bundle is not read line by line.** A malicious package that passed the
  14-day delay could run in the admin's session. The CSP limits where it can send data, because
  `connect-src` falls back to `'self'`. Fewer packages, the delay and the lockfile lower the risk but
  do not remove it.
- **There is no CI yet.** The audits and `HOPSYNC_REQUIRE_UI=1` run only when someone runs them. Phase
  4 must add them to CI and to the release build.
- **`localStorage` holds folder paths and the last server id.** This is unchanged from Phase 3.
- **The Phase 2 and Phase 3 accepted risks are unchanged.** That includes plain HTTP off loopback, no
  built-in TLS and a single admin account.
