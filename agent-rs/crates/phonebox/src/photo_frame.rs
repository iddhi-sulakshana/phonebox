//! `PhotoFrame.cs`: photos for the Photo frame widget from one folder on this computer (a synced OneDrive, Dropbox or
//! iCloud folder works too). The phone gets ids and asks for one photo at a time; large photos are scaled down here
//! first (sips on macOS, the `image` crate on Windows, none on Linux), so an old phone never decodes a camera
//! original. Nothing is scanned or served while no enabled page shows the widget, and files that are only in the
//! cloud are skipped, so a slideshow never downloads a whole synced library.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

use crate::config::store::ConfigStore;

pub const WIDGET_TYPE: &str = "photos";
const MAX_FILES: usize = 5000;
const MAX_ENTRIES: usize = 50_000;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const LONG_EDGE: u32 = 1600;
const MAX_SOURCE_BYTES: u64 = 60 * 1024 * 1024;
/// What an old phone is sent as it is: at most this many pixels and 8192 px a side.
const MAX_PHONE_PIXELS: i64 = 4096 * 4096;
const MAX_DECODE_PIXELS: i64 = 100_000_000;
const ORIGINAL_MAX_BYTES: u64 = 6 * 1024 * 1024;
const HEAD_BYTES: u64 = 256 * 1024;
const SCAN_EVERY_MS: i64 = 5 * 60_000;
const EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];
const MAX_RECURSION_DEPTH: usize = 6;
const CATEGORY: &str = "PhoneBox.Agent.PhotoFrame";

const ADD_WIDGET: &str = "Add a Photo frame widget to a page to show photos";
const UNKNOWN: &str = "Unknown photo";
const GONE: &str = "That photo is no longer available";
const CANT_SHOW: &str = "That photo can't be shown";
const TOO_LARGE: &str = "That photo is too large to show";
const SCAN_ERROR: &str = "The photo folder can't be read";

/// Windows file attributes the scan reads (`FileAttributes`).
pub(crate) const ATTR_HIDDEN: u32 = 0x2;
pub(crate) const ATTR_SYSTEM: u32 = 0x4;
#[cfg(test)]
pub(crate) const ATTR_DIRECTORY: u32 = 0x10;
#[cfg(test)]
pub(crate) const ATTR_ARCHIVE: u32 = 0x20;
#[cfg(test)]
pub(crate) const ATTR_NORMAL: u32 = 0x80;
pub(crate) const ATTR_REPARSE_POINT: u32 = 0x400;
pub(crate) const ATTR_OFFLINE: u32 = 0x1000;
pub(crate) const ATTR_RECALL_ON_OPEN: u32 = 0x0004_0000;
pub(crate) const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// One photo of the folder.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Photo {
    pub id: String,
    pub path: PathBuf,
    pub length: u64,
    /// The last write time in .NET ticks (100 ns since 0001-01-01, UTC).
    pub ticks: i64,
}

/// The photos of one selection (`key`) as of the last scan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Library {
    pub key: String,
    pub photos: Vec<Photo>,
    pub scanned_at: i64,
    pub error: Option<String>,
}

/// Lists a folder: `scan(root, subfolders, id_key)` (a test seam that can block).
pub(crate) type Scanner = Arc<dyn Fn(&str, bool, &[u8; 32]) -> Library + Send + Sync>;

/// What `/api/photos` answers.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PhotoList {
    pub ids: Vec<String>,
    pub error: Option<String>,
    pub scanning: bool,
}

/// The scan that owns the slot (C#'s `_scan` task): its id and when it is done.
#[derive(Clone)]
pub(crate) struct ScanHandle {
    #[cfg_attr(not(test), allow(dead_code))]
    pub id: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    done: watch::Receiver<bool>,
}

#[cfg(test)]
impl ScanHandle {
    /// Waits until the scan has finished (published or dropped).
    pub async fn finished(mut self) {
        let _ = self.done.wait_for(|d| *d).await;
    }
}

#[derive(Default)]
struct State {
    library: Option<Arc<Library>>,
    scan: Option<ScanHandle>,
    scan_key: Option<(String, u64)>,
    /// Bumped when the photo source setting is cleared or changed: a scan or conversion from before must not write
    /// back.
    generation: u64,
    /// Cache keys of photos that could not be converted.
    failed: HashSet<String>,
}

