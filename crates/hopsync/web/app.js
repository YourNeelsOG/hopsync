// Hopsync web UI (Phase 3). Plain ES module, no dependencies, served from the binary.
//
// Rules (docs/plans/2026-11-04-phase3-web-ui-plan.md):
// - Text from the API reaches the page only through textContent and attribute setters, never
//   innerHTML: file names and error messages come from servers we do not trust.
// - No inline script or style attributes (the CSP forbids them); styles change through classes
//   and the CSSOM.
// - Every transfer choice (policy, verify) is made in the confirm dialog, with a dry-run preview,
//   before anything is queued. Nothing asks a question while a job runs.

// DOM helpers.

/** Creates an element. `attrs`: class, text, on<event> handlers, anything else as an attribute. */
function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") el.className = v;
    else if (k === "text") el.textContent = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "value") el.value = v;
    else if (k === "checked") el.checked = true;
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

function clear(el) {
  while (el.firstChild) el.firstChild.remove();
  return el;
}

function toast(msg, kind = "error") {
  const t = h("div", { class: `toast ${kind}`, role: kind === "error" ? "alert" : "status", text: msg });
  document.getElementById("toasts").append(t);
  setTimeout(() => t.remove(), kind === "error" ? 8000 : 3500);
}

// Formatting (U20): the API sends raw numbers.

function bytes(n) {
  if (n === null || n === undefined) return "";
  const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  let i = 0;
  let v = Number(n);
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${v} B` : `${v.toFixed(v < 10 ? 2 : v < 100 ? 1 : 0)} ${units[i]}`;
}

function rate(n) {
  return n ? `${bytes(n)}/s` : "";
}

function duration(secs) {
  if (!Number.isFinite(secs) || secs < 0) return "";
  secs = Math.round(secs);
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
  return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
}

function when(epoch) {
  if (!epoch) return "";
  const d = new Date(epoch * 1000);
  const ago = (Date.now() - d.getTime()) / 1000;
  if (ago >= 0 && ago < 60) return "just now";
  if (ago >= 0 && ago < 3600) return `${Math.floor(ago / 60)} min ago`;
  if (ago >= 0 && ago < 86400) return `${Math.floor(ago / 3600)} h ago`;
  return d.toLocaleString();
}

/** "10G", "500 MiB", "0" to bytes; null if unreadable. */
function parseSize(s) {
  const m = /^\s*(\d+(?:\.\d+)?)\s*([kmgtp]?)(?:i?b)?\s*$/i.exec(s);
  if (!m) return null;
  const pow = { "": 0, k: 1, m: 2, g: 3, t: 4, p: 5 }[m[2].toLowerCase()];
  return Math.round(Number(m[1]) * 1024 ** pow);
}

function joinPath(dir, name) {
  return dir ? `${dir}/${name}` : name;
}

function parentPath(dir) {
  const i = dir.lastIndexOf("/");
  return i < 0 ? "" : dir.slice(0, i);
}

// Per-viewer conveniences only (last folder per pane, active tab). Never anything that matters.
const prefs = {
  get(k, fallback) {
    try {
      const v = localStorage.getItem(`hopsync.${k}`);
      return v === null ? fallback : JSON.parse(v);
    } catch {
      return fallback;
    }
  },
  set(k, v) {
    try {
      localStorage.setItem(`hopsync.${k}`, JSON.stringify(v));
    } catch {
      // Storage blocked or full: the UI works without it.
    }
  },
};

// API client.

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function api(method, path, body) {
  const init = { method, credentials: "same-origin", headers: {} };
  if (body !== undefined) {
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  let resp;
  try {
    resp = await fetch(path, init);
  } catch {
    throw new ApiError(0, "Hopsync is not reachable. Is the daemon running?");
  }
  const text = await resp.text();
  let data = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = null;
    }
  }
  if (!resp.ok) {
    const msg = (data && data.error) || `${resp.status} ${resp.statusText}`;
    if (resp.status === 401 && !path.startsWith("/api/login")) {
      showLogin();
    }
    if (resp.status === 403 && msg === "setup is required") {
      showSetup();
    }
    throw new ApiError(resp.status, msg);
  }
  return data;
}

/** Runs an action and reports its failure as a toast; returns its result or undefined. */
async function attempt(fn, okMessage) {
  try {
    const r = await fn();
    if (okMessage) toast(okMessage, "ok");
    return r;
  } catch (e) {
    toast(e.message || String(e));
    return undefined;
  }
}

// App state.

const state = {
  servers: [],
  settings: { default_existing: "skip", default_verify: false, local_min_free: 0 },
  jobs: [],
  snapshot: { changes: -1, queue_paused: false, running: [] },
  events: null,
  queueTab: prefs.get("queueTab", "active"),
};

const POLICIES = [
  ["skip", "Skip existing files", "Leave files that already exist at the destination alone."],
  ["update", "Update changed files", "Replace files whose size or date differ, but keep ones that are newer at the destination."],
  ["overwrite", "Overwrite", "Replace every existing file."],
  ["rename", "Keep both", "Write the new file beside the old one as \"name (1).ext\"."],
];

function serverName(id) {
  const s = state.servers.find((x) => x.id === id);
  return s ? s.name : `server ${id}`;
}

// Boot and routing.

const app = () => document.getElementById("app");

async function boot() {
  let health;
  try {
    health = await api("GET", "/api/health");
  } catch (e) {
    clear(app()).append(h("div", { class: "card narrow" }, h("h1", { text: "Hopsync" }), h("p", { class: "error", text: e.message })));
    return;
  }
  if (health.setup_required) {
    showSetup();
    return;
  }
  try {
    state.settings = await api("GET", "/api/settings");
  } catch {
    return; // api() has shown the login page.
  }
  await loadServers();
  startEvents();
  route();
}

window.addEventListener("hashchange", () => {
  if (state.events) route();
});

function route() {
  const hash = location.hash || "#/transfers";
  const [, view, arg] = hash.split("/");
  const views = { transfers: viewTransfers, servers: viewServers, settings: viewSettings, job: viewJob };
  const fn = views[view] || viewTransfers;
  shell(view in views ? view : "transfers");
  fn(arg);
}

function stopEvents() {
  if (state.events) state.events.close();
  state.events = null;
}

function startEvents() {
  stopEvents();
  const es = new EventSource("/api/events");
  es.addEventListener("snapshot", (ev) => {
    let snap;
    try {
      snap = JSON.parse(ev.data);
    } catch {
      return;
    }
    const changed = snap.changes !== state.snapshot.changes;
    state.snapshot = snap;
    if (changed) refreshJobs();
    else renderQueue();
    renderQueueToggle();
  });
  es.addEventListener("error", () => {
    // EventSource retries a dropped connection by itself, but gives up on a refused one (the
    // session ended): ask the API, which shows the login page on a 401.
    if (es.readyState === EventSource.CLOSED && state.events === es) {
      state.events = null;
      api("GET", "/api/settings").then(startEvents, () => {});
    }
  });
  state.events = es;
}

// Setup and login (U19).

function authPage(kind, title, intro, fields, submitLabel, onSubmit) {
  stopEvents();
  // Several requests can fail with 401 at once; keep the form (and what was typed) on screen.
  if (app().querySelector(`form[data-auth="${kind}"]`)) return;
  const err = h("p", { class: "error", role: "alert" });
  const form = h(
    "form",
    {
      class: "card narrow",
      "data-auth": kind,
      onsubmit: async (ev) => {
        ev.preventDefault();
        err.textContent = "";
        const button = form.querySelector("button");
        button.disabled = true;
        try {
          await onSubmit(new FormData(form));
          if (!location.hash) location.hash = "#/transfers";
          await boot();
        } catch (e) {
          err.textContent = e.message;
        } finally {
          button.disabled = false;
        }
      },
    },
    h("div", { class: "brand big" }, h("img", { src: "/icon.svg", alt: "" }), "Hopsync"),
    h("h1", { text: title }),
    intro,
    fields,
    err,
    h("button", { class: "primary", type: "submit", text: submitLabel }),
  );
  clear(app()).append(form);
  form.querySelector("input")?.focus();
}

function field(label, input, hint) {
  return h("label", { class: "field" }, h("span", { text: label }), input, hint ? h("small", { text: hint }) : null);
}

function showSetup() {
  authPage(
    "setup",
    "Set the admin password",
    h(
      "p",
      {},
      "This is the first start. The daemon wrote a one-time setup token to the file ",
      h("code", { text: "setup-token" }),
      " in its data directory (its log names the full path). Paste it here and choose a password.",
    ),
    [
      field("Setup token", h("input", { name: "token", required: true, autocomplete: "off", spellcheck: "false" })),
      field("Password", h("input", { name: "password", type: "password", required: true, minlength: 12, autocomplete: "new-password" }), "At least 12 characters."),
      field("Repeat password", h("input", { name: "again", type: "password", required: true, autocomplete: "new-password" })),
    ],
    "Set password",
    async (f) => {
      if (f.get("password") !== f.get("again")) throw new Error("The passwords differ.");
      await api("POST", "/api/setup", { token: String(f.get("token")).trim(), password: f.get("password") });
    },
  );
}

function showLogin() {
  authPage(
    "login",
    "Log in",
    null,
    [field("Password", h("input", { name: "password", type: "password", required: true, autocomplete: "current-password" }))],
    "Log in",
    async (f) => {
      try {
        await api("POST", "/api/login", { password: f.get("password") });
      } catch (e) {
        if (e.status === 401) throw new Error("Wrong password.");
        throw e;
      }
    },
  );
}

// Shell.

function shell(active) {
  const nav = [
    ["transfers", "Transfers"],
    ["servers", "Servers"],
    ["settings", "Settings"],
  ];
  const header = h(
    "header",
    { class: "top" },
    h("a", { class: "brand", href: "#/transfers" }, h("img", { src: "/icon.svg", alt: "" }), "Hopsync"),
    h(
      "nav",
      {},
      nav.map(([id, label]) => h("a", { href: `#/${id}`, class: active === id || (active === "job" && id === "transfers") ? "active" : "", text: label })),
    ),
    h("span", { class: "spacer" }),
    h("button", { id: "queue-toggle", class: "ghost" }),
    h("button", {
      class: "ghost",
      text: "Log out",
      onclick: async () => {
        await attempt(() => api("POST", "/api/logout", {}));
        showLogin();
      },
    }),
  );
  clear(app()).append(header, h("main", { id: "main" }));
  renderQueueToggle();
}

