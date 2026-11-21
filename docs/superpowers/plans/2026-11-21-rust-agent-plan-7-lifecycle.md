# Rust agent plan 7: Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Rust agent updates itself and the phone app the way the C# agent does (moving from one `1.0.0-alpha.N` to
the next and on to 1.0.0), behaves as a proper desktop program on every OS (Windows tray, no console, job object,
session ending, restart into the right binary), and the fork's release workflow and setup scripts build, sign and
install the Rust `PhoneBoxAgent` as `1.0.0-alpha.N` pre-releases.

**Architecture:** A new `update` module (version rules, asset selection, checksums, GitHub client, staging and swap,
phone installs) behind injected HTTP, adb-installer and clock seams, owned by `AppState` and started from `main` like
every other monitor; four routes in `server/routes_update.rs`. A `tray` module (a pure model on every OS, raw Win32 on
Windows, a log line elsewhere) and a `job_object` module; `main.rs` closes the startup/shutdown gaps. The release
workflow builds the five native targets with cargo, keeps the C# names and zip layout byte for byte, and publishes
pre-releases only.

**Tech Stack:** Rust 1.98, tokio, axum 0.8, reqwest 0.12 (stream), sha2, `zip` (new: default features off, Deflate
only), `winresource` (new, Windows build-dependency), the `windows` crate (Shell_NotifyIcon, job objects, ShellLink);
GitHub Actions; bash/PowerShell setup scripts; Python 3 tool tests.

**Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md`. The C# agent at `bb72345` is the behavioral spec.
Surveys with every rule, string, shape and file:line, in the plan workspace
(`.superpowers/sdd/2026-11-21-rust-agent-plan-7-lifecycle/`): `survey-a.md` (UpdateService, AppRestart, session
ending, the 17 tests), `survey-b.md` (tray, job object, start-at-login, `Program.cs` startup/shutdown),
`survey-c.md` (workflows, setup scripts, packaging, versions, signing). Below "a§N" means survey-a section N.

## Global Constraints

- The C# agent is the specification; the departures this plan makes are listed under "Rulings" and each is recorded
  in the ledger when its task lands.
- Release assets keep C#'s names and layout byte for byte (a§7.3, c§0.6): `PhoneBoxAgent-<v>-<rid>.zip` for
  `win-x64`, `linux-x64`, `linux-arm64`, `osx-x64`, `osx-arm64`, each holding `PhoneBoxAgent[.exe]` (and
  `THIRD-PARTY-NOTICES.txt`) at the zip root; `PhoneBox-<v>.apk`; `SHA256SUMS-<v>.txt` (`sha256sum` format);
  `PhoneBox-<v>.intoto.jsonl`.
- Versions: the release version is `agent-rs/Cargo.toml`'s; pre-switch builds are `1.0.0-alpha.N`, published as
  GitHub pre-releases and never "latest"; 1.0.0 needs an explicit workflow input (the owner is asked first).
  `agent/PhoneBox.Agent/PhoneBox.Agent.csproj` stays `0.9.23` (the parity oracle).
- The update channel is `updates.repo`, default `iddhi-sulakshana/phonebox`; a saved `ersinkoc/phonebox` is rewritten
  and now persisted on first start.
- The GitHub token (`updates.token` or `GITHUB_TOKEN`) goes only to `https://api.github.com` (C#'s test), and never to
  a log, an error text, test output or a commit.
- Nothing is pushed before the end of plan 8; no release, tag or secret is created by this plan's tasks. Signing
  material is generated under `/storage/Secrets/phonebox/`, never in the repository.
- Tests run on the server (`agent-rs/scripts/remote-*.sh`; Python tool tests through `ssh` after `sync_tree`);
  Windows-only code passes `remote-cross-check.sh` and runs on the CI `windows-latest` job after the push. MacBook
  checks run beside the installed agent on spare ports, under a scratch `HOME`, never touching the owner's install.
