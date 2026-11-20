//! Port of `BrowserGuard.cs`: keeps other web sites out of the local API. Browsers send Sec-Fetch-Site on every
//! request; the phone's USB proxy and command-line tools send none.

/// True for a request another site (or another port on this machine) made through a browser.
pub fn is_foreign(site: Option<&str>, mode: Option<&str>, method: &str, path: &str) -> bool {
    if !matches!(site, Some("cross-site" | "same-site")) {
        return false; // same-origin, typed URL ("none") or no browser
    }
    // following a link to the settings page (e.g. from the README) is fine; it only shows the page
    let page_navigation = mode == Some("navigate")
        && method.eq_ignore_ascii_case("GET")
        && !starts_with_segments(path, "/api");
    !page_navigation
}

/// `PathString.StartsWithSegments`: `path` is `prefix` or continues it with `/`, ignoring case.
pub fn starts_with_segments(path: &str, prefix: &str) -> bool {
    let (p, x) = (path.as_bytes(), prefix.as_bytes());
    p.len() >= x.len()
        && p[..x.len()].eq_ignore_ascii_case(x)
        && (p.len() == x.len() || p[x.len()] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_browser_requests_are_refused() {
        for (site, mode, method, path, foreign) in [
            (
                Some("cross-site"),
                Some("no-cors"),
                "GET",
                "/api/widget/devops/command",
                true,
            ),
            (
                Some("same-site"),
                Some("cors"),
                "GET",
                "/api/settings",
                true,
            ),
            (
                Some("cross-site"),
                Some("navigate"),
                "GET",
                "/api/widget/p/w",
                true,
            ),
            (Some("cross-site"), Some("navigate"), "POST", "/", true),
            (Some("cross-site"), Some("navigate"), "GET", "/", false),
            (
                Some("same-origin"),
                Some("cors"),
                "GET",
                "/api/settings",
                false,
            ),
            (Some("none"), Some("navigate"), "GET", "/", false),
            (None, None, "GET", "/api/state", false),
        ] {
            assert_eq!(
                is_foreign(site, mode, method, path),
                foreign,
                "{site:?} {mode:?} {method} {path}"
            );
        }
    }

    #[test]
    fn foreign_browser_origins_are_refused_by_browser_guard() {
        assert!(is_foreign(
            Some("cross-site"),
            Some("websocket"),
            "GET",
            "/api/trackpad"
        ));
        assert!(is_foreign(
            Some("same-site"),
            Some("cors"),
            "GET",
            "/api/trackpad"
        ));
        assert!(!is_foreign(None, None, "GET", "/api/trackpad"));
    }
}
