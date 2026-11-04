import json
import statistics
import sys
from collections import defaultdict


def main():
    rows = [json.loads(l) for l in open(sys.argv[1])]
    idle = next((r["sys_cpu_per_s"] for r in rows if r["tool"] == "idle"), 0.0)
    groups, errors = defaultdict(list), []
    for r in rows:
        if r["tool"] == "idle":
            continue
        if "error" in r:
            errors.append(r)
            continue
        key = (r["tool"], r["dir"], r["cipher"], r.get("lanes", 1), r.get("inflight"))
        groups[key].append(r)

    print(f"idle load: {idle} CPU-s/s (subtracted)\n")
    print("| tool | dir | cipher | lanes | in-flight | MB/s median (min-max) | sys CPU-s/GB | client CPU-s/GB | peak RSS MiB |")
    print("|---|---|---|---|---|---|---|---|---|")
    for key in sorted(groups, key=lambda k: (k[0], k[1], k[2], k[3], k[4] or 0)):
        rs = groups[key]
        mbps = [r["MBps"] for r in rs]
        gb = [r["bytes"] / 1e9 for r in rs]
        sys_cpu = statistics.median(r["sys_cpu_s_per_GB"] - idle * r["secs"] / g for r, g in zip(rs, gb))
        client = [r["self_cpu_s_per_GB"] for r in rs if "self_cpu_s_per_GB" in r]
        rss = [r["peak_rss_MiB"] for r in rs if "peak_rss_MiB" in r]
        tool, d, cipher, lanes, inflight = key
        print(f"| {tool} | {d} | {cipher.split('@')[0]} | {lanes} | {inflight if inflight else '-'} "
              f"| {statistics.median(mbps):.0f} ({min(mbps):.0f}-{max(mbps):.0f}) | {sys_cpu:.2f} "
              f"| {statistics.median(client):.2f} | {max(rss):.1f} |"
              if client else
              f"| {tool} | {d} | {cipher.split('@')[0]} | {lanes} | - "
              f"| {statistics.median(mbps):.0f} ({min(mbps):.0f}-{max(mbps):.0f}) | {sys_cpu:.2f} | - | - |")
    for e in errors:
        print(f"\nERROR {e['tool']} {e['dir']} {e['cipher']} rep {e['rep']}: {e['error']}")


if __name__ == "__main__":
    main()
