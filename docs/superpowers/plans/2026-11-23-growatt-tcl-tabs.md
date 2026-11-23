# Growatt and TCL tabs: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** two phone panel widgets, Growatt (the shop inverter) and TCL (the air conditioners), each on its own page,
with the actions their web dashboards offer, through the Rust agent on the Mac.

**Architecture:** the agent holds the credentials (Settings › General) and talks to each service itself, like the Home
Assistant integration: Growatt through the owner's growatt-server REST API (`http://growatt.lan/api/v1`, a 30-day JWT
from `POST /auth/login`), TCL through TCL's cloud exactly as the tcl-server dashboard does (TCL account sign-in → cloud
URLs → SaaS token → Cognito credentials → AWS IoT shadow read and publish, SigV4-signed). The phone asks the agent
(`/api/growatt*`, `/api/tcl*`); every action is validated and mapped to fixed payloads in the agent.

**Tech Stack:** Rust (agent-rs: reqwest, serde, sha2, hmac, new `md-5`), the panel's plain ES modules, the settings
page, Playwright UI tests in the server's container.

**Spec:** none written. The owner asked in conversation (2026-11-23) for "separate tabs in my mobile with actions to
take just like the web has" for /Volumes/storage/Codes/growatt-server and /Volumes/storage/Codes/tcl-server and said
"go ahead" twice without further questions. Facts this plan relies on (verified 2026-11-23): `http://growatt.lan`
answers `/api/v1/health` from the Mac; the tcl-server backend's Vercel deployment no longer exists
(`DEPLOYMENT_NOT_FOUND`), and the hosted TCL dashboard calls TCL's cloud directly
(`tcl-server/frontend/src/service/tcl/TclClient.ts`, constants in `config.ts`, both public). The decisions below are
mine, stated to the owner.

## Global Constraints

- The C# agent stays the parity oracle: with Growatt and TCL unset, `GET /api/settings` and `GET /api/config` are
  byte-identical to before (no new keys, no nulls).
- Passwords, tokens and AWS credentials never leave the computer: not in `to_public`, not in any `/api` reply, not in a
  log line or an error text, not in a `Debug` print.
- Network only while needed: no loops. Requests go out when the phone or the settings page asks, and only while an
  enabled page has the widget. Caches: Growatt status 2 s, totals 5 min, inverter settings 10 min; TCL device list
  30 s, unit states 4 s.
- An action needs a widget of that type on an enabled page with `options.control == true`; the agent validates the
  action and value and builds the payload; the phone never sends raw properties.
- Growatt settings: `ac-output-source` 0–3 (SBU Priority, Solar First, Utility First, SUB Priority), `charge-source`
  0–2 (Solar First, Solar and Utility, Only Solar), `max-charge-current` 0–100 (presets 1 5 10 15 20 25 30 100, A).
- TCL payloads (the web dashboard's): power on `{"powerSwitch":1,"healthy":1,"targetTemperature":26}` (all units: no
  `healthy`), off `{"powerSwitch":0}`; cool `{"workMode":1,"targetTemperature":26}`, fan
  `{"workMode":3,"targetTemperature":31}`, dry `{"workMode":2,"targetTemperature":16}`, heat
  `{"workMode":4,"targetTemperature":31}`, auto `{"workMode":0}`; temperature `{"targetTemperature":n}` with n a whole
  number 16–31; fan `{"windSpeed7Gear":n}` 0–7; generator `{"generatorMode":n}` 0–3. Any `targetTemperature` is sent
  with `targetCelsiusDegree` equal to it.
- The two widgets' own texts are plain English, as the Docker widget's; catalog strings get el and vi entries; `s.*`
  settings keys get en, el and vi.
- Tests and builds run on the test server (`agent-rs/scripts/remote-*.sh`, the UI container), never on the laptop.
- Commits on branch `growatt-tcl-tabs`, no AI co-author trailer, nothing pushed.

## Review Focus

1. The Mac away from the shop (growatt.lan not resolving) or the Growatt server down: the tab shows the error over the
   last readings, never a blank or broken widget.
