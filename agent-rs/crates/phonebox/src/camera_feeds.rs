//! Port of `CameraFeeds.cs`: the pictures of the IP cameras the panel shows. One ffmpeg per camera turns its RTSP
//! stream into a few JPEG frames a second and only the latest is kept. A camera is pulled only while a Camera widget on
//! an enabled page asks for it, and stops a few seconds after the last request. ffmpeg's own messages can contain the
//! address (and with it the password), so they never reach the phone: it gets one of a few fixed states, and the log
//! gets a masked line.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use regex::{NoExpand, Regex, RegexBuilder};

use crate::config::CameraSource;
use crate::config::store::ConfigStore;
use crate::dotnet_text::{utf16_len, utf16_prefix};
use crate::server::Stop;

const CATEGORY: &str = "PhoneBox.Agent.CameraFeeds";

pub const WIDGET_TYPE: &str = "camera";
/// At most this many cameras are pulled at the same time (each one is a decoder running on this computer).
pub const MAX_RUNNING: usize = 4;
/// Pictures a second ffmpeg produces for a camera unless its settings say otherwise, and the most it may ask for.
pub const DEFAULT_FPS: i32 = 5;
pub const MAX_FPS: i32 = 10;
/// A camera stops this long after the last request.
pub(crate) const KEEP_MS: i64 = 10_000;
/// No frame for this long: ffmpeg is restarted.
pub(crate) const STALL_MS: i64 = 20_000;
/// An older frame isn't shown as live.
pub(crate) const FRESH_MS: i64 = 5_000;
pub(crate) const MAX_BACKOFF_MS: i64 = 60_000;
/// Without ffmpeg (or when it cannot start), the next try is this much later.
const NO_FFMPEG_RETRY_MS: i64 = 10_000;
/// `JpegSplitter`'s default limit for one picture.
pub const JPEG_MAX: usize = 4 * 1024 * 1024;
/// ffmpeg's error lines are kept whole up to this many characters (an address is at most 2048): cutting one before
/// `mask` could leave half a password.
const ERROR_LINE_CHARS: usize = 8192;

/// A running camera decoder (ffmpeg, or a fake in the tests). Dropping it stops it.
pub trait CameraRun: Send {
    /// Both of its output pipes are at their end (or it was stopped).
    fn has_ended(&self) -> bool;
    /// The last thing it reported (raw: may contain the address), or None.
    fn last_error(&self) -> Option<String>;
}

