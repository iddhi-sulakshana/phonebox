# Rust agent plan 5: AI and dev Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Rust agent shows the phone what the C# agent shows for AI coding tools and dev work: running Claude
Code / Codex / Cursor sessions and their conversation, Claude Code permission prompts answered from the phone, quota
meters (Claude, Codex, Antigravity, CodexBar), GitHub pull requests, Docker containers and command widgets.

**Architecture:** One module per C# source, ported with their tests, each with explicit seams (roots, clocks, HTTP and
process runners) instead of C#'s process-wide statics, so Rust's parallel tests stay independent. Background loops
(sessions 2 s, GitHub 3 min, quotas 30 s / 5 s, Docker on demand) start in `main` under the shared `Stop`; one new
route module serves `/api/work*`, `/api/quotas`, `/api/approvals*`, `/api/hooks/claude/permission`, `/api/claude/hook`,
`/api/docker*` and `/api/widget/*` on both listeners (the Wi-Fi allow-list from plan 4 already decides which).

**Tech Stack:** Rust 1.98, axum 0.8, tokio (process, time), reqwest 0.12 (rustls under `tls`), hyper-util + hyper-rustls
(the Claude usage client, under `tls`), sysinfo 0.40, regex, chrono, sha2, serde_json (`preserve_order`, `raw_value`).

**Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md`. The C# agent at `bb72345` is the behavioral spec.
Survey with every rule, string and file:line: `/tmp/plan5-survey.md`, copied into the plan workspace as `survey.md`
(below "survey §N").

## Global Constraints

- JSON as C# writes it: camelCase keys in the record order the survey lists, nulls written (routes), doubles in .NET's
  shortest form (`json_response`). The quota files `claude-quota.json` / `claude-quota-schedule.json` in `local_dir()`
  use the config writer's options instead (indented, nulls left out, case-sensitive keys) and are written through
  `{path}.{uuid}.tmp` + rename, owner-only: the installed C# agent shares them.
- Cuts count UTF-16 units: 3000 (hook detail), 4000 (transcript text, command output — surrogate-safe), 200 (titles),
  40 (program names). `Math.Round(x, 1)` is half-to-even; the custom formats `0`, `0.#`, `0.##`, `#,0` round half away
  from zero and drop trailing zeros; all invariant culture.
- A Claude token is read, fingerprinted (SHA-256 of its UTF-8, upper-case hex, first 16) and sent only to
  `https://api.anthropic.com/api/oauth/usage`; it is never logged, stored, printed in test output or committed. Tests
  use fake tokens and a fake HTTP seam; no test reaches the network.
- Every reader takes its roots (home, Codex sessions, Cursor projects and workspace storage, credentials path, local
  dir) as parameters; production passes the real ones. No test reads the real home.
- Processes run through `tool_process` (async, both streams drained, `neutral_cwd`, stdin null, whole tree killed on
  timeout or cancel, timeout and cancel told apart). Blocking calls (`ioreg`, `security`, `ps`, directory scans) run
  off the async workers.
- Settings: `claude.phoneApprovals` (true), `claude.timeoutSec` (90, 10–3600), `claude.shareConversation`,
  `claude.onlyWhenAway`, `claude.shareSessionTitles`, `cursor.sessions` (all false), widget options `docker.control`,
  `codexbar|quotas.showOverview`, `command.command|commandUnix|interval` — read live on every use.
- `/api/hooks/claude/permission` and `/api/claude/hook` stay local-only (not on the Wi-Fi allow-list).
- Tests run on the server (`agent-rs/scripts/remote-*.sh`); the Mac exit check is the owner-approved laptop exception
  (short-lived, stopped by PID, the installed agent restored, the real `~/.claude/settings.json` never written).

## Review Focus

1. A permission request whose curl client goes away (the user answered in the terminal): the prompt must leave the
   phone at once (`approval-done`) and a late phone decision must be refused.
2. The quota files shared with a running C# agent: a file written by either agent is read by the other; no extra
   rate-limited request happens because the two agents disagree on the schedule format.
3. Rewriting `~/.claude/settings.json`: key order, other users' hooks, comments dropped as in C#, number text such as
   `1.0` kept, the backup and the owner-only mode; a malformed file left byte-identical.
4. A command widget or CLI that hangs or leaves a background child: the widget answers within its bounds and no process
   outlives its timeout (Unix process group, Windows descendants).
