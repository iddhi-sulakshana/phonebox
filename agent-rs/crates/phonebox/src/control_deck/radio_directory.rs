//! `ControlDeck.RadioDirectory.cs`: importing stations from radio-browser.info (countries, a country's popular
//! stations) and checking that stream addresses play. Anyone can add a station to the directory, so only http(s) to a
//! public address is accepted: no single-label hosts, local suffixes or names carrying a private address, and the
//! client connects to public addresses only and follows redirects itself (at most 5, never https to http).

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use super::fetch::{Fetch, FetchError, FetchRequest, FetchResponse, read_bounded};
use crate::dotnet_text::{lower_invariant, upper_invariant, utf16_len, utf16_prefix};

/// The text the phone shows when no mirror answered.
pub const UNAVAILABLE: &str = "Could not reach radio-browser.info mirrors. Check your internet connection or try again shortly.";

const FALLBACK_MIRRORS: [&str; 5] = [
    "de1.api.radio-browser.info",
    "fi1.api.radio-browser.info",
    "nl1.api.radio-browser.info",
    "at1.api.radio-browser.info",
    "de2.api.radio-browser.info",
];
const MAX_REDIRECTS: usize = 5;
const MAX_DIRECTORY_BYTES: usize = 4 * 1024 * 1024;
const ATTEMPT: Duration = Duration::from_secs(5);
const DISCOVERY: Duration = Duration::from_secs(2);
const MAX_CHECKED: usize = 100;
const CHECKS_AT_ONCE: usize = 6;
const LOCAL_SUFFIXES: [&str; 12] = [
    ".localhost",
    ".local",
    ".lan",
    ".home",
    ".home.arpa",
    ".internal",
    ".intranet",
    ".localdomain",
    ".corp",
    ".private",
    ".test",
    ".invalid",
];

static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("regex"));
static NOT_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").expect("regex"));

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadioCountry {
    pub code: String,
    pub name: String,
    pub station_count: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadioStation {
    pub id: String,
    pub label: String,
    pub stream_url: String,
    pub logo_url: Option<String>,
    pub click_count: i32,
    pub votes: i32,
}

/// One station of a check request (either part may be missing).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CheckItem {
    pub id: Option<String>,
    pub stream_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckResult {
    pub id: Option<String>,
    pub ok: bool,
    pub status: String,
}

/// The directory client. The mirror that answered last is asked first next time (C#'s process-wide static, kept here).
pub struct RadioDirectory {
    fetch: Fetch,
    /// Look for mirrors in DNS (the real client only).
    discover: bool,
    preferred: Mutex<Option<String>>,
}

impl RadioDirectory {
    /// The real client (10 s per exchange) with mirror discovery.
    pub fn new() -> RadioDirectory {
        RadioDirectory {
            fetch: super::fetch::public_fetch(Duration::from_secs(10)),
            discover: true,
            preferred: Mutex::new(None),
        }
    }

    /// Over another transport, without discovery (tests).
    pub fn with_fetch(fetch: Fetch) -> RadioDirectory {
        RadioDirectory {
            fetch,
            discover: false,
            preferred: Mutex::new(None),
        }
    }

    /// Countries with stations, by name.
    pub async fn countries(&self) -> Result<Vec<RadioCountry>, String> {
        Ok(parse_countries(
            &self.directory_json("countries?hidebroken=true").await?,
        ))
    }

    /// A country's popular stations (by clicks): `limit` 1–100 (40 when not above 0). A code that is not two letters
    /// asks nothing.
    pub async fn stations(&self, country: &str, limit: i32) -> Result<Vec<RadioStation>, String> {
        let code = upper_invariant(country.trim());
        if utf16_len(&code) != 2 || !code.chars().all(char::is_alphabetic) {
            return Ok(vec![]);
        }
        let max = if limit <= 0 { 40 } else { limit }.clamp(1, 100);
        let relative = format!(
            "stations/search?countrycode={}&hidebroken=true&order=clickcount&reverse=true&limit={}",
            crate::net::escape_data_string(&code),
            max * 2
        );
        Ok(parse_stations(
            &self.directory_json(&relative).await?,
            &code,
            max as usize,
        ))
    }

    /// Checks up to 100 stream addresses, 6 at a time, in the order given.
    pub async fn check(&self, items: Vec<Option<CheckItem>>) -> Vec<CheckResult> {
        futures_util::stream::iter(items.into_iter().take(MAX_CHECKED))
            .map(|item| probe(&self.fetch, item))
            .buffered(CHECKS_AT_ONCE)
            .collect()
            .await
    }

    /// The mirrors to try: the one that answered last, then (with discovery) the ones DNS names, then the fixed list.
    async fn mirrors(&self) -> Vec<String> {
        let mut list: Vec<String> = Vec::new();
        let mut add = |host: &str| {
            let host = host.trim();
            if !host.is_empty() && !list.iter().any(|h| h.eq_ignore_ascii_case(host)) {
                list.push(host.to_string());
            }
        };
        if let Some(preferred) = self
            .preferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            add(&preferred);
        }
        if self.discover
            && let Ok(found) = tokio::time::timeout(DISCOVERY, discover_mirrors()).await
        {
            for host in found {
                add(&host);
            }
        }
        for host in FALLBACK_MIRRORS {
            add(host);
        }
        list
    }

    /// The first mirror's answer that is a JSON array (each asked for at most 5 s), or [`UNAVAILABLE`].
    async fn directory_json(&self, relative: &str) -> Result<String, String> {
        for host in self.mirrors().await {
            let Ok(url) = reqwest::Url::parse(&format!("https://{host}/json/{relative}")) else {
                continue;
            };
            let attempt = async {
                let res = send_public_get(&self.fetch, url).await.ok()??;
                if !(200..=299).contains(&res.status) {
                    return None;
                }
                let bytes = read_bounded(res, MAX_DIRECTORY_BYTES).await?;
                // a UTF-8 byte-order mark is dropped, as ReadAsStringAsync did
                let bytes = bytes
                    .strip_prefix(&[0xEF, 0xBB, 0xBF][..])
                    .unwrap_or(&bytes);
                let body = String::from_utf8_lossy(bytes).into_owned();
                serde_json::from_str::<Value>(&body)
                    .is_ok_and(|v| v.is_array())
                    .then_some(body)
            };
            if let Ok(Some(body)) = tokio::time::timeout(ATTEMPT, attempt).await {
                if self.discover {
                    *self.preferred.lock().unwrap_or_else(|e| e.into_inner()) = Some(host);
                }
                return Ok(body);
            }
        }
        Err(UNAVAILABLE.into())
    }
}

