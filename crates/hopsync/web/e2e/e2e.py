#!/usr/bin/env python3
"""Drives the real UI in a real browser against a real daemon and a real OpenSSH server.

Run from `crates/hopsync/web` after `pnpm build` and `cargo build --release`:

    python3 e2e/e2e.py --binary ../../../target/release/hopsync --shots /tmp/shots

Needs the Python `playwright` package and an installed Chromium. Set HOPSYNC_E2E_CHROMIUM to use a
specific browser binary. The daemon runs with an empty PATH, which proves a server needs no Node.
"""

import argparse
import glob
import os
import re
import shutil
import sys
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

sys.path.insert(0, str(Path(__file__).parent))
from fixture import HOSTILE_NAME, PASSWORD, World  # noqa: E402

NEW_PASSWORD = "an-even-longer-passphrase"


def chromium_path():
    if os.environ.get("HOPSYNC_E2E_CHROMIUM"):
        return os.environ["HOPSYNC_E2E_CHROMIUM"]
    found = sorted(glob.glob(os.path.expanduser("~/.cache/ms-playwright/chromium-*/chrome-linux*/chrome")))
    return found[-1] if found else None


class Run:
    def __init__(self, world, page, shots):
        self.w = world
        self.p = page
        self.shots = shots
        self.problems = []
        page.on("console", self.on_console)
        page.on("pageerror", lambda e: self.problems.append(f"page error: {e}"))

    def on_console(self, msg):
        text = msg.text
        if "Content Security Policy" in text or "Content-Security-Policy" in text:
            self.problems.append(f"CSP violation: {text}")
        # Refused requests are part of these flows (wrong password, destination exists).
        elif msg.type == "error" and "Failed to load resource" not in text:
            self.problems.append(f"console error: {text}")

    def shot(self, name):
        if self.shots:
            self.p.screenshot(path=str(Path(self.shots) / f"{name}.png"))

    def row(self, side, name):
        return self.p.locator(f'#pane-{side} [role="row"][draggable], #pane-{side} [role="row"][title]').filter(has_text=name).first

    def select(self, side, name):
        self.row(side, name).get_by_role("checkbox").check()

    def transfer(self, button="Upload", policy=None):
        self.p.get_by_role("button", name=button, exact=True).click()
        dialog = self.p.locator("dialog[open]")
        expect(dialog).to_be_visible()
        if policy:
            dialog.locator("select[name=existing]").select_option(policy)
        expect(dialog.get_by_role("button", name="Queue")).to_be_enabled(timeout=15000)
        return dialog


def step(name):
    print(f"  ok  {name}", flush=True)


def first_run(r):
    w, p = r.w, r.p
    p.goto(w.base)
    expect(p.get_by_role("heading", name="Set the admin password")).to_be_visible()
    r.shot("setup")
    p.get_by_label("Setup token").fill(w.setup_token())
    p.get_by_label("Password", exact=True).fill(PASSWORD)
    p.get_by_label("Repeat password").fill("something else entirely")
    p.get_by_role("button", name="Set password").click()
    expect(p.get_by_role("alert").filter(has_text="The passwords differ")).to_be_visible()
    p.get_by_label("Repeat password").fill(PASSWORD)
    p.get_by_role("button", name="Set password").click()
    expect(p.get_by_role("region", name="This box")).to_be_visible()
    step("first-run setup needs the token and a matching password")


