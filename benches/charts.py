#!/usr/bin/env python3
"""Draws the README's benchmark charts as SVG from docs/benchmarks/results.json.

    python3 benches/charts.py             # redraw the charts from results.json
    python3 benches/charts.py --from-raw  # rebuild results.json first, from the raw bench.py rows

Raw rows from real servers name the host and stay out of the repository (.gitignore).
results.json holds medians only, so it can be public. Python standard library only.
"""

import argparse
import json
import statistics
from collections import defaultdict
from html import escape
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "docs/benchmarks/results.json"
OUT = ROOT / "docs/assets/charts"
RAW = {
    "loopback": ROOT / "docs/phase1/results/loopback-2026-11-04.jsonl",
    "real_link": ROOT / "docs/phase1/results/wan-2026-11-04.jsonl",
}
TOOLS = ["hopsync", "scp", "sftp"]

# Checked with the dataviz palette validator against both surfaces below. Values are also written
# on every bar, so identity never depends on color alone.
STYLE = """
.bg{fill:#ffffff}.title{fill:#0b0b0b}.text{fill:#52514e}.base{stroke:#c3c2b7}
.hopsync{fill:#2a78d6}.scp{fill:#eb6834}.sftp{fill:#1baf7a}
@media (prefers-color-scheme:dark){
.bg{fill:#0d1117}.title{fill:#f0efec}.text{fill:#c3c2b7}.base{stroke:#3d444d}
.hopsync{fill:#3987e5}.scp{fill:#d95926}.sftp{fill:#199e70}}
text{font-family:ui-sans-serif,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}
"""


def from_raw():
    """Medians per scenario and tool. Keeps no host name and no timestamps."""
    out = {}
    for link, path in RAW.items():
        groups = defaultdict(list)
        for line in path.read_text().splitlines():
            row = json.loads(line)
            groups[(row["scenario"], row["tool"])].append(row)
        scenarios = defaultdict(dict)
        runs = set()
        for (scenario, tool), rows in sorted(groups.items()):
            secs = statistics.median(r["secs"] for r in rows)
            runs.add(len(rows))
            scenarios[scenario][tool] = {
                "secs": round(secs, 3),
                "mb_per_s": round(rows[0]["bytes"] / secs / 1e6, 1),
                "peak_mib": round(statistics.median(r["maxrss_kib"] for r in rows) / 1024, 1),
            }
        out[link] = {"runs": max(runs), "scenarios": scenarios}
    RESULTS.parent.mkdir(parents=True, exist_ok=True)
    RESULTS.write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")


def bar(x, y, width, height, radius=4):
    """A bar square at the baseline with a rounded data end."""
    r = min(radius, width)
    return (
        f"M{x:.1f} {y:.1f}H{x + width - r:.1f}Q{x + width:.1f} {y:.1f} {x + width:.1f} {y + r:.1f}"
        f"V{y + height - r:.1f}Q{x + width:.1f} {y + height:.1f} {x + width - r:.1f} {y + height:.1f}"
        f"H{x:.1f}Z"
    )


