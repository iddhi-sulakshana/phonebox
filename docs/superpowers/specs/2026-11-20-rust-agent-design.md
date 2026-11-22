# Rust desktop agent: design

Date: 2026-11-20
Status: approved by the owner, 2026-11-20

## Intent

Rewrite the PhoneBox desktop agent, currently C# on .NET 10 (`agent/PhoneBox.Agent`), in Rust, in a fork of
`ersinkoc/phonebox`. The owner chose a **full port**: all three desktop platforms and every feature, with the test
suite ported, finished and shown equal to the C# agent **before** anyone switches to it.

Why (as understood, not stated in detail by the owner): they want a Rust codebase of their own for the agent. A
secondary benefit is resource use. Measured on the owner's MacBook Air (Apple silicon, macOS 26) with the 0.9.22
release running: about 10% CPU continuously, about 270 MB memory, 40 threads, 109 MB installed on disk.

## Goals

1. **Drop-in replacement.** An unmodified Android app, an unmodified panel (`android/app/src/main/assets/www`) and
   the unmodified settings page (`agent/PhoneBox.Agent/settings.html`) work against the Rust agent exactly as they do
   against the C# one. Concretely the Rust agent keeps:
   - the HTTP API: every route in `Program.cs` (about 105, plus `/api/hooks/claude/permission`), with the same methods,
     paths, status codes, JSON field names and casing, headers (`X-Config-Version`, CSP, `nosniff`) and content types;
     the server-sent-events stream `/api/stream`; the WebSocket endpoints;
   - ports 8765 (local/USB, `127.0.0.1` only) and 8766 (Wi-Fi/Tailscale HTTPS);
   - the files and their formats: `config.json` (+ `.bak`, the version/conflict semantics), `data.json`, `icons/`,
     `lan-cert.pfx`, `agent.log`, in the same folders (`AppPaths.cs`), with owner-only modes on macOS/Linux;
   - the command line: `--config <path>`, `--port <n>`, `--print-install-dir`, `--restart`;
   - the install layout and service names the setup scripts rely on: `<LocalDir>/PhoneBox/app/PhoneBoxAgent(.exe)`,
     launchd label `net.oxog.phonebox.agent`, systemd user unit `phonebox.service`, Windows startup shortcut, and the
     launchd/systemd restart contract (exit code 75 when service-managed);
   - release artifacts: `PhoneBoxAgent-<ver>-<rid>.zip` for `win-x64`, `linux-x64`, `linux-arm64`, `osx-x64`,
     `osx-arm64`, plus `SHA256SUMS-<ver>.txt`, as checked by `.github/workflows/artifact-contract.yml`.
2. **Feature parity with a fixed baseline**: the fork's `main` at `bb72345`, which is upstream release 0.9.23 (`26a0d32`)
   plus the fork's battery-care fix. Every feature at that commit, on every platform it supports there. (Until
   2026-11-20 the baseline was upstream `43800f9`, just after release 0.9.22; see decision 1.)
3. **Tests ported**: every behavior covered by `agent/PhoneBox.Agent.Tests` (about 18,300 lines, xUnit) has an
   equivalent Rust test.
4. **Measurably lighter** than the C# agent on the same machine with the same config (CPU, memory, threads, disk).

## Non-goals

- Changes to the Android app, the panel JavaScript, the settings page, `widgets.json` or `templates.json`.
- New features, or redesigns of existing ones, during the port.
- Platforms upstream does not build for (Windows ARM, musl Linux).

## Anti-goals (failure even if it "works")

- The phone or the settings page needs a change to work with the Rust agent.
- A user switching from the C# agent loses settings, phone pairings (Wi-Fi certificate and key), shortcut icons or
  pushed values.
- The security or privacy model gets weaker: settings, pairing-start and other local-only routes reachable over Wi-Fi;
  Wi-Fi without the pinned certificate and key; secrets in world-readable files; sampling data no visible widget needs
  (see `CONTRIBUTING.md` and `PRIVACY.md`); any telemetry.
- Shipping to users before parity is demonstrated.

## Constraints

