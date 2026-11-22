//! Port of `RuleEngine.cs`: automation rules (Settings › Rules). Checks every rule once a second and, when its
//! condition starts to hold, switches the phone to a page, shows a notification, pushes a value or runs a shortcut.
//! Rules marked "while active" undo their effect when the condition ends: the phone goes back to the page it was on, a
//! pushed value is removed again, a toggle switches back off.

mod inputs;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use jiff::civil::{DateTime, Weekday};
use serde_json::{Value, json};

pub use inputs::{LiveInputs, NoInputs, RuleInputs};

use crate::audio_control::AudioControl;
use crate::config::RuleConfig;
use crate::config::store::ConfigStore;
use crate::push_store::{PushItem, PushStore, json_element_text};
use crate::server::Stop;

const CATEGORY: &str = "PhoneBox.Agent.RuleEngine";

/// CPU spikes are normal; the CPU rule waits this long unless the rule sets its own "for" time.
const CPU_SUSTAIN_SEC: i64 = 15;
/// A condition has to stay off this long before a rule ends, so a flickering condition does not flap.
pub const RELEASE_SEC: i64 = 4;
/// .NET ticks (100 ns) in a day.
const DAY_TICKS: i64 = 864_000_000_000;

/// The engine's clocks: a monotonic one for holding and cooling down, the Unix time for pauses and stale values, and
/// the local time for days and hours.
#[derive(Clone)]
pub struct RuleClock {
    pub mono_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub unix_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub local: Arc<dyn Fn() -> DateTime + Send + Sync>,
}

impl RuleClock {
    pub fn system() -> RuleClock {
        let start = Instant::now();
        RuleClock {
            mono_ms: Arc::new(move || start.elapsed().as_millis() as i64),
            unix_ms: Arc::new(crate::server::now_ms),
            local: Arc::new(|| jiff::Zoned::now().datetime()),
        }
    }
}

struct RuleState {
    rule: RuleConfig,
    active: bool,
    true_since: Option<i64>,
    false_since: Option<i64>,
}

#[derive(Default)]
struct Inner {
    /// Keyed by the rule's serialized form, so editing a rule ends the old version and starts the new one.
    states: IndexMap<String, RuleState>,
    /// Last time each rule (by id) fired, for the cooldown; survives edits of the rule.
    fired: HashMap<String, i64>,
}

type Warn = Arc<dyn Fn(&str) + Send + Sync>;

