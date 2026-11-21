//! `CameraFeedsTests.cs` (20 here; `ThePhoneMayAskForCameraPicturesOverWifi` is in `lan.rs`): the JPEG splitting, what
//! may reach the phone or the log, and when a camera is pulled. Plus the real ffmpeg run, which no C# test covers.

use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::mpsc;
use std::sync::{Condvar, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::config::MAX_CAMERAS;

const URL: &str = "rtsp://admin:s3cret@192.168.1.20:554/stream1";

fn jpeg(fill: u8, size: usize) -> Vec<u8> {
    let mut b = vec![fill; size];
    b[0] = 0xFF;
    b[1] = 0xD8;
    b[size - 2] = 0xFF;
    b[size - 1] = 0xD9;
    b
}

fn j(fill: u8) -> Vec<u8> {
    jpeg(fill, 16)
}

#[test]
fn back_to_back_jpegs_are_split_even_across_reads() {
    let mut got = Vec::new();
    {
        let mut split = JpegSplitter::new(|f| got.push(f), JPEG_MAX);
        let stream = [j(1), jpeg(2, 40), j(3)].concat();
        // in odd pieces, including one that ends between 0xFF and 0xD9
        for chunk in stream.chunks(7) {
            split.push(chunk);
        }
    }
    assert_eq!(got, vec![j(1), jpeg(2, 40), j(3)]);
}

#[test]
fn stuffed_bytes_and_noise_before_an_image_do_not_end_or_start_it() {
    let mut got = Vec::new();
    // FF 00 inside the data (byte stuffing) is not an end; garbage before FF D8 is skipped
    let image = [0xFF, 0xD8, 0x10, 0xFF, 0x00, 0x20, 0xFF, 0xD9];
    {
        let mut split = JpegSplitter::new(|f| got.push(f), JPEG_MAX);
        split.push(&[[0x00, 0x13, 0x37].as_slice(), &image].concat());
    }
    assert_eq!(got, vec![image.to_vec()]);
}

#[test]
fn a_runaway_image_is_dropped_instead_of_growing_without_bound() {
    let mut got = Vec::new();
    {
        let mut split = JpegSplitter::new(|f| got.push(f), 64);
        split.push(&[[0xFF, 0xD8].as_slice(), &[0u8; 100]].concat()); // never ends within the limit
        split.push(&j(5));
    }
    assert_eq!(got, vec![j(5)]);
}

#[test]
fn ffmpegs_messages_become_a_fixed_state() {
    for (error, state) in [
        (
            Some("[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized"),
            "login",
        ),
        (Some("rtsp://cam/x: Connection refused"), "unreachable"),
        (
            Some("Connection to tcp://10.0.0.9:554 failed: Connection timed out"),
            "unreachable",
        ),
        (
            Some("rtsp://cam.local/x: Error number -138 occurred"),
            "unreachable",
        ),
        (
            Some(
                "[tcp @ 0x5601a4c0f401] Connection to tcp://10.0.0.9:5401/stream401 failed: Connection refused",
            ),
            "unreachable",
        ),
        (
            Some("Invalid data found when processing input"),
            "no-picture",
        ),
        (None, "no-picture"),
    ] {
        assert_eq!(classify(error), state, "{error:?}");
    }
}

#[test]
fn the_log_never_gets_the_address_or_a_password() {
    let line = format!(
        "[rtsp @ 0x55] {URL}: 401 Unauthorized; also rtsp://u:p@other/x and http://cam/cgi?user=a&pwd=hunter2&x=1"
    );
    let masked = mask(&line, URL);
    assert!(!masked.contains("s3cret"));
    assert!(!masked.contains("u:p@"));
    assert!(!masked.contains("hunter2"));
    assert!(masked.contains("<camera>"));
    // ffmpeg also names the host on its own
    let host = mask(
        "Connection to tcp://192.168.1.20:554 failed: Connection timed out",
        URL,
    );
    assert!(!host.contains("192.168.1.20"));
    assert!(host.contains("Connection timed out"));
    // a host named like a secret parameter doesn't stop the parameter from being hidden
    let tricky = mask(
        "rtsp://token:554/live?token=secret123: Connection refused",
        "rtsp://admin:pw@token:554/live?token=secret123",
    );
    assert!(!tricky.contains("secret123"));
    // user info stops at ? and #: "?token=x@" is a query, and its value is still hidden
    assert!(!mask("rtsp://cam?token=secret123@x: refused", "").contains("secret123"));
    // a long line with the password far in is masked whole, then shortened
    let long_password = "p".repeat(500);
    let long_line = mask(
        &format!("[rtsp @ 0x1] rtsp://admin:{long_password}@10.0.0.2/live: 401 Unauthorized"),
        "",
    );
    assert!(!long_line.contains("pppp"));
    assert!(masked.contains("401 Unauthorized"));
}

#[test]
fn only_rtsp_addresses_are_handed_to_ffmpeg() {
    for (url, ok) in [
        ("rtsp://user:pw@10.0.0.2:554/live", true),
        ("RTSPS://cam.example/live", true),
        ("rtsp:///nohost", false),
        ("file:///C:/Windows/win.ini", false),
        ("concat:a.ts|b.ts", false),
        ("http://cam/video.mjpg", false),
        ("-i evil", false),
    ] {
        assert_eq!(valid_url(url), ok, "{url}");
    }
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("config.json")
    }

    fn read(&self) -> String {
        std::fs::read_to_string(self.path()).unwrap()
    }

    fn store_with(&self, camera_option: &str, page_enabled: bool) -> Arc<ConfigStore> {
        std::fs::write(
            self.path(),
            format!(
                r#"{{"cameras":{{"sources":[{{"id":"cam000001","name":"Door","url":"{URL}"}}]}},
 "pages":[{{"id":"p","title":"P","enabled":{page_enabled},"widgets":[{{"id":"w","type":"camera","x":0,"y":0,"w":4,"h":3,"options":{{"camera":"{camera_option}"}}}}]}},
          {{"id":"q","title":"Q","widgets":[{{"id":"c","type":"clock","x":0,"y":0,"w":4,"h":3}}]}}]}}"#
            ),
        )
        .unwrap();
        Arc::new(ConfigStore::new(self.path()))
    }

    fn store(&self) -> Arc<ConfigStore> {
        self.store_with("cam000001", true)
    }
}