- **The C# agent is the specification.** Where docs and code disagree, the code at `bb72345` and its tests win.
- **Where tests and servers run.** The owner's rule: no tests or servers on the laptop. Linux test runs (unit tests,
  the parity harness, the C# reference agent) go to the `hetzner-iddhi` server (x86_64, 20 cores, .NET 10 installed,
  Rust **not** installed yet). macOS- and Windows-specific tests run on GitHub Actions runners (macOS arm64, macOS
  x64, Windows x64, Ubuntu x64 and arm64). The laptop is for editing and building only; the real-device check (below)
  is the one exception, as it needs the phone on USB.
- **Toolchain.** Rust stable via rustup; on the laptop it already lives under `/storage` (`CARGO_HOME`,
  `RUSTUP_HOME`). Release binaries are built by CI on native runners per target.
- **Embedded resources** stay single-sourced: the Rust binary embeds the same files the C# build embeds
  (`settings.html`, `widgets.json`, `templates.json`, `config.default*.json`, `phonebox.ico`, and the panel `www/`
  tree from `android/app/src/main/assets/www`) from their current paths, so the C# and Rust agents serve identical
  bytes during the port.
- **macOS permissions** (Accessibility for key presses, Automation for Music/Spotify, Local Network) are tied to the
  binary's designated requirement: the identifier `net.oxog.phonebox.agent` plus the signing certificate
  (`.github/actions/sign-macos-agent`). *My call:* the fork signs with its own self-signed certificate the same way,
  so users re-grant these permissions once after switching and keep them across later updates. An ad hoc signature
  would make them re-grant after every update.
- **Config files from newer versions.** The C# `ConfigStore` drops properties it does not know. *My call, a
  deliberate departure from the C# behavior:* the Rust agent keeps unknown fields (at every level of `config.json`)
  when it saves, so a config last written by a newer C# agent loses nothing.
- **Wi-Fi adb without a reverse rule.** Android 8 refuses `adb reverse` over Wi-Fi adb ("more than one
  device/emulator") while its USB transport is still registered, and the C# agent then ends every 4-second round with
  "adb error" (keep-awake and the "phone connected" state never run). *Owner's request, a deliberate departure from
  the C# behavior:* when a phone on Wi-Fi adb (`host:port` or an mDNS `._tcp` serial) refuses the rule, the Rust agent
  logs it once, does not ask again until that phone reconnects, and finishes the round; the panel uses its own Wi-Fi
  link. Phones that accept the rule over Wi-Fi (Android 11+) keep it, and a refusal over USB still ends the round.

## Approaches considered

| | Approach | Trade-off |
|---|---|---|
| A | Mac first, ported in order of what the owner uses; unported routes answer "unavailable" | Usable in days; other platforms later or never; some widgets dark for a while |
| B | Rust in front on 8765/8766, forwarding unported routes to the C# agent behind it | Everything works from day one, but both agents run during the move (more resources), and `/api/stream` merges events from every monitor into one feed, which cannot be split across two processes cleanly |
| **C** | **Full port, all platforms, tests ported, parity shown before switching** | **Weeks before anything usable; closest match to upstream; one clean switch** |

**Chosen: C**, by the owner.

## Design

### Repository

- A public GitHub fork of `ersinkoc/phonebox`: `iddhi-sulakshana/phonebox`. The Rust agent lives in a new top-level `agent-rs/` directory beside
  `agent/`. The C# agent stays, untouched, as the reference and parity oracle until the switch, then is removed.
- *My call:* one Cargo workspace in `agent-rs/` with a library crate holding all logic (so tests and the parity
  harness can drive it in-process) and a thin binary crate producing `PhoneBoxAgent`. Module layout mirrors the C#
  file names (`config.rs`, `adb_reverse.rs`, `quota_monitor.rs`, `unix/media_monitor.rs`, ...) so each port can be
  reviewed side by side with its source.

### Stack (my calls)

- Async runtime and HTTP: `tokio`, `axum` (routes, SSE, WebSockets), `tower-http`.
- Wi-Fi TLS: `rustls` / `tokio-rustls`; certificate generation with `rcgen`. **The existing `lan-cert.pfx` must be
  read** (PKCS#12, no password, written by `Lan.cs`) so the fingerprint, and with it every phone pairing, survives
  the switch; a new certificate is generated only when the file is missing, keeping the same extensions and validity.
  The fingerprint stays SHA-256 over the DER, lower-case hex.
- JSON and config: `serde` / `serde_json`, matching `ConfigStore`'s read options (case-insensitive property names,
  comments/trailing commas if allowed there), write options, defaults, legacy-option migrations and validation.
- Outbound HTTP: `reqwest` with rustls (GitHub releases, quota APIs, feeds, Home Assistant, the weather/feed proxy).
- System data: `sysinfo` as the shared base for CPU, memory, disks, network and processes, with per-OS code where
  the C# agent goes further: Windows via the `windows` crate (PDH counters, WMI, WASAPI audio sessions, GPU counters,
  key sending, job objects), macOS via `objc2` / `core-foundation` / IOKit (IORegistry GPU, SMC temperatures,
  CoreAudio volume), Linux via `/proc`, `/sys` and the external tools the C# agent already shells out to
  (`playerctl`, `wpctl`/`pactl`, `nvidia-smi`, `xprintidle`).
