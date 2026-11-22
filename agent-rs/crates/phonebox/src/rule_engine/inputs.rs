//! What the rule engine reads: the monitors (sessions, media, CPU, privacy, calendar, OBS, quotas, GitHub, batteries,
//! ping) and three OS probes (the process names, the idle time, the app in front), behind one narrow trait so the tests
//! hand it fixed values. The probes are cached as C# cached them.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::battery_monitor::DeviceBattery;
use crate::calendar::CalendarEvent;
use crate::github_monitor::GhSnapshot;
use crate::media_monitor::MediaSnapshot;
use crate::ping_monitor::PingSnapshot;
use crate::privacy_monitor::{PrivacySnapshot, PrivacySource};
use crate::quota::QuotaProvider;
use crate::session_monitor::AgentSession;

/// The inputs of the rule conditions; each answers what its monitor knows now (nothing by default).
pub trait RuleInputs: Send + Sync {
    fn sessions(&self) -> Vec<AgentSession> {
        vec![]
    }
    /// The system's media session (not the control deck's view).
    fn media(&self) -> Option<MediaSnapshot> {
        None
    }
    fn cpu(&self) -> Option<f64> {
        None
    }
    fn privacy(&self) -> Option<PrivacySnapshot> {
        None
    }
    /// The calendar event going on now whose title has one of the names (any event without names).
    fn calendar_event(&self, _names: &[String]) -> Option<CalendarEvent> {
        None
    }
    /// (live, streaming): OBS streams, or records without a pause.
    fn obs(&self) -> (bool, bool) {
        (false, false)
    }
    fn quotas(&self) -> Vec<QuotaProvider> {
        vec![]
    }
    fn github(&self) -> Option<GhSnapshot> {
        None
    }
    fn batteries(&self) -> Vec<DeviceBattery> {
        vec![]
    }
    fn ping(&self) -> Option<PingSnapshot> {
        None
    }
    /// The running processes' names as .NET gives them (no ".exe"; Linux's 15-character kernel name).
    fn processes(&self) -> Vec<String> {
        vec![]
    }
    fn idle_seconds(&self) -> f64 {
        0.0
    }
    /// The app that owns the foreground window, or None when it cannot be told.
    fn front_app(&self) -> Option<String> {
        None
    }
}

/// No monitors: every condition reads nothing.
pub struct NoInputs;

impl RuleInputs for NoInputs {}

/// The monitors the agent runs.
pub struct LiveInputs {
    pub sessions: Arc<crate::session_monitor::SessionMonitor>,
    pub media: Arc<dyn crate::media_monitor::SystemMedia>,
    pub system: Arc<crate::system_monitor::SystemMonitor>,
    pub privacy: Arc<crate::privacy_monitor::PrivacyMonitor>,
    pub calendar: Arc<crate::calendar::CalendarMonitor>,
    pub obs: Arc<crate::obs_monitor::ObsMonitor>,
    pub quotas: Arc<crate::quota::QuotaMonitor>,
    pub github: Arc<crate::github_monitor::GitHubMonitor>,
    pub batteries: Arc<crate::battery_monitor::BatteryMonitor>,
    pub ping: Arc<crate::ping_monitor::PingMonitor>,
    caches: Mutex<Caches>,
}

