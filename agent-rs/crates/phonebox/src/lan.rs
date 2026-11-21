//! Port of `Lan.cs` without the certificate (see [`crate::lan_cert`]): the addresses the phone can try and the Wi-Fi
//! request authorization.

/// Addresses the phone can try: private LAN IPv4 addresses first, then Tailscale (100.64.0.0/10).
pub fn addresses() -> Vec<String> {
    use crate::net_adapters::AdapterKind;
    let mut lan: Vec<String> = Vec::new();
    let mut tailscale: Vec<String> = Vec::new();
    for ni in crate::net_adapters::all() {
        if !ni.up || ni.kind == AdapterKind::Loopback {
            continue;
        }
        let desc = format!("{} {}", ni.description, ni.name);
        let is_tailscale = desc.to_lowercase().contains("tailscale");
        if !is_tailscale && should_skip_adapter(&desc, ni.has_ipv4_gateway) {
            continue;
        }
        for ip in &ni.ipv4 {
            let [b0, b1, ..] = ip.octets();
            if b0 == 100 && (64..=127).contains(&b1) {
                tailscale.push(ip.to_string());
            } else if b0 == 10 || (b0 == 172 && (16..=31).contains(&b1)) || (b0 == 192 && b1 == 168)
            {
                lan.push(ip.to_string());
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    lan.into_iter()
        .chain(tailscale)
        .filter(|a| seen.insert(a.clone()))
        .collect()
}

/// Virtual adapters (Hyper-V, WSL, VMware, Bluetooth PAN) are skipped unless one owns an IPv4 default gateway: a
/// Hyper-V external switch takes over the physical NIC's address and route.
pub fn should_skip_adapter(desc: &str, has_ipv4_gateway: bool) -> bool {
    let d = desc.to_lowercase();
    !has_ipv4_gateway
        && [
            "virtual",
            "vethernet",
            "hyper-v",
            "wsl",
            "vmware",
            "bluetooth",
        ]
        .iter()
        .any(|w| d.contains(w))
}

/// The Wi-Fi side as the agent started it (`lanCfg` and `lanCert` in `Program.cs`): `lan.port` at startup, the
/// certificate when the Wi-Fi listener runs, and the pairing codes.
pub struct Wifi {
    pub port: u16,
    /// Some when Wi-Fi was on, with its own port, at startup: the Wi-Fi listener serves this certificate.
    pub cert: Option<crate::lan_cert::LanCert>,
    pub codes: crate::pairing::PairingCodes,
}

impl Wifi {
    /// Wi-Fi as `startup` sets it: the certificate is loaded (or made) only when Wi-Fi is on with a port other than the
    /// local one. An error (a certificate that cannot be written) ends the agent, as in C#.
    pub fn start(
        startup: &crate::config::LanConfig,
        local_port: u16,
        config_dir: &std::path::Path,
    ) -> std::io::Result<Wifi> {
        let port = startup.port.clamp(1, 65535) as u16;
        let cert = if startup.enabled && port != local_port {
            Some(crate::lan_cert::certificate(config_dir)?)
        } else {
            None
        };
        Ok(Wifi {
            port,
            cert,
            codes: crate::pairing::PairingCodes::new(),
        })
    }

    /// No Wi-Fi listener (tests, and the state before `main` sets Wi-Fi up).
    pub fn off(port: u16) -> Wifi {
        Wifi {
            port,
            cert: None,
            codes: crate::pairing::PairingCodes::new(),
        }
    }
}

/// What a request on the Wi-Fi port may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanAccess {
    Denied,
    Phone,
    Webhook,
    PairingClaim,
}

/// `CryptographicOperations.FixedTimeEquals`: false at once for different lengths, else no early exit.
pub(crate) fn fixed_time_equals(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Authorization: Bearer <token>`, exactly ("Bearer" with that case and one space), compared in constant time.
pub fn matches(header: Option<&str>, token: &str) -> bool {
    header
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|given| fixed_time_equals(given.as_bytes(), token.as_bytes()))
}

/// A `?token=` value: non-empty and equal, in constant time.
pub fn matches_query(given: Option<&str>, token: &str) -> bool {
    given.is_some_and(|g| !g.is_empty() && fixed_time_equals(g.as_bytes(), token.as_bytes()))
}

/// `Lan.cs` compares with `path == "…"` (PathString: ignoring case).
const EXACT: [&str; 7] = [
    "/api/controls/spotify/state",
    "/api/controls/state",
    "/api/controls/listen",
    "/api/dictate",
    "/api/camera",
    "/api/spectrum",
    "/api/pair/claim",
];

/// … and with `path.StartsWithSegments("…")`.
const SEGMENTS: [&str; 31] = [
    "/api/controls/spotify/action",
    "/api/controls/radio-logo",
    "/api/controls/command",
    "/api/controls/obs",
    "/api/state",
    "/api/config",
    "/api/art",
    "/api/media",
    "/api/volume",
    "/api/quotas",
    "/api/work",
    "/api/procs",
    "/api/run",
    "/api/apps",
    "/api/shortcuts/icon",
    "/api/console",
    "/api/widget",
    "/api/calendar",
    "/api/feeds",
    "/api/photos",
    "/api/docker",
    "/api/obs",
    "/api/discord",
    "/api/batteries",
    "/api/ha/states",
    "/api/ha/toggle",
    "/api/approvals",
    "/api/rules/pause",
    "/api/phone/battery",
    "/api/stream",
    "/api/trackpad",
];

const WEBHOOKS: [&str; 4] = ["/api/push", "/api/notify", "/api/goto", "/api/refresh"];

/// Endpoints the phone may use over Wi-Fi. Settings, tests and pairing (except the claim) stay local-only.
pub fn allowed_over_lan(path: &str) -> bool {
    use crate::browser_guard::starts_with_segments;
    EXACT.iter().any(|p| path.eq_ignore_ascii_case(p))
        || SEGMENTS.iter().any(|p| starts_with_segments(path, p))
        || is_webhook(path)
}

/// The one Wi-Fi path that needs no key: a phone pairing without USB trades a one-time code for it.
pub fn is_pairing_claim(method: &str, path: &str) -> bool {
    method == "POST" && path.eq_ignore_ascii_case("/api/pair/claim")
}

/// The webhook API: other machines on the network (or Tailscale) may push data with the webhook key.
pub fn is_webhook(path: &str) -> bool {
    WEBHOOKS
        .iter()
        .any(|p| crate::browser_guard::starts_with_segments(path, p))
}

/// The Wi-Fi port's decision for one request, from the live config. `origin_present` is true for any Origin header,
/// readable or not; `authorization` is every Authorization header joined with ",".
pub fn authorize_request(
    lan: &crate::config::LanConfig,
    method: &str,
    path: &str,
    origin_present: bool,
    authorization: Option<&str>,
    query_token: Option<&str>,
) -> LanAccess {
    if !lan.enabled || origin_present || !allowed_over_lan(path) {
        return LanAccess::Denied;
    }
    // A hand-edited config can give both keys the same value. Every webhook sender then holds the phone key too, and
    // the agent cannot tell them apart, so that value is not trusted as a phone key until one of them changes.
    let shared_key = lan.token == lan.webhook_token;
    if let Some(token) = lan.token.as_deref().filter(|t| !t.is_empty())
        && !shared_key
        && matches(authorization, token)
    {
        return LanAccess::Phone;
    }
    if let Some(hook) = lan.webhook_token.as_deref().filter(|t| !t.is_empty())
        && is_webhook(path)
        && (matches(authorization, hook) || matches_query(query_token, hook))
    {
        return LanAccess::Webhook;
    }
    if is_pairing_claim(method, path) {
        return LanAccess::PairingClaim;
    }
    LanAccess::Denied
}

#[cfg(test)]
mod tests {
    use super::*;

    // LanTests (ConfigTests.cs): a Hyper-V external switch owns the physical NIC's address and gateway (#175).
    #[test]
    fn virtual_adapters_are_skipped_unless_they_carry_the_default_route() {
        for (desc, has_gateway, skip) in [
            (
                "Hyper-V Virtual Ethernet Adapter vEthernet (WAN)",
                true,
                false,
            ),
            (
                "Hyper-V Virtual Ethernet Adapter vEthernet (Default Switch)",
                false,
                true,
            ),
            ("WSL adapter vEthernet (WSL)", false, true),
            ("VMware Virtual Ethernet Adapter VMnet8", false, true),
            ("Bluetooth Device (Personal Area Network)", false, true),
            (
                "Intel(R) Ethernet Controller I219-LM Ethernet",
                false,
                false,
            ),
            ("Intel(R) Ethernet Controller I219-LM Ethernet", true, false),
        ] {
            assert_eq!(
                should_skip_adapter(desc, has_gateway),
                skip,
                "{desc} {has_gateway}"
            );
        }
    }

    #[test]
    fn addresses_are_private_lan_then_tailscale() {
        for a in addresses() {
            let ip: std::net::Ipv4Addr = a.parse().unwrap();
            let [b0, b1, ..] = ip.octets();
            assert!(
                b0 == 10
                    || (b0 == 172 && (16..=31).contains(&b1))
                    || (b0 == 192 && b1 == 168)
                    || (b0 == 100 && (64..=127).contains(&b1)),
                "{a}"
            );
        }
    }

    // LanTests (ConfigTests.cs)
    #[test]
    fn only_the_phone_api_is_reachable_over_wifi() {
        for (path, allowed) in [
            ("/api/state", true),
            ("/api/stream", true),
            ("/api/run/terminal", true),
            ("/api/apps", true),
            ("/api/apps/open", true),
            ("/api/feeds", true),
            ("/api/feeds/open", true),
            ("/api/photos", true),
            ("/api/photos/0123456789abcdef", true),
            ("/api/push/temperature", true),
            ("/api/settings", false),
            ("/api/pair", false),
            ("/api/pair/claim", true),
            ("/api/pair/start", false),
            ("/api/pair/cancel", false),
            ("/api/pair/status", false),
            ("/api/pair/claim/x", false),
            ("/api/shortcuts/test", false),
            ("/api/shortcuts/icon", true),
            ("/api/settings/shortcut-icon", false),
            ("/api/claude/hook", false),
            ("/api/obs-test", false),
            ("/api/ha/test", false),
            ("/api/phones", false),
            ("/api/rules/test", false),
            ("/panel/main.js", false),
            ("/", false),
            // PathString compares without case
            ("/API/State", true),
            ("/Api/Pair/Claim", true),
            ("/api/STATES", false),
        ] {
            assert_eq!(allowed_over_lan(path), allowed, "{path}");
        }
    }

    #[test]
    fn the_webhook_key_opens_only_webhooks() {
        for (path, webhook) in [
            ("/api/push/x", true),
            ("/api/notify", true),
            ("/api/goto/2", true),
            ("/api/refresh", true),
            ("/api/state", false),
            ("/api/run/x", false),
            ("/API/Push/x", true),
            ("/api/pushx", false),
        ] {
            assert_eq!(is_webhook(path), webhook, "{path}");
        }
    }

    #[test]
    fn tokens_are_compared_exactly() {
        assert!(matches(Some("Bearer abc123"), "abc123"));
        assert!(!matches(Some("Bearer abc12"), "abc123"));
        assert!(!matches(Some("abc123"), "abc123"));
        assert!(!matches(None, "abc123"));
        assert!(matches_query(Some("abc123"), "abc123"));
        assert!(!matches_query(Some(""), "abc123"));
        assert!(!matches_query(None, "abc123"));
    }

    #[test]
    fn bearer_is_exact() {
        for header in [
            "bearer abc123",
            "BEARER abc123",
            "Bearer  abc123",
            "Bearer abc123 ",
            "Bearer abc123,Bearer abc123",
            "Bearer\tabc123",
            "Bearer ABC123",
        ] {
            assert!(!matches(Some(header), "abc123"), "{header:?}");
        }
        assert!(matches(Some("Bearer äöü-ключ"), "äöü-ключ"), "UTF-8 bytes");
        assert!(!matches(Some("Bearer "), "abc123"));
    }

    // SpectrumAnalyzerTests
    #[test]
    fn the_phone_may_ask_for_bands_over_wifi() {
        assert!(allowed_over_lan("/api/spectrum"));
        assert!(!allowed_over_lan("/api/spectrum/raw"));
    }

    // CameraFeedsTests
    #[test]
    fn the_phone_may_ask_for_camera_pictures_over_wifi() {
        assert!(allowed_over_lan("/api/camera"));
        assert!(!allowed_over_lan("/api/camera/sources"));
    }

    // WidgetFeedProxyTests
    #[test]
    fn proxy_endpoint_stays_local_and_off_lan_allowlist() {
        assert!(!allowed_over_lan("/api/proxy"));
    }

    // PairingTests
    #[test]
    fn only_the_claim_needs_no_key_over_wifi() {
        for (method, path, open) in [
            ("POST", "/api/pair/claim", true),
            ("GET", "/api/pair/claim", false),
            ("POST", "/api/pair/start", false),
            ("POST", "/api/pair", false),
            ("POST", "/api/state", false),
            ("post", "/api/pair/claim", false),
            ("POST", "/API/PAIR/CLAIM", true),
        ] {
            assert_eq!(is_pairing_claim(method, path), open, "{method} {path}");
        }
    }

    fn lan(enabled: bool, token: &str, webhook_token: &str) -> crate::config::LanConfig {
        crate::config::LanConfig {
            enabled,
            port: 8766,
            token: Some(token.into()),
            webhook_token: Some(webhook_token.into()),
            ..Default::default()
        }
    }

    #[test]
    fn disabled_lan_rejects_all_tiers_and_webhook_token_stays_separate_from_phone_token() {
        use LanAccess::*;
        let enabled = lan(true, "phone-tok", "hook-tok");
        let auth = |l, m, p, o, a, q| authorize_request(l, m, p, o, a, q);
        assert_eq!(
            auth(
                &enabled,
                "GET",
                "/api/state",
                false,
                Some("Bearer phone-tok"),
                None
            ),
            Phone
        );
        assert_eq!(
            auth(
                &enabled,
                "POST",
                "/api/push/ci",
                false,
                None,
                Some("hook-tok")
            ),
            Webhook
        );
        assert_eq!(
            auth(
                &enabled,
                "GET",
                "/api/state",
                false,
                Some("Bearer hook-tok"),
                None
            ),
            Denied
        );
        assert_eq!(
            auth(&enabled, "POST", "/api/pair/claim", false, None, None),
            PairingClaim
        );
        assert_eq!(
            auth(
                &enabled,
                "GET",
                "/api/state",
                true,
                Some("Bearer phone-tok"),
                None
            ),
            Denied
        );

        let disabled = lan(false, "phone-tok", "hook-tok");
        assert_eq!(
            auth(
                &disabled,
                "GET",
                "/api/state",
                false,
                Some("Bearer phone-tok"),
                None
            ),
            Denied
        );
        assert_eq!(
            auth(
                &disabled,
                "POST",
                "/api/push/ci",
                false,
                None,
                Some("hook-tok")
            ),
            Denied
        );
        assert_eq!(
            auth(&disabled, "POST", "/api/pair/claim", false, None, None),
            Denied
        );

        // beyond the C# rows: the phone key on a local-only path, the webhook key's header form, an empty phone key,
        // a missing webhook key, an Origin on a claim, the query key off a webhook path
        assert_eq!(
            auth(
                &enabled,
                "GET",
                "/api/settings",
                false,
                Some("Bearer phone-tok"),
                None
            ),
            Denied
        );
        assert_eq!(
            auth(
                &enabled,
                "POST",
                "/api/push/ci",
                false,
                Some("Bearer hook-tok"),
                None
            ),
            Webhook
        );
        assert_eq!(
            auth(
                &enabled,
                "POST",
                "/api/push/ci",
                false,
                Some("Bearer phone-tok"),
                None
            ),
            Phone
        );
        assert_eq!(
            auth(&enabled, "GET", "/api/state", false, None, Some("hook-tok")),
            Denied
        );
        assert_eq!(
            auth(
                &enabled,
                "GET",
                "/api/state",
                false,
                None,
                Some("phone-tok")
            ),
            Denied
        );
        assert_eq!(
            auth(&enabled, "POST", "/api/pair/claim", true, None, None),
            Denied
        );
        let empty = lan(true, "", "hook-tok");
        assert_eq!(
            auth(&empty, "GET", "/api/state", false, Some("Bearer "), None),
            Denied
        );
        let no_hook = crate::config::LanConfig {
            webhook_token: None,
            ..lan(true, "phone-tok", "")
        };
        assert_eq!(
            auth(&no_hook, "POST", "/api/push/ci", false, None, Some("")),
            Denied
        );
        assert_eq!(
            auth(
                &no_hook,
                "GET",
                "/api/state",
                false,
                Some("Bearer phone-tok"),
                None
            ),
            Phone
        );
    }

    // Both keys with one value (a hand-edited config): every webhook sender would hold the phone key too.
    #[test]
    fn a_shared_phone_and_webhook_key_is_not_a_phone_key_and_cannot_be_saved() {
        use crate::server::routes_settings::lan_keys_shared;
        let shared = lan(true, "same-tok", "same-tok");
        assert_eq!(
            authorize_request(
                &shared,
                "GET",
                "/api/state",
                false,
                Some("Bearer same-tok"),
                None
            ),
            LanAccess::Denied
        );
        assert_eq!(
            authorize_request(
                &shared,
                "POST",
                "/api/push/ci",
                false,
                Some("Bearer same-tok"),
                None
            ),
            LanAccess::Webhook
        );

        // the settings page's save refuses it (other writes, like a Note from the phone, still go through)
        assert!(lan_keys_shared(
            r#"{"lan":{"enabled":true,"token":"same-tok","webhookToken":"same-tok"}}"#
        ));
        assert!(lan_keys_shared(
            r#"{"lan":{"token":"a","webhookToken":"b"},"LAN":{"token":"c","webhookToken":"c"}}"#
        ));
        assert!(!lan_keys_shared(
            r#"{"lan":{"enabled":true,"token":"phone-tok","webhookToken":"hook-tok"}}"#
        ));
        assert!(!lan_keys_shared(
            r#"{"lan":{"token":"","webhookToken":""}}"#
        ));
        assert!(!lan_keys_shared(
            r#"{"lan":{"token":"c","webhookToken":"c"},"lan":{"token":"a","webhookToken":"b"}}"#
        ));

        let dir = tempfile::tempdir().unwrap();
        let store = crate::config::store::ConfigStore::new(dir.path().join("config.json"));
        assert!(
            store
                .save(
                    r#"{"lan":{"token":"same-tok","webhookToken":"same-tok"}}"#,
                    None
                )
                .is_ok()
        );
    }

    // ConfigStore.Save keeps the last of repeated members; the port check must look at that one.
    #[test]
    fn the_last_of_repeated_lan_members_is_the_one_checked() {
        use crate::server::routes_settings::lan_port_collides_with_local;
        for (json, collides) in [
            (
                r#"{"lan":{"enabled":false},"lan":{"enabled":true,"port":8765}}"#,
                true,
            ),
            (
                r#"{"lan":{"enabled":true,"port":8765},"LAN":{"enabled":false}}"#,
                false,
            ),
            (r#"{"lan":{"enabled":true,"port":8765},"lan":null}"#, false),
        ] {
            assert_eq!(lan_port_collides_with_local(json, 8765), collides, "{json}");
            let dir = tempfile::tempdir().unwrap();
            let store = crate::config::store::ConfigStore::new(dir.path().join("config.json"));
            assert!(store.save(json, None).is_ok(), "{json}");
            let cfg = store.get();
            assert_eq!(cfg.lan.enabled && cfg.lan.port == 8765, collides, "{json}");
        }
    }
}
