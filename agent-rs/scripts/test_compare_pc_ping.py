"""Tests for compare-pc-ping.py's comparison (pure; no agents are started)."""
import copy
import importlib.util
import sys
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("compare_pc_ping", Path(__file__).resolve().parent / "compare-pc-ping.py")
cmp = importlib.util.module_from_spec(spec)
sys.modules["compare_pc_ping"] = cmp
spec.loader.exec_module(cmp)


def sample(**pc):
    base = {
        "cpu": 10.0, "cpuTemp": 45.0, "cpuName": "Apple M2", "ramUsedGb": 9.1, "ramTotalGb": 16.0, "gpu": 5.0,
        "netDownBps": 100.0, "netUpBps": 50.0, "disks": [{"name": "Macintosh HD", "usedGb": 300.0, "totalGb": 460.0}],
        "uptimeSec": 1000, "gpus": [{"name": "Apple M2", "util": 5.0, "vendor": "apple"}],
    }
    base.update(pc)
    return {"pc": base, "ping": {"host": "1.1.1.1", "ms": 12.0, "avgMs": 12.5, "lossPct": 0.0}}


class CompareTests(unittest.TestCase):
    def test_close_live_values_match(self):
        a = [sample(cpu=10 + i, uptimeSec=1000 + i) for i in range(10)]
        b = [sample(cpu=14 + i, ramUsedGb=9.6, cpuTemp=47.0, uptimeSec=1001 + i) for i in range(10)]
        self.assertEqual(cmp.compare(a, b), [])

    def test_fixed_values_must_be_equal(self):
        a = [sample()]
        for change, expect in [
            ({"cpuName": "Apple M3"}, "cpuName"),
            ({"ramTotalGb": 32.0}, "ramTotalGb"),
            ({"disks": [{"name": "Data", "usedGb": 300.0, "totalGb": 460.0}]}, "disks"),
            ({"gpus": [{"name": "Apple M2", "util": 5.0, "vendor": "nvidia"}]}, "gpus"),
        ]:
            problems = cmp.compare(a, [sample(**change)])
            self.assertTrue(any(expect in p for p in problems), (change, problems))

    def test_key_sets_and_ping_host_must_match(self):
        b = sample()
        del b["pc"]["gpu"]
        self.assertTrue(any("keys" in p for p in cmp.compare([sample()], [b])))
        c = sample()
        c["ping"]["host"] = "8.8.8.8"
        self.assertTrue(any("ping.host" in p for p in cmp.compare([sample()], [c])))

    def test_live_values_out_of_tolerance_are_reported(self):
        a = [sample() for _ in range(5)]
        for change, expect in [
            ({"cpu": 60.0}, "cpu"),
            ({"ramUsedGb": 12.0}, "ramUsedGb"),
            ({"cpuTemp": None}, "cpuTemp"),
            ({"uptimeSec": 1010}, "uptimeSec"),
        ]:
            problems = cmp.compare(a, [sample(**change) for _ in range(5)])
            self.assertTrue(any(expect in p for p in problems), (change, problems))
        slow = [copy.deepcopy(s) for s in a]
        for s in slow:
            s["ping"]["ms"] = 80.0
        self.assertTrue(any("ping.ms" in p for p in cmp.compare(a, slow)))

    def test_ping_compares_the_fastest_probes_because_wifi_jitters(self):
        a = [sample() for _ in range(4)]
        b = [copy.deepcopy(s) for s in a]
        for s, ms in zip(a, [64.0, 64.0, 64.0, 12.0]):
            s["ping"]["ms"] = ms
        for s, ms in zip(b, [12.5, 12.5, 13.0, 13.0]):
            s["ping"]["ms"] = ms
        self.assertEqual(cmp.compare(a, b), [])
        for s in b:
            s["ping"]["ms"] = None
        self.assertTrue(any("ping.ms" in p for p in cmp.compare(a, b)))

    def test_a_missing_snapshot_is_reported(self):
        b = sample()
        b["ping"] = None
        self.assertTrue(cmp.compare([sample()], [b]))


if __name__ == "__main__":
    unittest.main()
