# Rust agent plan 6: Integrations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Rust agent serves every integration the C# agent serves: pushed values and webhooks, notes, the phone
list, Home Assistant, OBS, Discord, Spotify, feeds and the feed proxy, calendars (ICS), the photo frame, cameras,
privacy and session-lock monitors, the rule engine, the charge guard, the app console and the Control Deck (radio,
sounds, tasks, logos, directory, and the Windows native backend), so that every C# route has a Rust twin and the full
route corpus passes.

**Architecture:** One module (or module folder) per C# source, ported with its tests. Every outbound client, clock,
process runner, socket path and report getter is an injected seam, so the parallel Rust tests stay independent of each
other and of the network. Background loops (OBS 2 s, Discord 2 s with backoff, feeds 5 s, calendars 3 s, cameras 1 s,
rules 1 s, charge guard, privacy, session lock, deck 250 ms) start in `main` under the shared `Stop`. New route modules
register on both listeners; the Wi-Fi allow-list from plan 4 already decides what Wi-Fi serves. Integrations reach the
deck, the rules and the charge guard only through narrow traits.

**Tech Stack:** Rust 1.98, axum 0.8, tokio, reqwest 0.12 (rustls; system and webpki roots; gzip/deflate/brotli),
tokio-tungstenite 0.29 (OBS), jiff (time zones), roxmltree 0.20 (feeds), sha2/hmac, base64, image (photo scaling on
Windows only), the `windows` crate (Core Audio, WinRT media, GSMTC, WinMM, DDC/CI, services, Media Foundation).

**Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md`. The C# agent at `bb72345` is the behavioral spec.
Surveys with every rule, string, timing, shape and file:line: `/tmp/plan6-survey-{a,b,c,d}.md`, copied into the plan
workspace as `survey-a.md` (Home Assistant, OBS, Discord, Spotify), `survey-b.md` (feeds, proxy, calendar, photos,
cameras), `survey-c.md` (Control Deck), `survey-d.md` (rules, charge guard, privacy, session lock, console, notes,
webhooks, the route gap, the test assignment). Below "a§N" means survey-a section N.

## Global Constraints

- JSON as C# writes it: camelCase keys in the record order the surveys list, nulls written (`json_response`), doubles
  in .NET's shortest form. Sidecar files shared with an installed C# agent keep C#'s format byte for byte:
  `discord-token.json` and `<config>.spotify.json` (PascalCase, compact; `Expires` seconds vs milliseconds, a§0.3),
  `data.json` (PascalCase, plan 1), `LocalDir/photos-cache/` names (b§0.7) and `LocalDir/radio-logos/` names and prune
  rules (c§0.7). A difference there makes the two agents delete each other's files.
- Secrets never reach a log, an error text, test output or a commit: HA/OBS/Discord/Spotify tokens and passwords, feed
  and calendar URLs, camera addresses, the webhook key. reqwest errors are stripped of their URL (`without_url()` or a
  fixed text) before any feed, calendar, HA or Spotify error is built. Tests use fake values only.
- Outbound HTTP: one client per C# policy, never shared across policies (b§0.2, c§0.6): public-only clients filter DNS
  answers at connect time and refuse IP literals per hop; redirects followed by hand where C# does; size caps counted
  on decoded bytes while reading. HTTPS trusts the system store plus the webpki roots (C# uses the OS store).
- Control is gated by widgets on enabled pages and rechecked after every await, exactly where C# rechecks (a§0.4).
- User-visible fixed strings are copied exactly, `›` and `…` included. Where C# shows a .NET exception message, the
  Rust text is our own, short, URL-free, and the parity corpus ignores that field.
- UTF-16 counts for every length check and cut; `Math.Round` is `round_ties_even`; custom formats go through
  `dotnet_text::format_custom`.
- No process-wide statics beyond the ones C# has and the surveys list (`PhotoFrame` id key, `Ics` zone cache, the
  proxy and calendar clients, `Wake`); everything else is an instance owned by `AppState`.
- Blocking work (process probes, `osascript`, `ioreg`, `sips`, ffmpeg spawn, native audio, COM) runs off the async
  workers. Child processes are killed as a tree and reaped; none outlives the agent's `Stop`.
- Windows-only code sits behind `#[cfg(windows)]`, compiles in `remote-cross-check.sh` and is exercised by the CI
  `windows-latest` jobs; macOS-only code likewise for `aarch64-apple-darwin` and `x86_64-apple-darwin`.