#[test]
fn the_address_never_goes_to_the_phone() {
    let fx = Fixture::new();
    let public = serde_json::to_string(&crate::config::public::to_public(
        &fx.store().get(),
        None,
        None,
    ))
    .unwrap();
    assert!(!public.contains("s3cret"));
    assert!(!public.contains("192.168.1.20"));
    assert!(!public.contains("Door")); // only the id and the frame rate, for pacing the widget
    // C# asserts its default serializer's PascalCase {"Id","Fps"}; the phone gets camelCase through /api/config
    assert!(public.contains(r#""cameras":[{"id":"cam000001","fps":5}]"#));
}

#[test]
fn the_frame_rate_stays_within_its_range() {
    for (asked, kept) in [(0, 5), (1, 1), (10, 10), (60, 5)] {
        let fx = Fixture::new();
        let store = fx.store();
        store
            .save(
                &format!(
                    r#"{{"cameras":{{"sources":[{{"id":"cam000001","name":"Door","url":"rtsp://10.0.0.3/a","fps":{asked}}}]}}}}"#
                ),
                None,
            )
            .unwrap();
        assert_eq!(store.get().cameras.sources[0].fps, kept, "{asked}");
    }
}

#[test]
fn saving_refuses_anything_but_rtsp_and_keeps_the_camera_id() {
    let fx = Fixture::new();
    let store = fx.store();
    let refused = store
        .save(
            r#"{"cameras":{"sources":[{"id":"cam000001","name":"Door","url":"file:///etc/passwd"}]}}"#,
            None,
        )
        .unwrap_err();
    assert!(refused.contains("rtsp://"), "{refused}");
    store
        .save(
            r#"{"cameras":{"sources":[{"id":"cam000001","name":"Front door","url":"rtsp://10.0.0.3/a"},{"name":"","url":"  "},{"name":"New","url":"rtsp://10.0.0.4/b","transport":"UDP"}]}}"#,
            None,
        )
        .unwrap();
    let cfg = store.get();
    let sources = &cfg.cameras.sources;
    assert_eq!(sources.len(), 2); // the empty row is dropped
    assert_eq!(sources[0].id, "cam000001"); // renamed, same id: its widgets keep showing it
    // a new camera gets a random id, not one made from its name
    assert!(
        regex::Regex::new("^[a-z0-9]{12}$")
            .unwrap()
            .is_match(&sources[1].id)
    );
    assert_eq!(sources[1].transport, "udp");
    // more than the limit is refused with a message, not silently cut
    let nine = (1..=MAX_CAMERAS + 1)
        .map(|i| format!(r#"{{"name":"C{i}","url":"rtsp://10.0.0.{i}/a"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let refused = store
        .save(&format!(r#"{{"cameras":{{"sources":[{nine}]}}}}"#), None)
        .unwrap_err();
    assert!(refused.contains(&format!("more than {MAX_CAMERAS} cameras")));
}

#[derive(Default)]
struct FakeState {
    has_ended: AtomicBool,
    last_error: Mutex<Option<String>>,
    disposed: AtomicBool,
}

struct FakeRun(Arc<FakeState>);

impl CameraRun for FakeRun {
    fn has_ended(&self) -> bool {
        self.0.has_ended.load(Ordering::SeqCst)
    }

    fn last_error(&self) -> Option<String> {
        self.0.last_error.lock().unwrap().clone()
    }
}

impl Drop for FakeRun {
    fn drop(&mut self) {
        self.0.disposed.store(true, Ordering::SeqCst);
    }
}

struct Launched {
    run: Arc<FakeState>,
    frame: OnFrame,
    source: CameraSource,
}

struct Rig {
    feeds: Arc<CameraFeeds>,
    runs: Arc<Mutex<Vec<Launched>>>,
    now: Arc<AtomicI64>,
}

impl Rig {
    fn advance(&self, ms: i64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    fn count(&self) -> usize {
        self.runs.lock().unwrap().len()
    }

    fn run(&self, i: usize) -> Arc<FakeState> {
        self.runs.lock().unwrap()[i].run.clone()
    }

    fn disposed(&self, i: usize) -> bool {
        self.run(i).disposed.load(Ordering::SeqCst)
    }

    fn source(&self, i: usize) -> CameraSource {
        self.runs.lock().unwrap()[i].source.clone()
    }

    /// Hands run `i` a picture, the way its reader would: from outside the launch and without the runs list held.
    fn send(&self, i: usize, picture: Vec<u8>) {
        let frame = self.runs.lock().unwrap()[i].frame.clone();
        frame(picture);
    }

    fn get(&self, widget: &str) -> FrameAnswer {
        self.feeds.frame(None, Some(widget), false)
    }
}

type Hook = Arc<dyn Fn() + Send + Sync>;

fn make(store: Arc<ConfigStore>, ffmpeg: Option<&str>, before_clock_read: Option<Hook>) -> Rig {
    let runs: Arc<Mutex<Vec<Launched>>> = Default::default();
    let now = Arc::new(AtomicI64::new(1_000_000));
    let launch: Launcher = {
        let runs = runs.clone();
        Arc::new(
            move |_ffmpeg: &str, source: &CameraSource, on_frame: OnFrame| {
                let state: Arc<FakeState> = Default::default();
                runs.lock().unwrap().push(Launched {
                    run: state.clone(),
                    frame: on_frame,
                    source: source.clone(),
                });
                Ok(Box::new(FakeRun(state)) as Box<dyn CameraRun>)
            },
        )
    };
    let clock = {
        let now = now.clone();
        Arc::new(move || {
            if let Some(hook) = &before_clock_read {
                hook();
            }
            now.load(Ordering::SeqCst)
        })
    };
    let ffmpeg = ffmpeg.map(str::to_string);
    let feeds = CameraFeeds::new(store, launch)
        .with_clock(clock)
        .with_find_ffmpeg(Arc::new(move || ffmpeg.clone()));
    Rig {
        feeds: Arc::new(feeds),
        runs,
        now,
    }
}

#[test]
fn a_picture_a_widget_already_has_is_not_sent_again() {
    let fx = Fixture::new();
    let rig = make(fx.store(), Some("ffmpeg"), None);
    let ask = |phone: &str, fresh: bool| rig.feeds.frame(Some(phone), Some("p/w"), fresh);
    ask("phone1", false);
    rig.send(0, j(1));
    assert_eq!(ask("phone1", false).0, 200);
    assert_eq!(ask("phone1", false).0, 204); // nothing newer: nothing to download
    assert_eq!(ask("phone2", false).0, 200); // another phone hasn't had it yet
    assert_eq!(ask("phone1", true).0, 200); // the widget lost its picture: send it again
    rig.send(0, j(2));
    let next = ask("phone1", false);
    assert_eq!(next.0, 200);
    assert_eq!(next.1.as_deref(), Some(j(2).as_slice()));
}

fn queued_frame_request_starts_with_current_camera_settings(fps: i32) {
    let fx = Fixture::new();
    let store = fx.store();
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let released = Arc::new((Mutex::new(false), Condvar::new()));
    let hook: Hook = {
        let released = released.clone();
        let entered = Mutex::new(Some(entered_tx));
        Arc::new(move || {
            if let Some(tx) = entered.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let (flag, signal) = &*released;
            let held = flag.lock().unwrap();
            let _ = signal
                .wait_timeout_while(held, Duration::from_secs(5), |go| !*go)
                .unwrap();
        })
    };
    let rig = make(store.clone(), Some("ffmpeg"), Some(hook));
    let feeds = rig.feeds.clone();
    let pending = std::thread::spawn(move || feeds.frame(None, Some("p/w"), false));
    let release = || {
        let (flag, signal) = &*released;
        *flag.lock().unwrap() = true;
        signal.notify_all();
    };
    let entered = entered_rx.recv_timeout(Duration::from_secs(5));
    if entered.is_ok() && fps != DEFAULT_FPS {
        let mut settings: Value = serde_json::from_str(&fx.read()).unwrap();
        settings["cameras"]["sources"][0]["fps"] = json!(fps);
        store.save(&settings.to_string(), None).unwrap();
        rig.feeds.settings_saved();
    }
    release();
    entered.expect("the request reads the clock");
    let (status, _, state) = pending.join().unwrap();
    assert_eq!((status, state), (503, "starting"));
    assert_eq!(rig.count(), 1);
    let actual = rig.source(0).fps;
    assert!(actual == fps, "EXPECTED: fps={fps} | ACTUAL: fps={actual}");
}

#[test]
fn a_queued_frame_request_starts_with_current_camera_settings() {
    for fps in [5, 8, 10] {
        queued_frame_request_starts_with_current_camera_settings(fps);
    }
}

#[test]
fn a_widget_on_an_enabled_page_starts_its_camera_and_gets_the_latest_frame() {
    let fx = Fixture::new();
    let rig = make(fx.store(), Some("ffmpeg"), None);
    let first = rig.get("p/w");
    assert_eq!((first.0, first.2), (503, "starting"));
    assert_eq!(rig.count(), 1);
    assert_eq!(rig.source(0).url, URL);
    rig.send(0, j(1));
    rig.send(0, j(2));
    let live = rig.get("p/w");
    assert_eq!(live.0, 200);
    assert_eq!(live.1.as_deref(), Some(j(2).as_slice()));
    // asking again doesn't start a second ffmpeg
    rig.get("p/w");
    assert_eq!(rig.count(), 1);
}

#[test]
fn nothing_is_pulled_without_a_camera_widget_on_an_enabled_page() {
    for (key, page_enabled, option, status, state) in [
        ("p/w", false, "cam000001", 404, "unknown"), // the page is hidden
        ("p/nope", true, "cam000001", 404, "unknown"),
        ("q/c", true, "cam000001", 404, "unknown"), // a widget, but not a camera
        ("p/w", true, "", 404, "pick"),
        ("p/w", true, "removed123", 404, "pick"),
    ] {
        let fx = Fixture::new();
        let rig = make(fx.store_with(option, page_enabled), Some("ffmpeg"), None);
        let r = rig.get(key);
        assert_eq!((r.0, r.2), (status, state), "{key} {page_enabled} {option}");
        assert!(r.1.is_none());
        assert_eq!(rig.count(), 0);
    }
}

#[test]
fn a_camera_stops_soon_after_the_panel_stops_asking() {
    let fx = Fixture::new();
    let rig = make(fx.store(), Some("ffmpeg"), None);
    rig.get("p/w");
    rig.advance(KEEP_MS - 1000);
    rig.feeds.tick();
    assert!(!rig.disposed(0));
    rig.advance(2000);
    rig.feeds.tick();
    assert!(rig.disposed(0));
    // the next request starts it again
    rig.get("p/w");
    assert_eq!(rig.count(), 2);
}

#[test]
fn an_old_frame_is_not_shown_as_live_and_a_stalled_ffmpeg_is_restarted_later() {
    let fx = Fixture::new();
    let rig = make(fx.store(), Some("ffmpeg"), None);
    rig.get("p/w");
    rig.send(0, j(1));
    rig.advance(FRESH_MS + 1000);
    let stale = rig.get("p/w");
    assert_eq!(stale.0, 503);
    assert!(stale.1.is_none());
    // keep asking, but no frame arrives: after the stall limit ffmpeg is stopped, then started again after a pause
    for _ in (0..STALL_MS).step_by(1000) {
        rig.advance(1000);
        rig.get("p/w");
        rig.feeds.tick();
    }
    assert!(rig.disposed(0));
    for _ in 0..3 {
        rig.advance(1000);
        rig.get("p/w");
        rig.feeds.tick();
    }
    assert_eq!(rig.count(), 2);
}

#[test]
fn a_failed_login_is_reported_as_a_state_without_ffmpegs_text() {
    let fx = Fixture::new();
    let rig = make(fx.store(), Some("ffmpeg"), None);
    rig.get("p/w");
    *rig.run(0).last_error.lock().unwrap() = Some(format!(
        "[rtsp @ 0x1] {URL}: method DESCRIBE failed: 401 Unauthorized"
    ));
    rig.run(0).has_ended.store(true, Ordering::SeqCst);
    rig.advance(1000);
    rig.feeds.tick();
    let r = rig.get("p/w");
    assert_eq!((r.0, r.2), (503, "login"));
}

#[test]
fn a_camera_that_never_answers_is_reported_unreachable() {
    let fx = Fixture::new();
    let rig = make(fx.store(), Some("ffmpeg"), None);
    // ffmpeg waits on the connection without a word and no picture ever comes
    for _ in (0..=STALL_MS + 1000).step_by(1000) {
        rig.advance(1000);
        rig.get("p/w");
        rig.feeds.tick();
    }
    assert!(rig.disposed(0));
    assert_eq!(rig.get("p/w").2, "unreachable");
}

#[test]
fn a_stopped_ffmpegs_late_frame_is_not_shown_and_does_not_reset_the_backoff() {
    let fx = Fixture::new();
    let rig = make(fx.store(), Some("ffmpeg"), None);
    rig.get("p/w");
    rig.run(0).has_ended.store(true, Ordering::SeqCst);
    rig.advance(1000);
    rig.feeds.tick(); // ended: stopped, next try after a pause
    rig.send(0, j(9)); // a frame still in its pipe arrives after that
    let r = rig.get("p/w");
    assert_eq!(r.0, 503);
    assert!(r.1.is_none());
    assert_eq!(rig.count(), 1); // and the pause still holds: no new ffmpeg yet
    rig.advance(2500);
    rig.feeds.tick();
    assert_eq!(rig.count(), 2);
    rig.send(0, j(9)); // the old one again, now that a new one runs
    assert_eq!(rig.get("p/w").0, 503);
    rig.send(1, j(1));
    assert_eq!(rig.get("p/w").1.as_deref(), Some(j(1).as_slice()));
}

#[test]
fn without_ffmpeg_nothing_starts_and_the_panel_is_told() {
    let fx = Fixture::new();
    let rig = make(fx.store(), None, None);
    assert_eq!(rig.get("p/w").2, "no-ffmpeg");
    assert_eq!(rig.count(), 0);
}

#[test]
fn a_changed_address_restarts_the_camera_and_a_removed_one_stops() {
    let fx = Fixture::new();
    let store = fx.store();
    let rig = make(store.clone(), Some("ffmpeg"), None);
    rig.get("p/w");
    // same address, new name: keeps running
    store
        .save(&fx.read().replace("\"Door\"", "\"Front door\""), None)
        .unwrap();
    rig.feeds.settings_saved();
    assert!(!rig.disposed(0));
    // a new frame rate restarts it too (ffmpeg's filter has it)
    let saved = fx.read();
    assert!(saved.contains("\"fps\": 5"));
    store
        .save(&saved.replace("\"fps\": 5", "\"fps\": 8"), None)
        .unwrap();
    rig.feeds.settings_saved();
    assert!(rig.disposed(0));
    rig.get("p/w");
    assert_eq!(rig.source(1).fps, 8);
    // new address: stopped, and the next request uses the new one
    store
        .save(&fx.read().replace("192.168.1.20", "192.168.1.21"), None)
        .unwrap();
    rig.feeds.settings_saved();
    assert!(rig.disposed(1));
    rig.get("p/w");
    assert!(rig.source(2).url.contains("192.168.1.21"));
}

// ---- not covered by the C# tests: the real ffmpeg run ----

#[test]
fn ffmpeg_gets_one_argument_per_item_and_udp_only_without_tls() {
    let source = |url: &str, transport: &str, fps: i32| CameraSource {
        url: url.into(),
        transport: transport.into(),
        fps,
        ..CameraSource::default()
    };
    assert_eq!(
        ffmpeg_args(&source(URL, "tcp", 5)),
        [
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-rtsp_transport",
            "tcp",
            "-i",
            URL,
            "-an",
            "-sn",
            "-dn",
            "-vf",
            "fps=5,scale='min(1280,iw)':-2",
            "-f",
            "image2pipe",
            "-c:v",
            "mjpeg",
            "-q:v",
            "6",
            "pipe:1",
        ]
    );
    let udp = ffmpeg_args(&source("rtsp://cam/a", "udp", 0));
    assert_eq!(udp[5], "udp");
    assert_eq!(udp[12], "fps=1,scale='min(1280,iw)':-2"); // the frame rate is clamped to 1..10
    let tls = ffmpeg_args(&source("RTSPS://cam/a", "udp", 60));
    assert_eq!(tls[5], "tcp"); // rtsps is TLS, which needs TCP
    assert_eq!(tls[12], "fps=10,scale='min(1280,iw)':-2");
}

#[test]
fn a_configured_ffmpeg_must_exist_and_quotes_around_it_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ffmpeg-test");
    std::fs::write(&file, b"").unwrap();
    let path = file.to_string_lossy().into_owned();
    assert_eq!(
        find_ffmpeg(&format!(" \"{path}\" ")).as_deref(),
        Some(path.as_str())
    );
    assert_eq!(
        find_ffmpeg(&dir.path().join("missing").to_string_lossy()),
        None
    );
    assert_eq!(find_ffmpeg(&dir.path().to_string_lossy()), None); // a folder is not ffmpeg
}

#[cfg(unix)]
fn script(dir: &std::path::Path, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("ffmpeg");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

#[cfg(unix)]
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[test]
fn a_real_run_splits_its_pictures_keeps_the_last_three_error_lines_and_ends_with_both_pipes() {
    let dir = tempfile::tempdir().unwrap();
    // two pictures on stdout; five error lines (\r, \r\n and \n endings, one empty, one far too long) on stderr
    let ffmpeg = script(
        dir.path(),
        r#"printf '\377\330\001\377\331\377\330\002\377\331'
printf 'one\rtwo\r\n\nthree\n' >&2
head -c 9000 /dev/zero | tr '\0' 'x' >&2
printf '\nfour' >&2"#,
    );
    let got: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sink = got.clone();
    let source = CameraSource {
        url: URL.into(),
        ..CameraSource::default()
    };
    let run = FfmpegRun::start(
        &ffmpeg,
        &source,
        Arc::new(move |f| sink.lock().unwrap().push(f)),
    )
    .unwrap();
    wait_until("both pipes to end", || run.has_ended());
    assert_eq!(
        *got.lock().unwrap(),
        vec![
            vec![0xFF, 0xD8, 0x01, 0xFF, 0xD9],
            vec![0xFF, 0xD8, 0x02, 0xFF, 0xD9]
        ]
    );
    let last = run.last_error().unwrap();
    let parts: Vec<&str> = last.split(" | ").collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], "three");
    assert_eq!(parts[1], "x".repeat(8192)); // kept whole up to 8192 characters, cut there
    assert_eq!(parts[2], "four");
}

#[cfg(unix)]
#[test]
fn stopping_a_run_ends_ffmpeg_and_what_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("child.pid");
    let ffmpeg = script(
        dir.path(),
        &format!("sleep 30 &\necho $! > '{}'\nwait", marker.display()),
    );
    let run = FfmpegRun::start(&ffmpeg, &CameraSource::default(), Arc::new(|_| {})).unwrap();
    let pid = run.pid.unwrap();
    wait_until("the child to start", || {
        std::fs::read_to_string(&marker).is_ok_and(|t| t.trim().parse::<u32>().is_ok())
    });
    let child: u32 = std::fs::read_to_string(&marker)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!run.has_ended());
    drop(run);
    assert!(!crate::tool_process::tests::alive(child));
    assert!(!crate::tool_process::tests::alive(pid));
}
