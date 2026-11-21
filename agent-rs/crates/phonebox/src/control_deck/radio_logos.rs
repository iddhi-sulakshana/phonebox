//! `ControlDeck.RadioLogos.cs`: the logo on a station's card. A configured icon URL is downloaded (always); with online
//! lookups on, radio-browser.info and then TuneIn are searched by the station's name; otherwise (or when nothing is
//! found within 8 s) a monogram SVG stands in. Rasters only (JPEG, PNG, WebP, GIF, ICO), from public addresses.
//! Kept in memory (128) and on disk in `<local>/radio-logos` as `<key>.bin` + `<key>.mime` for 14 days — a folder the
//! C# agent shares, so the names, the key and the prune rules are the same.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::fetch::{Fetch, FetchRequest, FetchResponse, read_bounded};
use super::radio_directory::is_safe_public_uri;
use crate::config::DeckAction;
use crate::dotnet_text::{upper_invariant, utf16_len};

const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SEARCH_BYTES: usize = 1 << 20;
const MAX_REDIRECTS: usize = 5;
const MAX_MEMORY_ENTRIES: usize = 128;
const MAX_DISK_ENTRIES: usize = 256;
const DISK_TTL_MS: i64 = 14 * 24 * 3600 * 1000;
const FALLBACK_TTL_MS: i64 = 5 * 60 * 1000;
const TEMP_TTL_MS: i64 = 5 * 60 * 1000;
const BUDGET: Duration = Duration::from_secs(8);
const DOWNLOAD: Duration = Duration::from_millis(2500);
const RADIO_BROWSER_SEARCH: Duration = Duration::from_secs(3);
const TUNEIN_SEARCH: Duration = Duration::from_secs(2);
pub const SVG: &str = "image/svg+xml";

static PARENTHESES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*\([^)]*\)\s*").expect("regex"));

/// A station's logo, and whether it is the monogram stand-in.
#[derive(Debug, Clone, PartialEq)]
pub struct RadioLogo {
    pub bytes: Arc<Vec<u8>>,
    pub mime: String,
    pub is_fallback: bool,
}

struct Entry {
    logo: RadioLogo,
    expires_at: i64,
    seq: u64,
}

type Pending = Shared<BoxFuture<'static, RadioLogo>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn modified_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    })
}

pub struct RadioLogoCache {
    me: Weak<RadioLogoCache>,
    dir: PathBuf,
    fetch: Fetch,
    memory: Mutex<HashMap<String, Entry>>,
    seq: AtomicU64,
    /// One resolution per key at a time, shared by its callers and finished even when they all go away.
    inflight: Mutex<HashMap<String, Pending>>,
}

impl RadioLogoCache {
    /// The cache in `dir` over the real client (4 s to connect, 6 s per exchange, no Accept header).
    pub fn new(dir: PathBuf) -> Arc<RadioLogoCache> {
        RadioLogoCache::with_fetch(
            dir,
            super::fetch::public_fetch(Duration::from_secs(4), Duration::from_secs(6), None),
        )
    }

    pub fn with_fetch(dir: PathBuf, fetch: Fetch) -> Arc<RadioLogoCache> {
        Arc::new_cyclic(|me| RadioLogoCache {
            me: me.clone(),
            dir,
            fetch,
            memory: Mutex::default(),
            seq: AtomicU64::new(0),
            inflight: Mutex::default(),
        })
    }

    /// The station's logo: from memory, else one shared resolution (disk, download, search, monogram).
    pub async fn get(&self, action: &DeckAction, allow_online: bool) -> RadioLogo {
        let key = cache_key(action, allow_online);
        if let Some(hit) = lock(&self.memory)
            .get(&key)
            .filter(|e| e.expires_at > unix_ms())
        {
            return hit.logo.clone();
        }
        let pending = {
            let mut inflight = lock(&self.inflight);
            match inflight.get(&key) {
                Some(pending) => pending.clone(),
                None => {
                    let Some(me) = self.me.upgrade() else {
                        return fallback(&action.label);
                    };
                    let (work_key, label, action) =
                        (key.clone(), action.label.clone(), action.clone());
                    let work = tokio::spawn(async move {
                        me.resolve_shared(work_key, action, allow_online).await
                    });
                    let pending: Pending =
                        async move { work.await.unwrap_or_else(|_| fallback(&label)) }
                            .boxed()
                            .shared();
                    inflight.insert(key.clone(), pending.clone());
                    pending
                }
            }
        };
        pending.await
    }

