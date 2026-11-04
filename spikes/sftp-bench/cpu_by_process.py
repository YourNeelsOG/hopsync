"""Run a command and report CPU time by process name while it runs (no perf needed).

Usage: python3 cpu_by_process.py <command...>
Polls /proc every 20 ms for this user's processes and threads. Short-lived processes lose at most
their last poll interval. Prints, per process name: CPU seconds, and the busiest single thread's CPU
as a fraction of wall time (close to 1.0 means that thread is saturated).
"""

import os
import subprocess
import sys
import time
from collections import defaultdict

TICK = os.sysconf("SC_CLK_TCK")
UID = os.getuid()


def snapshot():
    """Returns {(pid, tid): (comm, cpu_secs)} for this user's threads."""
    out = {}
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            if os.stat(f"/proc/{pid}").st_uid != UID:
                continue
            with open(f"/proc/{pid}/comm") as f:
                comm = f.read().strip()
            for tid in os.listdir(f"/proc/{pid}/task"):
                with open(f"/proc/{pid}/task/{tid}/stat") as f:
                    fields = f.read().rsplit(") ", 1)[1].split()
                out[(pid, tid)] = (comm, (int(fields[11]) + int(fields[12])) / TICK)
        except (FileNotFoundError, ProcessLookupError, PermissionError, IndexError):
            continue
    return out


def main():
    base = snapshot()
    first, last = {}, {}
    t0 = time.time()
    proc = subprocess.Popen(sys.argv[1:])
    while proc.poll() is None:
        for k, v in snapshot().items():
            first.setdefault(k, base.get(k, (v[0], 0.0)))
            last[k] = v
        time.sleep(0.02)
    wall = time.time() - t0

    by_comm, top_thread = defaultdict(float), defaultdict(float)
    for k, (comm, cpu) in last.items():
        d = cpu - first[k][1]
        by_comm[comm] += d
        top_thread[comm] = max(top_thread[comm], d / wall)
    print(f"wall {wall:.2f}s", file=sys.stderr)
    for comm, cpu in sorted(by_comm.items(), key=lambda x: -x[1]):
        if cpu >= 0.05:
            print(f"  {comm:18} cpu {cpu:6.2f}s  busiest thread {top_thread[comm]:.2f} of wall", file=sys.stderr)
    sys.exit(proc.returncode)


if __name__ == "__main__":
    main()
