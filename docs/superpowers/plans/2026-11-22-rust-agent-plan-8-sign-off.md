# Rust agent plan 8: Sign-off and switch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the Rust agent to the spec's switch-over bar: fix the open code gaps, take every build and asset off
`agent/` so the C# tree can go, add the live-agent browser suite and the resource comparison, prepare the docs and a
sign-off record; then, after the whole-branch review and the push, publish alphas and walk the two owner gates
(1.0.0, removal of `agent/`).

**Architecture:** Stage A (Tasks 1–9) is code and tooling on `rust-agent`, verified on the test server; it ends with
the whole-branch final review, one fix pass and the push. Stage B (Tasks 10–14) runs after the push and is gated:
CI, the owner's secrets, published alphas, the phone, then owner gate 1 (1.0.0) and owner gate 2 (removing `agent/`).

**Tech Stack:** Rust 1.98 (agent-rs), Python 3 stdlib tools, Playwright 1.65 (npm, `tools/ui-tests`, committed
lockfile) in the `mcr.microsoft.com/playwright:v1.65.0-noble` container on the server, GitHub Actions, bash and
PowerShell scripts.

**Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md` ("Verification", "Switch-over", "Decisions by the
owner", "Consequences"). Surveys in `.superpowers/sdd/2026-11-22-rust-agent-plan-8-sign-off/`: `survey-a.md` (browser
suite), `survey-b.md` (1.0.0 and the removal of `agent/`), `survey-c.md` (open items, exit checks, resource method,
accepted risks). Below "a§N" means survey-a section N.

## Global Constraints

- The C# agent stays the behavioral oracle until owner gate 2: its code is not changed; only its project file may
  change paths (Task 5). `agent/PhoneBox.Agent.csproj` stays `0.9.23`.
- The owner is asked before 1.0.0 (Task 13) and before removing `agent/` (Task 14); nothing in Stage A does either.
- Push only after the whole-branch final review and its fix pass (end of Stage A); alphas may be published after the
  push; secrets are set by the owner.
- Tests and servers run on `hetzner-iddhi` only (`agent-rs/scripts/remote-*.sh`, Python and pwsh tools through ssh
  after `sync_tree`, run from bash, never zsh). MacBook runs only beside the installed agent on spare ports with a
  scratch `HOME`, stopped by PID, and the long resource run only with the owner's go.
- No process-wide mutable statics; blocking work off the async workers.
- Commits are `Rust agent: …`, with no co-author trailer. Nothing secret is printed or committed.

## Review Focus

- A pushed value with a TTL that expires before the 2 s save: the save completes and shutdown does not hang
  (Task 1 test `a_value_that_expires_before_the_save_does_not_deadlock`).
- A non-ASCII app name in the console/rules app list (`猫猫`, `café.EXE`): no panic, `.exe` still stripped (Task 2).
- A settings page saved after an outside write: 409 and the draft is kept (Task 7 `settings.spec.js`).
- Moving the assets: `/`, `/favicon.ico` and `/api/catalog` stay byte-identical to C# (Task 5, parity phase 1).
- A source install with `scripts/install.sh` over a running agent: a failed build leaves the running install alone
  (Task 6, the existing contract tests, now with a `cargo` stub).

## Rulings made while writing this plan

- **The MacBook resource run is the sign-off measurement and runs at owner gate 1 with the owner's go** (about 70 min
  of two agents beside the installed one, possibly with one-time Automation/Local Network prompts); Stage A builds and
  proves the tooling with a server cross-check (c§4.5). Cost if wrong: the numbers come one step later.
- **The browser suite runs in the Playwright container on the server** (a§4 option A); the C# leg uses a
  self-contained publish if the server's SDK can make one, otherwise it runs on CI only (`ui-live` job). Cost if wrong:
  the suite is proven against C# only after the push.
- **Parity keeps C# as its reference until gate 2**; the switch of the parity job to "last published release" (b§2.2 a)
  is part of Task 14.
- **Docs for the switch are written in Stage A** but name nothing that needs an owner decision (pairing host, winget
  id, fork settings); translations get URL changes only.

---

### Task 1: Push store: no deadlock when a value expires before the save

**Files:** `agent-rs/crates/phonebox/src/push_store.rs` (+ its tests).

**Interfaces:** none new. `save_pending_locked` must not call anything that can reach `changed()` → `persist.lock()`
while the `persist` lock is held (c§1d G1): take the snapshot without `purge_expired`'s side effect (filter expired
values only), and let the purge's `changed()` run outside the lock.

- [ ] **Step 1: Test (RED)** `a_value_that_expires_before_the_save_does_not_deadlock`: set `x` with a 1 ms TTL through
  the store's setter, wait past the expiry, then call `dispose()` on a helper thread and require it to return within
  5 s (a `std::sync::mpsc` `recv_timeout`); `data.json` exists and lacks `x`.
- [ ] **Step 2:** RED on the server (timeout). **Step 3:** fix. **Step 4:** GREEN, mutation (restore the purge in the
  save path → the test times out), full check.
- [ ] **Step 5: Commit** `Rust agent: push store save without a relock`.

### Task 2: Config: non-ASCII app names and null night times

**Files:** `agent-rs/crates/phonebox/src/config/mod.rs` (`app_names`, c§1d G2), `config/read.rs` (`NULL_KEPT`,
G3), tests beside them.

- [ ] **Step 1: Tests (RED):** `app_names_with_non_ascii_text_do_not_panic` (`"猫猫, café.EXE, x.exe,é"` →
  `["猫猫", "café", "x", "é"]`); `a_null_night_start_or_end_is_refused_as_in_csharp` (read the C# validation for
  `night.start`/`night.end` first, `agent/PhoneBox.Agent/Config.cs`, and assert the same error text on a save, and
  the same load behavior).
- [ ] **Step 2:** RED. **Step 3:** byte-safe suffix check (`a.len() >= 4 && a.is_char_boundary(a.len() - 4)`);
  keep a null `night.start`/`night.end` so validation sees it, as C#. **Step 4:** GREEN, full check.
- [ ] **Step 5: Commit** `Rust agent: config edge cases from the review of PR 1`.

### Task 3: The remaining review findings against C#

**Files:** as each finding needs (c§1d G4): `adb.rs:276` (emulator/TCP devices reported as USB), `adb.rs:317`
(open-panel retry after a failed `am start`), `adb.rs:345` (one phone's rule error blocking the next),
`config/mod.rs:1271` (per-type null rules), `config/read.rs:47` (LAN validation of a BOM body), `config/read.rs:151`
(unknown-field casing per containing type), `config/store.rs:170` (unsaved generated keys reported).

- [ ] **Step 1:** For each finding, read the C# counterpart and decide: **parity** (Rust already does what C# does →
  ledger `Ruling: … matches C# <file:line>`), or **divergence** (Rust differs from C# → a failing test named after the
  C# behavior, then the fix).
