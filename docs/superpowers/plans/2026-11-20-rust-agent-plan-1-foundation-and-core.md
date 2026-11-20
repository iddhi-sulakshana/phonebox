# Rust agent, plan 1: foundation and core — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create the fork, the Rust toolchains and test infrastructure, and a Rust `PhoneBoxAgent` whose core (paths,
config, security, settings page, state and stream, USB/adb) matches the C# agent, ending with the owner's phone
showing the panel from the Rust agent over USB.

**Architecture:** A Cargo workspace in `agent-rs/` with one library crate (`phonebox`, all logic, module per C# file)
and one binary crate (`phonebox-agent`, producing `PhoneBoxAgent`). Features later phases port sit behind a
`StateSource` trait whose phase-1 implementations return exactly what the C# agent returns when that feature has no
data. A stdlib-only Python harness runs the C# and Rust agents side by side and diffs their HTTP answers.

**Tech Stack:** Rust stable 1.98 (edition 2024), tokio, axum, tower-http, serde, serde_json, jsonc-parser, rust-embed,
tracing, gethostname; Python 3 (stdlib only) for the parity harness; .NET 10 SDK for the C# reference agent.

**Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md` · **Roadmap:** `docs/superpowers/plans/2026-11-20-rust-agent-roadmap.md`

## Global Constraints

- Baseline: upstream commit `43800f9`; the C# code there and its tests are the specification.
- Fork: `iddhi-sulakshana/phonebox` (public); work happens on branch `rust-agent`.
- Ports: local `8765` on `127.0.0.1` only; Wi-Fi `8766` (phase 4). `--port` accepts 1024–65535, error text
  `--port requires one integer between 1024 and 65535`.
- CLI flags: `--config <path>`, `--port <n>`, `--print-install-dir`, `--restart` (waits up to 15 s for the previous
  instance).
- Files, same folders as `AppPaths.cs`: `config.json`, `config.json.bak`, `config.json.tmp`, `agent.log`, `data.json`,
  `icons/`, `lan-cert.pfx`; owner-only mode `0600` on macOS/Linux.
- Log line format: `yyyy-MM-dd HH:mm:ss <lvl4> <Category>: <message>[ | <error>]` in local time, `<lvl4>` = first
  four letters lower-case (`info`, `warn`, `erro`, `crit`, `debu`, `trac`); file restarts when over 2 MB at startup.
- Config: keep unknown fields at every level when saving (deliberate departure); default `updates.repo` is
  `iddhi-sulakshana/phonebox`, and a saved `ersinkoc/phonebox` is rewritten to it on first start (departure).
- Agent version string for all pre-switch builds: `1.0.0-alpha.N`.
- Tests and servers never run on the owner's laptop; they run on `hetzner-iddhi` via `agent-rs/scripts/remote-test.sh`
  or in CI. Laptop exceptions (owner-approved): macOS hardware parity runs, the resource comparison, the real-phone
  check — each short-lived, beside the installed agent, stopped by PID.
- On the laptop, Rust lives under `/storage` (`CARGO_HOME=/Volumes/storage/Runtimes/Rust/cargo`).
- Commit messages carry no AI co-author trailer.

## Review Focus

1. **A `config.json` written by C# 0.9.22 with comments, trailing commas, odd key casing and fields Rust does not
   model** must load, and a save must keep every unknown field. Pinned by `config::tests::keeps_unknown_fields` and
   `config::tests::reads_comments_trailing_commas_and_any_case` (Task 6).
2. **Whole-number doubles** (for example `"brightness": 1`) must be written as .NET writes them (`1`, not `1.0`), or
   every Rust save rewrites the user's file. Pinned by `config::write::tests::writes_doubles_like_dotnet` (Task 7).
3. **A save from a stale settings tab** must get HTTP 409 with the exact conflict text, while a script that sends no
   `X-Config-Version` still saves. Pinned by
   `server::routes_settings::tests::stale_save_conflicts_and_headerless_save_succeeds` (Task 10).
4. **DNS-rebinding and foreign-origin requests** (Host other than `127.0.0.1`/`localhost`, an `Origin` other than the
   settings page) must get 403 before any handler runs. Pinned by `server::guard::tests::rejects_foreign_host_and_origin`
   (Task 9).
5. **Phone unplugged, adb server killed, or adb missing** must leave the agent running and logging
   `no phone connected, waiting...`, then reconnect when the phone returns. Pinned by
   `adb::tests::survives_missing_adb_and_unplug` (Task 12).

---

### Task 1: Fork, branch and planning documents

**Files:**
- Create (on the fork): branch `rust-agent` from `43800f9`
- Commit: `docs/superpowers/specs/2026-11-20-rust-agent-design.md`, `docs/superpowers/plans/2026-11-20-rust-agent-roadmap.md`, this plan

**Interfaces:**
- Produces: git remote `fork` → `https://github.com/iddhi-sulakshana/phonebox`; branch `rust-agent`.

- [ ] **Step 1: Create the fork**

Run: `gh repo fork ersinkoc/phonebox --remote=false --clone=false`
Expected: `iddhi-sulakshana/phonebox` exists; `gh api repos/iddhi-sulakshana/phonebox --jq .fork` prints `true`.

- [ ] **Step 2: Add the remote and branch**

Run: `git remote add fork https://github.com/iddhi-sulakshana/phonebox.git && git switch -c rust-agent 43800f9`
Expected: `git log -1 --format=%h` prints `43800f9`; the three documents are still in the working tree (untracked).

- [ ] **Step 3: Commit and push**

```bash
git add docs/superpowers/specs/2026-11-20-rust-agent-design.md docs/superpowers/plans/
git commit -m "Rust agent: design, roadmap and plan 1"
git push -u fork rust-agent
```
Expected: push succeeds; `gh api repos/iddhi-sulakshana/phonebox/branches/rust-agent --jq .name` prints `rust-agent`.

---

### Task 2: Toolchain on hetzner-iddhi and the remote test script

**Files:**
- Create: `agent-rs/scripts/remote-test.sh`
- Create: `agent-rs/scripts/remote-dotnet-test.sh`

**Interfaces:**
- Produces: `agent-rs/scripts/remote-test.sh [test-name filters…]` — syncs the working tree to
  `hetzner-iddhi:/root/work/phonebox/` and runs `cargo test --workspace -- [filters]` in `agent-rs/` there (libtest
  accepts several filters after `--`); exit code is cargo's. `agent-rs/scripts/remote-dotnet-test.sh [dotnet test args…]` — same sync, runs
  `dotnet test agent/PhoneBox.Agent.Tests [args]`.

- [ ] **Step 1: Install Rust on the server**

Run: `ssh hetzner-iddhi 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.98.0 --profile minimal -c clippy,rustfmt'`
Expected: `ssh hetzner-iddhi '~/.cargo/bin/cargo --version'` prints `cargo 1.98.0 …`.

- [ ] **Step 2: Write `remote-test.sh`**

Sync with `rsync -az --delete --exclude .git --exclude target --exclude node_modules --exclude android/.gradle
--exclude android/app/build ./ hetzner-iddhi:/root/work/phonebox/` from the repository root (the script `cd`s there
from its own location), then `ssh hetzner-iddhi 'cd /root/work/phonebox/agent-rs && . ~/.cargo/env && cargo test
--workspace -- <filters>'`. Use `set -euo pipefail`; quote forwarded args with `printf '%q '`. `remote-dotnet-test.sh` is
the same with `cd /root/work/phonebox && dotnet test agent/PhoneBox.Agent.Tests <args>`.

- [ ] **Step 3: Record the C# baseline**

Run: `agent-rs/scripts/remote-dotnet-test.sh`
Expected: all tests pass. Write the total count into `agent-rs/PORTING.md` (created in Task 3) as the baseline line.
If any test fails on Linux at `43800f9`, list it there as "fails upstream on Linux" instead of fixing it.

- [ ] **Step 4: Commit**

```bash
git add agent-rs/scripts && git commit -m "Rust agent: remote test scripts for hetzner"
```

---

### Task 3: Workspace skeleton, CI and the porting coverage table

**Files:**
- Create: `agent-rs/Cargo.toml` (workspace, `resolver = "3"`, shared `[workspace.package] edition = "2024"`, `version = "1.0.0-alpha.1"`)
- Create: `agent-rs/rust-toolchain.toml` (`channel = "1.98.0"`, components `clippy`, `rustfmt`)
- Create: `agent-rs/crates/phonebox/Cargo.toml`, `agent-rs/crates/phonebox/src/lib.rs`
- Create: `agent-rs/crates/phonebox-agent/Cargo.toml` (`[[bin]] name = "PhoneBoxAgent"`), `agent-rs/crates/phonebox-agent/src/main.rs`
- Create: `agent-rs/scripts/port-status.py`, `agent-rs/PORTING.md`
- Create: `.github/workflows/rust.yml`

**Interfaces:**
- Produces: library crate `phonebox`; binary `PhoneBoxAgent`; `python3 agent-rs/scripts/port-status.py` regenerates
  `agent-rs/PORTING.md` and exits non-zero with `--check <phase>` when a C# test assigned to that phase has no Rust
  test.
- Naming rule every later task follows: C# test `Class.Method` ↔ Rust test `<module>::tests::<method_snake_case>`
  (`ConfigTests.SaveRejectsStaleVersion` → `config::tests::save_rejects_stale_version`). A C# test that cannot apply
  gets a line `// not-ported: <C# Class.Method>: <reason>` in the Rust module's test block.

- [ ] **Step 1: Write the failing smoke test**

In `agent-rs/crates/phonebox/src/lib.rs`:
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn crate_version_is_alpha() {
        assert!(env!("CARGO_PKG_VERSION").starts_with("1.0.0-alpha."));
    }
}
```

- [ ] **Step 2: Run it**

Run: `agent-rs/scripts/remote-test.sh`
Expected: PASS once the workspace files exist (FAIL before: no `Cargo.toml`).

- [ ] **Step 3: Write `port-status.py`**

Parse every `[Fact]`/`[Theory]` method in `agent/PhoneBox.Agent.Tests/*.cs` (class name + method name); scan
`agent-rs/**/*.rs` for `fn <snake>(` inside `#[cfg(test)]` blocks and for `// not-ported: Class.Method` lines; read
the phase of each C# test file from a table at the top of the script (phase 1 files: `ConfigTests`,
`ConfigPathTests`, `ConfigLegacyOptionsTests`, `ConfigVersionTextTests`, `EmptyPageConfigTests`, `StackConfigTests`,
`PrivateFilesTests`, `EndpointTests`, `FileLoggerTests`, `WebViewDoctorTests`, `PhonesTests`,
`PhoneRegistryConcurrencyTests`, `RequestTimingsTests`, `StateFrameCacheTests`, `StateSignalTests`,
`PushCapacityTests`, `PushInputTests`, `PushShutdownTests`, `PushVersionTests`, `AdbReverseTests`,
`AdbUnreachablePhoneTests`; files that mix phases, like `CoreAuditTests` and `AuditRegressionTests`, list a phase per
method). Write `PORTING.md` as one Markdown table: C# test, phase, Rust test or "not ported: reason", status.