2. A TCL session aging out mid-use (SaaS token about 30 min, AWS credentials about 1 h) or a 403 from AWS IoT: renewed
   from the 30-day base token without a password sign-in; a dead base token signs in with the password again.
3. A unit that is offline or whose shadow lacks fields: the card says Offline or shows "—", no controls on it, and "all
   units" actions skip it.
4. Rapid temperature taps: one publish with the final value after a short pause; the agent refuses anything outside
   16–31.
5. Settings changed while a token is held (new address or password): the next request uses the new settings and the old
   token is never sent to the new server.

---

### Task 1: The `growatt` and `tcl` settings

**Files:** Modify `agent-rs/crates/phonebox/src/config/mod.rs` (structs, `AgentConfig` fields, `KNOWN_FIELDS`, helpers,
tests), `config/normalize.rs`.

**Interfaces:**
- Produces `pub struct GrowattConfig { pub url: String, pub username: String, pub password: String, extra }` (JSON
  `growatt`) and `pub struct TclConfig { pub username: String, pub password: String, extra }` (JSON `tcl`), both
  `#[serde(rename_all = "camelCase", default)]` with a hand-written `Debug` that prints the password as `"***"`.
- `AgentConfig.growatt: Option<GrowattConfig>`, `AgentConfig.tcl: Option<TclConfig>`, each
  `skip_serializing_if = "Option::is_none"`; `"growatt"` and `"tcl"` in `KNOWN_FIELDS`.
- `AgentConfig::has_widget(&self, kind: &str) -> bool` (a widget of that type on an enabled page, Smart Stack cards
  included, via `widgets(true)`), `AgentConfig::widget_control(&self, kind: &str) -> bool` (one of them has
  `options.control == true`; missing is false).
- Normalize: `url` and `username` trimmed, password untouched; `growatt` dropped when url and password are empty and
  extra is empty; `tcl` dropped when username and password are empty and extra is empty.

- [ ] Tests in `config/mod.rs`: `growatt_and_tcl_are_absent_by_default` (default config JSON has neither key);
  `growatt_and_tcl_round_trip` (load → save keeps url, username and a password `" p w "` exactly);
  `blank_growatt_and_tcl_are_dropped` (`{"growatt":{"url":" ","password":""},"tcl":{"username":"","password":""}}` →
  both None; `" http://growatt.lan "` is trimmed); `growatt_and_tcl_never_reach_the_phone` (`to_public` text has no
  `growatt`, no `tcl`, no password text); `debug_never_prints_the_passwords` (`format!("{cfg:?}")` lacks both
  passwords); `widget_helpers_follow_enabled_pages` (an enabled page's `growatt` widget → `has_widget` true; only on
  a disabled page (with an enabled neighbour page) → false; `control` true/missing → `widget_control` true/false).
- [ ] RED on the server (`agent-rs/scripts/remote-test.sh config::`), implement, GREEN; commit
  "Growatt and TCL settings".

### Task 2: The Growatt client's actions

**Files:** Move `src/growatt.rs` → `src/growatt/client.rs` and `src/growatt_tests.rs` → `src/growatt/client_tests.rs`;
create `src/growatt/mod.rs` (`pub mod client;`); test fake shared as `src/growatt/fake.rs` (`#[cfg(test)]`, the
`Server` from the client tests, extended with the settings and relogin routes).

**Interfaces:**
- Consumes the existing `GrowattClient`, `DeviceStatus`, `TotalData`, `Energy`, `api_base`.
- Produces `GrowattClient::setting(&self, name: &str) -> Result<i64, String>` (`GET /growatt/inverter-settings/{name}`
  → `data.value`), `GrowattClient::set_setting(&self, name: &str, value: i64) -> Result<(), String>` (`POST` the same
  path, body `{"value":n}`, JSON content type), `GrowattClient::relogin(&self) -> Result<bool, String>`
  (`POST /growatt/relogin` → `data.connected`), `GrowattClient::sign_in(&self) -> Result<(), String>` (made public:
  signs in now). All share one request path with the 401-once retry of `read`.

