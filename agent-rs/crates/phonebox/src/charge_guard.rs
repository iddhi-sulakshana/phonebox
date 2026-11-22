//! Port of `ChargeGuard.cs`: keeps the phone's battery between two levels (say 50 and 80 %) so a panel that is plugged
//! in all the time does not wear its battery out. Phones cannot stop charging on their own without root, so the power
//! is switched where it comes from: a Home Assistant smart plug (switch entity) or two shortcuts (for example uhubctl
//! for a USB hub with switchable ports, or a smart plug's command line). The phone reports its level every half minute
//! (over USB or Wi-Fi).

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};

use crate::audio_control::AudioControl;
use crate::config::store::ConfigStore;
use crate::config::{AgentConfig, ChargeConfig, HomeAssistantConfig};
use crate::home_assistant::HomeAssistant;
use crate::push_store::PushStore;
use crate::server::Stop;
use crate::usb_doctor::PhoneBatteryReport;

const CATEGORY: &str = "PhoneBox.Agent.ChargeGuard";

/// A switch command is repeated at most this often if the phone's charging state does not follow.
const RETRY_MS: i64 = 10 * 60_000;
/// No report from the phone for this long while the power is off: switch it back on to be safe.
const SILENT_MS: i64 = 10 * 60_000;

/// The phone's latest battery report (the battery route keeps it; tests give their own).
pub type ReportSource = Arc<dyn Fn() -> Option<PhoneBatteryReport> + Send + Sync>;

/// A Home Assistant supply: its switch and the server and credential it was reached with.
#[derive(Debug, Clone, PartialEq)]
struct HaTarget {
    entity: Option<String>,
    url: Option<String>,
    token: Option<String>,
}

/// The same switch on the same server (the token does not matter).
fn same_supply(a: Option<&HaTarget>, b: &HaTarget) -> bool {
    a.is_some_and(|a| a.entity == b.entity && a.url == b.url)
}

/// The supply this guard cut, with the current token when the server is the same (rotated credentials).
fn recovery_target(owner: &HaTarget, current: &HaTarget) -> HaTarget {
    match &current.token {
        Some(token) if owner.url == current.url => HaTarget {
            token: Some(token.clone()),
            ..owner.clone()
        },
        _ => owner.clone(),
    }
}

/// The entity and the connection from one settings snapshot, so a save in between cannot pair an old entity with a
/// new server.
fn target(cfg: &AgentConfig) -> HaTarget {
    HaTarget {
        entity: cfg.charge.entity.clone(),
        url: cfg.home_assistant.url.clone(),
        token: cfg.home_assistant.token.clone(),
    }
}

/// Whether the supply a test asked for (its settings and server) is still the configured one.
fn same_supply_settings(
    requested: &ChargeConfig,
    requested_url: Option<&str>,
    current: &AgentConfig,
) -> bool {
    requested.method == current.charge.method
        && requested.entity == current.charge.entity
        && requested.on_shortcut == current.charge.on_shortcut
        && requested.off_shortcut == current.charge.off_shortcut
        && (requested.method == "shortcuts"
            || requested_url == current.home_assistant.url.as_deref())
}

#[derive(Default)]
struct GuardState {
    /// "on" / "off".
    last: Option<&'static str>,
    /// Unix ms; 0 = never.
    last_at: i64,
    error: Option<String>,
    /// The Home Assistant supply this guard switched off (or may have: an answer can fail after the switch).
    off_ha: Option<HaTarget>,
}

pub struct ChargeGuard {
    cfg: Arc<ConfigStore>,
    ha: Arc<HomeAssistant>,
    push: Arc<PushStore>,
    audio: Option<Arc<AudioControl>>,
    report: ReportSource,
    /// Serializes the checks and the settings' test switches (`_switchGate`).
    gate: tokio::sync::Mutex<()>,
    st: Mutex<GuardState>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `ChargeGuard.Report`, called by the phone (`POST /api/phone/battery`): kept for the guard and the USB doctor.
pub fn report(
    level: i32,
    plugged: bool,
    temp: Option<f64>,
    source: Option<&str>,
    usb: Option<&str>,
) {
    let clean = |s: Option<&str>| {
        s.filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_lowercase())
    };
    crate::usb_doctor::set_last_report(PhoneBatteryReport {
        level: level.clamp(0, 100),
        plugged,
        temp,
        at: crate::server::now_ms(),
        source: clean(source),
        usb: clean(usb),
    });
}

