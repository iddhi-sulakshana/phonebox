//! `ControlDeck.cs`, the core: the audio cache and the phone's grouping of application sessions, the snapshot the
//! controls page polls, the commands it sends (devices, sessions, default roles, push to talk, panic, ducking, power and
//! platform actions), audio profiles and their capture.
//!
//! C# guarded its state with reentrant locks; here everything `_state` covered is one [`DeckState`] behind one mutex
//! with `_locked` helpers, and `_pttGate` is the mutex around the pending panic mutes. Native calls block, and some
//! locks are held across them as C# did, so they run on blocking threads, never on the async workers.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::{Value, json};

use super::leases::{MicrophoneLeases, PowerConfirmations, nonce};
use super::models::*;
use super::monotonic_ms;
use super::tasks::ControlTasks;
use crate::config::store::ConfigStore;
use crate::config::{AgentConfig, DeckAction, DeckProfile, Shortcut};
use crate::dotnet_text::{lower_invariant, utf16_len};
use crate::server::Stop;

/// How long an audio reading is reused.
pub const AUDIO_CACHE_MS: i64 = 400;
/// How often monitors, power plans and MIDI ports are looked for again.
pub const HARDWARE_MS: i64 = 30_000;

const CONTROL_FAILED: &str = "Control failed; check the computer's integration settings";
const UNAVAILABLE: &str = "This control is unavailable on the selected device";
const UNKNOWN_CONTROL: &str = "Choose a configured control in desktop settings";

/// The caller's stop signal ended the request: the route answers nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// One `POST /api/controls/command/{kind}` request.
#[derive(Debug, Clone, Default)]
pub struct DeckCommand {
    pub kind: String,
    pub id: Option<String>,
    pub op: Option<String>,
    pub value: Option<i32>,
    pub muted: Option<bool>,
    pub lease: Option<String>,
}

/// What the deck is given beside its config and native seam (the C# test constructor's optional arguments).
#[derive(Clone, Default)]
pub struct DeckOptions {
    /// Unix milliseconds for the radio's elapsed time and the sleep deadlines (default: the system clock).
    pub now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
}

/// Everything C#'s reentrant `_state` lock covered.
#[derive(Default)]
pub(crate) struct DeckState {
    /// The last audio reading and when it was taken (monotonic ms).
    audio: Option<(DeckAudio, i64)>,
    /// Ducking: other sessions go down to this percentage while a microphone hears something.
    pub(crate) duck_percent: Option<i32>,
    /// Sleep timers: player id → when it pauses (Unix ms).
    pub(crate) sleep: IndexMap<String, i64>,
}

/// Monitors, power plans and MIDI ports: slow to find, looked for in the background.
#[derive(Default, Clone)]
struct Hardware {
    monitors: Vec<DeckMonitor>,
    plans: Vec<DeckItem>,
    midi: Vec<DeckItem>,
}

#[derive(Default)]
struct HardwareJob {
    at: Option<i64>,
    job: Option<std::thread::JoinHandle<()>>,
}

/// A command's outcome before the common ending: an answer as it is, or whether the control took (the audio cache is
/// dropped, and a refusal gets the generic text).
enum Step {
    Answer(DeckResult),
    Done(bool),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn invalidate(state: &Mutex<DeckState>) {
    lock(state).audio = None;
}

pub struct ControlDeck {
    pub(crate) cfg: Arc<ConfigStore>,
    native: Arc<dyn DeckNative>,
    state: Arc<Mutex<DeckState>>,
    /// `_pttGate`, around the microphones panic could not mute yet (`_panicPending`).
    ptt_gate: Mutex<HashSet<String>>,
    ptt: MicrophoneLeases,
    power: PowerConfirmations,
    /// One profile at a time.
    profiles: tokio::sync::Mutex<()>,
    hardware: Arc<Mutex<Hardware>>,
    hardware_job: Mutex<HardwareJob>,
    tasks: Arc<ControlTasks>,
    disposed: AtomicBool,
}

impl ControlDeck {
    pub fn new(
        cfg: Arc<ConfigStore>,
        native: Arc<dyn DeckNative>,
        _options: DeckOptions,
    ) -> Arc<ControlDeck> {
        let state: Arc<Mutex<DeckState>> = Default::default();
        // a hold reads the microphone as it is now, never the cached reading
        let read = {
            let native = native.clone();
            move |id: &str| {
                native
                    .audio()
                    .devices
                    .into_iter()
                    .find(|d| d.id == id && d.direction == "input")
                    .map(|d| d.muted)
            }
        };
        let write = {
            let (native, state) = (native.clone(), state.clone());
            move |id: &str, muted: bool| {
                let ok = native.device(id, None, Some(muted), None);
                invalidate(&state);
                ok
            }
        };
        Arc::new(ControlDeck {
            tasks: Arc::new(ControlTasks::new(cfg.clone())),
            cfg,
            native,
            ptt: MicrophoneLeases::new(read, write, monotonic_ms),
            state,
            ptt_gate: Mutex::default(),
            power: PowerConfirmations::new(monotonic_ms),
            profiles: tokio::sync::Mutex::new(()),
            hardware: Default::default(),
            hardware_job: Default::default(),
            disposed: AtomicBool::new(false),
        })
    }

