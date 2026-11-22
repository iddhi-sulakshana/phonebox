# Rust agent: roadmap of plans

**Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md` (approved 2026-11-20)

The design is a full port in eight phases. One plan per phase: each produces working, testable software on its own
and is written when its phase starts, against the code as it stands then. A phase is done when its exit check passes
and `agent-rs/PORTING.md` shows no unported C# test in its scope.

| Plan | Phase | Scope (C# sources at `bb72345`, 0.9.23; plan 1 was written at `43800f9`, 0.9.22) | Exit check |
|---|---|---|---|
| 1 | Foundation and core | Fork, toolchains, CI, parity harness; `AppPaths`, `AgentEndpoint`, `FileLogger`, `Config`, `BrowserGuard`, `PhoneRegistry`, `WebViewDoctor`, `RequestTimings`, `PushStore`, `StateSignal`, `StateFrameCache`, `AdbReverse`, `UsbDoctor`; routes `/`, `/favicon.ico`, `/panel/*`, `/api/settings`, `/api/config`, `/api/info`, `/api/state`, `/api/stream`, `/api/latency`, `/api/restart` | Owner's phone shows the panel from the Rust agent over USB; phase-1 corpus passes on Linux, macOS, Windows |
| 2 | System | `SystemMonitor`, `Unix/UnixSystem`, `Unix/AppleSmc`, `Gpus`, `WindowsGpu`, `PingMonitor`, `ProcessMonitor`, `Unix/UnixProc`, `BatteryMonitor` (+ Razer), `ListeningPorts`, `Lan` adapters, `DrmUsage` (moved from plan 6: `Gpus.ReadSysfs` needs it); routes `/api/procs*`, `/api/batteries`, `/api/network/adapters` | `pc`, `ping` in `/api/state` match C# on the MacBook hardware run |
| 3 | Desk controls | `MediaMonitor` (Windows + `Unix/`), `MediaArtworkCache`, `AudioControl` (+ `Unix/`), `SpectrumAnalyzer`, `Shortcuts`, `ShortcutIcons`, `ShortcutIconUploads`, `AppCatalog`, `KeySender`, `TrackpadHandler`, `Dictation`, `IconText` | Media, volume, shortcuts and apps work from the phone on the MacBook |
| 4 | Wi-Fi | `Lan` (TLS server, `lan-cert.pfx` reuse, request authorization), `Pairing`, QR, `/api/pair*`, `/api/lan` | Phone switches USB ⇄ Wi-Fi with an existing pairing, no re-pair |
| 5 | AI and dev | `SessionMonitor`, `ClaudeTranscript`, `ClaudeHook`, `ApprovalStore`, `QuotaMonitor` (+ Antigravity), `CodexBar`, `WorkBuddy`, `GitHubMonitor`, `DockerMonitor`, `CommandRunner`, `ToolProcess` | Sessions, quotas and a Claude Code permission prompt round-trip on the MacBook |
| 6 | Integrations | `HomeAssistant`, `ObsMonitor`, `DiscordMonitor`, `FeedMonitor`, `WidgetFeedProxy`, `CalendarMonitor`, `PhotoFrame`, `CameraFeeds`, `ControlDeck*`, `SpotifyControl`, `RuleEngine`, `ChargeGuard`, `PrivacyMonitor`, `SessionLockMonitor`, `AppConsole`, `PanelWidgets`, `StackWidgets`, `NoteEdit`, `PushStore` webhooks | Full route corpus passes on all platforms |
| 7 | Lifecycle | `UpdateService` (incl. phone app install), `AppRestart`, `Tray`, `JobObject`, start-at-login; `release.yml`, `build.yml`, setup scripts, scoop/winget, fork signing (APK keystore, macOS certificate) | A `1.0.0-alpha.N` pre-release installs with `setup.sh` on the MacBook and updates itself to the next alpha |
| 8 | Sign-off and switch | Browser suite against the live agent, resource comparison, remaining parity gaps, release 1.0.0, removal of `agent/` | Sign-off criteria in the spec's "Switch-over" section |