5. Huge or growing transcripts and rollouts (MBs, partial last lines, a file replaced by a shorter one): bounded memory,
   no panic, the same sessions and titles as C#.

---

### Task 1: Process runner, wake, idle, phase-5 test mapping

**Files:** Create `tool_process.rs`, `wake.rs`, `idle.rs`; modify `lib.rs`, `agent-rs/scripts/port-status.py` (survey
§4.6 entries), `agent-rs/PORTING.md`.

**Interfaces:**
- Produces: `tool_process::{find(tool) -> Option<PathBuf>, Cancel (clonable: cancel(), is_cancelled(),
  cancelled().await), ToolCommand { program, args, env }, run(cmd, timeout, &Cancel) -> Result<Output { code, stdout,
  stderr }, RunError>}` with `RunError::{Timeout("<name> timed out"), Cancelled, Start(String)}`; the tree kill is
  `process_group(0)` + `killpg` on Unix and the descendants (sysinfo parent pids) then the child on Windows, reaped
  before `run` returns. `wake::{count() -> u64, now(), delay(d, &Stop)}`. `idle::seconds() -> f64` (blocking; survey
  §1.3 per OS).

- [ ] **Step 1: Tests (RED):** `drains_both_streams_and_reports_failure`, `timeout_terminates_the_child_and_cancellation_remains_cancellation`
  (the child's pid is gone afterwards), `a_grandchild_is_killed_with_its_parent` (Unix: `sh -c 'sleep 30 & sleep 30'`),
  `find_looks_in_path_then_the_homebrew_folders`, `wake_ends_every_delay_and_counts`, `idle_parses_ioreg_and_xprintidle`
  (parsers split out of the process calls).
- [ ] **Step 2: Implement.** Add the survey §4.6 `PHASE_BY_FILE` / `PHASE_BY_METHOD` entries (CodexBarTests'
  RuleEngine test → 6) and regenerate `PORTING.md`.
- [ ] **Step 3:** PASS; cross-check clean. **Commit** `Rust agent: process runner, wake, idle`.

### Task 2: Claude transcripts

**Files:** Create `claude_transcript.rs`.

**Interfaces:**
- Produces: `ClaudeTranscript` (instance: the bounded cache), `ConversationMessage { role, text, at }`,
  `ConversationStep { tool, detail, at, running }`, `ConversationView { messages, steps }`;
  `find_file(home, session_id) -> Option<PathBuf>`, `read(&self, path) -> ConversationView`,
  `trim(&self, running_ids: &[String], enabled, now)`, `for_phone(&self, enabled, running: &[AgentSession], id, home)
  -> serde_json::Value`, `program(command) -> Option<String>`, `is_cached(path)`.

- [ ] **Step 1: Tests (RED):** the 12 `ClaudeTranscriptTests` (survey §4.1; all 43 `ACommandIsShownAsTheProgramItRuns`
  rows as a table).
- [ ] **Step 2: Implement** survey §1.2 (4 MiB tail, 2 Mi line cap, 30 messages / 12 steps, merge rule, `Clean`,
  `Detail`, `Program`).
- [ ] **Step 3:** PASS. **Commit** `Rust agent: Claude transcripts`.

### Task 3: Sessions: Claude, titles, WorkBuddy names, the work feed

**Files:** Create `work_buddy.rs`, `session_monitor/mod.rs`, `session_monitor/claude.rs`.

**Interfaces:**
- Produces: `work_buddy::WorkBuddyNames` (instance with a `read` seam) `for_cwd(cwd, home) -> Option<String>`;
  `AgentSession` (22 fields, survey §1.1 order); `SessionMonitor { cfg: Option<Arc<ConfigStore>>, home, roots… }`
  with `snapshot() -> Vec<AgentSession>`, `set_sessions_for_test`, `read_claude(home, is_alive, titles)`,
  `read_claude_title`, `clean_title`, `is_codex_bar_probe`, `is_claude_mem_observer`; `work_feed(sessions, github)`;
  `alive(pid)`.

- [ ] **Step 1: Tests (RED):** the 8 `WorkBuddyNamesTests` except `ThePermissionCard…` (Task 6), the 11
  `SessionTitleTests` except `ThePermissionCardNeverShowsTheTitle` (Task 6), `SessionDetailsTests.Claude_*` (3),
  `MonitorAuditTests.ClaudeSessionFileOfAReusedPidIsNotShownAsALiveSession` (`#[cfg(windows)]`).
- [ ] **Step 2: Implement** survey §1.1 (Claude part) and §1.5; `alive` = `kill(pid,0)` or EPERM on Unix.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: Claude sessions and titles`.

### Task 4: Sessions: Codex and Cursor

**Files:** Create `session_monitor/codex.rs`, `session_monitor/cursor.rs`; modify `session_monitor/mod.rs` (`tick`).

**Interfaces:**
- Produces: `SessionMonitor::{read_codex(root), read_codex_cwd(path), read_codex_details(path, previous, offset),
  read_cursor(projects_root, workspace_storage_root), read_cursor_last_role(path), cursor_project_slug,
  cursor_project_fallback, has_cached_cursor_role, tick(&self)}` (one pass: Claude + Codex + Cursor when on, then the
  transcript trim).

- [ ] **Step 1: Tests (RED):** the other 12 `SessionDetailsTests`;
  `MonitorAuditTests.CodexProjectComesFromALongOrBomPrefixedFirstRecordAndWaitsForAnIncompleteOne`.
- [ ] **Step 2: Implement** survey §1.1 (Codex, Cursor). Codex details are read in chunks from the offset (complete
  lines only), never the whole file at once.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: Codex and Cursor sessions`.

### Task 5: Approvals

**Files:** Create `approval_store.rs`.

**Interfaces:**
- Consumes: `PushStore::emit`, `server::PhoneSeenState`.
- Produces: `Approval` (10 fields, survey §1.4), `ApprovalStore::new(push, phone: Arc<PhoneSeenState>, clock)`,
  `snapshot()`, `ask(a) -> impl Future<Output = Option<String>>` (dropping the future ends the request like C#'s
  `aborted`: the entry is removed and `approval-done` emitted), `decide(id, decision) -> bool`, `new_id()`.

- [ ] **Step 1: Tests (RED):** `ApprovalOutcomeTests` (2), `ApprovalExpiryTests` (2), plus
  `dropping_the_request_removes_it_and_refuses_a_late_decision`, `no_phone_no_prompt`.
- [ ] **Step 2: Implement** survey §1.4 (`PhoneConnected` = last seen within 5 s; strict expiry; the decision wins
  only if the request returns it).
- [ ] **Step 3:** PASS. **Commit** `Rust agent: approvals`.

### Task 6: The Claude Code hook

**Files:** Create `claude_hook.rs`; modify `config/write.rs` (indented `JavaScriptEncoder.Default` writer that keeps
the number text of a `RawValue`-parsed document).

**Interfaces:**
- Produces: `claude_hook::{ENDPOINT, handle(input: &Value, cfg, approvals, sessions, idle_seconds) -> Option<String>`
  (async), `project_name(cwd, home, names)`, `tool_title`, `detail`, `question_detail`, `hook_command(port)`,
  `installed(path) -> bool`, `configure(install, port, timeout_sec, path) -> Option<String>`, `settings_path(home)`}.