- [ ] Tests in `growatt/client_tests.rs`: `a_setting_is_read_with_the_token` (GET path, `Bearer t1`, `{value:1}` →
  1); `a_setting_is_written_as_a_value` (POST body `{"value":30}`, content type JSON → Ok);
  `a_refused_setting_passes_the_message_on` (500 `"Device is not a storage device"` → `"The Growatt server answered
  500: Device is not a storage device"`); `relogin_reports_whether_the_server_reconnected` (`{connected:true}` →
  true); `a_write_with_an_expired_token_signs_in_again_once` (POST sent twice with the same body, second with `t2`).
  The 14 existing client tests keep passing after the move.
- [ ] RED, implement, GREEN (`remote-test.sh growatt::`); commit "Growatt client: inverter settings and relogin".

### Task 3: The Growatt service and its routes

**Files:** `src/growatt/mod.rs` (service), `src/growatt/service_tests.rs`; create `src/server/routes_devices.rs` (with
its tests); modify `src/server/mod.rs` (`AppState.growatt`, construction, `all_routes`), `src/lan.rs` (`SEGMENTS` gains
`"/api/growatt"`), `src/server/routes_dev.rs` only if test helpers live there.

**Interfaces:**
- Consumes Task 1's `growatt`, `has_widget`, `widget_control`; Task 2's client.
- Produces in `growatt/mod.rs`: `NOT_CONFIGURED = "Set up Growatt in Settings › General"`, `NO_WIDGET = "Add a Growatt
  widget to a page"`, `NOT_ALLOWED = "Growatt actions are off in the widget's settings"`;
  `GrowattView { configured, error: Option<String>, control, host: String, status: Option<DeviceStatus>, totals:
  Option<TotalData> }` and `GrowattSettings { error: Option<String>, ac_output_source, charge_source,
  max_charge_current: Option<i64> }` (camelCase); `pub fn setting_range(name: &str) -> Option<RangeInclusive<i64>>`;
  `Growatt::new(cfg: Arc<ConfigStore>)`, `view(&self) -> GrowattView`, `settings(&self) -> GrowattSettings`,
  `set(&self, name: &str, value: &str) -> Result<(), String>`, `relogin(&self) -> Result<bool, String>`,
  `test(&self, url: &str, username: &str, password: &str) -> String` ("Connected", or the error),
  `#[cfg(test)] expire(&self)` (marks every cache old).
- The client is kept per `url\nusername\npassword` (in memory) and rebuilt when that changes. An error keeps the last
  status and totals.
- Routes: `GET /api/growatt` → `GrowattView`; `GET /api/growatt/settings` → `GrowattSettings`;
  `POST /api/growatt/settings/{name}?value=N` → `{"ok":true}` or 400 `{"error"}`; `POST /api/growatt/relogin` →
  `{"ok":true,"connected":bool}` or 400 `{"error"}`; `POST /api/growatt-test` (local only, JSON body
  `{url,username,password}`, names in any case, as `/api/ha/test`) → `{"result"}`.

- [ ] Service tests: `nothing_is_sent_without_settings_or_a_widget` (NOT_CONFIGURED; then NO_WIDGET; zero requests);
  `the_view_has_the_readings_the_totals_and_the_host`; `reads_within_two_seconds_share_one_request`;
  `an_error_keeps_the_last_readings` (good view, `expire`, server 500 → error set, status unchanged);
  `new_settings_get_a_new_client` (url changed to a second fake: its first request is the sign-in, never
  `Bearer t1`); `actions_need_a_widget_that_allows_them` (control false → NOT_ALLOWED, no POST);
  `values_outside_the_range_are_refused` (`ac-output-source=4`, `charge-source=-1`, `max-charge-current=101`,
  `max-charge-current=abc`, `nope=1` → Err, no POST; `max-charge-current=30` → body `{"value":30}`);
  `the_settings_are_read_once_and_a_write_updates_them` (3 GETs, then none; after `set charge-source 2` →
  `charge_source == Some(2)` without a GET).
- [ ] Route tests: `the_growatt_routes_answer_with_their_shapes`; `only_the_phones_growatt_routes_are_served_over_wifi`
  (`/api/growatt` and `/api/growatt/settings` 200, `/api/growatt-test` 403); in `lan.rs`:
  `allowed_over_lan("/api/growatt/settings/x")` true, `("/api/growatt-test")` false.
