# Rust agent plan 4: Wi-Fi Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Rust agent serves the phone over Wi-Fi exactly as the C# agent does: HTTPS on the Wi-Fi port with the
existing `lan-cert.pfx` (so every pairing survives the switch), the Wi-Fi request authorization, pairing codes with the
QR link, and the `/api/pair*` and `/api/lan` routes.

**Architecture:** Pure-Rust building blocks first (a small DER reader/writer, PKCS#12 read and write, the self-signed
P-256 certificate, the authorization rules, pairing codes), all compiled and tested on every target. The TLS server
(rustls + tokio-rustls, ring) is the only part behind the `tls` cargo feature: it serves a second axum router, built
from the same routes with a Wi-Fi guard instead of the local one, under the same `Stop` and grace period.

**Tech Stack:** Rust 1.98, axum 0.8 (custom `serve::Listener`), rustls 0.23 / tokio-rustls 0.26 (ring, feature `tls`),
RustCrypto `p256` (ecdsa, pkcs8), `sha1`, `sha2`, `hmac`, `des`, `rc2`, `aes`, `cbc`, `pbkdf2`, `qrcode` (svg),
`getrandom`, `base64`.

**Spec:** `docs/superpowers/specs/2026-11-20-rust-agent-design.md` (Wi-Fi TLS at lines 109-112; switch-over and fork
departures at 182-217). The C# agent at `bb72345` is the behavioral spec. Survey with file:line references:
`/tmp/plan4-survey.md` (copied into the plan workspace as `survey.md` at Task 1).

## Global Constraints

- The existing `<config folder>/lan-cert.pfx` must be read so the fingerprint, and every phone pairing, survives; a new
  one is made only when the file is missing or unreadable, with the C# layout (ECDSA P-256 / SHA-256, subject
  `CN=PhoneBox agent (<machine name>)`, BasicConstraints CA:false, KeyUsage digitalSignature, EKU serverAuth, all
  non-critical, no SAN/SKI/AKI, 8-byte serial, valid from now − 1 day to now + 20 years).
- `lan-cert.pfx` is written in the .NET layout (null password, MAC HMAC-SHA1 2000 iterations 20-byte salt, cert bag in
  EncryptedData `pbeWithSHAAnd3-KeyTripleDES-CBC` 2000 iterations, `pkcs8ShroudedKeyBag` 3DES 2000 iterations in a
  Data ContentInfo, `localKeyID` = `01 00 00 00`), owner-only (0600) on macOS/Linux, so the C# agent still reads it.
- The reader accepts both PKCS#12 empty-password conventions (zero-length first, then the BMP `00 00`), 3DES, RC2-40
  and RC2-128 bags, PBES2 with AES-128/256-CBC and PBKDF2-HMAC-SHA1/SHA256, and SHA-1 / SHA-256 MACs.
- Fingerprint: SHA-256 of the certificate DER, 64 lower-case hex characters. The TLS server sends exactly that DER as
  the only certificate (no SNI resolver).
- Wi-Fi listener: IPv4 `0.0.0.0:<lan.port>`, only when `lan.enabled && lan.port != local port` at startup; TLS 1.2 and
  1.3, ALPN `http/1.1`, 10 s handshake timeout; HTTP/1.1 upgrades (the trackpad WebSocket) work. A failed bind or a
  certificate that cannot be written ends the agent, as in C#.
- Wi-Fi authorization (live config, every request): `lan.enabled`, no Origin header (an unreadable one counts as
  present), path on the allow-list; then the phone key (`Authorization: Bearer`, only while the two keys differ), the
  webhook key (header or `?token=`, webhook paths only), or a keyless `POST /api/pair/claim`. Denied → 403, empty body.
  Constant-time comparison over UTF-8 bytes; several Authorization headers are joined with ",".
- Claim and webhook requests get `Cache-Control: no-store` only (no `X-Frame-Options`, no phone registry, no app
  version); phone requests get the local tail with via `"Wi-Fi"`. No `/agent/api` rewrite on the Wi-Fi port.
- Pairing: 22-character base64url codes (16 random bytes), 5 minutes, single use, dropped after 10 wrong tries, the
  1 KB claim body cap, Wi-Fi re-checked after the body arrives.