    /// The programs of the task and server actions (`GET /api/controls/task-output/{id}` reads them).
    pub fn tasks(&self) -> Arc<ControlTasks> {
        self.tasks.clone()
    }

    /// Runs blocking deck work on a blocking thread; None when it panicked.
    async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&ControlDeck) -> T + Send + 'static,
    ) -> Option<T> {
        let deck = self.clone();
        tokio::task::spawn_blocking(move || work(&deck)).await.ok()
    }

    /// The audio reading, reused for [`AUDIO_CACHE_MS`]. Blocks.
    fn audio(&self) -> DeckAudio {
        let mut state = lock(&self.state);
        self.audio_locked(&mut state)
    }

    /// The native call runs with the state locked, as C# did: a reading is never taken twice at once.
    pub(crate) fn audio_locked(&self, state: &mut DeckState) -> DeckAudio {
        let now = monotonic_ms();
        if let Some((audio, at)) = &state.audio
            && now - at < AUDIO_CACHE_MS
        {
            return audio.clone();
        }
        let audio = self.native.audio();
        state.audio = Some((audio.clone(), now));
        audio
    }

    fn invalidate(&self) {
        invalidate(&self.state);
    }

    /// Mutes a device; the cache is dropped either way.
    fn try_mute(&self, id: &str) -> bool {
        let ok = self.native.device(id, None, Some(true), None);
        self.invalidate();
        ok
    }

    /// Retries the mutes panic could not apply (with `_pttGate` held).
    pub(crate) fn retry_panic_mutes_locked(&self, pending: &mut HashSet<String>) {
        pending.retain(|id| !self.try_mute(id));
    }

    /// `GET /api/controls/state`. None when building it panicked (the route answers 500).
    pub async fn snapshot(self: &Arc<Self>) -> Option<Value> {
        self.refresh_hardware();
        let (media, media_error) = match self.native.media().await {
            Ok(media) => (media, None),
            Err(_) => (Vec::new(), Some("Media sessions are unavailable")),
        };
        let deck = self.clone();
        let parts = self
            .blocking(move |d| {
                let c = d.cfg.get();
                let (ducking, sleep) = {
                    let state = lock(&d.state);
                    let sleep: Vec<Value> = state
                        .sleep
                        .iter()
                        .map(|(id, at)| json!({ "id": id, "at": at }))
                        .collect();
                    (state.duck_percent.is_some(), sleep)
                };
                let hardware = lock(&d.hardware).clone();
                let audio = group_for_phone(d.audio());
                let actions: Vec<DeckItem> = c
                    .controls
                    .actions
                    .iter()
                    .filter(|a| a.enabled)
                    .map(|a| d.describe(&c, a, &hardware.midi))
                    .collect();
                let shortcuts: Vec<DeckItem> = c.shortcuts.iter().map(describe_shortcut).collect();
                (c, audio, hardware, actions, shortcuts, ducking, sleep)
            })
            .await?;
        let (c, audio, hardware, actions, shortcuts, ducking, sleep) = parts;
        let loading = lock(&deck.hardware_job)
            .job
            .as_ref()
            .is_some_and(|job| !job.is_finished());
        let profiles: Vec<DeckItem> = c
            .controls
            .profiles
            .iter()
            .map(|p| item(&p.id, &p.label, "profile", None))
            .collect();
        Some(json!({
            "platform": crate::app_paths::os(),
            "audio": audio,
            "media": media,
            "mediaError": media_error,
            "monitors": hardware.monitors,
            "powerPlans": hardware.plans,
            "midi": hardware.midi,
            "hardwareLoading": loading,
            "actions": actions,
            "shortcuts": shortcuts,
            "profiles": profiles,
            "allowListening": c.controls.allow_listening,
            "allowPower": c.controls.allow_power,
            "ducking": ducking,
            "sleep": sleep,
        }))
    }

    /// Looks for monitors, power plans and MIDI ports in the background, at most every [`HARDWARE_MS`].
    fn refresh_hardware(&self) {
        let mut job = lock(&self.hardware_job);
        let now = monotonic_ms();
        if job.job.as_ref().is_some_and(|j| !j.is_finished())
            || job.at.is_some_and(|at| now - at < HARDWARE_MS)
        {
            return;
        }
        job.at = Some(now);
        let (native, hardware) = (self.native.clone(), self.hardware.clone());
        job.job = std::thread::Builder::new()
            .name("deck-hardware".into())
            .spawn(move || {
                let found = Hardware {
                    monitors: native.monitors(),
                    plans: native.power_plans(),
                    midi: native.midi_ports(),
                };
                *lock(&hardware) = found;
            })
            .ok();
    }

    /// What the phone shows for a configured control. Blocks (native states, a tool lookup).
    fn describe(&self, c: &AgentConfig, a: &DeckAction, midi: &[DeckItem]) -> DeckItem {
        let mut state =
            matches!(a.kind.as_str(), "task" | "server").then(|| self.tasks.state(&a.id));
        let mut available = true;
        let mut set = |ok: bool, text: &str| {
            available = ok;
            Some(text.to_string())
        };
        match a.kind.as_str() {
            "service" => {
                let s = self
                    .native
                    .action_state(a)
                    .unwrap_or_else(|| "Service unavailable".into());
                available = s != "Service unavailable";
                state = Some(s);
            }
            "sound" | "radio" => {
                state = self.native.action_state(a);
                // None: no player here (Linux or macOS without ffplay, mpv or vlc)
                available = cfg!(windows) || state.is_some();
                if state.is_none() {
                    state = Some(
                        if cfg!(windows) {
                            "Playback unavailable"
                        } else {
                            "Install ffplay, mpv, or vlc"
                        }
                        .into(),
                    );
                }
            }
            "workflow" if crate::tool_process::find("gh").is_none() => {
                state = set(false, "Install GitHub CLI")
            }
            "playlist"
                if blank(c.controls.spotify_client_id.as_deref())
                    && blank(c.controls.spotify_token.as_deref()) =>
            {
                state = set(false, "Connect Spotify in desktop settings")
            }
            "midi" => {
                let port = a.device.as_deref().and_then(parse_int);
                if port.is_none_or(|p| !midi.iter().any(|m| m.id == p.to_string())) {
                    state = set(false, "MIDI port unavailable");
                }
            }
            "app" | "project" | "shortcut" | "focus" => {
                state = match c.shortcuts.iter().find(|s| s.id == a.target) {
                    None => set(false, "Shortcut is missing"),
                    Some(s) => set(
                        true,
                        if shortcut_running(s) {
                            "Running"
                        } else {
                            "Ready"
                        },
                    ),
                };
            }
            "mixer" | "mixer-mute" => state = set(false, "Mixer is not connected"),
            "audio-route"
                if c.controls.mixer_executable.as_deref().is_none_or(|exe| {
                    exe.trim().is_empty() || !std::path::Path::new(exe).is_file()
                }) =>
            {
                state = set(false, "Configure SoundVolumeView")
            }
            _ => {}
        }
        DeckItem {
            id: a.id.clone(),
            name: a.label.clone(),
            kind: a.kind.clone(),
            state,
            available,
            rev: None,
        }
    }

    /// `POST /api/controls/capture`: a draft for the desktop editor (saving still checks the config version). Err when
    /// audio is unavailable. Blocks.
    pub fn capture(&self) -> Option<CapturedProfile> {
        let audio = self.native.audio();
        if !audio.available {
            return None;
        }
        let mut levels: Vec<DeckLevel> = audio
            .devices
            .iter()
            .filter(|d| d.can_control)
            .map(|d| level("device", &d.id, d.volume, d.muted))
            .collect();
        let mut applications: IndexMap<String, &DeckSession> = IndexMap::new();
        for s in &audio.sessions {
            applications.entry(application_key(s)).or_insert(s);
        }
        levels.extend(
            applications
                .into_iter()
                .map(|(key, s)| level("application", &key, s.volume, s.muted)),
        );
        Some(CapturedProfile {
            id: format!("profile-{}", &nonce()[..12]),
            label: "Audio profile".into(),
            levels,
            actions: vec![],
            output: None,
            input: None,
            defaults: audio.defaults.unwrap_or_default(),
        })
    }

    /// `POST /api/controls/command/{kind}`. Err only when `stop` ended the request.
    pub async fn act(
        self: &Arc<Self>,
        command: DeckCommand,
        stop: &Stop,
    ) -> Result<DeckResult, Cancelled> {
        let DeckCommand {
            kind,
            id,
            op,
            value,
            muted,
            lease,
        } = command;
        let (id, op) = (id.unwrap_or_default(), op.unwrap_or_default());
        if utf16_len(&id) > 2048
            || utf16_len(&op) > 64
            || lease.as_deref().is_some_and(|l| utf16_len(l) > 64)
        {
            return Ok(DeckResult::fail("Invalid control request"));
        }
        let allow_power = || self.cfg.get().controls.allow_power;
        let step = match kind.as_str() {
            "device" => {
                self.blocking(move |d| d.act_device(&id, &op, value, muted))
                    .await
            }
            "session" => {
                self.blocking(move |d| d.act_session(&id, value, muted))
                    .await
            }
            "default" => {
                let role = match op.as_str() {
                    "" | "all" => None,
                    "media" => Some(1),
                    "communications" => Some(2),
                    "console" => Some(0),
                    _ => return Ok(DeckResult::fail("Unknown audio role")),
                };
                self.blocking(move |d| Step::Done(d.native.default_device(&id, role)))
                    .await
            }
            "ptt" => {
                self.blocking(move |d| d.act_ptt(&id, &op, lease.as_deref()))
                    .await
            }
            "profile" => return self.profile(&id, stop).await,
            "duck" => {
                lock(&self.state).duck_percent = if op == "off" {
                    None
                } else {
                    Some(value.unwrap_or(25).clamp(0, 100))
                };
                Some(Step::Done(true))
            }
            "panic" => self.blocking(|d| d.act_panic()).await,
            "action" => return self.action(&id, &op, value, stop).await,
            "brightness" => match value {
                Some(v @ 0..=100) => {
                    self.blocking(move |d| Step::Done(d.native.brightness(&id, v)))
                        .await
                }
                _ => Some(Step::Done(false)),
            },
            "desktop" => match value {
                Some(v @ (-1 | 1)) => Some(Step::Done(
                    self.platform("desktop", "", None, v, stop).await?,
                )),
                _ => Some(Step::Done(false)),
            },
            "power-plan" => {
                let target = id.clone();
                match self
                    .blocking(move |d| d.native.power_plans().iter().any(|p| p.id == target))
                    .await
                {
                    None => None,
                    Some(false) => Some(Step::Done(false)),
                    Some(true) => Some(Step::Done(
                        self.platform("power-plan", &id, None, 0, stop).await?,
                    )),
                }
            }
            "power-arm" => {
                if !allow_power() || !matches!(id.as_str(), "sleep" | "restart" | "shutdown") {
                    return Ok(DeckResult::fail(
                        "Enable power controls in desktop settings",
                    ));
                }
                match self.power.arm(&id) {
                    Ok(nonce) => Some(Step::Answer(DeckResult {
                        ok: true,
                        error: None,
                        lease: Some(nonce),
                    })),
                    Err(_) => None,
                }
            }
            "power" => {
                if !allow_power() || !lease.as_deref().is_some_and(|l| self.power.consume(l, &id)) {
                    return Ok(DeckResult::fail("Hold the power button to confirm"));
                }
                Some(Step::Done(
                    self.platform("power", &id, None, 0, stop).await?,
                ))
            }
            "lock" | "monitor-off" => {
                let shortcut = Shortcut {
                    cmd: format!("builtin:{kind}"),
                    label: kind.clone(),
                    ..Shortcut::default()
                };
                self.blocking(move |_| {
                    Step::Done(crate::shortcuts::execute(&shortcut, None, 0, None, None).is_none())
                })
                .await
            }
            _ => return Ok(DeckResult::fail("Unknown control action")),
        };
        Ok(match step {
            None => DeckResult::fail(CONTROL_FAILED),
            Some(Step::Answer(answer)) => answer,
            Some(Step::Done(ok)) => {
                self.invalidate();
                if ok {
                    DeckResult::ok()
                } else {
                    DeckResult::fail(UNAVAILABLE)
                }
            }
        })
    }

    /// A platform action, ended early by `stop`.
    async fn platform(
        &self,
        kind: &str,
        target: &str,
        argument: Option<&str>,
        value: i32,
        stop: &Stop,
    ) -> Result<bool, Cancelled> {
        tokio::select! {
            biased;
            _ = stop.stopped() => Err(Cancelled),
            ok = self.native.platform_action(kind, target, argument, value) => Ok(ok),
        }
    }

    fn act_device(&self, id: &str, op: &str, value: Option<i32>, muted: Option<bool>) -> Step {
        if value.is_none() && muted.is_none() {
            return Step::Answer(DeckResult::fail("Choose a volume or mute state"));
        }
        if !self
            .audio()
            .devices
            .iter()
            .any(|d| d.id == id && d.can_control)
            || value.is_some_and(|v| !(0..=100).contains(&v))
        {
            return Step::Answer(DeckResult::fail("Audio device is unavailable"));
        }
        let channel = match op {
            "left" => Some(0),
            "right" => Some(1),
            _ => None,
        };
        if muted.is_some() {
            // the user's own mute choice replaces a pending panic mute, but only once it took effect: a failed request
            // must leave the panic mute pending (retried, and push-to-talk refused)
            let mut pending = lock(&self.ptt_gate);
            let ok = self.native.device(id, value, muted, channel);
            if ok {
                pending.remove(id);
            }
            return Step::Done(ok);
        }
        Step::Done(self.native.device(id, value, muted, channel))
    }

    fn act_session(&self, id: &str, value: Option<i32>, muted: Option<bool>) -> Step {
        if value.is_none() && muted.is_none() {
            return Step::Answer(DeckResult::fail("Choose a volume or mute state"));
        }
        if value.is_some_and(|v| !(0..=100).contains(&v)) {
            return Step::Answer(DeckResult::fail("Choose a valid volume"));
        }
        let ended = || Step::Answer(DeckResult::fail("Application audio session ended"));
        if let Some(name) = id.strip_prefix("application:") {
            let sessions: Vec<DeckSession> = self
                .audio()
                .sessions
                .into_iter()
                .filter(|s| matches_application(s, name))
                .collect();
            if sessions.is_empty() {
                return ended();
            }
            // every copy is written, even after one failed
            let mut ok = true;
            for s in &sessions {
                ok = self.native.session(&s.id, value, muted) && ok;
            }
            return Step::Done(ok);
        }
        if !self.audio().sessions.iter().any(|s| s.id == id) {
            return ended();
        }
        Step::Done(self.native.session(id, value, muted))
    }

    fn act_ptt(&self, id: &str, op: &str, lease: Option<&str>) -> Step {
        let mut pending = lock(&self.ptt_gate);
        self.retry_panic_mutes_locked(&mut pending);
        if op == "begin" {
            // a microphone panic could not mute stays closed to push-to-talk until the mute lands
            let token = if pending.contains(id) {
                None
            } else {
                self.ptt.begin(id)
            };
            return Step::Answer(DeckResult {
                ok: token.is_some(),
                error: token.is_none().then(|| "Microphone is unavailable".into()),
                lease: token,
            });
        }
        let ok = lease.is_some_and(|lease| match op {
            "renew" => self.ptt.renew(lease),
            "end" => self.ptt.end(lease),
            _ => false,
        });
        Step::Done(ok)
    }

    fn act_panic(&self) -> Step {
        let audio = self.native.audio();
        let controllable: Vec<&DeckDevice> =
            audio.devices.iter().filter(|d| d.can_control).collect();
        if !audio.available || controllable.is_empty() {
            return Step::Answer(DeckResult::fail("Audio is unavailable"));
        }
        // held across the mutes: a hold that began in between would save the still-open state as its original and
        // reopen the microphone when it ends
        let mut pending = lock(&self.ptt_gate);
        self.ptt.abandon_all();
        let mut ok = true;
        for d in controllable {
            let muted = self.try_mute(&d.id);
            // the holds no longer restore anything, so a microphone left open keeps a pending mute
            if muted {
                pending.remove(&d.id);
            } else if d.direction == "input" {
                pending.insert(d.id.clone());
            }
            ok &= muted;
        }
        Step::Done(ok)
    }

    async fn profile(self: &Arc<Self>, id: &str, stop: &Stop) -> Result<DeckResult, Cancelled> {
        let c = self.cfg.get();
        let Some(profile) = c.controls.profiles.iter().find(|p| p.id == id).cloned() else {
            return Ok(DeckResult::fail("No such profile"));
        };
        let _one = tokio::select! {
            biased;
            _ = stop.stopped() => return Err(Cancelled),
            one = self.profiles.lock() => one,
        };
        let levels = profile.clone();
        match self.blocking(move |d| d.apply_profile_audio(&levels)).await {
            None => return Ok(DeckResult::fail(CONTROL_FAILED)),
            Some(Err(answer)) => return Ok(answer),
            Some(Ok(())) => {}
        }
        let configured = self.cfg.get();
        for action in &profile.actions {
            if !action.starts_with("shortcut:")
                && configured
                    .controls
                    .actions
                    .iter()
                    .find(|a| a.id == *action)
                    .is_some_and(|a| !a.enabled)
            {
                continue;
            }
            let result = Box::pin(self.action(action, "", None, stop)).await?;
            if !result.ok {
                return Ok(result);
            }
        }
        self.invalidate();
        Ok(DeckResult::ok())
    }

    /// Checks every level, device and role of a profile before writing any, then writes them in order, stopping at
    /// the first failure. Blocks.
    fn apply_profile_audio(&self, profile: &DeckProfile) -> Result<(), DeckResult> {
        let fail = |text: &str| Err(DeckResult::fail(text));
        let before = self.audio();
        for level in &profile.levels {
            let id = level.id.as_deref();
            let missing = match level.kind.as_deref() {
                Some("device") => !before
                    .devices
                    .iter()
                    .any(|d| Some(d.id.as_str()) == id && d.can_control),
                Some("session") => !before.sessions.iter().any(|s| Some(s.id.as_str()) == id),
                Some("application") => false,
                _ => true,
            };
            if missing || !(0..=100).contains(&level.volume) {
                return fail(
                    "A profile's audio target is unavailable; refresh the profile in desktop settings",
                );
            }
        }
        let lacks = |selected: &Option<String>, direction: &str| {
            selected.as_deref().is_some_and(|id| {
                !id.is_empty()
                    && !before
                        .devices
                        .iter()
                        .any(|d| d.id == id && d.direction == direction)
            })
        };
        if lacks(&profile.output, "output") || lacks(&profile.input, "input") {
            return fail("A profile's selected device is unavailable");
        }
        for (key, value) in &profile.defaults {
            if audio_role(key).is_none()
                || !before
                    .devices
                    .iter()
                    .any(|d| d.id == *value && key.starts_with(&format!("{}:", d.direction)))
            {
                return fail("A profile's default device is unavailable");
            }
        }
        for level in &profile.levels {
            let (volume, muted) = (Some(level.volume), Some(level.muted));
            let id = level.id.as_deref().unwrap_or_default();
            match level.kind.as_deref() {
                Some("application") => {
                    if level.id.is_none() {
                        continue;
                    }
                    for s in before
                        .sessions
                        .iter()
                        .filter(|s| matches_application(s, id))
                    {
                        if !self.native.session(&s.id, volume, muted) {
                            return fail("An application level could not be restored");
                        }
                    }
                }
                Some("device") if !self.native.device(id, volume, muted, None) => {
                    return fail("Profile could not be applied completely");
                }
                Some("session") if !self.native.session(id, volume, muted) => {
                    return fail("Profile could not be applied completely");
                }
                _ => {}
            }
        }
        for (key, value) in &profile.defaults {
            if !self.native.default_device(value, audio_role(key)) {
                return fail("A profile's audio role could not be restored");
            }
        }
        let selected = |s: &Option<String>| s.clone().filter(|s| !s.is_empty());
        if let Some(output) = selected(&profile.output)
            && !self.native.default_device(&output, None)
        {
            return fail("Profile output is unavailable");
        }
        if let Some(input) = selected(&profile.input)
            && !self.native.default_device(&input, None)
        {
            return fail("Profile microphone is unavailable");
        }
        Ok(())
    }

    /// A configured control the phone names (or a profile runs).
    async fn action(
        self: &Arc<Self>,
        id: &str,
        op: &str,
        value: Option<i32>,
        stop: &Stop,
    ) -> Result<DeckResult, Cancelled> {
        let c = self.cfg.get();
        let Some(action) = c.controls.actions.iter().find(|a| a.id == id) else {
            return Ok(DeckResult::fail(UNKNOWN_CONTROL));
        };
        if !action.enabled
            && !(op == "stop"
                && matches!(action.kind.as_str(), "task" | "server" | "sound" | "radio"))
        {
            return Ok(DeckResult::fail("Control is disabled"));
        }
        let ok = self
            .platform(
                &action.kind,
                &action.target,
                action.argument.as_deref(),
                value.unwrap_or(action.value),
                stop,
            )
            .await?;
        Ok(if ok {
            DeckResult::ok()
        } else {
            DeckResult::fail("Integration is unavailable or requires setup on the computer")
        })
    }

    /// The agent is stopping: every hold ends (microphones restored), task programs end (at most 8 s), and the native
    /// side lets go. Once; blocks.
    pub fn dispose(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.ptt.sweep(true);
        self.tasks.stop_all(Duration::from_secs(8));
        self.native.dispose();
    }
}

