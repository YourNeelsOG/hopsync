#!/usr/bin/env python3
"""Hopsync release benchmark: hopsync against scp and sftp on the same server and data.

Usage:
  python3 benches/bench.py --host H --port P --user U --key KEY \\
      --known-hosts OPENSSH_KNOWN_HOSTS --hopsync-known-hosts HOPSYNC_KNOWN_HOSTS \\
      --local-dir DIR --remote-dir DIR [--size 1G] [--files 2000] [--reps 3] \\
      [--hopsync target/release/hopsync] [--only push-file,...] [--out results.jsonl]

Scenarios: one big file and a tree of small files, each pushed and pulled. Every run starts from an
empty destination and is one JSON line in --out (wall seconds, bytes, peak RSS of the client
process tree as reported by wait4). The summary printed at the end is the median per tool.

Data lives in --local-dir and --remote-dir; put both on tmpfs to measure protocol and CPU cost, or
on disks to measure a real setup. Pushes overwrite the same remote destination, which is emptied
with sftp before and after the run, so the script never runs a remote shell command. The seeded
source data is removed at the end too, unless --keep-seed is given. Needs only the
Python standard library and the OpenSSH client.
"""

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

SCENARIOS = ["push-file", "pull-file", "push-tree", "pull-tree"]


def parse_size(s):
    mul = {"K": 1 << 10, "M": 1 << 20, "G": 1 << 30}.get(s[-1].upper(), 1)
    return int(s[:-1] if mul > 1 else s) * mul


def tree_pids(pid):
    """pid and all its descendants, from /proc/<pid>/task/*/children."""
    out, todo = [], [pid]
    while todo:
        p = todo.pop()
        out.append(p)
        try:
            for task in os.listdir(f"/proc/{p}/task"):
                with open(f"/proc/{p}/task/{task}/children") as f:
                    todo.extend(int(c) for c in f.read().split())
        except OSError:
            pass
    return out


def hwm_kib(pid):
    """Peak RSS of one process since its exec (VmHWM), or None once it is gone."""
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1])
    except OSError:
        return None
    return None


def run(argv, **kw):
    """Runs argv to completion; returns (seconds, peak RSS in KiB summed over its process tree).

    RSS is sampled from /proc every 10 ms: wait4's ru_maxrss would also count the Python process
    that forked the child, since Linux keeps the high-water mark across exec."""
    start = time.monotonic()
    proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, **kw)
    peaks = {}
    while proc.poll() is None:
        for p in tree_pids(proc.pid):
            h = hwm_kib(p)
            if h is not None:
                peaks[p] = max(peaks.get(p, 0), h)
        time.sleep(0.01)
    secs = time.monotonic() - start
    err = proc.stderr.read().decode(errors="replace")
    proc.stderr.close()
    if proc.returncode != 0:
        sys.exit(f"{argv[0]} failed ({proc.returncode}): {err.strip()}")
    return secs, sum(peaks.values())