    /// One resolution within the 8 s budget; past it, the monogram for 5 minutes.
    async fn resolve_shared(
        &self,
        key: String,
        action: DeckAction,
        allow_online: bool,
    ) -> RadioLogo {
        let logo =
            match tokio::time::timeout(BUDGET, self.resolve_and_cache(&key, &action, allow_online))
                .await
            {
                Ok(logo) => logo,
                Err(_) => {
                    let logo = fallback(&action.label);
                    self.store_memory(&key, logo.clone(), unix_ms() + FALLBACK_TTL_MS, unix_ms());
                    logo
                }
            };
        lock(&self.inflight).remove(&key);
        logo
    }

    async fn resolve_and_cache(
        &self,
        key: &str,
        action: &DeckAction,
        allow_online: bool,
    ) -> RadioLogo {
        let now = unix_ms();
        let disk = {
            let (dir, key) = (self.dir.clone(), key.to_string());
            tokio::task::spawn_blocking(move || read_disk(&dir, &key, now))
                .await
                .ok()
                .flatten()
        };
        if let Some((bytes, mime)) = disk {
            let logo = RadioLogo {
                bytes: Arc::new(bytes),
                mime,
                is_fallback: false,
            };
            self.store_memory(key, logo.clone(), now + DISK_TTL_MS, now);
            return logo;
        }
        if let Some((bytes, mime)) = self.fetch_logo(action, allow_online).await {
            let bytes = Arc::new(bytes);
            {
                let (dir, key, bytes, mime) = (
                    self.dir.clone(),
                    key.to_string(),
                    bytes.clone(),
                    mime.clone(),
                );
                let _ = tokio::task::spawn_blocking(move || write_disk(&dir, &key, &bytes, &mime))
                    .await;
            }
            let logo = RadioLogo {
                bytes,
                mime,
                is_fallback: false,
            };
            self.store_memory(key, logo.clone(), now + DISK_TTL_MS, now);
            return logo;
        }
        let logo = fallback(&action.label);
        self.store_memory(key, logo.clone(), now + FALLBACK_TTL_MS, now);
        logo
    }

    /// At 128 entries the expired ones go, then the ones that expire first (the oldest first among equals).
    fn store_memory(&self, key: &str, logo: RadioLogo, expires_at: i64, now: i64) {
        let mut memory = lock(&self.memory);
        if memory.len() >= MAX_MEMORY_ENTRIES {
            memory.retain(|_, e| e.expires_at > now);
            if !memory.contains_key(key) && memory.len() >= MAX_MEMORY_ENTRIES {
                let evict = memory.len() - MAX_MEMORY_ENTRIES + 1;
                let mut order: Vec<(i64, u64, String)> = memory
                    .iter()
                    .map(|(k, e)| (e.expires_at, e.seq, k.clone()))
                    .collect();
                order.sort();
                for (_, _, old) in order.into_iter().take(evict) {
                    memory.remove(&old);
                }
            }
        }
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        memory.insert(
            key.to_string(),
            Entry {
                logo,
                expires_at,
                seq,
            },
        );
    }

    /// The configured icon first (even with lookups off), then the directory searches.
    async fn fetch_logo(
        &self,
        action: &DeckAction,
        allow_online: bool,
    ) -> Option<(Vec<u8>, String)> {
        if let Some(icon) = action
            .icon
            .as_deref()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            && let Some(url) = http_url(icon)
            && let Some(found) = self.download_raster(url).await
        {
            return Some(found);
        }
        if !allow_online {
            return None;
        }
        let name = clean_search_name(&action.label);
        if utf16_len(&name) >= 2 && !normalize_key(&name).is_empty() {
            if let Some(found) = self.search_radio_browser(&name).await {
                return Some(found);
            }
            if let Some(found) = self.search_tunein(&name).await {
                return Some(found);
            }
        }
        None
    }