/// The folder chosen in the settings.
#[derive(Debug, Clone, PartialEq)]
struct Source {
    root: String,
    subfolders: bool,
    key: String,
}

pub struct PhotoFrame {
    cfg: Arc<ConfigStore>,
    /// Ids the phone sees are keyed per run, so a paired phone can't confirm guessed file names.
    id_key: [u8; 32],
    cache_dir: PathBuf,
    /// A monotonic clock in ms (`Environment.TickCount64`).
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    scanner: Scanner,
    state: Mutex<State>,
    /// Conversions run one at a time.
    convert: tokio::sync::Mutex<()>,
    next_scan: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `MimeOf`: JPEG, PNG, GIF or WebP by their first bytes; anything else (SVG, HTML…) is None.
pub fn mime_of(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Width and height from the image's own header (PNG IHDR, GIF screen, WebP VP8/VP8L/VP8X, JPEG SOFn).
pub(crate) fn size_of(b: &[u8], mime: &str) -> Option<(i64, i64)> {
    let be16 = |i: usize| i64::from(u16::from_be_bytes([b[i], b[i + 1]]));
    let le16 = |i: usize| i64::from(u16::from_le_bytes([b[i], b[i + 1]]));
    let be32 = |i: usize| i64::from(u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]));
    let le24 = |i: usize| i64::from(b[i]) | i64::from(b[i + 1]) << 8 | i64::from(b[i + 2]) << 16;
    match mime {
        "image/png" if b.len() >= 24 => Some((
            be32(16).min(i64::from(i32::MAX)),
            be32(20).min(i64::from(i32::MAX)),
        )),
        "image/gif" if b.len() >= 10 => Some((le16(6), le16(8))),
        "image/webp" if b.len() >= 30 => match &b[12..16] {
            b"VP8 " => Some((le16(26) & 0x3FFF, le16(28) & 0x3FFF)),
            b"VP8L" => {
                let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
                Some((
                    i64::from(bits & 0x3FFF) + 1,
                    i64::from((bits >> 14) & 0x3FFF) + 1,
                ))
            }
            b"VP8X" => Some((le24(24) + 1, le24(27) + 1)),
            _ => None,
        },
        "image/jpeg" => {
            let mut i = 2;
            while i + 9 < b.len() {
                if b[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let marker = b[i + 1];
                if matches!(marker, 0xD8 | 0x01 | 0xD0..=0xD7 | 0xFF) {
                    i += if marker == 0xFF { 1 } else { 2 };
                    continue;
                }
                // SOF0..SOF15 except DHT (C4), JPG (C8) and DAC (CC) carry the frame size
                if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                    return Some((be16(i + 7), be16(i + 5)));
                }
                if marker == 0xDA {
                    return None; // image data begins before any frame header: give up
                }
                i += 2 + be16(i + 2) as usize;
            }
            None
        }
        _ => None,
    }
}

/// A symlink or junction (a reparse point with a target), not a cloud-synced file or folder (no target); the target
/// is asked for only when the attribute is set.
pub(crate) fn is_link(attributes: u32, link_target: impl FnOnce() -> Option<String>) -> bool {
    attributes & ATTR_REPARSE_POINT != 0 && link_target().is_some()
}

/// Windows cloud placeholders (OneDrive Files On-Demand and others): reading them downloads the file.
pub(crate) fn cloud_only(attributes: u32) -> bool {
    attributes & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0
}

pub(crate) fn folder_key(root: &str, subfolders: bool) -> String {
    format!("folder\n{root}\n{}", if subfolders { "1" } else { "0" })
}

/// A photo's id: HMAC-SHA256 of the folder and its relative path (with `/`), the first 16 hex digits.
pub(crate) fn id_of(id_key: &[u8; 32], root: &str, relative: &str) -> String {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(id_key).expect("HMAC takes any key");
    mac.update(
        format!(
            "{}\n{}",
            trim_ending_separator(root),
            relative.replace('\\', "/")
        )
        .as_bytes(),
    );
    hex(&mac.finalize().into_bytes()[..8])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const SEP: char = if cfg!(windows) { '\\' } else { '/' };

fn is_separator(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// The root of a full path: `/`, `C:\` or `\\server\share`.
fn path_root(full: &str) -> String {
    if !cfg!(windows) {
        return if full.starts_with('/') {
            "/".into()
        } else {
            String::new()
        };
    }
    let s = full.replace('/', "\\");
    if let Some(unc) = s.strip_prefix("\\\\") {
        let mut parts = unc.splitn(3, '\\');
        let (server, share) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        return format!("\\\\{server}\\{share}");
    }
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        return format!("{}\\", &s[..2]);
    }
    String::new()
}

/// `Path.TrimEndingDirectorySeparator`: one trailing separator beyond the root goes.
fn trim_ending_separator(path: &str) -> &str {
    let root = path_root(path);
    match path.chars().last() {
        Some(c) if is_separator(c) && path.len() > root.len() => &path[..path.len() - 1],
        _ => path,
    }
}

/// The name of a photo's scaled copy: its full path, time and size, never sent anywhere (shared with the C# agent's
/// `photos-cache`).
pub(crate) fn cache_key(p: &Photo) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(p.path.to_string_lossy().as_bytes());
    format!("{}-{:x}-{:x}", hex(&hash[..10]), p.ticks, p.length)
}

