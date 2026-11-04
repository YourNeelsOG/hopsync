"""Phase 0 benchmark matrix: runs sftp-bench sweeps plus scp/sftp baselines, writes JSONL.

Usage: python3 matrix.py <out.jsonl> host=.. port=.. user=.. key=.. known_hosts=..
       config_dir=<dir with ssh_config.<cipher> files> down_file=<remote sparse file>
       local_file=<local sparse file, same size as size=> [size=2G] [reps=3]

Each configuration runs `reps` times; every run is one JSON line so medians are computed later.
"""

import json
import subprocess
import sys
import time

CIPHERS = ["aes128-gcm@openssh.com", "aes256-gcm@openssh.com", "chacha20-poly1305@openssh.com"]
BENCH = "target/release/sftp-bench"


def system_busy_secs():
    with open("/proc/stat") as f:
        v = [int(x) for x in f.readline().split()[1:9]]
    return (sum(v) - v[3] - v[4]) / 100.0


def parse_size(s):
    mul = {"K": 1 << 10, "M": 1 << 20, "G": 1 << 30}.get(s[-1].upper(), 1)
    return int(s[:-1] if mul > 1 else s) * mul


def bench_configs():
    """Cipher sweep, in-flight depth sweep and lane sweep; duplicates are removed."""
    seen, out = set(), []
    for backend in ("russh", "openssh"):
        for d in ("up", "down"):
            sweeps = [(c, 1, 16) for c in CIPHERS]
            sweeps += [(CIPHERS[0], 1, i) for i in (1, 4, 16, 64)]
            sweeps += [(CIPHERS[0], l, 16) for l in (1, 2, 4, 8)]
            for cipher, lanes, inflight in sweeps:
                key = (backend, d, cipher, lanes, inflight)
                if key not in seen:
                    seen.add(key)
                    out.append(key)
    return out


def main():
    out_path = sys.argv[1]
    kv = dict(a.split("=", 1) for a in sys.argv[2:])
    size, reps = kv.get("size", "2G"), int(kv.get("reps", "3"))
    ssh_common = ["-F", "none", "-i", kv["key"], "-o", "IdentitiesOnly=yes",
                  "-o", "UserKnownHostsFile=" + kv["known_hosts"], "-o", "StrictHostKeyChecking=yes",
                  "-o", "BatchMode=yes"]
    target = f"{kv['user']}@{kv['host']}"

    with open(out_path, "a") as out:
        def emit(rec):
            rec["ts"] = time.strftime("%Y-%m-%dT%H:%M:%S")
            out.write(json.dumps(rec) + "\n")
            out.flush()
            print(json.dumps(rec), flush=True)

        t0, s0 = time.time(), system_busy_secs()
        time.sleep(3)
        emit({"tool": "idle", "sys_cpu_per_s": round((system_busy_secs() - s0) / (time.time() - t0), 3)})

        for backend, d, cipher, lanes, inflight in bench_configs():
            for rep in range(reps):
                args = [BENCH, f"backend={backend}", f"dir={d}", f"cipher={cipher}", f"lanes={lanes}",
                        f"inflight={inflight}", f"size={size}", f"host={kv['host']}", f"port={kv['port']}",
                        f"user={kv['user']}", f"key={kv['key']}", f"known_hosts={kv['known_hosts']}",
                        f"ssh_config={kv['config_dir']}/ssh_config.{cipher}", "zc=1",
                        f"remote={'/dev/null' if d == 'up' else kv['down_file']}"]
                r = subprocess.run(args, capture_output=True, text=True)
                if r.returncode != 0:
                    emit({"tool": backend, "dir": d, "cipher": cipher, "lanes": lanes, "inflight": inflight,
                          "rep": rep, "error": r.stderr.strip()[-300:]})
                    continue
                rec = json.loads(r.stdout.strip().splitlines()[-1])
                rec.update(tool=backend, rep=rep)
                emit(rec)

        nbytes = parse_size(size)
        for tool in ("scp", "sftp"):
            for cipher in CIPHERS:
                for d in ("up", "down"):
                    for rep in range(reps):
                        opts = ssh_common + ["-o", f"Ciphers={cipher}"]
                        if tool == "scp":
                            src, dst = ((kv["local_file"], f"{target}:/dev/null") if d == "up"
                                        else (f"{target}:{kv['down_file']}", "/dev/null"))
                            cmd, stdin = ["scp", "-q", "-P", kv["port"], *opts, src, dst], None
                        else:
                            line = (f"put {kv['local_file']} /dev/null" if d == "up"
                                    else f"get {kv['down_file']} /dev/null")
                            cmd, stdin = ["sftp", "-q", "-b", "-", "-P", kv["port"], *opts, target], line + "\n"
                        s0, t0 = system_busy_secs(), time.time()
                        r = subprocess.run(cmd, input=stdin, capture_output=True, text=True)
                        secs, cpu = time.time() - t0, system_busy_secs() - s0
                        rec = {"tool": tool, "dir": d, "cipher": cipher, "lanes": 1, "rep": rep, "bytes": nbytes,
                               "secs": round(secs, 3)}
                        if r.returncode != 0:
                            rec["error"] = (r.stderr or r.stdout).strip()[-300:]
                        else:
                            # Includes connection setup, unlike sftp-bench's transfer-only timing.
                            rec["MBps"] = round(nbytes / 1e6 / secs, 1)
                            rec["sys_cpu_s_per_GB"] = round(cpu / (nbytes / 1e9), 3)
                        emit(rec)


if __name__ == "__main__":
    main()