function renderQueueToggle() {
  const b = document.getElementById("queue-toggle");
  if (!b) return;
  const paused = state.snapshot.queue_paused;
  b.textContent = paused ? "Queue paused: resume" : "Queue running: pause";
  b.className = paused ? "warn" : "ghost";
  b.title = paused ? "No new jobs start until you resume. Running jobs finish." : "Stop starting new jobs. Running jobs finish.";
  b.onclick = () => attempt(() => api("POST", paused ? "/api/queue/resume" : "/api/queue/pause", {}));
}

const main = () => document.getElementById("main");

async function loadServers() {
  const list = await attempt(() => api("GET", "/api/servers"));
  if (list) state.servers = list;
}

// Transfers: two panes (U1), compare (U15), recent paths (U21), and the queue below.

const panes = {
  left: { kind: "local", server: null, path: prefs.get("path.local", ""), entries: [], selected: new Set(), error: "" },
  right: { kind: "remote", server: null, path: "", entries: [], selected: new Set(), error: "" },
};
let compare = prefs.get("compare", false);

function viewTransfers() {
  const usable = state.servers.filter((s) => !s.blocked);
  if (panes.right.server === null || !state.servers.some((s) => s.id === panes.right.server)) {
    const last = prefs.get("server", null);
    panes.right.server = (usable.find((s) => s.id === last) || usable[0] || state.servers[0] || {}).id ?? null;
    panes.right.path = panes.right.server === null ? "" : prefs.get(`path.${panes.right.server}`, "");
  }
  const m = clear(main());
  m.append(
    h(
      "section",
      { class: "panes" },
      paneBox("left"),
      h(
        "div",
        { class: "pane-actions" },
        h("button", { class: "primary", id: "push-btn", title: "Copy the selection on the left to the folder on the right", onclick: () => startTransfer("push") }, "Upload ", h("span", { class: "wide-only", text: "→" }), h("span", { class: "narrow-only", text: "↓" })),
        h("button", { class: "primary", id: "pull-btn", title: "Copy the selection on the right to the folder on the left", onclick: () => startTransfer("pull") }, h("span", { class: "wide-only", text: "← " }), h("span", { class: "narrow-only", text: "↑ " }), "Download"),
        h(
          "label",
          { class: "check", title: "Mark files that exist on only one side, or differ in size or date" },
          h("input", {
            type: "checkbox",
            checked: compare,
            onchange: (e) => {
              compare = e.target.checked;
              prefs.set("compare", compare);
              renderPane("left");
              renderPane("right");
            },
          }),
          "Compare",
        ),
      ),
      paneBox("right"),
    ),
    h("section", { id: "queue", class: "queue" }),
  );
  loadPane("left");
  loadPane("right");
  renderQueue();
  refreshJobs();
}

