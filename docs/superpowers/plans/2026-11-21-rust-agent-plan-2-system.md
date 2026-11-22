# Rust agent, plan 2: system — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port the PC monitors (CPU, memory, disks, network, GPUs, temperatures), the ping monitor, processes,
listening ports and batteries, so `pc` and `ping` in `/api/state` and the routes `/api/procs*`, `/api/batteries` and
`/api/network/adapters` answer as the C# agent does.

**Architecture:** One Rust module per C# file, as in plan 1 (`Unix/` becomes `unix/`). Parsers and decisions are pure
functions tested on every OS; the OS readers behind them are `cfg`-gated and compile-checked for Windows and macOS on
the server. Each monitor runs as a tokio task (blocking reads in `spawn_blocking`), ends on the agent's `Stop`, and
replaces its phase-1 `Fixed` state source. `ProcessMonitor` reads agent sessions through a small trait until plan 5
ports `SessionMonitor`.

**Tech Stack:** as plan 1, plus `sysinfo` (shared base for CPU, memory, disks, processes, per the spec), `windows`
(GPU counters, D3DKMT, memory/CPU times, TCP tables, process queries, SetupAPI batteries), `core-foundation` + IOKit
FFI (SMC temperatures), `hidapi` (Razer batteries), `libc`.

**Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md` · **Roadmap:** `docs/superpowers/plans/2026-11-20-rust-agent-roadmap.md` · **C# scope:** `SystemMonitor.cs`, `Gpus.cs`, `WindowsGpu.cs`, `DrmUsage.cs`, `PingMonitor.cs`, `ProcessMonitor.cs`, `BatteryMonitor.cs`, `BatteryMonitor.Razer.cs`, `ListeningPorts.cs`, `Lan.cs` (`Addresses`, `ShouldSkipAdapter`), `Unix/Sh.cs`, `Unix/UnixSystem.cs`, `Unix/AppleSmc.cs`, `Unix/UnixProc.cs`, and their routes in `Program.cs`.

## Global Constraints

- Everything in plan 1's Global Constraints still holds (baseline `bb72345`, tests only on `hetzner-iddhi` or CI,
  laptop exceptions for hardware checks stopped by PID, no AI co-author trailer, version `1.0.0-alpha.N`).
- Owner's run mode (2026-11-21): plans 2–8 run back to back; no per-phase review; nothing is pushed until the final
  review, so macOS and Windows code is checked with cross-target `cargo clippy` on the server (Task 1).
- JSON as the C# agent writes it: camelCase property names in the record's declaration order, `null` fields written
  (no ignore condition), numbers through the existing .NET writer (`to_dotnet_json*`).
- `DrmUsage.cs` moves from plan 6 to this plan: `Gpus.ReadSysfs` needs it (roadmap updated in Task 1).
- External tools stay external and are called exactly as the C# agent calls them (`nvidia-smi` arguments, `ioreg -r -d
  1 -w 0 -c IOAccelerator`, `ps`, `lsof`, `system_profiler`, `ping`), with the same timeouts.
- The parity corpus never kills a real process: kill requests use names and pids that cannot exist.

## Review Focus

1. **A machine with no GPU, no hwmon sensors, no SMC and no `nvidia-smi`** (a VM, the CI runner) must still publish
   `pc` every second with `gpu: null`, `gpus: []`, `cpuTemp: null`, and log the missing source once, not every
   tick. Pinned by `system_monitor::tests::a_machine_without_gpu_or_temperature_sources_still_publishes_pc` (Task 9).
2. **A process that exits between being listed and being read** (its `/proc` entry, `ps` row or handle is gone) is
   left out of the groups and the detail; `/api/procs` never answers 500. Pinned by
   `process_monitor::tests::a_process_that_exits_while_listing_is_left_out` (Task 12).
3. **Killing the agent itself, its parent, or a protected process** (the C# `Protected` set, PID 0/1) is refused
   before any signal is sent. Pinned by `process_monitor::tests::kill_refuses_the_agent_and_protected_processes`
   (Task 12).
4. **A `ping.host` that does not resolve, or a machine where unprivileged ICMP is not allowed**, counts as a lost
   ping (`ms: null`, loss rises), falls back as .NET does, and never panics or logs every second. Pinned by
   `ping_monitor::tests::an_unresolvable_host_counts_as_loss` (Task 10).
5. **`/api/procs?top=` with 0, a negative number, a huge number or text** answers exactly as the C# agent. Pinned by
   the `phase2.json` parity requests `procs-top-*` (Task 14).

---

### Task 1: Tooling — phase-2 coverage, test-class attribution, cross-target checks, roadmap

**Files:**
- Modify: `agent-rs/scripts/port-status.py`, `docs/superpowers/plans/2026-11-20-rust-agent-roadmap.md`
- Create: `agent-rs/scripts/remote-cross-check.sh`
- Regenerate: `agent-rs/PORTING.md`

**Interfaces:**
- Produces: `port-status.py --check 2`; `agent-rs/scripts/remote-cross-check.sh` (exit code = clippy's).

- [ ] **Step 1: Attribute test methods to their public test class.** `csharp_tests()` currently switches class on any
  `class`/`record` match, so methods after a private helper (`Fixture`, `Handler`, `FakeRun`) or a string such as
  `<class AGXAcceleratorG14X>` land under the wrong class. Only a line declaring a `public` class or record (and not
  inside a string) starts a new test class. Run `python3 -I agent-rs/scripts/port-status.py` and check: no class
  named `Fixture`, `Handler`, `FakeRun`, `Capture`, `Slow`, `StubHandler`, `Clock`, `AGXAcceleratorG14X` remains in
  `PORTING.md`; phase-1 rows unchanged (`--check 1` still passes).
- [ ] **Step 2: Phase-2 map.** `PHASE_BY_FILE`: `GpuSourceTests`, `GpuIntegratedMergeTests`, `AppleGpuTests`,
  `CpuTempTests`, `CpuTempMalformedTests`, `AppleSmcTests`, `BatteryTests`, `BatteryParsingTests`,
  `ListeningPortsTests`, `PingMonitorTests`, `NetworkAdapterTests`, `NetworkRatesTests`, `ShTests`, `DrmUsageTests`
  → 2. `PHASE_BY_METHOD`: `MonitorAuditTests.ProcessDetailToleratesDuplicateSessionPidsAndLocksAgentProcess` and
  `MonitorAuditTests.ColdProcessIconDoesNotCacheNullAndShortenArgsHandlesTabSeparators` → 2 (the other
  `MonitorAuditTests` methods belong to plans 5 and 6). `LanTests.VirtualAdaptersAreSkipped…` is already 2.
- [ ] **Step 3: Cross-target check script.** `remote-cross-check.sh` syncs like `remote-test.sh` and on the server runs
  `cargo clippy --workspace --all-targets --target <t> -- -D warnings` for `x86_64-pc-windows-msvc`,
  `aarch64-apple-darwin` and `x86_64-apple-darwin` (installing the targets with `rustup target add` once). Run it on
  the current tree. Expected: clean for all three (plan 1 code already compiles there in CI).
- [ ] **Step 4: Roadmap.** Move `DrmUsage` from plan 6's scope to plan 2's, with the reason (`Gpus.ReadSysfs`).
- [ ] **Step 5: Commit** `Rust agent: plan 2 tooling: phase-2 coverage, test classes, cross-target checks`.

### Task 2: `unix/sh.rs` — `Sh`

**Files:** Create `agent-rs/crates/phonebox/src/unix/mod.rs`, `unix/sh.rs`; modify `lib.rs`.

**Interfaces:**
- Produces: `sh::which(tool: &str) -> Option<PathBuf>`, `sh::run(tool: &str, args: &[&str]) -> Option<String>`
  (2000 ms), `sh::run_with_timeout(ms: u64, tool: &str, args: &[&str]) -> Option<String>`,
  `sh::read_file(path: &Path) -> Option<String>`, `sh::uid() -> u32` (0 on Windows).

- [ ] **Step 1: Port `ShTests` (1 test)** to `unix::sh::tests`; run `agent-rs/scripts/remote-test.sh sh::` → FAIL
  (module missing).
- [ ] **Step 2: Implement** from `Unix/Sh.cs`: same PATH search, a child killed at the timeout (`kill_on_drop`),
  stdout only, `None` on non-zero exit, start failure or timeout; working directory `app_paths::neutral_cwd()`.
- [ ] **Step 3: Run** → PASS. **Commit** `Rust agent: Sh`.

### Task 3: CPU, memory, disks and uptime readers

**Files:** Create `unix/unix_system.rs`, `system_monitor.rs` (readers and records only); modify `Cargo.toml` (`sysinfo`,
`windows` with the features this plan uses, `libc` on unix).

**Interfaces:**
- Produces: `unix_system::{cpu_times() -> (i64, i64), memory() -> (f64, f64), cpu_name() -> String}`;
  `system_monitor::{DiskInfo { name: String, used_gb: f64, total_gb: f64 }, cpu_times(), memory(), read_disks() ->
  Vec<DiskInfo>, read_cpu_name() -> String, disk_label(name: &str) -> String, uptime_sec() -> i64}`.

- [ ] **Step 1: Write the tests first** (no C# tests cover these readers): `memory_total_is_positive_and_used_is_within_it`,
  `cpu_times_grow_between_two_reads`, `disks_have_a_name_and_used_within_total`, `disk_label_matches_csharp`
  (cases from `DiskLabel`), in `system_monitor::tests`. Run → FAIL (not defined).
- [ ] **Step 2: Implement.** Follow `UnixSystem.cs` (Linux `/proc/stat`, `/proc/meminfo`; macOS `host_statistics64` /
  `sysctl`) and the Windows `GetSystemTimes` / `GlobalMemoryStatusEx` path. Use `sysinfo` only where it yields the
  same value as the C# code (read the C# formula; RAM "used" differs between libraries); otherwise the per-OS call.
  Disks: the C# `ReadDisks` filter (drive type, ready, size > 0, the `DiskLabel` naming) on each OS. Uptime:
  `Environment.TickCount64 / 1000` equivalent (time since boot).
- [ ] **Step 3: Run** → PASS; `remote-cross-check.sh` clean. **Commit** `Rust agent: CPU, memory, disk readers`.

### Task 4: CPU temperature — hwmon, LibreHardwareMonitor JSON, Apple SMC

**Files:** Create `unix/apple_smc.rs`; modify `unix/unix_system.rs` (`cpu_temp`), `system_monitor.rs` (`find_cpu_temp`).

**Interfaces:**
- Produces: `unix_system::cpu_temp() -> Option<f64>` (Linux hwmon); `system_monitor::find_cpu_temp(root: &Value) ->
  Option<f64>` (LHM `data.json`); `apple_smc::{is_cpu_key(&str) -> bool, is_gpu_key(&str) -> bool,
  average(impl IntoIterator<Item = Option<f64>>) -> Option<f64>, four_cc(&str) -> u32, key_name(u32) -> String}`
  on every OS, and `#[cfg(target_os = "macos")] AppleSmc::{open() -> Option<AppleSmc>, read(&self) ->
  (Option<f64>, Option<f64>)}`.

- [ ] **Step 1: Port `CpuTempTests` (7), `CpuTempMalformedTests` (2), `AppleSmcTests` (7).** Run → FAIL.
- [ ] **Step 2: Implement** from `UnixSystem.CpuTemp`, `SystemMonitor.FindCpuTemp` (+ `CpuTempNames`, `IsCpuNode`,
  `ReadCelsius`, `CoreSensor`), `AppleSmc.cs` (IOKit `AppleSMC` service, `IOConnectCallStructMethod` selector and
  struct layout exactly as the C# `DllImport`s; `Drop` closes the connection).
- [ ] **Step 3: Run** → PASS; cross-check clean. **Commit** `Rust agent: CPU temperature sources`.

### Task 5: Network adapters, byte counters, rates and LAN addresses

**Files:** Create `net_adapters.rs` (per-OS enumeration), `lan.rs` (phase-2 part); modify `system_monitor.rs`.

**Interfaces:**
- Produces: `net_adapters::{AdapterKind (Ethernet, Wireless, Loopback, Tunnel, Other — the
  NetworkInterfaceType values the C# code tests), NetAdapter { name, description, kind, up: bool, rx: i64, tx: i64,
  ipv4: Vec<Ipv4Addr>, has_ipv4_gateway: bool }, all() -> Vec<NetAdapter>}`;
  `system_monitor::{counted(name, description, kind, chosen: &[String]) -> bool, adapters(chosen: &[String]) ->
  Vec<Value>, net_bytes(chosen) -> HashMap<String, (i64, i64)>, network_rates(before, after, secs: f64) -> (f64,
  f64)}`; `lan::{addresses() -> Vec<String>, should_skip_adapter(desc: &str, has_ipv4_gateway: bool) -> bool}`.

- [ ] **Step 1: Port `NetworkAdapterTests` (3), `NetworkRatesTests` (2),
  `LanTests.VirtualAdaptersAreSkippedUnlessTheyCarryTheDefaultRoute`.** Run → FAIL.
- [ ] **Step 2: Implement.** `all()` gives what .NET's `NetworkInterface.GetAllNetworkInterfaces()` gives the C# code:
  Linux `/sys/class/net` + `/proc/net/dev` + routes, macOS `getifaddrs` (`AF_LINK` `if_data` counters), Windows
  `GetIfTable2`/`GetAdaptersAddresses`. `adapters()` returns the objects `SystemMonitor.Adapters` returns, same keys
  and order. `lan::addresses()` follows `Lan.Addresses`.
- [ ] **Step 3: Run** → PASS; cross-check clean. **Commit** `Rust agent: network adapters and rates`.

### Task 6: `drm_usage.rs` — `DrmUsage`

**Files:** Create `drm_usage.rs`.

**Interfaces:**
- Produces: the public and `internal` API of `DrmUsage.cs`, same names in snake case (what `Gpus.ReadSysfs` calls:
  a per-device usage sampler over `fdinfo` under a `/proc` root).

- [ ] **Step 1: Port `DrmUsageTests` (23).** Run → FAIL.
- [ ] **Step 2: Implement** from `DrmUsage.cs`, keeping the `/proc` root injectable as in C#.
- [ ] **Step 3: Run** → PASS. **Commit** `Rust agent: DRM usage`.

### Task 7: `gpus.rs` — sources, composition and the Apple GPU

**Files:** Create `gpus.rs`; modify `system_monitor.rs` (`GpuInfo` lives in `gpus.rs`, `parse_apple_gpu` in
`system_monitor.rs` as in C#).

**Interfaces:**
- Produces: `gpus::GpuInfo` (all 21 fields of the C# record in its order, `Option<f64>` for `double?`, serde camelCase,
  `Clone`); `gpus::{compose(sources: &[Option<&[GpuInfo]>]) -> Vec<GpuInfo>, merge(a, b) -> GpuInfo, key(&str) ->
  String, vendor_of_name(&str) -> Option<String>, vendor_of_pci(u32) -> Option<String>, looks_integrated(&str) ->
  bool, parse_nvidia_line(line, names) -> Option<(i32, GpuInfo)>, parse_nvidia_names(&str) -> HashMap<i32, (String,
  Option<String>)>, parse_lhm(&Value) -> Vec<GpuInfo>, parse_lhm_value(&str, Option<&str>) -> Option<(&'static str,
  f64)>, read_sysfs(drm_root: &Path, pci_ids: Option<&Path>, proc_root: &Path, usage: &DrmUsage) -> Vec<GpuInfo>,
  pci_address(&str) -> Option<String>, dpm_current(Option<&str>) -> Option<f64>, pci_name(u32, u32, Option<&Path>)
  -> Option<String>}`; `system_monitor::parse_apple_gpu(ioreg: &str, total_memory_bytes: f64) -> Option<GpuInfo>`.

- [ ] **Step 1: Port `GpuSourceTests` (19), `GpuIntegratedMergeTests` (1), `AppleGpuTests` (4).** Run → FAIL.
- [ ] **Step 2: Implement** from `Gpus.cs` and `SystemMonitor.ParseAppleGpu` (regexes with the `regex` crate, same
  patterns; the PCI name cache as in C#).
- [ ] **Step 3: Run** → PASS. **Commit** `Rust agent: GPU sources`.

### Task 8: `windows_gpu.rs` — `WindowsGpu`

**Files:** Create `windows_gpu.rs` (`#[cfg(windows)]`).