def add_servers(r):
    w, p = r.w, r.p
    p.goto(w.base + "/#/servers")
    expect(p.get_by_text("No servers yet")).to_be_visible()
    p.get_by_role("button", name="Add server").click()
    d = p.locator("dialog[open]")
    d.locator("input[name=name]").fill("seedbox")
    d.locator("input[name=user]").fill(os.environ.get("USER") or __import__("getpass").getuser())
    d.locator("input[name=host]").fill("127.0.0.1")
    d.locator("input[name=port]").fill(str(w.ssh_port))
    d.locator("input[name=root]").fill(str(w.remote))
    d.get_by_label("Use a key file on this box").check()
    d.get_by_role("textbox", name="Key file").fill(str(w.client_key))
    d.get_by_role("button", name="Add", exact=True).click()
    d = p.locator("dialog[open]")
    expect(d.get_by_role("button", name="They match: trust")).to_be_visible(timeout=15000)
    r.shot("trust")
    d.get_by_role("button", name="They match: trust").click()
    expect(p.locator("dialog[open]")).to_have_count(0)
    expect(p.get_by_role("listitem").filter(has_text="seedbox")).to_be_visible()
    step("add a server with a key file; the host key needs an explicit trust")

    p.get_by_role("button", name="Add server").click()
    d = p.locator("dialog[open]")
    d.locator("input[name=name]").fill("generated")
    d.locator("input[name=user]").fill("nobody")
    d.locator("input[name=host]").fill("127.0.0.1")
    d.locator("input[name=port]").fill(str(w.ssh_port))
    d.locator("input[name=root]").fill("/tmp")
    d.get_by_label("Generate a key, and add it to the server myself").check()
    d.get_by_role("button", name="Add", exact=True).click()
    d = p.locator("dialog[open]")
    # The same key was pinned for the other server on this host and port, so it may match at once.
    trust = d.get_by_role("button", name="They match: trust")
    install = p.get_by_role("heading", name="Install the key for generated")
    expect(trust.or_(install)).to_be_visible(timeout=15000)
    if trust.is_visible():
        trust.click()
    expect(install).to_be_visible(timeout=15000)
    d = p.locator("dialog[open]")
    expect(d.get_by_label("Public key")).to_have_value(re.compile("^ssh-ed25519 "))
    d.get_by_role("button", name="Install").click()
    expect(d.get_by_role("alert").filter(has_text="Enter the account password")).to_be_visible()
    d.get_by_role("button", name="Later").click()
    step("a generated key shows its public line and needs a password or key to install")

    row = p.get_by_role("listitem").filter(has_text="generated")
    row.get_by_role("button", name="Remove").click()
    d = p.locator("dialog[open]")
    expect(d.get_by_text("This cannot be undone")).to_be_visible()
    d.get_by_role("button", name="Keep").click()
    expect(row).to_be_visible()
    row.get_by_role("button", name="Remove").click()
    p.locator("dialog[open]").get_by_role("button", name="Remove").click()
    expect(row).to_have_count(0)
    step("removing a server asks first, and Keep keeps it")
    r.shot("servers")


def browse(r):
    w, p = r.w, r.p
    p.goto(w.base + "/#/transfers")
    left = p.locator("#pane-left")
    expect(left.get_by_text(HOSTILE_NAME, exact=True)).to_be_visible()
    assert left.locator("b").count() == 0, "a hostile name became markup"
    step("a hostile file name is shown as text")

    expect(r.row("right", "only-on-server.txt")).to_be_visible()
    expect(p.get_by_role("button", name="Upload", exact=True)).to_be_disabled()

    left.get_by_role("button", name="Datasets", exact=True).click()
    expect(r.row("left", "wikipedia-en")).to_be_visible()
    left.get_by_role("button", name="Up one folder").click()
    expect(r.row("left", "notes.txt")).to_be_visible()
    step("folders open, and Up returns")

    p.get_by_label("Compare").check()
    expect(r.row("left", "notes.txt")).to_have_attribute("title", re.compile("Differs"))
    expect(r.row("left", "transfer.bin")).to_have_attribute("title", re.compile("Only on this side"))
    p.get_by_label("Compare").uncheck()
    step("compare marks files that differ or exist on one side")

    left.get_by_role("button", name="Huge", exact=True).click()
    expect(r.row("left", "file-00000")).to_be_visible()
    rows = left.locator('[role="row"][draggable]').count()
    assert 0 < rows < 200, f"expected only visible rows in the DOM, found {rows}"
    scroller = left.locator('[role="table"]')
    scroller.evaluate("el => { el.scrollTop = el.scrollHeight; }")
    expect(r.row("left", "file-19999")).to_be_visible()
    step(f"a folder of 20,000 files puts only {rows} rows in the DOM and still scrolls to the end")
    left.get_by_role("button", name="root", exact=True).click()
    expect(r.row("left", "notes.txt")).to_be_visible()