function paneBox(side) {
  return h("div", { class: "pane", id: `pane-${side}` });
}

async function loadPane(side) {
  const p = panes[side];
  p.selected.clear();
  p.error = "";
  if (p.kind === "remote" && p.server === null) {
    p.entries = [];
    p.error = "No servers yet. Add one under Servers.";
    renderPane(side);
    return;
  }
  // Each load gets a number; a slow answer for a folder the user already left is dropped.
  const mine = (p.seq = (p.seq || 0) + 1);
  p.entries = [];
  p.loading = true;
  renderPane(side);
  const url =
    p.kind === "local"
      ? `/api/local/list?path=${encodeURIComponent(p.path)}`
      : `/api/servers/${p.server}/list?path=${encodeURIComponent(p.path)}`;
  try {
    const entries = await api("GET", url);
    if (mine !== p.seq) return;
    p.entries = entries;
    prefs.set(p.kind === "local" ? "path.local" : `path.${p.server}`, p.path);
  } catch (e) {
    if (mine !== p.seq) return;
    p.entries = [];
    p.error = e.message;
    if (e.status === 404 && p.path) {
      // The remembered folder is gone: fall back to the root once.
      p.path = "";
      p.loading = false;
      return loadPane(side);
    }
    if (p.kind === "remote") await loadServers();
  }
  p.loading = false;
  renderPane(side);
}

function compareMark(side, entry) {
  if (!compare) return "";
  const other = panes[side === "left" ? "right" : "left"];
  if (other.loading || other.error) return "";
  const twin = other.entries.find((e) => e.name === entry.name);
  if (!twin) return "only";
  if (entry.kind !== twin.kind) return "differs";
  if (entry.kind === "file" && (entry.size !== twin.size || (entry.mtime && twin.mtime && entry.mtime !== twin.mtime))) return "differs";
  return "same";
}

function renderPane(side) {
  const p = panes[side];
  const box = document.getElementById(`pane-${side}`);
  if (!box) return;
  clear(box);
  let title;
  if (p.kind === "local") {
    title = h("div", { class: "pane-title" }, h("strong", { text: "This box" }));
  } else {
    const select = h(
      "select",
      {
        "aria-label": "Server",
        onchange: (e) => {
          p.server = Number(e.target.value);
          prefs.set("server", p.server);
          p.path = prefs.get(`path.${p.server}`, "");
          loadPane(side);
        },
      },
      state.servers.map((s) =>
        h("option", { value: s.id, selected: s.id === p.server ? "selected" : null, text: s.blocked ? `${s.name} (blocked)` : s.name }),
      ),
    );
    title = h("div", { class: "pane-title" }, state.servers.length ? select : h("strong", { text: "No server" }));
  }
  const crumbs = h("div", { class: "crumbs" });
  const parts = p.path ? p.path.split("/") : [];
  const go = (path) => () => {
    p.path = path;
    loadPane(side);
  };
  if (p.kind === "local" || p.server !== null) {
    crumbs.append(h("button", { class: "link", text: p.kind === "local" ? "root" : "server root", onclick: go("") }));
  }
  parts.forEach((part, i) => {
    crumbs.append(h("span", { class: "sep", text: "/" }), h("button", { class: "link", text: part, onclick: go(parts.slice(0, i + 1).join("/")) }));
  });
  const tools = h(
    "div",
    { class: "pane-tools" },
    h("button", { class: "ghost small", text: "Up", disabled: !p.path, onclick: go(parentPath(p.path)) }),
    h("button", { class: "ghost small", text: "Refresh", onclick: () => loadPane(side) }),
    h("span", { class: "muted", text: p.selected.size ? `${p.selected.size} selected` : "" }),
  );
  const body = h("tbody");
  const table = h(
    "table",
    { class: "files" },
    h("thead", {}, h("tr", {}, h("th", { class: "sel" }), h("th", { text: "Name" }), h("th", { class: "num", text: "Size" }), h("th", { text: "Modified" }))),
    body,
  );
  const sorted = [...p.entries].sort((a, b) => (a.kind === "dir") === (b.kind === "dir") ? a.name.localeCompare(b.name) : a.kind === "dir" ? -1 : 1);
  for (const e of sorted) {
    const mark = compareMark(side, e);
    const row = h("tr", {
      class: [e.valid ? "" : "invalid", p.selected.has(e.name) ? "selected" : "", mark ? `cmp-${mark}` : ""].join(" "),
      title: e.valid ? (mark ? { only: "Only on this side", differs: "Differs from the other side", same: "Same on both sides" }[mark] : "") : e.kind === "symlink" ? "Symlinks are skipped, never followed" : "Hopsync cannot transfer this entry (special file or unsafe name)",
      draggable: e.valid ? "true" : null,
      onclick: (ev) => {
        if (!e.valid || ev.target.closest("button, input, a")) return;
        if (p.selected.has(e.name)) p.selected.delete(e.name);
        else p.selected.add(e.name);
        renderPane(side);
      },
      ondragstart: (ev) => {
        if (!p.selected.has(e.name)) {
          p.selected.clear();
          p.selected.add(e.name);
        }
        ev.dataTransfer.setData("application/x-hopsync", side);
        ev.dataTransfer.effectAllowed = "copy";
      },
    });
    const check = h("input", {
      type: "checkbox",
      "aria-label": `Select ${e.name}`,
      checked: p.selected.has(e.name),
      disabled: !e.valid,
      onchange: (ev) => {
        if (ev.target.checked) p.selected.add(e.name);
        else p.selected.delete(e.name);
        renderPane(side);
      },
    });
    const name =
      e.kind === "dir" && e.valid
        ? h("button", { class: "link name dir", text: e.name, onclick: go(joinPath(p.path, e.name)) })
        : h("span", { class: `name ${e.kind}`, text: e.name });
    row.append(
      h("td", { class: "sel" }, check),
      h("td", {}, h("span", { class: `icon ${e.kind}`, "aria-hidden": "true" }), name),
      h("td", { class: "num", text: e.kind === "file" ? bytes(e.size) : "" }),
      h("td", { class: "muted", text: e.mtime ? new Date(e.mtime * 1000).toLocaleString() : "" }),
    );
    body.append(row);
  }
  const scroller = h(
    "div",
    {
      class: "scroll",
      ondragover: (ev) => {
        if (ev.dataTransfer.types.includes("application/x-hopsync")) {
          ev.preventDefault();
          scroller.classList.add("drop");
        }
      },
      ondragleave: () => scroller.classList.remove("drop"),
      ondrop: (ev) => {
        scroller.classList.remove("drop");
        const from = ev.dataTransfer.getData("application/x-hopsync");
        if (from && from !== side) {
          ev.preventDefault();
          startTransfer(from === "left" ? "push" : "pull");
        }
      },
    },
    table,
  );
  if (p.loading) scroller.append(h("p", { class: "muted pad", text: "Loading…" }));
  else if (p.error) scroller.append(h("p", { class: "error pad", text: p.error }));
  else if (!p.entries.length) scroller.append(h("p", { class: "muted pad", text: "Empty folder" }));
  box.append(title, crumbs, tools, scroller);
}