/// `Path.GetFullPath`, lexically: separators collapsed, `.` and `..` resolved, symlinks not followed; the trailing
/// separator trimmed (`TrimEndingDirectorySeparator`).
fn full_path(root: &str) -> String {
    let s = if cfg!(windows) {
        root.replace('/', "\\")
    } else {
        root.to_string()
    };
    let prefix = path_root(&s);
    let rest = &s[prefix.len().min(s.len())..];
    let mut parts: Vec<&str> = Vec::new();
    for seg in rest.split(SEP) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return prefix;
    }
    let base = if prefix.ends_with(SEP) || prefix.is_empty() {
        prefix
    } else {
        format!("{prefix}{SEP}")
    };
    format!("{base}{}", parts.join(&SEP.to_string()))
}

/// `Path.IsPathFullyQualified`.
fn fully_qualified(path: &str) -> bool {
    if !cfg!(windows) {
        return path.starts_with('/');
    }
    let b = path.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
        || (b.len() >= 2 && matches!(b[0], b'\\' | b'/') && matches!(b[1], b'\\' | b'/'))
}

/// Ordinal on macOS and Linux, OrdinalIgnoreCase on Windows.
fn fold(s: &str) -> String {
    if cfg!(windows) {
        s.to_uppercase()
    } else {
        s.to_string()
    }
}

/// The user's home folder (`SpecialFolder.UserProfile`).
fn home_dir() -> String {
    std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default()
}

/// The folders C# refused as system folders besides `/etc` and friends: Windows, Program Files (x86), and on macOS
/// `/Applications` (.NET's `SpecialFolder.ProgramFiles` there).
fn system_folders() -> Vec<String> {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    if cfg!(windows) {
        vec![env("windir"), env("ProgramFiles"), env("ProgramFiles(x86)")]
    } else if cfg!(target_os = "macos") {
        vec!["/Applications".into()]
    } else {
        vec![]
    }
}

#[cfg(windows)]
fn is_network_drive(full: &str) -> bool {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    let root: Vec<u16> = path_root(full)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // DRIVE_REMOTE
    unsafe { GetDriveTypeW(windows::core::PCWSTR(root.as_ptr())) == 4 }
}

/// Folders too broad to show as a photo frame: a drive or file-system root, the home folder itself, system folders.
pub(crate) fn refuse_root(root: &str) -> Option<&'static str> {
    const FULL: &str = "Choose a full folder path in Settings › General";
    if root.trim().is_empty() || !fully_qualified(root) || root.contains('\0') {
        return Some(FULL);
    }
    let full = full_path(root);
    if full.starts_with("\\\\") || full.starts_with("//") {
        return Some("Network paths aren't supported; use a folder on this computer");
    }
    #[cfg(windows)]
    if is_network_drive(&full) {
        return Some("Network drives aren't supported; use a folder on this computer");
    }
    let home = home_dir();
    let home = trim_ending_separator(&home);
    let folded = fold(&full);
    let same = |other: &str| !other.is_empty() && folded == fold(trim_ending_separator(other));
    let under = |other: &str| {
        !other.is_empty()
            && (same(other)
                || folded.starts_with(&fold(&format!("{}{SEP}", trim_ending_separator(other)))))
    };
    if same(&path_root(&full)) {
        return Some("Choose a photo folder, not a whole drive");
    }
    let home_parent = Path::new(home)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if same(home) || same(&home_parent) {
        return Some("Choose a photo folder, not your whole home folder");
    }
    let config_dir = crate::app_paths::config_dir()
        .to_string_lossy()
        .into_owned();
    let unix_system = [
        "/etc", "/proc", "/sys", "/dev", "/System", "/Library", "/usr",
    ];
    if system_folders().iter().any(|f| under(f))
        || under(&config_dir)
        || (!cfg!(windows) && unix_system.iter().any(|f| under(f)))
    {
        return Some("Choose a photo folder, not a system folder");
    }
    if !Path::new(&full).is_dir() {
        return Some("The photo folder doesn't exist");
    }
    None
}