- Tests run on the server (`agent-rs/scripts/remote-*.sh`); MacBook checks run beside the installed agent on spare
  ports with an unmatched `deviceSerial` (spec, owner decision 4), stopped by PID, never writing the owner's files.
- Rust test names are the snake_case of the C# method names (`port-status.py`). A C# test that cannot apply gets a
  `not ported` reason in `port-status.py`, never silence.

## Rulings made while writing this plan

- **The deck's media focus becomes async** (c§7.1 option B): `MediaDeck` gets an async focus gate
  (`tokio::sync::Mutex<()>` held across the native media call, the 5 s "pause other media" step and the Spotify call);
  `routes_desk.rs` and `media_monitor.rs` follow. It mirrors C# and its nine race tests; the sync option would need
  `block_on` inside blocking threads.
- **Windows playback uses one WinRT `MediaPlayer`** for files, HTTP streams and HLS (c§7.2) instead of Media Foundation
  + WASAPI + a second HLS player: the same ended/failed events and device selection with far less unverifiable code.
  Cost if wrong: a Windows-only playback difference no test sees.
- **`POST /api/refresh?reload=1` stays a 400** as in C# (d§7.1); the settings page's Reload button is an upstream bug
  to fix after the switch (owner decision 1: upstream is frozen).
- **`GET /api/phones` and `POST /api/phone/open`** (plan-1 components nobody owned, already called by the settings
  page) are ported here (d§0.2).
- **Time zones come from `jiff`**; Windows zone ids resolve on Unix only if the C# baseline resolves them there
  (probe it in Task 11, b§7.2).
- **Photo scaling**: `sips` on macOS as C#; the `image` crate on Windows; Linux keeps C#'s "too large" (b§7.7).
- **OBS speaks `ws://` and, under `tls`, `wss://`** through `tokio-tungstenite` (a§7.1).

## Review Focus

1. A side-by-side C# agent on the same machine: the shared files and cache folders (`photos-cache`, `radio-logos`,
   `discord-token.json`, `.spotify.json`, `data.json`) are read and written by both without either deleting or
   corrupting the other's entries.
2. A secret in a URL (feed, calendar, camera, HA) when the remote fails in every way (DNS, refused, TLS, redirect,
   timeout, oversize): no error text, log line or phone answer carries it.
3. A widget hidden or a page disabled while an integration call is in flight: the call is not made, or its result is
   not applied, exactly where C# rechecks.
4. Overlapping deck commands (radio start during stop, settings saved during play, a request cancelled mid-start, a
   late "sound stopped" callback): no deadlock, no stuck radio, no station left playing that the settings disabled.
5. A remote that misbehaves at the protocol level (an OBS server that never answers Identify, a Discord pipe that
   closes mid-frame, an ICS with a 10 000-period RRULE, a 50 MiB feed, a redirect loop to a private address): bounded
   time and memory, no panic, C#'s error text.

---

## Task 1: Foundations