impl Default for RadioDirectory {
    fn default() -> Self {
        RadioDirectory::new()
    }
}

/// The mirrors `all.api.radio-browser.info` points at: up to 4 public addresses, each named by its PTR record.
async fn discover_mirrors() -> Vec<String> {
    let Ok(found) = tokio::net::lookup_host(("all.api.radio-browser.info", 0)).await else {
        return vec![];
    };
    let addresses: Vec<IpAddr> = found
        .map(|a| a.ip())
        .filter(|ip| is_public_ip(*ip))
        .take(4)
        .collect();
    let mut hosts = Vec::new();
    for ip in addresses {
        if let Ok(Some(name)) = tokio::task::spawn_blocking(move || reverse_lookup(ip)).await {
            let name = name.trim_end_matches('.').to_string();
            if name
                .to_ascii_lowercase()
                .ends_with(".api.radio-browser.info")
            {
                hosts.push(name);
            }
        }
    }
    hosts
}

/// A GET that follows redirects itself: every hop must be a safe public URL (else [`FetchError::NonPublic`]); None for
/// a redirect to another scheme, from https to http, or more than 5 hops.
async fn send_public_get(
    fetch: &Fetch,
    url: reqwest::Url,
) -> Result<Option<FetchResponse>, FetchError> {
    let mut current = url;
    for _ in 0..=MAX_REDIRECTS {
        if !is_safe_public_uri(&current) {
            return Err(FetchError::NonPublic);
        }
        let mut request = FetchRequest::get(current.clone());
        request.headers.push(("Icy-MetaData", "0".into()));
        let res = fetch(request).await?;
        if (300..=399).contains(&res.status)
            && let Some(location) = res.location.clone()
        {
            drop(res);
            let Ok(next) = current.join(&location) else {
                return Ok(None);
            };
            if !matches!(next.scheme(), "http" | "https")
                || (current.scheme() == "https" && next.scheme() == "http")
            {
                return Ok(None);
            }
            if !is_safe_public_uri(&next) {
                return Err(FetchError::NonPublic);
            }
            current = next;
            continue;
        }
        return Ok(Some(res));
    }
    Ok(None)
}