def chart(name, title, subtitle, groups, unit, decimals=1):
    """Grouped horizontal bars on one axis. `groups` is [(label, {tool: value})]."""
    width, left, right = 760, 112, 96
    bar_h, gap, group_gap, top = 18, 2, 18, 100
    plot = width - left - right
    top_value = max(v for _, values in groups for v in values.values())
    group_h = len(TOOLS) * bar_h + (len(TOOLS) - 1) * gap
    height = top + len(groups) * (group_h + group_gap) - group_gap + 18

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height}" width="{width}" '
        f'role="img" aria-labelledby="t d"><title id="t">{escape(title)}</title>'
        f'<desc id="d">{escape(subtitle)}</desc><style>{STYLE}</style>',
        f'<rect class="bg" width="{width}" height="{height}" rx="8"/>',
        f'<text class="title" x="20" y="32" font-size="17" font-weight="600">{escape(title)}</text>',
        f'<text class="text" x="20" y="54" font-size="13">{escape(subtitle)}</text>',
    ]
    x = 20
    for tool in TOOLS:
        parts.append(f'<rect class="{tool}" x="{x}" y="69" width="11" height="11" rx="2"/>')
        parts.append(f'<text class="text" x="{x + 16}" y="79" font-size="13">{tool}</text>')
        # Roughly the label's width, so the gaps look even without measuring text.
        x += 16 + len(tool) * 8 + 18

    y = top
    for label, values in groups:
        parts.append(
            f'<text class="title" x="{left - 12}" y="{y + group_h / 2 + 5}" font-size="13" '
            f'text-anchor="end">{escape(label)}</text>'
        )
        for i, tool in enumerate(TOOLS):
            value = values[tool]
            w = max(2, value / top_value * plot)
            by = y + i * (bar_h + gap)
            shown = f"{value:,.{decimals}f} {unit}"
            parts.append(f'<path class="{tool}" d="{bar(left, by, w, bar_h)}"><title>{tool}: {shown}</title></path>')
            weight = ' font-weight="600"' if tool == "hopsync" else ""
            parts.append(
                f'<text class="{"title" if tool == "hopsync" else "text"}" x="{left + w + 6:.1f}" '
                f'y="{by + 13.5}" font-size="12"{weight}>{shown}</text>'
            )
        y += group_h + group_gap
    baseline_end = top + len(groups) * (group_h + group_gap) - group_gap
    parts.append(f'<line class="base" x1="{left}" x2="{left}" y1="{top - 4}" y2="{baseline_end + 4}"/>')
    parts.append("</svg>\n")
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / f"{name}.svg").write_text("".join(parts))


def pick(link, field, *scenarios):
    data = link["scenarios"]
    return [(label, {t: data[key][t][field] for t in TOOLS}) for label, key in scenarios]


def draw(results):
    wan, loop = results["real_link"], results["loopback"]
    wan_runs = f"Median of {wan['runs']} runs"
    loop_runs = f"Median of {loop['runs']} runs"
    files = (("Upload", "push-file"), ("Download", "pull-file"))
    trees = (("Upload", "push-tree"), ("Download", "pull-tree"))
    chart(
        "real-link-big-file",
        "One big file over a 200 ms link",
        f"256 MiB file, to and from a shared seedbox. {wan_runs}. Higher is better.",
        pick(wan, "mb_per_s", *files),
        "MB/s",
    )
    chart(
        "real-link-small-files",
        "300 small files over a 200 ms link",
        f"Files of 0.5 to 0.8 KiB, same seedbox. {wan_runs}. Lower is better.",
        pick(wan, "secs", *trees),
        "s",
        decimals=0,
    )
    chart(
        "loopback-big-file",
        "One big file on loopback",
        f"512 MiB file on tmpfs, so this measures CPU only. {loop_runs}. Higher is better.",
        pick(loop, "mb_per_s", *files),
        "MB/s",
        decimals=0,
    )
    chart(
        "loopback-small-files",
        "2,000 small files on loopback",
        f"Files of 0.5 to 4.5 KiB on tmpfs. {loop_runs}. Lower is better.",
        pick(loop, "secs", *trees),
        "s",
        decimals=2,
    )
    chart(
        "memory",
        "Peak client memory over the 200 ms link",
        f"Parallel lanes cost memory on big files, up to a fixed limit. {wan_runs}. Lower is better.",
        pick(wan, "peak_mib", ("Big file up", "push-file"), ("Big file down", "pull-file"), ("Small files up", "push-tree")),
        "MiB",
        decimals=1,
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--from-raw", action="store_true")
    if ap.parse_args().from_raw:
        from_raw()
    draw(json.loads(RESULTS.read_text()))


if __name__ == "__main__":
    main()