**Files:** modify `agent-rs/scripts/port-status.py` (d§4.5 edits: all 370 `todo` rows assigned, 353 → 6, 17 → 7; plus
the entries a§4.11, b§4.6 and c§4.7 list); create `net/mod.rs` (`public_resolver`, `without_url`, `strip_error`),
`server/settings_hooks.rs`; modify `server/routes_settings.rs`, `Cargo.toml` (reqwest features `gzip`, `deflate`,
`brotli`, `rustls-tls-native-roots` under `tls`; `tokio-tungstenite` and `jiff` to `[dependencies]`; `windows`
features the later tasks need), `widget_feed_proxy.rs` (rename its test to the C# names).

**Interfaces:**
- Produces: `net::PublicResolver` (reqwest `dns::Resolve` keeping only `widget_feed_proxy::is_public_address`
  answers; a refusal error tagged so callers can map it), `net::is_public_ip_address` (the proxy rule plus
  `192.88.99.0/24`, c§0.6), `net::error_text(&reqwest::Error) -> String` (never a URL);
  `SettingsHooks { after_save: Vec<Arc<dyn Fn() + Send + Sync>> }` on `AppState`, called by `put_settings` after a
  successful save in C#'s order (`Program.cs:370-373`: deck stop-disabled, photos, cameras, deck reconcile).
- Consumes: plans 1–5.

- [ ] **Step 1: Tests (RED):** the tests d§4.1 marks "no new production code" (`CoreAuditTests` 5, `AppearanceTests`
  4, `StoreFollowUpTests` 3 incl. `DocumentationAddressesAreNotPublic`, `AuditRegressionTests` 3 PushStore ones,
  `TestPortsTests` as `not ported` with its reason), the two C# address tests b§3 names, a resolver test (a name
  resolving to `127.0.0.1` and to a public address), `error_text` on a refused connection to a URL with a secret
  query (the secret absent), and a settings-hooks test (hooks run once, in order, only after a successful save).
- [ ] **Step 2:** Implement; `python3 -I agent-rs/scripts/port-status.py` shows 0 rows without a phase.
- [ ] **Step 3:** PASS (`remote-check.sh`, `remote-cross-check.sh`). **Commit** `Rust agent: plan 6 foundations`.

## Task 2: Pushed values, webhooks, notes and the phone list

**Files:** create `server/routes_push.rs`, `note_edit.rs`, `server/routes_phones.rs`; modify `push_store.rs` (the
camelCase API view, d§0.3), `adb.rs` (`open_panel`, `PhoneRegistry` listing), `server/mod.rs`.

**Interfaces:**
- Produces: the 9 webhook/push routes, `POST /api/refresh`, `POST /api/notify`, the note route, `GET /api/phones`,
  `POST /api/phone/open`, `POST /api/phone/battery` if d§2 lists it here (it feeds `usb_doctor::LAST_REPORT`); their
  auth (local, Wi-Fi phone key, webhook key) per d§1.9 and d§2; `json_element_text(&Value) -> String` (.NET
  `JsonElement.ToString()`: `True`/`False`, `""` for null, raw number and raw JSON text, d§0.4) for Task 16 to reuse.
- Consumes: `push_store`, `wake`, `config_store`, `adb`.

- [ ] **Step 1: Tests (RED):** every d§4.1 test for PushStore webhooks, `NoteEditTests` (7), the phone list and open
  (shape, unknown serial, adb failure), `json_element_text` cases, and route tests for each d§2 row (status codes,
  shapes, key order, auth, the `reload=1` 400).
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: pushed values, webhooks, notes and phones`.

## Task 3: Deck foundations and the async media focus

**Files:** create `control_deck/mod.rs`, `control_deck/models.rs`, `control_deck/leases.rs`, `control_deck/midi.rs`;
modify `media_monitor.rs` (`MediaDeck` async focus), `server/routes_desk.rs`, `phonebox-agent/src/main.rs`.

**Interfaces:**
- Produces: `DeckResult { ok, error, … }` (c§1.1, shared with OBS and Spotify), the deck records with their
  serializers (the `/api/controls/capture` profile writes `output`/`input` as null, c§0.7), `trait DeckNative`
  (c§1.1) with a test fake, `MicrophoneLeases`, `PowerConfirmations`, `DeckMidi::parse`, `osc_packet`;
  `MediaDeck::run_with_media_focus` becomes `async fn with_media_focus<F: Future>(&self, work: F) -> F::Output` (a
  `tokio::sync::Mutex<()>`; `NoDeck` keeps its behavior).
- Consumes: `media_monitor`, `audio_control`.

- [ ] **Step 1: Tests (RED):** the 8 `ControlDeckTests` c§7 task 1 lists (leases, power confirmations, OSC, MIDI,
  phone config), the media routes' existing tests still green through the async focus.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: deck foundations and async media focus`.