#[derive(Default)]
struct Caches {
    processes: Option<(Instant, Vec<String>)>,
    idle: Option<(Instant, f64)>,
    front: Option<(Instant, Option<String>)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

const PROCESSES_FOR: Duration = Duration::from_millis(3000);
const IDLE_FOR: Duration = Duration::from_millis(5000);
const FRONT_FOR: Duration = Duration::from_millis(1500);

impl LiveInputs {
    /// The agent's monitors, from its state.
    pub fn of(state: &crate::server::AppState) -> LiveInputs {
        LiveInputs {
            sessions: state.sessions.clone(),
            media: state.media.clone(),
            system: state.system.clone(),
            privacy: state.privacy.clone(),
            calendar: state.calendar.clone(),
            obs: state.obs.clone(),
            quotas: state.quotas.clone(),
            github: state.github.clone(),
            batteries: state.batteries.clone(),
            ping: state.ping.clone(),
            caches: Default::default(),
        }
    }
}

impl RuleInputs for LiveInputs {
    fn sessions(&self) -> Vec<AgentSession> {
        self.sessions.snapshot()
    }
    fn media(&self) -> Option<MediaSnapshot> {
        self.media.snapshot()
    }
    fn cpu(&self) -> Option<f64> {
        self.system.snapshot().and_then(|s| s.cpu)
    }
    fn privacy(&self) -> Option<PrivacySnapshot> {
        PrivacySource::snapshot(&*self.privacy)
    }
    fn calendar_event(&self, names: &[String]) -> Option<CalendarEvent> {
        self.calendar.current(names)
    }
    fn obs(&self) -> (bool, bool) {
        (self.obs.live(), self.obs.snapshot().streaming)
    }
    fn quotas(&self) -> Vec<QuotaProvider> {
        self.quotas.snapshot(crate::server::now_ms())
    }
    fn github(&self) -> Option<GhSnapshot> {
        self.github.snapshot()
    }
    fn batteries(&self) -> Vec<DeviceBattery> {
        self.batteries.snapshot()
    }
    fn ping(&self) -> Option<PingSnapshot> {
        self.ping.snapshot()
    }

    /// Cached 3 s.
    fn processes(&self) -> Vec<String> {
        if let Some((at, names)) = &lock(&self.caches).processes
            && at.elapsed() < PROCESSES_FOR
        {
            return names.clone();
        }
        let names: Vec<String> = crate::process_monitor::list_processes(None)
            .into_iter()
            .map(|p| p.name)
            .collect();
        lock(&self.caches).processes = Some((Instant::now(), names.clone()));
        names
    }

    /// Cached 5 s.
    fn idle_seconds(&self) -> f64 {
        if let Some((at, idle)) = lock(&self.caches).idle
            && at.elapsed() < IDLE_FOR
        {
            return idle;
        }
        let idle = crate::idle::seconds();
        lock(&self.caches).idle = Some((Instant::now(), idle));
        idle
    }

    /// Cached 1.5 s, timed from the end of the probe: osascript or xdotool can take up to 2 s, and every app-in-front
    /// rule in the same pass reuses that answer instead of starting its own probe.
    fn front_app(&self) -> Option<String> {
        if let Some((at, front)) = &lock(&self.caches).front
            && at.elapsed() < FRONT_FOR
        {
            return front.clone();
        }
        let front = front_app_os();
        lock(&self.caches).front = Some((Instant::now(), front.clone()));
        front
    }
}

#[cfg(windows)]
fn front_app_os() -> Option<String> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let mut pid = 0u32;
    // SAFETY: plain queries of the foreground window and its process id
    let thread = unsafe {
        let window = GetForegroundWindow();
        if window.is_invalid() {
            return None;
        }
        GetWindowThreadProcessId(window, Some(&mut pid as *mut u32))
    };
    if thread == 0 {
        return None;
    }
    crate::privacy_monitor::process_name(pid)
}

/// macOS: System Events, the same Accessibility permission as sending keys. Linux: xdotool on X11 only; under Wayland
/// there is no desktop-wide frontmost-app query, so the rule simply stays off.
#[cfg(unix)]
fn front_app_os() -> Option<String> {
    let out = if cfg!(target_os = "macos") {
        crate::unix::sh::run(
            "osascript",
            &[
                "-e",
                "tell application \"System Events\" to get name of first process whose frontmost is true",
            ],
        )
    } else if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        crate::unix::sh::run("xdotool", &["getactivewindow", "getwindowclassname"])
    } else {
        None
    };
    out.map(|o| o.trim().to_string()).filter(|o| !o.is_empty())
}