**Interfaces:**
- Produces: `WindowsGpu::{new() -> WindowsGpu, sample(&mut self) -> Result<Vec<GpuInfo>, String>}` (Windows only);
  `windows_gpu::{parse_instance(&str) -> Option<(String, String)>, luid(…) -> String}` compiled on every OS (pure);
  `Drop` closes the PDH query and adapter handles.

- [ ] **Step 1: Write `windows_gpu::tests::instance_names_parse_like_csharp`** for the pure helpers (the `Instance()`
  regex and `Luid`), with instance names of the form `WindowsGpu.cs` matches. The helpers sit outside the
  `#[cfg(windows)]` part so the test runs on the server. Run → FAIL.
- [ ] **Step 2: Implement** from `WindowsGpu.cs` with the `windows` crate (D3DKMT adapter enumeration, PDH GPU
  engine/memory counters, the driver-version registry read).
- [ ] **Step 3: Run** → PASS; cross-check (Windows target) clean. **Commit** `Rust agent: Windows GPU counters`.

### Task 9: `SystemMonitor` — the sampling loop and `pc` in the state

**Files:** Modify `system_monitor.rs`, `state.rs`, `server/mod.rs`, `phonebox-agent/src/main.rs`.

**Interfaces:**
- Consumes: Tasks 3–8.
- Produces: `system_monitor::{PcSnapshot (C# field order: cpu, cpuTemp, cpuName, ramUsedGb, ramTotalGb, gpu, netDownBps,
  netUpBps, disks, uptimeSec, gpus), SystemMonitor::spawn(cfg: Arc<ConfigStore>, stop: Stop) -> Arc<SystemMonitor>,
  SystemMonitor::snapshot(&self) -> Option<PcSnapshot>}`; `impl StateSource for SystemMonitor` (JSON of the snapshot,
  `null` before the first tick); `StateSources::with_monitors(...)` replacing `pc` and `ping` (ping from Task 10).

