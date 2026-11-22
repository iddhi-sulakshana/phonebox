"""Turns sample.py's CSVs into summary.json and the release-notes table of survey-c 4.4. Standard library only.

    python3 -I tools/resources/report.py --out-dir DIR run1/samples.csv run2/samples.csv \
        --label cs="C# 0.9.23" --label rs="Rust 1.0.0" --installed-size cs=BYTES --installed-size rs=BYTES \
        --download-size cs=BYTES --download-size rs=BYTES [--platform macos|linux] [--title TEXT] [--note TEXT]

Measured are the scenarios S0 (idle), S1 (phone connected) and S2 (every page cycling); the warm-up is not. CPU is a
percentage of one core with the child tools included: the mean of a scenario comes from the cumulative counters,
p50/p95/max from 5 s sub-windows. Memory, resident memory and threads are taken over every measured sample, wake-ups
per second over the measured span, disk writes as the growth of the write counter over the measured span of one run.
Every run in the arguments is pooled.

Pass rule: on every axis the candidate's median over all runs (for CPU and wake-ups the median 5 s rate) is below the
baseline's. An axis within 5% either way is flagged "rerun" (rerun it before calling it); both zero is a "tie", which
does not block a pass; an axis without data on either side is "n/a".
"""
import argparse
import csv
import json
import sys
from pathlib import Path

MEASURED = ("S0", "S1", "S2")
TOLERANCE = 0.05
WINDOW = 5.0
INTS = ("footprint", "rss", "threads", "disk_r", "disk_w", "idle_wkups")


# ---------------------------------------------------------------- statistics


def median(values):
    v = sorted(values)
    if not v:
        return None
    mid = len(v) // 2
    return v[mid] if len(v) % 2 else (v[mid - 1] + v[mid]) / 2


def percentile(values, p):
    """Linear interpolation between closest ranks (numpy's default)."""
    v = sorted(values)
    if not v:
        return None
    k = (len(v) - 1) * p / 100
    lo = int(k)
    hi = min(lo + 1, len(v) - 1)
    return v[lo] + (v[hi] - v[lo]) * (k - lo)


def _cpu(row):
    return row["cpu_s"] + (row["child_cpu_s"] or 0)


def cpu_mean(rows):
    """Mean CPU in % of one core over a run of samples, from the first and last cumulative counters."""
    if len(rows) < 2 or rows[-1]["t"] <= rows[0]["t"]:
        return None
    return (_cpu(rows[-1]) - _cpu(rows[0])) / (rows[-1]["t"] - rows[0]["t"]) * 100


def rate_windows(rows, value, window=WINDOW):
    """Rates of a cumulative counter over consecutive whole windows of `window` seconds (a restarted counter's
    window is dropped)."""
    out, start = [], 0
    for i in range(1, len(rows)):
        dt = rows[i]["t"] - rows[start]["t"]
        if dt >= window - 1e-6:
            dv = value(rows[i]) - value(rows[start])
            if dv >= 0:
                out.append(dv / dt)
            start = i
    return out


def cpu_windows(rows, window=WINDOW):
    return [r * 100 for r in rate_windows(rows, _cpu, window)]


def verdict(base, cand, tolerance=TOLERANCE):
    if base is None or cand is None:
        return "n/a"
    if base == 0 and cand == 0:
        return "tie"
    if abs(cand - base) <= tolerance * abs(base):
        return "rerun"
    return "pass" if cand < base else "fail"


# ---------------------------------------------------------------- loading and summarizing


def load_rows(path):
    out = []
    with open(path, newline="") as f:
        for r in csv.DictReader(f):
            row = {"t": float(r["t"]), "agent": r["agent"], "scenario": r["scenario"],
                   "cpu_s": float(r["cpu_s"]), "child_cpu_s": float(r["child_cpu_s"] or 0)}
            for k in INTS:
                row[k] = None if r[k] == "" else int(float(r[k]))
            out.append(row)
    return out


def _segments(rows, agent, scenario):
    seg = [r for r in rows if r["agent"] == agent and r["scenario"] == scenario]
    return sorted(seg, key=lambda r: r["t"])


def _step(rows):
    steps = [b["t"] - a["t"] for a, b in zip(rows, rows[1:])]
    return median(steps) or 0


