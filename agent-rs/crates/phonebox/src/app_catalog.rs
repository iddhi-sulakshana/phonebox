//! `AppCatalog`: the applications installed on the computer, for the All apps widget: the list, their icons and
//! opening one. The phone only ever sends an app's id; the agent looks it up in its own scan, so a request cannot name
//! a path or a command. Opening apps is allowed only while an All apps widget is on one of the pages. macOS for now.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::config::store::ConfigStore;

pub const WIDGET_TYPE: &str = "apps";

/// Apps are listed, shown and opened on macOS only.
pub const SUPPORTED: bool = cfg!(target_os = "macos");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub path: String,
}

/// The icon conversion (sips / Quick Look; a fake in tests).
pub type Converter = Arc<dyn Fn(&AppEntry) -> Option<Vec<u8>> + Send + Sync>;

struct Scan {
    apps: Arc<Vec<Arc<AppEntry>>>,
    by_id: HashMap<String, Arc<AppEntry>>,
    /// Monotonic ms of the last scan; far in the past before the first.
    scanned_at: i64,
}

struct IconEntry {
    png: Option<Vec<u8>>,
    at: i64,
}

/// A counting semaphore: icon conversion spawns sips / qlmanage, at most two at a time.
struct Slots {
    free: Mutex<usize>,
    freed: Condvar,
}

pub struct AppCatalog {
    cfg: Arc<ConfigStore>,
    scan: Mutex<Scan>,
    scanning: AtomicBool,
    warming: AtomicBool,
    icons: Mutex<HashMap<String, IconEntry>>,
    /// One conversion per app at a time.
    icon_gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    slots: Slots,
    /// How long a failed conversion is remembered before it is tried again (a timeout on a busy computer). Shorter than
    /// the panel's one retry after 15 s (widgets/pc.js), so that retry gets a fresh attempt instead of the cached failure.
    pub(crate) failure_retry_ms: i64,
    /// For tests: the conversion instead of sips / Quick Look.
    pub(crate) converter: Converter,
    /// Folders scanned for .app bundles (top level and one folder deep, e.g. /Applications/Utilities).
    pub(crate) roots: Vec<String>,
    /// Convert all icons in the background after a scan (off in tests, which scan fake bundles).
    pub(crate) warm_icons: bool,
    /// [`SUPPORTED`]; tests turn it on to run the icon pipeline everywhere.
    pub(crate) supported: bool,
}

const RESCAN_MS: i64 = 5 * 60_000;

/// Monotonic milliseconds (`Environment.TickCount64` stands in for it in the C# agent).
fn now_ms() -> i64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as i64
}

impl Slots {
    fn acquire(&self) -> SlotGuard<'_> {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 {
            free = self.freed.wait(free).unwrap_or_else(|e| e.into_inner());
        }
        *free -= 1;
        SlotGuard(self)
    }
}