- [ ] **Step 1: Write the tests first:** `a_machine_without_gpu_or_temperature_sources_still_publishes_pc` (Review Focus
  1: the loop built with empty GPU/temperature sources publishes a snapshot within 2 s of paused time and logs the
  missing source once), `pc_json_has_the_csharp_keys_in_order` (key list and nulls). Run → FAIL.
- [ ] **Step 2: Implement** `ExecuteAsync`, `GpuLoop`, `NvidiaLoop` (`nvidia-smi` names query, then the streaming
  `-lms 1000` query, restart after 10 s), `OsGpuLoop` (2 s with data, 60 s without, first failure logged once),
  `AppleGpuLoop` (5 s timeout, 2 s period), `CpuTempLoop` (Linux hwmon / Windows LHM at
  `http://127.0.0.1:8085/data.json` with 600 ms timeout, also feeding `parse_lhm`), `AppleTempLoop` (2 s, 30 s back-off).
  Rounding as in C# (`Math.Round` is banker's rounding: use round-half-even). Every loop ends on `Stop`.
- [ ] **Step 3: Run** → PASS; the parity corpus `phase1.json` still passes (`pc` stays ignored as a live value).
  **Commit** `Rust agent: SystemMonitor`.

### Task 10: `PingMonitor` and `ping` in the state