    /// radio-browser.info's stations by name: the first favicon (not TuneIn's) of a matching name that downloads.
    async fn search_radio_browser(&self, name: &str) -> Option<(Vec<u8>, String)> {
        let target = normalize_key(name);
        if target.is_empty() {
            return None;
        }
        let url = format!(
            "https://de1.api.radio-browser.info/json/stations/search?name={}&order=clickcount&reverse=true&limit=6&hidebroken=true",
            crate::net::escape_data_string(name)
        );
        let root = tokio::time::timeout(RADIO_BROWSER_SEARCH, self.search(&url))
            .await
            .ok()??;
        let Value::Array(items) = root else {
            return None;
        };
        for item in &items {
            let found = normalize_key(item.get("name").and_then(Value::as_str).unwrap_or(""));
            if found.is_empty()
                || !(found == target || found.contains(&target) || target.contains(&found))
            {
                continue;
            }
            let Some(favicon) = item.get("favicon").and_then(Value::as_str) else {
                continue;
            };
            if favicon.trim().is_empty() || favicon.to_lowercase().contains("tunein.com") {
                continue;
            }
            if let Some(url) = http_url(favicon.trim())
                && let Some(image) = self.download_raster(url).await
            {
                return Some(image);
            }
        }
        None
    }

    /// TuneIn's stations: the two best name matches (equal, starts with, contains) that have a real image.
    async fn search_tunein(&self, name: &str) -> Option<(Vec<u8>, String)> {
        let target = normalize_key(name);
        if target.is_empty() {
            return None;
        }
        let url = format!(
            "https://opml.radiotime.com/Search.ashx?query={}&types=station&render=json",
            crate::net::escape_data_string(name)
        );
        let root = tokio::time::timeout(TUNEIN_SEARCH, self.search(&url))
            .await
            .ok()??;
        let Some(Value::Array(body)) = root.get("body") else {
            return None;
        };
        let mut candidates: Vec<(i32, String)> = Vec::new();
        for item in body {
            let Some(image) = item.get("image").and_then(Value::as_str) else {
                continue;
            };
            let lower = image.to_lowercase();
            if image.trim().is_empty() || lower.contains("s0q.png") || lower.contains("logod.png") {
                continue;
            }
            let text = normalize_key(item.get("text").and_then(Value::as_str).unwrap_or(""));
            if text.is_empty() {
                continue;
            }
            let score = if text == target {
                3
            } else if text.starts_with(&target) {
                2
            } else if text.contains(&target) {
                1
            } else {
                0
            };
            if score > 0 {
                candidates.push((score, image.trim().to_string()));
            }
        }
        candidates.sort_by_key(|(score, _)| -score);
        for (_, image) in candidates.into_iter().take(2) {
            if let Some(url) = http_url(&image)
                && let Some(found) = self.download_raster(url).await
            {
                return Some(found);
            }
        }
        None
    }

    /// A directory search's JSON answer, at most 1 MB.
    async fn search(&self, url: &str) -> Option<Value> {
        let res = self.send_public_get(reqwest::Url::parse(url).ok()?).await?;
        if !(200..=299).contains(&res.status) {
            return None;
        }
        let body = read_bounded(res, MAX_SEARCH_BYTES).await?;
        serde_json::from_slice(&body).ok()
    }

    /// A raster image (2.5 s, at most 2 MiB, at least 32 bytes, a known format by its first bytes).
    async fn download_raster(&self, url: reqwest::Url) -> Option<(Vec<u8>, String)> {
        let work = async {
            let res = self.send_public_get(url).await?;
            if !(200..=299).contains(&res.status) {
                return None;
            }
            let bytes = read_bounded(res, MAX_IMAGE_BYTES).await?;
            if bytes.len() < 32 {
                return None;
            }
            let mime = detect_raster_mime(&bytes)?;
            Some((bytes, mime.to_string()))
        };
        tokio::time::timeout(DOWNLOAD, work).await.ok().flatten()
    }