- [ ] **Step 4: Write `.github/workflows/rust.yml`**

On push and pull request touching `agent-rs/**`: matrix `ubuntu-24.04`, `ubuntu-24.04-arm`, `macos-14`, `macos-13`,
`windows-2022`; steps: checkout, `dtolnay/rust-toolchain` with the pinned version, `cargo fmt --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Working directory `agent-rs`.

- [ ] **Step 5: Enable Actions on the fork and check the run**

Run: `gh api -X PUT repos/iddhi-sulakshana/phonebox/actions/permissions -F enabled=true -f allowed_actions=all`, push,
then `gh run watch` on the `rust` workflow.
Expected: all five jobs green.

- [ ] **Step 6: Commit**

```bash
git add agent-rs .github/workflows/rust.yml && git commit -m "Rust agent: workspace, CI and porting table"
```

---

### Task 4: Parity harness

**Files:**
- Create: `tools/parity/parity.py`, `tools/parity/fixtures/default.json`, `tools/parity/corpus/phase1.json`
- Create: `agent-rs/scripts/remote-parity.sh`

**Interfaces:**
- Produces: `python3 tools/parity/parity.py --csharp <cmd> --rust <cmd> --corpus <file> [--fixture <config.json>]`.
  It copies the fixture into two temp folders, starts each agent as `<cmd> --config <tmp>/config.json --port <free
  port>`, waits for `GET /api/info` to answer, sends each request, diffs, stops both by PID, exits 1 on any
  difference. Corpus entry shape:
  `{"name": str, "method": str, "path": str, "headers": {str: str}, "body": str | null,
    "ignore": [json-pointer, …], "compare": "json" | "bytes" | "status"}`.
  `remote-parity.sh <corpus>` syncs like `remote-test.sh`, builds the C# agent (`dotnet build -c Release
  agent/PhoneBox.Agent`) and the Rust agent (`cargo build --release`), and runs the harness on the server.
- The fixture sets `deviceSerial` to `"parity-none"` (so neither agent touches a phone), `updates.auto` to `false` and
  `lan.enabled` to `false`.

- [ ] **Step 1: Write the harness's own test**

`tools/parity/test_parity.py` (`unittest`): `test_diff_ignores_pointers` — two JSON bodies differing only at
`/ts` with `ignore: ["/ts"]` compare equal; `test_diff_reports_status` — 200 vs 403 is reported with the request
name; `test_bytes_mode` — identical bytes equal, one byte different reported.

- [ ] **Step 2: Run it to see it fail, implement `parity.py`, run it to see it pass**

Run: `python3 -I -m unittest tools/parity/test_parity.py` (pure functions, no servers: allowed on the laptop).
Expected: FAIL (no module), then PASS.

- [ ] **Step 3: Prove the harness against C# vs C#**

Run: `agent-rs/scripts/remote-parity.sh tools/parity/corpus/phase1.json` with `--rust` temporarily pointing at a
second C# instance.
Expected: zero differences after the `ignore` lists in the corpus are right (`/ts`, `/cfgVersion`, timing fields).
Commit the corpus with those ignores.

- [ ] **Step 4: Commit**

```bash
git add tools/parity agent-rs/scripts/remote-parity.sh && git commit -m "Rust agent: parity harness and phase-1 corpus"
```

---

### Task 5: Paths, endpoint arguments and the log file

**Files:**
- Create: `agent-rs/crates/phonebox/src/app_paths.rs`, `src/agent_endpoint.rs`, `src/file_logger.rs`
- Port from: `AppPaths.cs`, `AgentEndpoint.cs`, `FileLogger.cs`
- Tests: ported `ConfigPathTests`, `PrivateFilesTests`, `EndpointTests`, `FileLoggerTests` (methods that exercise these classes)

**Interfaces:**
- Produces:
  - `app_paths::{config_dir() -> PathBuf, local_dir() -> PathBuf, config_path() -> PathBuf, log_path() -> PathBuf,
    install_dir() -> PathBuf, os() -> &'static str /* "windows"|"linux"|"macos" */, rid() -> &'static str,
    version() -> &'static str, ensure_config() -> io::Result<()>, restrict_config(&Path),
    create_private(&Path) -> io::Result<File>, copy_private(&Path, &Path) -> io::Result<()>,
    write_all_text_private(&Path, &str) -> io::Result<()>, restrict_to_owner(&Path)}`
  - `agent_endpoint::AgentEndpoint { pub port: u16 }` with `from_args(&[String]) -> Result<AgentEndpoint, String>`,
    `settings_url(&self) -> String` (`http://127.0.0.1:{port}/`), `instance_lock_name(&self) -> String`
    (`PhoneBox.Agent` for 8765, else `PhoneBox.Agent.{port}`).
  - `file_logger::init(log_path: &Path) -> tracing_appender::non_blocking::WorkerGuard` installing a `tracing`
    subscriber that writes the Global Constraints line format; category = last `::` segment of the target.