- The pairing link base stays upstream's `https://ersinkoc.github.io/phonebox/pair/#v=1&…` (the phone's intent filter);
  the APK link uses the configured `updates.repo` and the Rust version (spec departure).
- Never answer `GET /api/pair` with `enabled:false` because of a Rust-side failure: the phone then forgets every
  pairing with that computer name.
- Everything except the TLS server compiles without the `tls` feature (the cross-check builds Windows/macOS that way).
- Tests run on the server (`agent-rs/scripts/remote-*.sh`); the Mac/phone exit check is the owner-approved laptop
  exception (short-lived, stopped by PID, installed agent restored).
- Never print keys, tokens or private keys in logs, test output or commits; test fixtures use throwaway keys.

## Review Focus

1. A `lan-cert.pfx` written by .NET on Windows (RC2-40 or PBES2 bags, a key without its public point): it must load
   with the same fingerprint, never silently regenerate. Tests use OpenSSL-made fixtures in those formats.
2. An Origin header with non-ASCII bytes, several Authorization headers, a `Bearer` token with a different case or an
   extra space: all must be refused on the Wi-Fi port.
3. A TLS client that connects and never finishes the handshake (or a slow-loris body on `/api/pair/claim`) must not
   block other phones or hold the code: handshakes run in their own tasks with a timeout; the claim body is capped.
4. Stopping or restarting the agent with a phone streaming over Wi-Fi: both listeners stop within the grace period and
   release both ports for the next instance.
5. Turning Wi-Fi off in the settings while the agent runs: Wi-Fi requests are refused at once (port still bound until
   restart), `/api/lan` says `restartNeeded`, and `GET /api/pair` answers `enabled:false` only because of that setting.

---

### Task 1: DER helpers and PKCS#12 read/write

**Files:** Create `agent-rs/crates/phonebox/src/der.rs`, `pkcs12.rs`, `src/testdata/lan/` (fixtures); modify
`lib.rs`, `Cargo.toml` (p256, sha1, hmac, des, rc2, aes, cbc, pbkdf2).

**Interfaces:**
- Produces: `der::{Reader (tag, length, value; nested reads; OID/INTEGER/OCTET STRING/BIT STRING helpers), Writer
  (sequence, set, context tags, OID, INTEGER, OCTET STRING, BIT STRING, UTCTime/GeneralizedTime, UTF8String)}`;
  `pkcs12::{Pfx { cert_der: Vec<u8>, key_pkcs8: Vec<u8> }, read(bytes) -> Result<Pfx, String>, write(&Pfx) ->
  Vec<u8>}`. `key_pkcs8` is always re-encoded through `p256::SecretKey` so it carries the public key.

- [ ] **Step 1: Fixtures (on the server).** Run the C# agent once with `{"lan":{"enabled":true,"port":<free>}}` in a
  temporary config folder and keep its `lan-cert.pfx` as `testdata/lan/dotnet.pfx`; make `openssl-rc2.pfx`
  (`openssl pkcs12 -export -legacy -certpbe PBE-SHA1-RC2-40 -keypbe PBE-SHA1-3DES -macalg sha1 -passout pass:`) and
  `openssl-aes.pfx` (`-certpbe AES-256-CBC -keypbe AES-256-CBC -macalg sha256 -passout pass:`) from a throwaway P-256
  key and self-signed cert; record each certificate's SHA-256 (`openssl x509 -fingerprint -sha256`) in
  `testdata/lan/README.md`. Copy the survey to the plan workspace.
- [ ] **Step 2: Tests (RED).** `der` round trips; `the_dotnet_file_reads_with_its_fingerprint`,
  `rc2_and_aes_files_read_with_their_fingerprints`, `the_key_matches_the_certificate` (sign with the key, verify with
  the certificate's public key), `a_written_file_reads_back_and_has_the_dotnet_layout` (re-parse: MAC SHA-1 / 2000 /
  20-byte salt, 3DES bags, `localKeyID`), `both_empty_password_conventions_are_accepted`, `a_tampered_file_fails_the_mac`,
  `garbage_and_truncated_files_are_errors_not_panics`.