struct SlotGuard<'a>(&'a Slots);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.0.freed.notify_one();
    }
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AppCatalog {
    pub fn new(cfg: Arc<ConfigStore>) -> Arc<AppCatalog> {
        Arc::new(AppCatalog::plain(cfg))
    }

    /// The catalog before it is shared; tests set its test fields first.
    pub(crate) fn plain(cfg: Arc<ConfigStore>) -> AppCatalog {
        let roots = if cfg!(target_os = "macos") {
            let home = crate::app_paths::home().join("Applications");
            vec![
                "/Applications".to_string(),
                "/System/Applications".to_string(),
                home.to_string_lossy().into_owned(),
            ]
        } else {
            vec![]
        };
        AppCatalog {
            cfg,
            scan: Mutex::new(Scan {
                apps: Arc::new(vec![]),
                by_id: HashMap::new(),
                scanned_at: i64::MIN / 2,
            }),
            scanning: AtomicBool::new(false),
            warming: AtomicBool::new(false),
            icons: Mutex::new(HashMap::new()),
            icon_gates: Mutex::new(HashMap::new()),
            slots: Slots {
                free: Mutex::new(2),
                freed: Condvar::new(),
            },
            failure_retry_ms: 10_000,
            converter: Arc::new(make_icon),
            roots,
            warm_icons: true,
            supported: SUPPORTED,
        }
    }

    /// Only while an All apps widget is on a page that is shown (like allow-kill): hiding the page turns opening off.
    pub fn open_allowed(&self) -> bool {
        self.cfg
            .get()
            .widgets(true)
            .iter()
            .any(|w| w.r#type == WIDGET_TYPE)
    }

    /// Whether the list, icons and opening work on this system.
    pub fn supported(&self) -> bool {
        self.supported
    }

    /// The installed apps by name. The first call scans; later calls return the last scan and rescan in the background.
    pub fn list(self: &Arc<Self>) -> Arc<Vec<Arc<AppEntry>>> {
        let (first, scanned_at) = {
            let scan = locked(&self.scan);
            (
                scan.by_id.is_empty() && scan.scanned_at < 0,
                scan.scanned_at,
            )
        };
        if first {
            self.rescan();
        } else if now_ms() - scanned_at > RESCAN_MS && !self.scanning.swap(true, Ordering::SeqCst) {
            let me = self.clone();
            std::thread::spawn(move || {
                me.rescan();
                me.scanning.store(false, Ordering::SeqCst);
            });
        }
        locked(&self.scan).apps.clone()
    }

    pub fn find(self: &Arc<Self>, id: &str) -> Option<Arc<AppEntry>> {
        if id.is_empty() {
            return None;
        }
        self.list();
        locked(&self.scan).by_id.get(id).cloned()
    }

    pub(crate) fn rescan(self: &Arc<Self>) {
        let mut found: indexmap::IndexMap<String, Arc<AppEntry>> = indexmap::IndexMap::new();
        for root in &self.roots {
            for app in bundles(root) {
                let entry = AppEntry {
                    id: id_of(&app),
                    name: name_without_extension(&app).to_string(),
                    path: app.clone(),
                };
                found.entry(entry.id.clone()).or_insert(Arc::new(entry));
            }
        }
        let mut list: Vec<Arc<AppEntry>> = found.values().cloned().collect();
        list.sort_by(|a, b| ordinal_ignore_case(&a.name, &b.name)); // stable, as OrderBy
        let list = Arc::new(list);
        {
            let mut scan = locked(&self.scan);
            scan.apps = list.clone();
            scan.by_id = found.into_iter().collect();
            scan.scanned_at = now_ms();
        }
        if self.supported && self.warm_icons && !self.warming.swap(true, Ordering::SeqCst) {
            let me = self.clone();
            std::thread::spawn(move || {
                me.warm(&list);
                me.warming.store(false, Ordering::SeqCst);
            });
        }
    }

    /// Converts the icons one after another in the background, so the phone gets them from the cache: a first
    /// conversion (sips / Quick Look) can take longer than the phone waits for an image on a busy computer.
    fn warm(self: &Arc<Self>, apps: &[Arc<AppEntry>]) {
        for a in apps {
            if self.cached(&a.id).is_none() {
                self.icon_blocking(&a.id);
            }
        }
    }

    /// Opens (or brings forward) an app from the last scan; an error message, or None when it worked.
    pub fn open(self: &Arc<Self>, id: &str) -> Option<String> {
        if !self.supported {
            return Some("Opening apps is supported on macOS only".into());
        }
        if !self.open_allowed() {
            return Some("Add an All apps widget to a page to open apps from the phone".into());
        }
        let Some(app) = self.find(id) else {
            return Some("Unknown app".into());
        };
        if !std::path::Path::new(&app.path).is_dir() {
            return Some(format!("{} is no longer installed", app.name));
        }
        // open(1) with the bundle path as a single argument: no shell, nothing from the request but the id
        let ok = crate::unix::sh::run_with_timeout(10_000, "/usr/bin/open", &[&app.path]).is_some();
        crate::log::info(
            "AppCatalog",
            &format!(
                "app {} opened from the phone: {}",
                app.name,
                if ok { "ok" } else { "failed" }
            ),
        );
        (!ok).then(|| format!("Could not open {}", app.name))
    }

    /// A 96 px PNG of the app's icon. Successes are kept; a failure is kept for `failure_retry_ms`, then the next request
    /// converts again.
    pub async fn icon(self: &Arc<Self>, id: &str) -> Option<Vec<u8>> {
        let (me, id) = (self.clone(), id.to_string());
        tokio::task::spawn_blocking(move || me.icon_blocking(&id))
            .await
            .ok()
            .flatten()
    }

    fn icon_blocking(self: &Arc<Self>, id: &str) -> Option<Vec<u8>> {
        if !self.supported {
            return None;
        }
        let app = self.find(id)?;
        if let Some(cached) = self.cached(id) {
            return cached;
        }
        // background warming and a phone request can ask for the same app at once: the second waits for the first
        let gate = locked(&self.icon_gates)
            .entry(id.to_string())
            .or_default()
            .clone();
        let _gate = locked(&gate);
        if let Some(cached) = self.cached(id) {
            return cached;
        }
        let _slot = self.slots.acquire();
        let png = (self.converter)(&app);
        locked(&self.icons).insert(
            id.to_string(),
            IconEntry {
                png: png.clone(),
                at: now_ms(),
            },
        );
        png
    }

    /// Some(the cached result) while it holds; a failure expires after `failure_retry_ms`.
    fn cached(&self, id: &str) -> Option<Option<Vec<u8>>> {
        let icons = locked(&self.icons);
        let e = icons.get(id)?;
        if e.png.is_none() && now_ms() - e.at >= self.failure_retry_ms {
            return None; // failed a while ago: try again
        }
        Some(e.png.clone())
    }
}

/// The .app bundles in a folder and one folder deep; hidden apps and folders are helpers, not apps to open.
fn bundles(root: &str) -> Vec<String> {
    let dirs = |dir: &std::path::Path| -> Option<Vec<std::path::PathBuf>> {
        Some(
            std::fs::read_dir(dir)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect(),
        )
    };
    let hidden = |p: &std::path::Path| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
    };
    let is_app = |p: &std::path::Path| {
        let s = p.to_string_lossy();
        s.len() >= 4
            && s.is_char_boundary(s.len() - 4)
            && s[s.len() - 4..].eq_ignore_ascii_case(".app")
    };
    let root = std::path::Path::new(root);
    if !root.is_dir() {
        return vec![];
    }
    let Some(top) = dirs(root) else {
        return vec![];
    };
    let mut out = vec![];
    for d in top {
        if hidden(&d) {
            continue;
        }
        if is_app(&d) {
            out.push(d.to_string_lossy().into_owned());
            continue;
        }
        let Some(inner) = dirs(&d) else { continue };
        out.extend(
            inner
                .into_iter()
                .filter(|a| is_app(a) && !hidden(a))
                .map(|a| a.to_string_lossy().into_owned()),
        );
    }
    out
}

