"""Samples agent processes once a second into a CSV (survey-c 4.4 and 4.5). Standard library only; run with python3 -I.

    python3 -I tools/resources/sample.py --out samples.csv --phase-file phase --pid cs=1234 --pid rs=5678
    python3 -I tools/resources/sample.py --check      # samples itself once and checks the numbers are sane

Columns: t, agent, scenario, cpu_s, child_cpu_s, footprint, rss, threads, disk_r, disk_w, idle_wkups. The counters
(CPU seconds, disk bytes, wake-ups) are cumulative since the process started; report.py turns them into rates.
  macOS (libproc through ctypes): proc_pid_rusage(RUSAGE_INFO_V4) gives user+system and the reaped children's time in
    Mach ticks (converted with mach_timebase_info: 125/3 ns per tick on Apple silicon), ri_phys_footprint (Activity
    Monitor's "Memory"), ri_resident_size, the disk bytes and ri_pkg_idle_wkups; proc_pidinfo(PROC_PIDTASKINFO) the
    thread count.
  Linux (/proc): stat utime+stime and cutime+cstime / SC_CLK_TCK, status VmRSS and Threads, smaps_rollup Pss as the
    footprint, io read_bytes/write_bytes; no wake-up counter.
The scenario is whatever the phase file (written by client.py) holds when the sample is taken. The sampler stops on
SIGTERM or SIGINT, or when every sampled process is gone.
"""
import argparse
import csv
import ctypes
import os
import signal
import sys
import time
from pathlib import Path

COLUMNS = ["t", "agent", "scenario", "cpu_s", "child_cpu_s", "footprint", "rss", "threads", "disk_r", "disk_w",
           "idle_wkups"]

# ---------------------------------------------------------------- Linux: /proc


def parse_proc_stat(text: str, clk_tck: int) -> tuple:
    """(own CPU seconds, reaped children's CPU seconds) from /proc/<pid>/stat."""
    # the command name is in parentheses and may hold spaces and parentheses itself: it ends at the last ")"
    fields = text[text.rindex(")") + 2:].split()
    # fields[0] is field 3 (state): utime, stime, cutime, cstime are fields 14 to 17
    utime, stime, cutime, cstime = (int(x) for x in fields[11:15])
    return (utime + stime) / clk_tck, (cutime + cstime) / clk_tck


def _kb_field(text: str, name: str) -> int:
    for line in text.splitlines():
        key, _, value = line.partition(":")
        if key == name:
            return int(value.split()[0]) * 1024
    raise ValueError(f"no {name} line")


def parse_proc_status(text: str) -> tuple:
    """(resident bytes, threads) from /proc/<pid>/status."""
    threads = next(int(line.split(":", 1)[1]) for line in text.splitlines() if line.startswith("Threads:"))
    return _kb_field(text, "VmRSS"), threads


def parse_smaps_rollup(text: str) -> int:
    """The proportional set size in bytes from /proc/<pid>/smaps_rollup."""
    return _kb_field(text, "Pss")


def parse_proc_io(text: str) -> tuple:
    """(bytes read from storage, bytes written to storage) from /proc/<pid>/io."""
    values = dict(line.split(": ", 1) for line in text.splitlines() if ": " in line)
    return int(values["read_bytes"]), int(values["write_bytes"])


def linux_sample(pid: int, proc_root: str = "/proc", clk_tck: int = 0) -> dict:
    folder = Path(proc_root, str(pid))
    try:
        stat, status, rollup, io = (
            (folder / name).read_text() for name in ("stat", "status", "smaps_rollup", "io"))
    except (FileNotFoundError, ProcessLookupError) as e:
        raise ProcessLookupError(pid) from e
    cpu, child = parse_proc_stat(stat, clk_tck or os.sysconf("SC_CLK_TCK"))
    rss, threads = parse_proc_status(status)
    disk_r, disk_w = parse_proc_io(io)
    return {"cpu_s": cpu, "child_cpu_s": child, "footprint": parse_smaps_rollup(rollup), "rss": rss,
            "threads": threads, "disk_r": disk_r, "disk_w": disk_w, "idle_wkups": None}


