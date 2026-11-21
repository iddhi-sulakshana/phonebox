//! Port of `Pairing.cs`: pairing a phone over Wi-Fi without USB. The settings page asks for a one-time code and shows
//! a QR code with a pairing link: the computer's name, addresses, Wi-Fi port, the TLS certificate fingerprint and the
//! code. The phone opens the link, pins the certificate from the fingerprint, and trades the code for the Wi-Fi key
//! over TLS (`POST /api/pair/claim`). The key itself is never in the QR code; a code works once, for a few minutes.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

pub const LIFETIME_MS: i64 = 5 * 60_000;
/// Wrong codes before the current one is dropped (a code has 128 random bits; this only stops noise).
pub const MAX_FAILURES: i32 = 10;

#[derive(Default)]
struct Inner {
    code: Option<Vec<u8>>,
    expires_at: i64,
    failures: i32,
    /// numbers each start, so a late cancel from an old dialog cannot end a newer pairing
    attempt: i64,
}

#[derive(Default)]
pub struct PairingCodes {
    inner: Mutex<Inner>,
    claims: AtomicI64,
}

impl PairingCodes {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many phones have paired with a code since the agent started (the settings page watches it).
    pub fn claims(&self) -> i64 {
        self.claims.load(Ordering::SeqCst)
    }

    /// Starts a new pairing; the previous code stops working. Gives the code, when it expires and the attempt number.
    pub fn start(&self, now_ms: i64) -> (String, i64, i64) {
        use base64::Engine;
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the system random source");
        let code = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut inner = self.lock();
        inner.code = Some(code.as_bytes().to_vec());
        inner.expires_at = now_ms + LIFETIME_MS;
        inner.failures = 0;
        inner.attempt += 1;
        (code, inner.expires_at, inner.attempt)
    }

    /// Ends a pairing when its dialog closes; a newer pairing (another attempt) is left alone.
    pub fn cancel(&self, attempt: i64) {
        let mut inner = self.lock();
        if attempt == inner.attempt {
            inner.code = None;
        }
    }

    /// True once for the current, unexpired code; a used, expired or wrong code returns false.
    pub fn claim(&self, code: Option<&str>, now_ms: i64) -> bool {
        let mut inner = self.lock();
        let (Some(current), Some(code)) = (inner.code.as_deref(), code.filter(|c| !c.is_empty()))
        else {
            return false;
        };
        if now_ms > inner.expires_at {
            inner.code = None;
            return false;
        }
        if !crate::lan::fixed_time_equals(code.as_bytes(), current) {
            inner.failures += 1;
            if inner.failures >= MAX_FAILURES {
                inner.code = None;
            }
            return false;
        }
        inner.code = None; // one phone per code
        self.claims.fetch_add(1, Ordering::SeqCst);
        true
    }
}

/// `ReadFromJsonAsync` accepts `application/json` and any `+json` type; the charset must be UTF-8 (or absent).
fn is_utf8_json(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let mut parts = content_type.split(';');
    let media = parts.next().unwrap_or_default().trim().as_bytes();
    let json = media.eq_ignore_ascii_case(b"application/json")
        || (media.contains(&b'/')
            && media.len() > 5
            && media[media.len() - 5..].eq_ignore_ascii_case(b"+json"));
    if !json {
        return false;
    }
    for parameter in parts {
        if let Some((name, value)) = parameter.split_once('=')
            && name.trim().eq_ignore_ascii_case("charset")
        {
            let value = value.trim().trim_matches('"');
            return value.eq_ignore_ascii_case("utf-8") || value.eq_ignore_ascii_case("utf8");
        }
    }
    true
}