fn item(id: &str, name: &str, kind: &str, state: Option<String>) -> DeckItem {
    DeckItem {
        id: id.into(),
        name: name.into(),
        kind: kind.into(),
        state,
        available: true,
        rev: None,
    }
}

fn level(kind: &str, id: &str, volume: i32, muted: bool) -> DeckLevel {
    DeckLevel {
        kind: kind.into(),
        id: id.into(),
        volume: Some(volume),
        muted: Some(muted),
    }
}

/// A shortcut as the phone lists it (it prefixes the id with `shortcut:`).
fn describe_shortcut(s: &Shortcut) -> DeckItem {
    let kind = match s.cmd.as_str() {
        "builtin:keys" => "keys",
        "builtin:text" => "text",
        "builtin:macro" => "macro",
        cmd if cmd.starts_with("builtin:") => "shortcut",
        _ => "app",
    };
    item(
        &s.id,
        &s.label,
        kind,
        shortcut_running(s).then(|| "Running".into()),
    )
}

/// Whether the shortcut's program shows a window. Only Windows can say (.NET's `MainWindowHandle` is always 0
/// elsewhere); the Windows check comes with the actions.
fn shortcut_running(_: &Shortcut) -> bool {
    false
}

/// The sessions the phone shows: active ones (and the system's, pid 0), one per application. A group shows its
/// session on the default output (else its first), as `application:<key>`, with the group's highest peak.
pub fn group_for_phone(mut audio: DeckAudio) -> DeckAudio {
    if !audio.available || audio.sessions.is_empty() {
        return audio;
    }
    let default_output = audio
        .defaults
        .as_ref()
        .and_then(|d| d.get("output:1"))
        .cloned();
    let mut groups: IndexMap<String, Vec<DeckSession>> = IndexMap::new();
    for s in audio.sessions.drain(..).filter(|s| s.active || s.pid == 0) {
        groups.entry(application_key(&s)).or_default().push(s);
    }
    audio.sessions = groups
        .into_iter()
        .map(|(key, group)| {
            let peak = group
                .iter()
                .map(|s| s.peak)
                .fold(f64::NEG_INFINITY, f64::max);
            let mut shown = group
                .iter()
                .find(|s| Some(&s.device_id) == default_output.as_ref())
                .unwrap_or(&group[0])
                .clone();
            shown.id = format!("application:{key}");
            shown.peak = peak;
            shown
        })
        .collect();
    audio
}

