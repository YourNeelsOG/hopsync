# React web UI plan (2026-11-04)

Replaces the plain JavaScript UI of Phase 3 with React. The API, the behaviour and the security
rules stay. Decisions and reasons are in `plan.md` (Decision log, 2026-11-04).

## Keep from Phase 3

- Every flow: setup, login, two panes, compare, drag between panes, dry-run preview, queue with
  pause, resume, cancel, retry and reorder, job page, servers, host-key trust, key install,
  settings, password change, audit log, logout.
- API text is shown only as text. No raw HTML. No inline script or style element.
- The CSP stays `default-src 'self'`. Nothing loads from another site.
- No question appears while a job runs. Every choice is made before the job is queued.

## New

- Live speed: a total speed readout, a graph of the last two minutes, and the browser tab title.
- Numbers animate between updates, and progress bars move on the compositor.
- A progress update changes only that job's numbers. Buttons keep their DOM nodes, so a click is
  never lost.
- File lists render only visible rows. Compare uses a name lookup, not a search per row.
- A stalled event stream is detected and reopened.
- Destructive actions use a confirm dialog in the app.
- Dark and light themes.

## Design rules

UI Skills `baseline-ui`, `fixing-accessibility` and `fixing-motion-performance`, in short:
`tabular-nums` for data, `h-dvh`, no gradients, one accent per view, animate only `transform` and
`opacity`, at most 200 ms for interaction feedback, `prefers-reduced-motion` respected, `aria-label`
on icon-only buttons, errors next to the action, empty states with one next action. Motion
Primitives supplied the animated number idea; none of its code is used.

## Edge cases

| Case | Handling |
|---|---|
| `dist/` missing at build time | The binary serves a page saying how to build it. `HOPSYNC_REQUIRE_UI=1` fails the build. |
| Session ends during use | The next 401 shows the login page and keeps what was typed there. |
| Slow listing for a folder the user has left | The newer request wins. |
| Remembered folder deleted | A 404 falls back to the root once. |
| Remembered server removed | The first usable server is selected. |
| Hostile file names | Shown as text. The test fixture has one. |
| Listing of 100,000 entries | Virtual rows, and compare runs in linear time. |
| Tab hidden | Animation stops, and the speed graph keeps sampling from the stream. |
| Stream silent for 15 s | The stream is closed and reopened. |
| Reduced motion requested | Numbers jump, and bars do not animate. |
| Storage blocked | Theme, folder and tab choices are lost and nothing breaks. |
| Very long names | Rows truncate with the full name in the tooltip. Dialogs wrap. |
| Phone width | Panes stack, the nav moves to the bottom, and nothing scrolls sideways. |

## Steps

1. Daemon: generated asset table, `/assets/` caching, gzip, SSE heartbeat, tests first.
2. Frontend project: pinned packages, lockfile, install scripts off, Tailwind tokens.
3. Logic with unit tests first: format, compare, prefs, live store, API errors.
4. Screens: shell, auth, transfers, queue, job, servers, settings.
5. Browser test against a real daemon and sshd (`web/e2e/e2e.py`): every flow, hostile names, no
   CSP violations, before and after screenshots at desktop and phone widths.
6. Security review: `docs/phase3/react-security-review.md`.
7. README and charts.