- [ ] **Step 1: Tests (RED):** `QuestionPromptTests` (5), `StoreFollowUpTests.ReplacingClaudeSettingsKeepsThemOwnerOnly`,
  `StoreFollowUpTests.AMalformedHooksValueIsLeftAsItIs`, `SessionTitleTests.ThePermissionCardNeverShowsTheTitle`,
  `WorkBuddyNamesTests.ThePermissionCardPrefersTheWorkspaceNameOverTheFolder`, plus
  `configure_keeps_key_order_and_number_text` (`1.0`, `1e3` and key order survive an install and a removal).
- [ ] **Step 2: Implement** survey §1.3 (comments and trailing commas accepted on read; backup before the shape check;
  `.tmp` + rename, 0600; CRLF on Windows).
- [ ] **Step 3:** PASS. **Commit** `Rust agent: Claude Code hook`.

### Task 7: Quota records, the IPv4-first connect, Claude helpers and cache

**Files:** Create `quota/mod.rs` (records, `rolled`, `QuotaMonitor` shell), `quota/connect.rs`, `quota/claude.rs`
(credentials, backoff, cache and schedule files, `fetch`).

**Interfaces:**
- Produces: `QuotaLimit`, `CodexUsage`, `QuotaProvider` (survey §1.6 order); `connect::{ipv4_first(addrs),
  connect_in_order(addrs, port, budget, connect_fn, cancel), connect_ipv4_first(host, port, cancel)}`;
  `UsageHttp` seam `Arc<dyn Fn(UsageRequest) -> BoxFuture<Result<UsageReply, HttpFail { Connect(msg), Other(msg) }>>>`
  and the real client (hyper-util + hyper-rustls over `connect_ipv4_first`, 15 s, `User-Agent: PhoneBox/<version>`;
  without `tls` every request is `Other("this build has no TLS")`); `claude::{sign_in_expiry, pick_credentials,
  claude_delay, retry_after, plan_label, parse_claude_limits, claude_extra, fingerprint}`, cache load/save.