/// Windows file attributes of an entry (without following links); macOS and Linux: a symlink is a reparse point and
/// a dot-name (or macOS's hidden flag) is hidden, as .NET reports them.
fn attributes(path: &Path, meta: &std::fs::Metadata) -> u32 {
    #[cfg(windows)]
    {
        let _ = path;
        std::os::windows::fs::MetadataExt::file_attributes(meta)
    }
    #[cfg(not(windows))]
    {
        let mut a = 0;
        if meta.file_type().is_symlink() {
            a |= ATTR_REPARSE_POINT;
        }
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            a |= ATTR_HIDDEN;
        }
        #[cfg(target_os = "macos")]
        if std::os::macos::fs::MetadataExt::st_flags(meta) & 0x8000 != 0 {
            a |= ATTR_HIDDEN; // UF_HIDDEN
        }
        a
    }
}

fn link_target(path: &Path) -> Option<String> {
    std::fs::read_link(path)
        .ok()
        .map(|t| t.to_string_lossy().into_owned())
}

/// The last write time in .NET ticks.
fn ticks_of(meta: &std::fs::Metadata) -> i64 {
    const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;
    let Ok(t) = meta.modified() else {
        return UNIX_EPOCH_TICKS;
    };
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => {
            UNIX_EPOCH_TICKS + d.as_secs() as i64 * 10_000_000 + i64::from(d.subsec_nanos() / 100)
        }
        Err(e) => {
            let d = e.duration();
            UNIX_EPOCH_TICKS - d.as_secs() as i64 * 10_000_000 - i64::from(d.subsec_nanos() / 100)
        }
    }
}

/// Lists the images in the folder: bounded, breadth-first, no hidden or system files, no links (symlinks or junctions,
/// files or folders), nothing that is only in the cloud; sorted by path.
pub(crate) fn scan(root: &str, subfolders: bool, id_key: &[u8; 32]) -> Library {
    let key = folder_key(root, subfolders);
    let failed = |error: &str| Library {
        key: key.clone(),
        photos: vec![],
        scanned_at: 0,
        error: Some(error.into()),
    };
    if let Some(refused) = refuse_root(root) {
        return failed(refused);
    }
    let full = full_path(root);
    let mut photos = Vec::new();
    let mut seen = 0usize;
    // breadth-first, in directory order: files up to 6 folders below the root
    let mut pending =
        std::collections::VecDeque::from([(PathBuf::from(&full), MAX_RECURSION_DEPTH)]);
    let mut first = true;
    'walk: while let Some((dir, remaining)) = pending.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) if first => return failed(SCAN_ERROR),
            Err(_) => continue, // IgnoreInaccessible
        };
        first = false;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let attrs = attributes(&path, &meta);
            if attrs & (ATTR_HIDDEN | ATTR_SYSTEM) != 0 {
                continue;
            }
            // .NET reads a symlink to a folder as a folder (with the reparse attribute)
            let is_dir = if meta.file_type().is_symlink() {
                std::fs::metadata(&path).is_ok_and(|m| m.is_dir())
            } else {
                meta.is_dir()
            };
            if is_dir {
                // a junction or symlinked folder is never entered; a cloud-synced folder (reparse point, no target) is
                if subfolders && remaining > 0 && !is_link(attrs, || link_target(&path)) {
                    pending.push_back((path, remaining - 1));
                }
                continue;
            }
            seen += 1;
            if seen > MAX_ENTRIES {
                break 'walk;
            }
            let image = path
                .extension()
                .is_some_and(|e| EXTENSIONS.contains(&e.to_string_lossy().to_lowercase().as_str()));
            if !image || cloud_only(attrs) || is_link(attrs, || link_target(&path)) {
                continue;
            }
            let length = meta.len();
            if length == 0 || length > MAX_SOURCE_BYTES {
                continue;
            }
            let relative = path
                .strip_prefix(&full)
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_default();
            photos.push(Photo {
                id: id_of(id_key, &full, &relative),
                path,
                length,
                ticks: ticks_of(&meta),
            });
            if photos.len() >= MAX_FILES {
                break 'walk;
            }
        }
    }
    photos.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
    let error = photos
        .is_empty()
        .then(|| "No photos in the folder".to_string());
    Library {
        key,
        photos,
        scanned_at: 0,
        error,
    }
}