- No process-wide mutable statics; immutable `LazyLock` is fine. Blocking work (process runs, zip extraction, file
  swaps, COM, Win32 message loops) runs off the async workers.
- Commits are `Rust agent: …`, with no co-author trailer.

## Rulings made while writing this plan

- **Version order departs from C#** (a§0.2, a§7.1): `newer(candidate, current)` keeps `System.Version` rules for 2–4
  numeric parts and adds SemVer pre-release precedence; the current version is no longer cut at `-`. Without it an
  alpha never reaches the next alpha or 1.0.0. Cost if wrong: none for C# installs (they cannot parse an alpha tag).
- **Release lookup by channel** (a§7.2): a stable agent asks `/releases/latest` as C#; an agent on a pre-release lists
  `/releases?per_page=30`, drops drafts, and takes the highest version.
- **A staged binary must pass `--print-install-dir` (5 s) before the swap** (a§7.4); C# swaps blind with no rollback.
- **The restart target is captured at startup** and the relaunch runs in the exe's folder (a§0.3, b§0.5a/b).
- **Session ending ends the adb round, not the adb loop** (a§7.7): C# stops the loop for good after a refused call.
- **Windows tray in raw Win32** instead of the spec's `tray-icon` (b§7.2): exact menu parity, no new dependency, and
  the same hidden top-level window receives `WM_QUERYENDSESSION`. No tray on macOS/Linux, as in C#.
- **The job object covers what C#'s covers** (nvidia-smi `-lms`, camera ffmpeg; b§0.3). Deck tasks stay out.
- **A busy local port is logged to `agent.log`** before exiting non-zero (b§7.6), like Kestrel's `crit` line; no new
  behavior beyond that.
- **Plan 7's release-based exit check runs after the final push** (c§0.12, c§7.4): before it, the self-update is
  proven end to end on the server against a local fake GitHub (Task 13); after the push and the owner's secrets, a
  CI job on `macos-latest` installs alpha N−1 with `setup.sh` under a scratch `HOME` and updates to alpha N. The
  owner's MacBook install is replaced only with the owner's explicit consent.
- **Source installers** (`scripts/install.*`, `build.ps1`) **move to cargo in plan 8** with `agent/` (c§7.4 Q7).
  Scoop and winget move to the fork at 1.0.0 (c§7.4 Q6).

## Review Focus

- A release whose zip lacks the binary, or whose binary does not start on this machine: the update is refused, the
  installed binary stays and keeps running (Task 4 test `a_staged_binary_that_does_not_start_is_never_swapped_in`).
- A checksum that does not match or a download that stops half way: nothing is swapped, the status shows the error,
  the next round tries again (Task 4 test `a_bad_checksum_leaves_the_install_untouched`).
- A phone whose PhoneBox app is signed with upstream's key (install refused with `INSTALL_FAILED_UPDATE_INCOMPATIBLE`):
  logged once per phone and app version, not every minute; the agent keeps working (Task 5 test
  `a_refused_phone_install_is_logged_once`).
- Drafts and older releases in the owner's release list (a token sees drafts): never chosen (Task 3 test
  `pre_release_agents_take_the_highest_published_release`).
- After a self-update on Linux outside a service manager: the relaunch starts the new binary from the install path,
  in the install folder (Task 4 test `the_restart_target_is_the_path_captured_at_startup`).

---

## Task 1: Version rules, assets and checksums