class Bench:
    def __init__(self, a):
        self.a = a
        self.ssh = [
            "-F", "none", "-i", a.key, "-o", "IdentitiesOnly=yes",
            "-o", f"UserKnownHostsFile={a.known_hosts}", "-o", "StrictHostKeyChecking=yes",
            "-o", "BatchMode=yes",
        ]
        self.dest = f"{a.user}@{a.host}"

    def sftp_batch(self, commands):
        """Runs sftp batch commands; a leading '-' lets one fail (rm of something missing)."""
        subprocess.run(
            ["sftp", "-P", str(self.a.port), *self.ssh, "-b", "-", self.dest],
            input="\n".join(commands).encode(), check=True,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )

    def remote(self, name):
        return f"{self.a.remote_dir}/{name}"

    def clear_remote(self):
        # sftp has no recursive rm: the tree is one level deep by construction.
        n = self.a.files
        cmds = ["-rm " + self.remote("big"), "-rm " + self.remote("big.hopsync-part")]
        cmds += [f"-rm {self.remote('tree')}/f{i}" for i in range(n)]
        cmds += ["-rmdir " + self.remote("tree")]
        self.sftp_batch(cmds)

    def clear_seed(self):
        """Removes the seeded source data and the run's directories from the server."""
        n = self.a.files
        cmds = ["-rm " + self.remote("src/big")]
        cmds += [f"-rm {self.remote('src/tree')}/f{i}" for i in range(n)]
        cmds += ["-rmdir " + self.remote("src/tree"), "-rmdir " + self.remote("src"),
                 "-rmdir " + self.a.remote_dir]
        self.sftp_batch(cmds)

    def clear_local(self, name):
        p = os.path.join(self.a.local_dir, name)
        if os.path.isdir(p):
            shutil.rmtree(p)
        elif os.path.exists(p):
            os.remove(p)

    def seed(self):
        """Source data: a random big file and a flat tree of small files, both sides."""
        size, n = parse_size(self.a.size), self.a.files
        src = os.path.join(self.a.local_dir, "src")
        os.makedirs(os.path.join(src, "tree"), exist_ok=True)
        big = os.path.join(src, "big")
        if not os.path.exists(big) or os.path.getsize(big) != size:
            with open(big, "wb") as f:
                left = size
                while left:
                    chunk = os.urandom(min(left, 8 << 20))
                    f.write(chunk)
                    left -= len(chunk)
        for i in range(n):
            p = os.path.join(src, "tree", f"f{i}")
            if not os.path.exists(p):
                with open(p, "wb") as f:
                    f.write(os.urandom(512 + i % 4096))
        self.clear_remote()
        self.sftp_batch(
            [f"-mkdir {self.a.remote_dir}", f"-mkdir {self.a.remote_dir}/src",
             f"put {big} {self.remote('src/big')}", f"-mkdir {self.remote('src/tree')}",
             f"put {src}/tree/* {self.remote('src/tree')}/"]
        )

    def hopsync(self, cmd, src, dst):
        return [
            self.a.hopsync, "--known-hosts", self.a.hopsync_known_hosts, cmd,
            "-p", str(self.a.port), "-i", self.a.key, "--no-resume", "--overwrite", src, dst,
        ]

    def commands(self, scenario):
        """(tool, argv) for each tool in one scenario."""
        L, R = self.a.local_dir, self.a.remote_dir
        at = f"{self.dest}:"
        scp = ["scp", "-q", "-P", str(self.a.port), *self.ssh]
        sftp = ["sftp", "-q", "-P", str(self.a.port), *self.ssh, "-b"]
        out = {
            "push-file": [
                ("hopsync", self.hopsync("push", f"{L}/src/big", f"{at}{R}/big")),
                ("scp", [*scp, f"{L}/src/big", f"{at}{R}/big"]),
                ("sftp", [*sftp, f"put {L}/src/big {R}/big"]),
            ],
            "pull-file": [
                ("hopsync", self.hopsync("pull", f"{at}{R}/src/big", f"{L}/big")),
                ("scp", [*scp, f"{at}{R}/src/big", f"{L}/big"]),
                ("sftp", [*sftp, f"get {R}/src/big {L}/big"]),
            ],
            "push-tree": [
                ("hopsync", self.hopsync("push", f"{L}/src/tree", f"{at}{R}/")),
                ("scp", [*scp, "-r", f"{L}/src/tree", f"{at}{R}/"]),
                ("sftp", [*sftp, f"put -r {L}/src/tree {R}/"]),
            ],
            "pull-tree": [
                ("hopsync", self.hopsync("pull", f"{at}{R}/src/tree", f"{L}/")),
                ("scp", [*scp, "-r", f"{at}{R}/src/tree", f"{L}/"]),
                ("sftp", [*sftp, f"get -r {R}/src/tree {L}/"]),
            ],
        }
        return out[scenario]

    def bytes_for(self, scenario):
        if scenario.endswith("file"):
            return parse_size(self.a.size)
        tree = os.path.join(self.a.local_dir, "src", "tree")
        return sum(os.path.getsize(os.path.join(tree, f)) for f in os.listdir(tree))

    def once(self, scenario, tool, argv):
        # Remote destinations are cleared only before and after the whole run: every tool rewrites
        # existing files in full (hopsync runs with --overwrite), and deleting a tree over SFTP
        # costs one round trip per file, minutes on a distant server.
        if scenario.startswith("pull"):
            self.clear_local("big")
            self.clear_local("tree")
        if tool == "sftp":
            # sftp -b reads its commands from a file.
            batch = os.path.join(self.a.local_dir, "batch")
            with open(batch, "w") as f:
                f.write(argv[-1] + "\n")
            argv = [*argv[:-1], batch, self.dest]
        return run(argv)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ("host", "user", "key", "known-hosts", "hopsync-known-hosts", "local-dir", "remote-dir"):
        p.add_argument(f"--{name}", required=True)
    p.add_argument("--port", type=int, default=22)
    p.add_argument("--size", default="1G")
    p.add_argument("--files", type=int, default=2000)
    p.add_argument("--reps", type=int, default=3)
    p.add_argument("--hopsync", default="target/release/hopsync")
    p.add_argument("--only", default=",".join(SCENARIOS))
    p.add_argument("--out", default="bench.jsonl")
    p.add_argument("--keep-seed", action="store_true",
                   help="leave the seeded source data on the server for another run")
    a = p.parse_args()

    b = Bench(a)
    b.seed()
    results = []
    with open(a.out, "a") as out:
        for scenario in a.only.split(","):
            size = b.bytes_for(scenario)
            for rep in range(a.reps):
                for tool, argv in b.commands(scenario):
                    secs, rss = b.once(scenario, tool, argv)
                    row = {
                        "scenario": scenario, "tool": tool, "rep": rep, "bytes": size,
                        "secs": round(secs, 3), "maxrss_kib": rss, "host": a.host,
                        "at": time.strftime("%Y-%m-%dT%H:%M:%S"),
                    }
                    out.write(json.dumps(row) + "\n")
                    out.flush()
                    results.append(row)
                    print(f"{scenario:10} {tool:8} rep {rep}: {secs:7.2f} s  "
                          f"{size / secs / 1e6:8.1f} MB/s  {rss / 1024:6.1f} MiB",
                          file=sys.stderr)
    b.clear_remote()
    if not a.keep_seed:
        b.clear_seed()

    print("| Scenario | Tool | Median s | MB/s | Peak RSS MiB |")
    print("|---|---|---|---|---|")
    for scenario in a.only.split(","):
        for tool in ("hopsync", "scp", "sftp"):
            rows = [r for r in results if r["scenario"] == scenario and r["tool"] == tool]
            if not rows:
                continue
            secs = statistics.median(r["secs"] for r in rows)
            rss = max(r["maxrss_kib"] for r in rows) / 1024
            print(f"| {scenario} | {tool} | {secs:.2f} | {rows[0]['bytes'] / secs / 1e6:.1f} | {rss:.1f} |")


if __name__ == "__main__":
    main()