**Files:** Create `ping_monitor.rs`; modify `state.rs`, `main.rs`.

**Interfaces:**
- Produces: `ping_monitor::{PingSnapshot { host, ms: Option<f64>, avg_ms: Option<f64>, loss_pct: f64 },
  PingMonitor::spawn(cfg, stop) -> Arc<PingMonitor>, PingMonitor::snapshot(&self) -> Option<PingSnapshot>,
  PingMonitor::sample(&self, ms: Option<f64>, host: &str)}`; `impl StateSource for PingMonitor`.

- [ ] **Step 1: Port `PingMonitorTests` (4)** and write `an_unresolvable_host_counts_as_loss` (Review Focus 4). Run →
  FAIL.
- [ ] **Step 2: Implement** from `PingMonitor.cs`. One echo per period with .NET `Ping.SendPingAsync` semantics:
  unprivileged ICMP datagram socket where the OS allows it (macOS; Linux with `ping_group_range`), else the system
  `ping` binary as .NET does on Linux, `IcmpSendEcho2` on Windows. Window, average and loss exactly as `Sample`.
- [ ] **Step 3: Run** → PASS; cross-check clean. **Commit** `Rust agent: PingMonitor`.

### Task 11: `listening_ports.rs` — `ListeningPorts`

**Files:** Create `listening_ports.rs`.