/// An application's key: its lower-cased name, or the session's own id for a nameless one (and for Windows' generic
/// "Application" when no process name says which).
pub fn application_key(s: &DeckSession) -> String {
    let name = s.name.trim();
    if name.is_empty() || (eq_ci(name, "Application") && blank(s.process_name.as_deref())) {
        return format!("session:{}", s.id);
    }
    lower_invariant(name)
}

/// Whether a session belongs to the application the phone named (by key, name or process name, in any case).
pub fn matches_application(s: &DeckSession, value: &str) -> bool {
    eq_ci(&application_key(s), value)
        || eq_ci(&s.name, value)
        || s.process_name.as_deref().is_some_and(|p| eq_ci(p, value))
}

/// A profile's role key: `output:N` or `input:N` with N 0 to 2.
fn audio_role(text: &str) -> Option<i32> {
    let pieces: Vec<&str> = text.split(':').collect();
    if pieces.len() != 2 || !matches!(pieces[0], "input" | "output") {
        return None;
    }
    parse_int(pieces[1]).filter(|role| (0..=2).contains(role))
}

/// `int.TryParse`: white space around and a sign allowed.
fn parse_int(text: &str) -> Option<i32> {
    text.trim_matches(|c| matches!(c, '\t'..='\r' | ' '))
        .parse()
        .ok()
}