/** Opens the confirm dialog for the selection: policy, verify, a dry-run preview, then queue. */
function startTransfer(direction) {
  const from = direction === "push" ? panes.left : panes.right;
  const to = direction === "push" ? panes.right : panes.left;
  const server = panes.right.server;
  if (server === null) {
    toast("Add a server first.");
    return;
  }
  const names = [...from.selected];
  if (!names.length) {
    toast(`Select files or folders on the ${direction === "push" ? "left" : "right"} first.`);
    return;
  }
  const specs = names.map((n) => ({
    direction,
    server,
    src: joinPath(from.path, n),
    dst: joinPath(to.path, n),
  }));
  const policy = h(
    "select",
    { name: "existing" },
    POLICIES.map(([v, label]) => h("option", { value: v, selected: v === state.settings.default_existing ? "selected" : null, text: label })),
  );
  const policyHint = h("small");
  const verify = h("input", { type: "checkbox", checked: state.settings.default_verify });
  const preview = h("div", { class: "preview" });
  const queueBtn = h("button", { class: "primary", text: "Queue", disabled: true });
  const where = direction === "push" ? `${serverName(server)}:/${to.path}` : `this box:/${to.path}`;
  const dialog = h(
    "dialog",
    { class: "wide" },
    h("h2", { text: `${direction === "push" ? "Upload" : "Download"} ${names.length === 1 ? names[0] : `${names.length} items`}` }),
    h("p", { class: "muted", text: `To ${where}` }),
    h("div", { class: "row" }, field("If a file exists", policy, null), h("label", { class: "check" }, verify, "Verify after copying (reads everything back)")),
    policyHint,
    preview,
    h("div", { class: "buttons" }, h("button", { class: "ghost", text: "Cancel", onclick: () => dialog.close() }), queueBtn),
  );
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
  let seq = 0;
  const runPreview = async () => {
    const mine = ++seq;
    policyHint.textContent = POLICIES.find(([v]) => v === policy.value)[2];
    queueBtn.disabled = true;
    clear(preview).append(h("p", { class: "muted", text: "Checking what would happen…" }));
    const results = [];
    for (const s of specs) {
      try {
        results.push({ spec: s, plan: await api("POST", "/api/jobs/dry-run", { ...s, existing: policy.value, verify: verify.checked }) });
      } catch (e) {
        results.push({ spec: s, error: e.message });
      }
    }
    if (mine !== seq) return;
    renderPreview(preview, results);
    queueBtn.disabled = results.every((r) => r.error);
  };
  policy.addEventListener("change", runPreview);
  queueBtn.addEventListener("click", async () => {
    queueBtn.disabled = true;
    let queued = 0;
    for (const s of specs) {
      const r = await attempt(() => api("POST", "/api/jobs", { ...s, existing: policy.value, verify: verify.checked }));
      if (r) queued += 1;
    }
    dialog.close();
    if (queued) {
      from.selected.clear();
      renderPane(direction === "push" ? "left" : "right");
      toast(`Queued ${queued} job${queued === 1 ? "" : "s"}.`, "ok");
      state.queueTab = state.snapshot.queue_paused ? "queued" : "active";
      prefs.set("queueTab", state.queueTab);
      refreshJobs();
    }
  });
  runPreview();
}

function renderPreview(box, results) {
  clear(box);
  let files = 0;
  let size = 0;
  let problems = 0;
  const counts = {};
  const rows = [];
  for (const r of results) {
    if (r.error) {
      rows.push(h("tr", { class: "bad" }, h("td", { text: r.spec.src }), h("td", { text: "error" }), h("td"), h("td", { text: r.error })));
      continue;
    }
    files += r.plan.files;
    size += r.plan.bytes;
    problems += r.plan.problems;
    for (const it of r.plan.items) {
      counts[it.outcome] = (counts[it.outcome] || 0) + 1;
      if (rows.length < 300) {
        const path = results.length > 1 ? joinPath(r.spec.src, it.path) : it.path;
        rows.push(
          h(
            "tr",
            { class: `out-${it.outcome}` },
            h("td", { text: path || r.spec.src }),
            h("td", { text: { create: "new", replace: "replace", unchanged: "unchanged", skipped: "skip", failed: "fail" }[it.outcome] || it.outcome }),
            h("td", { class: "num", text: it.outcome === "create" || it.outcome === "replace" ? bytes(it.bytes) : "" }),
            h("td", { class: "muted", text: it.outcome === "skipped" || it.outcome === "failed" ? it.detail : "" }),
          ),
        );
      }
    }
    if (r.plan.truncated) rows.push(h("tr", {}, h("td", { colspan: 4, class: "muted", text: "Only the first 10,000 entries are listed; the totals cover everything." })));
  }
  const summary = [
    `${files} file${files === 1 ? "" : "s"} to copy (${bytes(size)})`,
    counts.replace ? `${counts.replace} replaced` : null,
    counts.skipped ? `${counts.skipped} skipped` : null,
    counts.unchanged ? `${counts.unchanged} unchanged` : null,
    problems && !counts.skipped ? `${problems} problems` : null,
  ].filter(Boolean);
  box.append(
    h("p", { class: "summary", text: summary.join(" · ") }),
    rows.length ? h("div", { class: "scroll short" }, h("table", { class: "files" }, h("tbody", {}, rows))) : h("p", { class: "muted", text: "Nothing to do." }),
  );
}