/// Windows: decode with the `image` crate, scale the long edge to 1600 px over black, turn it upright from its EXIF
/// orientation and encode a JPEG at quality 85 (C#'s GDI+ path).
#[cfg(windows)]
fn scale_with_image(path: &Path) -> Result<Vec<u8>, String> {
    use image::ImageDecoder;
    let reader = image::ImageReader::open(path).map_err(|e| e.to_string())?;
    let reader = reader.with_guessed_format().map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    let scale = (f64::from(LONG_EDGE) / f64::from(w.max(h))).min(1.0);
    let size = |x: u32| ((f64::from(x) * scale).round_ties_even() as u32).max(1);
    if scale < 1.0 {
        img = img.resize_exact(size(w), size(h), image::imageops::FilterType::CatmullRom);
    }
    // transparent pixels over black, as GDI+ drew them
    let mut rgb = image::RgbImage::new(img.width(), img.height());
    for (x, y, p) in img.to_rgba8().enumerate_pixels() {
        let a = u16::from(p[3]);
        rgb.put_pixel(
            x,
            y,
            image::Rgb([0, 1, 2].map(|c| ((u16::from(p[c]) * a) / 255) as u8)),
        );
    }
    let mut upright = image::DynamicImage::ImageRgb8(rgb);
    upright.apply_orientation(orientation);
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85)
        .encode_image(&upright)
        .map_err(|e| e.to_string())?;
    Ok(jpeg)
}

/// A JPEG whose long edge is at most 1600 px, turned upright; Ok(None) where no converter is available (Linux) or
/// sips gave up. The conversion ends with the request (the future is dropped and sips killed) or after 20 s.
async fn scale(path: &Path) -> Result<Option<Vec<u8>>, String> {
    #[cfg(windows)]
    {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || scale_with_image(&path))
            .await
            .map_err(|e| e.to_string())?
            .map(Some)
    }
    #[cfg(target_os = "macos")]
    {
        // sips keeps the EXIF orientation, which the WebView honours
        let mut name = [0u8; 16];
        getrandom::fill(&mut name).map_err(|e| e.to_string())?;
        let tmp = std::env::temp_dir().join(format!("phonebox-photo-{}.jpg", hex(&name)));
        let long_edge = LONG_EDGE.to_string();
        let mut child = tokio::process::Command::new("/usr/bin/sips")
            .args([
                "-Z",
                long_edge.as_str(),
                "-s",
                "format",
                "jpeg",
                "-s",
                "formatOptions",
                "85",
            ])
            .arg(path)
            .arg("--out")
            .arg(&tmp)
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| e.to_string())?;
        let finished = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait()).await;
        let bytes = match finished {
            Ok(Ok(status)) if status.success() => std::fs::read(&tmp).ok(),
            Ok(_) => None,
            Err(_) => {
                let _ = child.kill().await;
                None
            }
        };
        let _ = std::fs::remove_file(&tmp);
        Ok(bytes)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = path;
        Ok(None)
    }
}

impl PhotoFrame {
    /// The frame for these settings; scaled copies go to `local_dir/photos-cache`.
    pub fn new(cfg: Arc<ConfigStore>, local_dir: &Path) -> PhotoFrame {
        let mut id_key = [0u8; 32];
        getrandom::fill(&mut id_key).expect("the system random source");
        let started = std::time::Instant::now();
        PhotoFrame {
            cfg,
            id_key,
            cache_dir: local_dir.join("photos-cache"),
            now: Arc::new(move || started.elapsed().as_millis() as i64),
            scanner: Arc::new(scan),
            state: Mutex::new(State::default()),
            convert: tokio::sync::Mutex::new(()),
            next_scan: AtomicU64::new(1),
        }
    }

    /// Tests: where scaled copies are kept.
    #[cfg(test)]
    pub(crate) fn with_cache_dir(mut self, dir: PathBuf) -> PhotoFrame {
        self.cache_dir = dir;
        self
    }

