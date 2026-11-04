# Phase 3 security review (2026-11-04)

Scope: the web UI (`crates/hopsync/web/`: `index.html`, `app.js`, `app.css`, `icon.svg`), how the
daemon serves it, the settings API, and the daemon changes the UI work exposed (event streams,
shutdown). Checked against CLAUDE.md §3, plan.md §7 and the Phase 2 threat model: Hopsync holds SSH
credentials for many servers; the browser that shows the UI also visits hostile pages; any remote
may be hostile, including the names of the files it lists.
Reviewer: the implementing agent, so this review is not independent. A second reviewer should check
it before a public release.

## Method

1. Read every place where data from the API reaches the DOM, and every place the UI builds a
   request.
2. Drive the whole UI in a real browser against a real daemon: a local OpenSSH server with a
   rotated host key, and a shared seedbox over a 200 ms link. Setup, login,
   wrong password, servers, trust, key install, changed key, uploads and downloads, compare,
   preview, queue pause, reorder, pause and resume of a running job, retry, settings, password
   change, logout, phone width. Every finding got a failing test where one can be written in Rust.
3. Check the browser console for CSP violations: none.

## Checklist

| Rule | Status | Where it is enforced |
|---|---|---|
| Hostile names cannot inject markup or script | Holds | All API text goes through `textContent` or attribute setters (`h()` in `app.js`). A file named `we<b>ird & "name".txt` showed as text in the listing, the preview and the job page. `ui_assets_keep_the_safety_rules` fails the build on `innerHTML`, `outerHTML`, `insertAdjacentHTML`, `document.write`, `eval`, `new Function`, inline styles and inline handlers. |
| Strict CSP holds for the UI | Holds | `default-src 'self'` with no `unsafe-inline`: the page has no inline script, style attribute or handler (same test). No CSP violation was logged in the browser run. |
| No code or assets fetched at runtime | Holds | The four assets are compiled into the binary (`include_bytes!`). No external URL, `@import`, `url()` or dynamic `import()` (same test). No npm packages, so no lockfile to audit (Decision log). |
| Auth from first start; no default password | Holds | The setup page needs the one-time token from the data directory. Assets are public and hold no data; every data call is an authenticated API request (Phase 2 tests). |
| Sessions end everywhere they are used | Holds after W2 | Logout and password change end sessions; open event streams now stop too (`an_event_stream_ends_when_its_session_does`). The UI then shows the login page. |
| CSRF | Holds | Every state change is a `fetch` with a JSON body from the same origin; the Phase 2 Origin, `Sec-Fetch-Site`, content-type and Host checks apply unchanged. The UI uses no forms that post natively. |
| Host keys are confirmed by a person | Holds | A new server opens the fingerprint dialog; trust needs a click on "They match". A changed key shows both fingerprints, and "Replace" stays disabled until a confirmation box is ticked (driven in the browser after rotating the fixture's key). |
| Passwords are not kept | Holds | The key-install password goes in one request and the field is cleared when the dialog closes. Password fields use `autocomplete` hints so password managers work. Nothing secret goes to `localStorage`, which holds only folders, the chosen server, the compare toggle and the queue tab. |
| Paths stay under their roots | Holds | The UI only joins names the API listed; the daemon parses every path at the boundary (Phase 2). |
| Clickjacking | Holds | `X-Frame-Options: DENY` and `frame-ancestors 'none'`. |

## Findings, all fixed

### W1. SIGTERM did not stop the daemon while a browser was open (medium, availability)

**What:** graceful shutdown waits for open responses. An open UI tab holds `/api/events` open
for as long as the tab lives, so `systemctl stop` hung until systemd's kill timeout, and a
package upgrade or reboot stalled.
**Found:** stopping the test daemon during the browser run; it was still running after 5 seconds.
**Fix:** the service has a stopping flag; event streams end on it, and `serve` waits at most 5
seconds for any other open request (Decision log).
**Test:** `shutdown_does_not_wait_for_open_event_streams`. Checked again by hand: with the browser
connected, SIGTERM stops the daemon in about 0.5 seconds with exit code 0.

### W2. An event stream outlived its session (low, information exposure)

**What:** the session check ran once, when the stream opened. After logout, or after a password
change that ends every session, an open stream kept sending job progress (job ids, bytes, rates).
**Found:** while testing password change in the browser.
**Fix:** before each snapshot the stream re-checks its session, and ends if the session is gone.
The check does not refresh the session's idle time, so a forgotten open tab cannot keep a session
alive. The UI notices the refused reconnect and shows the login page.
**Test:** `an_event_stream_ends_when_its_session_does` (failed before the fix with a snapshot
delivered after logout).

### W3. The changed-key dialog did not show the pinned fingerprint (medium, security UX)

**What:** the dialog printed `[object HTMLElement]` instead of the pinned fingerprint, so the user
could not compare the old key with the new one, which is the decision the dialog exists for.
**Fix:** the list of pinned keys is spread into the DOM call. A scan of every other `append` call
found no second case.
**Test:** checked in the browser run; there is no JavaScript unit harness (see accepted risks).

## Other bugs the browser run found, all fixed

- With no server, the right pane said "Empty folder" instead of pointing to Servers.
- A folder or server switch kept showing the old listing while the new one loaded, and a slow
  answer for a folder already left could replace the newer one (now dropped by sequence number).
- The destination pane did not refresh when a job finished; the selection stayed after queueing.
- Clicking a file's row did nothing; now it selects, as in FileZilla.
- A failed login said "not authorized" instead of "Wrong password."; after login the UI jumped to
  Transfers instead of the page the user was on.
- Jobs queued while the queue is paused were shown under Active.
- Queue rows did not line up; the servers table pushed its buttons off screen with long paths;
  arrows pointed sideways on phones, where the panes stack.
- `node --check` was named in the Decision log but not run: `tests/ui_syntax.rs` now runs it when
  node is installed.

## Accepted risks (documented, not fixed)

- **No JavaScript unit tests.** UI behaviour is covered by the browser run recorded here, the
  asset rule test and the syntax check, not by automated DOM tests. Adding a headless browser
  harness means npm packages, which the stack decision avoids. Revisit if the UI grows.
- **`localStorage` holds folder paths and the last server id.** Paths can be mildly sensitive
  (they may name content). They stay in the browser profile of the person who uses the UI, and the
  page works when storage is blocked.
- **A request still running at shutdown is cut after 5 seconds.** A key install could be
  interrupted between the append and the test login; repeating it is safe, because the install
  compares key blobs and adds nothing twice.
- **The data directory's own mode is not tightened.** The daemon creates a missing data directory
  as 0700, but leaves an existing directory's mode alone. Every file in it is 0600 except the empty
  lock file. Phase 4 packaging should create the directory itself.
- Phase 2's accepted risks (plain HTTP off loopback, no TLS until Phase 4, single admin account)
  are unchanged.