/// Where a run's pictures go.
pub type OnFrame = Arc<dyn Fn(Vec<u8>) + Send + Sync>;
/// Starts ffmpeg (the path) for a camera; frames go to the callback, which must not be called from inside the launch.
pub type Launcher =
    Arc<dyn Fn(&str, &CameraSource, OnFrame) -> Result<Box<dyn CameraRun>, String> + Send + Sync>;
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
type FindFfmpeg = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// What `/api/camera` answers: the status, the picture (200 only) and the state the panel shows.
pub type FrameAnswer = (u16, Option<Bytes>, &'static str);

struct Feed {
    source: CameraSource,
    run: Option<Box<dyn CameraRun>>,
    frame: Option<Bytes>,
    frame_at: i64,
    started_at: i64,
    wanted_until: i64,
    retry_at: i64,
    backoff: i64,
    /// Which start the running ffmpeg is: a stopped one's last buffered frame mustn't count for the next. Unique across
    /// feeds, so a removed feed's ffmpeg can't feed one created later for the same camera.
    generation: u64,
    /// Counts the pictures, so a panel that already has the latest one isn't sent it again.
    frame_seq: i64,
    /// The last picture each asker (phone + widget) got.
    served: HashMap<String, i64>,
    got_frame: bool,
    state: &'static str,
}

impl Feed {
    fn new(source: CameraSource) -> Feed {
        Feed {
            source,
            run: None,
            frame: None,
            frame_at: 0,
            started_at: 0,
            wanted_until: 0,
            retry_at: 0,
            backoff: 0,
            generation: 0,
            frame_seq: 0,
            served: HashMap::new(),
            got_frame: false,
            state: "starting",
        }
    }
}

#[derive(Default)]
struct Gate {
    feeds: HashMap<String, Feed>,
    disposed: bool,
    generations: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct CameraFeeds {
    cfg: Arc<ConfigStore>,
    launch: Launcher,
    now: Clock,
    find_ffmpeg: FindFfmpeg,
    gate: Arc<Mutex<Gate>>,
}

impl CameraFeeds {
    pub fn new(cfg: Arc<ConfigStore>, launch: Launcher) -> CameraFeeds {
        let started = Instant::now();
        let settings = cfg.clone();
        CameraFeeds {
            cfg,
            launch,
            now: Arc::new(move || started.elapsed().as_millis() as i64),
            find_ffmpeg: Arc::new(move || find_ffmpeg(&settings.get().cameras.ffmpeg)),
            gate: Default::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clock(mut self, now: Clock) -> CameraFeeds {
        self.now = now;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_find_ffmpeg(mut self, find: FindFfmpeg) -> CameraFeeds {
        self.find_ffmpeg = find;
        self
    }

    /// The latest picture for the Camera widget the phone names ("pageId/widgetId"): 200 with a JPEG, 204 when this
    /// phone's widget already has it, or a status with one of the states the panel shows: "unknown" (no such widget on
    /// an enabled page), "pick" (no camera chosen, or it was removed), "starting", "no-ffmpeg", "unreachable", "login",
    /// "no-picture", "busy" (too many cameras at once). `fresh`: the widget shows no picture, so the latest one is sent
    /// even if it was sent before.
    pub fn frame(
        &self,
        device_id: Option<&str>,
        widget_key: Option<&str>,
        fresh: bool,
    ) -> FrameAnswer {
        // the clock before the lock, the settings inside it: a request that waited for the lock starts its camera with
        // the settings saved meanwhile
        let now = (self.now)();
        let mut gate = lock(&self.gate);
        let cfg = self.cfg.get();
        let Some(widget) = crate::panel_widgets::find(&cfg, device_id, widget_key, WIDGET_TYPE)
        else {
            return (404, None, "unknown");
        };
        let id = widget.str("camera").unwrap_or("");
        let Some(source) = cfg
            .cameras
            .sources
            .iter()
            .find(|s| !id.is_empty() && s.id == id)
        else {
            return (404, None, "pick");
        };
        if gate.disposed {
            return (503, None, "starting");
        }
        if !gate.feeds.contains_key(id) {
            if gate.feeds.len() >= MAX_RUNNING {
                return (503, None, "busy");
            }
            gate.feeds.insert(id.to_string(), Feed::new(source.clone()));
        }
        let Gate {
            feeds, generations, ..
        } = &mut *gate;
        let feed = feeds.get_mut(id).expect("added above");
        feed.wanted_until = now + KEEP_MS;
        if feed.run.is_none() && now >= feed.retry_at {
            *generations += 1;
            self.start_locked(id, feed, now, *generations);
        }
        if let Some(jpeg) = &feed.frame
            && now - feed.frame_at <= FRESH_MS
        {
            // this phone's widget already has this picture: 204, nothing to download or decode again
            let asker = format!("{}\n{}", device_id.unwrap_or(""), widget_key.unwrap_or(""));
            if !fresh && feed.served.get(&asker) == Some(&feed.frame_seq) {
                return (204, None, "live");
            }
            if feed.served.len() > 64 {
                feed.served.clear(); // a few phones and widgets at most; never grows without bound
            }
            feed.served.insert(asker, feed.frame_seq);
            return (200, Some(jpeg.clone()), "live");
        }
        (503, None, feed.state)
    }

    /// The settings were saved: a camera whose address, transport or frame rate changed starts again, a removed one
    /// stops, a renamed one keeps running.
    pub fn settings_saved(&self) {
        let cfg = self.cfg.get();
        let sources: HashMap<&str, &CameraSource> = cfg
            .cameras
            .sources
            .iter()
            .map(|s| (s.id.as_str(), s))
            .collect();
        let mut stop = Vec::new();
        lock(&self.gate).feeds.retain(|id, feed| {
            if let Some(now) = sources.get(id.as_str())
                && now.url == feed.source.url
                && now.transport == feed.source.transport
                && now.fps == feed.source.fps
            {
                feed.source = (*now).clone();
                return true;
            }
            stop.extend(feed.run.take());
            false
        });
        drop(stop); // outside the lock
    }

    /// Once a second: stops cameras nobody asks for, restarts a stalled or ended ffmpeg (with a growing pause).
    pub(crate) fn tick(&self) {
        let now = (self.now)();
        let mut stop: Vec<Box<dyn CameraRun>> = Vec::new();
        {
            let mut gate = lock(&self.gate);
            let Gate {
                feeds, generations, ..
            } = &mut *gate;
            let ids: Vec<String> = feeds.keys().cloned().collect();
            for id in ids {
                let Some(feed) = feeds.get_mut(&id) else {
                    continue;
                };
                if now >= feed.wanted_until {
                    if let Some(run) = feed.run.take() {
                        stop.push(run);
                        crate::log::info(
                            CATEGORY,
                            &format!("camera {}: stopped (not on screen)", feed.source.name),
                        );
                    }
                    feeds.remove(&id);
                    continue;
                }
                if let Some(run) = &feed.run {
                    let ended = run.has_ended();
                    let since = if feed.got_frame {
                        feed.frame_at
                    } else {
                        feed.started_at
                    };
                    if !ended && now - since <= STALL_MS {
                        continue;
                    }
                    // ffmpeg can wait on an unreachable camera for a long time without a word (and a timeout option
                    // isn't usable: its meaning changed between ffmpeg versions), so silence before any picture
                    // counts as unreachable
                    let said = run.last_error();
                    feed.state = match &said {
                        Some(said) => classify(Some(said)),
                        None if feed.got_frame => "no-picture",
                        None => "unreachable",
                    };
                    crate::log::info(
                        CATEGORY,
                        &format!(
                            "camera {}: {} ({})",
                            feed.source.name,
                            if ended {
                                "ffmpeg ended"
                            } else {
                                "no picture for a while"
                            },
                            mask(said.as_deref().unwrap_or(""), &feed.source.url)
                        ),
                    );
                    stop.extend(feed.run.take());
                    *generations += 1;
                    feed.generation = *generations;
                    feed.frame = None;
                    feed.backoff = MAX_BACKOFF_MS.min(if feed.backoff == 0 {
                        2000
                    } else {
                        feed.backoff * 2
                    });
                    feed.retry_at = now + feed.backoff;
                } else if now >= feed.retry_at {
                    *generations += 1;
                    self.start_locked(&id, feed, now, *generations);
                }
            }
        }
        drop(stop); // outside the lock
    }

    /// Starts ffmpeg for a feed with no run, as start `generation`; called with the lock held.
    fn start_locked(&self, id: &str, feed: &mut Feed, now: i64, generation: u64) {
        let Some(ffmpeg) = (self.find_ffmpeg)() else {
            feed.state = "no-ffmpeg";
            feed.retry_at = now + NO_FFMPEG_RETRY_MS;
            return;
        };
        feed.started_at = now;
        // the last error is kept while retrying
        if !matches!(feed.state, "no-picture" | "unreachable" | "login") {
            feed.state = "starting";
        }
        feed.generation = generation;
        feed.got_frame = false;
        match (self.launch)(&ffmpeg, &feed.source, self.on_frame(id, generation)) {
            Ok(run) => {
                feed.run = Some(run);
                crate::log::info(CATEGORY, &format!("camera {}: started", feed.source.name));
            }
            Err(e) => {
                feed.run = None;
                feed.state = "no-ffmpeg";
                feed.retry_at = now + NO_FFMPEG_RETRY_MS;
                crate::log::warn(
                    CATEGORY,
                    &format!("camera {}: cannot start ffmpeg: {e}", feed.source.name),
                );
            }
        }
    }

    /// Where start `generation` of camera `id` puts its pictures: only while it is still the running one.
    fn on_frame(&self, id: &str, generation: u64) -> OnFrame {
        let gate = Arc::downgrade(&self.gate);
        let now = self.now.clone();
        let id = id.to_string();
        Arc::new(move |jpeg: Vec<u8>| {
            let Some(gate) = gate.upgrade() else {
                return;
            };
            let mut gate = lock(&gate);
            let Some(f) = gate.feeds.get_mut(&id) else {
                return;
            };
            if f.generation != generation || f.run.is_none() {
                return; // from an ffmpeg that was stopped since
            }
            f.frame = Some(Bytes::from(jpeg));
            f.frame_at = now();
            f.frame_seq += 1;
            f.got_frame = true;
            f.state = "live";
            f.backoff = 0;
        })
    }

    /// The 1 s timer, until `stop`; then every ffmpeg is stopped.
    pub fn start(self: &Arc<Self>, stop: Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(1),
                Duration::from_secs(1),
            );
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        let feeds = me.clone();
                        // it stops and starts processes: off the async workers
                        if let Err(e) = tokio::task::spawn_blocking(move || feeds.tick()).await {
                            crate::log::debug(CATEGORY, &format!("camera: {e}"));
                        }
                    }
                    _ = stop.stopped() => break,
                }
            }
            me.dispose();
        })
    }

    /// Stops every camera; later requests answer "starting".
    pub fn dispose(&self) {
        let stop: Vec<Box<dyn CameraRun>> = {
            let mut gate = lock(&self.gate);
            gate.disposed = true;
            gate.feeds.drain().filter_map(|(_, f)| f.run).collect()
        };
        drop(stop);
    }
}

