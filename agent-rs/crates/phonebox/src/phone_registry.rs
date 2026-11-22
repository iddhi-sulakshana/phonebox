//! Port of `PhoneRegistry.cs`: the phones that talk to this agent (X-PhoneBox-Device / X-PhoneBox-Model), remembered
//! while the agent runs, so the settings can give each one its own pages.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneSeen {
    pub id: String,
    pub model: String,
    pub last_seen: i64,
    pub via: String,
    pub web_view: Option<String>,
}

struct Registry {
    seen: HashMap<String, PhoneSeen>,
    warned: HashMap<String, Option<String>>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
    Mutex::new(Registry {
        seen: HashMap::new(),
        warned: HashMap::new(),
    })
});

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Records a request from a phone; returns the WebView problem when it is new for this phone (once, not every request).
pub fn touch(
    id: Option<&str>,
    model: Option<&str>,
    via: &str,
    web_view: Option<&str>,
) -> Option<String> {
    let id = id.filter(|i| {
        !i.trim().is_empty() && i.len() <= 32 && i.bytes().all(|b| b.is_ascii_alphanumeric())
    })?;
    let model = match model {
        Some(m) if !m.trim().is_empty() => m
            .chars()
            .filter(|c| (' '..='~').contains(c))
            .take(40)
            .collect(),
        _ => "Android phone".to_string(),
    };
    let normalized = crate::webview_doctor::normalize(web_view);
    let problem = crate::webview_doctor::diagnose(normalized.as_deref(), Some(&model));
    let mut r = registry();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    r.seen.insert(
        id.to_string(),
        PhoneSeen {
            id: id.to_string(),
            model,
            last_seen: now,
            via: via.to_string(),
            web_view: normalized,
        },
    );
    let previous = r.warned.insert(id.to_string(), problem.clone()).flatten();
    if problem != previous { problem } else { None }
}

pub fn all() -> Vec<PhoneSeen> {
    registry().seen.values().cloned().collect()
}

#[cfg(test)]
pub(crate) fn forget(ids: &[&str]) {
    let mut r = registry();
    for id in ids {
        r.seen.remove(*id);
        r.warned.remove(*id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{normalize::normalize, read::parse};
    use std::sync::{Arc, Barrier};

    const OLD: &str = "com.google.android.webview/83.0.4103.106";

    fn config(phones: &str) -> crate::config::AgentConfig {
        normalize(parse(&format!(r#"{{"pages":[{{"id":"home","title":"Home","widgets":[]}},{{"id":"music","title":"Music","widgets":[]}},{{"id":"clock","title":"Clock","widgets":[]}}],"phones":{phones}}}"#)).unwrap(), None)
    }
    fn ids(pages: Vec<&crate::config::PageConfig>) -> Vec<String> {
        pages.iter().map(|p| p.id.clone()).collect()
    }

    #[test]
    fn each_phone_gets_its_pages_in_the_usual_order() {
        let c = config(r#"[{"id":"desk","name":"Desk","pages":["clock","home"]}]"#);
        assert_eq!(ids(c.pages_for(Some("desk"))), ["home", "clock"]);
        assert_eq!(ids(c.pages_for(Some("tablet"))), ["home", "music", "clock"]);
        assert_eq!(ids(c.pages_for(None)), ["home", "music", "clock"]);
    }

    #[test]
    fn unknown_or_empty_selections_fall_back_to_all_pages() {
        let c =
            config(r#"[{"id":"a","name":"","pages":["gone"]},{"id":"b","name":"","pages":[]}]"#);
        assert_eq!(c.pages_for(Some("a")).len(), 3);
        assert_eq!(c.pages_for(Some("b")).len(), 3);
    }

    #[test]
    fn registry_ignores_malformed_ids() {
        touch(Some("abc123"), Some("Pixel 8"), "USB", None);
        touch(Some("../../etc"), Some("x"), "USB", None);
        touch(Some(&"a".repeat(64)), Some("x"), "USB", None);
        assert!(
            all()
                .iter()
                .any(|p| p.id == "abc123" && p.model == "Pixel 8")
        );
        assert!(!all().iter().any(|p| p.id.contains('/') || p.id.len() > 32));
    }

    #[test]
    fn concurrent_requests_return_an_unchanged_warning_only_once() {
        for wave in 0..8 {
            let id = format!("audit{wave}x{}", std::process::id());
            let barrier = Arc::new(Barrier::new(32));
            let threads: Vec<_> = (0..32)
                .map(|_| {
                    let (b, id) = (barrier.clone(), id.clone());
                    std::thread::spawn(move || {
                        b.wait();
                        touch(Some(&id), Some("Audit phone"), "USB", Some(OLD))
                    })
                })
                .collect();
            let warnings = threads
                .into_iter()
                .filter_map(|t| t.join().unwrap())
                .count();
            forget(&[&id]);
            assert_eq!(warnings, 1);
        }
    }

    #[test]
    fn registry_stores_normalized_web_view_and_deduplicates_warning() {
        let id = "wvtest01";
        assert!(
            touch(
                Some(id),
                Some("SM-G960F"),
                "Wi-Fi",
                Some("com.android.chrome/74.0.3729.185")
            )
            .unwrap()
            .contains("Update Google Chrome")
        );
        assert_eq!(
            touch(
                Some(id),
                Some("SM-G960F"),
                "Wi-Fi",
                Some("com.android.chrome/74.0.3729.185")
            ),
            None
        );
        assert!(all().iter().any(
            |p| p.id == id && p.web_view.as_deref() == Some("com.android.chrome/74.0.3729.185")
        ));
        assert_eq!(
            touch(
                Some(id),
                Some("SM-G960F"),
                "USB",
                Some("com.google.android.webview/134.0.0.0")
            ),
            None
        );
        assert_eq!(
            touch(Some(id), Some("SM-G960F"), "USB", Some("<script>/80.0")),
            None
        );
        assert!(all().iter().any(|p| p.id == id && p.web_view.is_none()));
    }
}
