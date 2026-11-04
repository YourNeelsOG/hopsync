#!/usr/bin/env python3
"""Captures the README screenshots from real transfers on a throwaway local daemon and sshd.

    TMPDIR=/some/real/disk python3 e2e/screenshots.py --binary ../../../target/release/hopsync \
        --out ../../../docs/assets/screens

The transfers write several GiB, so point TMPDIR at a disk with room, not at tmpfs.
"""

import argparse
import os
import shutil
import sys
import threading
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

sys.path.insert(0, str(Path(__file__).parent))
from e2e import chromium_path  # noqa: E402
from fixture import HOSTILE_NAME, PASSWORD, World, sparse  # noqa: E402

GIB = 1024**3
RUN_SECONDS = 35


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="../../../target/release/hopsync")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    with World(os.path.abspath(a.binary)) as w, sync_playwright() as pw:
        # Test-only rows would distract in a picture of normal use.
        (w.local / HOSTILE_NAME).unlink()
        (w.local / "link-to-notes").unlink()
        (w.local / "zero-byte.txt").unlink()
        shutil.rmtree(w.local / "Huge")
        shutil.rmtree(w.local / "Many")
        (w.local / "transfer.bin").rename(w.local / "ubuntu-24.04-server-arm64.iso")
        sparse(w.remote / "fedora-42-workstation-x86_64.iso", 3 * GIB)

        w.finish_setup()
        w.add_server()
        # One upload and one download at a time, for as long as the pictures take. The daemon refuses
        # two unfinished jobs with the same destination, so each repeat waits for the last.
        stop = threading.Event()

        def repeat(spec):
            while not stop.is_set():
                job_id = w.api("POST", "/api/jobs", spec)["id"]
                while not stop.wait(0.2):
                    if w.api("GET", f"/api/jobs/{job_id}")["state"] in ("done", "failed", "cancelled"):
                        break
                else:
                    # A copy still running at the end would hold the shutdown until it finished.
                    try:
                        w.api("POST", f"/api/jobs/{job_id}/cancel", {})
                    except RuntimeError:
                        pass  # It finished in the moment since the last look.

        threads = [
            threading.Thread(target=repeat, args=(w.job_spec("ubuntu-24.04-server-arm64.iso", "ubuntu-24.04-server-arm64.iso"),)),
            threading.Thread(target=repeat, args=(w.job_spec("fedora-42-workstation-x86_64.iso", "fedora-42-workstation-x86_64.iso", direction="pull"),)),
        ]
        for t in threads:
            t.start()

        browser = pw.chromium.launch(executable_path=chromium_path())
        ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=2)
        page = ctx.new_page()
        page.goto(w.base)
        page.get_by_label("Password").fill(PASSWORD)
        page.get_by_role("button", name="Log in").click()
        expect(page.get_by_role("region", name="This box")).to_be_visible()
        page.get_by_label("Compare").check()

        page.wait_for_timeout(RUN_SECONDS * 1000)
        page.screenshot(path=str(out / "transfers-dark.png"))
        page.get_by_role("button", name="Use the light theme").first.click()
        page.wait_for_timeout(400)
        page.screenshot(path=str(out / "transfers-light.png"))
        state = ctx.storage_state()

        phone = browser.new_context(viewport={"width": 390, "height": 844}, device_scale_factor=2, is_mobile=True, has_touch=True, storage_state=state).new_page()
        phone.goto(w.base + "/#/transfers")
        phone.get_by_role("button", name="Use the dark theme").first.click()
        # The graph starts empty on a new page, so give it time to draw some history.
        phone.wait_for_timeout(RUN_SECONDS * 1000)
        phone.screenshot(path=str(out / "transfers-phone.png"))
        browser.close()
        stop.set()
        for t in threads:
            t.join()
    print("screenshots written to", out)


if __name__ == "__main__":
    main()