**Interfaces:**
- Produces: `listening_ports::{Listener { address: IpAddr, port: u16 }, DEFAULT_MAX, for_process(pid: u32, max: usize)
  -> Vec<Listener>, reachable_on_loopback(IpAddr) -> bool, loopback_for(IpAddr) -> IpAddr, url_host(IpAddr) -> String,
  parse_owner_pid_table(&[u8], ipv6: bool, pid: u32) -> Vec<Listener>, parse_proc_net_tcp(&str, inodes:
  &HashSet<String>) -> Vec<Listener>, parse_lsof_names(&str) -> Vec<Listener>}` (used by plan 5's Antigravity).

- [ ] **Step 1: Port `ListeningPortsTests` (10).** Run → FAIL.
- [ ] **Step 2: Implement** from `ListeningPorts.cs` (Windows `GetExtendedTcpTable` owner-pid tables, Linux `/proc/<pid>/fd`
  socket inodes + `/proc/net/tcp{,6}`, macOS `lsof -nP -a -p <pid> -iTCP -sTCP:LISTEN -Fn` as the C# code runs it).
- [ ] **Step 3: Run** → PASS; cross-check clean. **Commit** `Rust agent: ListeningPorts`.

### Task 12: `ProcessMonitor` (+ `unix/unix_proc.rs`) and the `/api/procs*` routes

**Files:** Create `unix/unix_proc.rs`, `process_monitor.rs`, `server/routes_system.rs`; modify `server/mod.rs`.

**Interfaces:**
- Produces: `process_monitor::{ProcGroup, ProcInstance, ProcDetail, KillResult (C# records, camelCase),
  AgentSessions trait { fn sessions(&self) -> Vec<AgentSessionRef> }, AgentSessionRef { pid: Option<u32>, title:
  String, updated: i64 } (the fields `ProcessMonitor` reads from `AgentSession`), NoSessions,
  ProcessMonitor::spawn(sessions: Arc<dyn AgentSessions>, stop) -> Arc<ProcessMonitor>, snapshot() -> Vec<ProcGroup>,
  detail(&str) -> ProcDetail, kill(&str) -> KillResult, kill_pid(u32) -> KillResult, descendants(u32) -> Vec<u32>,
  icon(&str) -> Option<Vec<u8>>, shorten_args(Option<&str>) -> Option<String>}`; routes `GET /api/procs?top=`,
  `GET /api/procs/icon?name=`, `GET /api/procs/detail?name=`, `POST /api/procs/kill?name=|pid=` with the C# status
  codes, content types and config checks (`Program.cs:811-860`).

- [ ] **Step 1: Port `MonitorAuditTests.ProcessDetailToleratesDuplicateSessionPidsAndLocksAgentProcess` (with a fake
  `AgentSessions`) and `…ColdProcessIconDoesNotCacheNullAndShortenArgsHandlesTabSeparators`;** write
  `a_process_that_exits_while_listing_is_left_out` (Review Focus 2: the grouping takes per-process reads that may come
  back empty and drops those rows) and `kill_refuses_the_agent_and_protected_processes` (Review Focus 3), and route
  tests for the four routes' error cases (missing name, unknown name, bad pid). Run → FAIL.
- [ ] **Step 2: Implement** from `ProcessMonitor.cs` and `Unix/UnixProc.cs`: `sysinfo` for the process table and CPU
  deltas, the C# grouping, titles (`.desktop` names on Linux, bundle names on macOS, file descriptions on Windows),
  icons (`.desktop` icons, macOS `sips` conversion as the C# code does, Windows associated icon to PNG), the
  `Protected` sets, `Descendants`, kill order, and the `_icons` cache that never stores a miss.
- [ ] **Step 3: Run** → PASS; cross-check clean. **Commit** `Rust agent: ProcessMonitor and process routes`.

### Task 13: `BatteryMonitor` (+ Razer) and `/api/batteries`

**Files:** Create `battery_monitor.rs`, `battery_monitor/razer.rs`; modify `server/routes_system.rs`.

**Interfaces:**
- Produces: `battery_monitor::{DeviceBattery { name, level: i32, kind, charging: Option<bool> },
  BatteryMonitor::spawn(stop) -> Arc<BatteryMonitor>, snapshot() -> Vec<DeviceBattery>, refresh_interval_ms(mac_os:
  bool) -> u64, kind_of(&str) -> String, include_windows_battery_device(bool, &[u8]) -> bool,
  add_mac_bluetooth_batteries(&Value, &mut Vec<DeviceBattery>)}`; `razer::{REPORT_LENGTH = 91, build_report,
  parse_battery, parse_charging, build_pa_remote_frame, build_pa_query_frame, parse_pa_reply,
  build_barracuda_remote_frame, build_barracuda_query_frame, parse_barracuda_reply, device_info, kind_from_pid,
  merge_into_bluetooth}`; route `GET /api/batteries` → `{"devices":[…]}`.

- [ ] **Step 1: Port `BatteryTests` (13), `BatteryParsingTests` (1).** Run → FAIL.
- [ ] **Step 2: Implement** from `BatteryMonitor.cs` and `BatteryMonitor.Razer.cs`: Linux `/sys/class/power_supply`,
  macOS `system_profiler SPBluetoothDataType -json` (and `ioreg` as in C#), Windows SetupAPI Bluetooth properties,
  Razer HID feature reports and the PA/Barracuda frames through `hidapi` (Windows only, as in C#), the Synapse check,
  sorting and merging.
- [ ] **Step 3: Run** → PASS; cross-check clean. **Commit** `Rust agent: BatteryMonitor`.

### Task 14: Wiring, parity corpus and the MacBook check (exit)

**Files:** Modify `phonebox-agent/src/main.rs`, `server/mod.rs` (`/api/network/adapters` too, if not yet routed);
create `tools/parity/corpus/phase2.json`, `agent-rs/scripts/mac-state-compare.sh`, `agent-rs/scripts/compare-pc-ping.py`;
modify `.github/workflows/rust.yml` (parity job runs `phase2.json` too).

- [ ] **Step 1: Wire** the four monitors into `main.rs` and `AppState`; they start with the server and end on `Stop`.
- [ ] **Step 2: Parity corpus `phase2.json`:** `/api/network/adapters` (counters ignored), `/api/batteries`,
  `/api/procs` (status and content type; body ignored), `procs-top-0`, `procs-top--1`, `procs-top-1000000`,
  `procs-top-x` (Review Focus 5), `/api/procs/detail?name=phonebox-no-such-proc`, `/api/procs/icon?name=…` (missing),
  `/api/procs/kill` with no arguments, with `pid=4294967295` and with a missing name, and `/api/state` with `pc`/`ping`
  checked for keys only. Run `agent-rs/scripts/remote-parity.sh tools/parity/corpus/phase2.json`. Expected: all pass.
- [ ] **Step 3: MacBook check (laptop exception).** `mac-state-compare.sh` builds the release binary, runs the Rust agent
  on a spare port with a copy of the owner's config (as `mac-phone-check.sh` does), samples `/api/state` from it and
  from the installed C# agent ten times a second apart, and `compare-pc-ping.py` checks: identical key sets; equal
  `cpuName`, `ramTotalGb`, disk names and totals, GPU names/vendors, `ping.host`; `uptimeSec` within 5 s; `cpu`, `ramUsedGb`,
  `cpuTemp`, GPU utilisation and `ping.ms` within tolerances stated in the script (live values). Stops the Rust agent
  by PID. Expected: `pc and ping match`.
- [ ] **Step 4:** `port-status.py --check 2` → no unported phase-2 test. Full suite, `remote-cross-check.sh`, both
  parity corpora green. **Commit** `Rust agent: plan 2 exit: monitors wired, phase-2 parity, MacBook state check`.