- [ ] **Step 1: Tests (RED):** `QuotaConnectTests` (5), `ClaudeQuotaBackoffTests` (5), `ClaudeQuotaCacheTests` (3),
  `MonitorAuditTests.QuotaLimitWhoseWindowHasResetIsNotReportedAsUsedUp`, plus
  `the_cache_file_round_trips_with_the_csharp_format` (a C#-written fixture reads; a written file has the C# keys and
  no nulls).
- [ ] **Step 2: Implement** survey §1.6 (helpers, files).
- [ ] **Step 3:** PASS; cross-check clean. **Commit** `Rust agent: quota records and Claude helpers`.

### Task 8: The Claude quota schedule

**Files:** Modify `quota/claude.rs` (`claude_tick(now_ms)`, `adopt_shared`, `keep`), `quota/mod.rs`.

**Interfaces:**
- Produces: `QuotaMonitor::claude_tick(&self, now_ms)` (async), the Keychain seam `read_keychain:
  Option<Arc<dyn Fn() -> Option<String>>>` (macOS default only with the default credentials path).

- [ ] **Step 1: Tests (RED):** the 28 `ClaudeQuotaScheduleTests` with a fake `UsageHttp` that counts calls and checks
  the URL and bearer.
- [ ] **Step 2: Implement** survey §1.6 (`ClaudeTickAsync` steps 1–12, `AdoptShared`, the merge of the shared
  schedule, local `HH:mm`).
- [ ] **Step 3:** PASS. **Commit** `Rust agent: Claude quota schedule`.

### Task 9: Codex quota and CodexBar

**Files:** Create `quota/codex.rs`, `codex_bar.rs`; modify `quota/mod.rs` (`snapshot`, `overview`).

**Interfaces:**
- Produces: `quota::codex::{read_codex(root), read_codex_usage(path), last_line_with(path, needle)}`;
  `codex_bar::{parse(json, exit_code), parse_cost(json, exit_code, reference_date), select(local, external, now),
  select_overview, display_name, read(path), read_cost(path)}`; `QuotaSubscriptionCost`, `QuotaTopModelCost`,
  `QuotaOverview`; `QuotaMonitor::{snapshot(now), overview(now), refresh_overview_once(...)}`.

- [ ] **Step 1: Tests (RED):** `CodexQuotaTests` (3; the locked-log one `#[cfg(windows)]`), the 11 phase-5
  `CodexBarTests`, `MonitorAuditTests.CodexBarTextsDoNotDependOnTheWindowsLocale`,
  `MonitorAuditTests.CodexRolloutsUnderOneMegabyteDoNotDropFirstLineTurnContextOrRateLimits`.
- [ ] **Step 2: Implement** survey §1.6 (Codex) and §1.8.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: Codex quota and CodexBar`.

### Task 10: Antigravity

**Files:** Create `quota/antigravity.rs`.

**Interfaces:**
- Produces: `AntigravityEndpoint { port, csrf_token, scheme, host }` (`Display` without the token),
  `parse_endpoint_from_command_line`, `parse_https_server_from_command_line`, `find_antigravity_endpoints(read_commands,
  listening_ports)`, `read_antigravity(http, endpoints, cancel, https)`, `read_antigravity_cli(path, cancel, run)`,
  `parse_antigravity_limits`, `parse_antigravity_plan`, `parse_antigravity_legacy_limits`,
  `accept_loopback_certificate(url, has_errors)`, the loopback TLS client (`tls`).

- [ ] **Step 1: Tests (RED):** the 32 `AntigravityTests` (the self-signed server test under `#[cfg(feature = "tls")]`,
  its certificate from `lan_cert::create`).
- [ ] **Step 2: Implement** survey §1.7; command lines from sysinfo `cmd()` on every OS.
- [ ] **Step 3:** PASS; cross-check clean. **Commit** `Rust agent: Antigravity quota`.

### Task 11: GitHub and Docker