/// One stream check (5 s): "Live", or why not.
async fn probe(fetch: &Fetch, item: Option<CheckItem>) -> CheckResult {
    let Some(item) = item else {
        return CheckResult {
            id: Some(String::new()),
            ok: false,
            status: "Invalid URL".into(),
        };
    };
    let result = |ok: bool, status: &str| CheckResult {
        id: item.id.clone(),
        ok,
        status: status.into(),
    };
    let url = item
        .stream_url
        .as_deref()
        .and_then(|s| reqwest::Url::parse(s).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    let Some(url) = url else {
        return result(false, "Invalid URL");
    };
    if !is_safe_public_uri(&url) {
        return result(false, "local address, not checked");
    }
    match tokio::time::timeout(ATTEMPT, send_public_get(fetch, url)).await {
        Err(_) | Ok(Err(FetchError::Timeout)) => result(false, "Timed out"),
        Ok(Err(FetchError::NonPublic)) => result(false, "local address, not checked"),
        Ok(Err(FetchError::HostNotFound)) => result(false, "Host not found"),
        Ok(Err(FetchError::Refused)) => result(false, "Connection refused"),
        Ok(Err(FetchError::Unreachable)) => result(false, "Unreachable"),
        Ok(Err(FetchError::Other(_))) => result(false, "Connection failed"),
        Ok(Ok(None)) => result(false, "Invalid redirect"),
        Ok(Ok(Some(res))) if !(200..=299).contains(&res.status) => {
            result(false, &format!("HTTP {}", res.status))
        }
        Ok(Ok(Some(res))) => {
            let media_type = res
                .content_type
                .as_deref()
                .and_then(|t| t.split(';').next())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if media_type.contains("text/html") {
                result(false, "HTML page (not a stream)")
            } else {
                result(true, "Live")
            }
        }
    }
}

/// A JSON string member, or None.
fn text<'a>(e: &'a Value, key: &str) -> Option<&'a str> {
    e.get(key).and_then(Value::as_str)
}

/// A JSON number member that fits an int32, else 0.
fn int(e: &Value, key: &str) -> i32 {
    e.get(key)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(0)
}

/// The directory's countries: two-letter codes with stations, one per code (the higher count wins), by name.
pub fn parse_countries(json: &str) -> Vec<RadioCountry> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return vec![];
    };
    let mut countries: Vec<RadioCountry> = Vec::new();
    for e in &items {
        let code = text(e, "iso_3166_1")
            .map(|c| upper_invariant(c.trim()))
            .unwrap_or_default();
        let name = text(e, "name")
            .map(|n| n.trim().to_string())
            .unwrap_or_default();
        let count = int(e, "stationcount");
        if code.is_empty() || utf16_len(&code) != 2 || name.is_empty() || count <= 0 {
            continue;
        }
        match countries.iter_mut().find(|c| eq_ci(&c.code, &code)) {
            Some(prev) if count > prev.station_count => {
                *prev = RadioCountry {
                    code,
                    name,
                    station_count: count,
                }
            }
            Some(_) => {}
            None => countries.push(RadioCountry {
                code,
                name,
                station_count: count,
            }),
        }
    }
    countries.sort_by_key(|c| upper_invariant(&c.name));
    countries
}