    /// A GET following redirects itself; None for anything not public, another scheme, https to anything else, more
    /// than 5 hops, or a transport failure.
    async fn send_public_get(&self, url: reqwest::Url) -> Option<FetchResponse> {
        let mut current = url;
        for _ in 0..=MAX_REDIRECTS {
            if !is_safe_public_uri(&current) {
                return None;
            }
            let res = (self.fetch)(FetchRequest::get(current.clone()))
                .await
                .ok()?;
            if (300..=399).contains(&res.status)
                && let Some(location) = res.location.clone()
            {
                drop(res);
                let next = current.join(&location).ok()?;
                if !matches!(next.scheme(), "http" | "https")
                    || (current.scheme() == "https" && next.scheme() != "https")
                    || !is_safe_public_uri(&next)
                {
                    return None;
                }
                current = next;
                continue;
            }
            return Some(res);
        }
        None
    }
}

/// An absolute http(s) URL.
fn http_url(text: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(text)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
}

fn fallback(label: &str) -> RadioLogo {
    RadioLogo {
        bytes: Arc::new(monogram_svg(label)),
        mime: SVG.into(),
        is_fallback: true,
    }
}

/// The image format by its first bytes (SVG is never taken from the network).
pub fn detect_raster_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        Some("image/x-icon")
    } else {
        None
    }
}

/// The label without parenthetical parts ("Power FM (Turkey)" → "Power FM").
pub fn clean_search_name(label: &str) -> String {
    if label.is_empty() {
        return String::new();
    }
    PARENTHESES.replace_all(label, " ").trim().to_string()
}

/// A name as lower-case ASCII letters and digits (Turkish and circumflexed letters folded) for matching.
pub fn normalize_key(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'İ' | 'I' | 'ı' | 'î' | 'Î' => 'i',
            'Ş' | 'ş' => 's',
            'Ğ' | 'ğ' => 'g',
            'Ü' | 'ü' | 'û' | 'Û' => 'u',
            'Ö' | 'ö' => 'o',
            'Ç' | 'ç' => 'c',
            'Â' | 'â' => 'a',
            other => {
                let mut lower = other.to_lowercase();
                match (lower.next(), lower.next()) {
                    (Some(single), None) => single,
                    _ => other,
                }
            }
        })
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

/// `WebUtility.HtmlEncode`: the five markup characters, U+00A0–U+00FF and characters past U+FFFF as references.
fn html_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\u{A0}'..='\u{FF}' => out.push_str(&format!("&#{};", c as u32)),
            c if c as u32 > 0xFFFF => out.push_str(&format!("&#{};", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The monogram stand-in: the station's initials on a gradient whose hue comes from its name.
pub fn monogram_svg(label: &str) -> Vec<u8> {
    let clean = clean_search_name(label);
    let words: Vec<&str> = clean
        .split([' ', '-', '_', '.'])
        .filter(|w| !w.is_empty())
        .collect();
    let first = |word: &str, count: usize| word.chars().take(count).collect::<String>();
    let initials = match words.len() {
        0 => "FM".to_string(),
        1 => upper_invariant(&first(words[0], 2)),
        _ => upper_invariant(&(first(words[0], 1) + &first(words[1], 1))),
    };
    let hash = Sha256::digest(clean.as_bytes());
    let hue = u32::from(hash[0]) * 360 / 256;
    let hue2 = (hue + 38) % 360;
    let escaped = html_encode(&initials);
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 160 160\">\n  <defs>\n    <linearGradient id=\"g\" x1=\"0\" y1=\"0\" x2=\"1\" y2=\"1\">\n      <stop offset=\"0%\" stop-color=\"hsl({hue},72%,36%)\"/>\n      <stop offset=\"100%\" stop-color=\"hsl({hue2},78%,18%)\"/>\n    </linearGradient>\n  </defs>\n  <rect width=\"160\" height=\"160\" rx=\"28\" fill=\"url(#g)\"/>\n  <circle cx=\"80\" cy=\"80\" r=\"60\" fill=\"none\" stroke=\"rgba(255,255,255,0.14)\" stroke-width=\"2\"/>\n  <text x=\"80\" y=\"96\" text-anchor=\"middle\" font-family=\"system-ui,-apple-system,sans-serif\" font-size=\"52\" font-weight=\"800\" letter-spacing=\"1\" fill=\"#ffffff\">{escaped}</text>\n</svg>"
    )
    .into_bytes()
}

/// The logo's cache key (and, its first 12 digits, the card's `rev`): what decides the logo, hashed.
pub fn cache_key(action: &DeckAction, allow_online: bool) -> String {
    let raw = format!(
        "{}\n{}\n{}\n{}",
        action.icon.as_deref().map(str::trim).unwrap_or(""),
        action.label.trim(),
        action.target.trim(),
        if allow_online { "1" } else { "0" }
    );
    let digest = Sha256::digest(raw.as_bytes());
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..24]
        .to_string()
}

