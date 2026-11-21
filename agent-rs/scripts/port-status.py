#!/usr/bin/env python3
"""Porting coverage: every C# test of the agent against its Rust port.

    python3 agent-rs/scripts/port-status.py            # rewrite agent-rs/PORTING.md
    python3 agent-rs/scripts/port-status.py --check 1  # exit 1 if a phase-1 C# test has no Rust test

Naming rule: C# test `Class.Method` is ported as a Rust test named `method_snake_case` (in the module that ports the
class). A C# test that cannot apply gets a line `// not-ported: Class.Method: reason` in the Rust test module.
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CS_TESTS = ROOT / "agent" / "PhoneBox.Agent.Tests"
RUST = ROOT / "agent-rs"
OUT = RUST / "PORTING.md"

BASELINE = "C# baseline at bb72345 (0.9.23 + battery-care fix) on Linux (net10.0, hetzner-iddhi): 1623 test cases passed, 0 failed, 0 skipped."

# Phase of each C# test file (see docs/superpowers/plans/2026-11-20-rust-agent-roadmap.md). Files not listed belong to
# a later plan and are assigned when that plan is written.
PHASE_BY_FILE = {
    "ConfigTests": 1, "ConfigPathTests": 1, "ConfigLegacyOptionsTests": 1, "ConfigVersionTextTests": 1,
    "EmptyPageConfigTests": 1, "StackConfigTests": 1, "PrivateFilesTests": 1, "EndpointTests": 1,
    "FileLoggerTests": 1, "WebViewDoctorTests": 1, "PhonesTests": 1, "PhoneRegistryConcurrencyTests": 1,
    "RequestTimingsTests": 1, "StateFrameCacheTests": 1, "StateSignalTests": 1, "PushCapacityTests": 1,
    "PushInputTests": 1, "PushShutdownTests": 1, "PushVersionTests": 1, "AdbReverseTests": 1,
    "AdbUnreachablePhoneTests": 1,
    "GpuSourceTests": 2, "GpuIntegratedMergeTests": 2, "AppleGpuTests": 2, "CpuTempTests": 2,
    "CpuTempMalformedTests": 2, "AppleSmcTests": 2, "BatteryTests": 2, "BatteryParsingTests": 2,
    "ListeningPortsTests": 2, "PingMonitorTests": 2, "NetworkAdapterTests": 2, "NetworkRatesTests": 2, "ShTests": 2,
    "DrmUsageTests": 2,
    "ShortcutIconUploadsTests": 3, "AudioSetVolumeTests": 3, "MacMediaTests": 3, "SpectrumAnalyzerTests": 3,
    "TrackpadTests": 3, "DictationTests": 3, "KeySenderTests": 3, "AppCatalogTests": 3, "ShortcutIconTests": 3,
    "MediaArtworkTests": 3, "MediaArtworkBackoffTests": 3, "IconEncodingTests": 3, "AudioCacheTests": 3,
    "TerminalStandInTests": 3,
    "PairingTests": 4,
}
# Files that mix phases: the phase of single methods ("Class.Method"); the rest of such a file stays unassigned.
PHASE_BY_METHOD: dict[str, int] = {
    # ConfigTests.cs also holds the Wi-Fi checks (phase 4) and the network adapter filter (phase 2)
    "LanTests.OnlyThePhoneApiIsReachableOverWifi": 4,
    "LanTests.TheWebhookKeyOpensOnlyWebhooks": 4,
    "LanTests.TokensAreComparedExactly": 4,
    "LanTests.VirtualAdaptersAreSkippedUnlessTheyCarryTheDefaultRoute": 2,
    # needs Lan.AllowedOverLan, which comes with the Wi-Fi listener
    "SpectrumAnalyzerTests.ThePhoneMayAskForBandsOverWifi": 4,
    "CameraFeedsTests.ThePhoneMayAskForCameraPicturesOverWifi": 4,
    "WidgetFeedProxyTests.ProxyEndpointStaysLocalAndOffLanAllowlist": 4,
    # AppRestartTests.cs also holds SessionEndingTests (Windows session end, with the tray in a later plan)
    "AppRestartTests.TheSetupLoginItemAndSystemdAreServiceManagers": 1,
    "AppRestartTests.ATerminalOrTheTrayIsNot": 1,
    "AppRestartTests.AProcessThatOnlyInheritedTheUnitIsNot": 1,
    "AppRestartTests.TheRestartCodeIsAFailure": 1,
    # RuleTests.cs also holds the USB doctor (phase 1, with AdbReverse)
    "UsbDoctorTests.ChargingWithoutDataBlamesTheCable": 1,
    "UsbDoctorTests.WirelessChargingWithoutUsbDoesNotBlameACable": 1,
    "UsbDoctorTests.BatteryReportsFromBeforeAClockRollbackDoNotRaiseADiagnosis": 1,
    "UsbDoctorTests.ConnectedWithAdbButUnseenBlamesThePort": 1,
    "UsbDoctorTests.ConnectedWithoutAdbAsksForDebugging": 1,
    "UsbDoctorTests.StaysQuiet": 1,
    "UsbDoctorTests.NoReportNoDiagnosis": 1,
    # MonitorAuditTests.cs spans several monitors; these two are ProcessMonitor's (the rest: plans 5 and 6)
    "MonitorAuditTests.ProcessDetailToleratesDuplicateSessionPidsAndLocksAgentProcess": 2,
    "MonitorAuditTests.ColdProcessIconDoesNotCacheNullAndShortenArgsHandlesTabSeparators": 2,
    # shortcut and icon checks that live in audit and pairing files (the rest of those files: plans 4-6)
    "ShortcutRuleFollowUpTests.ThePhoneDoesNotGetTheConfiguredTextOfAFailedShortcut": 3,
    "ShortcutRuleFollowUpTests.ThePhoneStillGetsTheAgentsOwnAccessibilityHint": 3,
    "StoreFollowUpTests.AFormatChangeThatCannotRetireTheOldFileKeepsTheOldIcon": 3,
    "PairingTests.ShortcutIconUploadRejectsConcurrentSettingsSaveAcrossBodyReadWithoutOrphans": 3,
    "RuleTests.MacroShortcutStepsUpdateToggleStateAndRunOffCmd": 3,
    # StateSignal.MediaChanged compares media snapshots: it comes with the media monitor
    "StateSignalTests.AMediaChangeThatMattersPokesButTheTickingPositionDoesNot": 3,
}

# xUnit test classes are public; private helpers (Fixture, Handler, FakeRun…) and text such as "<class AGX…>" inside a
# string must not take over the methods that follow them
CLASS_RE = re.compile(r"^\s*public\s+(?:(?:sealed|static|partial|abstract)\s+)*(?:class|record)\s+([A-Z][A-Za-z0-9_]*)")
ATTR_RE = re.compile(r"^\s*\[(Fact|Theory)\b")
METHOD_RE = re.compile(r"\b(?:void|Task|ValueTask)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")
RUST_TEST_ATTR_RE = re.compile(r"^\s*#\[(?:test|tokio::test\b[^\]]*)\]")
RUST_FN_RE = re.compile(r"\bfn\s+([a-z_][a-z0-9_]*)\s*[<(]")
NOT_PORTED_RE = re.compile(r"//\s*not-ported:\s*([A-Za-z0-9_]+\.[A-Za-z0-9_]+)\s*:\s*(.+)$")


def snake(name: str) -> str:
    s = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1_\2", name)
    s = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s)
    return re.sub(r"_+", "_", s).strip("_").lower()


def csharp_tests() -> list[tuple[str, str, str]]:
    """(file stem, class, method) for every [Fact] / [Theory] method."""
    found = []
    for path in sorted(CS_TESTS.glob("*.cs")):
        cls = path.stem
        pending = False
        for line in path.read_text(encoding="utf-8").splitlines():
            if (m := CLASS_RE.search(line)) and "new " not in line:
                cls = m.group(1)
            if ATTR_RE.search(line):
                pending = True
                continue
            if pending and (m := METHOD_RE.search(line)):
                found.append((path.stem, cls, m.group(1)))
                pending = False
    return found


def rust_tests() -> tuple[set[str], dict[str, str]]:
    names: set[str] = set()
    not_ported: dict[str, str] = {}
    for path in RUST.rglob("*.rs"):
        if "target" in path.parts:
            continue
        pending = False
        for line in path.read_text(encoding="utf-8").splitlines():
            if m := NOT_PORTED_RE.search(line):
                not_ported[m.group(1)] = m.group(2).strip()
            if RUST_TEST_ATTR_RE.search(line):
                pending = True
                continue
            if pending and (m := RUST_FN_RE.search(line)):
                names.add(m.group(1))
                pending = False
    return names, not_ported


def phase_of(stem: str, cls: str, method: str) -> int | None:
    return PHASE_BY_METHOD.get(f"{cls}.{method}", PHASE_BY_FILE.get(stem))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", type=int, metavar="PHASE", help="exit 1 if a C# test of this phase is not ported")
    args = ap.parse_args()

    tests = csharp_tests()
    names, not_ported = rust_tests()
    rows, missing = [], []
    for stem, cls, method in tests:
        key = f"{cls}.{method}"
        phase = phase_of(stem, cls, method)
        rust = snake(method)
        if key in not_ported:
            status, where = "not ported", not_ported[key]
        elif rust in names:
            status, where = "ported", f"`{rust}`"
        else:
            status, where = "todo", f"`{rust}`"
            if args.check is not None and phase == args.check:
                missing.append(key)
        rows.append((key, "" if phase is None else str(phase), where, status))

    done = sum(1 for r in rows if r[3] != "todo")
    lines = [
        "# Porting coverage",
        "",
        "Generated by `python3 agent-rs/scripts/port-status.py`; do not edit by hand.",
        "",
        BASELINE,
        "",
        f"C# test methods: {len(rows)} · ported or ruled out: {done} · to do: {len(rows) - done}",
        "",
        "| C# test | Phase | Rust test / reason | Status |",
        "|---|---|---|---|",
    ]
    lines += [f"| `{k}` | {p} | {w} | {s} |" for k, p, w, s in rows]
    OUT.write_text("\n".join(lines) + "\n", encoding="utf-8")

    if missing:
        print(f"{len(missing)} phase-{args.check} C# tests have no Rust test:", file=sys.stderr)
        for key in missing:
            print(f"  {key}", file=sys.stderr)
        return 1
    print(f"{OUT.relative_to(ROOT)}: {len(rows)} C# tests, {done} done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