- [ ] **Step 1: Port the tests** (one `#[test]` per C# case, same inputs and assertions; macOS/Linux mode checks behind `#[cfg(unix)]`)
- [ ] **Step 2: Run to see them fail** — `agent-rs/scripts/remote-test.sh app_paths agent_endpoint file_logger` → FAIL (unresolved)
- [ ] **Step 3: Implement the three modules.** `config_dir`: Windows `%APPDATA%\PhoneBox`, macOS `~/Library/Application Support/PhoneBox`, Linux `$XDG_CONFIG_HOME|~/.config` + `/PhoneBox`; `local_dir`: Windows `%LOCALAPPDATA%\PhoneBox`, macOS same as config, Linux `$XDG_DATA_HOME|~/.local/share` + `/PhoneBox`. `create_private` removes an existing file then creates with `O_EXCL` and mode `0600`. `ensure_config` writes the embedded `config.default.macos.json` / `config.default.linux.json` / `config.default.json` from `agent/PhoneBox.Agent/`.
- [ ] **Step 4: Run to see them pass** — same command → PASS
- [ ] **Step 5: Commit** — `git commit -m "Rust agent: paths, --port, log file"`

---

### Task 6: Config model and reading

**Files:**
- Create: `agent-rs/crates/phonebox/src/config/mod.rs` (model), `src/config/read.rs`, `src/config/normalize.rs`
- Port from: `Config.cs` (every class, its `Normalize`, legacy-option converters such as `WeatherUnitJsonConverter`, `Hour12JsonConverter`, `IconJsonConverter`)
- Tests: ported `ConfigTests` (reading/normalizing cases), `ConfigLegacyOptionsTests`, `EmptyPageConfigTests`, `StackConfigTests`; new tests below

**Interfaces:**
- Produces: `config::AgentConfig` and one struct per C# config class, fields in C# declaration order, serde
  `rename_all = "camelCase"`, `Option` for C# nullable reference/nullable value types with
  `skip_serializing_if = "Option::is_none"`, and on every struct `#[serde(flatten)] pub extra: serde_json::Map<String,
  serde_json::Value>`. `config::read::parse(text: &str) -> Result<AgentConfig, String>` (error `Invalid JSON: …`).
  `config::normalize::normalize(cfg: AgentConfig, stale_ids: Option<&HashSet<String>>) -> AgentConfig`.
  `AgentConfig::widgets(&self, enabled_pages_only: bool) -> impl Iterator<Item = &WidgetConfig>`.

- [ ] **Step 1: Write the new failing tests** (in the `tests` module of `src/config/mod.rs`)

```rust
#[test]
fn keeps_unknown_fields() {
    let cfg = parse(r#"{ "futureFlag": 7, "weather": { "city": "Colombo", "newKnob": "x" } }"#).unwrap();
    let out = serde_json::to_value(&cfg).unwrap();
    assert_eq!(out["futureFlag"], 7);
    assert_eq!(out["weather"]["newKnob"], "x");
}

#[test]
fn reads_comments_trailing_commas_and_any_case() {
    let cfg = parse("{ // c\n \"Weather\": { \"CITY\": \"Colombo\", }, }").unwrap();
    assert_eq!(cfg.weather.city, "Colombo");
    assert!(cfg.extra.is_empty());
}

#[test]
fn upstream_update_repo_moves_to_fork() {
    let cfg = normalize(parse(r#"{ "updates": { "repo": "ersinkoc/phonebox" } }"#).unwrap(), None);
    assert_eq!(cfg.updates.repo, "iddhi-sulakshana/phonebox");
}
```
Plus every reading/normalizing case from the C# test files listed above.

- [ ] **Step 2: Run to see them fail** — `agent-rs/scripts/remote-test.sh config::` → FAIL
- [ ] **Step 3: Implement.** Read with `jsonc-parser` (comments, trailing commas) into `serde_json::Value`, then canonicalize object keys case-insensitively against each struct's field names (a `KnownFields` trait listing the camelCase names, implemented per struct by a small `macro_rules!`), then `serde_json::from_value`. Keys that match no field stay as written and land in `extra`. Port `Normalize` line by line, plus the `updates.repo` rewrite.
- [ ] **Step 4: Run to see them pass** — same command → PASS
- [ ] **Step 5: Commit** — `git commit -m "Rust agent: config model, reading and normalizing"`

---

### Task 7: Config store: version, save, conflicts, backups

**Files:**
- Create: `agent-rs/crates/phonebox/src/config/store.rs`, `src/config/write.rs`
- Port from: `ConfigStore` in `Config.cs` (`Get`, `Version`, `VersionText`, `VersionConflict`, `Touch`, `Save`, `PersistGenerated`, `NewToken`)
- Tests: ported `ConfigTests` (store cases), `ConfigVersionTextTests`, `PrivateFilesTests` (config cases); new tests below

**Interfaces:**
- Consumes: `config::read::parse`, `config::normalize::normalize`, `app_paths::{write_all_text_private, copy_private}`.
- Produces: `config::store::ConfigStore::new(path: PathBuf) -> Self`, `get(&self) -> Arc<AgentConfig>` (re-reads
  when the file's mtime changed), `version(&self) -> i64`, `version_text(&self) -> String`,
  `version_conflict(&self, header: Option<&str>) -> bool`, `touch(&self)`,
  `save(&self, json: &str, expected: Option<i64>) -> Result<i64, String>` (Ok = saved version), `persist_generated(&self)`,
  `path(&self) -> &Path`. `pub const CONFLICT: &str` = the exact C# `ConfigStore.Conflict` text.
  `config::write::to_dotnet_json(&AgentConfig) -> String`.
- Version = .NET ticks of the file's last write time:
  `621_355_968_000_000_000 + (unix_time_nanos / 100)`; a missing header or one that does not parse is not a conflict.

- [ ] **Step 1: Write the new failing tests** (`writes_doubles_like_dotnet` in `src/config/write.rs`, the others in `src/config/store.rs`)

```rust
#[test]
fn writes_doubles_like_dotnet() {
    let mut cfg = AgentConfig::default();
    cfg.night.brightness = 1.0;
    cfg.weather.lat = 6.9271;
    let text = to_dotnet_json(&cfg);
    assert!(text.contains("\"brightness\": 1,") || text.contains("\"brightness\": 1\n"));
    assert!(text.contains("\"lat\": 6.9271"));
}

#[test]
fn version_is_dotnet_ticks_of_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.json");
    std::fs::write(&p, "{}").unwrap();
    let t = std::fs::metadata(&p).unwrap().modified().unwrap()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64;
    assert_eq!(ConfigStore::new(p).version(), 621_355_968_000_000_000 + t / 100);
}

#[test]
fn save_keeps_backup_and_owner_only_modes() { /* save twice; assert .bak equals first content; on unix both 0o600 */ }
```

- [ ] **Step 2: Run to see them fail** — `agent-rs/scripts/remote-test.sh config::store config::write` → FAIL
- [ ] **Step 3: Implement.** `to_dotnet_json`: serialize with a custom `serde_json::ser::Formatter` (two-space indent, `": "`), writing `f64` with shortest round-trip digits and no `.0` for whole values; non-ASCII written raw. Save order exactly as C#: validate (same messages), lock, compare mtime ticks with `expected` → `Err(CONFLICT)`, write `.tmp` owner-only, copy existing to `.bak` owner-only, rename `.tmp` over the file, return the new ticks.
- [ ] **Step 4: Byte-identical check against C#** — add corpus entries to `tools/parity/corpus/phase1.json` that `PUT /api/settings` the same body on both agents and compare the resulting `config.json` files with `compare: "bytes"` (harness option `--compare-config-file`). Run in Task 10's parity step.
- [ ] **Step 5: Run to see them pass** — same command → PASS
- [ ] **Step 6: Commit** — `git commit -m "Rust agent: config store, versions, conflicts, backups"`

---

### Task 8: Phone registry, WebView doctor, request timings, push store, state signal, state frames

**Files:**
- Create: `src/phone_registry.rs`, `src/webview_doctor.rs`, `src/request_timings.rs`, `src/push_store.rs`, `src/state_signal.rs`, `src/state_frame_cache.rs`
- Port from: the C# files of the same names (`PushStore.cs` without its webhook routes)
- Tests: ported `PhonesTests`, `PhoneRegistryConcurrencyTests`, `WebViewDoctorTests`, `RequestTimingsTests`, `PushCapacityTests`, `PushInputTests`, `PushShutdownTests`, `PushVersionTests`, `StateSignalTests`, `StateFrameCacheTests`

**Interfaces:**
- Produces (method names snake_case of the C# ones, same semantics):
  - `phone_registry::touch(device: &str, model: &str, via: &str, webview: &str) -> Option<String>` (the WebView
    problem text, once), `phone_registry::phones() -> Vec<PhoneSeen>`.
  - `webview_doctor::{parse, normalize, diagnose, diagnose_recent}`; `MIN_MAJOR = 105`, `RUN_MAJOR = 80`, `FRESH_MS = 120_000`.
  - `push_store::PushStore` with `seq() -> i64`, `version() -> i64`, `since(seq: i64) -> Vec<PushEvent>`,
    `subscribe() -> tokio::sync::broadcast::Receiver<()>`, plus the value/event methods the ported tests call.
  - `state_signal::{version() -> i64, poke(), subscribe() -> broadcast::Receiver<()>}`.
  - `state_frame_cache::StateFrameCache::new(build: Box<dyn Fn() -> serde_json::Value + Send + Sync>)`,
    `get(&self, push_seq: i64, signal_version: i64) -> Arc<str>` returning one whole-second frame
    `"event: state\ndata: <json>\n\n"`, rebuilt only when the second, `push_seq` or `signal_version` changed.

- [ ] **Step 1: Port the tests** — **Step 2: run, FAIL** — **Step 3: implement** — **Step 4: run, PASS** (`agent-rs/scripts/remote-test.sh phone_registry webview_doctor request_timings push_store state_`)
- [ ] **Step 5: Commit** — `git commit -m "Rust agent: phones, WebView check, timings, push store, state frames"`

---

### Task 9: HTTP server, request guard, single instance

**Files:**
- Create: `src/server/mod.rs` (`AppState`, `build_router`), `src/server/guard.rs`, `src/browser_guard.rs`, `src/single_instance.rs`
- Modify: `agent-rs/crates/phonebox-agent/src/main.rs`
- Port from: `Program.cs:1-37` (start-up), `Program.cs:186-249` (guard middleware), `BrowserGuard.cs`
- Tests: ported `RuleTests` / `TrackpadTests` methods that exercise `BrowserGuard`; new tests below

**Interfaces:**
- Consumes: `AgentEndpoint`, `ConfigStore`, `phone_registry::touch`, `webview_doctor`.
- Produces: `server::AppState { cfg: Arc<ConfigStore>, endpoint: AgentEndpoint, push: Arc<PushStore>, adb: Arc<AdbReverse>, timings: Arc<RequestTimings>, sources: Arc<StateSources> }`
  (`AdbReverse` from Task 12, `StateSources` from Task 11; until then use their `Default` stubs);
  `server::build_router(state: AppState) -> axum::Router`; `browser_guard::is_foreign(site: Option<&str>, mode: Option<&str>, method: &str, path: &str) -> bool`;
  `single_instance::acquire(name: &str, wait: Duration) -> Option<InstanceLock>`.
- Every response carries `Cache-Control: no-store` and `X-Frame-Options` `SAMEORIGIN` for `/panel*`, `DENY` otherwise.
  Requests with `X-PhoneBox-Device` call `phone_registry::touch` (`via` = `USB` on the local port) and log
  `WebView: <problem>` at warn under category `PhoneBoxAgent`; `X-PhoneBox-App` shorter than 32 chars sets the
  phone app version.

- [ ] **Step 1: Write the failing tests** (`rejects_foreign_host_and_origin` and `sets_cache_and_frame_headers` in `src/server/guard.rs`, the lock test in `src/single_instance.rs`)

```rust
#[tokio::test]
async fn rejects_foreign_host_and_origin() {
    let app = test_router();
    assert_eq!(get(&app, "/api/info", &[("host", "evil.example:8765")]).await.status(), 403);
    assert_eq!(get(&app, "/api/info", &[("host", "127.0.0.1:8765"), ("origin", "http://evil.example")]).await.status(), 403);
    assert_eq!(get(&app, "/api/info", &[("host", "127.0.0.1:8765")]).await.status(), 200);
    assert_eq!(get(&app, "/api/info", &[("host", "localhost:8765"), ("origin", "http://127.0.0.1:8765")]).await.status(), 200);
}

#[tokio::test]
async fn sets_cache_and_frame_headers() { /* /panel/index.html → SAMEORIGIN; /api/info → DENY; both no-store */ }

#[test]
fn second_instance_lock_fails_while_first_held() { /* acquire twice with the same name, Duration::ZERO → second None */ }
```

- [ ] **Step 2: Run, FAIL** — `agent-rs/scripts/remote-test.sh server:: browser_guard single_instance`
- [ ] **Step 3: Implement.** `main.rs` order mirrors `Program.cs`: parse endpoint → `--print-install-dir` prints `install_dir()` and exits 0 → acquire the instance lock (15 s wait with `--restart`); on failure open `settings_url()` with the OS opener and exit 0 → `ensure_config()` unless `--config` → absolute config path → `restrict_config` → `ConfigStore::new` → `persist_generated` → logger → bind `127.0.0.1:{port}` with a 16 KB request-line limit → serve. The instance lock is an OS-level exclusive lock on `<local_dir>/<lock name>.lock` (`fs4`), released on exit.
- [ ] **Step 4: Run, PASS** — same command
- [ ] **Step 5: Commit** — `git commit -m "Rust agent: HTTP server, request guard, single instance"`

---

### Task 10: Settings page, panel files, settings and config routes, info, latency, restart

**Files:**
- Create: `src/server/static_files.rs`, `src/server/routes_settings.rs`, `src/server/routes_info.rs`, `src/app_restart.rs`
- Port from: `Program.cs` routes `/`, `/favicon.ico`, `/panel/{*file}` (content types at `Program.cs:330-344`), `GET/PUT /api/settings` (`Program.cs:347-380`, incl. `LanPortCollidesWithLocal`, `LanKeysShared`), `GET /api/config` (`Program.cs:749-…`), `GET /api/info` (`Program.cs:408-…`), `GET /api/latency`, `POST /api/restart`, `AppRestart` (`UpdateService.cs:520-560`)
- Tests: new tests below; `AppRestartTests` methods about the exit code

**Interfaces:**
- Consumes: `AppState`, `ConfigStore`, `CONFLICT`.
- Produces: embedded assets via `rust-embed` from `agent/PhoneBox.Agent/{settings.html,widgets.json,templates.json,phonebox.ico}` and `android/app/src/main/assets/www/`; `app_restart::{requested() -> bool, request(), SERVICE_RESTART_CODE: i32 = 75, service_managed() -> bool}` (`XPC_SERVICE_NAME == "net.oxog.phonebox.agent"`, or systemd `INVOCATION_ID` + `SYSTEMD_EXEC_PID == pid`).

- [ ] **Step 1: Write the failing tests** (in `src/server/routes_settings.rs`, except `panel_files_are_the_embedded_bytes` in `src/server/static_files.rs`)

```rust
#[tokio::test]
async fn stale_save_conflicts_and_headerless_save_succeeds() {
    let (app, store) = test_app_with_config("{}");
    let v = store.version_text();
    let body = serde_json::to_string(&*store.get()).unwrap();
    assert_eq!(put_settings(&app, &body, Some(&v)).await.status(), 200);
    let r = put_settings(&app, &body, Some(&v)).await; // v is stale now
    assert_eq!(r.status(), 409);
    assert_eq!(json(r).await["error"], CONFLICT);
    assert_eq!(put_settings(&app, &body, None).await.status(), 200);
}

#[tokio::test]
async fn settings_put_requires_json_content_type() { /* text/plain → 415 */ }

#[tokio::test]
async fn panel_files_are_the_embedded_bytes() { /* GET /panel/main.js body == include_bytes!(".../www/main.js") */ }
```

- [ ] **Step 2: Run, FAIL** — **Step 3: implement** (restart: set the flag, shut the server down gracefully; `main` then exits with 75 when `service_managed()`, else re-executes itself with the same args plus `--restart`) — **Step 4: run, PASS** (`agent-rs/scripts/remote-test.sh server::routes app_restart`)
- [ ] **Step 5: Parity** — `agent-rs/scripts/remote-parity.sh tools/parity/corpus/phase1.json` for these routes. Expected: no differences (including the byte-for-byte `config.json` check from Task 7).
- [ ] **Step 6: Commit** — `git commit -m "Rust agent: settings page, panel, settings/config/info routes, restart"`

---

### Task 11: State and live stream

**Files:**
- Create: `src/state.rs` (`StateSources`, `build_state`), `src/server/routes_state.rs`
- Port from: `State()` (`Program.cs:469-492`), `GET /api/state`, `GET /api/stream` (`Program.cs:625-700`)

**Interfaces:**
- Consumes: `PushStore`, `state_signal`, `StateFrameCache`, `ConfigStore`.
- Produces: `trait StateSource: Send + Sync { fn snapshot(&self) -> serde_json::Value; }` and
  `state::StateSources { update, pc, media, audio, ping, rules, rules_paused, privacy, obs, toggles, work: Box<dyn StateSource> }`
  with `StateSources::phase1()` returning, for each key, the exact JSON the C# agent returns on headless Linux with
  the parity fixture (record them with the harness, store as `src/state_empty.json`, load with `include_str!`).
  `state::build_state(sources: &StateSources, cfg: &ConfigStore, push: &PushStore, ev: Option<i64>) -> serde_json::Value`
  with keys in this order: `update, ts, host, cfgVersion, pushVersion, evSeq, events, pc, media, audio, ping, rules,
  rulesPaused, privacy, obs, toggles`; nulls are written (not skipped). `host` = the machine's host name cut at the
  first `.`, as .NET's `Environment.MachineName`.
- Stream: `Content-Type: text/event-stream`, `X-Accel-Buffering: no`; log `live stream opened (phone|other client, USB/local)`;
  `event` frames for each new push event; a `state` frame at every whole second and within 100 ms after a
  `state_signal::poke`; a `work` frame on every second tick when its JSON changed.

- [ ] **Step 1: Write the failing tests** — `state::tests::keys_in_csharp_order` (serialize `build_state` and assert the key sequence above); `routes_state::tests::stream_sends_state_each_second` (read 2 frames within 2.5 s, both start with `event: state\n`); `routes_state::tests::poke_sends_state_within_100ms`.
- [ ] **Step 2: Run, FAIL** — **Step 3: implement** — **Step 4: run, PASS** (`agent-rs/scripts/remote-test.sh state routes_state`)
- [ ] **Step 5: Parity** — add `/api/state` (ignoring `/ts`, `/cfgVersion`, `/host` only) to the corpus; run `remote-parity.sh`. Expected: no differences.
- [ ] **Step 6: Commit** — `git commit -m "Rust agent: state and live stream"`

---

### Task 12: adb: discovery, reverse tunnel, wake and open, USB doctor

**Files:**
- Create: `src/adb.rs` (`find_adb`, `AdbReverse`), `src/usb_doctor.rs`
- Port from: `AdbReverse.cs`, `UsbDoctor.cs`
- Tests: ported `AdbReverseTests`, `AdbUnreachablePhoneTests`; new test below

**Interfaces:**
- Consumes: `ConfigStore` (`adbPath`, `deviceSerial`), `AgentEndpoint`.
- Produces: `adb::find_adb(configured: Option<&str>) -> Option<PathBuf>` (configured path first, then each `PATH` entry,
  then `~/Android/Sdk/platform-tools`, `~/Library/Android/sdk/platform-tools`, `/opt/homebrew/bin`, `/usr/local/bin`,
  `/usr/bin`, `/usr/lib/android-sdk/platform-tools`; Windows list as in C#); `adb::AdbReverse::spawn(state) -> Arc<AdbReverse>`
  with `connected() -> bool`, `all_devices() -> Vec<String>`, `missing_app() -> Vec<String>`, `usb_problem() -> Option<String>`.
  The adb runner is a trait `AdbRunner` (`async fn run(&self, args: &[&str]) -> Result<String, String>`) so tests use a fake.
- Log lines, exact: `<serial>: adb reverse set up, panel opened`, `<serial>: screen saver off, stays awake while plugged in`,
  `phone connected: <serial>`, `no phone connected, waiting...` (category `AdbReverse`).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test(start_paused = true)]
async fn survives_missing_adb_and_unplug() {
    let fake = FakeAdb::new();
    fake.devices(&[]);                                   // nothing plugged in
    let adb = AdbReverse::spawn_with(fake.clone(), test_cfg());
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(!adb.connected());
    fake.devices(&["8UE5T17A26006928"]);                 // plugged in
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(adb.connected());
    assert!(fake.calls().iter().any(|c| c == "-s 8UE5T17A26006928 reverse tcp:8765 tcp:8765"));
    fake.fail_all("adb: not found");                     // adb gone / server killed
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(!adb.connected());
}
```

- [ ] **Step 2: Run, FAIL** — **Step 3: implement** (port the C# loop and its timings exactly; the reverse maps phone `tcp:8765` to `tcp:{endpoint.port}`) — **Step 4: run, PASS** (`agent-rs/scripts/remote-test.sh adb usb_doctor`)
- [ ] **Step 5: Coverage** — `python3 agent-rs/scripts/port-status.py --check 1` → exit 0
- [ ] **Step 6: Commit** — `git commit -m "Rust agent: adb discovery, reverse tunnel, wake, USB doctor"`

---

### Task 13: Phase-1 parity on all platforms and the real-phone check

**Files:**
- Modify: `.github/workflows/rust.yml` (add a `parity` job: macOS arm64, macOS x64, Windows x64 runners; setup .NET 10 and Rust; run `tools/parity/parity.py` with the phase-1 corpus)
- Create: `agent-rs/scripts/mac-phone-check.sh`

**Interfaces:**
- Consumes: everything above.
- Produces: green `parity` job; `mac-phone-check.sh` (the owner-approved laptop exception).

- [ ] **Step 1: CI parity** — push; `gh run watch`. Expected: `parity` job green on all three runners (Linux runs on hetzner via `remote-parity.sh`).
- [ ] **Step 2: Write `mac-phone-check.sh`** — `cargo build --release`; record the installed agent's PID (`lsof -ti tcp:8765 -sTCP:LISTEN`); `launchctl unload ~/Library/LaunchAgents/net.oxog.phonebox.agent.plist` so launchd does not restart it; start `target/release/PhoneBoxAgent --config <copy of the real config.json in a temp folder>` on port 8765, saving its PID; wait for `phone connected` in its log; print the instructions below; on Enter, `kill <rust PID>`, `launchctl load` the plist, and wait until `/api/info` answers again.
- [ ] **Step 3: Run the check on the MacBook with the phone on USB**

Run: `agent-rs/scripts/mac-phone-check.sh`
Expected: the phone shows the panel (clock, page dots, placeholders for unported widgets), the Rust agent's log has
`<serial>: adb reverse set up, panel opened`, and after Enter the installed C# agent is back (`/api/info` reports
version `0.9.22`, `connected: true`).

- [ ] **Step 4: Commit and push**

```bash
git add .github/workflows/rust.yml agent-rs/scripts/mac-phone-check.sh agent-rs/PORTING.md
git commit -m "Rust agent: phase-1 parity in CI, real-phone check"
git push fork rust-agent
```