def transfers(r):
    w, p = r.w, r.p
    r.select("left", HOSTILE_NAME)
    d = r.transfer()
    expect(d.get_by_text("1 file to copy")).to_be_visible()
    assert d.locator("b").count() == 0
    r.shot("transfer-dialog")
    d.get_by_role("button", name="Queue").click()
    expect(r.row("right", HOSTILE_NAME)).to_be_visible(timeout=15000)
    step("upload: preview first, then the file appears on the right with no manual refresh")

    r.select("left", "notes.txt")
    p.get_by_role("button", name="Upload", exact=True).click()
    d = p.locator("dialog[open]")
    # One existing file under Skip: the daemon refuses it in the preview, and nothing can be queued.
    expect(d.get_by_text("destination exists")).to_be_visible(timeout=15000)
    expect(d.get_by_role("button", name="Queue")).to_be_disabled()
    d.locator("select[name=existing]").select_option("overwrite")
    expect(d.get_by_text("1 replaced")).to_be_visible(timeout=15000)
    d.get_by_role("button", name="Cancel").click()
    expect(p.locator("dialog[open]")).to_have_count(0)
    r.row("left", "notes.txt").get_by_role("checkbox").uncheck()
    step("the preview follows the policy; Cancel queues nothing")

    r.select("right", "only-on-server.txt")
    d = r.transfer("Download")
    d.get_by_role("button", name="Queue").click()
    expect(r.row("left", "only-on-server.txt")).to_be_visible(timeout=15000)
    step("download works the other way")

    left = r.row("left", "zero-byte.txt")
    left.drag_to(p.locator('#pane-right [role="table"]'))
    d = p.locator("dialog[open]")
    expect(d.get_by_role("heading", name="Upload zero-byte.txt")).to_be_visible()
    d.get_by_role("button", name="Cancel").click()
    r.row("left", "zero-byte.txt").get_by_role("checkbox").uncheck()
    step("dragging a row between panes opens the same dialog")


def browser_files(r):
    w, p = r.w, r.p
    data = bytes(i % 251 for i in range(3_000_000))
    for side, folder in (("right", w.remote), ("left", w.local)):
        pane = p.locator(f"#pane-{side}")
        name = f"from-computer-{side}.bin"
        pane.get_by_label("Files to upload").set_input_files(
            {"name": name, "mimeType": "application/octet-stream", "buffer": data}
        )
        expect(r.row(side, name)).to_be_visible(timeout=15000)
        assert (folder / name).read_bytes() == data, f"{side}: the file arrived changed"

        r.select(side, name)
        with p.expect_download() as saved:
            pane.get_by_role("button", name="Save to this computer").click()
        assert saved.value.suggested_filename == name
        with open(saved.value.path(), "rb") as f:
            assert f.read() == data, f"{side}: the saved file changed"
        r.row(side, name).get_by_role("checkbox").uncheck()
    step("files upload from and save to this computer, on this box and on a server")


def live_speed(r):
    w, p = r.w, r.p
    r.select("left", "transfer.bin")
    d = r.transfer(policy="overwrite")
    d.get_by_role("button", name="Queue").click()
    job = p.locator("[data-job]").filter(has_text="transfer.bin").first
    pause = job.get_by_role("button", name="Pause")
    expect(pause).to_be_visible(timeout=10000)
    pause.evaluate("el => { el.dataset.sameNode = 'yes'; }")
    expect(p).to_have_title(re.compile("↑"), timeout=15000)
    title = p.title()
    assert re.search(r"↑ [\d.]+ [KMG]iB/s", title), title
    step(f"the tab title shows live speed: {title}")
    upload = p.get_by_role("region", name="Live speed").locator("span.font-mono").nth(1)
    expect(upload).not_to_have_text("0 B/s")
    r.shot("live")
    seen = {upload.text_content()}
    for _ in range(12):
        p.wait_for_timeout(60)
        seen.add(upload.text_content())
    assert len(seen) > 3, f"the speed readout did not count smoothly: {seen}"
    step(f"the speed readout changes between updates ({len(seen)} values in 0.7 s)")
    if job.count():
        marked = job.locator("button[data-same-node='yes']").count()
        assert marked == 1, "the Pause button was replaced during live updates"
        step("the Pause button is the same DOM node after many updates")
    expect(p).not_to_have_title(re.compile("↑"), timeout=60000)
    step("the tab title drops the speed when the job ends")
    expect(upload).to_have_text("0 B/s", timeout=10000)