// Queue (U5, U6, U9).

const TABS = [
  ["active", "Active", (j) => j.state === "running"],
  ["queued", "Queued", (j) => j.state === "queued" || j.state === "paused"],
  ["failed", "Failed", (j) => j.state === "failed" || j.state === "cancelled"],
  ["done", "Done", (j) => j.state === "done"],
];

let refreshing = null;
async function refreshJobs() {
  if (refreshing) return refreshing;
  refreshing = (async () => {
    try {
      const before = new Map(state.jobs.map((j) => [j.id, j.state]));
      state.jobs = await api("GET", "/api/jobs");
      // A job that just finished changed a folder: show the result without a manual refresh.
      const finished = state.jobs.some((j) => ["done", "failed", "cancelled"].includes(j.state) && before.has(j.id) && before.get(j.id) !== j.state);
      if (finished && document.getElementById("pane-left")) {
        loadPane("left");
        loadPane("right");
      }
    } catch {
      // Shown by api() when it matters (login); a transient failure keeps the old list.
    }
    refreshing = null;
    renderQueue();
  })();
  return refreshing;
}

function progressOf(id) {
  return state.snapshot.running.find((p) => p.job === id);
}

function renderQueue() {
  const box = document.getElementById("queue");
  if (!box) return;
  clear(box);
  const tabs = h("div", { class: "tabs", role: "tablist" });
  for (const [id, label, pick] of TABS) {
    const n = state.jobs.filter(pick).length;
    tabs.append(
      h("button", {
        role: "tab",
        "aria-selected": state.queueTab === id ? "true" : "false",
        class: state.queueTab === id ? "tab active" : "tab",
        text: n ? `${label} (${n})` : label,
        onclick: () => {
          state.queueTab = id;
          prefs.set("queueTab", id);
          renderQueue();
        },
      }),
    );
  }
  if (state.queueTab === "done" && state.jobs.some((j) => j.state === "done")) {
    tabs.append(
      h("span", { class: "spacer" }),
      h("button", {
        class: "ghost small",
        text: "Clear done",
        onclick: async () => {
          for (const j of state.jobs.filter((x) => x.state === "done")) await attempt(() => api("DELETE", `/api/jobs/${j.id}`));
          refreshJobs();
        },
      }),
    );
  }
  const pick = TABS.find(([id]) => id === state.queueTab)[2];
  const jobs = state.jobs.filter(pick);
  if (state.queueTab === "done" || state.queueTab === "failed") jobs.reverse();
  const list = h("div", { class: "jobs" });
  if (!jobs.length) list.append(h("p", { class: "muted pad", text: { active: "Nothing is running.", queued: "The queue is empty.", failed: "No failed or cancelled jobs.", done: "No finished jobs yet." }[state.queueTab] }));
  jobs.forEach((j, i) => list.append(jobRow(j, i, jobs)));
  box.append(tabs, list);
}

function jobRow(j, i, list) {
  const p = progressOf(j.id);
  const arrow = j.direction === "push" ? "→" : "←";
  const what = h(
    "div",
    { class: "what" },
    h("a", { href: `#/job/${j.id}`, class: "job-title", text: j.direction === "push" ? j.src : j.dst }),
    h("span", { class: "muted", text: ` ${arrow} ${serverName(j.server)}:/${j.direction === "push" ? j.dst : j.src}` }),
  );
  const status = h("div", { class: "status" });
  if (j.state === "running" && p) {
    const bar = h("progress", { max: p.total || 1 });
    if (p.total) bar.value = Math.min(p.bytes, p.total);
    else bar.removeAttribute("value");
    const eta = p.total && p.rate ? ` · ${duration((p.total - p.bytes) / p.rate)} left` : "";
    status.append(
      bar,
      h("span", {
        class: "muted",
        text: `${bytes(p.bytes)}${p.total ? ` of ${bytes(p.total)}` : ""} · ${rate(p.rate) || "starting"}${eta} · ${p.files} files${p.problems ? ` · ${p.problems} problems` : ""}`,
      }),
    );
  } else {
    const label = { queued: "Queued", paused: "Paused", running: "Starting", done: "Done", failed: "Failed", cancelled: "Cancelled" }[j.state];
    const detail =
      j.state === "done"
        ? `${j.files} files, ${bytes(j.bytes)}${j.problems ? `, ${j.problems} skipped or failed` : ""} · ${when(j.finished)}`
        : j.state === "failed"
          ? j.error || ""
          : j.state === "cancelled"
            ? when(j.finished)
            : "";
    status.append(h("span", { class: `badge ${j.state}`, text: label }), h("span", { class: j.state === "failed" ? "error" : "muted", text: ` ${detail}` }));
  }
  const act = (label, path, title) => h("button", { class: "ghost small", text: label, title, onclick: () => attempt(() => api("POST", `/api/jobs/${j.id}/${path}`, {})) });
  const actions = h("div", { class: "actions" });
  if (j.state === "running" || j.state === "queued") actions.append(act("Pause", "pause", "Stop for now; resumes where it left off"));
  if (j.state === "paused") actions.append(act("Resume", "resume"));
  if (j.state === "failed" || j.state === "cancelled") actions.append(act("Retry", "resume", "Queue again; finished parts are not copied again"));
  if (!["done", "failed", "cancelled"].includes(j.state)) actions.append(act("Cancel", "cancel"));
  if (j.state === "queued" && i > 0) {
    actions.append(h("button", { class: "ghost small", text: "↑", title: "Move up", onclick: () => attempt(() => api("POST", `/api/jobs/${j.id}/move`, { before: list[i - 1].id })) }));
  }
  if (["done", "failed", "cancelled"].includes(j.state)) {
    actions.append(h("button", { class: "ghost small", text: "Remove", onclick: async () => { await attempt(() => api("DELETE", `/api/jobs/${j.id}`)); refreshJobs(); } }));
  }
  return h("div", { class: `job ${j.state}` }, what, status, actions);
}

