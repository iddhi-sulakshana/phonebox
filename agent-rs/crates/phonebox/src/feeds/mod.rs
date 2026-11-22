//! `FeedMonitor.cs`: headlines from the RSS and Atom feeds in the settings, for the Feeds widget.

mod html_entities;
pub mod http;
pub mod parser;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use reqwest::Url;
use serde::ser::{Serialize, SerializeStruct, Serializer};

use crate::config::store::ConfigStore;
use crate::config::{AgentConfig, FeedSource};
use crate::server::Stop;
use http::FeedSend;

pub const WIDGET_TYPE: &str = "feeds";
/// Each address is fetched at most this often (failed fetches wait just as long).
pub const INTERVAL_MS: i64 = 15 * 60_000;
const MAX_PER_FEED: usize = 50;
const MAX_ITEMS: usize = 200;
const CATEGORY: &str = "PhoneBox.Agent.FeedMonitor";
const OPEN_OFF: &str = "Opening articles is off: turn on \"Open articles on the PC\" in the News feeds widget's settings";

/// One headline. The link stays on this computer: the phone opens an item by its id.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedItem {
    pub id: String,
    pub title: String,
    pub source: String,
    pub published: i64,
    pub link: Option<String>,
}

impl FeedItem {
    pub fn openable(&self) -> bool {
        self.link.is_some()
    }
}

impl Serialize for FeedItem {
    /// `{"id","title","source","published","openable"}`: the link is never sent.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut o = s.serialize_struct("FeedItem", 5)?;
        o.serialize_field("id", &self.id)?;
        o.serialize_field("title", &self.title)?;
        o.serialize_field("source", &self.source)?;
        o.serialize_field("published", &self.published)?;
        o.serialize_field("openable", &self.openable())?;
        o.end()
    }
}

/// The headlines (newest first, at most 200), when they were published and each failing feed's error.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct FeedSnapshot {
    pub items: Vec<FeedItem>,
    pub updated: i64,
    pub errors: Vec<String>,
}

/// One address's last good headlines (with an empty source: the name is applied when the snapshot is built).
#[derive(Default)]
struct FeedState {
    items: Vec<FeedItem>,
    next_at: i64,
    error: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
}

