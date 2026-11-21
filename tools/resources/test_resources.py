"""Tests for the resource comparison tools' pure parts (parsers, conversions, statistics, the client's schedule).

No agent is started and nothing is sampled live:  python3 -I tools/resources/test_resources.py
"""
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import client  # noqa: E402
import report  # noqa: E402
import sample  # noqa: E402

# /proc/<pid>/stat with a command name that holds a space and a closing parenthesis: only the last ")" ends it
STAT = ("4242 (Phone Box) x) S 1 4242 4242 0 -1 4194560 2000 0 0 0 250 50 30 20 20 0 41 0 12345 1000000 5000 "
        "18446744073709551615 1 1 0 0 0 0 0 4096 0 0 0 0 17 3 0 0 0 0 0\n")
STATUS = "Name:\tPhoneBoxAgent\nUmask:\t0022\nState:\tS (sleeping)\nVmPeak:\t  900000 kB\nVmRSS:\t   51200 kB\nThreads:\t41\n"
SMAPS_ROLLUP = ("55d0c0a00000-7ffd5e7f9000 ---p 00000000 00:00 0                          [rollup]\n"
                "Rss:               52000 kB\nPss:               48000 kB\nPss_Anon:          30000 kB\n"
                "Pss_File:          18000 kB\nShared_Clean:       4000 kB\n")
IO = ("rchar: 1000\nwchar: 2000\nsyscr: 1\nsyscw: 2\nread_bytes: 4096\nwrite_bytes: 8192\n"
      "cancelled_write_bytes: 0\n")