// Job detail (U10, U11).

async function viewJob(id) {
  const m = clear(main());
  const job = await attempt(() => api("GET", `/api/jobs/${Number(id)}`));
  if (!job) {
    m.append(h("p", { class: "error", text: "No such job." }));
    return;
  }
  const events = (await attempt(() => api("GET", `/api/jobs/${job.id}/events`))) || [];
  const head = h(
    "div",
    { class: "card" },
    h("a", { href: "#/transfers", class: "muted", text: "← Transfers" }),
    h("h1", { text: `Job ${job.id}: ${job.direction === "push" ? "upload" : "download"}` }),
    h(
      "dl",
      { class: "facts" },
      h("dt", { text: "From" }),
      h("dd", { text: job.direction === "push" ? `this box:/${job.src}` : `${serverName(job.server)}:/${job.src}` }),
      h("dt", { text: "To" }),
      h("dd", { text: job.direction === "push" ? `${serverName(job.server)}:/${job.dst}` : `this box:/${job.dst}` }),
      h("dt", { text: "State" }),
      h("dd", {}, h("span", { class: `badge ${job.state}`, text: job.state })),
      h("dt", { text: "Existing files" }),
      h("dd", { text: (POLICIES.find(([v]) => v === job.existing) || [, job.existing])[1] }),
      h("dt", { text: "Verify" }),
      h("dd", { text: job.verify ? "yes" : "no" }),
      h("dt", { text: "Result" }),
      h("dd", { text: `${job.files} files, ${bytes(job.bytes)}, ${job.problems} skipped or failed` }),
      job.error ? h("dt", { text: "Error" }) : null,
      job.error ? h("dd", { class: "error", text: job.error }) : null,
    ),
  );
  const eventList = h("ol", { class: "events" }, events.map((e) => h("li", {}, h("span", { class: "muted", text: new Date(e.at * 1000).toLocaleString() }), " ", e.text)));
  const filter = h(
    "select",
    { "aria-label": "Show" },
    [["", "All files"], ["sent", "Copied"], ["skipped", "Skipped"], ["failed", "Failed"], ["unchanged", "Unchanged"]].map(([v, l]) => h("option", { value: v, text: l })),
  );
  const tbody = h("tbody");
  const more = h("button", { class: "ghost", text: "Load more" });
  let offset = 0;
  const loadItems = async (reset) => {
    if (reset) {
      offset = 0;
      clear(tbody);
    }
    const page = (await attempt(() => api("GET", `/api/jobs/${job.id}/items?offset=${offset}&limit=500`))) || [];
    offset += page.length;
    for (const it of page) {
      if (filter.value && it.outcome !== filter.value) continue;
      tbody.append(h("tr", { class: `out-${it.outcome}` }, h("td", { text: it.path }), h("td", { text: it.outcome }), h("td", { class: "num", text: it.outcome === "sent" ? bytes(it.bytes) : "" }), h("td", { class: "muted", text: it.outcome === "sent" ? "" : it.detail })));
    }
    more.hidden = page.length < 500;
  };
  filter.addEventListener("change", () => loadItems(true));
  more.addEventListener("click", () => loadItems(false));
  m.append(
    head,
    h("div", { class: "card" }, h("h2", { text: "Events" }), events.length ? eventList : h("p", { class: "muted", text: "None." })),
    h(
      "div",
      { class: "card" },
      h("div", { class: "row" }, h("h2", { text: "Files" }), h("span", { class: "spacer" }), filter),
      h("div", { class: "scroll tall" }, h("table", { class: "files" }, h("thead", {}, h("tr", {}, h("th", { text: "Path" }), h("th", { text: "Outcome" }), h("th", { class: "num", text: "Size" }), h("th", { text: "Detail" }))), tbody)),
      more,
    ),
  );
  loadItems(true);
}

// Servers (U2, U3, U4, U14).

function viewServers() {
  const m = clear(main());
  const list = h("div", { class: "card" });
  m.append(
    h("div", { class: "row" }, h("h1", { text: "Servers" }), h("span", { class: "spacer" }), h("button", { class: "primary", text: "Add server", onclick: () => serverForm(null) })),
    list,
  );
  if (!state.servers.length) {
    list.append(h("p", { class: "muted", text: "No servers yet. Any SSH account works: the other side needs only OpenSSH, not Hopsync." }));
    return;
  }
  const rows = state.servers.map((s) =>
    h(
      "tr",
      {},
      h("td", {}, h("strong", { text: s.name })),
      h("td", { class: "path", text: `${s.user}@${s.host}:${s.port}` }),
      h("td", { class: "path", text: s.root }),
      h("td", { class: "path", text: s.key_secret ? "generated key" : s.key_file }),
      h("td", {}, s.blocked ? h("span", { class: "badge failed", title: s.blocked, text: "blocked" }) : null),
      h(
        "td",
        { class: "actions" },
        h("button", { class: s.blocked ? "warn small" : "ghost small", text: s.blocked ? "Review key" : "Check key", onclick: () => trustFlow(s) }),
        s.key_secret ? h("button", { class: "ghost small", text: "Install key", onclick: () => installFlow(s) }) : null,
        h("button", { class: "ghost small", text: "Edit", onclick: () => serverForm(s) }),
        h("button", {
          class: "ghost small danger",
          text: "Remove",
          onclick: async () => {
            if (!confirm(`Remove ${s.name}? Its finished jobs and its generated key are removed too.`)) return;
            if (await attempt(() => api("DELETE", `/api/servers/${s.id}`), `Removed ${s.name}.`) !== undefined) {
              await loadServers();
              viewServers();
            }
          },
        }),
      ),
    ),
  );
  list.append(
    h(
      "div",
      { class: "scroll short" },
      h("table", { class: "files servers" }, h("thead", {}, h("tr", {}, ["Name", "Account", "Root", "Key", "Status", ""].map((t) => h("th", { text: t })))), h("tbody", {}, rows)),
    ),
  );
}