type Opener = Arc<dyn Fn(&Url) -> Result<(), String> + Send + Sync>;
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Headlines from the RSS and Atom feeds in the settings, for the Feeds widget. Each address is fetched at most every
/// 15 minutes, and only while an enabled page shows a Feeds widget. The addresses stay on this computer (private
/// feeds carry keys in them); the phone gets titles, the feed's name and times, and opens an article here by its id.
pub struct FeedMonitor {
    cfg: Arc<ConfigStore>,
    send: FeedSend,
    opener: Opener,
    now: Clock,
    feeds: Mutex<HashMap<String, FeedState>>,
    snap: Mutex<Arc<FeedSnapshot>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Some enabled page (or enabled Smart Stack child) shows a Feeds widget.
pub fn shown(cfg: &AgentConfig) -> bool {
    cfg.widgets(true).iter().any(|w| w.r#type == WIDGET_TYPE)
}

/// Opening an article starts the browser on this computer: only with a Feeds widget's "open" option on.
pub fn open_allowed(cfg: &AgentConfig) -> bool {
    cfg.widgets(true)
        .iter()
        .any(|w| w.r#type == WIDGET_TYPE && w.bool("open") == Some(true))
}

impl FeedMonitor {
    pub fn new(cfg: Arc<ConfigStore>) -> FeedMonitor {
        FeedMonitor {
            cfg,
            send: http::real_send(),
            opener: Arc::new(open_in_browser),
            now: Arc::new(crate::server::now_ms),
            feeds: Mutex::new(HashMap::new()),
            snap: Mutex::new(Arc::new(FeedSnapshot::default())),
        }
    }

    /// Tests: the HTTP pools behind the fetches.
    pub fn with_send(mut self, send: FeedSend) -> FeedMonitor {
        self.send = send;
        self
    }

    /// Tests: what opens an article on this computer.
    pub fn with_opener(
        mut self,
        opener: impl Fn(&Url) -> Result<(), String> + Send + Sync + 'static,
    ) -> FeedMonitor {
        self.opener = Arc::new(opener);
        self
    }

    /// Tests: the clock, in Unix ms.
    pub fn with_clock(mut self, now: impl Fn() -> i64 + Send + Sync + 'static) -> FeedMonitor {
        self.now = Arc::new(now);
        self
    }

    pub fn snapshot(&self) -> Arc<FeedSnapshot> {
        lock(&self.snap).clone()
    }

    pub fn shown(&self) -> bool {
        shown(&self.cfg.get())
    }

    pub fn open_allowed(&self) -> bool {
        open_allowed(&self.cfg.get())
    }

    /// One pass every 5 s (the first at once) until `stop`.
    pub fn start(self: &Arc<Self>, stop: Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            while !stop.is_stopped() {
                tokio::select! {
                    _ = me.tick() => {}
                    _ = stop.stopped() => return,
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = stop.stopped() => return,
                }
            }
        })
    }

    /// Fetches every feed that is due. Nothing is fetched while no enabled page shows a Feeds widget.
    pub async fn tick(&self) {
        let list = sources(&self.cfg.get());
        lock(&self.feeds).retain(|url, _| list.iter().any(|s| s.url == *url));
        if list.is_empty() || !self.shown() {
            self.publish(&list);
            return;
        }
        for src in &list {
            // a fetch can take seconds: the widget or the feed may have been removed while the previous one ran
            if !self.shown() {
                break;
            }
            if !sources(&self.cfg.get()).iter().any(|s| s.url == src.url) {
                continue;
            }
            let validators = {
                let mut feeds = lock(&self.feeds);
                let st = feeds.entry(src.url.clone()).or_default();
                let now = (self.now)();
                if now < st.next_at {
                    continue;
                }
                st.next_at = now + INTERVAL_MS; // failed fetches wait just as long
                (st.etag.clone(), st.last_modified.clone())
            };
            self.fetch(src, validators).await;
        }
        self.publish(&sources(&self.cfg.get())); // a completed request can outlive a feed rename or removal
    }

    async fn fetch(
        &self,
        src: &FeedSource,
        (etag, last_modified): (Option<String>, Option<String>),
    ) {
        let got = tokio::time::timeout(
            Duration::from_secs(20),
            http::get(
                &self.send,
                &src.url,
                etag.as_deref(),
                last_modified.as_deref(),
            ),
        )
        .await;
        let result = match got {
            Err(_) => Err("no answer in 20 s".to_string()),
            Ok(Err(e)) => Err(e),
            Ok(Ok(r)) if r.not_modified => Ok(None),
            Ok(Ok(r)) => {
                let (body, base, key) = (
                    r.body.unwrap_or_default(),
                    r.final_uri.clone(),
                    src.url.clone(),
                );
                let parsed = tokio::task::spawn_blocking(move || {
                    parser::parse(&body, &base, &key, MAX_PER_FEED)
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
                parsed
                    .map(|items| Some((items, r.etag, r.last_modified)))
                    .map_err(|_| "not an RSS or Atom feed".to_string())
            }
        };
        let mut feeds = lock(&self.feeds);
        let Some(st) = feeds.get_mut(&src.url) else {
            return; // removed meanwhile
        };
        match result {
            Ok(None) => st.error = None,
            Ok(Some((items, etag, last_modified))) => {
                st.items = items;
                st.error = None;
                st.etag = etag;
                st.last_modified = last_modified;
            }
            Err(msg) => {
                // the last good headlines stay
                crate::log::info(CATEGORY, &format!("feed {}: {msg}", name_of(src)));
                st.error = Some(msg);
            }
        }
    }

    fn publish(&self, list: &[FeedSource]) {
        let mut items = Vec::new();
        let mut errors = Vec::new();
        {
            let feeds = lock(&self.feeds);
            for src in list {
                let Some(st) = feeds.get(&src.url) else {
                    continue;
                };
                let name = name_of(src);
                items.extend(st.items.iter().map(|i| FeedItem {
                    source: name.clone(),
                    ..i.clone()
                }));
                if let Some(e) = &st.error {
                    errors.push(format!("{name}: {e}"));
                }
            }
        }
        // newest first; undated items keep their feed order after the dated ones (a stable sort)
        items.sort_by_key(|i| std::cmp::Reverse(i.published));
        items.truncate(MAX_ITEMS);
        *lock(&self.snap) = Arc::new(FeedSnapshot {
            items,
            updated: (self.now)(),
            errors,
        });
    }

    /// Opens a headline from the current list on this computer; the error text, or None when it worked.
    pub fn open(&self, id: Option<&str>) -> Option<String> {
        if !self.open_allowed() {
            return Some(OPEN_OFF.into());
        }
        let snap = self.snapshot();
        let Some(item) = id
            .filter(|i| !i.is_empty())
            .and_then(|id| snap.items.iter().find(|i| i.id == id))
        else {
            return Some("That headline is no longer in the list".into());
        };
        let Some(uri) = parser::safe_link(item.link.as_deref()) else {
            return Some("This headline has no link".into());
        };
        let host = uri.host_str().unwrap_or("").to_string();
        if let Err(e) = (self.opener)(&uri) {
            crate::log::warn(CATEGORY, &format!("feed: could not open {host}: {e}"));
            return Some("Could not open it on the computer".into());
        }
        crate::log::info(
            CATEGORY,
            &format!("feed article opened from the phone: {host}"),
        );
        None
    }
}

/// The configured feeds with an address, the first of each address, in order.
fn sources(cfg: &AgentConfig) -> Vec<FeedSource> {
    let mut seen = std::collections::HashSet::new();
    cfg.feeds
        .iter()
        .filter(|f| !f.url.trim().is_empty() && seen.insert(f.url.clone()))
        .cloned()
        .collect()
}

/// The feed's name, else its host, else "Feed".
fn name_of(s: &FeedSource) -> String {
    if !s.name.trim().is_empty() {
        return s.name.trim().to_string();
    }
    Url::parse(&s.url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "Feed".into())
}

/// Hands the address to the default browser without a shell: explorer.exe on Windows, open(1) on macOS, xdg-open on
/// Linux, the address as one argument. explorer and open pass it on and exit, so one that hangs is ended after 15 s;
/// xdg-open can become the browser itself, so it is left running (and reaped).
fn open_in_browser(uri: &Url) -> Result<(), String> {
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        "xdg-open"
    };
    let mut command = std::process::Command::new(program);
    command
        .arg(uri.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().map_err(|e| {
        let text = e.to_string();
        text.find(" (os error")
            .map_or(text.clone(), |i| text[..i].to_string())
    })?;
    let bounded = cfg!(windows) || cfg!(target_os = "macos");
    std::thread::spawn(move || {
        if !bounded {
            let _ = child.wait(); // reaped, never ended
            return;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    });
    Ok(())
}

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;