/// `Path.GetFileNameWithoutExtension`.
fn name_without_extension(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.rfind('.').map_or(file, |dot| &file[..dot])
}

/// `StringComparer.OrdinalIgnoreCase`: each UTF-16 unit upper-cased (simple mapping), then compared by value.
fn ordinal_ignore_case(a: &str, b: &str) -> std::cmp::Ordering {
    let fold = |s: &str| -> Vec<u16> {
        s.chars()
            .map(|c| {
                let mut up = c.to_uppercase();
                match (up.next(), up.next()) {
                    (Some(u), None) => u,
                    _ => c,
                }
            })
            .collect::<String>()
            .encode_utf16()
            .collect()
    };
    fold(a).cmp(&fold(b))
}

/// The bundle's icon as a 96 px PNG, cached in `$TMPDIR/phonebox-app-icons/<id>.png`: the icon file Info.plist names
/// (through sips), else Quick Look's thumbnail of the bundle. Converters write to names of their own; only a finished
/// PNG is moved (renamed) to the cached name, so a reader never sees a half-written file, now or after a restart.
fn make_icon(app: &AppEntry) -> Option<Vec<u8>> {
    use std::path::Path;
    let dir = std::env::temp_dir().join("phonebox-app-icons");
    let id = id_of(&app.path);
    let unique = {
        let mut b = [0u8; 16];
        let _ = getrandom::fill(&mut b);
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    };
    let tmp = dir.join(format!("{id}.{unique}.tmp.png"));
    let ql = dir.join(format!("ql-{id}-{unique}"));
    let run = |ms: u64, exe: &str, args: &[&str]| crate::unix::sh::run_with_timeout(ms, exe, args);
    let publish = |finished: &Path, target: &Path| -> std::io::Result<Vec<u8>> {
        std::fs::rename(finished, target)?; // a rename within the folder: atomic
        std::fs::read(target)
    };
    let attempt = || -> std::io::Result<Option<Vec<u8>>> {
        std::fs::create_dir_all(&dir)?;
        let out = dir.join(format!("{id}.png"));
        if std::fs::metadata(&out).is_ok_and(|m| !m.is_dir()) {
            return std::fs::read(&out).map(Some);
        }
        // 1) the icon file the bundle names in Info.plist (.icns)
        let contents = Path::new(&app.path).join("Contents");
        let plist = contents.join("Info.plist");
        let icon_name = if plist.is_file() {
            run(
                3000,
                "/usr/bin/plutil",
                &[
                    "-extract",
                    "CFBundleIconFile",
                    "raw",
                    "-o",
                    "-",
                    &plist.to_string_lossy(),
                ],
            )
            .map(|s| s.trim().to_string())
        } else {
            None
        };
        if let Some(mut name) = icon_name.filter(|n| !n.is_empty()) {
            if !name.to_ascii_lowercase().ends_with(".icns") {
                name.push_str(".icns");
            }
            let file = name.rsplit('/').next().unwrap_or(&name).to_string();
            let icns = contents.join("Resources").join(file);
            let tmp_text = tmp.to_string_lossy().into_owned();
            if icns.is_file()
                && run(
                    8000,
                    "/usr/bin/sips",
                    &[
                        "-s",
                        "format",
                        "png",
                        "--resampleWidth",
                        "96",
                        &icns.to_string_lossy(),
                        "--out",
                        &tmp_text,
                    ],
                )
                .is_some()
                && tmp.is_file()
            {
                return publish(&tmp, &out).map(Some);
            }
        }
        // 2) apps whose icon lives only in an asset catalog: Quick Look renders the bundle's icon
        std::fs::create_dir_all(&ql)?;
        run(
            10_000,
            "/usr/bin/qlmanage",
            &["-t", "-s", "96", "-o", &ql.to_string_lossy(), &app.path],
        );
        let thumb = std::fs::read_dir(&ql)?
            .flatten()
            .map(|e| e.path())
            .find(|p| !p.is_dir() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")));
        match thumb {
            Some(thumb) => publish(&thumb, &out).map(Some),
            None => Ok(None),
        }
    };
    let result = attempt().ok().flatten();
    let _ = std::fs::remove_file(&tmp);
    if ql.is_dir() {
        let _ = std::fs::remove_dir_all(&ql);
    }
    result
}

/// A supported catalog over test folders with a fake conversion and no warm-up (for the route tests).
#[cfg(test)]
pub(crate) fn test_catalog(
    cfg: Arc<ConfigStore>,
    roots: Vec<String>,
    converter: Converter,
) -> Arc<AppCatalog> {
    Arc::new(AppCatalog {
        roots,
        converter,
        warm_icons: false,
        supported: true,
        ..AppCatalog::plain(cfg)
    })
}

/// A short id that stays the same for the same bundle path: the first 16 hex digits of its SHA-256.
pub fn id_of(path: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(path.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn config(&self, widget_type: &str, page_enabled: bool) -> Arc<ConfigStore> {
            let path = self.dir.path().join("config.json");
            let json = serde_json::json!({"pages": [
                {"id": "p", "title": "P", "enabled": page_enabled,
                 "widgets": [{"id": "w", "type": widget_type, "x": 0, "y": 0, "w": 6, "h": 4}]},
                {"id": "q", "title": "Q",
                 "widgets": [{"id": "c", "type": "clock", "x": 0, "y": 0, "w": 6, "h": 4}]}
            ]});
            std::fs::write(&path, json.to_string()).unwrap();
            Arc::new(ConfigStore::new(path))
        }

        fn root(&self, bundles: &[&str]) -> String {
            let root = self.dir.path().join("Applications");
            for b in bundles {
                std::fs::create_dir_all(root.join(b).join("Contents")).unwrap();
            }
            root.to_string_lossy().into_owned()
        }

        fn catalog(&self, widget_type: &str, roots: Vec<String>) -> Arc<AppCatalog> {
            Arc::new(AppCatalog {
                warm_icons: false,
                roots,
                ..AppCatalog::plain(self.config(widget_type, true))
            })
        }
    }

    #[test]
    fn hiding_the_page_with_the_widget_turns_opening_off() {
        let f = Fixture::new();
        assert!(AppCatalog::plain(f.config("apps", true)).open_allowed());
        assert!(!AppCatalog::plain(f.config("apps", false)).open_allowed());
    }

    /// The C# test returns early off macOS; here the pipeline is switched on, so it runs everywhere.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_convert_an_icon_once() {
        let f = Fixture::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let catalog = Arc::new(AppCatalog {
            warm_icons: false,
            supported: true,
            roots: vec![f.root(&["Notes.app"])],
            converter: Arc::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(200));
                Some(vec![1, 2, 3])
            }),
            ..AppCatalog::plain(f.config("apps", true))
        });
        let id = catalog.list()[0].id.clone();
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let (catalog, id) = (catalog.clone(), id.clone());
                tokio::spawn(async move { catalog.icon(&id).await })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap(), Some(vec![1, 2, 3]));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_conversion_is_retried_after_it_expires() {
        let f = Fixture::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let convert = |calls: Arc<AtomicUsize>| -> Converter {
            Arc::new(move |_| (calls.fetch_add(1, Ordering::SeqCst) > 0).then(|| vec![9]))
        };
        let root = f.root(&["Notes.app"]);

        let kept = Arc::new(AppCatalog {
            warm_icons: false,
            supported: true,
            roots: vec![root.clone()],
            converter: convert(calls.clone()),
            ..AppCatalog::plain(f.config("apps", true))
        });
        let id = kept.list()[0].id.clone();
        assert_eq!(kept.icon(&id).await, None);
        assert_eq!(kept.icon(&id).await, None); // within the retry window: not converted again
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        calls.store(0, Ordering::SeqCst);
        let expiring = Arc::new(AppCatalog {
            warm_icons: false,
            supported: true,
            roots: vec![root],
            converter: convert(calls.clone()),
            failure_retry_ms: 0,
            ..AppCatalog::plain(f.config("apps", true))
        });
        assert_eq!(expiring.icon(&id).await, None);
        assert_eq!(expiring.icon(&id).await, Some(vec![9])); // the failure expired: converted again
        assert_eq!(expiring.icon(&id).await, Some(vec![9])); // a success is kept
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn icons_are_none_where_apps_are_not_supported_and_for_unknown_ids() {
        let f = Fixture::new();
        let off = Arc::new(AppCatalog {
            warm_icons: false,
            supported: false,
            roots: vec![f.root(&["Notes.app"])],
            converter: Arc::new(|_| Some(vec![1])),
            ..AppCatalog::plain(f.config("apps", true))
        });
        let id = off.list()[0].id.clone();
        assert_eq!(off.icon(&id).await, None);
        let on = Arc::new(AppCatalog {
            supported: true,
            converter: Arc::new(|_| Some(vec![1])),
            warm_icons: false,
            roots: off.roots.clone(),
            ..AppCatalog::plain(f.config("apps", true))
        });
        assert_eq!(on.icon("0123456789abcdef").await, None);
        assert_eq!(on.icon(&id).await, Some(vec![1]));
    }

    #[test]
    fn scans_top_level_and_one_folder_deep_sorted_by_name() {
        let f = Fixture::new();
        let root = f.root(&[
            "Zed.app",
            "alpha.app",
            "Utilities/Terminal.app",
            "Utilities/Inner/TooDeep.app",
            "Zed.app/Contents/Helper.app",
            ".hidden/Ghost.app",
            ".Helper.app",
            "Utilities/.Agent.app",
            "NotAnApp",
        ]);
        let apps = f.catalog("apps", vec![root]).list();
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Terminal", "Zed"]);
    }

    #[test]
    fn ids_are_stable_and_only_known_ids_resolve() {
        let f = Fixture::new();
        let root = f.root(&["Notes.app"]);
        let catalog = f.catalog("apps", vec![root.clone()]);
        let apps = catalog.list();
        assert_eq!(apps.len(), 1);
        let notes = apps[0].clone();
        let path = std::path::Path::new(&root)
            .join("Notes.app")
            .to_string_lossy()
            .into_owned();
        assert_eq!(notes.id, id_of(&path));
        assert_eq!(notes.id.len(), 16);
        assert!(Arc::ptr_eq(&notes, &catalog.find(&notes.id).unwrap()));
        assert!(catalog.find("not-an-id").is_none());
        assert!(catalog.find(&path).is_none()); // a path is never accepted in place of an id
        assert!(catalog.find("").is_none());
    }

    #[test]
    fn the_id_is_the_first_16_hex_digits_of_the_sha256_of_the_path() {
        // SHA-256("/Applications/Safari.app"), as Convert.ToHexStringLower(SHA256.HashData(...))[..16]
        use sha2::Digest;
        let full = sha2::Sha256::digest(b"/Applications/Safari.app");
        let hex: String = full.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(id_of("/Applications/Safari.app"), hex[..16]);
    }

    #[test]
    fn opening_needs_an_apps_widget_on_a_page() {
        let f = Fixture::new();
        assert!(AppCatalog::plain(f.config("apps", true)).open_allowed());
        let off = f.catalog("clock", vec![f.root(&["Notes.app"])]);
        assert!(!off.open_allowed());
        let id = off.list()[0].id.clone();
        assert!(off.open(&id).is_some()); // refused (and on other systems: not supported), never launched
    }

    #[test]
    fn unknown_ids_are_refused() {
        let f = Fixture::new();
        let catalog = f.catalog("apps", vec![f.root(&["Notes.app"])]);
        assert!(catalog.open("0123456789abcdef").is_some());
        assert!(
            catalog
                .open("/System/Applications/Calculator.app")
                .is_some()
        );
    }

    #[test]
    fn the_refusals_say_why() {
        let f = Fixture::new();
        let root = f.root(&["Notes.app"]);
        let on = |widget: &str| {
            Arc::new(AppCatalog {
                warm_icons: false,
                supported: true,
                roots: vec![root.clone()],
                ..AppCatalog::plain(f.config(widget, true))
            })
        };
        let unsupported = Arc::new(AppCatalog {
            supported: false,
            warm_icons: false,
            ..AppCatalog::plain(f.config("apps", true))
        });
        assert_eq!(
            unsupported.open("x").as_deref(),
            Some("Opening apps is supported on macOS only")
        );
        assert_eq!(
            on("clock").open("x").as_deref(),
            Some("Add an All apps widget to a page to open apps from the phone")
        );
        let catalog = on("apps");
        assert_eq!(
            catalog.open("0123456789abcdef").as_deref(),
            Some("Unknown app")
        );
        let id = catalog.list()[0].id.clone();
        std::fs::remove_dir_all(std::path::Path::new(&root).join("Notes.app")).unwrap();
        assert_eq!(
            catalog.open(&id).as_deref(),
            Some("Notes is no longer installed")
        );
    }

    #[test]
    fn missing_roots_give_an_empty_list() {
        let f = Fixture::new();
        let nope = f.dir.path().join("nope").to_string_lossy().into_owned();
        assert!(f.catalog("apps", vec![nope]).list().is_empty());
    }

    #[test]
    fn the_first_bundle_with_an_id_wins_and_names_sort_ignoring_case() {
        let f = Fixture::new();
        let root = f.root(&["b.app", "D.app", "a.app", "C.app"]);
        // the same root twice: every id is found twice, kept once
        let apps = f.catalog("apps", vec![root.clone(), root]).list();
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "C", "D"]);
    }

    #[test]
    fn a_later_list_returns_the_last_scan_without_scanning_again() {
        let f = Fixture::new();
        let root = f.root(&["Notes.app"]);
        let catalog = f.catalog("apps", vec![root.clone()]);
        assert_eq!(catalog.list().len(), 1);
        std::fs::create_dir_all(std::path::Path::new(&root).join("Mail.app")).unwrap();
        assert_eq!(catalog.list().len(), 1); // within five minutes: the last scan
        catalog.rescan();
        assert_eq!(catalog.list().len(), 2);
    }
}