- [ ] RED, implement, GREEN; `remote-check.sh`; commit "Growatt: the agent's service and routes".

### Task 4: Signing for TCL (MD5 and AWS SigV4)

**Files:** `agent-rs/crates/phonebox/Cargo.toml` (`md-5 = "0.10"`), `agent-rs/Cargo.lock`; create
`src/tcl/mod.rs` (`pub mod sigv4;` for now), `src/tcl/sigv4.rs` with its tests; `agent-rs/THIRD-PARTY-NOTICES.txt`
(`scripts/notices.sh`).

**Interfaces:**
- Produces `pub fn md5_hex(text: &str) -> String`; `pub struct AwsCredentials { pub access_key: String, pub secret:
  String, pub session_token: Option<String> }` (hand-written `Debug` without the secret and token);
  `pub fn canonical_path(path: &str) -> String` (each segment of the already-encoded path URI-encoded again, as AWS
  does for every service but S3); `pub fn sign(method: &str, url: &reqwest::Url, headers: &[(String, String)], body:
  &[u8], region: &str, service: &str, creds: &AwsCredentials, at_ms: i64) -> Vec<(String, String)>` returning the
  headers to add: `x-amz-date`, `x-amz-security-token` when there is one, `authorization`. Signed headers: `host`,
  the given headers, `x-amz-date`, `x-amz-security-token`, lower-cased and sorted.

- [ ] Tests: `md5_matches_rfc_1321` (`""` → `d41d8cd98f00b204e9800998ecf8427e`, `"abc"` →
  `900150983cd24fb0d6963f7d28e17f72`); `sigv4_matches_awss_get_vanilla_example` (GET `https://example.amazonaws.com/`,
  us-east-1, service `service`, `AKIDEXAMPLE` / `wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY`, 2015-08-30T12:36:00Z →
  `Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date,
  Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31`);
  `sigv4_signs_the_session_token_and_the_body` (a POST with a JSON body, a session token and `content-type`: the
  token header is added and signed; the signature equals one computed independently with Python's `hmac`/`hashlib`
  from the same inputs, pasted as a literal); `the_canonical_path_encodes_each_segment_again`
  (`/topics/%24aws%2Fthings%2Fac1%2Fshadow%2Fupdate` → `/topics/%2524aws%252Fthings%252Fac1%252Fshadow%252Fupdate`,
  `/things/ac-1/shadow` unchanged).
- [ ] RED, implement, GREEN; `scripts/notices.sh`, `python3 -I tools/check-notices.py`; commit "TCL: MD5 and SigV4".

### Task 5: The TCL cloud client

**Files:** create `src/tcl/client.rs`, `src/tcl/client_tests.rs`, `src/tcl/fake.rs` (`#[cfg(test)]`: one FakeHttp
playing TCL's login, cloud-URL, cloud and device hosts, Cognito and AWS IoT, recording requests, with switches for
"refuse token X", "IoT 403 next", "login refuses").