def queue(r):
    w, p = r.w, r.p
    tabs = p.get_by_role("tablist", name="Queue")
    p.get_by_role("button", name="Pause queue").click()
    expect(p.get_by_role("button", name="Resume queue")).to_be_visible()
    for name in ["notes.txt", "zero-byte.txt"]:
        r.select("left", name)
        d = r.transfer(policy="overwrite")
        d.get_by_role("button", name="Queue").click()
        expect(p.locator("dialog[open]")).to_have_count(0)
    expect(tabs.get_by_role("tab", name=re.compile("Queued"))).to_have_attribute("aria-selected", "true")
    rows = p.locator("[data-job]")
    expect(rows).to_have_count(2)
    expect(rows.nth(0)).to_contain_text("notes.txt")
    rows.nth(1).get_by_role("button", name="Move up").click()
    expect(rows.nth(0)).to_contain_text("zero-byte.txt")
    step("a paused queue holds jobs, and they can be reordered")

    rows.nth(0).get_by_role("button", name="Pause").click()
    expect(rows.nth(0).get_by_role("button", name="Resume")).to_be_visible()
    rows.nth(0).get_by_role("button", name="Cancel").click()
    tabs.get_by_role("tab", name=re.compile("Failed")).click()
    cancelled = p.locator("[data-job]").filter(has_text="zero-byte.txt").first
    expect(cancelled).to_contain_text("Cancelled")
    cancelled.get_by_role("button", name="Retry").click()
    tabs.get_by_role("tab", name=re.compile("Queued")).click()
    expect(p.locator("[data-job]").filter(has_text="zero-byte.txt")).to_have_count(1)
    p.get_by_role("button", name="Resume queue").click()
    tabs.get_by_role("tab", name=re.compile("Done")).click()
    expect(p.locator("[data-job]").filter(has_text="zero-byte.txt").first).to_be_visible(timeout=20000)
    expect(p.locator("[data-job]").filter(has_text="notes.txt").first).to_be_visible(timeout=20000)
    step("pause, resume, cancel and retry all work")

    p.locator("[data-job]").filter(has_text=HOSTILE_NAME).first.get_by_role("link").click()
    expect(p.get_by_role("heading", name=re.compile(r"Job \d+: upload"))).to_be_visible()
    expect(p.locator("ol li").filter(has_text="started")).to_be_visible()
    expect(p.locator('[role="row"]').filter(has_text=HOSTILE_NAME)).to_be_visible()
    assert p.locator("main b").count() == 0
    r.shot("job")
    p.get_by_role("link", name="Transfers").first.click()
    step("the job page lists events and each file")

    tabs.get_by_role("tab", name=re.compile("Done")).click()
    p.get_by_role("button", name="Clear done").click()
    expect(p.get_by_text("No finished jobs yet.")).to_be_visible(timeout=10000)
    step("Clear done removes finished jobs")


def changed_host_key(r):
    w, p = r.w, r.p
    w.rotate_host_key()
    p.goto(w.base + "/#/servers")
    p.get_by_role("button", name="Check key").click()
    d = p.locator("dialog[open]")
    replace = d.get_by_role("button", name="Replace the pinned key")
    expect(replace).to_be_disabled(timeout=15000)
    expect(d.get_by_text("someone is intercepting")).to_be_visible()
    r.shot("changed-key")
    d.get_by_role("checkbox").check()
    expect(replace).to_be_enabled()
    replace.click()
    expect(p.locator("dialog[open]")).to_have_count(0)
    step("a changed host key blocks until a person confirms it")


def settings(r):
    w, p = r.w, r.p
    p.goto(w.base + "/#/settings")
    p.locator("input[name=local_min_free]").fill("nonsense")
    p.get_by_role("button", name="Save", exact=True).click()
    expect(p.get_by_role("alert").filter(has_text="Minimum free space")).to_be_visible()
    p.locator("input[name=local_min_free]").fill("20G")
    p.locator("select[name=default_existing]").select_option("update")
    p.get_by_role("button", name="Save", exact=True).click()
    expect(p.get_by_role("status").filter(has_text="Saved")).to_be_visible()
    p.reload()
    expect(p.locator("input[name=local_min_free]")).to_have_value("20G")
    expect(p.locator("select[name=default_existing]")).to_have_value("update")
    step("settings are validated and survive a reload")
    expect(p.get_by_role("cell", name="settings", exact=False).first).to_be_visible()
    step("the audit log is shown")
    r.shot("settings")

    p.get_by_label("Current password").fill(PASSWORD)
    p.get_by_label("New password", exact=True).fill(NEW_PASSWORD)
    p.get_by_label("Repeat new password").fill(NEW_PASSWORD)
    p.get_by_role("button", name="Change password").click()
    expect(p.get_by_role("status").filter(has_text="Password changed")).to_be_visible()
    step("the password can be changed")

    p.get_by_role("button", name="Log out").first.click()
    expect(p.get_by_role("heading", name="Log in")).to_be_visible()
    r.shot("login")
    p.get_by_label("Password").fill("not the password at all")
    p.get_by_role("button", name="Log in").click()
    expect(p.get_by_role("alert").filter(has_text="Wrong password")).to_be_visible()
    p.get_by_label("Password").fill(NEW_PASSWORD)
    p.get_by_role("button", name="Log in").click()
    expect(p.get_by_role("heading", name="Settings")).to_be_visible()
    step("logout, a wrong password, then the right one, back on the same page")