/// `{"code": "…"}` read like System.Text.Json's web defaults: the property name in any case, the last of repeated
/// properties wins, other properties are skipped, a UTF-8 BOM is allowed, anything else is an error.
fn json_code(body: &[u8]) -> Option<String> {
    use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
    struct Claim;
    impl<'de> Visitor<'de> for Claim {
        type Value = Option<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a claim object")
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut code = None;
            while let Some(key) = map.next_key::<String>()? {
                if key.eq_ignore_ascii_case("code") {
                    code = map.next_value::<Option<String>>()?;
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(code)
        }
    }
    let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    let mut de = serde_json::Deserializer::from_slice(body);
    let code = (&mut de).deserialize_any(Claim).ok()?;
    de.end().ok()?;
    code
}

/// The code from a `POST /api/pair/claim` body, or None for anything else. Anyone on the Wi-Fi can send this request
/// without the key, so every bad body (wrong or missing Content-Type, unknown charset, malformed JSON) is a plain
/// refusal. The 1 KB body cap is the caller's.
pub fn read_claim_code(content_type: Option<&str>, body: &[u8]) -> Option<String> {
    if !is_utf8_json(content_type) {
        return None;
    }
    json_code(body).filter(|c| (1..=64).contains(&c.encode_utf16().count()))
}

/// `Uri.EscapeDataString`: RFC 3986 unreserved characters stay, everything else is UTF-8 in upper-case `%XX`.
fn escape_data_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The link in the QR code. Everything is in the fragment (after `#`), which browsers never send to a server, so
/// opening the link in a browser instead of the app does not leak the code to the web page's host.
pub fn link(pc: &str, fingerprint: &str, port: u16, hosts: &[String], code: &str) -> String {
    let hosts: Vec<String> = hosts.iter().map(|h| escape_data_string(h)).collect();
    format!(
        "https://ersinkoc.github.io/phonebox/pair/#v=1&pc={}&fp={fingerprint}&port={port}&h={}&c={code}",
        escape_data_string(pc),
        hosts.join(",")
    )
}

/// The start of the certificate fingerprint ("AB12 CD34"): the settings page shows it next to the QR code and the
/// phone shows it in its confirmation, so a pairing link from anyone else is easy to spot.
pub fn check(fingerprint: &str) -> String {
    let chars: Vec<char> = fingerprint.chars().collect();
    if chars.len() < 8 {
        return fingerprint.to_uppercase();
    }
    let part = |r: std::ops::Range<usize>| chars[r].iter().collect::<String>();
    format!("{} {}", part(0..4), part(4..8)).to_uppercase()
}

/// A QR code as SVG markup (shown through an `<img>`, so it cannot run anything): error correction M, 8-pixel modules,
/// the 4-module quiet zone, black on white. Text too long for a QR code gives an empty string.
pub fn svg(text: &str) -> String {
    use qrcode::render::svg;
    let Ok(code) = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M)
    else {
        return String::new();
    };
    let markup = code
        .render::<svg::Color>()
        .module_dimensions(8, 8)
        .quiet_zone(true)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();
    // the XML declaration is dropped: the page embeds the markup as an <img> source
    match markup.find("<svg") {
        Some(start) => markup[start..].to_string(),
        None => markup,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn a_code_works_once_and_counts_the_claim() {
        let codes = PairingCodes::new();
        let (code, expires, _) = codes.start(NOW);
        assert_eq!(expires, NOW + LIFETIME_MS);
        assert_eq!(code.len(), 22, "128 bits, base64url");
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "{code}"
        );
        assert!(codes.claim(Some(&code), NOW + 1000));
        assert_eq!(codes.claims(), 1);
        assert!(!codes.claim(Some(&code), NOW + 2000));
        assert_eq!(codes.claims(), 1);
        assert_ne!(codes.start(NOW).0, code, "every code is new");
    }

    #[test]
    fn an_expired_cancelled_or_replaced_code_fails() {
        let codes = PairingCodes::new();
        let (expired, _, _) = codes.start(NOW);
        assert!(!codes.claim(Some(&expired), NOW + LIFETIME_MS + 1));

        let (cancelled, _, attempt) = codes.start(NOW);
        codes.cancel(attempt);
        assert!(!codes.claim(Some(&cancelled), NOW));

        let (old, _, _) = codes.start(NOW);
        let (current, _, _) = codes.start(NOW);
        assert!(!codes.claim(Some(&old), NOW));
        assert!(codes.claim(Some(&current), NOW));

        // at the last millisecond the code still works
        let (edge, _, _) = codes.start(NOW);
        assert!(codes.claim(Some(&edge), NOW + LIFETIME_MS));
    }

    #[test]
    fn a_late_cancel_from_an_older_dialog_leaves_the_new_pairing() {
        let codes = PairingCodes::new();
        let (_, _, first) = codes.start(NOW);
        let (second, _, second_attempt) = codes.start(NOW);
        assert_eq!(second_attempt, first + 1);
        codes.cancel(first);
        assert!(codes.claim(Some(&second), NOW));
    }

    #[test]
    fn too_many_wrong_codes_drop_the_current_one() {
        let codes = PairingCodes::new();
        let (code, _, _) = codes.start(NOW);
        assert!(!codes.claim(None, NOW));
        assert!(!codes.claim(Some(""), NOW));
        for i in 0..MAX_FAILURES {
            assert!(!codes.claim(Some(&format!("wrong{i}")), NOW));
        }
        assert!(!codes.claim(Some(&code), NOW));
        assert_eq!(codes.claims(), 0);

        // one wrong try fewer leaves the code working
        let (code, _, _) = codes.start(NOW);
        for i in 0..MAX_FAILURES - 1 {
            assert!(!codes.claim(Some(&format!("wrong{i}")), NOW));
        }
        assert!(codes.claim(Some(&code), NOW));
    }

    #[test]
    fn the_link_keeps_everything_in_the_fragment() {
        let hosts = ["192.168.1.10".to_string(), "100.64.0.7".to_string()];
        let link = link("Desk PC & co", "ab12", 8766, &hosts, "CODE_x-1");
        assert_eq!(
            link,
            "https://ersinkoc.github.io/phonebox/pair/#v=1&pc=Desk%20PC%20%26%20co&fp=ab12&port=8766\
             &h=192.168.1.10,100.64.0.7&c=CODE_x-1"
        );
        let (before, _) = link.split_once('#').unwrap();
        assert!(!before.contains('?'), "nothing a web server would receive");
        // Uri.EscapeDataString: RFC 3986 unreserved characters stay, the rest is UTF-8 in upper-case hex
        let other = super::link("Büro~._-/:#?", "f", 1, &["fe80::1%eth0".to_string()], "c");
        assert!(other.contains("&pc=B%C3%BCro~._-%2F%3A%23%3F&"), "{other}");
        assert!(other.contains("&h=fe80%3A%3A1%25eth0&"), "{other}");
    }

    #[test]
    fn the_check_code_is_the_start_of_the_fingerprint() {
        assert_eq!(check(&format!("ab12cd34{}", "0".repeat(56))), "AB12 CD34");
        assert_eq!(check("ab12"), "AB12");
        assert_eq!(check(""), "");
    }

    #[test]
    fn the_qr_code_is_an_svg() {
        let svg = svg(&link(
            "PC",
            &"a".repeat(64),
            8766,
            &["192.168.1.10".to_string()],
            &"A".repeat(22),
        ));
        assert!(
            svg.trim_start().starts_with("<svg"),
            "{}",
            &svg[..60.min(svg.len())]
        );
        assert!(!svg.to_ascii_lowercase().contains("<script"));
        assert!(svg.contains("viewBox"), "it scales with the page");
        assert!(
            svg.contains("#000000") && svg.contains("#ffffff"),
            "black on white"
        );
        assert!(svg.trim_end().ends_with("</svg>"));
    }

    // Anyone on the Wi-Fi can send a claim without the key: a body that is not a JSON code is refused.
    #[test]
    fn a_claim_body_that_is_not_a_json_code_is_refused_without_throwing() {
        let long = format!(r#"{{"code":"{}"}}"#, "a".repeat(65));
        for (content_type, body) in [
            (Some("text/plain"), r#"{"code":"abc"}"#),
            (None, r#"{"code":"abc"}"#),
            (Some("application/json; charset=nope"), r#"{"code":"abc"}"#),
            (Some("application/json"), "{ not json"),
            (Some("application/json"), r#"{"code":123}"#),
            (Some("application/json"), r#"{"code":""}"#),
            (Some("application/json"), long.as_str()),
            // beyond the C# rows
            (Some("application/json"), r#"{"code":null}"#),
            (Some("application/json"), "null"),
            (Some("application/json"), r#"["abc"]"#),
            (Some("application/json"), r#"{"code":"abc"} x"#),
            (Some("application/json"), r#"{"code":"abc",}"#),
            (Some("application/json"), r#"{"code":"\ud800"}"#),
            (Some("application/json"), ""),
            (
                Some("application/json; charset=utf-16"),
                r#"{"code":"abc"}"#,
            ),
            (Some("application/jsonx"), r#"{"code":"abc"}"#),
            (Some(""), r#"{"code":"abc"}"#),
        ] {
            assert_eq!(
                read_claim_code(content_type, body.as_bytes()),
                None,
                "{content_type:?} {body}"
            );
        }
        assert_eq!(
            read_claim_code(Some("application/json"), b"{\"code\":\"ab\xffc\"}"),
            None,
            "not UTF-8"
        );
        let emoji = format!(r#"{{"code":"{}"}}"#, "😀".repeat(33));
        assert_eq!(
            read_claim_code(Some("application/json"), emoji.as_bytes()),
            None,
            "66 UTF-16 units"
        );
    }

    #[test]
    fn a_json_claim_body_gives_its_code() {
        assert_eq!(
            read_claim_code(
                Some("application/json; charset=utf-8"),
                br#"{"code":"CODE_x-1"}"#
            )
            .as_deref(),
            Some("CODE_x-1")
        );
        for (content_type, body, code) in [
            ("application/json", r#"{"Code":"A"}"#, "A"),
            (
                "APPLICATION/JSON; CHARSET=\"UTF-8\"",
                r#"{"CODE":"B"}"#,
                "B",
            ),
            (
                "application/merge-patch+json",
                r#"{"other":[1,{"x":2}],"code":"C"}"#,
                "C",
            ),
            ("application/json", r#"{"code":"x","code":"D"}"#, "D"),
            ("application/json;charset=utf8", r#" {"code" : "E"} "#, "E"),
            ("application/json", "\u{feff}{\"code\":\"F\"}", "F"),
        ] {
            assert_eq!(
                read_claim_code(Some(content_type), body.as_bytes()).as_deref(),
                Some(code),
                "{body}"
            );
        }
        let full = "é".repeat(64);
        let body = format!(r#"{{"code":"{full}"}}"#);
        assert_eq!(
            read_claim_code(Some("application/json"), body.as_bytes()),
            Some(full),
            "64 UTF-16 units"
        );
    }
}