/// `OrdinalIgnoreCase`.
fn eq_ci(a: &str, b: &str) -> bool {
    a.to_uppercase() == b.to_uppercase()
}

/// `string.IsNullOrWhiteSpace`.
fn blank(s: Option<&str>) -> bool {
    s.is_none_or(|s| s.trim().is_empty())
}

/// The native side until the platform's own is in place: no audio, no players, nothing to act on.
pub struct NoNative;

impl DeckNative for NoNative {
    fn audio(&self) -> DeckAudio {
        DeckAudio {
            available: false,
            error: Some("Audio backend is unavailable".into()),
            devices: vec![],
            sessions: vec![],
            defaults: None,
        }
    }
    fn device(&self, _: &str, _: Option<i32>, _: Option<bool>, _: Option<i32>) -> bool {
        false
    }
    fn session(&self, _: &str, _: Option<i32>, _: Option<bool>) -> bool {
        false
    }
    fn default_device(&self, _: &str, _: Option<i32>) -> bool {
        false
    }
    fn monitors(&self) -> Vec<DeckMonitor> {
        vec![]
    }
    fn brightness(&self, _: &str, _: i32) -> bool {
        false
    }
    fn power_plans(&self) -> Vec<DeckItem> {
        vec![]
    }
    fn platform_action<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<&'a str>,
        _: i32,
    ) -> futures_util::future::BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn media(&self) -> futures_util::future::BoxFuture<'_, Result<Vec<DeckMedia>, String>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn media_action<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: i64,
    ) -> futures_util::future::BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn play(&self, _: &str, _: &str, _: Option<&str>, _: i32) -> Result<(), String> {
        Err("Playback is unavailable".into())
    }
    fn stop_sound(&self, _: Option<&str>) {}
    fn midi_ports(&self) -> Vec<DeckItem> {
        vec![]
    }
    fn midi(&self, _: i32, _: &str, _: i32) -> Result<(), String> {
        Err("MIDI is unavailable".into())
    }
}

#[cfg(test)]
#[path = "core_tests.rs"]
mod tests;