- [ ] **Step 2:** Every divergence RED → GREEN; full check.
- [ ] **Step 3: Commit** `Rust agent: review findings matched to C#` (no commit when all are parity).

### Task 4: Windows log line ends and the CI path filters

**Files:** `agent-rs/crates/phonebox/src/file_logger.rs` (C# `WriteLine`: CRLF on Windows, LF elsewhere;
`FileLogger.cs:38`), `.github/workflows/rust.yml` (paths: add `android/app/src/main/assets/www/**`,
`agent/PhoneBox.Agent/**` until Task 5 moves the assets, `tools/ui-tests/**`).

- [ ] **Step 1: Test (RED):** `log_lines_end_as_the_platform_writes_them` (`"\r\n"` on Windows, `"\n"` elsewhere).
- [ ] **Step 2:** RED (Windows-only branch: compile in the cross-check; on Linux the assertion of `"\n"` stays green —
  mutate the Windows constant to prove the test reads it). **Step 3:** implement. **Step 4:** GREEN, cross-check.
- [ ] **Step 5: Commit** `Rust agent: platform line ends in agent.log; CI paths for the embedded files`.

### Task 5: The embedded files move to `agent-rs/assets/`

**Files:** `git mv` `agent/PhoneBox.Agent/{settings.html,phonebox.ico,widgets.json,templates.json,
config.default.json,config.default.macos.json,config.default.linux.json}` → `agent-rs/assets/`; the csproj's
`EmbeddedResource Include` and `ApplicationIcon` paths (keep every `LogicalName`; b§1.3); Rust paths in `catalog.rs`,
`app_paths.rs`, `server/static_files.rs`, the tests that read them, `phonebox-agent/build.rs` (prefer
`concat!(env!("CARGO_MANIFEST_DIR"), …)`); every other reader b§1.2 lists (`tools/check-release.py`,
`tools/pages/build.py`, `tools/demo-server.py`, the UI test, `make-icon.ps1`, `.github/workflows/pages.yml`,
`agent-rs/scripts/remote-update-check.sh`, READMEs that name the path); `rust.yml` paths (`agent-rs/**` now covers
them; drop the Task 4 `agent/PhoneBox.Agent/**` entry if nothing else needs it).