## Task 4: Home Assistant

**Files:** create `home_assistant.rs`, `server/routes_integrations.rs` (HA rows); test fake: a loopback HTTP server
helper in `testutil` that can hold responses open.

**Interfaces:**
- Produces: `HomeAssistant::{new(cfg, http), states, act, switch(id, on, settings) -> (Option<String>, bool)}` (a§1.1;
  `submitted` true as soon as the send starts), the 3 s cache keyed by URL+token, the lock/vacuum fresh-read rules
  (a§0.5), the HA routes (a§2).
- Consumes: Task 1 (`net`), `config` widgets.

- [ ] **Step 1: Tests (RED):** `HomeAssistantTests` (16), `HomeAssistantStateTests` (7), `IntegrationVisibilityTests`
  HA methods (a§4.8), route tests per a§2.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: Home Assistant`.

## Task 5: OBS

**Files:** create `obs_monitor/mod.rs`, `obs_monitor/controls.rs`; modify `state.rs` (`with_obs`), routes module.

**Interfaces:**
- Produces: `ObsMonitor::{new(cfg), start(stop), snapshot(), live() -> bool, act, deck, deck_act}` (a§1.2–1.3), the
  obs-websocket 5 handshake and auth (a§0.2), the session-bound 1.5 s deck cache with the 1 s answer budget (a§0.6,
  session ids instead of reference equality), `pub(crate)` test hooks for the fields the C# tests reflect on, the
  `obs` state frame, the OBS routes.
- Consumes: Task 3 (`DeckResult`), Task 1.

- [ ] **Step 1: Tests (RED):** `ObsTests` (6), `ObsPermissionTests` (5) against a loopback WebSocket fake (password
  `supersecret`, fixed salt/challenge), the `obs` frame, route tests per a§2.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: OBS`.

## Task 6: Discord

**Files:** create `discord_monitor/mod.rs`, `discord_monitor/ipc.rs`; routes module.

**Interfaces:**
- Produces: the IPC client (Unix socket / Windows named pipe, a§0.2; frame codec with the 1 MiB cap), login with the
  shared `discord-token.json`, backoff 5→10→20→30 s, the refusal/timeout/drop distinctions (a§1.4), the Discord
  routes and image route (uses Task 13's `mime_of`; until then a local copy of the 8-byte sniffer moved there in
  Task 13).
- Consumes: Task 1, `app_paths::write_all_text_private`.

- [ ] **Step 1: Tests (RED):** `DiscordTests` (19) over an in-memory duplex IPC fake, the sidecar file round trip with
  a C#-written fixture, route tests per a§2.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: Discord`.

## Task 7: Spotify

**Files:** create `spotify_control.rs`; modify `server/guard.rs` (the callback exception, a§0.8), routes module.

**Interfaces:**
- Produces: `SpotifyControl::{new(cfg, http), begin, has_pending_state, complete, snapshot, act(kind, id)}` (a§1.5:
  PKCE, account generations, 5 s cache, 429 handling), the shared `<config>.spotify.json`, the Spotify routes; the
  action route runs inside `MediaDeck::with_media_focus` after `stop_all_radios` (Task 3).
- Consumes: Tasks 1, 3; an HTTP seam like plan 5's `UsageHttp`.

- [ ] **Step 1: Tests (RED):** `SpotifyControlTests` (11), `SpotifyCredentialTests` (4),
  `DeckAuditTests.SpotifyTimeoutIsReportedAsUnavailableInsteadOfThrowing`, the guard exception (a cross-site GET with
  the pending `state` passes, any other does not), route tests per a§2.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: Spotify`.

## Task 8: Feed parser

**Files:** create `feeds/parser.rs`, `feeds/html_entities.rs` (HTML 4 table generated from .NET's list).

**Interfaces:**
- Produces: `parse(bytes, base_url) -> Result<Vec<FeedItem>, String>` (b§1.3: RSS/Atom/RDF by local name, DOCTYPE
  stripped then `allow_dtd: false`, .NET's encoding set, `html_decode`, RFC 822 dates with an ISO 8601/RFC 3339/bare
  date fallback — the supported subset is a ruling).
- Consumes: nothing new.

- [ ] **Step 1: Tests (RED):** `FeedParserTests` (13).
- [ ] **Step 2:** Implement. Probe the C# baseline for the encodings b§7.4 leaves open; ledger what it shows.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: feed parser`.