# ---------------------------------------------------------------- macOS: libproc


def mach_ticks_to_seconds(ticks: int, numer: int, denom: int) -> float:
    """Mach absolute-time ticks to seconds (rusage times are ticks on arm64, nanoseconds on Intel where 1/1)."""
    return ticks * numer / denom / 1e9


RUSAGE_INFO_V4 = 4
PROC_PIDTASKINFO = 4
# <sys/resource.h> struct rusage_info_v4, after the 16-byte ri_uuid
_RUSAGE_V4_FIELDS = (
    "user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins", "wired_size", "resident_size",
    "phys_footprint", "proc_start_abstime", "proc_exit_abstime", "child_user_time", "child_system_time",
    "child_pkg_idle_wkups", "child_interrupt_wkups", "child_pageins", "child_elapsed_abstime", "diskio_bytesread",
    "diskio_byteswritten", "cpu_time_qos_default", "cpu_time_qos_maintenance", "cpu_time_qos_background",
    "cpu_time_qos_utility", "cpu_time_qos_legacy", "cpu_time_qos_user_initiated", "cpu_time_qos_user_interactive",
    "billed_system_time", "serviced_system_time", "logical_writes", "lifetime_max_phys_footprint", "instructions",
    "cycles", "billed_energy", "serviced_energy", "interval_max_phys_footprint", "runnable_time")


class RUsageInfoV4(ctypes.Structure):
    _fields_ = [("ri_uuid", ctypes.c_uint8 * 16)] + [("ri_" + n, ctypes.c_uint64) for n in _RUSAGE_V4_FIELDS]


class ProcTaskInfo(ctypes.Structure):
    """<sys/proc_info.h> struct proc_taskinfo."""
    _fields_ = [(n, ctypes.c_uint64) for n in ("pti_virtual_size", "pti_resident_size", "pti_total_user",
                                                "pti_total_system", "pti_threads_user", "pti_threads_system")] + \
               [(n, ctypes.c_int32) for n in ("pti_policy", "pti_faults", "pti_pageins", "pti_cow_faults",
                                               "pti_messages_sent", "pti_messages_received", "pti_syscalls_mach",
                                               "pti_syscalls_unix", "pti_csw", "pti_threadnum", "pti_numrunning",
                                               "pti_priority")]


class MachTimebaseInfo(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


RUSAGE_INFO_V4_SIZE = ctypes.sizeof(RUsageInfoV4)
PROC_TASKINFO_SIZE = ctypes.sizeof(ProcTaskInfo)


class MacSampler:
    def __init__(self):
        lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)  # libproc and mach live in libSystem
        self._rusage = lib.proc_pid_rusage
        self._rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
        self._rusage.restype = ctypes.c_int
        self._pidinfo = lib.proc_pidinfo
        self._pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
        self._pidinfo.restype = ctypes.c_int
        timebase = MachTimebaseInfo()
        lib.mach_timebase_info.argtypes = [ctypes.c_void_p]
        if lib.mach_timebase_info(ctypes.addressof(timebase)) != 0 or not timebase.denom:
            raise OSError("mach_timebase_info failed")
        self.numer, self.denom = timebase.numer, timebase.denom

    def __call__(self, pid: int) -> dict:
        ri = RUsageInfoV4()
        if self._rusage(pid, RUSAGE_INFO_V4, ctypes.addressof(ri)) != 0:
            raise ProcessLookupError(pid)
        ti = ProcTaskInfo()
        if self._pidinfo(pid, PROC_PIDTASKINFO, 0, ctypes.addressof(ti), PROC_TASKINFO_SIZE) != PROC_TASKINFO_SIZE:
            raise ProcessLookupError(pid)
        sec = lambda ticks: mach_ticks_to_seconds(ticks, self.numer, self.denom)
        return {"cpu_s": sec(ri.ri_user_time + ri.ri_system_time),
                "child_cpu_s": sec(ri.ri_child_user_time + ri.ri_child_system_time),
                "footprint": ri.ri_phys_footprint, "rss": ri.ri_resident_size, "threads": ti.pti_threadnum,
                "disk_r": ri.ri_diskio_bytesread, "disk_w": ri.ri_diskio_byteswritten,
                "idle_wkups": ri.ri_pkg_idle_wkups}