function serverForm(s) {
  const editing = s !== null;
  const input = (name, attrs = {}) => h("input", { name, value: s ? s[name] : attrs.value, ...attrs });
  const generate = h("input", { type: "radio", name: "auth", value: "generate", checked: !editing });
  const useFile = h("input", { type: "radio", name: "auth", value: "file", checked: editing && !s.key_secret });
  const keyFile = input("key_file", { placeholder: "/home/you/.ssh/id_ed25519", value: "" });
  const err = h("p", { class: "error", role: "alert" });
  const form = h(
    "form",
    {
      onsubmit: async (ev) => {
        ev.preventDefault();
        err.textContent = "";
        const f = new FormData(form);
        const minFree = parseSize(String(f.get("min_free") || "0"));
        if (minFree === null) {
          err.textContent = "Minimum free space: use a size like 10G or 0.";
          return;
        }
        const spec = {
          name: String(f.get("name")).trim(),
          host: String(f.get("host")).trim(),
          port: Number(f.get("port") || 22),
          user: String(f.get("user")).trim(),
          root: String(f.get("root")).trim(),
          min_free: minFree,
        };
        const gen = !editing && generate.checked;
        if (!gen && !(editing && s.key_secret)) spec.key_file = String(f.get("key_file")).trim();
        try {
          const saved = editing ? await api("PUT", `/api/servers/${s.id}`, spec) : await api("POST", "/api/servers", { ...spec, generate_key: gen });
          dialog.close();
          await loadServers();
          viewServers();
          if (!editing) trustFlow(saved, gen);
        } catch (e) {
          err.textContent = e.message;
        }
      },
    },
    h("h2", { text: editing ? `Edit ${s.name}` : "Add a server" }),
    h(
      "div",
      { class: "grid2" },
      field("Name", input("name", { required: true, maxlength: 64, placeholder: "seedbox" })),
      field("User", input("user", { required: true, autocomplete: "off" })),
      field("Host", input("host", { required: true, placeholder: "box.example.com", spellcheck: "false" })),
      field("Port", input("port", { type: "number", min: 1, max: 65535, value: s ? s.port : 22 })),
    ),
    field("Root folder", input("root", { required: true, placeholder: "/home/you", spellcheck: "false" }), "Absolute path on the server. Hopsync never reads or writes outside it."),
    field("Minimum free space", h("input", { name: "min_free", value: s ? bytes(s.min_free).replace(" ", "").replace("iB", "") : "0" }), "Uploads that would leave less than this free are refused, e.g. 10G. Protects the box's own work."),
    editing && s.key_secret
      ? h("p", { class: "muted", text: "Authentication: a key Hopsync generated for this server." })
      : h(
          "fieldset",
          {},
          h("legend", { text: "Authentication" }),
          editing ? null : h("label", { class: "check" }, generate, "Generate a key for this server (recommended): stored encrypted, installed with your password once"),
          h("label", { class: "check" }, useFile, "Use a key file on this box"),
          keyFile,
        ),
    err,
    h("div", { class: "buttons" }, h("button", { class: "ghost", type: "button", text: "Cancel", onclick: () => dialog.close() }), h("button", { class: "primary", type: "submit", text: editing ? "Save" : "Add" })),
  );
  if (editing && s.key_file) keyFile.value = s.key_file;
  const syncKey = () => {
    keyFile.disabled = generate.checked;
    keyFile.required = !generate.checked;
  };
  generate.addEventListener("change", syncKey);
  useFile.addEventListener("change", syncKey);
  if (!editing || !s.key_secret) syncKey();
  const dialog = h("dialog", {}, form);
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
}

/** U3: shows the offered host key and pins it only on an explicit confirm. */
async function trustFlow(s, thenInstall = false) {
  const body = h("div", {}, h("p", { class: "muted", text: `Asking ${s.host}:${s.port} for its host key…` }));
  const dialog = h("dialog", { class: "wide" }, h("h2", { text: `Host key of ${s.name}` }), body);
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
  let probe;
  try {
    probe = await api("POST", `/api/servers/${s.id}/probe`, {});
  } catch (e) {
    clear(body).append(h("p", { class: "error", text: e.message }), h("div", { class: "buttons" }, h("button", { class: "ghost", text: "Close", onclick: () => dialog.close() })));
    return;
  }
  const fp = h("code", { class: "fingerprint", text: probe.fingerprint });
  const close = h("button", { class: "ghost", text: "Close", onclick: () => dialog.close() });
  clear(body);
  const trust = async (replace) => {
    try {
      await api("POST", `/api/servers/${s.id}/trust`, { fingerprint: probe.fingerprint, replace });
      dialog.close();
      toast(`${s.name}: host key pinned.`, "ok");
      await loadServers();
      if (location.hash.startsWith("#/servers")) viewServers();
      if (thenInstall) installFlow(state.servers.find((x) => x.id === s.id) || s);
    } catch (e) {
      toast(e.message);
    }
  };
  if (probe.status === "trusted") {
    body.append(h("p", {}, "The server offers the pinned ", probe.algorithm, " key: "), fp, h("p", { class: "ok", text: "It matches. Nothing to do." }), h("div", { class: "buttons" }, close));
    if (thenInstall) installFlow(s);
  } else if (probe.status === "unknown") {
    body.append(
      h("p", {}, "The server offers this ", probe.algorithm, " key:"),
      fp,
      h("p", { text: "Compare it with the fingerprint your provider shows, or with the output of ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub on the server. Trust it only if they match: Hopsync will refuse any other key from then on." }),
      h("div", { class: "buttons" }, close, h("button", { class: "primary", text: "They match: trust", onclick: () => trust(false) })),
    );
  } else {
    const ack = h("input", { type: "checkbox" });
    const replace = h("button", { class: "danger", text: "Replace the pinned key", disabled: true, onclick: () => trust(true) });
    ack.addEventListener("change", () => {
      replace.disabled = !ack.checked;
    });
    body.append(
      h("p", { class: "error", text: "The server now offers a different host key than the one pinned. This happens when a server is reinstalled, and also when someone is intercepting the connection." }),
      h("p", { text: "Pinned:" }),
      ...(probe.pinned || []).map((p) => h("code", { class: "fingerprint", text: p })),
      h("p", { text: `Offered now (${probe.algorithm}):` }),
      fp,
      h("label", { class: "check" }, ack, "I confirmed with the provider that the server's key changed on purpose"),
      h("div", { class: "buttons" }, close, replace),
    );
  }
}