## Task 9: Feed client and monitor

**Files:** create `feeds/http.rs`, `feeds/mod.rs` (monitor); routes module.

**Interfaces:**
- Produces: `FeedHttp` (two pools, manual redirects ≤ 5, 4 MiB decoded cap, 10 s connect / 20 s per feed, ETag and
  Last-Modified, b§1.2), `FeedMonitor::{new, start(stop), snapshot, open(id)}` (5 s tick, 15 min per URL, only while an
  enabled page shows a `feeds` widget), `GET /api/feeds`, `POST /api/feeds/open`.
- Consumes: Tasks 1, 8.

- [ ] **Step 1: Tests (RED):** `FeedHttpTests` (11), `FeedMonitorTests` (10), route tests per b§2.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: feeds`.

## Task 10: Widget feed proxy

**Files:** modify `widget_feed_proxy.rs`; routes module.

**Interfaces:**
- Produces: the five allowed hosts, URL validation, public-only connect trying every address IPv4 first, 1 MiB cap,
  3xx → 502, timeout → 504, caller cancellation, `GET /api/proxy` (b§1.4, b§2).
- Consumes: Task 1.

- [ ] **Step 1: Tests (RED):** `WidgetFeedProxyTests` (3), `WidgetFeedProxyTransportTests` (7), route tests.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: widget feed proxy`.

## Task 11: ICS parser

**Files:** create `calendar/ics.rs`, `calendar/windows_zones.rs` (CLDR table, used where the probe says C# resolves
Windows ids).

**Interfaces:**
- Produces: `Ics::parse(text, now, local_zone, horizon) -> Vec<CalendarEvent>` (b§1.6: TZID zones, DST gap +1 h, fold
  earliest, .NET date limits, RRULE with the 3000-period cap and every edge b§7.13 lists, EXDATE, RECURRENCE-ID).
- Consumes: `jiff`.

- [ ] **Step 1: Tests (RED):** `IcsTests` (29) and `CalendarOverlapTests` (2), with the zones passed in so results do
  not depend on the server's zone.
- [ ] **Step 2:** Implement. Probe the C# baseline on the server for Windows ids on Linux; ledger the result.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: ICS parser`.

## Task 12: Calendar monitor

**Files:** create `calendar/mod.rs`; routes module.

**Interfaces:**
- Produces: `CalendarMonitor::{new, start(stop), snapshot, current(names) -> Vec<CalendarEvent>}` (b§1.5: 3 s check
  with `Wake`, refresh at start / 10 min / key change / wake, no widget gate, no address filter, redirects followed,
  20 s), `GET /api/calendar`; the `webcal://` rewrite.
- Consumes: Tasks 1, 11.

- [ ] **Step 1: Tests (RED):** the calendar `MonitorAuditTests` method, the b§4 monitor tests, the URL-free error
  test, route tests.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: calendars`.

## Task 13: Photo frame

**Files:** create `photo_frame.rs`; routes module; settings hook registration.

**Interfaces:**
- Produces: `PhotoFrame::{new(cfg, local_dir), list, photo(id), settings_saved, mime_of(bytes)}` (b§1.7: HMAC ids with
  a per-process key, 5 min scan, `ScanNow` 30 s, one conversion at a time, `sips` 20 s on macOS, the `image` crate on
  Windows, the shared `photos-cache` names and prune rules), `GET /api/photos`, `GET /api/photos/{id}`.
- Consumes: Task 1 (settings hooks).

- [ ] **Step 1: Tests (RED):** `PhotoFrameTests` (18) with `#[cfg(test)]` accessors for the scan generation and the
  published library, a cache-name test against a C#-computed name.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: photo frame`.

