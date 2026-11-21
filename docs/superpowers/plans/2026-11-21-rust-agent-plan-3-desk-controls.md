# Rust agent, plan 3: desk controls — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port media, volume, the spectrum, shortcuts (run, test, toggles, icons, uploads), the apps catalogue, key
and mouse sending, the trackpad and dictation, so the phone's desk widgets get the same answers from the Rust agent as
from the C# agent: state parts `media`, `audio`, `toggles`, the `spectrum` stream event, and the routes listed below.

**Architecture:** One Rust module per C# file, as in plans 1–2 (`Unix/MediaMonitor.cs` → `unix/media_monitor.rs`,
`Unix/AudioControl.cs` → `unix/audio_control.rs`, `Unix/MacAudioProbe.cs` → `unix/mac_audio_probe.rs`; the Windows
halves live beside them as `cfg(windows)` modules of the same type). Decisions and parsers are pure functions tested
on every OS; OS calls are `cfg`-gated and compile-checked for Windows and macOS on the server. Classes from later plans
that these call are reached through small traits with a do-nothing implementation until their plan ports them:
`MediaDeck` (ControlDeck's radio override and media focus, plan 6) and Dictation's focused-app probe (AppConsole,
plan 6). Two small helpers from plan-6 files are ported here because nothing works without them:
`PanelWidgets.Find` and `WidgetFeedProxy.IsPublicAddress`.

**Tech Stack:** as plan 2, plus axum's `ws` feature (trackpad WebSocket), `reqwest` with rustls (cover art over
HTTPS, with a resolver that only returns public addresses), `sha2` (icon revisions, app ids), `roxmltree` (SVG upload
check), and the `windows` crate's Core Audio, `SendInput`, GSMTC (WinRT `Media_Control`) and WASAPI loopback APIs.

**Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md` · **Roadmap:** `docs/superpowers/plans/2026-11-20-rust-agent-roadmap.md` · **C# scope:** `MediaMonitor.cs`, `Unix/MediaMonitor.cs`, `MediaArtworkCache.cs`, `AudioControl.cs`, `Unix/AudioControl.cs`, `Unix/MacAudioProbe.cs`, `SpectrumAnalyzer.cs`, `Shortcuts.cs`, `ShortcutIcons.cs`, `ShortcutIconUploads.cs`, `AppCatalog.cs`, `KeySender.cs`, `TrackpadHandler.cs`, `Dictation.cs`, `PanelWidgets.Find`, `WidgetFeedProxy.IsPublicAddress`, and their routes and helpers in `Program.cs` (`RunShortcutForPhone`, `HintOrGeneric`, `UploadShortcutIconAsync`, `IconCommit`). `IconText.cs` was ported in plan 1. Survey of the C# behaviour used to write this plan: `/tmp/plan3-survey.md` (not committed; the C# source is the authority).

## Global Constraints

- Everything in plans 1–2's Global Constraints still holds (baseline `bb72345`; tests only on `hetzner-iddhi` or CI;
  laptop exceptions for hardware checks, stopped by PID; no AI co-author trailer; owner's run mode: no per-phase
  review, nothing pushed before the final review, macOS and Windows code checked by `remote-cross-check.sh`).
- JSON as the C# agent writes it (camelCase, declaration order, nulls written, .NET number format); `Math.Round` is
  banker's rounding (`round_ties_even` / `round_dotnet`).
- External tools stay external and are called exactly as the C# code calls them, with its timeouts: `playerctl`,
  `nowplaying-cli`, `osascript`, `sips`, `plutil`, `qlmanage`, `open`, `pmset`, `loginctl`, `xdg-screensaver`, `xset`,
  `wpctl`, `pactl`, `m1ddc`, `xdotool`, `wtype`.
- Query and route binding through `server/query.rs` (plan 2): a present `int?`/`long?` that does not parse is a 400;
  a missing required `string` is a 400; several values join with ",".
- Shortcut errors reach the phone only through `HintOrGeneric` (the Accessibility hint or the generic sentence); the
  raw message goes to the log and to the settings page's test route only.
- The parity corpus never sends keys, moves the mouse, types text, changes the volume, controls media or opens an app
  on the machine it runs on: every state-changing request in it is one both agents refuse.
- Text the phone dictates is never logged.

## Review Focus

1. **A shortcut that fails** (missing program, a macro step that throws, a toggle's off command that fails) gives the
   phone the generic sentence (or the Accessibility hint), never the configured text. Pinned by the ports of
   `ShortcutRuleFollowUpTests.ThePhoneDoesNotGetTheConfiguredTextOfAFailedShortcut` and
   `…ThePhoneStillGetsTheAgentsOwnAccessibilityHint` (Task 4).
2. **A machine with no audio tools or devices** (the server, a CI runner): `audio` is `null`, `/api/volume` answers
   400 within the tool timeouts, and nothing logs every second. Pinned by
   `unix::audio_control::tests::a_machine_without_audio_tools_answers_without_hanging` (Task 3).
3. **Cover art from a URL the player hands over** (a LAN or loopback host, a huge body, a server that stalls) is
   refused without connecting, cut at 5 MiB and given up after 5 s. Pinned by the `MacMediaTests` ports (Task 10).
4. **A flood of trackpad frames** (malformed JSON, binary frames, NaN or huge deltas, a socket closed mid-drag) is
   clamped or ignored, releases a held button, and never panics. Pinned by
   `trackpad_handler::tests::a_flood_of_malformed_frames_is_ignored_and_a_held_button_is_released` (Task 7).
5. **A shortcut icon upload racing a settings save** leaves no orphan file and answers 409. Pinned by the port of
   `PairingTests.ShortcutIconUploadRejectsConcurrentSettingsSaveAcrossBodyReadWithoutOrphans` (Task 5).

---

### Task 1: Tooling and the two borrowed helpers

**Files:** Modify `agent-rs/scripts/port-status.py`; create `panel_widgets.rs`, `widget_feed_proxy.rs`; regenerate
`agent-rs/PORTING.md`.

**Interfaces:**
- Produces: `panel_widgets::find(cfg: &AgentConfig, device_id: Option<&str>, key: Option<&str>, kind: &str) ->
  Option<WidgetConfig>` (key `page/widget`, or `page/widget:child` for an enabled Smart Stack child; enabled pages for
  that device only; None for an empty key); `widget_feed_proxy::is_public_address(IpAddr) -> bool` (as C#).

- [ ] **Step 1:** Phase map: `ShortcutIconUploadsTests`, `AudioSetVolumeTests`, `MacMediaTests`,
  `SpectrumAnalyzerTests`, `TrackpadTests`, `DictationTests`, `KeySenderTests`, `AppCatalogTests`, `ShortcutIconTests`,
  `MediaArtworkTests`, `MediaArtworkBackoffTests`, `IconEncodingTests`, `AudioCacheTests`, `TerminalStandInTests` → 3;
  methods `ShortcutRuleFollowUpTests.ThePhoneDoesNotGetTheConfiguredTextOfAFailedShortcut`,
  `…ThePhoneStillGetsTheAgentsOwnAccessibilityHint`, `StoreFollowUpTests.AFormatChangeThatCannotRetireTheOldFileKeepsTheOldIcon`,
  `PairingTests.ShortcutIconUploadRejectsConcurrentSettingsSaveAcrossBodyReadWithoutOrphans`,
  `RuleTests.MacroShortcutStepsUpdateToggleStateAndRunOffCmd` → 3. (`PortableAudioPlaybackTests` tests ControlDeck's
  portable deck: plan 6.)
- [ ] **Step 2:** Tests first: `find_matches_page_widget_and_stack_children_of_enabled_pages` (disabled page, disabled
  child, wrong type, empty key, device-specific pages), `public_addresses_exclude_loopback_private_link_local_and_mapped`
  (the C# ranges, IPv4-mapped IPv6). Run → FAIL; implement from `PanelWidgets.cs` and `WidgetFeedProxy.cs`; PASS.
- [ ] **Step 3:** **Commit** `Rust agent: plan 3 tooling, PanelWidgets.Find, IsPublicAddress`.

### Task 2: `KeySender` — keys, text and the mouse

**Files:** Create `key_sender.rs`.

**Interfaces:**
- Produces: `key_sender::{ACCESSIBILITY_HINT, parse(&str) -> Result<Vec<Vec<String>>, String>, send_keys(&str) ->
  Result<(), String>, send_combo(&[String]) -> Result<bool, String>, type_text(&str) -> Result<(), String>,
  try_type_text(&str, &Stop-like cancel) -> Result<bool, String>, win_vk(&str) -> u8, win_extended(&str) -> bool,
  MouseCall { action, dx, dy, delta }, set_test_mouse_sink(Option<Box<dyn Fn(MouseCall)+Send+Sync>>), mouse_move,
  mouse_click, mouse_right_click, mouse_scroll, mouse_horizontal_scroll, mouse_down, mouse_up}`. Errors are the C#
  messages (ArgumentException texts, the Accessibility hint, "sending keys needs xdotool (X11) or wtype (Wayland)", …).

- [ ] **Step 1:** Port `KeySenderTests` (8); add `unusual_specs_are_refused_with_the_csharp_messages` (empty combos,
  two keys besides modifiers, an emoji, modifiers only on macOS). Run → FAIL.
- [ ] **Step 2:** Implement from `KeySender.cs`: the parser, numpad normalisation, aliases, the special-key table
  (VK / X name / mac key code) and F-key codes; Windows `SendInput` (scan codes from `MapVirtualKeyW(vk, 4)`,
  extended keys, Unicode text, mouse flags); Linux `xdotool` (X11) / `wtype` (Wayland) with the C# arguments and
  timeouts, `SendKeys` ignoring a combo's false and pausing 60 ms between combos; macOS `osascript` System Events with
  the Accessibility hint on failure; the Linux mouse with 16 ms move coalescing and 120-unit scroll notches; macOS mouse
  → "The Trackpad widget is not supported on macOS yet."; the test mouse sink (tests serialised behind one lock).
- [ ] **Step 3:** Run → PASS; cross-check clean. **Commit** `Rust agent: KeySender`.

### Task 3: `AudioControl` (Windows, Linux, macOS + `MacAudioProbe`) and `/api/volume`

**Files:** Create `audio_control.rs` (the type, the Windows Core Audio half), `unix/audio_control.rs`,
`unix/mac_audio_probe.rs`; modify `server/routes_system.rs` (or a new `server/routes_desk.rs`, recorded as a ruling),
`state.rs`, `server/mod.rs`, `phonebox-agent/src/main.rs`.

**Interfaces:**
- Produces: `audio_control::{AudioSnapshot { volume: i32, muted: bool, can_control: bool }, AudioControl::new() ->
  Arc<AudioControl>, AudioControl::with(read, set, toggle_mute)` (test seams as the C# internal constructor),
  `snapshot()`, `current()`, `set_volume(i32) -> bool`, `step(i32) -> bool`, `toggle_mute() -> bool`,
  `muted_after_set(bool, Option<&AudioSnapshot>) -> Option<bool>`, `known_reading()`; `impl StateSource` (`audio`)};
  `mac_audio_probe::{unmuted, scale_channels, effective_level, all_written, selected_output, set_volume, toggle_mute,
  can_control, display_volume}`; route `POST /api/volume?level=&op=`; `AppState.audio`; `StateSources::with_audio`.

- [ ] **Step 1:** Port `AudioSetVolumeTests` (17) and `AudioCacheTests` (2) against the seams; add
  `a_machine_without_audio_tools_answers_without_hanging` (Review Focus 2) and route tests (`op=up|down|mute`, a level,
  no arguments → 400 empty, `level=x` → 400; success answers `current()` JSON and pokes the state). Run → FAIL.
- [ ] **Step 2:** Implement from `AudioControl.cs`, `Unix/AudioControl.cs`, `Unix/MacAudioProbe.cs`: the background
  cache (900 ms max age, tickets and generations, poke on change), the control gate, Linux `wpctl`/`pactl` reads,
  writes and toggles; macOS CoreAudio FFI (property selectors as in C#, per-channel scaling, the mute clear) with the
  `osascript` and `m1ddc` fallbacks; Windows `IAudioEndpointVolume` on the default render endpoint, re-acquired every
  10 s, synchronous, no cache.
- [ ] **Step 3:** Run → PASS; cross-check clean; `phase1.json` still passes. **Commit** `Rust agent: AudioControl`.

### Task 4: `Shortcuts` — run, test, macros, toggles, and their routes

**Files:** Create `shortcuts.rs`, `server/routes_desk.rs` (if not created in Task 3); modify `state.rs` (`toggles`),
`server/mod.rs`.

**Interfaces:**
- Consumes: `key_sender` (Task 2), `AudioControl` (Task 3), `PushStore`.
- Produces: `shortcuts::{run(cfg, id, push, audio) -> Option<String>, execute(sc, cfg, depth, push, audio) ->
  Option<String>, state(sc, push, audio) -> Option<bool>, states(cfg, push, audio) -> Option<Map<String, bool>>,
  terminal_stand_ins(cmd, terminal_env) -> Vec<String>, hint_or_generic(&str) -> String}`; routes
  `POST /api/run/{id}` (200 empty, or 500 `application/problem+json` with the RFC 9110 type, title, status and the
  hint-or-generic detail) and `POST /api/shortcuts/test` (415 / 400 `{"error":…}` / 200); state `toggles`.

- [ ] **Step 1:** Port `TerminalStandInTests` (7), `IconEncodingTests` (5), `RuleTests.MacroShortcutStepsUpdateToggleStateAndRunOffCmd`,
  `ShortcutRuleFollowUpTests.ThePhoneDoesNotGetTheConfiguredTextOfAFailedShortcut` and
  `…ThePhoneStillGetsTheAgentsOwnAccessibilityHint` (Review Focus 1); route tests for unknown id (generic 500),
  wrong content type (415), empty command (400 `command is empty`), a failing test run (400 raw message),
  `toggles` JSON (ids as written, not camel-cased). Run → FAIL.
- [ ] **Step 2:** Implement from `Shortcuts.cs` and `Program.cs`: the toggle logic and remembered states, push-item
  state values, `builtin:keys|text|macro|lock|monitor-off|mute` per OS (`LockWorkStation`, `PostMessage` monitor off,
  `VK_VOLUME_MUTE`; `pmset`, `loginctl`/`xdg-screensaver`, `xset`; the "is not installed" rule), macro steps (depth 3,
  `wait` clamp, 40 ms between steps, the C# error texts), program start with `~` expansion and the terminal stand-ins,
  Windows shell-execute and the error-2 stand-in, Unix "run if executable, else `xdg-open`/`open` without args".
- [ ] **Step 3:** Run → PASS; cross-check clean. **Commit** `Rust agent: Shortcuts and the run/test routes`.

### Task 5: `ShortcutIcons`, `ShortcutIconUploads` and the icon routes

**Files:** Create `shortcut_icons.rs`, `shortcut_icon_uploads.rs`; modify `server/routes_desk.rs`,
`server/routes_settings.rs` (cleanup after a save, the shared `IconCommit` lock), `server/routes_info.rs` or wherever
`/api/config` builds `to_public` (the icon lookup), `server/mod.rs`.

**Interfaces:**
- Produces: `shortcut_icon_uploads::{MAX_BYTES = 524288, ShortcutIconUploads::new(icons_dir), exists, revision(id) ->
  Option<i64> (first 8 bytes of SHA-256, little-endian), read(id) -> Option<(Vec<u8>, &'static str)>, read_bounded,
  save(id, content_type, bytes) -> Result<Option<String>, io::Error> (Err when the move fails, as C# throws), remove,
  cleanup(live_ids)}`; `shortcut_icons::{has_icon, revision, clear_has_icon_cache, picture(sc, uploads, upload_only),
  icon(sc) -> Option<Vec<u8>>, looks_like_file_path}`; `server::ICON_COMMIT` (process-wide lock, also taken by the
  settings PUT); routes `GET /api/shortcuts/icon?id=&upload=` (CSP and nosniff headers), `POST` and `DELETE
  /api/settings/shortcut-icon?id=` (`X-Config-Version`, `{"version":…}` / 404 / 409 / 400); `/api/config` fills
  `iconRev` / `iconUpload`.

- [ ] **Step 1:** Port `ShortcutIconUploadsTests` (20), `ShortcutIconTests` (7),
  `StoreFollowUpTests.AFormatChangeThatCannotRetireTheOldFileKeepsTheOldIcon` (`cfg(windows)`, as in C#) and
  `PairingTests.ShortcutIconUploadRejectsConcurrentSettingsSaveAcrossBodyReadWithoutOrphans` (Review Focus 5). Run → FAIL.
- [ ] **Step 2:** Implement from `ShortcutIconUploads.cs`, `ShortcutIcons.cs`, `UploadShortcutIconAsync` and the
  DELETE route: PNG/WEBP signatures, SVG well-formedness (root `svg` in no namespace or the SVG namespace; a DTD is
  ignored, never resolved), the tmp-file-and-move write, retiring other formats and the "previous icon is in use"
  answer, safe file names, the resolution cache revalidated on use, Windows App Execution Alias rejection
  (`win_icon::associated_icon_png`), Unix `.app` bundles through `unix_proc::icon`.
- [ ] **Step 3:** Run → PASS; cross-check clean; `phase1.json` still passes (`/api/config`). **Commit** `Rust agent:
  shortcut icons and uploads`.

### Task 6: `AppCatalog` and `/api/apps*`

**Files:** Create `app_catalog.rs`; modify `server/routes_desk.rs`, `server/mod.rs`, `phonebox-agent/src/main.rs`.

**Interfaces:**
- Produces: `app_catalog::{AppEntry { id, name, path }, AppCatalog::new(cfg) -> Arc<AppCatalog>` with test fields
  `failure_retry_ms`, `converter`, `roots`, `warm_icons`; `SUPPORTED` (macOS), `open_allowed`, `list`, `find`,
  `rescan`, `id_of(path)` (first 16 hex chars of SHA-256), `open(id) -> Option<String>` (the C# error texts),
  `icon(id) -> Option<Vec<u8>>` (async)}; routes `GET /api/apps`, `GET /api/apps/icon?id=` (`Cache-Control: private,
  max-age=86400`), `POST /api/apps/open?id=`.

- [ ] **Step 1:** Port `AppCatalogTests` (8; the two macOS-only ones keep their early return elsewhere); route tests
  for the JSON shape, missing id (400), unknown icon (404) and the refusals. Run → FAIL.
- [ ] **Step 2:** Implement from `AppCatalog.cs`: the scan (roots, one level of folders, hidden names skipped, first
  id wins, name order ignoring case), 5-minute background rescans, the icon pipeline (`plutil`, `sips --resampleWidth
  96`, `qlmanage -t -s 96`, atomic rename into `$TMPDIR/phonebox-app-icons/`), per-id locks plus a global limit of two
  conversions, failures cached for `failure_retry_ms`, warm-up after a scan, `/usr/bin/open` with a 10 s limit.
- [ ] **Step 3:** Run → PASS; cross-check clean. **Commit** `Rust agent: AppCatalog`.

### Task 7: `TrackpadHandler` and `/api/trackpad`

**Files:** Create `trackpad_handler.rs`; modify `server/routes_desk.rs`, `Cargo.toml` (axum `ws`).

**Interfaces:**
- Consumes: `key_sender` mouse functions (Task 2).
- Produces: `trackpad_handler::{TrackpadPayload, is_allowed(cfg) -> bool, process_payload(&[u8], &ConfigStore),
  execute(&TrackpadPayload)}`; route `/api/trackpad`: 403 before the upgrade when not allowed, WebSocket loop, POST
  (body processed when non-empty, always 200 empty), other methods 405.

- [ ] **Step 1:** Port `TrackpadTests` (as handler and router tests instead of `DefaultHttpContext`); add
  `a_flood_of_malformed_frames_is_ignored_and_a_held_button_is_released` (Review Focus 4). Run → FAIL.
- [ ] **Step 2:** Implement from `TrackpadHandler.cs`: case-insensitive payload fields, `type ?? action`, deltas
  rounded half to even and clamped (500 / 1000), non-finite → 0, the action table, config re-checked per payload,
  one warning per process for execution errors, text frames processed one by one (no reassembly beyond the C# 1 KiB
  buffer behaviour, recorded as a ruling if axum's framing differs), close with "Closing", mouse-up on exit when held.
- [ ] **Step 3:** Run → PASS; cross-check clean. **Commit** `Rust agent: trackpad`.

### Task 8: `Dictation` and `/api/dictate`

**Files:** Create `dictation.rs`; modify `server/routes_desk.rs`, `server/mod.rs`.

**Interfaces:**
- Consumes: `panel_widgets::find` (Task 1), `key_sender::{try_type_text, send_combo}` (Task 2).
- Produces: `dictation::{WIDGET_TYPE = "dictate", MAX_CHARS = 1000, FocusedApp { elevated }, clean(Option<&str>) ->
  String, Dictation::new(cfg)` with seams `type_text`, `send_combo`, `focused` (None until plan 6's AppConsole),
  `check_elevation`, `agent_elevated`, `type_async(device_id, widget, text) -> (u16, Option<String>)`}; route
  `POST /api/dictate?widget=&text=` with `X-PhoneBox-Device`.

- [ ] **Step 1:** Port `DictationTests` (9). Run → FAIL.
- [ ] **Step 2:** Implement from `Dictation.cs`: the cleaning rules (control characters and U+2028/2029 to spaces,
  collapsed, 1000 UTF-16 units), the check order and status codes, the one-at-a-time gate with a 2 s wait, the Enter
  key, the re-check, never logging the text.
- [ ] **Step 3:** Run → PASS; cross-check clean. **Commit** `Rust agent: dictation`.

### Task 9: `MediaSnapshot`, `MediaArtworkCache`, `StateSignal.MediaChanged` and the Windows media monitor

**Files:** Create `media_monitor.rs` (shared types, the `MediaDeck` trait, the Windows GSMTC monitor),
`media_artwork_cache.rs`; modify `state_signal.rs`.

**Interfaces:**
- Produces: `media_monitor::{MediaSnapshot { app, title, artist, album, playing, pos_ms, dur_ms, art_id, can_seek }`
  (JSON adds `ageMs` from the sample time), `AlbumArt { bytes, mime }`, `MediaDeck` trait
  (`effective_media`, `try_control_radio`, `stop_all_radios`, `run_with_media_focus`) and `NoDeck`, the Windows
  `MediaMonitor` (`snapshot`, `art`, `control(action, ms)`, `pause`)}; `media_artwork_cache::MediaArtworkCache::with_clock`;
  `state_signal::media_changed(Option<&MediaSnapshot>, Option<&MediaSnapshot>) -> bool`.

- [ ] **Step 1:** Port `MediaArtworkTests` (7), `MediaArtworkBackoffTests` (2) and
  `StateSignalTests.AMediaChangeThatMattersPokesButTheTickingPositionDoesNot`; add `friendly_app_names_follow_csharp`
  (the Windows app-id table). Run → FAIL.
- [ ] **Step 2:** Implement from `MediaArtworkCache.cs` (generations, 1 s / 30 s refresh, failure backoff, ids that
  change only with the bytes or type) and `MediaMonitor.cs` (GSMTC session manager, position from the timeline plus
  the time since its update, clamping, the 8 MiB thumbnail limit, controls, the play/pause key when no session, 1 s
  loop, poke on a change that matters).
- [ ] **Step 3:** Run → PASS; cross-check clean. **Commit** `Rust agent: media snapshot, artwork cache, Windows media`.

### Task 10: The Linux and macOS media monitor, `/api/media/{action}`, `/api/art` and `media` in the state

**Files:** Create `unix/media_monitor.rs`; modify `server/routes_desk.rs`, `state.rs`, `server/mod.rs`,
`phonebox-agent/src/main.rs`, `Cargo.toml` (reqwest rustls).

**Interfaces:**
- Consumes: Task 9, `widget_feed_proxy::is_public_address` (Task 1).
- Produces: `unix::media_monitor::{MediaMonitor::new() -> Arc<…>, start(Stop), snapshot, art, control(action, ms),
  pause, read_mac_system_media(supplied_json, sampled_at), mac_published_at, set_app_position, forget_player_position,
  download_art(url, deadline), sniff_image_mime, load_art(url)}` (`MAX_ART_BYTES = 5 MiB`, `ART_DEADLINE = 5 s`,
  `ANCHOR_EVERY = 5 s`); routes `POST /api/media/{action}?ms=` (through `MediaDeck`; 200 `{"ok":true,"media":…}` or
  400 empty; always pokes) and `GET /api/art`; state `media` = `deck.effective_media(snapshot)`.

- [ ] **Step 1:** Port `MacMediaTests` (15; the Chrome tab scan keeps its macOS-only branch); route tests for an
  unknown action (400), `ms=x` (400), `/api/art` with no art (404). Run → FAIL.
- [ ] **Step 2:** Implement from `Unix/MediaMonitor.cs`: `playerctl` metadata and controls (and the one-time install
  hint), `nowplaying-cli get-raw` parsing with the position rules (unique-id keys, published timestamps, Spotify/Music
  anchors every 5 s), the YouTube tab title lookup, the AppleScript fallback, artwork from `ArtworkData` (magic bytes,
  `sips` for TIFF), art URLs (`file://` checks; `http(s)://` through a client whose resolver returns only public
  addresses, IPv4 first, with the 4 s connect / 5 s total limits and the 5 MiB cap), the 1 s loop and pokes.
- [ ] **Step 3:** Run → PASS; cross-check clean; `phase1.json` still passes. **Commit** `Rust agent: Linux and macOS
  media, media routes`.

### Task 11: `SpectrumAnalyzer`, `/api/spectrum` and the stream's `spectrum` event

**Files:** Create `spectrum_analyzer.rs`; modify `server/routes_desk.rs`, `server/routes_state.rs` (stream event),
`server/mod.rs`, `phonebox-agent/src/main.rs`.

**Interfaces:**
- Produces: `spectrum_analyzer::{BANDS = 48, FPS = 30, FFT_SIZE = 2048, MAX_BOOST_DB = 40, SpectrumAnalyzer::new(cfg),
  active, frame, shown, want, analyze(&[f32], sample_rate) -> Result<Vec<i32>, String>, loopback_gain(bool, f32) ->
  f32, downmix(...)}`; route `GET /api/spectrum` → `{"shown","active","fps":30,"b"}`; stream `event: spectrum` with
  `{"b":[…]}` while active, at most every 1000/30 − 3 ms.

- [ ] **Step 1:** Port `SpectrumAnalyzerTests` (14). Run → FAIL.
- [ ] **Step 2:** Implement from `SpectrumAnalyzer.cs`: Hann window, in-place radix-2 FFT, the band edges, peak
  amplitude, tilt and scaling; downmix of float32 / PCM16 / PCM32 with the gain; Windows WASAPI loopback on the default
  render endpoint (hardware-volume check, gain from the endpoint volume, 4096-sample ring, 33 ms tick, 250 ms silence
  → zeros, device-change restart, stop 5 s after the last want or when no page shows the widget); inactive elsewhere.
- [ ] **Step 3:** Run → PASS; cross-check clean. **Commit** `Rust agent: spectrum`.

### Task 12: Wiring, parity corpus and the MacBook desk check (exit)

**Files:** Modify `phonebox-agent/src/main.rs`, `server/mod.rs`; create `tools/parity/corpus/phase3.json`,
`agent-rs/scripts/mac-desk-check.sh`; modify `.github/workflows/rust.yml`.

- [ ] **Step 1: Wire** media, audio, spectrum, apps and dictation into `AppState` and `serve`; they end on `Stop`.
- [ ] **Step 2: Parity corpus `phase3.json`** (refusals and reads only): `/api/state` with `media`/`audio`/`toggles`
  shape-checked, `/api/apps`, `/api/apps/icon?id=nope`, `/api/apps/open?id=nope`, `/api/spectrum`, `/api/art`,
  `/api/volume` (no arguments, `level=x`), `/api/media/bogus`, `/api/media/seek?ms=x`, `/api/run/phonebox-no-such`,
  `/api/shortcuts/test` (wrong type, empty command), `/api/shortcuts/icon?id=nope`, `/api/settings/shortcut-icon`
  POST and DELETE for an unknown id, `/api/dictate` (no widget), `/api/trackpad` POST and GET without the control
  option. Run `remote-parity.sh` on all three corpora. Expected: all pass.
- [ ] **Step 3: MacBook desk check (laptop exception).** `mac-desk-check.sh` runs the Rust agent beside the installed
  one (spare port, config copy with an unmatched `deviceSerial`, temporary home), compares `media`, `audio` and
  `toggles` in `/api/state` and `/api/apps` (ids and names) between the two agents, fetches one app icon (a PNG),
  sets the volume to its current level when not muted (no audible change) and checks the answer, and runs
  `/api/shortcuts/test` with `/usr/bin/true`. It never controls media, sends keys or opens apps. Stops the Rust agent by
  PID. Expected: `desk controls match`. The phone itself drives these routes again in plan 4's exit check, once it can
  reach the Rust agent over Wi-Fi (today the phone is on Wi-Fi adb, which refuses the USB tunnel).
- [ ] **Step 4:** `port-status.py --check 3` → no unported phase-3 test. Full suite, `remote-cross-check.sh`, all
  parity corpora green. **Commit** `Rust agent: plan 3 exit: desk controls wired, phase-3 parity, MacBook desk check`.