/// A logo on disk, if both files are there, it is less than 14 days old (a future date counts as stale) and it is the
/// raster its `.mime` names. A stale one is deleted. Blocks.
fn read_disk(dir: &Path, key: &str, now: i64) -> Option<(Vec<u8>, String)> {
    let bin = dir.join(format!("{key}.bin"));
    let mime_path = dir.join(format!("{key}.mime"));
    if !bin.is_file() || !mime_path.is_file() {
        return None;
    }
    let age = now - modified_ms(&bin)?;
    if !(0..=DISK_TTL_MS).contains(&age) {
        let _ = std::fs::remove_file(&bin);
        let _ = std::fs::remove_file(&mime_path);
        return None;
    }
    let bytes = std::fs::read(&bin).ok()?;
    let mime = std::fs::read_to_string(&mime_path).ok()?.trim().to_string();
    (bytes.len() >= 32 && detect_raster_mime(&bytes) == Some(mime.as_str()))
        .then_some((bytes, mime))
}

/// Writes a logo through temporary files (the `.mime` moved in first), then prunes. Errors are ignored. Blocks.
fn write_disk(dir: &Path, key: &str, bytes: &[u8], mime: &str) {
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let suffix = format!(".{}.tmp", &super::leases::nonce()[..8]);
        let temp_mime = dir.join(format!("{key}.mime{suffix}"));
        let temp_bin = dir.join(format!("{key}.bin{suffix}"));
        std::fs::write(&temp_mime, mime)?;
        std::fs::write(&temp_bin, bytes)?;
        std::fs::rename(&temp_mime, dir.join(format!("{key}.mime")))?;
        std::fs::rename(&temp_bin, dir.join(format!("{key}.bin")))?;
        Ok(())
    };
    if write().is_ok() {
        prune_disk(dir);
    }
}

/// Removes temporary files older than 5 minutes, logos older than 14 days, `.mime` files without their logo, and the
/// oldest logos past 256.
fn prune_disk(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = unix_ms();
    let files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    let extension = |p: &Path| {
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
    };
    let stem = |p: &Path| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string()
    };
    for temp in files
        .iter()
        .filter(|p| extension(p).as_deref() == Some("tmp"))
    {
        if modified_ms(temp).is_some_and(|at| now - at > TEMP_TTL_MS) {
            let _ = std::fs::remove_file(temp);
        }
    }
    let mut bins: Vec<(i64, PathBuf)> = Vec::new();
    let mut active: HashSet<String> = HashSet::new();
    for bin in files
        .iter()
        .filter(|p| extension(p).as_deref() == Some("bin"))
    {
        let at = modified_ms(bin).unwrap_or(now);
        if now - at > DISK_TTL_MS {
            let _ = std::fs::remove_file(bin);
            let _ = std::fs::remove_file(dir.join(format!("{}.mime", stem(bin))));
        } else {
            active.insert(stem(bin).to_lowercase());
            bins.push((at, bin.clone()));
        }
    }
    for mime in files
        .iter()
        .filter(|p| extension(p).as_deref() == Some("mime"))
    {
        if !active.contains(&stem(mime).to_lowercase()) {
            let _ = std::fs::remove_file(mime);
        }
    }
    if bins.len() > MAX_DISK_ENTRIES {
        bins.sort_by_key(|(at, _)| *at);
        let extra = bins.len() - MAX_DISK_ENTRIES;
        for (_, old) in bins.into_iter().take(extra) {
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::remove_file(dir.join(format!("{}.mime", stem(&old))));
        }
    }
}

#[cfg(test)]
#[path = "radio_logos_tests.rs"]
mod tests;