- [ ] **Step 3: Implement.** RFC 7292 appendix B KDF (SHA-1 and SHA-256), the MAC check before decrypting, the PBE
  schemes above, bag walking (ignore unknown attributes and bags), the writer with random salts/IVs (`getrandom`).
- [ ] **Step 4:** Run → PASS; `remote-check.sh`, `remote-cross-check.sh` clean. **Commit** `Rust agent: PKCS#12`.

### Task 2: The Wi-Fi certificate

**Files:** Create `lan_cert.rs`; modify `lib.rs`.

**Interfaces:**
- Consumes: Task 1, `state::machine_name()`, `app_paths::{create_private, restrict_to_owner}`.
- Produces: `lan_cert::{LanCert { der, key_pkcs8, fingerprint }, certificate(config_dir: &Path) -> io::Result<LanCert>,
  fingerprint(der) -> String, create(machine_name, now) -> Pfx}`.

- [ ] **Step 1: Tests (RED).** From `PairingTests.LanCertificateIsolatesToConfigDirectoryAndCollidingLanPortIsDetected`
  (the certificate half: "corrupt" is replaced, the fingerprint is stable across loads, mode 0600, the file lives in
  the given folder); `a_new_certificate_has_the_csharp_layout` (walk the DER: v3, 8-byte positive serial,
  ecdsa-with-SHA256, CN, validity −1 day / +20 years with GeneralizedTime from 2050, the three extensions non-critical,
  no others); `the_dotnet_fixture_keeps_its_fingerprint` (copied into a temp folder, loaded, file unchanged);
  `an_unreadable_file_is_replaced_with_a_warning`.