def summarize(runs, baseline="cs", candidate="rs", sizes=None):
    """runs: one list of sample rows per run. sizes: {"installed": {agent: bytes}, "download": {agent: bytes}}."""
    sizes = sizes or {}
    agents = {}
    for agent in (baseline, candidate):
        a = {}
        for sc in MEASURED:
            cpu_sum = dt_sum = 0.0
            windows, per_run = [], []
            for run in runs:
                seg = _segments(run, agent, sc)
                if len(seg) < 2:
                    continue
                cpu_sum += _cpu(seg[-1]) - _cpu(seg[0])
                dt_sum += seg[-1]["t"] - seg[0]["t"]
                windows += cpu_windows(seg)
                per_run.append(cpu_mean(seg))
            a[sc] = {"cpu_mean": cpu_sum / dt_sum * 100 if dt_sum else None, "cpu_p50": median(windows),
                     "cpu_p95": percentile(windows, 95), "cpu_max": max(windows) if windows else None,
                     "cpu_mean_per_run": per_run}
        samples, wk_windows, writes, minutes = [], [], [], []
        wk_sum = wk_dt = 0.0
        for run in runs:
            seg = sorted((r for r in run if r["agent"] == agent and r["scenario"] in MEASURED), key=lambda r: r["t"])
            if not seg:
                continue
            samples += seg
            span = seg[-1]["t"] - seg[0]["t"]
            minutes.append((span + _step(seg)) / 60)
            writes.append(seg[-1]["disk_w"] - seg[0]["disk_w"])
            if seg[0]["idle_wkups"] is not None and span > 0:
                wk_sum += seg[-1]["idle_wkups"] - seg[0]["idle_wkups"]
                wk_dt += span
                for sc in MEASURED:
                    wk_windows += rate_windows(_segments(run, agent, sc), lambda r: r["idle_wkups"])
        col = lambda k: [r[k] for r in samples if r[k] is not None]
        a["measured"] = {
            "samples": len(samples),
            "footprint_median": median(col("footprint")), "footprint_peak": max(col("footprint"), default=None),
            "rss_median": median(col("rss")), "rss_peak": max(col("rss"), default=None),
            "threads_median": median(col("threads")), "threads_peak": max(col("threads"), default=None),
            "idle_wkups_per_s": wk_sum / wk_dt if wk_dt else None, "idle_wkups_p50": median(wk_windows),
            "disk_w": median(writes), "disk_w_per_run": writes,
            "minutes": median(minutes),
            "installed": (sizes.get("installed") or {}).get(agent),
            "download": (sizes.get("download") or {}).get(agent),
        }
        agents[agent] = a

    def axis(name, value):
        b, c = value(agents[baseline]), value(agents[candidate])
        return {"axis": name, "baseline": b, "candidate": c, "verdict": verdict(b, c)}

    axes = [axis(f"cpu_{sc}", lambda a, sc=sc: a[sc]["cpu_p50"]) for sc in MEASURED]
    axes += [axis("footprint", lambda a: a["measured"]["footprint_median"]),
             axis("rss", lambda a: a["measured"]["rss_median"]),
             axis("threads", lambda a: a["measured"]["threads_median"]),
             axis("wakeups", lambda a: a["measured"]["idle_wkups_p50"]),
             axis("disk_writes", lambda a: a["measured"]["disk_w"]),
             axis("installed", lambda a: a["measured"]["installed"]),
             axis("download", lambda a: a["measured"]["download"])]
    verdicts = {a["verdict"] for a in axes}
    overall = "fail" if "fail" in verdicts else "rerun" if "rerun" in verdicts else "pass"
    return {"baseline": baseline, "candidate": candidate, "runs": len(runs), "agents": agents, "axes": axes,
            "verdict": overall}


# ---------------------------------------------------------------- the table


def _mb(v):
    return "—" if v is None else f"{v / 1e6:.1f} MB"


def _bytes(v):
    if v is None:
        return "—"
    return f"{v / 1e3:.1f} kB" if v < 1e6 else f"{v / 1e6:.1f} MB"


def _pct(v):
    return "—" if v is None else f"{v:.1f}%"


def _num(v):
    return "—" if v is None else f"{v:g}"


def _change(b, c):
    if b is None or c is None or b == 0:
        return "—"
    return f"{(c - b) / b * 100:+.0f}%".replace("+-", "-")