class LinuxParserTests(unittest.TestCase):
    def test_stat_gives_own_and_child_cpu_seconds(self):
        self.assertEqual(sample.parse_proc_stat(STAT, 100), (3.0, 0.5))

    def test_status_gives_resident_bytes_and_threads(self):
        self.assertEqual(sample.parse_proc_status(STATUS), (51200 * 1024, 41))

    def test_smaps_rollup_gives_pss_not_pss_anon(self):
        self.assertEqual(sample.parse_smaps_rollup(SMAPS_ROLLUP), 48000 * 1024)

    def test_io_gives_storage_bytes_not_chars(self):
        self.assertEqual(sample.parse_proc_io(IO), (4096, 8192))

    def test_a_whole_sample_from_a_proc_folder(self):
        with tempfile.TemporaryDirectory() as root:
            d = Path(root, "4242")
            d.mkdir()
            for name, text in (("stat", STAT), ("status", STATUS), ("smaps_rollup", SMAPS_ROLLUP), ("io", IO)):
                (d / name).write_text(text)
            row = sample.linux_sample(4242, proc_root=root, clk_tck=100)
        self.assertEqual(row, {"cpu_s": 3.0, "child_cpu_s": 0.5, "footprint": 48000 * 1024, "rss": 51200 * 1024,
                               "threads": 41, "disk_r": 4096, "disk_w": 8192, "idle_wkups": None})

    def test_a_process_that_is_gone_raises_lookup_error(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaises(ProcessLookupError):
                sample.linux_sample(4242, proc_root=root, clk_tck=100)

    def test_csv_columns_are_the_methods(self):
        self.assertEqual(sample.COLUMNS, ["t", "agent", "scenario", "cpu_s", "child_cpu_s", "footprint", "rss",
                                          "threads", "disk_r", "disk_w", "idle_wkups"])


class MachTickTests(unittest.TestCase):
    def test_apple_silicon_ticks_are_125_over_3_nanoseconds(self):
        self.assertEqual(sample.mach_ticks_to_seconds(24_000_000, 125, 3), 1.0)

    def test_intel_ticks_are_nanoseconds(self):
        self.assertEqual(sample.mach_ticks_to_seconds(2_500_000_000, 1, 1), 2.5)

    def test_rusage_layout_matches_the_sdk(self):
        # struct rusage_info_v4: a 16-byte uuid and 35 uint64 counters; proc_taskinfo: 6 uint64 and 12 int32
        self.assertEqual(sample.RUSAGE_INFO_V4_SIZE, 16 + 35 * 8)
        self.assertEqual(sample.PROC_TASKINFO_SIZE, 6 * 8 + 12 * 4)


def rows(agent, cpu_per_s, footprint, threads, wkups_per_s=0.0, write_per_s=0, warmup_cpu_per_s=None,
         phases=(("warmup", 10), ("S0", 20), ("S1", 20), ("S2", 20))):
    """1 s samples with cumulative counters, as sample.py writes them."""
    out, t, cpu, wk, w = [], 1000.0, 0.0, 0.0, 0
    for phase, seconds in phases:
        for _ in range(seconds):
            rate = warmup_cpu_per_s if phase == "warmup" and warmup_cpu_per_s is not None else cpu_per_s
            out.append({"t": t, "agent": agent, "scenario": phase, "cpu_s": cpu, "child_cpu_s": 0.0,
                        "footprint": footprint, "rss": footprint * 2, "threads": threads, "disk_r": 0, "disk_w": w,
                        "idle_wkups": wk})
            t, cpu, wk, w = t + 1, cpu + rate, wk + wkups_per_s, w + write_per_s
    return out


class StatisticsTests(unittest.TestCase):
    def test_median(self):
        self.assertEqual(report.median([4, 1, 3, 2]), 2.5)
        self.assertEqual(report.median([3, 1, 2]), 2)

    def test_p95_interpolates_between_ranks(self):
        self.assertAlmostEqual(report.percentile(list(range(1, 21)), 95), 19.05)

    def test_cpu_mean_comes_from_the_cumulative_counters(self):
        r = rows("cs", 0.10, 100, 40)
        s0 = [x for x in r if x["scenario"] == "S0"]
        self.assertAlmostEqual(report.cpu_mean(s0), 10.0)

    def test_cpu_windows_are_5_s_rates(self):
        r = [x for x in rows("cs", 0.10, 100, 40) if x["scenario"] == "S1"]
        windows = report.cpu_windows(r, 5.0)
        self.assertEqual(len(windows), 3)  # 20 samples span 19 s: three whole 5 s windows
        for w in windows:
            self.assertAlmostEqual(w, 10.0)

    def test_child_cpu_counts(self):
        r = rows("cs", 0.10, 100, 40)
        for i, x in enumerate(r):
            x["child_cpu_s"] = i * 0.05
        s0 = [x for x in r if x["scenario"] == "S0"]
        self.assertAlmostEqual(report.cpu_mean(s0), 15.0)


class VerdictTests(unittest.TestCase):
    def test_lighter_by_more_than_5_percent_passes(self):
        self.assertEqual(report.verdict(100, 90), "pass")

    def test_within_5_percent_either_way_asks_for_a_rerun(self):
        self.assertEqual(report.verdict(100, 97), "rerun")
        self.assertEqual(report.verdict(100, 103), "rerun")

    def test_heavier_fails(self):
        self.assertEqual(report.verdict(100, 110), "fail")

    def test_both_zero_is_a_tie(self):
        self.assertEqual(report.verdict(0, 0), "tie")

    def test_a_missing_axis_is_not_applicable(self):
        self.assertEqual(report.verdict(None, 5), "n/a")


class ReportTests(unittest.TestCase):
    def summarize(self, cs, rs, **kw):
        return report.summarize([cs + rs], baseline="cs", candidate="rs",
                                sizes={"installed": {"cs": 109_000_000, "rs": 14_000_000},
                                       "download": {"cs": 44_322_227, "rs": 6_100_000}}, **kw)

    def test_the_warmup_is_not_measured(self):
        s = self.summarize(rows("cs", 0.10, 100e6, 40, warmup_cpu_per_s=0.9), rows("rs", 0.05, 20e6, 14))
        self.assertAlmostEqual(s["agents"]["cs"]["S0"]["cpu_mean"], 10.0)
        self.assertAlmostEqual(s["agents"]["rs"]["S0"]["cpu_mean"], 5.0)

    def test_memory_threads_wakeups_and_writes_over_the_measured_scenarios(self):
        s = self.summarize(rows("cs", 0.10, 100e6, 40, wkups_per_s=20, write_per_s=1000),
                           rows("rs", 0.05, 20e6, 14, wkups_per_s=4, write_per_s=10))
        cs = s["agents"]["cs"]["measured"]
        self.assertEqual(cs["footprint_median"], 100e6)
        self.assertEqual(cs["footprint_peak"], 100e6)
        self.assertEqual(cs["rss_median"], 200e6)
        self.assertEqual((cs["threads_median"], cs["threads_peak"]), (40, 40))
        self.assertAlmostEqual(cs["idle_wkups_per_s"], 20.0)
        self.assertEqual(cs["disk_w"], 59 * 1000)  # 60 measured samples: 59 s of writes
        self.assertAlmostEqual(cs["minutes"], 1.0)

    def test_a_lighter_candidate_passes_every_axis(self):
        s = self.summarize(rows("cs", 0.10, 100e6, 40, wkups_per_s=20, write_per_s=1000),
                           rows("rs", 0.05, 20e6, 14, wkups_per_s=4, write_per_s=10))
        self.assertEqual(s["verdict"], "pass")
        self.assertTrue(all(a["verdict"] == "pass" for a in s["axes"]), s["axes"])

    def test_one_close_axis_asks_for_a_rerun_and_one_heavier_axis_fails(self):
        close = self.summarize(rows("cs", 0.10, 100e6, 40), rows("rs", 0.05, 20e6, 39))
        self.assertEqual(close["verdict"], "rerun")
        self.assertEqual([a["axis"] for a in close["axes"] if a["verdict"] == "rerun"], ["threads"])
        heavier = self.summarize(rows("cs", 0.10, 100e6, 40), rows("rs", 0.05, 20e6, 60))
        self.assertEqual(heavier["verdict"], "fail")

    def test_medians_pool_both_runs(self):
        run1 = rows("cs", 0.10, 100e6, 40) + rows("rs", 0.05, 20e6, 10)
        run2 = rows("cs", 0.10, 100e6, 40) + rows("rs", 0.05, 20e6, 20)
        s = report.summarize([run1, run2], baseline="cs", candidate="rs", sizes={})
        self.assertEqual(s["agents"]["rs"]["measured"]["threads_median"], 15)
        self.assertEqual(s["agents"]["rs"]["measured"]["threads_peak"], 20)

    def test_the_release_notes_table(self):
        s = self.summarize(rows("cs", 0.10, 100e6, 40, wkups_per_s=20, write_per_s=1000),
                           rows("rs", 0.05, 20e6, 14, wkups_per_s=4, write_per_s=10))
        table = report.render_table(s, title="MacBook Air M4, macOS 26.6, same config",
                                    labels={"cs": "C# 0.9.23", "rs": "Rust 1.0.0"}, platform="macos",
                                    download_label="Download (osx-arm64 zip)")
        lines = table.splitlines()
        self.assertEqual(lines[0], "| MacBook Air M4, macOS 26.6, same config | C# 0.9.23 | Rust 1.0.0 | Change |")
        self.assertIn("| CPU, idle (mean % of one core; with child tools) | 10.0% | 5.0% | -50% |", lines)
        self.assertIn("| CPU, phone connected (mean / p95) | 10.0% / 10.0% | 5.0% / 5.0% | -50% |", lines)
        self.assertIn("| Memory (phys footprint, median / peak) | 100.0 MB / 100.0 MB | 20.0 MB / 20.0 MB | -80% |", lines)
        self.assertIn("| Threads (median / peak) | 40 / 40 | 14 / 14 | -65% |", lines)
        self.assertIn("| Idle wake-ups per second | 20.0 | 4.0 | -80% |", lines)
        self.assertIn("| Installed size | 109.0 MB | 14.0 MB | -87% |", lines)
        self.assertIn("| Download (osx-arm64 zip) | 44.3 MB | 6.1 MB | -86% |", lines)
        self.assertIn("| Disk writes during the 1-min run | 59.0 kB | 0.6 kB | -99% |", lines)

    def test_linux_has_pss_and_no_wakeup_row(self):
        cs, rs = rows("cs", 0.10, 100e6, 40), rows("rs", 0.05, 20e6, 14)
        for x in cs + rs:
            x["idle_wkups"] = None
        s = self.summarize(cs, rs)
        table = report.render_table(s, title="Linux", labels={"cs": "C#", "rs": "Rust"}, platform="linux",
                                    download_label="Download (linux-x64 zip)")
        self.assertIn("Memory (PSS, median / peak)", table)
        self.assertNotIn("wake-ups", table)
        self.assertNotIn("wakeups", [a["axis"] for a in s["axes"] if a["verdict"] != "n/a"])

    def test_csv_round_trip(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d, "samples.csv")
            with open(path, "w", newline="") as f:
                w = sample.CsvWriter(f)
                w.write({"t": 1.5, "agent": "rs", "scenario": "S0", "cpu_s": 0.25, "child_cpu_s": 0.0,
                         "footprint": 10, "rss": 20, "threads": 3, "disk_r": 0, "disk_w": 7, "idle_wkups": None})
            self.assertEqual(report.load_rows(path), [{"t": 1.5, "agent": "rs", "scenario": "S0", "cpu_s": 0.25,
                                                       "child_cpu_s": 0.0, "footprint": 10, "rss": 20, "threads": 3,
                                                       "disk_r": 0, "disk_w": 7, "idle_wkups": None}])


CONFIG = {"version": "7", "config": {
    "pages": [
        {"id": "home", "widgets": [{"id": "c", "type": "clock"}, {"id": "m", "type": "media"},
                                   {"id": "s", "type": "stat"}]},
        {"id": "ai", "widgets": [{"id": "q", "type": "quotas"}]},
        {"id": "procs", "widgets": [{"id": "p", "type": "processes"}]},
        {"id": "deck", "widgets": [{"id": "k", "type": "shortcuts"}]},
        {"id": "pc", "widgets": [{"id": "b", "type": "batteries"}, {"id": "o", "type": "obs"}]},
        {"id": "off", "enabled": False, "widgets": [{"id": "a", "type": "agenda"}]},
    ],
    "shortcuts": [{"id": "terminal", "iconRev": 3}, {"id": "mute"}],
}}


def paths(schedule, path=None):
    """The requests that always go out (a cover check only sends one when the cover changed)."""
    return [(e.at, e.path) for e in schedule.events if e.kind != "art" and (path is None or e.path == path)]


class ScheduleTests(unittest.TestCase):
    def test_shown_pages_leave_out_disabled_ones(self):
        self.assertEqual([p["id"] for p in client.shown_pages(CONFIG)], ["home", "ai", "procs", "deck", "pc"])

    def test_s0_sends_nothing(self):
        s = client.build_schedule(CONFIG, "S0", 600)
        self.assertEqual((s.stream, s.events), (False, []))

    def test_s1_is_the_stream_the_boot_requests_and_the_global_loops(self):
        s = client.build_schedule(CONFIG, "S1", 61)
        self.assertTrue(s.stream)
        self.assertEqual(s.pages, [(0, "home")])
        boot = sorted(p for at, p in paths(s) if at == 0)
        self.assertEqual(boot, ["/api/approvals", "/api/config", "/api/pair", "/api/push", "/api/quotas",
                                "/api/shortcuts/icon?id=terminal", "/api/state?ev=0"])
        self.assertEqual([at for at, _ in paths(s, "/api/approvals")], list(range(0, 61, 5)))
        self.assertEqual([at for at, _ in paths(s, "/api/quotas")], [0, 30, 60])
        self.assertEqual([at for at, _ in paths(s, "/api/config")], [0, 60])
        self.assertEqual([at for at, _ in paths(s, "/api/pair")], [0, 60])
        everything = {p for _, p in paths(s)}
        self.assertNotIn("/api/calendar", everything)  # the agenda is on a disabled page
        self.assertNotIn("/api/procs?top=150", everything)  # the processes page is not the one shown
        self.assertTrue(all(e.method == "GET" for e in s.events))

    def test_the_media_widget_checks_the_cover_every_second_while_its_page_is_active(self):
        s = client.build_schedule(CONFIG, "S1", 10)
        self.assertEqual([e.at for e in s.events if e.kind == "art"], list(range(10)))

    def test_image_requests_go_without_the_panel_header(self):
        s = client.build_schedule(CONFIG, "S1", 10)
        icon = [e for e in s.events if e.path.startswith("/api/shortcuts/icon")][0]
        self.assertFalse(icon.panel)
        self.assertTrue([e for e in s.events if e.path == "/api/config"][0].panel)

    def test_s2_cycles_the_shown_pages_with_their_requests(self):
        s = client.build_schedule(CONFIG, "S2", 100, page_seconds=10, boot=False)
        self.assertEqual(s.pages, [(0, "home"), (10, "ai"), (20, "procs"), (30, "deck"), (40, "pc"),
                                   (50, "home"), (60, "ai"), (70, "procs"), (80, "deck"), (90, "pc")])
        # the processes loop asks only while its page is the one shown, every 2 s, and fetches new icons
        procs = [e for e in s.events if e.path == "/api/procs?top=150"]
        self.assertEqual([e.at for e in procs], [20, 22, 24, 26, 28, 70, 72, 74, 76, 78])
        self.assertTrue(all(e.kind == "procs-icons" for e in procs))
        # widget loaders run on the shown page and its neighbours; every() keeps its interval across pages
        self.assertEqual([at for at, _ in paths(s, "/api/batteries")], [30, 80])
        self.assertEqual([at for at, _ in paths(s, "/api/obs")], list(range(30, 50, 2)) + list(range(80, 100, 2)))
        # boot=False: S2 continues S1's panel, so no second boot
        everything = {p for _, p in paths(s)}
        self.assertNotIn("/api/push", everything)
        self.assertNotIn("/api/shortcuts/icon?id=terminal", everything)

    def test_the_battery_report_is_off_unless_asked_for(self):
        s = client.build_schedule(CONFIG, "S2", 100)
        self.assertNotIn("/api/phone/battery", " ".join(p for _, p in paths(s)))
        on = client.build_schedule(CONFIG, "S1", 100, battery=True)
        posts = [e for e in on.events if e.method == "POST"]
        self.assertEqual([e.at for e in posts], [5, 30, 60, 90])
        self.assertTrue(all(e.path.startswith("/api/phone/battery?") for e in posts))

    def test_phases_parse_and_name_their_kind(self):
        self.assertEqual(client.parse_phases("warmup=120,S0=600,S1=600,S2=720"),
                         [("warmup", 120), ("S0", 600), ("S1", 600), ("S2", 720)])
        with self.assertRaises(ValueError):
            client.parse_phases("S3=10")


if __name__ == "__main__":
    unittest.main()
