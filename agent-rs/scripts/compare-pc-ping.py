#!/usr/bin/env python3
"""Compares the `pc` and `ping` parts of /api/state samples from the C# and the Rust agent on the same machine.

    python3 -I agent-rs/scripts/compare-pc-ping.py csharp.jsonl rust.jsonl

Each file has one /api/state answer per line, taken a second apart. Fixed facts must be equal; live readings are
compared by their medians (ping: the fastest probes) within the tolerances below. Prints "pc and ping match" or the differences.
"""
from __future__ import annotations

import json
import statistics
import sys

# Live readings: the largest difference between the two agents' medians that still counts as the same reading.
TOLERANCE = {
    "cpu": 15.0,  # percentage points: each agent samples its own second
    "ramUsedGb": 1.5,  # GB: memory moves while sampling
    "cpuTemp": 8.0,  # °C
    "gpu": 20.0,  # percentage points
    "ping.ms": 40.0,  # ms between the two agents' fastest probes: each sends its own, and Wi-Fi jitters (10-75 ms seen)
}
UPTIME_SECONDS = 5


def _median(values):
    nums = [v for v in values if isinstance(v, (int, float)) and not isinstance(v, bool)]
    return statistics.median(nums) if nums else None


def _fastest(values):
    nums = [v for v in values if isinstance(v, (int, float)) and not isinstance(v, bool)]
    return min(nums) if nums else None


def _keys(doc):
    """The key structure of a document: nested keys in order, lists by their items' structure."""
    if isinstance(doc, dict):
        return {k: _keys(v) for k, v in doc.items()}
    if isinstance(doc, list):
        return [_keys(v) for v in doc]
    return None


def _live(name, a_values, b_values, problems, tolerance_key=None, pick=None):
    tolerance = TOLERANCE[tolerance_key or name]
    pick = pick or _median
    ma, mb = pick(a_values), pick(b_values)
    if ma is None and mb is None:
        return
    if ma is None or mb is None:
        problems.append(f"{name}: C# {ma} vs Rust {mb} (one side has no reading)")
    elif abs(ma - mb) > tolerance:
        problems.append(f"{name}: C# {ma:.1f} vs Rust {mb:.1f} (more than {tolerance} apart)")


def compare(csharp: list[dict], rust: list[dict]) -> list[str]:
    problems: list[str] = []
    for i, (a, b) in enumerate(zip(csharp, rust)):
        for part in ("pc", "ping"):
            if not isinstance(a.get(part), dict) or not isinstance(b.get(part), dict):
                problems.append(f"sample {i}: {part} is {type(a.get(part)).__name__} on C#, {type(b.get(part)).__name__} on Rust")
            elif list(a[part].keys()) != list(b[part].keys()):
                problems.append(f"sample {i}: {part} keys {list(a[part].keys())} != {list(b[part].keys())}")
    if problems:
        return problems
    pa, pb = [s["pc"] for s in csharp], [s["pc"] for s in rust]
    last_a, last_b = pa[-1], pb[-1]
    for key in ("cpuName", "ramTotalGb"):
        if last_a.get(key) != last_b.get(key):
            problems.append(f"{key}: {last_a.get(key)!r} != {last_b.get(key)!r}")
    disks = lambda pc: [(d.get("name"), d.get("totalGb")) for d in pc.get("disks") or []]  # noqa: E731
    if disks(last_a) != disks(last_b):
        problems.append(f"disks: {disks(last_a)} != {disks(last_b)}")
    gpus = lambda pc: [(g.get("name"), g.get("vendor")) for g in pc.get("gpus") or []]  # noqa: E731
    if gpus(last_a) != gpus(last_b):
        problems.append(f"gpus: {gpus(last_a)} != {gpus(last_b)}")
    if _keys(last_a.get("disks")) != _keys(last_b.get("disks")) or _keys(last_a.get("gpus")) != _keys(last_b.get("gpus")):
        problems.append("disk or GPU keys differ")
    if abs((last_a.get("uptimeSec") or 0) - (last_b.get("uptimeSec") or 0)) > UPTIME_SECONDS:
        problems.append(f"uptimeSec: {last_a.get('uptimeSec')} vs {last_b.get('uptimeSec')}")
    for key in ("cpu", "ramUsedGb", "cpuTemp", "gpu"):
        _live(key, [p.get(key) for p in pa], [p.get(key) for p in pb], problems)
    for i in range(min(len(last_a.get("gpus") or []), len(last_b.get("gpus") or []))):
        def util(pcs):
            return [p["gpus"][i].get("util") for p in pcs if len(p.get("gpus") or []) > i]
        _live(f"gpus[{i}].util", util(pa), util(pb), problems, tolerance_key="gpu")
    hosts = {s["ping"].get("host") for s in csharp} | {s["ping"].get("host") for s in rust}
    if len(hosts) != 1:
        problems.append(f"ping.host: {sorted(map(str, hosts))}")
    # a probe every 5 s gives each agent two or three probes in ten samples: compare the fastest ones
    _live("ping.ms", [s["ping"].get("ms") for s in csharp], [s["ping"].get("ms") for s in rust], problems,
          pick=_fastest)
    return problems


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    read = lambda path: [json.loads(line) for line in open(path, encoding="utf-8") if line.strip()]  # noqa: E731
    problems = compare(read(sys.argv[1]), read(sys.argv[2]))
    for p in problems:
        print(p)
    if not problems:
        print("pc and ping match")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
