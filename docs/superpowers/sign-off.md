# Rust agent: sign-off record

The record for owner gate 1 (release 1.0.0) and owner gate 2 (removing `agent/`): every switch-over criterion of the
spec and every roadmap exit check, with its status and evidence or the step still pending; the accepted risks the
1.0.0 release notes must list; and the decisions still open for the owner.

State: `main`, 2026-11-22, after plan 8 Task 13: owner gate 1 passed, 1.0.0 published from `main` and installed on
the owner's MacBook in place of the C# agent. Owner gate 2 (Task 14, removing `agent/`) is still open. Updated at
each gate.

- **Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md` ("Verification", "Switch-over").
- **Roadmap:** `docs/superpowers/plans/2026-11-20-rust-agent-roadmap.md`.
- **Plan 8:** `docs/superpowers/plans/2026-11-22-rust-agent-plan-8-sign-off.md` (Task 14 is the pending step
  named below).
- **Evidence** cited as "plan N, Task M" is that task's entry in the plan's ledger
  (`.superpowers/sdd/<date>-rust-agent-plan-<N>-*/progress.md`, kept out of git on the owner's machine). Plan 1 has no
  ledger; its evidence is the body of PR #1 on the fork ("PR1").
- **Status:** **met**, **partly** (some of it shown, the rest pending), **pending** (nothing shown yet).
- **Test server:** `hetzner-iddhi` (Linux x86_64). The C# agent at `bb72345` (0.9.23 + the battery-care fix) is the
  reference.

## 1. Spec switch-over criteria

| # | Criterion | Status | Evidence | Pending step |
|---|---|---|---|---|
| S1 | Every route passes the parity harness on every platform | met | Linux: phases 1–7 (30, 26, 26, 23, 21, 128 and 9 requests) with 0 differences, and all 96 C# route paths routed in Rust (plan 7, Task 13; plan 8, Task 5). macOS (Apple silicon) and Windows: phases 1–7 green in `rust.yml`'s `parity` job on every run since 44997776943, and in both release runs (plan 8, Tasks 10–11). macOS Intel is not run by the harness — a ruling (Task 10): behavior does not depend on the processor, and the Intel build's own tests pass natively on the self-hosted macOS 15 Intel runner and under Rosetta 2. | — |
| S2 | The test coverage table has no gaps | met | `agent-rs/PORTING.md`: 898 C# test methods, 898 ported or ruled out (3 ruled out, each with its reason), 0 to do; the C# baseline ran 1623 test cases, all passed. Rust: 1310 + 2 tests pass on Linux x64 and arm64, macOS arm64, macOS Intel (native and Rosetta 2) and Windows in CI, the Windows- and macOS-only tests included (plan 8, Task 10). | — |
| S3 | The new browser suite passes against the Rust agent | met | Plan 8 Task 7: the live-agent suite (settings load and save, the `X-Config-Version` conflict, the live preview, the pairing dialog and QR codes, shortcut icon upload and removal) against C# first, then Rust; CI job `ui-live` green on every run since the push (Task 10). | — |
| S4 | The macOS real-device and hardware checks pass | met | MacBook hardware comparisons of the two agents: `pc` and `ping` (plan 2, Task 14), desk controls (plan 3, Task 12), the Wi-Fi certificate and pairing (plan 4, Task 8), sessions, quotas and a permission prompt (plan 5, Task 14), integrations (plan 6, Task 32). The phone checks P1–P7 on the owner's phone (plan 8, Task 12; section 3). | — |
| S5 | The Rust agent is lighter on every measured axis on the owner's MacBook | met | Plan 8, Task 13: `agent-rs/scripts/mac-resources.sh`, two 34-minute runs side by side with the owner's config, C# 0.9.23 against the 1.0.0 build (CI package of ffd89b6): CPU idle / phone / pages 12.2 / 12.1 / 14.1 % -> 6.4 / 5.8 / 7.4 %, memory 230.9 -> 13.6 MB, threads 35 -> 22, idle wake-ups 3.5 -> 3.1 per second, installed 113.6 -> 13.6 MB, download 44.3 -> 5.8 MB, disk writes 0 on both. The first run (alpha.2) failed on wake-ups (12.5 vs 3.9); fixed in ffd89b6 (tools waited for with poll(2), the media thread with one timed wait). | — |
| S6 | 1.0.0 carries settings, pairings, icons and pushed values over | met | The switch on the owner's MacBook (plan 8, Task 13): `main`'s `setup.sh` (dry run, then `--yes`) replaced the running C# 0.9.23 with 1.0.0 in place under the same launchd job; `lan-cert.pfx`, `data.json` and `icons/` byte-identical to the backup taken before, `config.json` kept with only the designed normalizations on its first save (`updates.repo` to the fork, the `display.openOnConnect` default); the phone came back over Wi-Fi with its existing pairing, no re-pair, and the agent updated the phone app to 1.0.0 over adb by itself 30 s after start. Before that: the same folders and files, the C# agent's `lan-cert.pfx` read with the same fingerprint (plan 4, Task 8; P2), `config.json` byte parity (PR1), `setup.yml` and `lifecycle.yml` (plan 8, Task 11; and on `main` against the published 1.0.0, Task 13). | — |
| S7 | `agent/` and its tests are removed after the switch | pending | — | Owner gate 2 (Task 14): a final C#↔Rust parity run on every OS and `port-status.py --check` as evidence, then the removal. |

## 2. Roadmap exit checks (plans 1–7)

| Plan | Exit check | Status | Evidence | Pending step |
|---|---|---|---|---|
| 1 | The owner's phone shows the panel from the Rust agent over USB; the phase-1 corpus passes on Linux, macOS and Windows | met | P1 on the current code (plan 8, Task 12): the full panel over USB from 1.0.0-alpha.2. Phase 1: 0 differences on Linux (plan 8, Task 5), macOS and Windows (CI, Task 10). | — |
| 2 | `pc` and `ping` in `/api/state` match C# on the MacBook hardware run | met | Plan 2, Task 14: `mac-state-compare.sh` and `compare-pc-ping.py`. | — |
| 3 | Media, volume, shortcuts and apps work from the phone on the MacBook | met | Plan 3, Task 12: "desk controls match" between the two agents on the MacBook. P3 (plan 8, Task 12): from the phone, the volume (over USB and over Wi-Fi), media play and pause, an app (Calculator) and a shortcut (Mute on and off). | — |
| 4 | The phone switches USB ⇄ Wi-Fi with an existing pairing, no re-pair | met | Plan 4, Task 8: the Rust agent read the installed agent's `lan-cert.pfx` (same fingerprint), `/api/pair` matched, the served certificate matched. P2 (plan 8, Task 12): the phone came over Wi-Fi to the Rust agent with the pairing it made with C#, TLS on Android 8 included; again with the fork's app after it paired once. | — |
| 5 | Sessions, quotas and a Claude Code permission prompt round-trip on the MacBook | met | Plan 5, Task 14 (through a stand-in for the phone). P4 (plan 8, Task 12): the installed hook's prompt showed on the real phone and Allow there reached the hook; sessions, the GitHub user and quotas the same as C#. | — |
| 6 | The full route corpus passes on all platforms | met | Linux phases 1–7, 0 differences (plans 6–8); macOS and Windows phases 1–7 on CI (plan 8, Task 10); MacBook integrations match (plan 6, Task 32). P5 (plan 8, Task 12): locking the Mac put the phone to sleep within a second and unlocking woke it; with a secure lock screen on the phone the panel shows after the phone's own unlock, as with C#. | — |
| 7 | A `1.0.0-alpha.N` pre-release installs with `setup.sh` on the MacBook and updates itself to the next alpha | met | `remote-update-check.sh` on the test server (plan 7, Task 13). Published: 1.0.0-alpha.1 and 1.0.0-alpha.2 (pre-releases), then 1.0.0. `lifecycle.yml` on `macos-latest`: `setup.sh` installed alpha.1, which updated itself to alpha.2 by check and apply (plan 8, Task 11); from `main`, alpha.2 updated itself to the published 1.0.0 (Task 13). `setup.yml` installed alpha.2, and from `main` the latest release, 1.0.0, replacing a running agent. The owner's MacBook went from C# 0.9.23 straight to 1.0.0 with `setup.sh` (S6): the alpha-to-alpha step ran on CI's Macs, not on the MacBook (a ruling, Task 13). | — |

## 3. Steps since Stage A, and what gate 1 still runs

**CI (Task 10), done:** the `rust.yml` matrix (fmt, clippy and tests on Ubuntu x64 and arm64, macOS arm64, macOS
Intel under Rosetta 2, Windows) green, with the Windows-only code and tests, the `build.rs` resources, the
`ProductVersion` readback and the CRLF log test; parity phases 1–7 on macOS (Apple silicon) and Windows; the package
job (throwaway macOS certificate, PE checks, glibc floor, smoke test, zip); `ui-live`. A self-hosted macOS 15 Intel
runner (a throwaway VM per job on the test server, never for fork pull requests or releases) also runs clippy, the
tests, the release build and the smoke test natively.

**Phone (Task 12; Huawei RNE-L22, Android 8), done:** P1 USB on the current code; P2 Wi-Fi with the existing pairing;
P3 desk controls from the phone; P4 a Claude prompt answered on the phone; P5 follow the computer's lock; P6 an
Android 8 phone on Wi-Fi adb that refuses `adb reverse` (logged once, the round goes on, the panel stays on Wi-Fi);
P7 the agent installed the fork's APK over USB after the upstream app was uninstalled (with the owner's consent), and
the fork's app then paired once over Wi-Fi.

**Releases (Task 11), done:** 1.0.0-alpha.1 and 1.0.0-alpha.2 built, reviewed (assets, checksums, zip contents,
architectures, macOS signatures) and published as pre-releases; `setup.yml` and `lifecycle.yml` (alpha.1 → alpha.2)
green. `lifecycle.yml` ran from a throwaway branch until the merge; it is on `main` now.

**Found by P7, for the switch procedure:** the agent looks for a missing app only while its own `adb reverse` rule is
absent (as C# does), so after uninstalling the old app the user unplugs the phone and plugs it back in (or restarts
the agent) before Settings › Install the phone app offers the install.

**Gate 1 (Task 13), done:** the owner consented to 1.0.0 and to switching the MacBook, picked the winget identifier
`iddhi-sulakshana.PhoneBox`, kept the original project's pairing page and accepted the two config-null divergences.
The 1.0.0 bump: `build.yml`'s metadata job runs `check-release.py --final`; the `crate_version_is_alpha` test is gone;
the Scoop bucket and the winget manifests point at the fork; `remote-update-check.sh` runs alpha → 1.0.0; the setup
scripts, `setup.yml` and `RELEASING.md` drop the pre-release wording; the CHANGELOG carries the switch procedure (with
the replug step above), the resource table and section 4. The resource run (S5) failed once on idle wake-ups and
passed after the fix. Two drafts were reviewed and deleted, PR #1 merged (c49d459), 1.0.0 published from `main` as the
latest release (the bot committed the Scoop and winget manifests, 554bfae), `lifecycle.yml` (alpha.2 → 1.0.0) and
`setup.yml` green from `main`, and the MacBook switched (S6).

**Fixed in plan 8 Stage A:** the push-store deadlock when a value expires before the 2 s save (Task 1); the panic on a
non-ASCII app name, and a null `night.start` / `night.end` in a save refused as in C# (Task 2); a BOM before
`PUT /api/settings` no longer skips the Wi-Fi checks, and generated keys that cannot be written stop start-up as in
C# (Task 3, which found three other review findings to match C# already); `agent.log` lines end with CRLF on Windows,
and the CI path filters cover the embedded files (Task 4); the embedded files moved to `agent-rs/assets/` (Task 5).

**Known minors, not criteria:** the upstream-repo rewrite matches `ersinkoc/phonebox` exactly (other casing or stray
spaces keep the upstream channel); nothing checks that the Android `versionCode` rises from the alphas to 1.0.0; no
round-trip test for unknown fields at widget, shortcut and rule level; single-instance errors read "already running";
no test for the "adb not found" path.

## 4. Accepted risks for the 1.0.0 release notes

Required by the spec:

1. **Windows and Linux were not checked on real hardware** (no Windows PC or Linux desktop): the tray, per-app audio,
   GPU counters, Razer batteries, X11 idle time and the session-end guard rest on the ported tests and the parity
   harness on CI and the test server.
2. **The APK is signed with the fork's key.** Each phone uninstalls PhoneBox once at the switch and installs the
   fork's app; later updates install normally.
3. **macOS users re-grant Accessibility and Automation once** (the fork's own certificate). The Windows agent is not
   code-signed.
4. **Deliberate departures from the C# agent:**
   - unknown `config.json` fields are kept on save;
   - a phone on Wi-Fi adb that refuses the reverse rule no longer ends the round;
   - updates come from the fork, and a saved `updates.repo` of `ersinkoc/phonebox` is rewritten to the fork; the
     pairing dialog's app link uses the configured repo;
   - a null `night.start` / `night.end` in a hand-edited `config.json` loads as the default times (C# keeps the null).

From the ledgers and PR #1 (user-visible):

5. **Claude quota behind a proxy or a TLS-inspecting CA:** the client ignores the system proxy, follows no redirects
   and trusts the webpki roots, not the system store; the quota then shows "Could not reach Anthropic".
6. **Pairing goes through the original project's page** (`ersinkoc.github.io/phonebox/pair/`), whose "Download the
   app" offers that project's APK, signed with the other key (the owner kept that page at gate 1).
7. **Wi-Fi details:** ALPN offers HTTP/1.1 only; a percent-encoded API path gets 403 over Wi-Fi (TLS on Android 8 is
   proven on the owner's phone, P2).
8. **Feed read state may not carry over:** item ids keyed by link come from the WHATWG URL parser.
9. **One icon refetch after the switch:** the program-icon revision differs.
10. **Process start times have whole-second precision.**
11. **A missing `adb devices` answer clears the phone list** (up to 4 s "not connected").
12. **Byte-level `config.json` differences:** the escaping of emoji, NBSP and private-use characters, and numbers in
    free-form widget options; both agents read either form.
13. **A busy-port `crit` line no longer says "Kestrel".**
14. **macOS process group after a crash:** deck tasks can outlive a killed agent, as in C#.
15. **`setup.sh --pre` / `setup.ps1 -Pre` take the newest published release**, not the highest version.
16. **Minor visible behavior:** SVG uploads accept internal-DTD entities and refuse non-UTF SVGs; a typing tool can
    stay running after a dictation hang-up; ping caps DNS at 5 s; the macOS network rate dips every 4 GiB.
17. **The two config-null divergences of plan 8 Task 3**, accepted by the owner at gate 1 (section 5).

## 5. Owner decisions

Settled at gate 1 (plan 8, Task 13):

- **The pairing host:** the original project's page (`ersinkoc.github.io/phonebox/pair/`) stays; the fork's Pages
  workflow is disabled. The live-demo links in the READMEs, `docs/DEVELOPMENT.md` and the issue template point there.
- **The winget identifier:** `iddhi-sulakshana.PhoneBox` (publisher `iddhi-sulakshana`). The release workflow commits
  the manifests to `main`; submitting them to winget-pkgs is a manual step.
- **The two config-null divergences ruled in plan 8 Task 3** (both reached only by hand-written API calls, both
  leaving a valid config): accepted as departures (section 4, item 17).
  - a saved null for a value field of a nested element type (`cameras.sources[].fps`, a deck action's `value`,
    `levels[].volume` / `muted`, `photos.subfolders`, `controls.allow*`) is dropped to the default, where C# refuses
    the save;
  - a null for a name that is a value field elsewhere but unknown to its containing type
    (`{"weather":{"ENABLED":null}}`) is refused, where C# ignores it.
- **The release secrets, the MacBook resource run and replacing the MacBook install:** done (S5, S6).
- **Consent to 1.0.0:** given.

Still open:

1. **Issues and private vulnerability reporting on the fork** (both off). `setup.sh` and `setup.ps1` link the
   fork's issues and `CONTRIBUTING.md` asks for an issue first; `SECURITY.md` and the issue template still link the
   original project's advisories, and `PRIVACY.md` its issues. Enable them and point those texts at the fork, or
   have them reworded.
2. **Owner gate 2 (Task 14):** after 1.0.0 has run a while, consent to removing `agent/` and its tests (S7).