/// The stations of a search: safe public streams, each name and stream once, at most `max`.
pub fn parse_stations(json: &str, country: &str, max: usize) -> Vec<RadioStation> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return vec![];
    };
    let country = lower_invariant(country);
    let mut seen_streams: HashSet<String> = HashSet::new();
    // compared ignoring case
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut stations = Vec::new();
    for e in &items {
        let raw_name = text(e, "name").map(str::trim).unwrap_or("");
        let resolved = text(e, "url_resolved").map(str::trim).unwrap_or("");
        let stream = if resolved.is_empty() {
            text(e, "url").map(str::trim).unwrap_or("")
        } else {
            resolved
        };
        if raw_name.is_empty() || stream.is_empty() {
            continue;
        }
        let Some(url) = reqwest::Url::parse(stream).ok().filter(is_safe_public_uri) else {
            continue;
        };
        let name = clean_station_name(raw_name);
        if name.is_empty() || seen_names.contains(&name.to_uppercase()) {
            continue;
        }
        // scheme and host in lower case, no default port, the path and query as written, no fragment
        let mut key = url.clone();
        key.set_fragment(None);
        let key = key.to_string();
        if !seen_streams.insert(key) {
            continue;
        }
        seen_names.insert(name.to_uppercase());
        let favicon = text(e, "favicon").map(str::trim).unwrap_or("");
        let logo_url = (!favicon.is_empty()
            && reqwest::Url::parse(favicon)
                .ok()
                .is_some_and(|u| is_safe_public_uri(&u)))
        .then(|| favicon.to_string());
        stations.push(RadioStation {
            id: format!("radio-{country}-{}", slugify(&name)),
            label: name,
            stream_url: stream.to_string(),
            logo_url,
            click_count: int(e, "clickcount"),
            votes: int(e, "votes"),
        });
        if stations.len() >= max {
            break;
        }
    }
    stations
}

/// White space collapsed, at most 48 UTF-16 units.
pub fn clean_station_name(name: &str) -> String {
    let s = SPACES.replace_all(name.trim(), " ").into_owned();
    if utf16_len(&s) > 48 {
        utf16_prefix(&s, 48).trim().to_string()
    } else {
        s
    }
}

/// A station id's tail: Turkish letters folded to ASCII, anything else but a–z and 0–9 a dash, at most 28.
pub fn slugify(text: &str) -> String {
    let lower = lower_invariant(&text.replace(['İ', 'I'], "i"))
        .replace('ı', "i")
        .replace('ğ', "g")
        .replace('ü', "u")
        .replace('ş', "s")
        .replace('ö', "o")
        .replace('ç', "c");
    let slug = NOT_SLUG.replace_all(&lower, "-");
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "station".into()
    } else if slug.len() > 28 {
        slug[..28].trim_matches('-').to_string()
    } else {
        slug.to_string()
    }
}

/// `OrdinalIgnoreCase`.
fn eq_ci(a: &str, b: &str) -> bool {
    a.to_uppercase() == b.to_uppercase()
}

/// A URL from the public directory that may be imported or fetched: http(s) to a public address, or a host name that
/// is not single-label, has no local suffix and carries no private address.
pub fn is_safe_public_uri(url: &reqwest::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    // the parser already gives an address host in its canonical form (an IPv6 one in brackets)
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');
    if host.trim().is_empty() {
        return false;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_public_ip(ip);
    }
    if !host.contains('.') {
        return false;
    }
    let lower = host.to_ascii_lowercase();
    if LOCAL_SUFFIXES
        .iter()
        .any(|suffix| lower == suffix[1..] || lower.ends_with(suffix))
    {
        return false;
    }
    !embedded_ipv4(host)
        .iter()
        .filter_map(|m| dotnet_ipv4(&m.replace('-', ".")))
        .any(|ip| !is_public_ip(IpAddr::V4(ip)))
}

/// The matches of `(?<![0-9])(?:[0-9]{1,3}[.-]){3}[0-9]{1,3}(?![0-9])`: four runs of 1–3 digits joined by dots or
/// dashes, not inside a longer run of digits.
pub fn embedded_ipv4(host: &str) -> Vec<String> {
    let b = host.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && b[i - 1].is_ascii_digit()) {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut matched = true;
        for group in 0..4 {
            let start = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if !(1..=3).contains(&(j - start)) {
                matched = false;
                break;
            }
            if group < 3 {
                if j < b.len() && matches!(b[j], b'.' | b'-') {
                    j += 1;
                } else {
                    matched = false;
                    break;
                }
            }
        }
        if matched {
            found.push(host[i..j].to_string());
            i = j;
        } else {
            i += 1;
        }
    }
    found
}

/// `IPAddress.TryParse` of four dotted decimal parts: a part with a leading 0 is octal (so `08` fails), each at most
/// 255.
pub fn dotnet_ipv4(text: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (octet, part) in octets.iter_mut().zip(&parts) {
        let value = if part.len() > 1 && part.starts_with('0') {
            u32::from_str_radix(part, 8).ok()?
        } else {
            part.parse::<u32>().ok()?
        };
        *octet = u8::try_from(value).ok()?;
    }
    Some(Ipv4Addr::from(octets))
}