## Task 14: Cameras

**Files:** create `camera_feeds.rs`; routes module; settings hook registration.

**Interfaces:**
- Produces: `CameraFeeds::{new(cfg, launcher), frame(widget, fresh), settings_saved, start(stop)}` (b§1.8: 1 s timer,
  10 s idle stop, 20 s stall restart, 2→60 s backoff, missing ffmpeg every 10 s, at most 4 running, the stderr ring,
  `fresh` true only for `true`/`1`), `GET /api/camera`; ffmpeg in its own process group, killed as a tree.
- Consumes: Task 1, `tool_process`.

- [ ] **Step 1: Tests (RED):** `CameraFeedsTests` (20) with a fake launcher that never calls back inside `launch`;
  `TheAddressNeverGoesToThePhone` asserts the camelCase shape the phone gets (b§7.17).
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: cameras`.

## Task 15: Privacy, session lock and screen sync

**Files:** create `privacy_monitor.rs`, `session_lock_monitor.rs`, `screen_sync.rs`; modify `adb.rs` (follow the
computer lock before open-on-connect, d§0.8), `state.rs` (`privacy`).

**Interfaces:**
- Produces: `PrivacyMonitor` (Linux `/proc`, Windows Core Audio + MF sensor activity + consent-store fallback, macOS
  "unknown"; d§1.4), `SessionLockMonitor` (macOS CoreGraphics; d§1.5), `ScreenSynchronizer` (gate, per-device failure,
  cancellation), `trait PrivacySource { snapshot() }` for Task 16.
- Consumes: `adb`, `state`.

- [ ] **Step 1: Tests (RED):** `PrivacyMonitorTests` (3), `ScreenSyncTests` (3), `ScreenCancellationTests` (3), the
  adb order test.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: privacy, session lock and screen sync`.

## Task 16: Rule engine

**Files:** create `rule_engine/mod.rs`, `rule_engine/inputs.rs`; routes module; modify `state.rs` (`rules`,
`rulesPaused`).

**Interfaces:**
- Produces: `RuleEngine::{new(cfg, inputs, clock), start(stop), snapshot, test(rule)}` over `trait RuleInputs`
  (calendar `current`, OBS `live`/snapshot, privacy, push values, process names, idle seconds, front app, CPU, battery,
  time; d§0.5): config order, state keyed by serialized form, cooldowns, release 4 s except `time`, CPU 15 s, start 3 s,
  every 1 s on a blocking thread; `GET/POST /api/rules*` (d§2); the `TimeSpan.TryParse` subset.
- Consumes: Tasks 2 (`json_element_text`), 5, 12, 15, `shortcuts`.

- [ ] **Step 1: Tests (RED):** `RuleTests` (13), `RuleWindowTests` (1), `ShortcutRuleFollowUpTests` (2, with a log
  capture seam), the `CodexBarTests` rule check, route tests.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: rule engine`.

## Task 17: Charge guard

**Files:** create `charge_guard.rs`; routes module.

**Interfaces:**
- Produces: `ChargeGuard::{new(cfg, ha, push, audio, report: Arc<dyn Fn() -> Option<Report>>), start(stop), status}`
  (d§1.3: thresholds, recovery state machine, HA switch or shortcuts), the 3 charge routes.
- Consumes: Task 4 (`switch`), `usb_doctor::LAST_REPORT` through the injected getter.

- [ ] **Step 1: Tests (RED):** `ChargeGuardTests` (8), `ChargeGuardSwitchTests` (2), `ChargeRecoveryTests` (11), the
  charge `MonitorAuditTests` method, against Task 4's loopback fake holding `turn_off` open.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: charge guard`.

## Task 18: App console

**Files:** create `app_console.rs`; modify `config/store.rs` (key syntax and 20-press checks), `dictation.rs`
(`focused`), routes module.