- [ ] **Step 1:** Move and repoint. **Step 2: Proof:** `dotnet build` and `dotnet test` of the C# project on the
  server (the oracle still builds with the same resources), `remote-check.sh`, cross-check, parity phase 1 (byte
  comparison of `/`, `/favicon.ico`, `/api/catalog`) and phases 2–7, `check-release.py`, the pages build,
  `tools/test-release-metadata.py`.
- [ ] **Step 3: Commit** `Rust agent: embedded files move to agent-rs/assets`.

### Task 6: Source installers and their tests on cargo

**Files:** `scripts/install.sh`, `scripts/install.ps1`, `scripts/build.ps1` (host-only: version from
`agent-rs/Cargo.toml`, `cargo build --release --locked -p phonebox-agent`, `tools/zip-agent.py`, the APK),
`scripts/release.ps1` (fork repo, `-Ref`, `-Final` → `-f final=true` and `check-release.py --final`; compare with the
fork's main through `gh api`), `.github/workflows/build.yml` (a dotnet-free `installers` job takes the installer
regression steps out of the C# `agent` job, b§2.1), tests `tools/test-unix-installer.py`,
`tools/test-unix-autostart.py` (no `DOTNET_ROOT` in the unit or plist), `tools/test-windows-source-publish.ps1`,
`tools/test-install-apk.ps1`, `tools/test-build-staging.ps1` (one host zip) — `cargo` stubs replace `dotnet` stubs.

- [ ] **Step 1: Tests (RED):** the updated tests expect `cargo` and the Rust staging (`PhoneBoxAgent` +
  `THIRD-PARTY-NOTICES.txt`), and no `DOTNET_ROOT`.
- [ ] **Step 2:** RED on the server (Python; pwsh from /root/work/tools/pwsh). **Step 3:** implement. **Step 4:**
  GREEN; shellcheck; PSScriptAnalyzer errors; actionlint.
- [ ] **Step 5: Commit** `Rust agent: source installers build the Rust agent`.

### Task 7: The browser suite against the live agent

**Files:** `tools/ui-tests/playwright.config.js` (`testIgnore: ['live/**']`), create `tools/ui-tests/live/`
(`playwright.config.js`, `agent.js`, `fixture.js`, `fixtures.js`, `settings.spec.js`, `preview.spec.js`,
`pairing.spec.js`, `shortcut-icon.spec.js`; a§4's layout), `agent-rs/scripts/remote-ui-live.sh` (sync, build, run the
suite in the Playwright container with `PHONEBOX_AGENT_CMD`), `.github/workflows/rust.yml` job `ui-live`
(`ubuntu-24.04`: .NET, Rust, Node 22, `npm ci`, `npx playwright install --with-deps chromium`, the suite against C#
then Rust, reports on failure).

**Interfaces:** `PHONEBOX_AGENT_CMD` (required; the agent command line), optional `PHONEBOX_AGENT_LABEL`,
`PHONEBOX_KEEP=1`. Each spec file gets its own agent on free ports with a scratch `HOME`/`XDG_*`, the fixture of a§3
(`deviceSerial` dummy, updates off, Wi-Fi on with `tools/parity/fixtures/lan-cert.pfx`, a note page, a `builtin:lock`
shortcut), stopped by PID.

- [ ] **Step 1: Specs** per a§2: load and save (toast, persisted value), Ctrl+S, a 400 toast, the 409 conflict after an
  outside `PUT /api/settings` (poll until the version differs, a§5.3) with the draft kept; the live preview shows the
  note text and an unsaved edit; the pairing dialog shows both QR images, the decoded or linked values equal the
  `/api/pair/start` response (no markup comparison), a Wi-Fi-off agent shows the "Turn on" toast; icon upload SVG/PNG,
  `upload=true` 200, removal → 404, a bad type → 400 toast. Never click restart or update; accept `beforeunload`.
- [ ] **Step 2: Prove the suite:** green against C# (container self-contained publish, else the CI leg), then a
  mutation (break one expectation → red), then green against Rust. **Step 3:** actionlint.
- [ ] **Step 4: Commit** `Rust agent: browser suite against the live agent`.

### Task 8: The resource comparison tooling

**Files:** create `tools/resources/sample.py` (macOS `libproc` via ctypes: CPU incl. children in Mach ticks →
seconds, phys footprint, RSS, threads, disk bytes, idle wake-ups; Linux `/proc`: stat, status, smaps_rollup, io;
1 s samples; CSV), `tools/resources/client.py` (the headless phone of c§4.3: S0 idle, S1 the stream plus the panel's
global loops, S2 every enabled page for 30 s in turn, pages from `/api/config`), `tools/resources/report.py`
(summary JSON and the release-notes table of c§4.4), `agent-rs/scripts/mac-resources.sh` (both agents beside the
installed one, scratch homes, config copy with `charge.enabled=false`, warm-up 120 s, S0 10 min, S1 10 min, S2 12 min,
two runs in swapped order, stop by PID), `agent-rs/scripts/remote-resources.sh` (the Linux cross-check under
`taskset -c 0-9` in tmux `phonebox-resource`), tests `tools/resources/test_resources.py` (parsers on fixtures: a
`/proc` sample, the Mach tick conversion, the report's medians and pass rule).

- [ ] **Step 1: Tests (RED)** for the pure parts. **Step 2:** implement; GREEN on the server.
- [ ] **Step 3:** run `remote-resources.sh` once (short durations allowed by a flag) and keep its table in the plan
  workspace; the MacBook run waits for gate 1.
- [ ] **Step 4: Commit** `Rust agent: resource comparison tooling`.

### Task 9: Switch docs and the sign-off record

**Files:** `README.md` (Rust, no .NET; fork URLs; signing section: the Windows agent is not code-signed, the fork's
macOS certificate and APK key), `docs/SETUP.md`, `docs/DEVELOPMENT.md` (Rust toolchain, server-only tests, cargo
run), `CONTRIBUTING.md`, `.github/pull_request_template.md`, `docs/README.*.md` (URLs only); create
`docs/superpowers/sign-off.md`: every switch-over criterion and roadmap exit check with its evidence or its pending
step (c§2), the accepted risks for the 1.0.0 notes (c§5), the owner's decisions still open (pairing host, winget id,
Pages / issues / private vulnerability reporting, the MacBook resource run, the MacBook install).

- [ ] **Step 1:** write; **Step 2:** `check-release.py`, the pages build and `tools/test-release-metadata.py` stay
  green; links checked by a grep for `ersinkoc/phonebox` that leaves only intended upstream references (issues #56,
  #69, #269, the pairing host, history).
- [ ] **Step 3: Commit** `Rust agent: docs for the switch and the sign-off record`.

**End of Stage A:** the whole-branch final review on the most capable model (review package from
`git merge-base main HEAD`), one fix pass (each fix RED → GREEN, full suite), then push `rust-agent` to the fork.

---

### Task 10: CI after the push

- [ ] Watch `rust`, `build`, `setup` (skips until an alpha exists) and `ui-live`. CI-only failures (Windows tray, the
  job-object and startup-shortcut tests, `build.rs`, the package job, macOS/Windows parity) get one fix pass, each
  with the failing CI job as its RED. **Stop for the owner: the six secrets** (`/storage/Secrets/phonebox/README.txt`).

### Task 11: Alphas and the real self-update

- [ ] Dispatch `release.yml` for `1.0.0-alpha.1` as a draft, review the assets, publish; bump to `1.0.0-alpha.2`
  (Cargo, lock, Android 101, CHANGELOG), publish; run `lifecycle.yml` (from alpha.1 to alpha.2) and `setup.yml`.

### Task 12: Phone checks (when the phone is connected)

- [ ] c§1a P1–P7 with `agent-rs/scripts/mac-phone-check.sh` (USB, `--wifi`), `mac-dev-check.sh --phone`, the lock
  follow, and the fork APK install over USB (with the owner's consent to uninstall the upstream app).

### Task 13: OWNER GATE 1 — 1.0.0

- [ ] Ask the owner, with `docs/superpowers/sign-off.md`. On yes: the MacBook resource run; the bump commit (b§4: Cargo
  1.0.0, lock, Android, CHANGELOG with the switch procedure and accepted risks, `build.yml --final`, `lib.rs` test,
  `remote-update-check.sh` parameterized with an alpha → 1.0.0 run, Scoop homepage/autoupdate, the winget id the owner
  picks, setup header texts, `setup.yml` default path, RELEASING.md); draft release, merge to `main`, publish from
  `main`; `lifecycle.yml` alpha → 1.0.0.

### Task 14: OWNER GATE 2 — removing `agent/`

- [ ] Ask the owner after 1.0.0 has run a while. On yes: a final C#↔Rust parity run on every OS and `port-status
  --check` as evidence; freeze `PORTING.md`; `git rm -r agent/`; parity against the last release (`--reference`);
  delete the C# jobs, `sign-test.yml`, `sign-windows-agent`, the dotnet steps in scripts; relabel or retire the
  `mac-*.sh` comparisons (b§6 R2).