/// ffmpeg's last words, as one of the states the phone shows; the text itself never leaves this module.
pub fn classify(error: Option<&str>) -> &'static str {
    let e = error.unwrap_or("").to_lowercase();
    let says = |words: &[&str]| words.iter().any(|w| e.contains(w));
    // not a bare "401": ffmpeg's text has pointers and the address in it ([rtsp @ 0x55d4401a2b0c], :5401, /stream401)
    if says(&["unauthorized", "authorization failed", "403 forbidden"]) {
        return "login";
    }
    if says(&[
        "connection refused",
        "timed out",
        "no route to host",
        "unreachable",
        "could not resolve",
        "name or service not known",
        "no such host",
        "failed to resolve",
        "connection reset",
        "i/o error",
        "error number -138",
    ]) {
        return "unreachable";
    }
    "no-picture"
}

static USER_INFO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"://[^/\s@?#]*@").expect("a valid pattern"));
static SECRET_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)((?:pass(?:word)?|pwd|token|key|auth|secret)=)[^&\s]*")
        .expect("a valid pattern")
});

/// `string.Replace(old, new, StringComparison.OrdinalIgnoreCase)`.
fn replace_ignore_case(text: &str, old: &str, new: &str) -> String {
    match RegexBuilder::new(&regex::escape(old))
        .case_insensitive(true)
        .build()
    {
        Ok(re) => re.replace_all(text, NoExpand(new)).into_owned(),
        Err(_) => text.replace(old, new),
    }
}

