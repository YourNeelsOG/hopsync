#!/usr/bin/env python3
"""Installs a generated key with an account password, through the daemon, against real OpenSSH.

Usage:
    python3 e2e/password_install.py --binary ../../../target/release/hopsync

OpenSSH cannot check passwords without root, so the server runs in a container (needs Docker). Each
server setup below offers the password by one method only. What the README promises is checked:
the password is used once and never kept, the key logs in afterwards, and other keys are untouched.
"""

import argparse
import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import time

PASSWORD = "correct-horse-battery"
USER = "seed"
OTHER_KEY = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEG8meRvoV7EyXJhuHGtXcGoPbVudsgcnhBv8fpUxVzM other"

IMAGE = "hopsync-e2e-password-sshd"
DOCKERFILE = f"""
FROM debian:12-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends openssh-server >/dev/null
RUN useradd -m -s /bin/bash {USER} && echo '{USER}:{PASSWORD}' | chpasswd && mkdir -p /run/sshd
"""

SETUPS = {
    "password method": "PasswordAuthentication yes\nKbdInteractiveAuthentication no\n",
    "keyboard-interactive only": "PasswordAuthentication no\nKbdInteractiveAuthentication yes\n",
}


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(60)
        self.sock.connect(self.path)


class Daemon:
    """`hopsync serve` on a private data directory; requests go over its owner-only socket."""

    def __init__(self, binary, root):
        self.root = root
        self.socket = os.path.join(root, "s")
        self.log = os.path.join(root, "log")
        os.mkdir(os.path.join(root, "files"))
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        self.proc = subprocess.Popen(
            [
                binary,
                "serve",
                "--listen",
                f"127.0.0.1:{port}",
                "--data-dir",
                os.path.join(root, "data"),
                "--root",
                os.path.join(root, "files"),
                "--known-hosts",
                os.path.join(root, "known_hosts"),
                "--journal",
                os.path.join(root, "journal.sqlite"),
                "--socket",
                self.socket,
            ],
            stdout=open(self.log, "w"),
            stderr=subprocess.STDOUT,
        )
        for _ in range(100):
            if os.path.exists(self.socket):
                return
            time.sleep(0.1)
        sys.exit("the daemon did not start: " + open(self.log).read())

    def call(self, method, path, body=None):
        c = UnixConnection(self.socket)
        c.request(
            method,
            path,
            None if body is None else json.dumps(body),
            {"Content-Type": "application/json", "Host": "localhost"},
        )
        r = c.getresponse()
        data = r.read().decode()
        c.close()
        return r.status, json.loads(data) if data else None

    def stop(self):
        self.proc.terminate()
        self.proc.wait(15)


def docker(*args, **kw):
    return subprocess.run(["docker", *args], check=True, capture_output=True, text=True, **kw).stdout


def host_port(container):
    for _ in range(50):
        out = docker("port", container, "22/tcp")
        port = int(out.splitlines()[0].rsplit(":", 1)[1])
        with socket.socket() as s:
            s.settimeout(1)
            try:
                s.connect(("127.0.0.1", port))
                s.settimeout(3)
                if s.recv(4).startswith(b"SSH-"):
                    return port
            except OSError:
                pass
        time.sleep(0.2)
    sys.exit("sshd did not start in the container")


def expect(ok, what):
    print(("  ok  " if ok else "FAIL  ") + what, flush=True)
    if not ok:
        sys.exit(1)


def check_setup(binary, name, config, index):
    print(name, flush=True)
    container = f"{IMAGE}-{os.getpid()}-{index}"
    docker("run", "-d", "--rm", "--name", container, "-p", "127.0.0.1::22", IMAGE, "sleep", "600")
    tmp = tempfile.TemporaryDirectory(prefix="hs-pw-")
    daemon = None
    try:
        docker(
            "exec",
            container,
            "sh",
            "-c",
            f"printf '{config}' > /etc/ssh/sshd_config.d/00-test.conf && "
            f"mkdir -m 700 /home/{USER}/.ssh && echo '{OTHER_KEY}' > /home/{USER}/.ssh/authorized_keys && "
            f"chmod 600 /home/{USER}/.ssh/authorized_keys && chown -R {USER}: /home/{USER}/.ssh && "
            "/usr/sbin/sshd",
        )
        daemon = Daemon(binary, tmp.name)
        spec = {
            "name": "box",
            "host": "127.0.0.1",
            "port": host_port(container),
            "user": USER,
            "root": f"/home/{USER}",
            "key_file": None,
            "generate_key": True,
        }
        status, server = daemon.call("POST", "/api/servers", spec)
        expect(status == 201, f"server added ({status})")
        sid = server["id"]
        status, probe = daemon.call("POST", f"/api/servers/{sid}/probe", {})
        expect(status == 200, f"host key fetched ({status})")
        real = docker("exec", container, "ssh-keygen", "-lf", "/etc/ssh/ssh_host_ed25519_key.pub")
        expect(probe["fingerprint"] == real.split()[1], "the fingerprint is the server's own")
        status, _ = daemon.call(
            "POST", f"/api/servers/{sid}/trust", {"fingerprint": probe["fingerprint"]}
        )
        expect(status == 200, f"host key pinned ({status})")

        status, _ = daemon.call(f"GET", f"/api/servers/{sid}/list")
        expect(status != 200, f"the generated key is refused before it is installed ({status})")
        install = f"/api/servers/{sid}/install-key"
        status, _ = daemon.call("POST", install, {"password": "wrong-password"})
        expect(status != 200, f"a wrong password is refused ({status})")
        status, reply = daemon.call("POST", install, {"password": PASSWORD})
        expect(status == 200, f"the account password installs the key ({status} {reply})")

        status, _ = daemon.call("GET", f"/api/servers/{sid}/list")
        expect(status == 200, f"the generated key logs in ({status})")
        status, public = daemon.call("GET", f"/api/servers/{sid}/public-key")
        keys = docker("exec", container, "cat", f"/home/{USER}/.ssh/authorized_keys")
        blob = json.dumps(public).split("ssh-ed25519 ")[1].split()[0].rstrip('"}')
        expect(keys.startswith(OTHER_KEY + "\n"), "the server's other key is untouched")
        expect(keys.count("\n") == 2 and blob in keys, "exactly one key was added")
        mode = docker("exec", container, "stat", "-c", "%a", f"/home/{USER}/.ssh/authorized_keys")
        expect(mode.strip() == "600", "authorized_keys stays private")

        status, _ = daemon.call("POST", install, {"password": PASSWORD})
        again = docker("exec", container, "cat", f"/home/{USER}/.ssh/authorized_keys")
        expect(status == 200 and again == keys, "installing twice adds nothing")

        daemon.stop()
        daemon = None
        needle = PASSWORD.encode()
        leaked = []
        for folder, _, files in os.walk(tmp.name):
            for f in files:
                p = os.path.join(folder, f)
                if os.path.isfile(p) and needle in open(p, "rb").read():
                    leaked.append(p)
        expect(not leaked, f"the password is in no log or stored file {leaked or ''}".strip())
    finally:
        if daemon:
            daemon.stop()
        subprocess.run(["docker", "rm", "-f", container], capture_output=True)
        tmp.cleanup()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="../../../target/release/hopsync")
    binary = os.path.abspath(parser.parse_args().binary)
    docker("build", "-q", "-t", IMAGE, "-", input=DOCKERFILE)
    for index, (name, config) in enumerate(SETUPS.items()):
        check_setup(binary, name, config.replace("\n", "\\n"), index)
    print("all password install checks passed")


if __name__ == "__main__":
    main()
