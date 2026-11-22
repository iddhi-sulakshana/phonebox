//! `CalendarMonitor.cs`: upcoming events from iCalendar (ICS) feeds — Google's "secret address in iCal format",
//! Outlook's published calendar link, iCloud public calendars, any CalDAV server's export. Fetched every 10 minutes,
//! right after the calendars change and on `/api/refresh`; the URLs stay on this computer (they are secrets), the phone
//! only gets the events of the next two weeks.

pub mod ics;
mod windows_zones;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::future::BoxFuture;
use jiff::tz::TimeZone;

use crate::config::CalendarSource;
use crate::config::store::ConfigStore;
use crate::server::Stop;

const DAYS_AHEAD: i64 = 14;
const DAY_MS: i64 = 86_400_000;
const DEFAULT_COLOR: &str = "#60a5fa";
const CATEGORY: &str = "PhoneBox.Agent.CalendarMonitor";

/// One occurrence. Start and end are UTC ms; an all-day event uses UTC midnight of its date (the phone shifts it to
/// its own day). `calendar` and `color` come from the feed's settings.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEvent {
    pub title: String,
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub location: Option<String>,
    pub calendar: String,
    pub color: String,
}

/// The events of the next two weeks (by start, then title), when they were read and each failing feed's error.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct CalendarSnapshot {
    pub events: Vec<CalendarEvent>,
    pub updated: i64,
    pub errors: Vec<String>,
}

/// Downloads a feed's text (C#'s `HttpClient.GetStringAsync`); an Err is the text the phone sees, without the URL.
pub type IcsFetch = Arc<dyn Fn(String) -> BoxFuture<'static, Result<String, String>> + Send + Sync>;