- External tools stay external, invoked the same way: `adb`, `nowplaying-cli`, `osascript`, `sips`, `pmset`,
  `playerctl`, `nvidia-smi`, `docker`, etc.
- Windows tray icon: `tray-icon`. Razer battery: `hidapi`. QR codes: `qrcode`. Logging: `tracing`, written to
  `agent.log` in the same line format (`yyyy-MM-dd HH:mm:ss level Category: message`) so existing troubleshooting
  advice still applies.

### Porting order

Each phase ends with its routes passing the parity harness on all platforms it touches.

1. **Core:** paths, CLI, config load/save/migrate/validate, `/api/settings`, `/api/config`, `/api/info`, `/api/state`,
   `/api/stream`, serving `settings.html` and `/panel/*`, adb discovery, `adb reverse`, wake/open-panel. Exit check:
   the real phone shows the panel over USB with clock and placeholders.
2. **System:** CPU, GPU, RAM, disks, network, ping, processes (list, detail, icons, kill), batteries, listening ports.
3. **Desk controls:** media, volume, spectrum, shortcuts (run, test, icons, uploads), apps catalogue, key sending,
   macros, dictation.
4. **Wi-Fi:** LAN TLS server, `/api/pair*`, pairing codes, QR, address listing, Tailscale.
5. **AI and dev:** Claude Code sessions, transcripts and titles, quotas (Claude, Codex, Antigravity, CodexBar), the
   Claude permission hook and approvals, GitHub monitor, Docker.
6. **Integrations:** Home Assistant, OBS, Discord, feeds, calendar (ICS), photos, cameras/HLS, radio directory, rule
   engine, webhooks and push values, Control Deck, charge guard, privacy monitor, console.
7. **Lifecycle:** self-update (download, checksum, install-dir swap), restart semantics, start-at-login, tray, the
   release workflow and the setup scripts building and shipping the Rust binary.
8. **Sign-off and switch** (below).

### Verification

1. **Ported unit tests.** Each `*Tests.cs` file gets a Rust counterpart with the same cases. A coverage table in
   `agent-rs/PORTING.md` maps every C# test to its Rust test (or records why it does not apply).
2. **Parity harness (black-box).** A runner starts the C# agent and the Rust agent side by side with isolated
   `--config` / `--port` and the same fixture configs, sends the same request corpus (every route, including error
   cases and conflict paths), and diffs status codes, headers and bodies after normalizing live values (times,
   samples, versions). Runs on `hetzner-iddhi` for Linux and on CI runners for macOS and Windows.