/// A line for the log with the address and any password in it hidden.
pub fn mask(text: &str, url: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut text = text.to_string();
    if !url.is_empty() {
        text = replace_ignore_case(&text, url, "<camera>");
    }
    // secrets first: a host that happens to be a word like "token" mustn't break "token=" before it is hidden
    text = USER_INFO.replace_all(&text, "://***@").into_owned();
    text = SECRET_PARAM.replace_all(&text, "${1}***").into_owned();
    // ffmpeg also names the host on its own ("Connection to tcp://10.0.0.9:554 failed"); a very short host could
    // match inside other words, so only from 3 characters on
    if let Some(host) = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        && utf16_len(&host) >= 3
    {
        text = replace_ignore_case(&text, &host, "<host>");
    }
    utf16_prefix(&text, 300).to_string()
}

/// The ffmpeg to run: the path from the settings if it is a file, otherwise the first one in PATH.
pub fn find_ffmpeg(configured: &str) -> Option<String> {
    let path = configured.trim().trim_matches('"');
    if !path.is_empty() {
        return Path::new(path).is_file().then(|| path.to_string());
    }
    crate::tool_process::find("ffmpeg").map(|p| p.to_string_lossy().into_owned())
}

/// Whether an address may be handed to ffmpeg: only RTSP, so a pasted file:, concat: or http: input is refused.
pub fn valid_url(url: &str) -> bool {
    crate::config::store::valid_camera_url(url)
}