def session_end(r):
    """Another tab logs out: the open stream ends, and this page must notice with no click."""
    w, p = r.w, r.p
    p.goto(w.base + "/#/servers")
    expect(p.get_by_role("heading", name="Servers")).to_be_visible()
    cookie = next(c for c in p.context.cookies() if "session" in c["name"].lower())
    w.cookie = f"{cookie['name']}={cookie['value']}"
    w.api("POST", "/api/logout", {})
    expect(p.get_by_role("heading", name="Log in")).to_be_visible(timeout=25000)
    step("a session ended elsewhere shows the login page by itself")
    p.get_by_label("Password").fill(NEW_PASSWORD)
    p.get_by_role("button", name="Log in").click()
    expect(p.get_by_role("heading", name="Servers")).to_be_visible()


def theme_and_phone(r, browser, chromium_state):
    w, p = r.w, r.p
    p.goto(w.base + "/#/transfers")
    expect(r.row("right", "only-on-server.txt")).to_be_visible()
    p.wait_for_timeout(1500)
    r.shot("transfers-dark")
    p.get_by_role("button", name="Use the light theme").first.click()
    assert p.evaluate("document.documentElement.dataset.theme") == "light"
    p.wait_for_timeout(400)
    r.shot("transfers-light")
    p.reload()
    assert p.evaluate("document.documentElement.dataset.theme") == "light"
    p.get_by_role("button", name="Use the dark theme").first.click()
    step("the theme switches and is remembered")

    ctx = browser.new_context(viewport={"width": 390, "height": 844}, has_touch=True, is_mobile=True, storage_state=chromium_state, device_scale_factor=2, reduced_motion="reduce")
    phone = ctx.new_page()
    errs = []
    phone.on("console", lambda m: errs.append(m.text) if "Content Security Policy" in m.text else None)
    for route, check in [
        ("transfers", lambda: expect(phone.get_by_role("region", name="This box")).to_be_visible()),
        ("servers", lambda: expect(phone.get_by_role("heading", name="Servers")).to_be_visible()),
        ("settings", lambda: expect(phone.get_by_role("heading", name="Settings")).to_be_visible()),
    ]:
        phone.goto(f"{w.base}/#/{route}")
        check()
        phone.wait_for_timeout(300)
        wide = phone.evaluate("document.documentElement.scrollWidth - window.innerWidth")
        assert wide <= 0, f"{route} scrolls sideways by {wide}px at phone width"
        if r.shots:
            phone.screenshot(path=str(Path(r.shots) / f"phone-{route}.png"))
    assert not errs, errs
    step("phone width: nothing scrolls sideways on any page")
    ctx.close()


def stream_recovers(r):
    """The event stream is the only source of live numbers, so its loss must be visible and heal."""
    w, p = r.w, r.p
    p.goto(w.base + "/#/transfers")
    expect(p.get_by_role("region", name="This box")).to_be_visible()
    w.restart_daemon()
    expect(p.get_by_role("status").filter(has_text="Lost contact")).to_be_visible(timeout=20000)
    step("losing the daemon shows a notice")
    expect(p.get_by_role("status").filter(has_text="Lost contact")).to_have_count(0, timeout=40000)
    step("the stream reconnects by itself after the daemon returns")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="../../../target/release/hopsync")
    ap.add_argument("--shots", default=None)
    ap.add_argument("--headed", action="store_true")
    a = ap.parse_args()
    if a.shots:
        os.makedirs(a.shots, exist_ok=True)
    binary = shutil.which(a.binary) or os.path.abspath(a.binary)
    print(f"browser test of {binary}")
    with World(binary) as w, sync_playwright() as pw:
        browser = pw.chromium.launch(executable_path=chromium_path(), headless=not a.headed)
        ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=2)
        page = ctx.new_page()
        expect.set_options(timeout=10000)
        r = Run(w, page, a.shots)
        try:
            first_run(r)
            add_servers(r)
            browse(r)
            transfers(r)
            browser_files(r)
            live_speed(r)
            queue(r)
            changed_host_key(r)
            settings(r)
            session_end(r)
            theme_and_phone(r, browser, ctx.storage_state())
            stream_recovers(r)
        except Exception:
            r.shot("failure")
            print(page.locator("body").inner_text()[:3000])
            print("".join(r.problems))
            raise
        browser.close()
        if r.problems:
            print("\n".join(sorted(set(r.problems))))
            sys.exit("console problems found")
    print("all browser checks passed; no CSP violations")


if __name__ == "__main__":
    main()