/** U4: installs the generated key with a password used once, or shows the line to add by hand. */
async function installFlow(s) {
  const pub = await attempt(() => api("GET", `/api/servers/${s.id}/public-key`));
  if (!pub || !pub.public_key) return;
  const password = h("input", { type: "password", name: "password", autocomplete: "off" });
  const keyFile = h("input", { name: "key_file", placeholder: "or: /home/you/.ssh/id_ed25519", spellcheck: "false" });
  const target = h("input", { name: "authorized_keys", placeholder: ".ssh/authorized_keys", spellcheck: "false" });
  const err = h("p", { class: "error", role: "alert" });
  const go = h("button", { class: "primary", type: "submit", text: "Install" });
  const form = h(
    "form",
    {
      onsubmit: async (ev) => {
        ev.preventDefault();
        err.textContent = "";
        const body = {};
        if (password.value) body.password = password.value;
        else if (keyFile.value.trim()) body.key_file = keyFile.value.trim();
        else {
          err.textContent = "Enter the account password, or a key file the server already accepts.";
          return;
        }
        if (target.value.trim()) body.authorized_keys = target.value.trim();
        go.disabled = true;
        try {
          await api("POST", `/api/servers/${s.id}/install-key`, body);
          password.value = "";
          dialog.close();
          toast(`${s.name}: key installed and working.`, "ok");
        } catch (e) {
          err.textContent = e.message;
        } finally {
          go.disabled = false;
        }
      },
    },
    h("h2", { text: `Install the key for ${s.name}` }),
    h("p", { text: "Hopsync made a key just for this server. It needs to be in the account's authorized_keys file. Either add this line yourself:" }),
    h("textarea", { readonly: true, rows: 3, class: "mono", text: pub.public_key }),
    h("p", { text: "or let Hopsync add it now, logging in once with the account password. The password is used for that one login and never stored." }),
    field("Account password", password),
    field("Or a key file on this box", keyFile),
    h("details", {}, h("summary", { text: "Advanced" }), field("authorized_keys file", target, "Relative to the login folder, or absolute. Must be named authorized_keys or authorized_keys2.")),
    err,
    h("div", { class: "buttons" }, h("button", { class: "ghost", type: "button", text: "Later", onclick: () => dialog.close() }), go),
  );
  const dialog = h("dialog", { class: "wide" }, form);
  dialog.addEventListener("close", () => {
    password.value = "";
    dialog.remove();
  });
  document.body.append(dialog);
  dialog.showModal();
}

// Settings.

async function viewSettings() {
  const m = clear(main());
  const s = state.settings;
  const policy = h("select", { name: "default_existing" }, POLICIES.map(([v, label]) => h("option", { value: v, selected: v === s.default_existing ? "selected" : null, text: label })));
  const verify = h("input", { type: "checkbox", name: "default_verify", checked: s.default_verify });
  const minFree = h("input", { name: "local_min_free", value: s.local_min_free ? bytes(s.local_min_free).replace(" ", "").replace("iB", "") : "0" });
  const defaults = h(
    "form",
    {
      class: "card",
      onsubmit: async (ev) => {
        ev.preventDefault();
        const n = parseSize(minFree.value);
        if (n === null) {
          toast("Minimum free space: use a size like 10G or 0.");
          return;
        }
        const r = await attempt(() => api("PUT", "/api/settings", { default_existing: policy.value, default_verify: verify.checked, local_min_free: n }), "Saved.");
        if (r) state.settings = r;
      },
    },
    h("h2", { text: "Defaults for new transfers" }),
    field("If a file exists", policy),
    h("label", { class: "check" }, verify, "Verify after copying"),
    field("Minimum free space on this box", minFree, "Downloads that would leave less than this free are refused, e.g. 20G."),
    h("div", { class: "buttons" }, h("button", { class: "primary", type: "submit", text: "Save" })),
  );
  const cur = h("input", { type: "password", autocomplete: "current-password", required: true });
  const nw = h("input", { type: "password", autocomplete: "new-password", required: true, minlength: 12 });
  const again = h("input", { type: "password", autocomplete: "new-password", required: true });
  const pw = h(
    "form",
    {
      class: "card",
      onsubmit: async (ev) => {
        ev.preventDefault();
        if (nw.value !== again.value) {
          toast("The new passwords differ.");
          return;
        }
        const r = await attempt(() => api("POST", "/api/password", { current: cur.value, new: nw.value }), "Password changed. Other sessions were logged out.");
        if (r) pw.reset();
      },
    },
    h("h2", { text: "Admin password" }),
    field("Current password", cur),
    field("New password", nw, "At least 12 characters. Changing it logs out every other session."),
    field("Repeat new password", again),
    h("div", { class: "buttons" }, h("button", { class: "primary", type: "submit", text: "Change password" })),
  );
  const audit = h("div", { class: "card" }, h("h2", { text: "Audit log" }));
  m.append(h("h1", { text: "Settings" }), h("div", { class: "grid2" }, defaults, pw), audit);
  const rows = (await attempt(() => api("GET", "/api/audit?limit=200"))) || [];
  audit.append(
    rows.length
      ? h("div", { class: "scroll tall" }, h("table", { class: "files" }, h("tbody", {}, rows.map((r) => h("tr", {}, h("td", { class: "muted", text: new Date(r.at * 1000).toLocaleString() }), h("td", { class: "nowrap", text: r.event }), h("td", { text: r.detail }))))))
      : h("p", { class: "muted", text: "Empty." }),
  );
}

boot();