/// Splits a byte stream of back-to-back JPEGs (ffmpeg's image2pipe output) into single images.
pub struct JpegSplitter<F: FnMut(Vec<u8>)> {
    on_frame: F,
    max: usize,
    buf: Vec<u8>,
}

fn find_pair(hay: &[u8], pair: [u8; 2]) -> Option<usize> {
    hay.windows(2).position(|w| w == pair)
}

impl<F: FnMut(Vec<u8>)> JpegSplitter<F> {
    pub fn new(on_frame: F, max: usize) -> Self {
        JpegSplitter {
            on_frame,
            max,
            buf: Vec::with_capacity(max.min(64 * 1024)),
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        // an image that never ends within the limit: start over (and a single read larger than the limit is skipped)
        if self.buf.len() + data.len() > self.max {
            self.buf.clear();
            if data.len() > self.max {
                return;
            }
        }
        self.buf.extend_from_slice(data);
        self.drain();
    }

    fn drain(&mut self) {
        loop {
            let Some(start) = find_pair(&self.buf, [0xFF, 0xD8]) else {
                // only a last 0xFF may be the start of the next image
                let keep = usize::from(self.buf.last() == Some(&0xFF));
                self.buf.drain(..self.buf.len() - keep);
                return;
            };
            if start > 0 {
                self.buf.drain(..start);
                continue;
            }
            // inside a JPEG, 0xFF in the data is followed by 0x00, so the first FF D9 after the start ends the image
            let Some(end) = find_pair(&self.buf[2..], [0xFF, 0xD9]) else {
                return;
            };
            let image: Vec<u8> = self.buf.drain(..end + 4).collect();
            (self.on_frame)(image);
        }
    }
}

/// The ffmpeg arguments for a camera: RTSP in, `fps` JPEG frames a second on stdout, at most 1280 wide.
pub fn ffmpeg_args(source: &CameraSource) -> Vec<String> {
    // rtsps is TLS, which needs TCP
    let tls = source
        .url
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("rtsps:"));
    let transport = if source.transport == "udp" && !tls {
        "udp"
    } else {
        "tcp"
    };
    let filter = format!(
        "fps={},scale='min(1280,iw)':-2",
        source.fps.clamp(1, MAX_FPS)
    );
    [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-rtsp_transport",
        transport,
        "-i",
        &source.url,
        "-an",
        "-sn",
        "-dn",
        "-vf",
        &filter,
        "-f",
        "image2pipe",
        "-c:v",
        "mjpeg",
        "-q:v",
        "6",
        "pipe:1",
    ]
    .map(String::from)
    .to_vec()
}

/// ffmpeg pulling one camera. Dropping it kills ffmpeg and anything it started.
pub struct FfmpegRun {
    pid: Option<u32>,
    shared: Arc<RunShared>,
}

#[derive(Default)]
struct RunShared {
    disposed: AtomicBool,
    stdout_done: AtomicBool,
    stderr_done: AtomicBool,
    reaped: AtomicBool,
    errors: Mutex<VecDeque<String>>,
}

