//! Port of `WebViewDoctor.cs`: flags phones whose Android WebView is too old for the panel, and says how to update it.

use crate::phone_registry::PhoneSeen;

/// Parsed `X-PhoneBox-WebView` header (`com.google.android.webview/134.0.6998.135`).
#[derive(Debug, Clone, PartialEq)]
pub struct WebViewInfo {
    pub package: String,
    pub version: String,
    pub major: u32,
}

/// First Chromium milestone with CSS container query units (widget scaling).
pub const MIN_MAJOR: u32 = 105;
/// First Chromium milestone that runs the panel at all (modules, `?.`, `??`).
pub const RUN_MAJOR: u32 = 80;
/// A phone report older than this does not raise the active connection banner.
pub const FRESH_MS: i64 = 2 * 60_000;

/// Parses a validated `package/version` header string, or None when missing or malformed.
pub fn parse(raw: Option<&str>) -> Option<WebViewInfo> {
    let raw = raw?;
    if raw.trim().is_empty() || raw.chars().count() > 80 {
        return None;
    }
    let s = raw.trim();
    let slash = s.find('/')?;
    if slash == 0 || Some(slash) != s.rfind('/') || slash == s.len() - 1 {
        return None;
    }
    let (package, version) = (&s[..slash], &s[slash + 1..]);
    if !valid_package(package) || !valid_version(version) {
        return None;
    }
    let digits = version.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0
        || (digits < version.len() && !matches!(version.as_bytes()[digits], b'.' | b'-' | b'+'))
    {
        return None;
    }
    let major: u32 = version[..digits]
        .parse()
        .ok()
        .filter(|m| (1..=999).contains(m))?;
    Some(WebViewInfo {
        package: package.to_string(),
        version: version.to_string(),
        major,
    })
}

/// A canonical `package/version` string, or None when malformed.
pub fn normalize(raw: Option<&str>) -> Option<String> {
    parse(raw).map(|w| format!("{}/{}", w.package, w.version))
}

/// An update message when the WebView is older than Chromium 105, else None.
pub fn diagnose(webview: Option<&str>, model: Option<&str>) -> Option<String> {
    let w = parse(webview).filter(|w| w.major < MIN_MAJOR)?;
    let who = model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("The phone");
    let pkg = w.package.to_lowercase();
    let how = if pkg.starts_with("com.android.chrome") || pkg.starts_with("com.chrome.") {
        "Update Google Chrome from Google Play (it provides WebView on this device)."
    } else if pkg.starts_with("com.google.android.webview") {
        "Update Android System WebView from Google Play."
    } else {
        "Install or update Android System WebView (or Google Chrome), then select it in Developer options › WebView implementation."
    };
    let needs = if w.major < RUN_MAJOR {
        format!("the panel needs Chromium {RUN_MAJOR}+ to run at all")
    } else {
        format!("widget scaling needs Chromium {MIN_MAJOR}+")
    };
    Some(format!(
        "{who} has an outdated WebView ({}, {}); {needs}. {how}",
        w.major, w.package
    ))
}

/// The most recently seen active phone with an outdated WebView, or None.
pub fn diagnose_recent(phones: &[PhoneSeen], now: i64) -> Option<String> {
    let mut sorted: Vec<&PhoneSeen> = phones.iter().collect();
    sorted.sort_by_key(|p| std::cmp::Reverse(p.last_seen));
    sorted
        .into_iter()
        .filter(|p| (0..=FRESH_MS).contains(&(now - p.last_seen)))
        .find_map(|p| diagnose(p.web_view.as_deref(), Some(&p.model)))
}