/// [`crate::widget_feed_proxy::is_public_address`] plus the 6to4 relay range 192.88.99.0/24 (also inside a 6to4
/// address).
pub fn is_public_ip(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let b = v4.octets();
            !(b[0] == 192 && b[1] == 88 && b[2] == 99)
                && crate::widget_feed_proxy::is_public_address(ip)
        }
        IpAddr::V6(v6) => {
            let b = v6.octets();
            if b[0] == 0x20 && b[1] == 0x02 {
                return is_public_ip(IpAddr::V4(Ipv4Addr::new(b[2], b[3], b[4], b[5])));
            }
            crate::widget_feed_proxy::is_public_address(ip)
        }
    }
}

/// The host name an address's PTR record gives (`getnameinfo` with `NI_NAMEREQD`). Blocks.
fn reverse_lookup(ip: IpAddr) -> Option<String> {
    #[cfg(unix)]
    {
        // SAFETY: the address structure is fully initialised for its family and outlives the call; the buffer's length
        // is passed with it
        unsafe {
            let mut storage: libc::sockaddr_storage = std::mem::zeroed();
            let length = match ip {
                IpAddr::V4(v4) => {
                    let sin = &mut *(&mut storage as *mut libc::sockaddr_storage
                        as *mut libc::sockaddr_in);
                    sin.sin_family = libc::AF_INET as libc::sa_family_t;
                    sin.sin_addr.s_addr = u32::from_ne_bytes(v4.octets());
                    std::mem::size_of::<libc::sockaddr_in>()
                }
                IpAddr::V6(v6) => {
                    let sin6 = &mut *(&mut storage as *mut libc::sockaddr_storage
                        as *mut libc::sockaddr_in6);
                    sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                    sin6.sin6_addr.s6_addr = v6.octets();
                    std::mem::size_of::<libc::sockaddr_in6>()
                }
            };
            #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
            {
                storage.ss_len = length as u8;
            }
            let mut host = [0 as libc::c_char; 1025];
            let status = libc::getnameinfo(
                &storage as *const libc::sockaddr_storage as *const libc::sockaddr,
                length as libc::socklen_t,
                host.as_mut_ptr(),
                host.len() as libc::socklen_t,
                std::ptr::null_mut(),
                0,
                libc::NI_NAMEREQD,
            );
            if status != 0 {
                return None;
            }
            Some(
                std::ffi::CStr::from_ptr(host.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::Networking::WinSock::{
            AF_INET, AF_INET6, GetNameInfoW, NI_NAMEREQD, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
            socklen_t,
        };
        let mut host = [0u16; 1025];
        // SAFETY: the address structure is fully initialised for its family and outlives the call (Winsock is started
        // by the name lookup before it)
        let status = unsafe {
            match ip {
                IpAddr::V4(v4) => {
                    let mut sa: SOCKADDR_IN = std::mem::zeroed();
                    sa.sin_family = AF_INET;
                    sa.sin_addr.S_un.S_addr = u32::from_ne_bytes(v4.octets());
                    GetNameInfoW(
                        &sa as *const SOCKADDR_IN as *const SOCKADDR,
                        socklen_t(std::mem::size_of::<SOCKADDR_IN>() as i32),
                        Some(&mut host),
                        None,
                        NI_NAMEREQD as i32,
                    )
                }
                IpAddr::V6(v6) => {
                    let mut sa: SOCKADDR_IN6 = std::mem::zeroed();
                    sa.sin6_family = AF_INET6;
                    sa.sin6_addr.u.Byte = v6.octets();
                    GetNameInfoW(
                        &sa as *const SOCKADDR_IN6 as *const SOCKADDR,
                        socklen_t(std::mem::size_of::<SOCKADDR_IN6>() as i32),
                        Some(&mut host),
                        None,
                        NI_NAMEREQD as i32,
                    )
                }
            }
        };
        if status != 0 {
            return None;
        }
        let end = host.iter().position(|&c| c == 0).unwrap_or(host.len());
        Some(String::from_utf16_lossy(&host[..end]))
    }
}

#[cfg(test)]
#[path = "radio_directory_tests.rs"]
mod tests;