impl RunShared {
    /// One error line: empty ones are skipped, and only the last three are kept.
    fn keep_line(&self, line: &mut Vec<u8>) {
        if line.is_empty() {
            return;
        }
        let text = utf16_prefix(&String::from_utf8_lossy(line), ERROR_LINE_CHARS).to_string();
        line.clear();
        let mut errors = lock(&self.errors);
        errors.push_back(text);
        while errors.len() > 3 {
            errors.pop_front();
        }
    }
}

/// Reads a pipe to its end in `size` pieces; a read error ends it too.
fn read_all(mut pipe: impl Read, size: usize, mut piece: impl FnMut(&[u8])) {
    let mut buf = vec![0u8; size];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => piece(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

impl FfmpegRun {
    pub fn start(
        ffmpeg: &str,
        source: &CameraSource,
        on_frame: OnFrame,
        jobs: &crate::job_object::JobObject,
    ) -> std::io::Result<FfmpegRun> {
        use std::process::{Command, Stdio};
        let mut command = Command::new(ffmpeg);
        command
            .args(ffmpeg_args(source))
            .current_dir(crate::app_paths::neutral_cwd())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0); // the tree is the group: one kill ends it all
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = command.spawn()?;
        // Windows: ffmpeg also ends when the agent is killed
        jobs.track_std(&child);
        let pid = child.id();
        let shared: Arc<RunShared> = Default::default();
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        // drained all the time: ffmpeg blocks once a full pipe isn't read
        let errors = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let mut line = Vec::new();
                if let Some(pipe) = stderr {
                    read_all(pipe, 8192, |piece| {
                        for &b in piece {
                            if b == b'\n' || b == b'\r' {
                                shared.keep_line(&mut line);
                            } else if line.len() < ERROR_LINE_CHARS * 4 {
                                line.push(b);
                            }
                        }
                    });
                }
                shared.keep_line(&mut line);
                shared.stderr_done.store(true, Ordering::SeqCst);
            })
        };
        {
            let shared = shared.clone();
            std::thread::spawn(move || {
                if let Some(pipe) = stdout {
                    let mut split = JpegSplitter::new(|jpeg| on_frame(jpeg), JPEG_MAX);
                    read_all(pipe, 64 * 1024, |piece| split.push(piece));
                }
                shared.stdout_done.store(true, Ordering::SeqCst);
                let _ = errors.join();
                let _ = child.wait();
                shared.reaped.store(true, Ordering::SeqCst);
            });
        }
        Ok(FfmpegRun {
            pid: Some(pid),
            shared,
        })
    }
}

impl CameraRun for FfmpegRun {
    /// Only once both pipes are read to the end, so the last error line is in before the run counts as over.
    fn has_ended(&self) -> bool {
        let s = &self.shared;
        s.disposed.load(Ordering::SeqCst)
            || (s.stdout_done.load(Ordering::SeqCst) && s.stderr_done.load(Ordering::SeqCst))
    }

    fn last_error(&self) -> Option<String> {
        let errors = lock(&self.shared.errors);
        (!errors.is_empty()).then(|| errors.iter().cloned().collect::<Vec<_>>().join(" | "))
    }
}

impl Drop for FfmpegRun {
    fn drop(&mut self) {
        if self.shared.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.shared.reaped.load(Ordering::SeqCst) {
            crate::tool_process::kill_tree(self.pid);
        }
    }
}

/// The launcher the agent uses: a real ffmpeg, in the agent's job (`jobs`).
pub fn real_launcher(jobs: Arc<crate::job_object::JobObject>) -> Launcher {
    Arc::new(move |ffmpeg, source, on_frame| {
        FfmpegRun::start(ffmpeg, source, on_frame, &jobs)
            .map(|run| Box::new(run) as Box<dyn CameraRun>)
            .map_err(|e| e.to_string())
    })
}

#[cfg(test)]
#[path = "camera_feeds_tests.rs"]
mod tests;