    /// Tests: what lists a folder.
    #[cfg(test)]
    pub(crate) fn with_scanner(mut self, scanner: Scanner) -> PhotoFrame {
        self.scanner = scanner;
        self
    }

    /// Tests: the id this frame gives a photo of `root`.
    #[cfg(test)]
    pub(crate) fn id_of(&self, root: &str, relative: &str) -> String {
        id_of(&self.id_key, root, relative)
    }

    fn source(&self) -> Option<Source> {
        let cfg = self.cfg.get();
        let folder = cfg.photos.folder.trim();
        (!folder.is_empty()).then(|| Source {
            root: folder.to_string(),
            subfolders: cfg.photos.subfolders,
            key: folder_key(folder, cfg.photos.subfolders),
        })
    }

    /// Some enabled page (or enabled Smart Stack child) shows a Photo frame widget.
    pub fn shown(&self) -> bool {
        self.cfg
            .get()
            .widgets(true)
            .iter()
            .any(|w| w.r#type == WIDGET_TYPE)
    }

    /// The photo ids, or an error the phone can show (never a local path).
    pub fn list(self: &Arc<Self>) -> PhotoList {
        let Some(src) = self.source() else {
            self.forget_cache();
            return PhotoList {
                ids: vec![],
                error: None,
                scanning: false,
            };
        };
        if !self.shown() {
            return PhotoList {
                ids: vec![],
                error: Some(ADD_WIDGET.into()),
                scanning: false,
            };
        }
        match self.current(&src) {
            None => PhotoList {
                ids: vec![],
                error: None,
                scanning: true,
            },
            Some(lib) => PhotoList {
                ids: lib.photos.iter().map(|p| p.id.clone()).collect(),
                error: lib.error.clone(),
                scanning: false,
            },
        }
    }

    /// The library for this source; scans in the background when it is missing or older than five minutes (the old
    /// list is served meanwhile).
    fn current(self: &Arc<Self>, src: &Source) -> Option<Arc<Library>> {
        let mut st = lock(&self.state);
        let lib = st.library.clone();
        let fresh = lib.as_ref().is_some_and(|l| l.key == src.key);
        if fresh
            && lib
                .as_ref()
                .is_some_and(|l| (self.now)() - l.scanned_at < SCAN_EVERY_MS)
        {
            return lib;
        }
        let key = (src.key.clone(), st.generation);
        if st.scan.is_none() || st.scan_key.as_ref() != Some(&key) {
            self.launch(&mut st, src, key);
        }
        if fresh { lib } else { None }
    }

    /// Starts a scan that owns the slot; it publishes only while it still owns it for the same selection and
    /// generation (a scan from a cleared or switched folder is dropped).
    fn launch(self: &Arc<Self>, st: &mut State, src: &Source, key: (String, u64)) {
        let id = self.next_scan.fetch_add(1, Ordering::SeqCst);
        let (done, finished) = watch::channel(false);
        st.scan = Some(ScanHandle { id, done: finished });
        st.scan_key = Some(key.clone());
        let (me, src) = (self.clone(), src.clone());
        tokio::task::spawn_blocking(move || {
            let scanned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (me.scanner)(&src.root, src.subfolders, &me.id_key)
            }));
            let next = match scanned {
                Ok(lib) => lib,
                Err(_) => {
                    crate::log::debug(CATEGORY, "photo scan: the scan failed");
                    Library {
                        key: src.key.clone(),
                        photos: vec![],
                        scanned_at: 0,
                        error: Some(SCAN_ERROR.into()),
                    }
                }
            };
            {
                let mut st = lock(&me.state);
                let owns = st.scan.as_ref().is_some_and(|s| s.id == id);
                if owns && st.generation == key.1 && st.scan_key.as_ref() == Some(&key) {
                    let lib = Library {
                        key: src.key.clone(),
                        scanned_at: (me.now)(),
                        ..next
                    };
                    me.clean_cache_locked(&lib);
                    st.library = Some(Arc::new(lib));
                }
                if owns {
                    st.scan = None;
                    st.scan_key = None;
                }
            }
            let _ = done.send(true);
        });
    }

    /// After a scan, under the state lock (the lock `write_cache` publishes with): scaled copies of photos that are
    /// gone, changed or in another folder are deleted.
    fn clean_cache_locked(&self, lib: &Library) {
        let keep: HashSet<String> = lib
            .photos
            .iter()
            .map(|p| format!("{}.jpg", cache_key(p)))
            .collect();
        let Ok(entries) = std::fs::read_dir(&self.cache_dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file())
                && !keep.contains(entry.file_name().to_string_lossy().as_ref())
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// No folder any more: no scaled copies of its photos either.
    fn forget_cache(&self) {
        let mut st = lock(&self.state);
        st.library = None;
        st.scan = None;
        st.scan_key = None;
        st.generation += 1;
        if self.cache_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.cache_dir);
        }
    }

    /// Tests: scans now and waits for it (at most 30 s).
    #[cfg(test)]
    pub(crate) async fn scan_now(self: &Arc<Self>) {
        lock(&self.state).library = None;
        let Some(src) = self.source() else {
            // nothing is set up: nothing to scan, the same as list() (which clears the cache and shows nothing)
            self.forget_cache();
            return;
        };
        let scan = {
            let mut st = lock(&self.state);
            let key = (src.key.clone(), st.generation);
            if st.scan.is_none() || st.scan_key.as_ref() != Some(&key) {
                self.launch(&mut st, &src, key);
            }
            st.scan.clone()
        };
        if let Some(scan) = scan {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), scan.finished()).await;
        }
    }

    /// After the settings are saved: a cleared source takes its scaled copies with it, widget or not; another folder
    /// or option makes a scan in flight unable to publish.
    pub fn settings_saved(&self) {
        let Some(src) = self.source() else {
            self.forget_cache();
            return;
        };
        // every option of the source counts as much as the source: a recursive scan in flight must not publish after
        // it was turned off
        let mut st = lock(&self.state);
        if st.library.as_ref().is_some_and(|l| l.key != src.key) {
            st.library = None;
            st.generation += 1;
        } else if st.scan_key.as_ref().is_some_and(|k| k.0 != src.key) {
            st.generation += 1;
        }
    }

    /// One photo, scaled down when it is large; Err with the reason the phone is told.
    pub async fn photo(
        self: &Arc<Self>,
        id: Option<&str>,
    ) -> Result<(Vec<u8>, &'static str), &'static str> {
        if !self.shown() {
            return Err(ADD_WIDGET);
        }
        let (Some(id), Some(src)) = (id.filter(|i| !i.is_empty()), self.source()) else {
            return Err(UNKNOWN);
        };
        let lib = self.current(&src);
        let Some(photo) = lib
            .as_ref()
            .and_then(|l| l.photos.iter().find(|p| p.id == id))
            .cloned()
        else {
            return Err(UNKNOWN);
        };
        // checked again now: the file, or a folder on its way, may have become a link since the scan
        let full = full_path(&src.root);
        let path = photo.path.clone();
        let fresh = tokio::task::spawn_blocking(move || still_there(&path, &full))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())));
        match fresh {
            Ok(Some((length, ticks))) => {
                self.prepare(Photo {
                    length,
                    ticks,
                    ..photo
                })
                .await
            }
            Ok(None) => Err(GONE),
            Err(e) => {
                crate::log::debug(CATEGORY, &format!("photo {}: {e}", photo.id));
                Err(CANT_SHOW)
            }
        }
    }

    async fn prepare(
        self: &Arc<Self>,
        photo: Photo,
    ) -> Result<(Vec<u8>, &'static str), &'static str> {
        let cant = |e: std::io::Error| {
            crate::log::debug(CATEGORY, &format!("photo {}: {e}", photo.id));
            CANT_SHOW
        };
        let generation = lock(&self.state).generation;
        let head = read_head(&photo.path, HEAD_BYTES.min(photo.length) as usize)
            .await
            .map_err(cant)?;
        let mime = mime_of(&head).ok_or(CANT_SHOW)?;
        let size = size_of(&head, mime);
        if size.is_some_and(|(w, h)| w <= 0 || h <= 0) {
            return Err(CANT_SHOW);
        }
        // only a known, moderate size goes to the phone as it is: a small file can still be 20000 x 20000 pixels
        let phone_safe = size
            .is_some_and(|(w, h)| w <= 8192 && h <= 8192 && w * h <= MAX_PHONE_PIXELS)
            && photo.length <= ORIGINAL_MAX_BYTES;
        let key = cache_key(&photo);
        let cached = self.cache_dir.join(format!("{key}.jpg"));
        if let Some(bytes) = read_if_there(&cached).await.map_err(cant)? {
            return Ok((bytes, "image/jpeg"));
        }
        if (matches!(mime, "image/gif" | "image/webp") || photo.length <= 400_000) && phone_safe {
            return Ok((tokio::fs::read(&photo.path).await.map_err(cant)?, mime));
        }
        let failed = lock(&self.state).failed.contains(&key);
        let decodable = matches!(mime, "image/jpeg" | "image/png")
            && size.map_or(photo.length <= 30 * 1024 * 1024, |(w, h)| {
                w * h <= MAX_DECODE_PIXELS
            });
        if decodable && !failed {
            let _one_at_a_time = self.convert.lock().await;
            if let Some(bytes) = read_if_there(&cached).await.map_err(cant)? {
                return Ok((bytes, "image/jpeg"));
            }
            match scale(&photo.path).await {
                Ok(Some(jpeg)) => {
                    self.write_cache(&cached, &jpeg, generation);
                    return Ok((jpeg, "image/jpeg"));
                }
                Ok(None) => {}
                Err(e) => {
                    // a damaged or half-synced file: remember it, don't decode it every round
                    crate::log::debug(
                        CATEGORY,
                        &format!("photo {} can't be converted: {e}", photo.id),
                    );
                    let mut st = lock(&self.state);
                    if st.failed.len() > 1000 {
                        st.failed.clear();
                    }
                    st.failed.insert(key);
                }
            }
        }
        if phone_safe {
            Ok((tokio::fs::read(&photo.path).await.map_err(cant)?, mime))
        } else {
            Err(TOO_LARGE)
        }
    }

    /// Writes a scaled copy to a temporary owner-only file first, so a reader never sees half of one; under the same
    /// lock as `forget_cache`, so a copy is either written before the folder was cleared or not at all.
    fn write_cache(&self, path: &Path, jpeg: &[u8], generation: u64) {
        let st = lock(&self.state);
        if generation != st.generation {
            return;
        }
        let written = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&self.cache_dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&self.cache_dir, std::fs::Permissions::from_mode(0o700))?;
            }
            let mut name = [0u8; 4];
            getrandom::fill(&mut name).map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut tmp = path.as_os_str().to_owned();
            tmp.push(format!(".{}.tmp", hex(&name)));
            let tmp = PathBuf::from(tmp);
            let mut file = crate::app_paths::create_private(&tmp)?;
            std::io::Write::write_all(&mut file, jpeg)?;
            drop(file);
            std::fs::rename(&tmp, path)
        })();
        drop(st);
        if let Err(e) = written {
            crate::log::debug(CATEGORY, &format!("photo cache: {e}"));
        }
    }

    /// Tests: the scan that owns the slot (C# read `_scan` by reflection).
    #[cfg(test)]
    pub(crate) fn scan_in_flight(&self) -> Option<ScanHandle> {
        lock(&self.state).scan.clone()
    }

    /// Tests: the published library (C# read `_library`).
    #[cfg(test)]
    pub(crate) fn published(&self) -> Option<Arc<Library>> {
        lock(&self.state).library.clone()
    }
}