pub struct CalendarMonitor {
    cfg: Arc<ConfigStore>,
    fetch: IcsFetch,
    zone: TimeZone,
    snap: Mutex<Arc<CalendarSnapshot>>,
    /// The last good events of each feed, by address (names are optional and may repeat).
    last_good: Mutex<HashMap<String, Vec<CalendarEvent>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The real client: 20 s per feed, `User-Agent: PhoneBox`, redirects followed, no address filter (C#'s policy).
pub fn real_fetch() -> IcsFetch {
    const INVALID_URI: &str = "An invalid request URI was provided. Either the request URI must be an absolute URI or BaseAddress must be set.";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("PhoneBox")
        .build()
        .expect("the calendar client");
    Arc::new(move |url: String| {
        let client = client.clone();
        Box::pin(async move {
            let parsed = reqwest::Url::parse(&url).map_err(|_| INVALID_URI.to_string())?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(format!(
                    "The '{}' scheme is not supported.",
                    parsed.scheme()
                ));
            }
            let host = parsed.host_str().unwrap_or("").to_string();
            let port = parsed.port_or_known_default().unwrap_or(0);
            // .NET's texts: the timeout names the limit, a failed connect names the endpoint, never the URL
            let failed = move |e: reqwest::Error| {
                if e.is_timeout() {
                    return "The request was canceled due to the configured HttpClient.Timeout of 20 seconds elapsing."
                        .to_string();
                }
                let text = crate::net::error_text(&e);
                if e.is_connect() {
                    let text = text
                        .strip_prefix("failed to lookup address information: ")
                        .unwrap_or(&text);
                    format!("{text} ({host}:{port})")
                } else {
                    text
                }
            };
            let res = client.get(parsed).send().await.map_err(&failed)?;
            let status = res.status();
            if !status.is_success() {
                return Err(match status.canonical_reason() {
                    Some(reason) => {
                        format!(
                            "Response status code does not indicate success: {} ({reason}).",
                            status.as_u16()
                        )
                    }
                    None => format!(
                        "Response status code does not indicate success: {}.",
                        status.as_u16()
                    ),
                });
            }
            let content_type = res
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let bytes = res.bytes().await.map_err(&failed)?;
            decode_body(&bytes, content_type.as_deref())
        })
    })
}

/// `HttpContent.ReadAsStringAsync`: the Content-Type's charset (.NET's names without code pages), else a byte order
/// mark, else UTF-8; a matching BOM is skipped and bad bytes become U+FFFD.
fn decode_body(bytes: &[u8], content_type: Option<&str>) -> Result<String, String> {
    #[derive(Clone, Copy, PartialEq)]
    enum Enc {
        Utf8,
        Latin1,
        Ascii,
        Utf16Le,
        Utf16Be,
        Utf32Le,
        Utf32Be,
    }
    let preamble = |enc: Enc| -> &'static [u8] {
        match enc {
            Enc::Utf8 => &[0xEF, 0xBB, 0xBF],
            Enc::Utf16Le => &[0xFF, 0xFE],
            Enc::Utf16Be => &[0xFE, 0xFF],
            Enc::Utf32Le => &[0xFF, 0xFE, 0, 0],
            Enc::Utf32Be => &[0, 0, 0xFE, 0xFF],
            Enc::Latin1 | Enc::Ascii => &[],
        }
    };
    let charset = content_type.and_then(|ct| {
        ct.split(';').skip(1).find_map(|p| {
            let (k, v) = p.split_once('=')?;
            k.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| v.trim().trim_matches('"').to_ascii_lowercase())
        })
    });
    let enc = match charset.as_deref() {
        Some(name) => match name {
            "utf-8"
            | "unicode-1-1-utf-8"
            | "unicode-2-0-utf-8"
            | "x-unicode-1-1-utf-8"
            | "x-unicode-2-0-utf-8" => Enc::Utf8,
            "iso-8859-1" | "latin1" | "l1" | "iso_8859-1" | "iso_8859-1:1987" | "iso-ir-100"
            | "ibm819" | "cp819" | "csisolatin1" | "iso8859-1" => Enc::Latin1,
            "us-ascii" | "ascii" | "ansi_x3.4-1968" | "ansi_x3.4-1986" | "cp367" | "csascii"
            | "ibm367" | "iso-ir-6" | "iso646-us" | "iso_646.irv:1991" | "us" => Enc::Ascii,
            "utf-16" | "utf-16le" | "unicode" | "ucs-2" | "iso-10646-ucs-2" | "csunicode" => {
                Enc::Utf16Le
            }
            "utf-16be" | "unicodefffe" => Enc::Utf16Be,
            "utf-32" | "utf-32le" => Enc::Utf32Le,
            "utf-32be" => Enc::Utf32Be,
            _ => {
                return Err("The character set provided in ContentType is invalid. Cannot read content as string \
                    using an invalid character set."
                    .into());
            }
        },
        // no charset: a byte order mark, else UTF-8
        None => [
            Enc::Utf8,
            Enc::Utf32Le,
            Enc::Utf16Le,
            Enc::Utf16Be,
            Enc::Utf32Be,
        ]
        .into_iter()
        .find(|e| bytes.starts_with(preamble(*e)))
        .unwrap_or(Enc::Utf8),
    };
    let body = bytes.strip_prefix(preamble(enc)).unwrap_or(bytes);
    Ok(match enc {
        Enc::Utf8 => String::from_utf8_lossy(body).into_owned(),
        Enc::Latin1 => body.iter().map(|&b| char::from(b)).collect(),
        Enc::Ascii => body
            .iter()
            .map(|&b| if b < 0x80 { char::from(b) } else { '?' })
            .collect(),
        Enc::Utf16Le | Enc::Utf16Be => {
            let (pairs, rest) = body.as_chunks::<2>();
            let units = pairs.iter().map(|&c| {
                if enc == Enc::Utf16Le {
                    u16::from_le_bytes(c)
                } else {
                    u16::from_be_bytes(c)
                }
            });
            let mut text: String = char::decode_utf16(units)
                .map(|r| r.unwrap_or('\u{FFFD}'))
                .collect();
            if !rest.is_empty() {
                text.push('\u{FFFD}');
            }
            text
        }
        Enc::Utf32Le | Enc::Utf32Be => {
            let (quads, rest) = body.as_chunks::<4>();
            let mut text: String = quads
                .iter()
                .map(|&c| {
                    let v = if enc == Enc::Utf32Le {
                        u32::from_le_bytes(c)
                    } else {
                        u32::from_be_bytes(c)
                    };
                    char::from_u32(v).unwrap_or('\u{FFFD}')
                })
                .collect();
            if !rest.is_empty() {
                text.push('\u{FFFD}');
            }
            text
        }
    })
}

impl CalendarMonitor {
    /// Cheap: no loop starts until [`CalendarMonitor::start`] (the rule engine holds one only to read it).
    pub fn new(cfg: Arc<ConfigStore>) -> CalendarMonitor {
        CalendarMonitor {
            cfg,
            fetch: real_fetch(),
            zone: TimeZone::system(),
            snap: Mutex::new(Arc::new(CalendarSnapshot::default())),
            last_good: Mutex::new(HashMap::new()),
        }
    }

    /// Tests: the download behind each feed.
    pub fn with_fetch(mut self, fetch: IcsFetch) -> CalendarMonitor {
        self.fetch = fetch;
        self
    }

