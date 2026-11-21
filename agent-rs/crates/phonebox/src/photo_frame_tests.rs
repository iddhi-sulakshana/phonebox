//! `PhotoFrameTests.cs` (18): the folder rules (bounded scan, no links or cloud-only files, id-only serving, gating),
//! plus the cache names the C# agent computes.

use std::sync::Condvar;
use std::sync::atomic::AtomicBool;

use std::time::Duration;

use super::*;

/// A 16 x 16 baseline JPEG header (SOI + SOF0).
const JPEG: [u8; 23] = [
    0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x10, 0x00, 0x10, 0x03, 0x01, 0x22, 0x00, 0x02,
    0x11, 0x01, 0x03, 0x11, 0x01, 0xFF, 0xD9,
];

fn png_of(w: u32, h: u32) -> Vec<u8> {
    let mut b = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R',
    ];
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&[8, 6, 0, 0, 0]);
    b
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("photos")).unwrap();
        Fixture { dir }
    }

    fn photos(&self) -> String {
        self.dir
            .path()
            .join("photos")
            .to_string_lossy()
            .into_owned()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn config_json(
        &self,
        folder: &str,
        widget: bool,
        page_enabled: bool,
        subfolders: bool,
    ) -> String {
        let widgets = if widget {
            r#"[{"id":"w","type":"photos","x":0,"y":0,"w":6,"h":4}]"#
        } else {
            "[]"
        };
        format!(
            r#"{{"photos":{{"folder":{},"subfolders":{subfolders}}},"pages":[{{"id":"p","title":"P","enabled":{page_enabled},"widgets":{widgets}}},{{"id":"q","title":"Q","widgets":[{{"id":"c","type":"clock","x":0,"y":0,"w":6,"h":4}}]}}]}}"#,
            serde_json::to_string(folder).unwrap()
        )
    }

    fn store(&self, widget: bool, page_enabled: bool, subfolders: bool) -> Arc<ConfigStore> {
        let path = self.path("config.json");
        std::fs::write(
            &path,
            self.config_json(&self.photos(), widget, page_enabled, subfolders),
        )
        .unwrap();
        Arc::new(ConfigStore::new(path))
    }

    fn frame(&self, widget: bool, page_enabled: bool, subfolders: bool) -> Arc<PhotoFrame> {
        let store = self.store(widget, page_enabled, subfolders);
        Arc::new(PhotoFrame::new(store, self.dir.path()).with_cache_dir(self.path("cache")))
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.path(&format!("photos/{rel}"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}

fn names(lib: &Library) -> Vec<String> {
    let mut n: Vec<String> = lib
        .photos
        .iter()
        .map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    n.sort();
    n
}

#[test]
fn broad_or_missing_folders_are_refused() {
    let f = Fixture::new();
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap();
    let root = if cfg!(windows) {
        format!("{}\\", &home[..2])
    } else {
        "/".to_string()
    };
    assert!(refuse_root(&root).is_some());
    assert!(refuse_root(&home).is_some());
    assert!(refuse_root(Path::new(&home).parent().unwrap().to_str().unwrap()).is_some());
    assert!(refuse_root("relative/folder").is_some());
    assert!(refuse_root(f.path("missing").to_str().unwrap()).is_some());
    if cfg!(windows) {
        assert!(refuse_root(r"\\server\share\photos").is_some());
        assert!(refuse_root(&std::env::var("windir").unwrap()).is_some());
    } else {
        assert!(refuse_root("/etc").is_some());
    }
    assert_eq!(refuse_root(&f.photos()), None);
}

#[test]
fn the_scan_keeps_images_only_and_follows_the_subfolder_setting() {
    let f = Fixture::new();
    f.write("a.jpg", &JPEG);
    f.write("b.PNG", &png_of(16, 16));
    f.write("notes.txt", b"not a photo");
    f.write("empty.jpg", b"");
    f.write("trip/c.jpeg", &JPEG);
    let key = [7u8; 32];
    let all = scan(&f.photos(), true, &key);
    assert_eq!(names(&all), ["a.jpg", "b.PNG", "c.jpeg"]);
    assert_eq!(all.error, None);
    assert_eq!(scan(&f.photos(), false, &key).photos.len(), 2);
    let c = all
        .photos
        .iter()
        .find(|p| p.path.ends_with("c.jpeg"))
        .unwrap();
    assert_eq!(c.id, id_of(&key, &f.photos(), "trip/c.jpeg"));
    // per folder
    assert_ne!(
        id_of(&key, &f.photos(), "a.jpg"),
        id_of(&key, f.path("other").to_str().unwrap(), "a.jpg")
    );
}

#[cfg(unix)]
#[test]
fn links_out_of_the_folder_are_skipped() {
    let f = Fixture::new();
    let outside = f.path("secret.jpg");
    std::fs::write(&outside, JPEG).unwrap();
    std::os::unix::fs::symlink(&outside, f.path("photos/link.jpg")).unwrap();
    assert!(scan(&f.photos(), true, &[0; 32]).photos.is_empty());
}

#[cfg(windows)]
#[test]
fn links_out_of_the_folder_are_skipped() {
    let f = Fixture::new();
    let outside = f.path("secret.jpg");
    std::fs::write(&outside, JPEG).unwrap();
    if std::os::windows::fs::symlink_file(&outside, f.path("photos/link.jpg")).is_err() {
        return; // no symlink rights on this machine
    }
    assert!(scan(&f.photos(), true, &[0; 32]).photos.is_empty());
}

#[test]
fn linked_folders_are_never_entered() {
    let f = Fixture::new();
    let outside = f.path("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.jpg"), JPEG).unwrap();
    let link = f.path("photos/jn");
    #[cfg(windows)]
    {
        // a junction needs no special rights
        let status = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert!(link.is_dir());
    assert!(scan(&f.photos(), true, &[0; 32]).photos.is_empty());
}

#[test]
fn a_cloud_synced_entry_is_not_a_link() {
    // OneDrive files and folders are reparse points without a link target; junctions and symlinks have one
    assert!(!is_link(ATTR_ARCHIVE | ATTR_REPARSE_POINT, || None));
    assert!(is_link(ATTR_DIRECTORY | ATTR_REPARSE_POINT, || Some(
        r"C:\elsewhere".into()
    )));
    assert!(!is_link(ATTR_ARCHIVE, || panic!(
        "not asked without the attribute"
    )));
}

#[test]
fn sizes_come_from_the_headers() {
    assert_eq!(size_of(&JPEG, "image/jpeg"), Some((16, 16)));
    assert_eq!(
        size_of(&png_of(4000, 3000), "image/png"),
        Some((4000, 3000))
    );
    assert_eq!(
        size_of(b"GIF89a\x40\x01\xC8\x00", "image/gif"),
        Some((320, 200))
    );
    let mut vp8x = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
    vp8x.extend_from_slice(&[10, 0, 0, 0, 0, 0, 0, 0, 0x7F, 0x07, 0, 0x37, 0x04, 0]);
    assert_eq!(size_of(&vp8x, "image/webp"), Some((1920, 1080)));
    assert_eq!(size_of(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2], "image/jpeg"), None);
    // a PNG width past int.MaxValue is clamped to it
    assert_eq!(
        size_of(&png_of(0x8000_0010, 16), "image/png"),
        Some((i64::from(i32::MAX), 16))
    );
}

#[tokio::test]
async fn a_small_file_with_huge_dimensions_is_not_sent_as_it_is() {
    // 20000 x 20000 in a few bytes: decoding it would take an old phone's WebView down
    let f = Fixture::new();
    f.write("bomb.png", &png_of(20000, 20000));
    let frame = f.frame(true, true, true);
    frame.scan_now().await;
    assert_eq!(
        frame
            .photo(Some(&frame.id_of(&f.photos(), "bomb.png")))
            .await,
        Err(TOO_LARGE)
    );
}

#[tokio::test]
async fn clearing_the_folder_deletes_the_scaled_copies() {
    let f = Fixture::new();
    let frame = f.frame(true, true, true);
    let cache = f.path("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("old.jpg"), JPEG).unwrap();
    frame
        .cfg
        .save(&f.config_json("", true, true, true), None)
        .unwrap();
    assert!(frame.list().ids.is_empty());
    assert!(!cache.exists());
}

#[test]
fn clearing_the_folder_without_the_widget_still_deletes_the_copies() {
    let f = Fixture::new();
    let frame = f.frame(true, true, true);
    let cache = f.path("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("old.jpg"), JPEG).unwrap();
    // the widget is gone and the folder cleared in the same save: the phone never asks again, the save does it
    frame
        .cfg
        .save(r#"{ "pages": [ { "id": "p", "title": "P", "widgets": [] } ], "photos": { "folder": "" } }"#, None)
        .unwrap();
    frame.settings_saved();
    assert!(!cache.exists());
}

#[test]
fn cloud_only_files_are_recognised() {
    for (attributes, want) in [
        (ATTR_NORMAL, false),
        (ATTR_ARCHIVE, false),
        (ATTR_OFFLINE, true),
        (ATTR_RECALL_ON_OPEN, true),
        (ATTR_RECALL_ON_DATA_ACCESS, true), // OneDrive Files On-Demand
    ] {
        assert_eq!(cloud_only(attributes), want, "{attributes:#x}");
    }
}

#[test]
fn the_type_comes_from_the_bytes_not_the_name() {
    assert_eq!(mime_of(&JPEG), Some("image/jpeg"));
    assert_eq!(mime_of(&png_of(16, 16)), Some("image/png"));
    assert_eq!(mime_of(b"\x89PNG\r\n"), None); // the whole 8-byte signature
    assert_eq!(mime_of(b"GIF89a\0\0\0\0\0\0"), Some("image/gif"));
    assert_eq!(mime_of(b"GIF87a\x01\x00"), Some("image/gif"));
    assert_eq!(mime_of(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
    assert_eq!(mime_of(b"<svg xmlns=\"http://www.w3.org/2000/svg\">"), None);
    assert_eq!(mime_of(b"<html><script>"), None);
    assert_eq!(mime_of(b""), None);
}

#[tokio::test]
async fn photos_are_served_by_id_only() {
    let f = Fixture::new();
    f.write("a.jpg", &JPEG);
    f.write("fake.jpg", b"<html>not an image</html>");
    let frame = f.frame(true, true, true);
    frame.scan_now().await;
    let list = frame.list();
    assert_eq!(list.error, None);
    assert_eq!(list.ids.len(), 2);
    let real = frame
        .photo(Some(&frame.id_of(&f.photos(), "a.jpg")))
        .await
        .unwrap();
    assert_eq!(real, (JPEG.to_vec(), "image/jpeg"));
    // not an image inside
    assert_eq!(
        frame
            .photo(Some(&frame.id_of(&f.photos(), "fake.jpg")))
            .await,
        Err(CANT_SHOW)
    );
    assert_eq!(frame.photo(Some("../../config.json")).await, Err(UNKNOWN));
    assert_eq!(
        frame
            .photo(Some(&frame.id_of(&f.photos(), "missing.jpg")))
            .await,
        Err(UNKNOWN)
    );
}

#[tokio::test]
async fn nothing_is_listed_or_served_without_the_widget_on_an_enabled_page() {
    let f = Fixture::new();
    f.write("a.jpg", &JPEG);
    for frame in [f.frame(false, true, true), f.frame(true, false, true)] {
        assert!(!frame.shown());
        let list = frame.list();
        assert!(list.ids.is_empty());
        assert_eq!(list.error.as_deref(), Some(ADD_WIDGET));
        assert_eq!(
            frame.photo(Some(&frame.id_of(&f.photos(), "a.jpg"))).await,
            Err(ADD_WIDGET)
        );
    }
}

#[test]
fn the_folder_never_reaches_the_phone() {
    let f = Fixture::new();
    let store = f.store(true, true, true);
    let public = crate::config::public::to_public(&store.get(), None, None);
    let text = public.to_string();
    assert!(
        !text.contains(&serde_json::to_string(&f.photos()).unwrap()),
        "{text}"
    );
    assert!(
        !text.contains(f.dir.path().file_name().unwrap().to_str().unwrap()),
        "{text}"
    );
}

/// Blocks a scan of `held` until released.
#[derive(Default)]
struct Gate {
    reached: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
}

impl Gate {
    fn wait(&self) {
        self.reached.store(true, Ordering::SeqCst);
        let released = lock(&self.released);
        let _ = self
            .wake
            .wait_timeout_while(released, Duration::from_secs(10), |r| !*r);
    }

    fn release(&self) {
        *lock(&self.released) = true;
        self.wake.notify_all();
    }

    async fn reached(&self) {
        for _ in 0..1000 {
            if self.reached.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the scan never started");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_flight_scans_do_not_outlive_folder_changes_and_zero_or_overflow_png_sizes_are_rejected()
{
    let f = Fixture::new();
    f.write("zero.png", &png_of(0, 16));
    f.write("overflow.png", &png_of(0x8000_0010, 16));
    let frame = f.frame(true, true, true);
    frame.scan_now().await;
    assert_eq!(
        frame
            .photo(Some(&frame.id_of(&f.photos(), "zero.png")))
            .await,
        Err(CANT_SHOW)
    );
    assert_eq!(
        frame
            .photo(Some(&frame.id_of(&f.photos(), "overflow.png")))
            .await,
        Err(TOO_LARGE)
    );

    let other = f.path("photos-other").to_string_lossy().into_owned();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(Path::new(&other).join("only.png"), png_of(16, 16)).unwrap();
    // the old folder's scan is held just before it publishes until the new folder's scan has finished
    let gate = Arc::new(Gate::default());
    let (held, photos) = (gate.clone(), f.photos());
    let store = Arc::new(ConfigStore::new(f.path("config.json")));
    let racing = Arc::new(
        PhotoFrame::new(store.clone(), f.dir.path())
            .with_cache_dir(f.path("cache-race"))
            .with_scanner(Arc::new(move |root, subfolders, key| {
                let lib = scan(root, subfolders, key);
                if root == photos {
                    held.wait();
                }
                lib
            })),
    );
    let _ = racing.list();
    let first = racing.scan_in_flight().unwrap();
    gate.reached().await;
    store
        .save(&f.config_json(&other, true, true, true), None)
        .unwrap();
    racing.settings_saved();
    let _ = racing.list();
    let second = racing.scan_in_flight().unwrap();
    assert_ne!(first.id, second.id);
    tokio::time::timeout(Duration::from_secs(10), second.finished())
        .await
        .unwrap();
    let only = vec![racing.id_of(&other, "only.png")];
    assert_eq!(racing.list().ids, only);
    gate.release();
    tokio::time::timeout(Duration::from_secs(10), first.finished())
        .await
        .unwrap();
    assert_eq!(racing.list().ids, only);
}

/// The scans a held frame started (with their subfolder option), in order.
type Scans = Arc<Mutex<Vec<(bool, Arc<Gate>)>>>;

/// A frame whose scans each wait for the test to let them publish, in the order they were started.
fn held_scans(store: Arc<ConfigStore>, dir: &Path) -> (Arc<PhotoFrame>, Scans) {
    let scans: Scans = Arc::new(Mutex::new(Vec::new()));
    let log = scans.clone();
    let frame = PhotoFrame::new(store, dir)
        .with_cache_dir(dir.join("cache-held"))
        .with_scanner(Arc::new(move |root, subfolders, key| {
            let lib = scan(root, subfolders, key);
            let gate = Arc::new(Gate::default());
            lock(&log).push((subfolders, gate.clone()));
            gate.wait();
            lib
        }));
    (Arc::new(frame), scans)
}

async fn reached(scans: &Mutex<Vec<(bool, Arc<Gate>)>>, count: usize) -> Arc<Gate> {
    for _ in 0..1000 {
        if lock(scans).len() >= count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let gate = lock(scans)[count - 1].1.clone();
    gate.reached().await;
    gate
}

fn release_all(scans: &Mutex<Vec<(bool, Arc<Gate>)>>) {
    for (_, gate) in lock(scans).iter() {
        gate.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_scan_for_the_same_selection_cannot_publish_over_the_one_that_owns_the_slot() {
    let f = Fixture::new();
    f.write("a.jpg", &JPEG);
    let store = f.store(true, true, true);
    let (frame, scans) = held_scans(store.clone(), f.dir.path());
    // recursive, then not, then recursive again (two requests that read the settings around a save)
    let _ = frame.list();
    let first = frame.scan_in_flight().unwrap();
    let gate = reached(&scans, 1).await;
    store
        .save(&f.config_json(&f.photos(), true, true, false), None)
        .unwrap();
    let _ = frame.list();
    reached(&scans, 2).await;
    store
        .save(&f.config_json(&f.photos(), true, true, true), None)
        .unwrap();
    let _ = frame.list();
    let third = frame.scan_in_flight().unwrap();
    reached(&scans, 3).await;
    assert_ne!(first.id, third.id);

    gate.release();
    tokio::time::timeout(Duration::from_secs(10), first.finished())
        .await
        .unwrap();
    assert!(frame.published().is_none());
    assert!(frame.list().scanning);
    release_all(&scans);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_subfolders_off_drops_the_recursive_scan_in_flight() {
    let f = Fixture::new();
    f.write("a.jpg", &JPEG);
    f.write("trip/b.jpg", &JPEG);
    let store = f.store(true, true, true);
    let (frame, scans) = held_scans(store.clone(), f.dir.path());
    let _ = frame.list();
    let recursive = frame.scan_in_flight().unwrap();
    let gate = reached(&scans, 1).await;
    store
        .save(&f.config_json(&f.photos(), true, true, false), None)
        .unwrap();
    frame.settings_saved();
    gate.release();
    tokio::time::timeout(Duration::from_secs(10), recursive.finished())
        .await
        .unwrap();
    assert!(frame.published().is_none());
    release_all(&scans);
}

#[tokio::test]
async fn an_unset_folder_shows_nothing_and_nothing_throws() {
    let f = Fixture::new();
    let path = f.path("config-unset.json");
    std::fs::write(
        &path,
        r#"{"photos":{"folder":""},"pages":[{"id":"p","title":"P","widgets":[{"id":"w","type":"photos","x":0,"y":0,"w":6,"h":4}]}]}"#,
    )
    .unwrap();
    let frame = Arc::new(
        PhotoFrame::new(Arc::new(ConfigStore::new(path)), f.dir.path())
            .with_cache_dir(f.path("cache-unset")),
    );
    frame.scan_now().await; // no source to scan: nothing to wait for
    assert_eq!(
        frame.list(),
        PhotoList {
            ids: vec![],
            error: None,
            scanning: false
        }
    );
    assert_eq!(frame.photo(Some("0123456789abcdef")).await, Err(UNKNOWN));
    assert!(frame.published().is_none());
}

#[test]
fn cache_names_match_the_csharp_agents() {
    // computed by the C# agent's CacheKey on the baseline
    let photo = |path: &str, ticks: i64, length: u64| Photo {
        id: String::new(),
        path: PathBuf::from(path),
        length,
        ticks,
    };
    assert_eq!(
        cache_key(&photo(
            "/Users/me/Pictures/Trip/IMG_0001.JPG",
            638_700_123_456_789_012,
            3_456_789
        )),
        "c6bc3b68ccddd4786e56-8dd1e60d8a3da14-34bf15"
    );
    assert_eq!(
        cache_key(&photo(
            "C:\\Users\\me\\Pictures\\é.jpg",
            621_355_968_000_000_000,
            1
        )),
        "bef839171dca8f0b792a-89f7ff5f7b58000-1"
    );
}

#[test]
fn full_paths_are_lexical() {
    if cfg!(windows) {
        assert_eq!(full_path(r"C:\a\\b\.\c\..\d\"), r"C:\a\b\d");
        assert_eq!(full_path("C:/x/y"), r"C:\x\y");
        assert_eq!(full_path(r"C:\"), r"C:\");
    } else {
        assert_eq!(full_path("/a//b/./c/../d/"), "/a/b/d");
        assert_eq!(full_path("/../x"), "/x");
        assert_eq!(full_path("/"), "/");
    }
}

#[tokio::test]
async fn a_scaled_copy_is_cached_and_kept_only_for_photos_still_there() {
    let f = Fixture::new();
    f.write("a.jpg", &JPEG);
    let frame = f.frame(true, true, true);
    frame.scan_now().await;
    let lib = frame.published().unwrap();
    let key = cache_key(&lib.photos[0]);
    let cache = f.path("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join(format!("{key}.jpg")), b"scaled").unwrap();
    std::fs::write(cache.join("stale.jpg"), b"old").unwrap();
    // a cached copy is served first
    assert_eq!(
        frame.photo(Some(&lib.photos[0].id)).await.unwrap(),
        (b"scaled".to_vec(), "image/jpeg")
    );
    // the next scan keeps only the copies of photos it found
    frame.scan_now().await;
    assert!(cache.join(format!("{key}.jpg")).exists());
    assert!(!cache.join("stale.jpg").exists());
}

#[cfg(windows)]
#[test]
fn a_large_photo_is_scaled_to_1600_pixels_and_turned_upright() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.png");
    image::RgbImage::from_pixel(3200, 1000, image::Rgb([200, 10, 10]))
        .save(&path)
        .unwrap();
    let jpeg = scale_with_image(&path).unwrap();
    let decoded = image::load_from_memory(&jpeg).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1600, 500));
}