**Interfaces:**
- Produces: `AppConsole` (Windows focus and macros; `supported:false` and 409 elsewhere; d§1.6), `GET/POST
  /api/console*`, `focused_now()` for dictation.
- Consumes: `key_sender`.

- [ ] **Step 1: Tests (RED):** `AppConsoleTests` (18), `AppConsoleFocusRaceTests` (2), route tests.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: app console`.

## Task 19: Control tasks

**Files:** create `control_deck/tasks.rs`; routes module.

**Interfaces:**
- Produces: `ControlTasks` (c§1: 30 min task / 24 h server, 64 Ki-character tail, "Running"/"Completed"/"Failed (n)"),
  `GET /api/controls/task-output/{id}`.
- Consumes: Task 3, `shortcuts::split_arguments`, `tool_process`.

- [ ] **Step 1: Tests (RED):** `ControlTasksTests` (2), `DeckAuditTests.ATaskThatCannotStart…`, route test.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: control tasks`.

## Task 20: Deck core I

**Files:** create `control_deck/core.rs`, `server/routes_deck.rs`.

**Interfaces:**
- Produces: the audio cache, `audio_for_phone`, `snapshot`, `act` for device, session, default, ptt, panic, duck,
  power-arm, power, brightness, desktop, power-plan, lock, monitor-off; `profile`, `capture`; routes `state`,
  `command`, `capture` (c§1, c§2; one `DeckState` mutex with `_locked` helpers, c§7.3).
- Consumes: Tasks 3, 19.

- [ ] **Step 1: Tests (RED):** the `ControlDeckCommandTests` (9), `ControlDeckPanicTests` (3) and `PanicMuteDrops…`
  rows c§7 task 3 lists, route tests.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: deck core`.

## Task 21: Deck core II: radio, sleep and ducking

**Files:** create `control_deck/radio.rs`, `control_deck/sleep.rs`; modify `server/mod.rs`, `main.rs` (the real deck
replaces `NoDeck`), settings hook registration.

**Interfaces:**
- Produces: the radio state machine and focus gate, `effective_media`, `try_control_radio`, the `MediaDeck`
  implementation, `stop_disabled_controls`, `reconcile_radios`, the 250 ms loop and shutdown, sleep with fade, ducking
  (c§0.2–0.5).
- Consumes: Tasks 3, 20.

- [ ] **Step 1: Tests (RED):** the radio `ControlDeckTests` (8), `RadioCardViewsTests.TapToToggle…`, the sleep
  `ControlDeckCommandTests` (2) and `DisabledControlIsHidden…`, the sleep `DeckAuditTests` (2).
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: deck radio, sleep and ducking`.

## Task 22: Deck actions and integrations

**Files:** create `control_deck/actions.rs`, `control_deck/integrations.rs`.

**Interfaces:**
- Produces: `action()` for every kind, `shortcut_action`, `shortcut_running` (false off Windows), OSC send, workflow,
  playlist dispatch through Task 7, the complete `describe` (c§1, non-Windows parts).
- Consumes: Tasks 7, 20, 21.

- [ ] **Step 1: Tests (RED):** the c§7 task 5 rows (`DisabledPlaylistProfile…`, `PlaylistActionStops…`,
  `ZeroSoundVolume…`, `AFolderShortcut…`, `DeckRunsTheOffCommand…` as Windows-only).
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: deck actions`.

## Task 23: Portable deck native

**Files:** create `control_deck/portable.rs`; routes (`listen` 403 path).

**Interfaces:**
- Produces: `PortableDeckNative` (CLI players `ffplay`/`mpv`/`cvlc`/`vlc`, `afplay` for files on macOS, c§1.8
  arguments, 15 s lookup cache, one process per sound, tree kill, exit → `SoundStopped`), selected off Windows.
- Consumes: Task 3.

- [ ] **Step 1: Tests (RED):** `PortableAudioPlaybackTests` (4).
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: portable deck playback`.

## Task 24: Radio directory

