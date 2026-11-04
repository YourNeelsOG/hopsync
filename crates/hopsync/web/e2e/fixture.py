"""A throwaway Hopsync world for browser tests: real sshd, real daemon, seeded folders.

Nothing outside the temporary directory is read or changed. `World` stops both processes on exit.
Used by e2e.py, which needs the Python `playwright` package and an OpenSSH server.
"""

import getpass
import json
import os
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

PASSWORD = "correct-horse-battery-staple"
HOSTILE_NAME = 'we<b>ird & "name".txt'
SFTP_SERVERS = [
    "/usr/lib/openssh/sftp-server",
    "/usr/libexec/openssh/sftp-server",
    "/usr/lib/ssh/sftp-server",
    "/usr/libexec/sftp-server",
]


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_for_port(port, proc, log):
    for _ in range(100):
        if proc.poll() is not None:
            raise RuntimeError(f"process exited early:\n{Path(log).read_text()}")
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return
        time.sleep(0.05)
    raise RuntimeError(f"nothing listening on {port}:\n{Path(log).read_text()}")


def sparse(path, size):
    """A file of `size` bytes that uses no disk until something writes a copy of it."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "wb") as f:
        f.truncate(size)


def seed(local, remote):
    """Folders that show every row style: dirs, sizes, symlinks, hostile names, and differences."""
    gib = 1024**3
    mib = 1024**2
    for name, size in [
        ("debian-13.1-amd64-DVD-1.iso", 4 * gib),
        ("debian-13.1-amd64-DVD-2.iso", 8 * gib),
        ("debian-13.1-arm64-DVD-1.iso", 4 * gib + 300 * mib),
    ]:
        sparse(local / "Images/debian" / name, size)
    sparse(local / "Images/ubuntu/ubuntu-24.04-desktop-amd64.iso", 6 * gib)
    sparse(local / "Datasets/wikipedia-en-2026-10.xml.zst", 42 * gib)
    sparse(local / "Datasets/openstreetmap-planet.osm.pbf", 9 * gib)
    # Big enough to watch live speed for a few seconds; sparse until the copy writes it.
    sparse(local / "transfer.bin", 3 * gib)
    (local / "Empty folder").mkdir()
    (local / "notes.txt").write_text("season pack notes\n")
    (local / "zero-byte.txt").write_text("")
    (local / HOSTILE_NAME).write_text("a hostile name, shown as text\n")
    (local / "link-to-notes").symlink_to("notes.txt")
    for i in range(30):
        (local / "Many" / "sub").mkdir(parents=True, exist_ok=True)
        (local / "Many" / f"file-{i:03}.dat").write_text("x" * (i + 1))
    (local / "Huge").mkdir()
    for i in range(20_000):
        (local / "Huge" / f"file-{i:05}").touch()

    sparse(remote / "Datasets/openstreetmap-planet.osm.pbf", 9 * gib)
    sparse(remote / "Datasets/common-voice-corpus.tar", 12 * gib)
    sparse(remote / "Archive/backup-2025.tar", 80 * gib)
    (remote / "notes.txt").write_text("a different copy of the notes\n")
    (remote / "only-on-server.txt").write_text("remote only\n")


class World:
    def __init__(self, binary):
        self.binary = str(binary)
        self.root = Path(tempfile.mkdtemp(prefix="hopsync-e2e-"))
        self.local = self.root / "local"
        self.remote = self.root / "remote"
        self.local.mkdir()
        self.remote.mkdir()
        seed(self.local, self.remote)
        self.procs = []
        self.cookie = None
        self.server_id = None

    def __enter__(self):
        try:
            self.start_sshd()
            self.start_daemon()
        except BaseException:
            self.__exit__()
            raise
        return self

    def __exit__(self, *exc):
        for p in self.procs:
            p.terminate()
        for p in self.procs:
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()
        shutil.rmtree(self.root, ignore_errors=True)

    def keygen(self, path):
        subprocess.run(
            ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(path)], check=True
        )

    def start_sshd(self):
        d = self.root / "sshd"
        d.mkdir()
        self.keygen(d / "host")
        self.keygen(d / "client")
        self.client_key = d / "client"
        shutil.copy(d / "client.pub", d / "authorized_keys")
        self.ssh_port = free_port()
        sftp = next(p for p in SFTP_SERVERS if os.path.exists(p))
        (d / "config").write_text(
            f"ListenAddress 127.0.0.1:{self.ssh_port}\nHostKey {d}/host\nPidFile {d}/pid\n"
            f"AuthorizedKeysFile {d}/authorized_keys\nPubkeyAuthentication yes\n"
            "PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\n"
            f"StrictModes no\nAllowUsers {getpass.getuser()}\nSubsystem sftp {sftp}\n"
        )
        sshd = shutil.which("sshd") or "/usr/sbin/sshd"
        log = d / "log"
        proc = subprocess.Popen(
            [sshd, "-D", "-e", "-f", str(d / "config")],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=open(log, "w"),
        )
        self.sshd = proc
        self.sshd_argv = [sshd, "-D", "-e", "-f", str(d / "config")]
        self.sshd_dir = d
        self.procs.append(proc)
        wait_for_port(self.ssh_port, proc, log)

    def start_daemon(self):
        d = self.root / "daemon"
        d.mkdir(mode=0o700)
        self.data_dir = d
        self.http_port = free_port()
        self.base = f"http://127.0.0.1:{self.http_port}"
        self.spawn_daemon()

    def spawn_daemon(self):
        d = self.data_dir
        log = d / "log"
        proc = subprocess.Popen(
            [
                self.binary,
                "serve",
                "--listen",
                f"127.0.0.1:{self.http_port}",
                "--data-dir",
                str(d),
                "--root",
                str(self.local),
                "--known-hosts",
                str(d / "known_hosts"),
                "--journal",
                str(d / "journal.sqlite"),
                "--socket",
                str(d / "hopsync.sock"),
            ],
            stdin=subprocess.DEVNULL,
            stdout=open(log, "a"),
            stderr=subprocess.STDOUT,
            # An empty PATH: the UI must come from the binary alone, with no Node or npm around.
            env={"HOME": str(d), "PATH": ""},
        )
        self.daemon = proc
        self.procs.append(proc)
        wait_for_port(self.http_port, proc, log)

    def restart_daemon(self, down_for=4.0):
        """Stops the daemon, waits, and starts it again on the same port and data directory."""
        self.daemon.terminate()
        self.daemon.wait(15)
        self.procs.remove(self.daemon)
        time.sleep(down_for)
        self.spawn_daemon()

    def rotate_host_key(self):
        """Restarts sshd with a new host key, as a reinstalled server or an attacker would have."""
        self.sshd.terminate()
        self.sshd.wait(10)
        self.procs.remove(self.sshd)
        for f in ("host", "host.pub"):
            (self.sshd_dir / f).unlink()
        self.keygen(self.sshd_dir / "host")
        log = self.sshd_dir / "log"
        self.sshd = subprocess.Popen(
            self.sshd_argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=open(log, "a"),
        )
        self.procs.append(self.sshd)
        wait_for_port(self.ssh_port, self.sshd, log)

    def setup_token(self):
        return (self.data_dir / "setup-token").read_text().strip()

    def api(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base + path, data=data, method=method)
        req.add_header("Origin", self.base)
        if data is not None:
            req.add_header("Content-Type", "application/json")
        if self.cookie:
            req.add_header("Cookie", self.cookie)
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                cookie = r.headers.get("Set-Cookie")
                if cookie:
                    self.cookie = cookie.split(";", 1)[0]
                text = r.read().decode()
                return json.loads(text) if text else None
        except urllib.error.HTTPError as e:
            raise RuntimeError(f"{method} {path}: {e.code} {e.read().decode()}") from e

    def finish_setup(self):
        """Sets the password and returns with a session, as if the setup page had been used."""
        self.api("POST", "/api/setup", {"token": self.setup_token(), "password": PASSWORD})
        if self.cookie is None:
            self.api("POST", "/api/login", {"password": PASSWORD})

    def add_server(self, name="seedbox"):
        spec = {
            "name": name,
            "host": "127.0.0.1",
            "port": self.ssh_port,
            "user": getpass.getuser(),
            "root": str(self.remote),
            "key_file": str(self.client_key),
            "min_free": 0,
        }
        s = self.api("POST", "/api/servers", spec)
        probe = self.api("POST", f"/api/servers/{s['id']}/probe", {})
        self.api(
            "POST",
            f"/api/servers/{s['id']}/trust",
            {"fingerprint": probe["fingerprint"], "replace": False},
        )
        self.server_id = s["id"]
        return s

    def job_spec(self, src, dst, direction="push", existing="overwrite"):
        return {
            "direction": direction,
            "server": self.server_id,
            "src": src,
            "dst": dst,
            "existing": existing,
            "verify": False,
        }

    def wait_job(self, job_id, states=("done", "failed", "cancelled"), timeout=60):
        end = time.time() + timeout
        while time.time() < end:
            job = self.api("GET", f"/api/jobs/{job_id}")
            if job["state"] in states:
                return job
            time.sleep(0.05)
        raise RuntimeError(f"job {job_id} stuck: {job}")