fn valid_package(pkg: &str) -> bool {
    (3..=60).contains(&pkg.len())
        && pkg.as_bytes()[0].is_ascii_alphabetic()
        && pkg.contains('.')
        && pkg
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

fn valid_version(ver: &str) -> bool {
    (1..=32).contains(&ver.len())
        && ver
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: i64 = 1_000_000_000;

    fn seen(id: &str, model: &str, last: i64, via: &str, wv: &str) -> PhoneSeen {
        PhoneSeen {
            id: id.into(),
            model: model.into(),
            last_seen: last,
            via: via.into(),
            web_view: Some(wv.into()),
        }
    }

    #[test]
    fn diagnoses_google_web_view_below105() {
        let msg = diagnose(
            Some("com.google.android.webview/83.0.4103.106"),
            Some("Pixel 3a"),
        )
        .unwrap();
        assert!(msg.contains("Pixel 3a"));
        assert!(msg.contains("outdated WebView (83"));
        assert!(msg.contains("Chromium 105+"));
        assert!(msg.contains("Update Android System WebView from Google Play"));
    }

    #[test]
    fn a_web_view_that_cant_run_the_panel_says_so() {
        let msg = diagnose(Some("com.google.android.webview/37.0.0.0"), Some("SM-T800")).unwrap();
        assert!(msg.contains("the panel needs Chromium 80+ to run at all"));
        assert!(!msg.contains("widget scaling"));
        assert!(
            diagnose(
                Some("com.google.android.webview/80.0.3987.99"),
                Some("SM-T800")
            )
            .unwrap()
            .contains("widget scaling needs Chromium 105+")
        );
    }

    #[test]
    fn diagnoses_chrome_provided_web_view_on_older_android() {
        for wv in [
            "com.android.chrome/74.0.3729.185",
            "com.chrome.beta/90.0.4430.82",
        ] {
            assert!(diagnose(Some(wv), Some("SM-G960F")).unwrap().contains(
                "Update Google Chrome from Google Play (it provides WebView on this device)"
            ));
        }
    }

    #[test]
    fn diagnoses_aosp_or_vendor_web_view() {
        assert!(
            diagnose(
                Some("com.android.webview/91.0.4472.114"),
                Some("Generic Tablet")
            )
            .unwrap()
            .contains("Developer options › WebView implementation")
        );
    }

    #[test]
    fn stays_quiet_on_modern_or_missing_web_view() {
        for wv in [
            Some("com.google.android.webview/105.0.5195.136"),
            Some("com.google.android.webview/134.0.6998.135-arm64"),
            None,
            Some(""),
        ] {
            assert_eq!(diagnose(wv, Some("Pixel 8")), None, "{wv:?}");
        }
    }

    #[test]
    fn rejects_malformed_or_malicious_headers() {
        for raw in [
            "<script>alert(1)</script>/80.0",
            "com.google.android.webview/80.0\r\nInjected: 1",
            "noslash",
            "bad..pkg/abc",
            "com.android.webview/0.1",
            "com.android.webview/1000.0",
            "com.android.webview/80.0/extra",
            "nopkg/80.0",
        ] {
            assert_eq!(normalize(Some(raw)), None, "{raw}");
            assert_eq!(diagnose(Some(raw), Some("Pixel 8")), None, "{raw}");
        }
    }

    #[test]
    fn rejects_overlong_package_header() {
        let long = format!("{}/80.0", "a".repeat(90));
        assert_eq!(normalize(Some(&long)), None);
        assert_eq!(diagnose(Some(&long), Some("Pixel 8")), None);
    }

    #[test]
    fn diagnose_recent_ignores_stale_phones_and_finds_active_outdated_one() {
        let mut phones = vec![
            seen(
                "stale1",
                "Old Tablet",
                NOW - FRESH_MS - 10_000,
                "Wi-Fi",
                "com.android.webview/74.0.0.0",
            ),
            seen(
                "new1",
                "Pixel 8",
                NOW - 1000,
                "USB",
                "com.google.android.webview/134.0.0.0",
            ),
        ];
        assert_eq!(diagnose_recent(&phones, NOW), None);
        phones.push(seen(
            "old2",
            "Pixel 3a",
            NOW - 5000,
            "USB",
            "com.google.android.webview/83.0.4103.106",
        ));
        assert!(diagnose_recent(&phones, NOW).unwrap().contains("Pixel 3a"));
    }
}
