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
    # phase 5 (AI and dev)
    "QuestionPromptTests": 5, "ApprovalOutcomeTests": 5, "ApprovalExpiryTests": 5, "SessionDetailsTests": 5,
    "SessionTitleTests": 5, "WorkBuddyNamesTests": 5, "ClaudeQuotaBackoffTests": 5, "ClaudeQuotaCacheTests": 5,
    "ClaudeQuotaScheduleTests": 5, "CodexQuotaTests": 5, "QuotaConnectTests": 5, "AntigravityTests": 5,
    "CodexBarTests": 5, "ClaudeTranscriptTests": 5, "DockerTests": 5, "DockerActionTests": 5, "ToolProcessTests": 5,
    "CommandVisibilityTests": 5,
    # phase 6 (integrations): rules, charge guard, privacy, session lock, app console, notes, webhooks
    "RuleTests": 6, "ChargeRecoveryTests": 6, "ChargeGuardSwitchTests": 6, "PrivacyMonitorTests": 6,
    "AppConsoleTests": 6, "AppConsoleFocusRaceTests": 6, "NoteEditTests": 6, "ShortcutRuleFollowUpTests": 6,
    "ScreenSyncTests": 6, "ScreenCancellationTests": 6, "AuditRegressionTests": 6, "TestPortsTests": 6,
    # plan-1 code whose tests were never assigned; ported with plan 6
    "CoreAuditTests": 6, "AppearanceTests": 6, "StoreFollowUpTests": 6,
    # phase 6: Home Assistant, OBS, Discord, Spotify; feeds, calendar, photos, cameras; the control deck
    "IntegrationVisibilityTests": 6, "HomeAssistantTests": 6, "HomeAssistantStateTests": 6, "ObsTests": 6,
    "ObsPermissionTests": 6, "DiscordTests": 6, "SpotifyControlTests": 6, "SpotifyCredentialTests": 6,
    "FeedTests": 6, "WidgetFeedProxyTests": 6, "WidgetFeedProxyTransportTests": 6, "IcsTests": 6,
    "CalendarOverlapTests": 6, "PhotoFrameTests": 6, "CameraFeedsTests": 6, "ControlDeckTests": 6,
    "ControlDeckCommandTests": 6, "ControlDeckPanicTests": 6, "ControlTasksTests": 6, "DeckAuditTests": 6,
    "HlsRadioTests": 6, "PortableAudioPlaybackTests": 6, "RadioCardViewsTests": 6, "RadioDirectoryTests": 6,
    "RadioDirectoryBodyTests": 6,
    # phase 7 (lifecycle)
    "UpdateChecksumTests": 7, "SingleFileMigrationTests": 7,
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
    # mixed files: the AI and dev monitors' checks (phase 5); CodexBarTests' RuleEngine check comes with the rules (6)
    "MonitorAuditTests.CodexBarTextsDoNotDependOnTheWindowsLocale": 5,
    "MonitorAuditTests.ClaudeSessionFileOfAReusedPidIsNotShownAsALiveSession": 5,
    "MonitorAuditTests.QuotaLimitWhoseWindowHasResetIsNotReportedAsUsedUp": 5,
    "MonitorAuditTests.CodexRolloutsUnderOneMegabyteDoNotDropFirstLineTurnContextOrRateLimits": 5,
    "MonitorAuditTests.CodexProjectComesFromALongOrBomPrefixedFirstRecordAndWaitsForAnIncompleteOne": 5,
    "AuditRegressionTests.ABackgroundProcessLeftByTheCommandDoesNotBlockTheWidget": 5,
    "StoreFollowUpTests.ReplacingClaudeSettingsKeepsThemOwnerOnly": 5,
    "StoreFollowUpTests.AMalformedHooksValueIsLeftAsItIs": 5,
    "CodexBarTests.QuotaAboveRuleKeepsFiringForCachedLimitsWithErrorAndSkipsOmittedProviders": 6,
    # RuleTests.cs also holds the update service checks (plan 7) and the browser guard (plan 1, ported)
    "UpdateServiceTests.TokenOnlyGoesToTheGitHubApi": 7,
    "UpdateServiceTests.TimeoutsAndHttpErrorsAreTransientButOurCancellationIsNot": 7,
    "BrowserGuardTests.ForeignBrowserRequestsAreRefused": 1,
    # AppRestartTests.cs: the Windows session end (adb), with the tray
    "SessionEndingTests.AdbWaitsAMinuteAfterTheSessionStartsEnding": 7,
    # MonitorAuditTests.cs: the calendar and charge guard checks
    "MonitorAuditTests.CalendarsWithTheSameNameDoNotMultiplyEventsWhileAFeedFails": 6,
    "MonitorAuditTests.ChargeGuardKeepsTryingToRestoreThePowerAfterAFailedSwitchOn": 6,
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