**Files:** create `control_deck/fetch.rs` (the transport seam, c§7.4), `control_deck/radio_directory.rs`; routes.

**Interfaces:**
- Produces: `trait Fetch` with a reqwest implementation (redirects off, public-only resolver) and a fake;
  `is_safe_public_uri` (a hand-written scanner for the lookaround, .NET's non-canonical IPv4 parse), mirrors with DNS +
  PTR discovery, parse, slugify, check; the 3 directory routes.
- Consumes: Task 1.

- [ ] **Step 1: Tests (RED):** `RadioDirectoryTests` (11), `RadioDirectoryBodyTests` (2).
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: radio directory`.

## Task 25: Radio logos

**Files:** create `control_deck/radio_logos.rs`; routes.

**Interfaces:**
- Produces: `RadioLogoCache` (memory and disk, in-flight sharing, search, the monogram SVG byte for byte, the shared
  `radio-logos` folder names and prune rules), the radio-logo route.
- Consumes: Task 24 (`Fetch`).

- [ ] **Step 1: Tests (RED):** `RadioCardViewsTests` (7), a monogram SVG test against C#-produced bytes.
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: radio logos`.

## Task 26: HLS playlists

**Files:** create `control_deck/hls.rs`.

**Interfaces:**
- Produces: `HlsPlaylist` pure helpers and the probe through `Fetch` (c§1, `Hls.cs:13-90`).
- Consumes: Task 24.

- [ ] **Step 1: Tests (RED):** `HlsRadioTests` (3).
- [ ] **Step 2:** Implement.
- [ ] **Step 3:** PASS. **Commit** `Rust agent: HLS playlists`.

## Tasks 27–31: Windows native deck (`#[cfg(windows)]`, CI-verified)

Each task: create its `control_deck/windows/*.rs` file, add pure-helper tests where C# has logic worth pinning (named
after the behavior; uncounted in `port-status.py`), pass `remote-cross-check.sh` and the CI Windows build, commit.

- **Task 27:** Core Audio endpoints, sessions, `ProcessName`, default device through `IPolicyConfig` (c§1, Native
  `:27-216`). Commit `Rust agent: Windows deck audio`.
- **Task 28:** GSMTC deck media for all sessions, media actions, pause (Native `:218-281`). Commit `Rust agent: Windows
  deck media`.
- **Task 29:** playback through one WinRT `MediaPlayer` (files, HTTP, HLS), device selection, `SoundStopped` (Native
  `:283-377`, Hls `:92-176`). Commit `Rust agent: Windows deck playback`.
- **Task 30:** monitors, DDC/CI brightness, display modes, power plans, services, focus, layout, presentation, power
  (Windows.cs). Commit `Rust agent: Windows deck hardware`.
- **Task 31:** WinMM MIDI, loopback `listen`, the Voicemeeter DLL, SoundVolumeView routing (Native `:378-420`,
  Integrations `:33-94`). Commit `Rust agent: Windows deck extras`.

## Task 32: Exit: parity and the MacBook check

**Files:** create `tools/parity/corpus/phase6.json`, `agent-rs/scripts/mac-integrations-check.sh`; modify
`.github/workflows/rust.yml`.

- [ ] **Step 1: Corpus:** every route d§5 lists, with the error and auth cases each survey's §2 gives and the
  deterministic deck cases c§7.7 lists; live values, .NET exception texts and network-dependent fields ignored. All six
  corpora pass on the server.
- [ ] **Step 2: Mac check (laptop exception, beside the installed agent):** both agents answer the same for pushed
  values, notes (on a copy of the config), feeds and calendars (the owner's, compared by shape and titles), photos
  (list), HA/OBS/Discord/Spotify status, rules, charge status, privacy, the deck state; a radio station plays and stops
  through ffplay or mpv; nothing is written to the owner's config or caches.
- [ ] **Step 3:** `port-status.py --check 6` clean; full suite, cross-check, all corpora green; no C# route left in
  d§5's gap table except plan 7's four. **Commit** `Rust agent: plan 6 exit: integrations parity and the MacBook
  check`.