def make_sampler():
    if sys.platform == "darwin":
        return MacSampler()
    if sys.platform.startswith("linux"):
        tck = os.sysconf("SC_CLK_TCK")
        return lambda pid: linux_sample(pid, clk_tck=tck)
    raise SystemExit(f"sampling is not implemented for {sys.platform}")


# ---------------------------------------------------------------- CSV


class CsvWriter:
    def __init__(self, f):
        self.f = f
        self.w = csv.writer(f)
        self.w.writerow(COLUMNS)

    def write(self, row: dict):
        self.w.writerow(["" if row[c] is None else (f"{row[c]:.6f}" if isinstance(row[c], float) else row[c])
                         for c in COLUMNS])


def read_phase(path) -> str:
    if not path:
        return ""
    try:
        return Path(path).read_text().strip()
    except OSError:
        return ""


def check() -> int:
    """Samples this process: a sanity check of the platform code before a long run."""
    sampler = make_sampler()
    first = sampler(os.getpid())
    deadline = time.process_time() + 0.3
    while time.process_time() < deadline:  # burn a little CPU, so the counter must move
        pass
    second = sampler(os.getpid())
    problems = []
    if not 0.2 <= second["cpu_s"] - first["cpu_s"] <= 5:
        problems.append(f"CPU moved {second['cpu_s'] - first['cpu_s']:.3f} s for about 0.3 s of work")
    if not 1_000_000 <= second["footprint"] <= 4_000_000_000:
        problems.append(f"footprint {second['footprint']} bytes")
    if not 1_000_000 <= second["rss"] <= 4_000_000_000:
        problems.append(f"resident {second['rss']} bytes")
    if not 1 <= second["threads"] <= 100:
        problems.append(f"{second['threads']} threads")
    if problems:
        print("sampler check failed: " + "; ".join(problems), file=sys.stderr)
        return 1
    print(f"sampler check: cpu {second['cpu_s']:.2f} s, footprint {second['footprint'] / 1e6:.1f} MB, "
          f"resident {second['rss'] / 1e6:.1f} MB, {second['threads']} threads, idle wake-ups {second['idle_wkups']}")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--pid", action="append", default=[], metavar="LABEL=PID", help="a process to sample (repeat)")
    ap.add_argument("--out", type=Path, help="the CSV to write")
    ap.add_argument("--phase-file", help="file holding the current scenario name")
    ap.add_argument("--interval", type=float, default=1.0)
    ap.add_argument("--duration", type=float, default=0, help="stop after this many seconds (0 = until stopped)")
    ap.add_argument("--check", action="store_true", help="sample this process once and check the numbers")
    args = ap.parse_args()
    if args.check:
        return check()
    if not args.pid or not args.out:
        ap.error("--pid and --out are required")
    targets = {}
    for item in args.pid:
        label, _, pid = item.partition("=")
        targets[label] = int(pid)
    sampler = make_sampler()
    stop = []
    signal.signal(signal.SIGTERM, lambda *_: stop.append(1))
    signal.signal(signal.SIGINT, lambda *_: stop.append(1))
    start = time.monotonic()
    tick = 0
    with open(args.out, "w", newline="") as f:
        out = CsvWriter(f)
        while targets and not stop:
            phase = read_phase(args.phase_file)
            now = time.time()
            for label, pid in list(targets.items()):
                try:
                    row = sampler(pid)
                except ProcessLookupError:
                    print(f"sample.py: {label} (pid {pid}) is gone", file=sys.stderr)
                    del targets[label]
                    continue
                out.write({"t": round(now, 3), "agent": label, "scenario": phase, **row})
            f.flush()
            tick += 1
            if args.duration and tick * args.interval >= args.duration:
                break
            while not stop:  # short naps, so a stop is seen within 0.2 s
                left = start + tick * args.interval - time.monotonic()
                if left <= 0:
                    break
                time.sleep(min(left, 0.2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
