# Rust agent: sign-off record

The record for owner gate 1 (release 1.0.0) and owner gate 2 (removing `agent/`): every switch-over criterion of the
spec and every roadmap exit check, with its status and evidence or the step still pending; the accepted risks the
1.0.0 release notes must list; and the decisions still open for the owner.

State: branch `rust-agent`, 2026-11-22, after plan 8 Tasks 1–5 (Stage A). Updated at each gate.

- **Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md` ("Verification", "Switch-over").
- **Roadmap:** `docs/superpowers/plans/2026-11-20-rust-agent-roadmap.md`.
- **Plan 8:** `docs/superpowers/plans/2026-11-22-rust-agent-plan-8-sign-off.md` (Tasks 10–14 are the pending steps
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
| S1 | Every route passes the parity harness on every platform | partly | Linux: phases 1–7 (30, 26, 26, 23, 21, 128 and 9 requests) with 0 differences, and all 96 C# route paths routed in Rust (plan 7, Task 13); again 0 differences after the asset move, phase 1 comparing `/`, `/favicon.ico` and `/api/catalog` byte for byte (plan 8, Task 5). | macOS (arm64, Intel) and Windows parity in `rust.yml`'s `parity` job: not run since plan 1, whose last PR run (44750943690) failed `parity (macos-15-intel)` phase 1 on `/audio` before audio was ported. Runs after the push (Task 10). |
| S2 | The test coverage table has no gaps | met | `agent-rs/PORTING.md`: 898 C# test methods, 898 ported or ruled out (3 ruled out, each with its reason), 0 to do; the C# baseline ran 1623 test cases, all passed. Rust: 1293 + 2 tests pass on the server (plan 8, Task 3). The C# suite still passes with the moved resources (1623/1623, plan 8, Task 5). | The Windows- and macOS-only Rust tests (startup shortcut, job-object child, tray, `build.rs` resources and the `ProductVersion` readback) compile on the server and run only on CI (Task 10). |
| S3 | The new browser suite passes against the Rust agent | pending | — | Plan 8 Task 7: the live-agent suite (settings load and save, the `X-Config-Version` conflict, the live preview, the pairing dialog and QR codes, shortcut icon upload and removal) in the Playwright container on the test server, against C# first, then Rust; CI job `ui-live` after the push. |
| S4 | The macOS real-device and hardware checks pass | partly | MacBook hardware comparisons of the two agents pass: `pc` and `ping` (plan 2, Task 14), desk controls (plan 3, Task 12), the Wi-Fi certificate and pairing (plan 4, Task 8), sessions, quotas and a permission prompt (plan 5, Task 14), integrations (plan 6, Task 32). | The phone checks P1–P7 (section 3) with `agent-rs/scripts/mac-phone-check.sh` (USB and `--wifi`), `mac-dev-check.sh --phone`, the lock follow and the fork's APK over USB (Task 12). |
| S5 | The Rust agent is lighter on every measured axis on the owner's MacBook | pending | — | Plan 8 Task 8 builds the tooling (`tools/resources/`, `agent-rs/scripts/mac-resources.sh`) and proves it with a cross-check on the test server. The MacBook run (about 70 minutes) waits for the owner's go at gate 1 (Task 13). |
| S6 | 1.0.0 carries settings, pairings, icons and pushed values over | partly | The Rust agent uses the same folders and files; it read the installed C# agent's `lan-cert.pfx` with the same fingerprint (plan 4, Task 8); `config.json` byte parity (PR1); the updater swaps the program in the install folder (plan 7, Task 13). | A real switch on the MacBook with the fork's setup script (Tasks 11 and 13, with the owner's consent to replace the install). |
| S7 | `agent/` and its tests are removed after the switch | pending | — | Owner gate 2 (Task 14): a final C#↔Rust parity run on every OS and `port-status.py --check` as evidence, then the removal. |

## 2. Roadmap exit checks (plans 1–7)

| Plan | Exit check | Status | Evidence | Pending step |
|---|---|---|---|---|
| 1 | The owner's phone shows the panel from the Rust agent over USB; the phase-1 corpus passes on Linux, macOS and Windows | partly | PR1: the phone showed the panel over USB and CI was green on all 8 jobs of plan 1's head. Linux phase 1: 0 differences on the current code (plan 8, Task 5). | The phone check on the current code: it ran before the 0.9.23 rebase (P1, Task 12). macOS and Windows phase 1 after the push, including the failed `macos-15-intel` run (Task 10). |
| 2 | `pc` and `ping` in `/api/state` match C# on the MacBook hardware run | met | Plan 2, Task 14: `mac-state-compare.sh` and `compare-pc-ping.py`. | — |
| 3 | Media, volume, shortcuts and apps work from the phone on the MacBook | partly | Plan 3, Task 12: "desk controls match" between the two agents on the MacBook (92 apps, icon PNG, volume set and read back, a shortcut run). | Driving them from the phone (P3, Task 12). |
| 4 | The phone switches USB ⇄ Wi-Fi with an existing pairing, no re-pair | partly | Plan 4, Task 8: the Rust agent read the installed agent's `lan-cert.pfx` (same fingerprint), `/api/pair` matched, the served certificate matched, HTTPS answered 200 with the key and 403 without. | The phone half, `mac-phone-check.sh --wifi` (P2, Task 12). |
| 5 | Sessions, quotas and a Claude Code permission prompt round-trip on the MacBook | met | Plan 5, Task 14: the installed hook command run with a `PermissionRequest`, answered through a stand-in for the phone on a live stream; sessions, the GitHub user and the quota providers compared live with C# (3 sessions; no quota provider on either agent). | Confirming it on the real phone, `mac-dev-check.sh --phone` (P4, Task 12). |
| 6 | The full route corpus passes on all platforms | partly | Linux phases 1–6, 0 differences (plan 6, Task 32), and phases 1–7 since (plan 7, Task 13; plan 8, Task 5); MacBook integrations match (plan 6, Task 32). | macOS and Windows parity (Task 10); follow-the-lock on the Mac (P5, Task 12). |
| 7 | A `1.0.0-alpha.N` pre-release installs with `setup.sh` on the MacBook and updates itself to the next alpha | partly | On the test server, `agent-rs/scripts/remote-update-check.sh` (plan 7, Task 13): alpha.1 → alpha.2 by check and apply against a fake GitHub, relaunched as a new process; a wrong checksum and a program that does not start were refused with the install unchanged; no request carried a token. `setup.sh --pre` / `setup.ps1 -Pre` tests pass (plan 7, Task 12). | The owner's six secrets; alpha.1 and alpha.2 published; `lifecycle.yml` (self-update across two published alphas on `macos-latest`) and `setup.yml` (Task 11); the MacBook install replaced with an alpha only with the owner's consent. |

## 3. Pending steps in detail

**After the push (Task 10), on CI:**

- C1: the `rust.yml` matrix (fmt, clippy and tests on Ubuntu x64 and arm64, macOS arm64 and Intel, Windows) and
  parity phases 1–7 on macOS and Windows; none of it has run since plan 1.
- C2–C4: Windows-only code checked so far only by the cross-build (icons, D3DKMT/PDH, Razer HID, camera and privacy,
  the Core Audio and GSMTC deck, WinRT playback, the raw Win32 tray), the Windows-only tests (startup shortcut,
  job-object child), the `build.rs` resources and the `ProductVersion` readback; the Windows `agent.log` line-end test
  (CRLF), whose RED exists only on Windows.
- C5: the package job (throwaway macOS certificate, PE checks, glibc floor, smoke test, zip).
- The browser suite's `ui-live` job (Task 7).

**With the phone (Task 12; Huawei RNE-L22, Android 8):**

- P1 USB on the current code; P2 Wi-Fi with the existing pairing; P3 desk controls from the phone; P4 a Claude prompt
  answered on the phone; P5 follow the computer's lock (lock → phone asleep, unlock → awake with the panel); P6 an
  Android 8 phone on Wi-Fi adb that refuses `adb reverse` (the owner's requested departure); P7 the agent installs the
  fork's APK over USB (needs the signed APK and the owner's consent to uninstall the upstream app).

**Releases (Task 11):** alpha.1 as a draft, reviewed, published; alpha.2; `lifecycle.yml` alpha.1 → alpha.2 and
`setup.yml` (which skips every install step until an alpha is published).

**In the 1.0.0 bump (Task 13):** `build.yml`'s metadata job runs `check-release.py --final`; the
`crate_version_is_alpha` test changes; the Scoop bucket's `homepage` and `autoupdate.url` move to the fork; the winget
manifests take the identifier the owner picks; `remote-update-check.sh` runs alpha → 1.0.0; the setup headers,
`setup.yml` and `RELEASING.md` drop the pre-release wording; the CHANGELOG carries the switch procedure and section 4.

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
   app" offers that project's APK, signed with the other key (unless the owner moves the host, section 5).
7. **Wi-Fi details:** ALPN offers HTTP/1.1 only; a percent-encoded API path gets 403 over Wi-Fi; TLS on Android 8 is
   unproven on the phone if P2 is still open at the release.
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
17. **The two config-null divergences of plan 8 Task 3**, if the owner accepts them (section 5, item 7).

## 5. Open owner decisions

1. **The pairing host and the fork's GitHub Pages.** Keep the original project's page
   (`ersinkoc.github.io/phonebox/pair/`: works today, including direct app-open on Android below 12; its download
   link serves the other key's APK; another person's host), or switch on the fork's Pages and move pairing to
   `iddhi-sulakshana.github.io/phonebox/pair/` (edits to `pair.html`, `panel.spec.js` and `PRIVACY.md`; Android
   below 12 then goes through the page with one extra tap, unless the app also accepts the new host, which is an app
   change the spec rules out). The live-demo links in the READMEs, `docs/DEVELOPMENT.md` and the issue template
   point at the original project's Pages until then.
2. **The winget identifier and publisher.** `packaging/winget/make-manifests.py` still uses `ersinkoc.PhoneBox`, and
   its `PortableCommandAlias: phonebox` collides with that package on a machine that has both. Submitting to
   winget-pkgs is a manual step.
3. **Issues and private vulnerability reporting on the fork** (both off). `setup.sh` and `setup.ps1` link the
   fork's issues and `CONTRIBUTING.md` asks for an issue first; `SECURITY.md` and the issue template still link the
   original project's advisories, and `PRIVACY.md` its issues. Enable them and point those texts at the fork, or
   have them reworded.
4. **The six release secrets:** `PHONEBOX_KEYSTORE_BASE64`, `PHONEBOX_STORE_PASSWORD`, `PHONEBOX_KEY_ALIAS`,
   `PHONEBOX_KEY_PASSWORD`, `MACOS_SIGNING_P12_BASE64`, `MACOS_SIGNING_P12_PASSWORD`. Without them the release stops
   before publishing anything.
5. **The MacBook resource run:** about 70 minutes of two agents beside the installed one on spare ports, possibly
   with one-time Automation or Local Network prompts; it is S5's measurement.
6. **Replacing the MacBook install with an alpha** (plan 7's exit check and S6).
7. **The two config-null divergences ruled in plan 8 Task 3**, both reached only by hand-written API calls (the
   settings page never sends these nulls), both leaving a valid config:
   - a saved null for a value field of a nested element type (`cameras.sources[].fps`, a deck action's `value`,
     `levels[].volume` / `muted`, `photos.subfolders`, `controls.allow*`) is dropped to the default, where C# refuses
     the save;
   - a null for a name that is a value field elsewhere but unknown to its containing type
     (`{"weather":{"ENABLED":null}}`) is refused, where C# ignores it.

   Accept them as departures (item 17 of section 4), or have them fixed, which needs the reader to know each field's
   containing type.
8. **The gates themselves:** consent to 1.0.0 (Task 13) and, after it has run a while, to removing `agent/`
   (Task 14); reconnecting the phone and consenting to uninstall the upstream app on it (Task 12).