pub struct RuleEngine {
    cfg: Arc<ConfigStore>,
    push: Arc<PushStore>,
    audio: Option<Arc<AudioControl>>,
    inputs: Arc<dyn RuleInputs>,
    clock: RuleClock,
    inner: Mutex<Inner>,
    active: Mutex<Vec<String>>,
    /// Unix ms; 0 = not paused.
    paused_until: AtomicI64,
    /// When the agent started (Unix ms); the tests move it back to skip the start-up grace of stale rules.
    started_at_ms: AtomicI64,
    warn: Warn,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl RuleEngine {
    pub fn new(
        cfg: Arc<ConfigStore>,
        push: Arc<PushStore>,
        audio: Option<Arc<AudioControl>>,
        inputs: Arc<dyn RuleInputs>,
        clock: RuleClock,
    ) -> Arc<RuleEngine> {
        let started = (clock.unix_ms)();
        Arc::new(RuleEngine {
            cfg,
            push,
            audio,
            inputs,
            clock,
            inner: Default::default(),
            active: Default::default(),
            paused_until: AtomicI64::new(0),
            started_at_ms: AtomicI64::new(started),
            warn: Arc::new(|line| crate::log::warn(CATEGORY, line)),
        })
    }

    /// Ids of the rules whose condition holds right now (the phone keeps a rule's page only while it is listed).
    pub fn active(&self) -> Vec<String> {
        lock(&self.active).clone()
    }

    /// Unix ms until which rules are paused (from the phone or the settings), or None.
    pub fn paused_until(&self) -> Option<i64> {
        let until = self.paused_until.load(Ordering::SeqCst);
        (until > (self.clock.unix_ms)()).then_some(until)
    }

    /// Pauses all rules for a while (0 resumes). Rules that hold end now and fire again after the pause.
    pub fn pause(&self, minutes: i32) {
        let until = if minutes <= 0 {
            0
        } else {
            (self.clock.unix_ms)() + minutes.min(24 * 60) as i64 * 60_000
        };
        self.paused_until.store(until, Ordering::SeqCst);
        if minutes <= 0 {
            crate::log::info(CATEGORY, "rules resumed");
        } else {
            // the minutes as asked, before the one-day cap (as C# logged them)
            crate::log::info(CATEGORY, &format!("rules paused for {minutes} min"));
        }
    }

    /// Runs a rule's action once, regardless of its condition (the Test button in the settings).
    pub fn test(&self, rule: &RuleConfig) {
        self.act(rule, "test");
    }

    /// After 3 s (the monitors take their first readings), every second until `stop`, on a blocking thread: the
    /// conditions run probes and shortcuts.
    pub fn start(self: &Arc<Self>, stop: Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut pause = Duration::from_secs(3);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(pause) => {}
                    _ = stop.stopped() => return,
                }
                pause = Duration::from_secs(1);
                let engine = me.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || engine.evaluate()).await {
                    (me.warn)(&format!("rules: {e}"));
                }
            }
        })
    }

    /// One pass over the rules (the tests drive it directly).
    pub(crate) fn evaluate(&self) {
        let cfg = self.cfg.get();
        let keys: IndexMap<String, RuleConfig> = cfg
            .rules
            .iter()
            .filter(|r| r.enabled)
            .map(|r| (serde_json::to_string(r).unwrap_or_default(), r.clone()))
            .collect();
        let paused = self.paused_until().is_some();
        let mut inner = lock(&self.inner);
        let Inner { states, fired } = &mut *inner;

        let gone: Vec<String> = states
            .keys()
            .filter(|k| paused || !keys.contains_key(*k))
            .cloned()
            .collect();
        for key in gone {
            if let Some(st) = states.get(&key)
                && st.active
                && st.rule.while_active
            {
                let rule = st.rule.clone();
                self.undo(&rule, &key, states);
                // the edit or the pause took the effect back, not the condition: the new version (or the rule after
                // the pause) may bring it back at once instead of waiting out the rest time
                fired.remove(&rule.id);
            }
            states.shift_remove(&key);
        }
        if paused {
            *lock(&self.active) = vec![];
            return;
        }

        let now = (self.clock.mono_ms)();
        let clock = (self.clock.local)();
        let mut active = Vec::new();
        for (key, rule) in &keys {
            let (holds, detail) = if allowed(rule, clock) {
                self.check(rule)
            } else {
                (false, String::new())
            };
            let st = states.entry(key.clone()).or_insert_with(|| RuleState {
                rule: rule.clone(),
                active: false,
                true_since: None,
                false_since: None,
            });
            let (true_since, false_since) = if holds {
                st.false_since = None;
                (*st.true_since.get_or_insert(now), None)
            } else {
                st.true_since = None;
                (now, Some(*st.false_since.get_or_insert(now)))
            };
            let sustain_sec = if rule.for_sec > 0 {
                rule.for_sec as i64
            } else if rule.when == "cpu-above" {
                CPU_SUSTAIN_SEC
            } else {
                0
            };
            let sustained = holds && now - true_since >= sustain_sec * 1000;
            let cooled = fired
                .get(&rule.id)
                .is_none_or(|last| now - last >= rule.cooldown_min as i64 * 60_000);
            let fire = sustained && !st.active && cooled;
            let end = !fire
                && !holds
                && st.active
                && (false_since.is_some_and(|f| now - f >= RELEASE_SEC * 1000)
                    || rule.when == "time");
            if fire {
                st.active = true;
            } else if end {
                st.active = false;
            }
            if fire {
                fired.insert(rule.id.clone(), now);
                crate::log::info(CATEGORY, &format!("rule {} fired ({detail})", rule.id));
                self.act(rule, &detail);
            } else if end {
                crate::log::info(CATEGORY, &format!("rule {} ended", rule.id));
                if rule.while_active {
                    self.undo(rule, key, states);
                }
            }
            if states.get(key).is_some_and(|st| st.active) {
                active.push(rule.id.clone());
            }
        }
        *lock(&self.active) = active;
    }

    #[cfg(test)]
    pub(crate) fn with_warn(self: Arc<Self>, warn: Warn) -> Arc<Self> {
        let mut me = Arc::into_inner(self).expect("not shared yet");
        me.warn = warn;
        Arc::new(me)
    }

    #[cfg(test)]
    pub(crate) fn started_at_ms(&self) -> i64 {
        self.started_at_ms.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn set_started_at_ms(&self, ms: i64) {
        self.started_at_ms.store(ms, Ordering::SeqCst);
    }

    /// `Do`: the rule's action.
    fn act(&self, r: &RuleConfig, detail: &str) {
        match r.then.as_str() {
            "goto" if r.page.as_deref().is_some_and(|p| !p.trim().is_empty()) => {
                self.push.emit(
                    "goto",
                    Some(json!({
                        "page": r.page,
                        "rule": r.id,
                        "hold": r.while_active,
                        "name": if r.name.is_empty() { None } else { Some(&r.name) },
                        "detail": detail,
                        "priority": r.priority,
                        "countdown": r.countdown_sec,
                    })),
                );
            }
            "notify" => {
                let title = if r.name.is_empty() {
                    "PhoneBox"
                } else {
                    &r.name
                };
                let text = match &r.text {
                    Some(t) if !t.trim().is_empty() => t.replace("{detail}", detail),
                    _ => detail.to_string(),
                };
                let level = r.level.as_deref().unwrap_or("info");
                self.push.emit(
                    "notify",
                    Some(json!({
                        "title": title,
                        "text": text,
                        "level": level,
                        "wake": matches!(r.level.as_deref(), Some("warn" | "error")),
                    })),
                );
            }
            "push" => {
                if let Some(key) = r.key.as_deref().filter(|k| !k.trim().is_empty())
                    && crate::push_store::valid_key(key)
                {
                    let value = r
                        .push_value
                        .as_deref()
                        .unwrap_or("true")
                        .replace("{detail}", detail);
                    // the result ("too many keys") is ignored, as C# did
                    let _ = self
                        .push
                        .set(key, crate::push_store::parse_value(&value), None);
                }
            }
            "shortcut" => {
                let Some(id) = r.shortcut.as_deref().filter(|s| !s.trim().is_empty()) else {
                    return;
                };
                // one settings snapshot for the lookup and the run
                let cfg = self.cfg.get();
                if r.while_active
                    && let Some(target) = cfg.shortcuts.iter().find(|s| s.id == id)
                    && (target.toggle || target.cmd == "builtin:mute")
                    && self.shortcut_state(target) == Some(true)
                {
                    return; // already on
                }
                if let Some(error) =
                    crate::shortcuts::run(&cfg, id, Some(&self.push), self.audio.as_deref())
                {
                    (self.warn)(&format!("rule {}: {error}", r.id));
                }
            }
            _ => {}
        }
    }

    fn shortcut_state(&self, sc: &crate::config::Shortcut) -> Option<bool> {
        crate::shortcuts::state(sc, Some(&self.push), self.audio.as_deref())
    }

    /// The effect of a "while active" rule taken back; `key` is its state's (the other holders of a toggle are the
    /// other states).
    fn undo(&self, r: &RuleConfig, key: &str, states: &IndexMap<String, RuleState>) {
        match r.then.as_str() {
            "goto" => {
                self.push.emit("back", Some(json!({ "rule": r.id })));
            }
            "push" => {
                if let Some(k) = r.key.as_deref().filter(|k| !k.trim().is_empty()) {
                    self.push.remove(k);
                }
            }
            "shortcut" => {
                // one settings snapshot for the lookup and the run: a save in between must not run another shortcut
                let cfg = self.cfg.get();
                let Some(sc) = cfg
                    .shortcuts
                    .iter()
                    .find(|s| Some(s.id.as_str()) == r.shortcut.as_deref())
                else {
                    return;
                };
                if !(sc.toggle || sc.cmd == "builtin:mute")
                    || self.shortcut_state(sc) == Some(false)
                {
                    return;
                }
                // another "while active" rule still holds the same toggle on (act skipped it because it was on
                // already): only the last one to end switches it off
                let held = states.iter().any(|(k, o)| {
                    k != key
                        && o.active
                        && o.rule.while_active
                        && o.rule.then == "shortcut"
                        && o.rule.shortcut == r.shortcut
                });
                if held {
                    return;
                }
                // a toggle switches back off
                if let Some(error) =
                    crate::shortcuts::run(&cfg, &sc.id, Some(&self.push), self.audio.as_deref())
                {
                    (self.warn)(&format!(
                        "rule {}: could not switch {} back off: {error}",
                        r.id, sc.id
                    ));
                }
            }
            _ => {}
        }
    }

    /// Whether the rule's condition holds now, and the detail for its action.
    fn check(&self, r: &RuleConfig) -> (bool, String) {
        let m = r.r#match.as_deref().unwrap_or("").trim().to_string();
        let names: Vec<String> = m
            .split('|')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect();
        let any_name = |hay: &str| names.is_empty() || names.iter().any(|n| contains_ci(hay, n));
        let no = (false, String::new());
        match r.when.as_str() {
            "session-waiting" | "session-busy" => {
                let status = if r.when == "session-waiting" {
                    "waiting"
                } else {
                    "busy"
                };
                let found = self.inputs.sessions().into_iter().find(|s| {
                    s.status == status
                        && (names.is_empty()
                            || names
                                .iter()
                                .any(|n| contains_ci(&s.project, n) || eq_ci(&s.tool, n)))
                });
                match found {
                    None => no,
                    Some(s) if status == "waiting" => (
                        true,
                        format!(
                            "{}: {}",
                            s.project,
                            s.waiting_for.as_deref().unwrap_or("needs you")
                        ),
                    ),
                    Some(s) => (true, format!("{} is working", s.project)),
                }
            }
            "media-playing" => match self.inputs.media() {
                Some(media) if media.playing && any_name(&media.app) => (
                    true,
                    if media.artist.is_empty() {
                        media.title
                    } else {
                        format!("{} \u{b7} {}", media.title, media.artist)
                    },
                ),
                _ => no,
            },
            "process-running" => {
                let procs = self.inputs.processes();
                let hit = names.iter().map(|n| strip_exe(n)).find(|n| {
                    procs.iter().any(|p| eq_ci(p, n))
                        || procs
                            .iter()
                            .any(|p| p.encode_utf16().count() >= 15 && starts_with_ci(n, p))
                });
                match hit {
                    Some(n) => (true, n.to_string()),
                    None => no,
                }
            }
            "app-in-front" => match self.inputs.front_app() {
                Some(front) if names.iter().any(|n| contains_ci(&front, n)) => (true, front),
                _ => no,
            },
            "mic-in-use" | "camera-in-use" => {
                let snap = self.inputs.privacy();
                let apps = snap
                    .map(|p| {
                        if r.when == "mic-in-use" {
                            p.mic_apps
                        } else {
                            p.camera_apps
                        }
                    })
                    .unwrap_or_default();
                match apps.into_iter().find(|a| any_name(a)) {
                    Some(app) => (true, app),
                    None => no,
                }
            }
            "obs-live" => match self.inputs.obs() {
                (true, streaming) => (
                    true,
                    if streaming {
                        "OBS is streaming"
                    } else {
                        "OBS is recording"
                    }
                    .to_string(),
                ),
                _ => no,
            },
            "calendar-event" => match self.inputs.calendar_event(&names) {
                Some(event) => (true, event.title),
                None => no,
            },
            "cpu-above" => match self.inputs.cpu() {
                Some(cpu) if cpu >= r.value.unwrap_or(85.0) => {
                    (true, format!("CPU {}%", zero(cpu)))
                }
                _ => no,
            },
            "idle" => {
                let minutes = r.value.unwrap_or(5.0);
                if self.inputs.idle_seconds() >= minutes * 60.0 {
                    (true, format!("away for {} min", zero(minutes)))
                } else {
                    no
                }
            }
            "time" => {
                if in_window(&m, time_of_day((self.clock.local)())) {
                    (true, m)
                } else {
                    no
                }
            }
            "quota-above" => {
                let limit = r.value.unwrap_or(80.0);
                for q in self.inputs.quotas() {
                    if !names.is_empty()
                        && !names
                            .iter()
                            .any(|n| contains_ci(&q.id, n) || contains_ci(&q.name, n))
                    {
                        continue;
                    }
                    // the highest one over the limit (the first of equals)
                    let mut hit: Option<&crate::quota::QuotaLimit> = None;
                    for l in q.limits.iter().filter(|l| l.percent >= limit) {
                        if hit.is_none_or(|h| l.percent > h.percent) {
                            hit = Some(l);
                        }
                    }
                    if let Some(hit) = hit {
                        return (
                            true,
                            format!("{} {} {}%", q.name, hit.label, zero(hit.percent)),
                        );
                    }
                }
                no
            }
            "review-requested" => {
                let matching: Vec<_> = self
                    .inputs
                    .github()
                    .map(|g| g.review_requests)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| !p.draft && any_name(&p.repo))
                    .collect();
                match matching.as_slice() {
                    [] => no,
                    [one] => (true, format!("{}#{}", one.repo, one.number)),
                    many => (true, format!("{} reviews waiting", many.len())),
                }
            }
            "ci-failed" => {
                let pr = self
                    .inputs
                    .github()
                    .map(|g| g.my_prs)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|p| {
                        matches!(
                            p.ci.as_deref().map(str::to_uppercase).as_deref(),
                            Some("FAILURE" | "ERROR")
                        ) && any_name(&p.repo)
                    });
                match pr {
                    Some(p) => (true, format!("CI failed: {}#{}", p.repo, p.number)),
                    None => no,
                }
            }
            "battery-low" => {
                let limit = r.value.unwrap_or(20.0);
                let found = self.inputs.batteries().into_iter().find(|d| {
                    d.level as f64 <= limit && d.charging != Some(true) && any_name(&d.name)
                });
                match found {
                    Some(b) => (true, format!("{} {}%", b.name, b.level)),
                    None => no,
                }
            }
            "ping-high" => {
                let limit = r.value.unwrap_or(150.0);
                match self.inputs.ping() {
                    None => no,
                    Some(p) => match p.ms {
                        None => (true, format!("{} unreachable", p.host)),
                        Some(_) if p.loss_pct >= 50.0 => (true, format!("{} unreachable", p.host)),
                        Some(ms) if ms >= limit => (true, format!("{} {} ms", p.host, zero(ms))),
                        Some(_) => no,
                    },
                }
            }
            "always" => (true, String::new()),
            "push" => self.check_push(r, &m),
            _ => no,
        }
    }

    /// The `push` trigger: a pushed value that is on, compares as asked, or has not been updated for a while.
    fn check_push(&self, r: &RuleConfig, key: &str) -> (bool, String) {
        if r.op.as_deref() == Some("stale") {
            if key.is_empty() {
                return (false, String::new());
            }
            return stale(
                key,
                self.push.get(key).as_ref(),
                r.arg.as_deref(),
                (self.clock.unix_ms)(),
                self.started_at_ms.load(Ordering::SeqCst),
            );
        }
        let Some(item) = (!key.is_empty())
            .then(|| self.push.snapshot().shift_remove(key))
            .flatten()
        else {
            return (false, String::new());
        };
        let d = &item.data;
        if let Some(op) = r.op.as_deref().filter(|op| !op.is_empty() && *op != "on") {
            return if compare(d, op, r.arg.as_deref()) {
                (true, json_element_text(d))
            } else {
                (false, String::new())
            };
        }
        // {"value": "off", "sub": …} is judged by its value, as compare and a toggle shortcut's state read it
        let v = match d {
            Value::Object(o) if o.contains_key("value") => &o["value"],
            other => other,
        };
        let truthy = match v {
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
            // any case, like a toggle shortcut's state: "OFF" (MQTT), "False" (PowerShell's $false)
            Value::String(t) => {
                !t.is_empty() && !matches!(t.to_lowercase().as_str(), "0" | "false" | "off" | "no")
            }
            Value::Object(_) | Value::Array(_) => true,
            Value::Null => false,
        };
        if truthy {
            (true, json_element_text(d))
        } else {
            (false, String::new())
        }
    }
}