**Files:** Create `github_monitor.rs`, `docker_monitor.rs`.

**Interfaces:**
- Produces: `GhPr`, `GhSnapshot`, `GitHubMonitor::{snapshot(), start(stop)}`, `github_monitor::{gh(args, cancel),
  parse_graphql(json)}`; `DockerContainer`, `DockerSnapshot`, `DockerMonitor::{snapshot(), snapshot_cold().await,
  control_allowed(), act(id, action).await, start(stop)}` with `cli_override` and `runner_override` seams;
  `parse_containers`, `parse_stats`.

- [ ] **Step 1: Tests (RED):** `DockerTests` (4), `DockerActionTests` (2),
  `ToolProcessTests.CancellingGitHubRefreshReapsTheCli`, plus `github_graphql_parses_mine_and_review_requests` and
  `a_failed_fetch_keeps_the_last_snapshot_with_the_error`.
- [ ] **Step 2: Implement** survey §1.9 and §1.10 (the Docker CLI list once per process).
- [ ] **Step 3:** PASS. **Commit** `Rust agent: GitHub and Docker`.

### Task 12: Command widgets

**Files:** Create `command_runner.rs`.

**Interfaces:**
- Produces: `CommandResult`, `CommandRunner::{get(page, widget, force) -> Option<CommandResult>, key(page, widget,
  command)}` with a `start_worker` seam; `truncate_output`.

- [ ] **Step 1: Tests (RED):** `CommandVisibilityTests` (4),
  `AuditRegressionTests.ABackgroundProcessLeftByTheCommandDoesNotBlockTheWidget`.
- [ ] **Step 2: Implement** survey §1.11 (login shell, 64 KiB caps, 1 s pipe wait after exit, 20 s tree kill).
- [ ] **Step 3:** PASS. **Commit** `Rust agent: command widgets`.

### Task 13: Routes, loops and wiring

**Files:** Create `server/routes_dev.rs`; modify `server/mod.rs` (`AppState` fields: `sessions`, `transcripts`,
`approvals`, `quotas`, `github`, `docker`, `commands`, `work_names`), `state.rs` (`with_work`), `process_monitor.rs`
(`AgentSessions` for `SessionMonitor`), `phonebox-agent/src/main.rs` (start the loops).

**Interfaces:**
- Consumes: Tasks 1–12.
- Produces: the 12 routes of survey §2 with their exact shapes and status codes; the permission route's guard emits
  `approval-done` when the client disconnects.

- [ ] **Step 1: Tests (RED):** route tests for every row of survey §2 (shapes, key order, 400/404/405), the work event
  on the stream, `a_hook_request_whose_client_disconnects_ends_its_prompt` (a real socket closed mid-request),
  `the_hook_routes_are_not_served_over_wifi`.
- [ ] **Step 2: Implement**; `main` starts sessions (2 s), GitHub (3 s then 3 min), quotas (usage 30 s and overview
  5 s), Docker (on demand) under `Stop`.
- [ ] **Step 3:** PASS; phases 1–4 still pass. **Commit** `Rust agent: AI and dev routes`.

### Task 14: Exit: parity and the MacBook round trip

**Files:** Create `tools/parity/corpus/phase5.json`; create `agent-rs/scripts/mac-dev-check.sh`; modify
`.github/workflows/rust.yml`.

- [ ] **Step 1: Corpus:** `/api/work` (github ignored, sessions empty in the parity home), conversation with and
  without `id`, `/api/quotas` (no tools in the parity home), `/api/approvals`, a decision for an unknown id, without
  `decision`, `/api/claude/hook` GET (path ignored), `/api/docker` (error text ignored), `/api/docker/x/start`, a
  command widget on a missing page, the hook endpoint with invalid JSON, the Wi-Fi denial of the hook routes. All five
  corpora pass.
- [ ] **Step 2: Mac check (laptop exception):** the Rust agent stands in on 8765 with the owner's config, a temporary
  home with `~/.claude` and `~/.codex` linked in and the quota files copied; `/api/work` lists the same sessions as the
  installed agent, `/api/quotas` the same providers; a Claude Code permission prompt reaches the phone and is answered
  there (or, with the phone away, `/api/approvals` lists it and a POST answers it); the installed agent is restored.
- [ ] **Step 3:** `port-status.py --check 5` clean; full suite, cross-check, all corpora green. **Commit** `Rust agent:
  plan 5 exit: AI and dev parity and the MacBook round trip`.