**Interfaces:**
- Consumes Task 4.
- Produces `pub struct TclEndpoints { pub login: String, pub cloud_urls: String, pub cognito: Option<String>, pub iot:
  Option<String> }` and `TclEndpoints::tcl()` (login `https://pa.account.tcl.com/account/login?clientId=54148614`,
  cloud URLs `https://prod-center.aws.tcljd.com/v3/global/cloud_url_get`; Cognito and IoT None = from the region:
  `https://cognito-identity.{region}.amazonaws.com/`, `https://data-ats.iot.{region}.amazonaws.com`);
  `const APP_ID: &str = "wx6e1af3fa84fbe523"`; `pub struct TclDevice { pub id: String, pub name: String, pub online:
  bool }`; `TclClient::new(username: &str, password: &str, endpoints: TclEndpoints, now_ms: Arc<dyn Fn() -> i64 +
  Send + Sync>) -> TclClient`; `devices(&self) -> Result<Vec<TclDevice>, String>`; `reported(&self, id: &str) ->
  Result<Value, String>` (the shadow's `state.reported`); `publish(&self, id: &str, properties: Map<String, Value>) ->
  Result<(), String>`. No `Debug`.
- Requests, as `TclClient.ts`: login POST with headers `th_platform: android`, `th_version: 4.8.1`, `th_appbulid: 830`,
  `user-agent: Android`, JSON body `{username, password: md5_hex(pw), equipment:2, osType:1, clientVersion:"4.8.1",
  osVersion:"6.0", deviceModel:"AndroidAndroid SDK built for x86", captchaRule:2, channel:"app"}`, success = HTTP 200
  and `status == 1` (keep `token`, `user.username`, `user.countryAbbr`); cloud URLs POST `{ssoId, ssoToken, appId}`,
  `code == 0` → `cloud_url`, `device_url`, `cloud_region`; refresh POST `{cloud_url}/v3/auth/refresh_tokens`
  `{ssoToken, userId, appId}` → `saasToken`, `cognitoToken`, `cognitoId`; Cognito POST with
  `X-Amz-Target: AWSCognitoIdentityService.GetCredentialsForIdentity`, `content-type: application/x-amz-json-1.1`,
  `{IdentityId, Logins:{"cognito-identity.amazonaws.com": cognitoToken}}` → `Credentials` (Expiration in epoch
  seconds); device list POST `{device_url}/v3/user/get_things` body `{}` with `platform: android`, `appversion: 5.4.1`,
  `thomeversion: 4.8.1`, `accesstoken`, `countrycode`, `accept-language: en`, `timestamp` (ms), `nonce` (12 random
  base-36 characters), `sign: md5_hex(timestamp + nonce + saasToken)`; shadow GET `/things/{id}/shadow` and publish
  POST `/topics/{urlencoded "$aws/things/{id}/shadow/update"}?qos=1` with `{"state":{"desired":…},"clientToken":
  "mobile_<unix s>"}`, both SigV4 for service `iotdata` in the cloud region.
- Session: the short-lived part is renewed from the base token when older than 25 min or within 5 min of the AWS
  credentials' expiry; a failed renewal signs in with the password once. A device-list `code != 0` or an IoT 401/403
  renews once and retries once. The region must match `^[a-z0-9-]+$` and the cloud and device URLs must be https;
  a thing id must match `^[A-Za-z0-9_.:-]+$` (else Err, no request). Errors never carry the password, its MD5, a token
  or a key.

- [ ] Tests: `signing_in_runs_the_whole_chain_once` (order and bodies of the five requests; a second `devices()` sends
  only `get_things`; `sign` checks against `md5_hex`); `devices_have_names_and_online_flags` (nickName, else
  deviceName, else id; `isOnline` `"1"`/`"0"`); `the_shadow_read_is_signed_for_iotdata` (credential scope
  `…/eu-central-1/iotdata/aws4_request`, session token header); `a_publish_sends_the_desired_state_with_the_celsius_copy`
  (raw path `/topics/%24aws%2Fthings%2Fac1%2Fshadow%2Fupdate`, query `qos=1`, `targetCelsiusDegree` 26);
  `an_old_session_is_renewed_from_the_base_token` (clock + 26 min → cloud URLs, refresh, Cognito; no login);
  `a_refused_iot_call_renews_and_retries_once` (403, then OK; 403 twice → Err, two IoT calls);
  `a_dead_base_token_signs_in_with_the_password_again`; `a_wrong_password_says_so` (`status` 0, `msg` → `"TCL refused
  the sign-in: <msg>"`, without the password or its MD5); `a_region_that_is_not_a_region_is_refused`;
  `a_thing_id_with_a_slash_is_refused`.
- [ ] RED, implement, GREEN (`remote-test.sh tcl::`); commit "TCL: the cloud client".

### Task 6: The TCL service, its actions and routes

**Files:** `src/tcl/mod.rs` (service), create `src/tcl/actions.rs` (+ tests), `src/tcl/service_tests.rs`; modify
`src/server/routes_devices.rs`, `src/server/mod.rs` (`AppState.tcl`), `src/lan.rs` (`"/api/tcl"`).

**Interfaces:**
- Consumes Tasks 1 and 5.
- Produces `actions::properties(action: &str, value: &str, all: bool) -> Result<Map<String, Value>, String>` (the
  Global Constraints table; actions `power` on|off, `mode` cool|fan|dry|heat|auto, `temp` 16–31 whole, `fan` 0–7,
  `generator` 0–3); `NOT_CONFIGURED = "Set up TCL in Settings › General"`, `NO_WIDGET = "Add a TCL widget to a page"`,
  `NOT_ALLOWED = "TCL actions are off in the widget's settings"`; `TclUnit { id, name, online, power: Option<bool>,
  mode, fan, generator: Option<i64>, target, inside, outside: Option<f64> }` from `powerSwitch`, `workMode`,
  `windSpeed7Gear`, `generatorMode`, `targetTemperature`, `currentTemperature`, `externalUnitTemperature`;
  `TclView { configured, error, control, units: Vec<TclUnit> }` (camelCase); `Tcl::new(cfg)`,
  `Tcl::with_endpoints(cfg, TclEndpoints)`, `view(&self) -> TclView`, `act(&self, unit: &str, action: &str, value:
  &str) -> Result<usize, String>` (unit = an id from the current list, or `all` = every online unit; returns how many
  publishes went out), `test(&self, username: &str, password: &str) -> String` ("Connected · N units", or the error),
  `#[cfg(test)] expire(&self)`.
- Offline units are listed without a shadow read. The client is kept per `username\npassword`. An error keeps the last
  units. A publish marks the states stale.
- Routes: `GET /api/tcl` → `TclView`; `POST /api/tcl/{unit}/{action}?value=V` → `{"ok":true,"sent":n}` or 400
  `{"error"}`; `POST /api/tcl-test` (local only, `{username,password}`) → `{"result"}`.

- [ ] Tests: `every_action_has_the_web_dashboards_payload` (each row of the table, all and single power on);
  `values_outside_the_ranges_are_refused` (`temp` 15, 32, 24.5; `fan` 8; `generator` 4; `mode` turbo; `power` maybe;
  `swing` 1); service: `nothing_is_sent_without_settings_or_a_widget`; `the_view_lists_units_with_their_state`
  (offline unit: no shadow read, None fields); `reads_share_the_list_and_the_states` (two views → one `get_things`,
  one shadow per online unit); `an_error_keeps_the_last_units`; `an_unknown_unit_is_refused`;
  `all_units_means_every_online_unit` (publishes = online count, no `healthy`); `actions_need_a_widget_that_allows_them`;
  `after_an_action_the_states_are_read_again`; routes: `the_tcl_routes_answer_with_their_shapes`,
  `only_the_phones_tcl_routes_are_served_over_wifi`.
- [ ] RED, implement, GREEN; `remote-check.sh`; commit "TCL: the agent's service and routes".

### Task 7: The Growatt widget

**Files:** create `android/app/src/main/assets/www/widgets/growatt.js`; modify `www/main.js` (import), `www/style.css`
(`gw-` block), `agent-rs/assets/widgets.json`, `www/i18n.js` (catalog el/vi), `tools/pages/demo.js` (answers for
`/api/growatt`, `/api/growatt/settings`, their POSTs, `/api/growatt-test`); create `tools/ui-tests/growatt.spec.js`.

**Interfaces:** consumes Task 3's routes and shapes. Catalog: `{"type":"growatt","category":"Web & data","name":
"Growatt inverter","icon":"☀️","w":12,"h":6,"minW":4,"minH":3,"options":[title "Solar", control "Allow changing
inverter settings from the phone" default true]}`.

- The widget polls `/growatt` every 3 s. Header: title, a badge Live (source `grobro`, reading < 30 s old) / Stale
  (`grobro`, older) / Cloud, and the reading's age. Four tiles: Solar (W; PV1 and PV2 W), Battery (% and W, "Charging"
  when negative, "Discharging" when positive, V), Load (W, %), Grid (W, V, Hz). Today row: Solar, Discharged, Imported,
  Load in kWh with the lifetime total small. With `control`: buttons AC output, Charge source, Max charge (labelled
  with the current value from `/growatt/settings`, loaded on first render and after each change) and Relogin; a
  button opens a sheet over the widget with the choices (current one marked; max charge: the presets and a number
  field 0–100); a choice POSTs and toasts the result. An error shows as a line under the readings, which stay.

- [ ] UI tests: `the_inverter_card_shows_the_flow_today_and_actions`; `choosing_an_ac_output_source_posts_it` (sheet
  has 4 choices, current marked; "Utility First" → POST `value=2`, button then says Utility First);
  `max_charge_offers_the_presets_and_a_custom_value`; `without_control_there_are_no_actions`;
  `an_error_keeps_the_readings` (fetch wrapped in the page to answer `/api/growatt` with `error` set);
  `a_small_widget_stays_legible` (4×3: tiles visible, nothing spills). Plus `panel.spec.js`, `i18n-coverage.spec.js`,
  `widget-requests.spec.js`.
- [ ] RED in the container (`/Volumes/storage/tmp/tc-ui.sh growatt.spec.js`), implement, GREEN; commit
  "Growatt widget".

### Task 8: The TCL widget

**Files:** create `www/widgets/tcl.js`; modify `www/main.js`, `www/style.css` (`tc-` block), `widgets.json`,
`www/i18n.js`, `tools/pages/demo.js` (`/api/tcl`, `POST /api/tcl/…`, `/api/tcl-test`); create
`tools/ui-tests/tcl.spec.js`.

**Interfaces:** consumes Task 6's routes. Catalog: `{"type":"tcl","category":"Web & data","name":"TCL air
conditioners","icon":"❄️","w":12,"h":6,"minW":4,"minH":3,"options":[title "Air conditioners", control "Allow
controlling the units from the phone" default true]}`.

- The widget polls `/tcl` every 5 s. Header: title and "N of M on". With `control`, an all-units bar: Power Off / On,
  Mode Cool / Fan / Auto, Generator Off / 1 / 2 / 3, Fan Mid (4) / Max (7); a value is highlighted when every online
  unit has it. One card per unit: name, inside temperature large, outside small; with `control`: power, target −/+
  (one POST 400 ms after the last tap, showing the pending value), Cool / Fan, fan Low (2) / Max (7), generator
  Off / 1 / 2 / 3. A value just sent is shown until the unit reports it or 8 s pass. Offline units say Offline and
  have no controls. An error shows as a line, the cards stay.

- [ ] UI tests: `the_units_show_their_state_and_offline_ones_have_no_controls`;
  `the_power_button_switches_a_unit` (POST `/api/tcl/<id>/power?value=on`); `quick_temperature_taps_send_one_change`
  (two + taps → one POST, value target + 2); `the_all_units_bar_sets_the_generator` (POST
  `/api/tcl/all/generator?value=2`); `without_control_there_are_no_buttons`; `a_small_widget_stays_legible`. Plus the
  three shared specs.
- [ ] RED, implement, GREEN; commit "TCL widget".

### Task 9: Settings, docs and the whole check

**Files:** `agent-rs/assets/settings.html` (two cards in General after Home Assistant: Growatt address, username
placeholder `admin`, password, Test; TCL account email, password, Test; bound with `data-bind`), `www/i18n.js` (`s.growatt.*`,
`s.tcl.*` in en/el/vi), `PRIVACY.md`, `CHANGELOG.md` (Unreleased), `docs/GUIDE.md`; a settings UI test in
`tools/ui-tests/growatt.spec.js` / `tcl.spec.js` (`the_settings_page_tests_the_typed_values`: Test posts the unsaved
form values to `/api/growatt-test` and `/api/tcl-test` and shows the result).

- [ ] The settings tests RED, implement, GREEN.
- [ ] PRIVACY rows: Growatt (your server only: sign-in and readings every 3 s while the widget is on screen, totals
  every 5 min, inverter settings when you open them); TCL (TCL's account and cloud hosts and AWS Cognito and IoT in
  your account's region: sign-in when needed, the unit list every 30 s and states every 5 s while the widget is on
  screen). CHANGELOG and GUIDE.
- [ ] Whole check on the server: `remote-check.sh`, `remote-cross-check.sh`, parity phases 1, 5 (with the LAN fixture)
  and 6, `tools/check-release.py --final` with `test-release-metadata.py`, the UI suite in the container; commit
  "Growatt and TCL: settings and docs".