/// What to do for a battery reading: "on", "off" or None (leave the power as it is).
pub fn decide(
    c: &ChargeConfig,
    r: Option<&PhoneBatteryReport>,
    now: i64,
    last: Option<&str>,
    last_at: i64,
) -> Option<&'static str> {
    // lost the phone (or the clock went back): never leave it unpowered
    let Some(r) = r.filter(|r| (0..=SILENT_MS).contains(&(now - r.at))) else {
        return (last == Some("off")).then_some("on");
    };
    let retry = now - last_at > RETRY_MS;
    if r.level >= c.max && r.plugged && (last != Some("off") || retry) {
        return Some("off");
    }
    // A plug cut at the source leaves the phone unplugged. Shortcuts may instead switch the charger inside a rooted
    // phone, which stays plugged in while not charging, so for them "plugged" can't tell whether it charges.
    if r.level <= c.min && (!r.plugged || c.method == "shortcuts") && (last != Some("on") || retry)
    {
        return Some("on");
    }
    None
}

impl ChargeGuard {
    pub fn new(
        cfg: Arc<ConfigStore>,
        ha: Arc<HomeAssistant>,
        push: Arc<PushStore>,
        audio: Option<Arc<AudioControl>>,
        report: ReportSource,
    ) -> ChargeGuard {
        ChargeGuard {
            cfg,
            ha,
            push,
            audio,
            report,
            gate: tokio::sync::Mutex::new(()),
            st: Default::default(),
        }
    }

    /// `GET /api/charge`: the phone's report, the power as last switched, when, and the last error (read without the
    /// gate).
    pub fn status(&self) -> Value {
        let st = lock(&self.st);
        json!({
            "phone": (self.report)(),
            "power": st.last,
            "switchedAt": (st.last_at != 0).then_some(st.last_at),
            "error": st.error,
        })
    }