3. **Browser tests against the real agent.** *My call:* the existing Playwright suite (`tools/ui-tests`) only drives
   the static demo with made-up data, so it proves nothing about either agent. A new Playwright suite runs against a
   live agent (C# first, to prove the suite, then Rust) and covers: loading and saving settings, the
   `X-Config-Version` conflict path, the layout editor's live preview at `/panel/`, the Wi-Fi pairing dialog and QR
   code, and shortcut icon upload and removal.
4. **Resource comparison.** The same config on the same machine, both agents, idle with the phone connected and with
   all default pages cycling: CPU, memory, threads, disk.
5. **Real devices.** USB and Wi-Fi with the owner's Huawei RNE-L22 (Android 8, WebView 138) on macOS. No Windows or
   Linux machine with a desktop is available (see the owner's decisions below).

### Switch-over

- Parity is signed off when: every route passes the harness on every platform; the test coverage table has no gaps;
  the new browser suite passes against the Rust agent; the macOS real-device and hardware checks pass; the Rust agent
  is lighter on every measured axis on the owner's MacBook.
- The switch is release 1.0.0 from the fork: the fork's setup scripts and self-update install the Rust
  `PhoneBoxAgent` into the same folder, and settings, pairings, icons and pushed values carry over untouched. The
  phone side follows the switch procedure in "Consequences of those decisions".
- After the switch, `agent/` (C#) and its tests are removed from the fork.

## Left to the builder

Crate boundaries beyond the library/binary split; the exact crate versions; how monitors are scheduled internally,
as long as sampling stays on-demand as in the C# agent; the parity harness's language and normalization rules; the
order of work inside each phase.

## Decisions by the owner (2026-11-20)

1. **Upstream is frozen at a fixed baseline.** First 0.9.22 (`43800f9`); on 2026-11-20, after plan 1, the owner moved
   it to 0.9.23 with the fork's battery-care fix (`bb72345`), rebased `rust-agent` onto it and had the parity harness
   rerun against it. The owner's installed C# agent has auto-update turned off (`updates.auto = false`), so the
   machine, the phone and the parity oracle stay on a known version for the port. Upstream features released later
   can be ported after the switch, one at a time, as separate work, or by moving the baseline again the same way.
2. **The fork is public, with its own releases:** `iddhi-sulakshana/phonebox` on GitHub. Its releases are built by
   GitHub Actions on the public repo (macOS, Windows and Linux runners).
3. **The fork builds and signs its own APK** with a new key belonging to the owner. The app's code is unchanged;
   only the signature differs. Because Android refuses an update signed with a different key, each phone uninstalls
   PhoneBox once at the switch and gets the fork's APK; after that, updates install normally.
4. **Where checks run:**
   - Rust is installed on `hetzner-iddhi` (rustup under `/root`) for Linux unit tests and the C#/Rust parity harness.
   - These checks may run on the owner's MacBook, and only these: the macOS parity runs that need real hardware
     (SMC temperatures, GPU, CoreAudio, media sessions, Accessibility), the resource comparison, and the real-phone
     check. Each runs only as long as needed, beside the installed agent on separate ports (`--port`, isolated
     `--config`, `deviceSerial` set so it never takes over the phone), and is stopped by PID afterwards.
   - The owner has **no Windows PC or Linux desktop** for real-device checks. Windows and Linux sign-off rests on
     the ported tests and the parity harness on CI runners and `hetzner-iddhi`; hardware-dependent behavior there
     (tray, per-app audio, GPU counters, Razer batteries, X11 idle time) is verified only as far as those
     environments allow. This is an accepted risk, listed in the release notes of the switch release.

## Consequences of those decisions (my calls)

- **Update channel.** The Rust agent's default `updates.repo` becomes `iddhi-sulakshana/phonebox`, and on first start
  it rewrites a saved `updates.repo` of `ersinkoc/phonebox` to the fork, so upstream's next C# release can never
  replace the Rust agent. The fork's `setup.sh` / `setup.ps1` (`repo=`), `scoop` and `winget` manifests point at the
  fork. The pairing dialog's app download link (`/api/pair/start`, `Program.cs:276`) uses the configured repo instead
  of the hard-coded upstream one. These are deliberate departures from the C# behavior. `settings.html` keeps its
  `ersinkoc/phonebox` display fallback, which only shows when a config has no `updates.repo`, and the Rust agent
  always writes one.
- **Version numbers.** The switch release is **1.0.0** for both the agent and the APK (`release.yml` requires them
  to match), above every 0.9.x so the update check (`System.Version` ordering) moves C# installs forward, and clear
  of upstream's numbering. Pre-switch builds are `1.0.0-alpha.N` and are never published as the "latest" release.
- **Switch procedure** for an existing install: run the fork's setup script (agent, settings and pairings carry
  over), uninstall PhoneBox on the phone once, then let the agent install the fork's APK over USB, or install it by
  hand for Wi-Fi-only phones and pair again. macOS users re-grant Accessibility and Automation once.
- **Signing material** the owner creates before phase 7: an Android keystore (kept out of the repo, provided to CI
  as secrets) and a self-signed macOS code-signing certificate for `net.oxog.phonebox.agent`. Windows binaries stay
  unsigned, as upstream's are today.