**Files:** create `agent-rs/crates/phonebox/src/update/mod.rs`, `update/version.rs`, `update/assets.rs` (tests in each
file's `mod tests`); modify `lib.rs` (`pub mod update;`).

**Interfaces:**
- Produces: `version::newer(candidate: &str, current: &str) -> bool`; `assets::current_rid() -> &'static str`;
  `assets::Release { tag: String, draft: bool, prerelease: bool, assets: Vec<Asset> }`, `Asset { name: String, url:
  String }` parsed by `Release::from_json(&serde_json::Value) -> Option<Release>`; `assets::select(release:
  &Release, rid: &str) -> Selected { version: String, agent: Option<Asset>, app: Option<Asset>, sums: Option<Asset> }`;
  `assets::parse_sums(text: &str) -> Option<HashMap<String, String>>`; `assets::file_matches(path: &Path, sha256:
  &str) -> bool`.

- [ ] **Step 1: Port the 8 pure `UpdateChecksumTests`** (a§4.1 cases and fixtures, names per `PORTING.md:870-877`):
  `every_rid_finds_its_zip_the_app_and_the_checksums_and_each_is_listed`, `the_linux_x64_zip_is_not_taken_for_the_arm64_one`,
  `listing_formats_are_read`, `names_that_are_not_listed_give_nothing`, `text_that_is_not_a_listing_gives_nothing`,
  `the_asset_name_comes_from_the_link_not_the_cached_name`, `a_release_without_a_checksum_file_has_no_sums`,
  `a_file_matches_only_its_own_hash`; and the version test `versions_order_as_system_version_with_pre_releases`:
  `newer("1.0.0-alpha.2","1.0.0-alpha.1")`, `newer("1.0.0","1.0.0-alpha.9")`, `newer("1.0.0-alpha.1","0.9.23")`,
  `newer("1.0.0-alpha.10","1.0.0-alpha.9")`, `newer("v1.0.1","1.0.0")` true; `newer("0.9.23","1.0.0-alpha.1")`,
  `newer("1.0.0","1.0.0")`, `newer("garbage","1.0.0")`, `newer("1.0","1.0.0")` false; `newer("1.0.0","1.0")` true.
- [ ] **Step 2:** `remote-test.sh update::` → compile failure (RED).
- [ ] **Step 3: Implement** per a§1.1 (`SelectAssets`, `ParseSums`, `FileMatches`, `Newer`, the rid table) with the
  pre-release ordering above (numeric identifiers as numbers, numeric < alphanumeric, shorter prefix lower,
  `+build` ignored, leading `v` stripped as C# does).
- [ ] **Step 4:** `remote-test.sh update::` → PASS; `remote-check.sh`, `remote-cross-check.sh` clean.
- [ ] **Step 5: Commit** `Rust agent: update rules and assets`.

## Task 2: Leftovers of the single-file migration

**Files:** create `update/migration.rs`; modify `phonebox-agent/src/main.rs` (call it when running installed).

**Interfaces:** Produces `migration::remove_framework_dependent_leftovers(dir: &Path, single_file: bool)`.

- [ ] **Step 1: Port `SingleFileMigrationTests`** (a§4.3): `a_single_file_agent_removes_the_framework_dependent_files_and_keeps_the_rest`,
  `a_framework_dependent_agent_keeps_its_files`, `a_missing_directory_or_partial_set_is_fine`.
- [ ] **Step 2:** RED on the server. **Step 3:** implement with C#'s file list (a§1.1); main calls it with `true` when
  the running exe's folder is `app_paths::install_dir()` (C#'s condition, a§1.1). **Step 4:** GREEN, full check.
- [ ] **Step 5: Commit** `Rust agent: single-file migration`.

## Task 3: The update service: GitHub, channel, status, loop, state

**Files:** create `update/github.rs`, `update/service.rs`; modify `server/mod.rs` (`AppState.updates:
Arc<UpdateService>`), `state.rs` (`update` source), `phonebox-agent/src/main.rs` (start the loop), `config/store.rs`
(persist the repo rewrite, a§7.10).

**Interfaces:**
- Produces: `trait UpdateHttp: Send + Sync { fn get_json(&self, url: &str, token: Option<&str>) -> BoxFuture<Result<Value,
  HttpFailure>>; fn download(&self, url: &str, dest: &Path, token: Option<&str>) -> BoxFuture<Result<(), HttpFailure>>; }`
  with `HttpFailure { transient: bool, message: String }`; `ReqwestUpdateHttp::new()` (connect 30 s, API 5 min, downloads
  5 min per chunk idle, a§7.8); `UpdateService::new(cfg, adb: Arc<AdbReverse>, phone: Arc<PhoneSeenState>, http:
  Arc<dyn UpdateHttp>, installer: Arc<dyn PhoneInstaller>, now) -> Arc<UpdateService>`; `check(&self, stop: &Stop)
  -> Result<(), String>`; `status(&self) -> Value` (the `GET /api/update` and `/api/state.update` shapes, a§2);
  `start(self: &Arc<Self>, stop: Stop)` (20 s, then every minute, auto apply per `updates.auto`); `api_base()` =
  `https://api.github.com`, overridable only for tests and by the env var `PHONEBOX_UPDATE_API` when it names
  `http://127.0.0.1:<port>` (Task 13's fake GitHub).

- [ ] **Step 1: Port `UpdateServiceTests`** (a§4.2): `token_only_goes_to_the_git_hub_api`,
  `timeouts_and_http_errors_are_transient_but_our_cancellation_is_not`; add `pre_release_agents_take_the_highest_published_release`
  (fixture list: a draft 1.0.0-alpha.9, a published alpha.3, alpha.2, a stable 0.9.23; current alpha.2 → alpha.3;
  current 0.9.23 asks `/releases/latest` only) and `a_saved_upstream_repo_is_rewritten_on_disk` (config store).
- [ ] **Step 2:** RED. **Step 3:** implement the check per a§1.1 (`CheckCoreAsync`: token fallback, `_apiUrls`,
  selection, the status fields and texts) with the channel ruling; the `update` state source returns `status()`.
  **Step 4:** GREEN, full check, cross-check.
- [ ] **Step 5: Commit** `Rust agent: update check`.

## Task 4: Download, verify, swap and restart

**Files:** create `update/swap.rs`; modify `update/service.rs` (`apply`), `app_restart.rs`, `phonebox-agent/src/main.rs`
(capture the exe path at startup; relaunch in its folder; `shutdown_timeout(5 s)`).

**Interfaces:** Produces `swap::stage(zip: &Path, into: &Path) -> Result<PathBuf, String>` (requires
`PhoneBoxAgent[.exe]`), `swap::self_test(exe: &Path) -> Result<(), String>` (`--print-install-dir`, 5 s),
`swap::replace(staged: &Path, install_dir: &Path) -> Result<(), String>` (rename to `.old`, move in, `chmod 0755`);
`UpdateService::apply(&self, stop: &Stop) -> Result<(), String>`; `app_restart::restart_target(captured: &Path) ->
(PathBuf, PathBuf)` (exe, working directory).

- [ ] **Step 1: Tests:** `a_bad_checksum_leaves_the_install_untouched`, `a_staged_binary_that_does_not_start_is_never_swapped_in`
  (a staged script that exits 1), `a_zip_without_the_agent_is_refused`, `the_swap_keeps_the_old_binary_as_old`
  (temp install dir; unix: mode 0755), `the_restart_target_is_the_path_captured_at_startup` (working dir = its folder).
- [ ] **Step 2:** RED. **Step 3:** implement per a§1.1 `UpdateAgentAsync` (staging under
  `local_dir()/updates/agent-<v>`, the checksum rule including C#'s "no sums file → warn and continue" (a§7.12), then
  `app_restart::request()` + `stop.stop()`). **Step 4:** GREEN, full check, cross-check.
- [ ] **Step 5: Commit** `Rust agent: update install and restart`.

## Task 5: The phone app: updates and installs

**Files:** create `update/phone.rs`; modify `update/service.rs`.

**Interfaces:** Produces `trait PhoneInstaller: Send + Sync { fn install(&self, serial: &str, apk: &Path) ->
BoxFuture<Result<(), String>>; }` and `AdbInstaller` (`tool_process::run` with an argument vector, stdout + stderr, 3 min;
a§0.5); `UpdateService::install_missing(&self, stop: &Stop) -> Result<usize, String>` (`POST /api/phone/install`).

- [ ] **Step 1: Port** `when_the_phones_report_the_current_app_only_the_ones_that_failed_are_tried_again`,
  `a_phone_stays_pending_until_its_own_update_works`, `apply_clears_error_after_retry_success_and_zero_targets_preserve_app_version`
  (a§4.1; the last one with the workspace version as the release tag, which now works through `newer`); add
  `a_refused_phone_install_is_logged_once`.
- [ ] **Step 2:** RED. **Step 3:** implement per a§1.1 (`_phoneRetry`, `_phoneRetryFor`, `AppVersion`, the error
  texts, "no connected phone is missing the app"). **Step 4:** GREEN, full check.
- [ ] **Step 5: Commit** `Rust agent: phone app updates`.

## Task 6: Update routes

**Files:** create `server/routes_update.rs`; modify `server/mod.rs` (both listeners), `lan.rs` only if a§2 shows a
Wi-Fi path (it shows none: all four stay local).

- [ ] **Step 1: Tests** (`server/routes_update.rs`): `GET /api/update` shape (a§2), `POST /api/update/check` 200 with
  the status, `POST /api/update/apply` 202 empty (409/400 as a§2 lists), `POST /api/phone/install` with no phone →
  400 `{"error":"no connected phone is missing the app"}`, and each path over Wi-Fi → 403.
- [ ] **Step 2:** RED. **Step 3:** implement (gate: `try_lock` for apply and install, `lock().await` for check,
  a§7.14). **Step 4:** GREEN, full check.
- [ ] **Step 5: Commit** `Rust agent: update routes`.

## Task 7: Session ending (adb)

**Files:** modify `adb.rs`.

**Interfaces:** Produces `AdbReverse::session_ending(&self)` (opens a 60 s window from the injected clock) and
`AdbError::SessionEnding`; the loop sleeps 4 s per turn while the window is open and keeps looping after it.

- [ ] **Step 1: Port** `adb_waits_a_minute_after_the_session_starts_ending` (a§4.4) and add
  `a_refused_adb_call_ends_the_round_not_the_loop`.
- [ ] **Step 2:** RED. **Step 3:** implement per a§1.3 with the ruling; `open_panel` and the screen sync get the gate,
  the updater's install does not. **Step 4:** GREEN, full check.
- [ ] **Step 5: Commit** `Rust agent: session ending`.

## Task 8: The tray

**Files:** create `agent-rs/crates/phonebox/src/tray/mod.rs` (model, Unix line), `tray/windows.rs`; modify
`phonebox-agent/src/main.rs` (start it last, stop it first), `Cargo.toml` (features if needed).

**Interfaces:** Produces `tray::menu(version: &str, connected: bool, devices: &str, autostart: bool) -> Vec<MenuItem>`,
`tray::status_text(connected, devices)`, `tray::tooltip(connected)`, `tray::start(endpoint, adb: Arc<AdbReverse>, stop:
Stop)` (Windows: an STA thread with a hidden top-level window, `Shell_NotifyIconW`, the b§1.1 menu, double-click opens
the settings URL, `WM_QUERYENDSESSION` → `adb.session_ending()`; elsewhere: logs b§1.2's line, category `Tray`);
`tray::startup_shortcut(folder: &Path) -> PathBuf`, `toggle_autostart(folder, exe) -> Result<bool, String>` (IShellLinkW).

- [ ] **Step 1: Tests:** `the_menu_lists_c_sharps_items_in_order` (exact texts, `Settings…` bold, `Quit`),
  `status_and_tooltip_texts` (`Phone connected (a, b)`, `No phone connected`, `PhoneBox — phone connected`,
  `PhoneBox — waiting for phone`), `the_unix_line_names_the_settings_url` (two spaces, trailing `/`); Windows only:
  `the_startup_shortcut_points_at_the_exe_and_toggles` (temp folder, read back through IShellLinkW).
- [ ] **Step 2:** RED. **Step 3:** implement; a tray that cannot start only logs (b§7.12). **Step 4:** GREEN, full
  check, cross-check.
- [ ] **Step 5: Commit** `Rust agent: tray`.

## Task 9: Job object

**Files:** create `job_object.rs`; modify `system_monitor.rs` (nvidia-smi), `camera_feeds.rs` (ffmpeg),
`Cargo.toml` (`Win32_System_JobObjects`).

**Interfaces:** Produces `job_object::track(child: &tokio::process::Child)` and a `std::process::Child` variant
(Windows: a lazily created job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, held by an instance in `AppState`, not a
static; elsewhere a no-op).

- [ ] **Step 1: Test** (Windows CI): `a_tracked_child_ends_with_its_parent` (b§4.3: re-run the test exe as a helper that
  tracks `ping -n 60 127.0.0.1`, kill the helper, the sleeper ends within 5 s); every OS: `track_is_harmless_elsewhere`.
- [ ] **Step 2–4:** RED (cross-check compile), implement, GREEN (remote-check + cross-check; the Windows test runs in CI).
- [ ] **Step 5: Commit** `Rust agent: job object`.

## Task 10: The Windows executable and startup gaps

**Files:** create `agent-rs/crates/phonebox-agent/build.rs`, `agent-rs/.cargo/config.toml`; modify
`phonebox-agent/src/main.rs`, `phonebox-agent/Cargo.toml` (`[target.'cfg(windows)'.build-dependencies] winresource`),
`single_instance.rs` or main (open the second-instance page with `ShellExecuteExW`).

- [ ] **Step 1: Tests:** `a_busy_local_port_is_written_to_the_log` (bind first, run `serve`), `a_trailing_config_flag_creates_no_config`
  (b§3.2 item 14).
- [ ] **Step 2:** RED. **Step 3:** `#![windows_subsystem = "windows"]`; `build.rs` adds `phonebox.ico` and VERSIONINFO
  (ProductVersion = the Cargo version) only when the target and the host are Windows; `+crt-static` for
  `x86_64-pc-windows-msvc`; SIGQUIT handled as SIGTERM; the bind failure logged as b§0.5c. **Step 4:** GREEN, full
  check, cross-check.
- [ ] **Step 5: Commit** `Rust agent: Windows executable and startup gaps`.

## Task 11: Release workflow, versions and notices

**Files:** modify `.github/workflows/release.yml`, `.github/workflows/rust.yml` (`workflow_call`, `--locked`, a package
job), `.github/workflows/pages.yml`, `tools/check-release.py`, `tools/pages/build.py`, `android/app/build.gradle.kts`
(`versionName = "1.0.0-alpha.1"`, `versionCode = 100`), `CHANGELOG.md` (`## 1.0.0-alpha.1 — <date>`),
`docs/RELEASING.md`; create `agent-rs/THIRD-PARTY-NOTICES.txt`, `agent-rs/about.toml`, `agent-rs/about.hbs`,
`agent-rs/scripts/notices.sh` (cargo-about on the server), `tools/check-notices.py`.

- [ ] **Step 1: Tests** (Python, run on the server): `check-release.py` accepts `1.0.0-alpha.1` from Cargo with a
  matching `versionName`, refuses a mismatch and `1.0.0` without the `final` flag; `check-notices.py` fails when a
  locked crate is missing from the notices; `pages/build.py` reads the Cargo version.
- [ ] **Step 2:** RED. **Step 3:** implement per c§7.1's step list: version from `agent-rs/Cargo.toml`; the agent job
  builds `cargo build --release --locked -p phonebox-agent` on `ubuntu-22.04`, `ubuntu-22.04-arm`, `macos-15-intel`,
  `macos-latest`, `windows-latest`, zips `PhoneBoxAgent[.exe]` + notices, signs on macOS with `sign-macos-agent`,
  runs `tools/smoke-agent.py`, checks the Windows exe's `ProductVersion`; `--prerelease --latest=false` when the
  version has `-`; the `final` input guards 1.0.0; the manifest step runs only for a final release from `main`;
  fork links in the notes footer; SignPath dropped. **Step 4:** GREEN; `actionlint` on the server if available.
- [ ] **Step 5: Commit** `Rust agent: release workflow`.

## Task 12: Setup scripts

**Files:** modify `setup.sh`, `setup.ps1`, `.github/workflows/setup.yml`, `tools/test-unix-setup-phone.py` (and the
other `tools/test-unix-*.py` where names change).

- [ ] **Step 1: Tests:** the Unix setup test installs a synthetic `1.0.0-alpha.2` release (names as Global
  Constraints) with `--version 1.0.0-alpha.2`, and with `--pre` picks the newest pre-release from a fixture
  `/releases` answer; the existing cases keep passing.
- [ ] **Step 2:** RED (on the server). **Step 3:** per c§7.1: `repo` = fork (and the header one-liners), the version
  regex accepts `-<pre>`, `--pre`/`-Pre`, the size texts, the issue links reworded to upstream's full URLs;
  `setup.yml` resolves the newest release including pre-releases or skips when there is none. **Step 4:** GREEN.
- [ ] **Step 5: Commit** `Rust agent: setup scripts for the fork`.

## Task 13: Exit: the self-update round trip, parity and signing material

**Files:** create `agent-rs/scripts/remote-update-check.sh`, `tools/fake-github.py`, `tools/parity/corpus/phase7.json`,
`.github/workflows/lifecycle.yml` (the post-push macOS check); modify `PORTING.md` via `port-status.py`.

- [ ] **Step 1: Self-update round trip on the server** (`remote-update-check.sh`): build the agent twice (the tree as
  `1.0.0-alpha.1`, a temp copy bumped to `1.0.0-alpha.2`), zip both as release assets with `SHA256SUMS`, serve them
  from `tools/fake-github.py` on 127.0.0.1, install alpha.1 into a scratch `HOME`'s install dir, start it with
  `PHONEBOX_UPDATE_API`, a spare `--port` and a dummy `deviceSerial`, `POST /api/update/check`, `POST
  /api/update/apply`, and expect `/api/info` to report `1.0.0-alpha.2` from the relaunched process with
  `PhoneBoxAgent.old` beside it; then a release with a wrong checksum and one whose binary exits 1 are refused and the
  running version stays. Stop every process by PID.
- [ ] **Step 2: Corpus** `phase7.json` (a§7.13): `GET /api/update` (normalize `agentVersion`), `POST /api/phone/install`
  with no phone, `POST /api/update/apply`, the four paths over Wi-Fi; all seven corpora pass on the server.
- [ ] **Step 3:** `lifecycle.yml` (manual dispatch, `macos-latest`): scratch `HOME`, `setup.sh --version <N-1> --no-autostart
  --no-start --no-phone --yes`, start on a spare port, check and apply, expect version N. It runs after the push.
- [ ] **Step 4: Signing material** under `/storage/Secrets/phonebox/` (mode 700): an Android keystore (`keytool`,
  alias `phonebox`, RSA 4096, 10000 days) and the self-signed macOS certificate (`tools/make-macos-signing-cert.sh`
  without `--set-secrets`); a `README.txt` there lists the six secret names and how to add them with `gh secret set
  --repo iddhi-sulakshana/phonebox`. Nothing is printed or committed.
- [ ] **Step 5:** `port-status.py --check 7` clean (898/898); full suite, cross-check, all corpora green; only plan 8's
  items remain in d§5. **Commit** `Rust agent: plan 7 exit: self-update round trip and release flow`.