    /// After 5 s, a check every 30 s until `stop`.
    pub fn start(self: &Arc<Self>, stop: Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut pause = Duration::from_secs(5);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(pause) => {}
                    _ = stop.stopped() => return,
                }
                pause = Duration::from_secs(30);
                me.check().await;
            }
        })
    }

    /// One pass: recover a supply the settings replaced, else switch by the phone's level.
    pub(crate) async fn check(&self) {
        let _gate = self.gate.lock().await;
        let cfg = self.cfg.get();
        let c = &cfg.charge;
        let target = target(&cfg);
        let owner = lock(&self.st).off_ha.clone();
        if let Some(owner) = owner
            && (!c.enabled
                || c.method == "shortcuts"
                || !same_supply(Some(&owner), &target)
                || (owner.token.is_some() && target.token.is_none()))
        {
            // recover the supply this guard cut before acting on replacement settings
            let recovered = self
                .switch_core(&ha_method(), "on", &recovery_target(&owner, &target))
                .await
                .is_none();
            if recovered && (c.method == "shortcuts" || !same_supply(Some(&owner), &target)) {
                // the old supply's state does not carry into the replacement
                let mut st = lock(&self.st);
                st.last = None;
                st.last_at = 0;
            }
            return;
        }
        if !c.enabled {
            if lock(&self.st).last == Some("off") {
                self.switch_core(c, "on", &target).await;
            }
            return;
        }
        let action = {
            let st = lock(&self.st);
            // a test "on" for another supply can leave `last` at "on" while this one is still tracked off; the tracked
            // supply decides, so a phone that stops reporting still gets its power back
            let last = if same_supply(st.off_ha.as_ref(), &target) {
                Some("off")
            } else {
                st.last
            };
            decide(
                c,
                (self.report)().as_ref(),
                crate::server::now_ms(),
                last,
                st.last_at,
            )
        };
        if let Some(action) = action {
            self.switch_core(c, action, &target).await;
        }
    }

    /// The settings' test switch ("on" / "off" for the configured supply); an error, or None.
    pub async fn switch(&self, c: &ChargeConfig, action: &str) -> Option<String> {
        // the server the test was asked for, read before waiting behind another switch
        let requested_url = self.cfg.get().home_assistant.url.clone();
        self.switch_from(c, action, requested_url).await
    }

    pub(crate) async fn switch_from(
        &self,
        c: &ChargeConfig,
        action: &str,
        requested_url: Option<String>,
    ) -> Option<String> {
        let _gate = self.gate.lock().await;
        // a test that waited behind another switch must not reach a supply the settings replaced meanwhile
        let cfg = self.cfg.get();
        if !same_supply_settings(c, requested_url.as_deref(), &cfg) {
            return Some("the power settings changed; test the new supply again".into());
        }
        let c = &cfg.charge;
        let target = target(&cfg);
        let owner = lock(&self.st).off_ha.clone();
        if action == "off"
            && let Some(owner) = owner
            && (c.method == "shortcuts" || !same_supply(Some(&owner), &target))
            && let Some(error) = self
                .switch_core(&ha_method(), "on", &recovery_target(&owner, &target))
                .await
        {
            return Some(error);
        }
        self.switch_core(c, action, &target).await
    }

    /// Switches the power; called with the gate held. A failed "on" keeps the old state, so the next pass asks again.
    async fn switch_core(
        &self,
        c: &ChargeConfig,
        action: &str,
        target: &HaTarget,
    ) -> Option<String> {
        let action: &'static str = if action == "on" { "on" } else { "off" };
        let error = if c.method == "shortcuts" {
            let id = if action == "on" {
                &c.on_shortcut
            } else {
                &c.off_shortcut
            };
            match id.as_deref().filter(|id| !id.trim().is_empty()) {
                None => Some(format!("no shortcut set for power {action}")),
                Some(id) => {
                    let (cfg, id, push, audio) = (
                        self.cfg.get(),
                        id.to_string(),
                        self.push.clone(),
                        self.audio.clone(),
                    );
                    // a shortcut runs a program: off the async workers
                    tokio::task::spawn_blocking(move || {
                        crate::shortcuts::run(&cfg, &id, Some(&push), audio.as_deref())
                    })
                    .await
                    .unwrap_or_else(|_| Some("the shortcut could not be run".into()))
                }
            }
        } else {
            match target.entity.as_deref().filter(|e| !e.trim().is_empty()) {
                None => Some("no Home Assistant switch set".into()),
                Some(entity) => {
                    let recovery = {
                        let st = lock(&self.st);
                        (action == "on" && same_supply(st.off_ha.as_ref(), target))
                            .then(|| st.off_ha.clone())
                            .flatten()
                    };
                    let settings = HomeAssistantConfig {
                        url: target.url.clone(),
                        token: target.token.clone().or(recovery.and_then(|r| r.token)),
                        ..HomeAssistantConfig::default()
                    };
                    let (error, submitted) =
                        self.ha.switch(entity, action == "on", &settings).await;
                    let mut st = lock(&self.st);
                    // a failed answer can follow a successful switch: that supply may be off too
                    if action == "off" && submitted {
                        st.off_ha = Some(target.clone());
                    }
                    if action == "on" && error.is_none() && same_supply(st.off_ha.as_ref(), target)
                    {
                        st.off_ha = None;
                    }
                    error
                }
            }
        };
        {
            let mut st = lock(&self.st);
            if error.is_none() || action == "off" {
                st.last = Some(action);
            }
            st.last_at = crate::server::now_ms();
            st.error = error.clone();
        }
        match &error {
            None => {
                let level = (self.report)()
                    .map(|r| r.level.to_string())
                    .unwrap_or_default();
                crate::log::info(
                    CATEGORY,
                    &format!("charge guard: power {action} (phone at {level}%)"),
                );
            }
            Some(e) => crate::log::warn(
                CATEGORY,
                &format!("charge guard: power {action} failed: {e}"),
            ),
        }
        error
    }
}

/// A Home Assistant supply, whatever the settings say now (to switch back on the one this guard cut).
fn ha_method() -> ChargeConfig {
    ChargeConfig {
        method: "ha".into(),
        ..ChargeConfig::default()
    }
}

#[cfg(test)]
#[path = "charge_guard_tests.rs"]
mod tests;