    /// Tests: the computer's zone (floating times and unknown TZIDs).
    pub fn with_zone(mut self, zone: TimeZone) -> CalendarMonitor {
        self.zone = zone;
        self
    }

    pub fn snapshot(&self) -> Arc<CalendarSnapshot> {
        lock(&self.snap).clone()
    }

    /// Checks every 3 s; refreshes at once, then every 10 minutes, right after the calendars change and on a wake.
    pub fn start(self: &Arc<Self>, stop: Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut next: Option<tokio::time::Instant> = None;
            let mut wakes = crate::wake::count();
            let mut known = String::new();
            while !stop.is_stopped() {
                let list = sources(&me.cfg.get());
                let key = list
                    .iter()
                    .map(|s| format!("{}{}{}", s.url, s.name, s.color.as_deref().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join("|");
                // every 10 minutes, right after the calendars change, and on /api/refresh
                let due = next.is_none_or(|n| tokio::time::Instant::now() >= n);
                if key != known || due || crate::wake::count() != wakes {
                    known = key;
                    wakes = crate::wake::count();
                    tokio::select! {
                        _ = me.refresh(&list) => {}
                        _ = stop.stopped() => return,
                    }
                    next = Some(tokio::time::Instant::now() + Duration::from_secs(600));
                }
                crate::wake::delay(Duration::from_secs(3), &stop).await;
            }
        })
    }

    /// Reads every feed and publishes the events from yesterday's UTC midnight to 15 days on.
    pub(crate) async fn refresh(&self, sources: &[CalendarSource]) {
        let mut events = Vec::new();
        let mut errors = Vec::new();
        let from = crate::server::now_ms().div_euclid(DAY_MS) * DAY_MS - DAY_MS;
        let to = from + (DAYS_AHEAD + 1) * DAY_MS;
        for src in sources {
            let name = if src.name.trim().is_empty() {
                "Calendar".to_string()
            } else {
                src.name.clone()
            };
            let mut url = src.url.trim().to_string();
            if url
                .get(..9)
                .is_some_and(|p| p.eq_ignore_ascii_case("webcal://"))
            {
                url = format!("https://{}", &url[9..]);
            }
            match (self.fetch)(url).await {
                Ok(text) => {
                    let zone = self.zone.clone();
                    let parsed =
                        tokio::task::spawn_blocking(move || ics::parse(&text, from, to, &zone))
                            .await
                            .unwrap_or_default();
                    let color = src.color.clone().unwrap_or_else(|| DEFAULT_COLOR.into());
                    let fresh: Vec<CalendarEvent> = parsed
                        .into_iter()
                        .map(|e| CalendarEvent {
                            calendar: name.clone(),
                            color: color.clone(),
                            ..e
                        })
                        .collect();
                    lock(&self.last_good).insert(src.url.clone(), fresh.clone());
                    events.extend(fresh);
                }
                Err(message) => {
                    errors.push(format!("{name}: {message}"));
                    // keep the last good events of this feed (by its address: names are optional and may repeat)
                    if let Some(last) = lock(&self.last_good).get(&src.url) {
                        events.extend(last.iter().cloned());
                    }
                }
            }
        }
        lock(&self.last_good).retain(|url, _| sources.iter().any(|s| s.url == *url));
        let count = events.len();
        // by start, then title (ordinal, as C# under InvariantGlobalization); a stable sort
        events.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.title.cmp(&b.title)));
        let detail = if errors.is_empty() {
            String::new()
        } else {
            format!("; {}", errors.join("; "))
        };
        *lock(&self.snap) = Arc::new(CalendarSnapshot {
            events,
            updated: crate::server::now_ms(),
            errors,
        });
        if !sources.is_empty() {
            crate::log::info(CATEGORY, &format!("calendars: {count} events{detail}"));
        }
    }

    /// The event happening now (not all-day), if any; with names, only one whose title contains one of them
    /// (case-insensitive).
    pub fn current(&self, names: &[String]) -> Option<CalendarEvent> {
        let now = crate::server::now_ms();
        let matches = |title: &str| {
            names.is_empty() || {
                let title = title.to_uppercase();
                names.iter().any(|n| title.contains(&n.to_uppercase()))
            }
        };
        self.snapshot()
            .events
            .iter()
            .find(|e| !e.all_day && e.start <= now && e.end > now && matches(&e.title))
            .cloned()
    }
}

/// The configured calendars with an address.
fn sources(cfg: &crate::config::AgentConfig) -> Vec<CalendarSource> {
    cfg.calendars
        .iter()
        .filter(|c| !c.url.trim().is_empty())
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;