/// A photo checked again before it is served: a file (not a link, not cloud-only, 1 byte to 60 MiB) inside the folder,
/// with no linked folder on its way below the root; its current length and time.
fn still_there(path: &Path, full: &str) -> std::io::Result<Option<(u64, i64)>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let name = path.to_string_lossy();
    let inside = fold(&name).starts_with(&fold(&format!("{full}{SEP}")));
    let attrs = attributes(path, &meta);
    if meta.is_dir() || !inside || is_link(attrs, || link_target(path)) || cloud_only(attrs) {
        return Ok(None);
    }
    if meta.len() == 0 || meta.len() > MAX_SOURCE_BYTES {
        return Ok(None);
    }
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.as_os_str().len() <= full.len() {
            break;
        }
        let m = std::fs::symlink_metadata(d)?;
        if is_link(attributes(d, &m), || link_target(d)) {
            return Ok(None);
        }
        dir = d.parent();
    }
    Ok(Some((meta.len(), ticks_of(&meta))))
}

/// The first `len` bytes (fewer when the file is shorter now).
async fn read_head(path: &Path, len: usize) -> std::io::Result<Vec<u8>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; len];
        let mut got = 0;
        while got < len {
            let n = std::io::Read::read(&mut file, &mut buf[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        buf.truncate(got);
        Ok(buf)
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?
}

async fn read_if_there(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
#[path = "photo_frame_tests.rs"]
mod tests;