- [ ] **Step 2: Implement** from `Lan.cs:17-39`: load (restrict to owner first), else create, write with
  `create_private`, return. A load failure logs a warning naming the reason ("phones paired over Wi-Fi only must pair
  again") — a Rust addition, ledgered.
- [ ] **Step 3:** Run → PASS; cross-check clean. **Commit** `Rust agent: Wi-Fi certificate`.

### Task 3: Wi-Fi authorization

**Files:** Modify `lan.rs`, `agent-rs/scripts/port-status.py` (assign `PairingTests` to 4, and the two pure allow-list
tests `CameraFeedsTests.ThePhoneMayAskForCameraPicturesOverWifi`, `WidgetFeedProxyTests.ProxyEndpointStaysLocalAndOffLanAllowlist`).

**Interfaces:**
- Produces: `lan::{matches(header: Option<&str>, token) -> bool, matches_query(given: Option<&str>, token) -> bool,
  allowed_over_lan(path) -> bool, is_pairing_claim(method, path) -> bool, is_webhook(path) -> bool, LanAccess {Denied,
  Phone, Webhook, PairingClaim}, authorize_request(lan: &LanConfig, method, path, origin_present: bool, authorization:
  Option<&str>, query_token: Option<&str>) -> LanAccess}`.

- [ ] **Step 1: Tests (RED).** `only_the_phone_api_is_reachable_over_wifi` (the 27 rows), `the_webhook_key_opens_only_webhooks`,
  `tokens_are_compared_exactly`, `the_phone_may_ask_for_bands_over_wifi`, `the_phone_may_ask_for_camera_pictures_over_wifi`,
  `proxy_endpoint_stays_local_and_off_lan_allowlist`, `only_the_claim_needs_no_key_over_wifi`,
  `disabled_lan_rejects_all_tiers_and_webhook_token_stays_separate_from_phone_token`, the authorization half of
  `a_shared_phone_and_webhook_key_is_not_a_phone_key_and_cannot_be_saved`; plus `bearer_is_exact` ("bearer x",
  "Bearer  x", "Bearer x,Bearer x" refused) and case-insensitive paths.
- [ ] **Step 2: Implement** from `Lan.cs:76-123` (exact and segment lists verbatim from the survey §1.1).
- [ ] **Step 3:** Run → PASS. **Commit** `Rust agent: Wi-Fi authorization`.

### Task 4: Pairing codes, link, check and QR

**Files:** Create `pairing.rs`; modify `lib.rs`, `Cargo.toml` (`qrcode`, svg only).

**Interfaces:**
- Produces: `pairing::{LIFETIME_MS = 300_000, MAX_FAILURES = 10, PairingCodes::new(), start(now_ms) -> (String, i64,
  i64), cancel(attempt), claim(Option<&str>, now_ms) -> bool, claims() -> i64, read_claim_code(content_type:
  Option<&str>, body: &[u8]) -> Option<String>, link(pc, fingerprint, port, hosts, code) -> String, check(fingerprint)
  -> String, svg(text) -> String}`.

- [ ] **Step 1: Tests (RED).** `a_code_works_once_and_counts_the_claim`, `an_expired_cancelled_or_replaced_code_fails`,
  `a_late_cancel_from_an_older_dialog_leaves_the_new_pairing`, `too_many_wrong_codes_drop_the_current_one`,
  `the_link_keeps_everything_in_the_fragment`, `the_check_code_is_the_start_of_the_fingerprint`,
  `the_qr_code_is_an_svg` (starts with `<svg`, has a `viewBox`, no `<script`), `a_claim_body_that_is_not_a_json_code_is_refused_without_throwing`
  (the 7 rows), `a_json_claim_body_gives_its_code`.
- [ ] **Step 2: Implement** from `Pairing.cs` (`Uri.EscapeDataString` = RFC 3986 unreserved kept, upper-case hex);
  JSON body: `application/json` or `+json`, charset absent or UTF-8 (others refused, ledgered), property name in any
  case, a string of 1–64 UTF-16 units; QR ECC M, 8 px modules, 4-module quiet zone, black on white, XML declaration
  dropped.
- [ ] **Step 3:** Run → PASS. **Commit** `Rust agent: pairing codes and QR`.

### Task 5: The pairing routes

**Files:** Create `server/routes_pair.rs`; modify `server/mod.rs` (`AppState.wifi: Arc<lan::Wifi>` with the startup
port, `Option<LanCert>`, `PairingCodes`), `phonebox-agent/src/main.rs` (build it).

**Interfaces:**
- Consumes: Tasks 2–4.
- Produces: `GET /api/pair`, `POST /api/pair/start`, `POST /api/pair/cancel?attempt=` (required `long`), `GET
  /api/pair/status`, `POST /api/pair/claim` (`claim_pairing(headers, body: Body, cfg, wifi) -> Response`, streaming
  body), `GET /api/lan`; JSON key order as C# (survey §2).

- [ ] **Step 1: Tests (RED).** `a_claim_that_outlives_wifi_being_turned_off_is_refused_and_keeps_the_code` (gated
  body), `the_last_of_repeated_lan_members_is_the_one_checked` (3 rows, both halves), the remaining halves of tests 16
  and 19 (`lan_keys_shared`, `ConfigStore::save` of equal keys, port collisions); route tests: `/api/pair` enabled and
  disabled shapes, `/api/pair/start` disabled (`configured`) and enabled (keys in order, `expiresAt`, `attempt`,
  `check`, `link` with the code, `qr`/`appQr` SVGs, `app` from `updates.repo`), cancel binding (400 without
  `attempt`), status counting a claim, claim 403 for a declared length over 1024 / a wrong code / Wi-Fi off, 200 with
  `{enabled,pc,port,token,fingerprint,hosts}`, `/api/lan` with `restartNeeded` and `fingerprint: null` when inactive.
- [ ] **Step 2: Implement** from `Program.cs:250-306, 1001-1023`.
- [ ] **Step 3:** Run → PASS; `phase1.json`..`phase3.json` still pass. **Commit** `Rust agent: pairing routes`.

### Task 6: The Wi-Fi router and guard

**Files:** Modify `server/guard.rs` (shared tail with a `via` parameter; `apply_local`, `apply_wifi`), `server/mod.rs`
(`build_wifi_router`: same routes, no rewrite, timings, Wi-Fi guard), `server/routes_state.rs` (stream log via from a
request extension).

**Interfaces:**
- Consumes: Task 3.
- Produces: `server::build_wifi_router(state) -> axum::Router`, `server::Via` (request extension: `"USB/local"` /
  `"Wi-Fi"`).

- [ ] **Step 1: Tests (RED)** (oneshot on the Wi-Fi router, no TLS): no key → 403 empty; the phone key → 200 with
  `Cache-Control: no-store` and `X-Frame-Options: DENY`, the phone registered as `"Wi-Fi"`; a non-ASCII Origin with the
  phone key → 403; `/api/settings` with the phone key → 403; the keyless claim gets `no-store` and no
  `X-Frame-Options` and does not touch the registry; Wi-Fi turned off in the live config → 403; `/agent/api/state` →
  403 (no rewrite); the stream logs `"Wi-Fi"`.
- [ ] **Step 2: Implement** from `Program.cs:195-246`.
- [ ] **Step 3:** Run → PASS. **Commit** `Rust agent: Wi-Fi router and guard`.

### Task 7: The TLS listener and two servers under one stop

**Files:** Create `server/tls.rs` (`#[cfg(feature = "tls")]`); modify `server/mod.rs` (`serve_wifi`), `Cargo.toml`
(optional `rustls`, `tokio-rustls` in `tls`), `phonebox-agent/src/main.rs` (certificate and listener at startup).

**Interfaces:**
- Consumes: Tasks 2, 5, 6.
- Produces: `server::tls::{server_config(&LanCert) -> Arc<rustls::ServerConfig>, TlsListener (axum::serve::Listener)}`,
  `server::serve_wifi(listener: TcpListener, config, state, grace)`; main binds `0.0.0.0:<lan.port>` when enabled and
  different from the local port, runs both servers, stops both on `Stop`.

- [ ] **Step 1: Tests (RED)** (real sockets, a rustls client that pins the fingerprint): `/api/state` with the phone
  key → 200 over TLS 1.2 and 1.3; without → 403; the trackpad upgrade over TLS answers 101; an idle TCP client does not
  block a second phone's handshake and is dropped after the (test-shortened) timeout; `Stop` ends both servers and the
  ports can be bound again.
- [ ] **Step 2: Implement:** `with_single_cert`, ALPN `http/1.1`, ring provider explicitly; TCP accept loop spawning
  handshakes with the timeout, finished streams through a channel to the `Listener`; graceful shutdown as `serve`.
- [ ] **Step 3:** Run → PASS; cross-check clean (without `tls`). **Commit** `Rust agent: Wi-Fi TLS listener`.

### Task 8: Exit — Wi-Fi parity, the phone over Wi-Fi

**Files:** Modify `tools/parity/parity.py` (+ `test_parity.py`: per-agent Wi-Fi port, the fixture certificate copied
next to each config, `"wifi": true` entries over HTTPS without verification, `{lanport}` placeholder), create
`tools/parity/fixtures/lan.json`, `tools/parity/fixtures/lan-cert.pfx` (the throwaway `dotnet.pfx`),
`tools/parity/corpus/phase4.json`; modify `agent-rs/scripts/mac-phone-check.sh` (copy `lan-cert.pfx`, compare
`/api/pair` with the installed agent), `.github/workflows/rust.yml` (phase-4 step).

- [ ] **Step 1: Corpus:** `/api/pair` (identical fingerprint proves the PKCS#12 read), `/api/lan`, `/api/pair/start`
  (shape-checked), `/api/pair/status`, cancel without `attempt`, local claim with a wrong code; over Wi-Fi: no key,
  the phone key on `/api/state` (ignore live parts), the webhook key on `/api/state`, an Origin, `/api/settings` with
  the phone key, the keyless claim with a wrong code. `remote-parity.sh` on all four corpora → all pass.
- [ ] **Step 2: Phone check (laptop exception):** with the owner's config and `lan-cert.pfx`, the Rust agent replaces
  the installed one on 8765/8766; `GET /api/pair` matches the installed agent's `pc`, `fingerprint`, `token`; the phone
  (paired with the C# agent) shows the panel over Wi-Fi without re-pairing and drives volume, media and a shortcut
  test; over USB if a cable is attached (else ledgered). The installed agent is restored and answers again.
- [ ] **Step 3:** `port-status.py --check 4` clean; full suite, cross-check, all corpora green. **Commit** `Rust agent:
  plan 4 exit: Wi-Fi parity and the phone over Wi-Fi`.