/// The engine `/api/state` reads, set once main has built it over its monitors.
#[derive(Default)]
pub struct RulesSlot(std::sync::RwLock<Option<Arc<RuleEngine>>>);

impl RulesSlot {
    pub fn set(&self, engine: Arc<RuleEngine>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(engine);
    }

    fn get(&self) -> Option<Arc<RuleEngine>> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// `rules` in `/api/state`: the ids of the rules that hold.
pub struct ActiveRules(pub Arc<RulesSlot>);

impl crate::state::StateSource for ActiveRules {
    fn snapshot(&self) -> Value {
        json!(self.0.get().map(|e| e.active()).unwrap_or_default())
    }
}

/// `rulesPaused` in `/api/state`: Unix ms until which the rules are paused, or null.
pub struct RulesPaused(pub Arc<RulesSlot>);

impl crate::state::StateSource for RulesPaused {
    fn snapshot(&self) -> Value {
        json!(self.0.get().and_then(|e| e.paused_until()))
    }
}

/// C#'s `{x:0}`: rounded half away from zero, no decimals.
fn zero(x: f64) -> String {
    format!("{}", x.round() as i64)
}

/// `OrdinalIgnoreCase` comparisons (by upper case, as .NET does).
fn eq_ci(a: &str, b: &str) -> bool {
    a.to_uppercase() == b.to_uppercase()
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_uppercase().contains(&needle.to_uppercase())
}

fn starts_with_ci(text: &str, prefix: &str) -> bool {
    text.to_uppercase().starts_with(&prefix.to_uppercase())
}

/// A process name as written, without a trailing ".exe" (any case).
fn strip_exe(name: &str) -> &str {
    let n = name.len();
    if n >= 4 && name.is_char_boundary(n - 4) && name[n - 4..].eq_ignore_ascii_case(".exe") {
        &name[..n - 4]
    } else {
        name
    }
}

/// The time of day in .NET ticks.
fn time_of_day(now: DateTime) -> i64 {
    let t = now.time();
    ((t.hour() as i64 * 60 + t.minute() as i64) * 60 + t.second() as i64) * 10_000_000
        + t.subsec_nanosecond() as i64 / 100
}

const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// "mon,tue,wed" (empty = every day).
pub fn on_day(days: Option<&str>, day: Weekday) -> bool {
    let Some(days) = days.filter(|d| !d.trim().is_empty()) else {
        return true;
    };
    let name = DAY_NAMES[day.to_sunday_zero_offset() as usize];
    days.split(',').map(str::trim).any(|d| eq_ci(d, name))
}

/// The rule's extra conditions: only on some days, only at some hours.
pub fn allowed(r: &RuleConfig, now: DateTime) -> bool {
    on_day(r.days.as_deref(), now.weekday())
        && r.during
            .as_deref()
            .is_none_or(|d| d.trim().is_empty() || in_window(d, time_of_day(now)))
}

/// "22:00-07:00" (may wrap past midnight) or a single "08:30" (that minute); `now` in .NET ticks since midnight.
pub fn in_window(window: &str, now: i64) -> bool {
    let parts: Vec<&str> = window.split('-').map(str::trim).collect();
    if parts.len() > 2 {
        return false;
    }
    let Some(from) = clock(parts[0]) else {
        return false;
    };
    // a single time means that minute
    let to = match parts.get(1) {
        None => from + 60 * 10_000_000,
        Some(end) => match clock(end) {
            Some(to) => to,
            None => return false,
        },
    };
    if from <= to {
        now >= from && now < to
    } else {
        now >= from || now < to
    }
}

/// A time of day in ticks; "24:00" is the end of the day (TimeSpan.TryParse rejects it).
fn clock(text: &str) -> Option<i64> {
    if matches!(text, "24:00" | "24:00:00") {
        return Some(DAY_TICKS);
    }
    parse_timespan(text).filter(|t| (0..DAY_TICKS).contains(t))
}

/// `TimeSpan.TryParse` (invariant) for the forms a time of day is written in: `[-]d` (a bare number is days) or
/// `[-][d.]h:m[:s[.f]]` with h 0-23, m and s 0-59 and up to 7 fraction digits; in ticks.
fn parse_timespan(text: &str) -> Option<i64> {
    const MAX_DAYS: i64 = 10_675_199;
    let t = text.trim();
    let (negative, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let digits = |s: &str| !s.is_empty() && s.len() <= 10 && s.bytes().all(|b| b.is_ascii_digit());
    let num = |s: &str, max: i64| -> Option<i64> {
        if !digits(s) {
            return None;
        }
        s.parse::<i64>().ok().filter(|v| *v <= max)
    };
    let ticks = match t.find(':') {
        None => num(t, MAX_DAYS)? * DAY_TICKS,
        Some(colon) => {
            let (days, hours) = match t[..colon].split_once('.') {
                Some((d, h)) => (num(d, MAX_DAYS)?, h),
                None => (0, &t[..colon]),
            };
            let h = num(hours, 23)?;
            let rest = &t[colon + 1..];
            let (m, s, fraction) = match rest.split_once(':') {
                None => (num(rest, 59)?, 0, 0),
                Some((m, sec)) => {
                    let (s, fraction) = match sec.split_once('.') {
                        None => (sec, 0),
                        Some((s, f)) => {
                            if !digits(f) || f.len() > 7 {
                                return None;
                            }
                            (s, f.parse::<i64>().ok()? * 10i64.pow(7 - f.len() as u32))
                        }
                    };
                    (num(m, 59)?, num(s, 59)?, fraction)
                }
            };
            days * DAY_TICKS + ((h * 60 + m) * 60 + s) * 10_000_000 + fraction
        }
    };
    Some(if negative { -ticks } else { ticks })
}

/// Compares a pushed value: above / at least / below / at most a number, or equals / differs from a text
/// (case-insensitive).
pub fn compare(d: &Value, op: &str, arg: Option<&str>) -> bool {
    use crate::config::normalize::parse_invariant_double as number;
    let text = match d {
        Value::Object(o) if o.contains_key("value") => &o["value"],
        other => other,
    };
    // no finite check: "NaN" and "Infinity" compare as numbers, as in C#
    let num = match text {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => number(s),
        _ => None,
    };
    let arg_num = arg.and_then(number);
    let both = num.zip(arg_num);
    let same_text = || {
        let s = match text {
            Value::String(s) => s.clone(),
            other => json_element_text(other),
        };
        eq_ci(&s, arg.unwrap_or(""))
    };
    match op {
        ">" => both.is_some_and(|(a, b)| a > b),
        ">=" => both.is_some_and(|(a, b)| a >= b),
        "<" => both.is_some_and(|(a, b)| a < b),
        "<=" => both.is_some_and(|(a, b)| a <= b),
        "=" => match both {
            Some((a, b)) => a == b,
            None => same_text(),
        },
        "!=" => match both {
            Some((a, b)) => a != b,
            None => !same_text(),
        },
        _ => false,
    }
}

/// "Has not been updated for N seconds": holds when the value is older than that, or when it is not there at all
/// (never pushed, deleted, or expired by its ttl). The time counts from the agent's start at the earliest, so a value
/// kept from before a restart, or one whose sender has not had the chance to post yet, is not stale at once.
pub fn stale(
    key: &str,
    item: Option<&PushItem>,
    arg: Option<&str>,
    now_ms: i64,
    started_at_ms: i64,
) -> (bool, String) {
    // a match that cannot be a push key (a typo, a pasted URL with a token) never holds, so it never reaches the log
    // or the notification text through {detail}
    if !crate::push_store::valid_key(key) {
        return (false, String::new());
    }
    let Some(sec) = arg
        .and_then(crate::config::normalize::parse_invariant_double)
        .filter(|s| s.is_finite() && *s > 0.0)
    else {
        return (false, String::new());
    };
    let since = item.map_or(0, |i| i.at).max(started_at_ms);
    if ((now_ms - since) as f64) < sec * 1000.0 {
        return (false, String::new());
    }
    let detail = match item {
        None => format!("{key}: no value"),
        Some(i) => format!("{key}: no update for {}", age(now_ms - i.at)),
    };
    (true, detail)
}

fn age(ms: i64) -> String {
    if ms < 120_000 {
        format!("{} s", (ms / 1000).max(0))
    } else if ms < 2 * 3_600_000 {
        format!("{} min", ms / 60_000)
    } else if ms < 2 * 86_400_000 {
        format!("{} h", ms / 3_600_000)
    } else {
        format!("{} days", ms / 86_400_000)
    }
}