def render_table(summary, title, labels, platform="macos", download_label="Download (zip)"):
    base, cand = summary["baseline"], summary["candidate"]
    A = summary["agents"]
    rows = []

    def row(name, fmt, primary):
        b, c = A[base], A[cand]
        rows.append(f"| {name} | {fmt(b)} | {fmt(c)} | {_change(primary(b), primary(c))} |")

    row("CPU, idle (mean % of one core; with child tools)", lambda a: _pct(a["S0"]["cpu_mean"]),
        lambda a: a["S0"]["cpu_mean"])
    row("CPU, phone connected (mean / p95)", lambda a: f"{_pct(a['S1']['cpu_mean'])} / {_pct(a['S1']['cpu_p95'])}",
        lambda a: a["S1"]["cpu_mean"])
    row("CPU, all pages cycling (mean / p95)",
        lambda a: f"{_pct(a['S2']['cpu_mean'])} / {_pct(a['S2']['cpu_p95'])}", lambda a: a["S2"]["cpu_mean"])
    memory = "Memory (phys footprint, median / peak)" if platform == "macos" else "Memory (PSS, median / peak)"
    m = lambda a: a["measured"]
    row(memory, lambda a: f"{_mb(m(a)['footprint_median'])} / {_mb(m(a)['footprint_peak'])}",
        lambda a: m(a)["footprint_median"])
    row("Resident memory (median)", lambda a: _mb(m(a)["rss_median"]), lambda a: m(a)["rss_median"])
    row("Threads (median / peak)", lambda a: f"{_num(m(a)['threads_median'])} / {_num(m(a)['threads_peak'])}",
        lambda a: m(a)["threads_median"])
    if m(A[base])["idle_wkups_per_s"] is not None or m(A[cand])["idle_wkups_per_s"] is not None:
        row("Idle wake-ups per second",
            lambda a: "—" if m(a)["idle_wkups_per_s"] is None else f"{m(a)['idle_wkups_per_s']:.1f}",
            lambda a: m(a)["idle_wkups_per_s"])
    row("Installed size", lambda a: _mb(m(a)["installed"]), lambda a: m(a)["installed"])
    row(download_label, lambda a: _mb(m(a)["download"]), lambda a: m(a)["download"])
    minutes = m(A[base])["minutes"] or m(A[cand])["minutes"] or 0
    row(f"Disk writes during the {round(minutes, 1):g}-min run", lambda a: _bytes(m(a)["disk_w"]),
        lambda a: m(a)["disk_w"])
    head = [f"| {title} | {labels.get(base, base)} | {labels.get(cand, cand)} | Change |", "|---|---|---|---|"]
    return "\n".join(head + rows) + "\n"


def render_verdicts(summary, labels):
    base, cand = summary["baseline"], summary["candidate"]
    fmt = lambda name, v: ("—" if v is None else _pct(v) if name.startswith("cpu_") else
                           _mb(v) if name in ("footprint", "rss", "installed", "download") else
                           _bytes(v) if name == "disk_writes" else f"{v:g}")
    lines = [f"Pass rule (medians over {summary['runs']} run(s); within {TOLERANCE:.0%} = rerun): "
             f"**{summary['verdict']}**", "",
             f"| Axis | {labels.get(base, base)} | {labels.get(cand, cand)} | Verdict |", "|---|---|---|---|"]
    for a in summary["axes"]:
        lines.append(f"| {a['axis']} | {fmt(a['axis'], a['baseline'])} | {fmt(a['axis'], a['candidate'])} | "
                     f"{a['verdict']} |")
    return "\n".join(lines) + "\n"


def _pairs(items, cast=str):
    out = {}
    for item in items:
        k, _, v = item.partition("=")
        out[k] = cast(v)
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("csv", nargs="+", type=Path, help="one samples.csv per run")
    ap.add_argument("--out-dir", type=Path, required=True)
    ap.add_argument("--baseline", default="cs")
    ap.add_argument("--candidate", default="rs")
    ap.add_argument("--label", action="append", default=[], metavar="AGENT=TEXT")
    ap.add_argument("--installed-size", action="append", default=[], metavar="AGENT=BYTES")
    ap.add_argument("--download-size", action="append", default=[], metavar="AGENT=BYTES")
    ap.add_argument("--download-label", default="Download (osx-arm64 zip)")
    ap.add_argument("--platform", choices=("macos", "linux"), default="macos")
    ap.add_argument("--title", default="MacBook Air M4, macOS 26.6, same config")
    ap.add_argument("--note", default="", help="the footnote: method, binaries, date")
    args = ap.parse_args()
    runs = [load_rows(p) for p in args.csv]
    sizes = {"installed": _pairs(args.installed_size, int), "download": _pairs(args.download_size, int)}
    summary = summarize(runs, args.baseline, args.candidate, sizes)
    labels = _pairs(args.label)
    table = render_table(summary, args.title, labels, args.platform, args.download_label)
    verdicts = render_verdicts(summary, labels)
    summary.update({"labels": labels, "title": args.title, "platform": args.platform, "note": args.note,
                    "inputs": [str(p) for p in args.csv], "table": table})
    args.out_dir.mkdir(parents=True, exist_ok=True)
    (args.out_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    text = table + ("\n" + args.note + "\n" if args.note else "") + "\n" + verdicts
    (args.out_dir / "table.md").write_text(text, encoding="utf-8")
    print(text, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
