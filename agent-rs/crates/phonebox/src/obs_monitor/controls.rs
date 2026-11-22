//! `ObsMonitor.Controls.cs`: the Control Deck's OBS surface — inputs with volume and mute, the scene's sources,
//! transitions, replay buffer and virtual camera. One shared discovery at a time; a deck is served only while the
//! session it describes is current, and for 1.5 s.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::watch;

use super::{ObsMonitor, lock};
use crate::control_deck::models::DeckResult;

/// A built deck, the session it describes and when (`_deck`, `_deckAt`).
#[derive(Default)]
pub(crate) struct DeckState {
    cache: Mutex<Option<(Value, u64, Option<Instant>)>>,
    /// `_deckGate`: one build at a time.
    gate: tokio::sync::Mutex<()>,
    /// `_deckRefresh`: the shared discovery; Some(deck) once it finished.
    refresh: Mutex<Option<watch::Receiver<Option<Value>>>>,
}

impl ObsMonitor {
    /// `GET /api/controls/obs`: the deck, or after 1 s of discovery the last deck of this session, or "loading".
    pub async fn deck_snapshot(self: &Arc<Self>) -> Value {
        if !self.deck_usable() {
            return self.disconnected_deck();
        }
        if let Some(cached) = self.cached_deck(true) {
            return cached;
        }
        let mut refresh = {
            let mut slot = lock(&self.deck.refresh);
            match &*slot {
                Some(running) if running.borrow().is_none() => running.clone(),
                _ => {
                    let (done, refresh) = watch::channel(None);
                    let me = self.clone();
                    tokio::spawn(async move {
                        let deck = me.build_deck().await;
                        let _ = done.send(Some(deck));
                    });
                    *slot = Some(refresh.clone());
                    refresh
                }
            }
        };
        // discovery can involve many inputs: keep the phone responsive while one shared refresh completes
        let finished = tokio::time::timeout(
            Duration::from_millis(1000),
            refresh.wait_for(Option::is_some),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|deck| deck.clone());
        if !self.deck_usable() {
            return self.disconnected_deck();
        }
        if let Some(deck) = finished {
            return deck;
        }
        self.cached_deck(false).unwrap_or_else(|| {
            json!({ "connected": false, "loading": true, "error": "Discovering OBS controls; waiting for OBS" })
        })
    }

    fn disconnected_deck(&self) -> Value {
        let error = self
            .snapshot()
            .error
            .unwrap_or_else(|| "Configure the OBS connection in desktop settings".into());
        json!({ "connected": false, "error": error })
    }

    /// Turning OBS off in the settings stops the deck at once, before the connection loop notices.
    fn deck_usable(&self) -> bool {
        self.snapshot().connected && self.cfg.get().obs.enabled
    }

    /// The cached deck if it belongs to the current session (and, when `fresh`, is younger than 1.5 s).
    fn cached_deck(&self, fresh: bool) -> Option<Value> {
        let current = self.current()?;
        let cache = lock(&self.deck.cache);
        let (deck, session, at) = cache.as_ref()?;
        let recent = at.is_some_and(|at| at.elapsed() < Duration::from_millis(1500));
        (*session == current.id && (!fresh || recent)).then(|| deck.clone())
    }

    /// The deck is no longer recent (an action changed something); it stays as the stale answer.
    fn age_deck(&self) {
        if let Some((_, _, at)) = lock(&self.deck.cache).as_mut() {
            *at = None;
        }
    }

    async fn build_deck(self: &Arc<Self>) -> Value {
        let Some(session) = self.current() else {
            return self.disconnected_deck();
        };
        if !self.snapshot().connected || !session.is_open() {
            return self.disconnected_deck();
        }
        if let Some(fast) = self.cached_deck(true) {
            return fast;
        }
        let _gate = self.deck.gate.lock().await;
        let retired = |me: &ObsMonitor| !me.snapshot().connected || !me.is_current(&session);
        if retired(self) || !session.is_open() {
            return self.disconnected_deck();
        }
        if let Some(fresh) = self.cached_deck(true) {
            return fresh;
        }
        let optional = |kind: &'static str, data: Option<Value>| async move {
            self.request(kind, data).await.ok()
        };
        let listed = optional("GetInputList", None).await;
        let mut inputs = Vec::new();
        for input in listed
            .as_ref()
            .and_then(|l| l["inputs"].as_array())
            .into_iter()
            .flatten()
            .take(64)
        {
            if retired(self) {
                return self.disconnected_deck();
            }
            let Some(name) = input["inputName"].as_str().filter(|n| !n.is_empty()) else {
                continue;
            };
            let volume = optional("GetInputVolume", Some(json!({ "inputName": name }))).await;
            let mute = optional("GetInputMute", Some(json!({ "inputName": name }))).await;
            let volume = volume
                .as_ref()
                .and_then(|v| v["inputVolumeMul"].as_f64())
                .map(|mul| (mul * 100.0).round_ties_even());
            let muted = mute
                .as_ref()
                .and_then(|m| m.get("inputMuted"))
                .cloned()
                .unwrap_or(Value::Null);
            inputs.push(json!({ "id": name, "name": name, "volume": volume, "muted": muted }));
        }
        let snap = self.snapshot();
        let mut sources = Vec::new();
        if let Some(scene) = &snap.scene {
            let items = optional("GetSceneItemList", Some(json!({ "sceneName": scene }))).await;
            for item in items
                .as_ref()
                .and_then(|i| i["sceneItems"].as_array())
                .into_iter()
                .flatten()
            {
                let field = |k: &str| item.get(k).cloned().unwrap_or(Value::Null);
                sources.push(
                    json!({ "id": field("sceneItemId"), "name": field("sourceName"),
                    "enabled": field("sceneItemEnabled") }),
                );
            }
        }
        let transitions = optional("GetSceneTransitionList", None).await;
        let transition = optional("GetCurrentSceneTransition", None).await;
        let replay = optional("GetReplayBufferStatus", None).await;
        let camera = optional("GetVirtualCamStatus", None).await;
        if retired(self) || !session.is_open() || !self.cfg.get().obs.enabled {
            return self.disconnected_deck();
        }
        let s = self.snapshot();
        let member = |v: &Option<Value>, k: &str| {
            v.as_ref()
                .and_then(|v| v.get(k))
                .cloned()
                .unwrap_or(Value::Null)
        };
        let deck = json!({
            "connected": true,
            "scene": snap.scene,
            "scenes": s.scenes,
            "inputs": inputs,
            "sources": sources,
            "streaming": s.streaming,
            "recording": s.recording,
            "recordPaused": s.record_paused,
            "streamMs": s.stream_ms,
            "recordMs": s.record_ms,
            "transitions": transitions.as_ref().and_then(|t| t.get("transitions")).filter(|t| !t.is_null())
                .cloned().unwrap_or_else(|| json!([])),
            "transition": member(&transition, "transitionName"),
            "duration": member(&transition, "transitionDuration"),
            "replayAvailable": replay.is_some(),
            "replayActive": member(&replay, "outputActive"),
            "cameraAvailable": camera.is_some(),
            "cameraActive": member(&camera, "outputActive"),
        });
        // the session may have been retired since the check above: then its cleanup already ran, and this deck must
        // neither be cached nor reported as connected
        let ws = lock(&self.ws);
        if !self.snapshot().connected || !ws.as_ref().is_some_and(|w| Arc::ptr_eq(w, &session)) {
            return self.disconnected_deck();
        }
        *lock(&self.deck.cache) = Some((deck.clone(), session.id, Some(Instant::now())));
        deck
    }

    /// `POST /api/controls/obs/{action}`: only for inputs, sources and transitions the deck lists.
    pub async fn deck_act(
        self: &Arc<Self>,
        action: &str,
        id: Option<&str>,
        value: Option<i32>,
        enabled: Option<bool>,
    ) -> DeckResult {
        const CONTROL_OFF: &str = "Enable OBS control in the widget's desktop settings";
        if !super::control_allowed(&self.cfg.get()) {
            return DeckResult::fail(CONTROL_OFF);
        }
        if !self.snapshot().connected {
            return DeckResult::fail("OBS is not connected");
        }
        let state = self.deck_snapshot().await;
        // discovery awaited OBS: the widget may have been hidden or its control turned off meanwhile
        if !super::control_allowed(&self.cfg.get()) {
            return DeckResult::fail(CONTROL_OFF);
        }
        let listed = |list: &str, key: &str, nonnull: &str| {
            state[list].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|i| i[key].as_str() == id && (nonnull.is_empty() || !i[nonnull].is_null()))
            })
        };
        let request = match action {
            "volume"
                if value.is_some_and(|v| (0..=100).contains(&v))
                    && listed("inputs", "id", "volume") =>
            {
                Some((
                    "SetInputVolume",
                    Some(
                        json!({ "inputName": id, "inputVolumeMul": f64::from(value.unwrap_or(0)) / 100.0 }),
                    ),
                ))
            }
            "mute" if enabled.is_some() && listed("inputs", "id", "muted") => Some((
                "SetInputMute",
                Some(json!({ "inputName": id, "inputMuted": enabled })),
            )),
            "source" => match (enabled, id.and_then(|i| i.trim().parse::<i32>().ok())) {
                (Some(on), Some(item))
                    if state["sources"].as_array().is_some_and(|s| {
                        s.iter().any(|s| s["id"].as_i64() == Some(i64::from(item)))
                    }) =>
                {
                    Some((
                        "SetSceneItemEnabled",
                        Some(
                            json!({ "sceneName": state["scene"], "sceneItemId": item, "sceneItemEnabled": on }),
                        ),
                    ))
                }
                _ => None,
            },
            "scene" if id.is_some_and(|i| self.snapshot().scenes.iter().any(|s| s == i)) => {
                Some(("SetCurrentProgramScene", Some(json!({ "sceneName": id }))))
            }
            "transition" if listed("transitions", "transitionName", "") => Some((
                "SetCurrentSceneTransition",
                Some(json!({ "transitionName": id })),
            )),
            "duration" if value.is_some_and(|v| (50..=10_000).contains(&v)) => Some((
                "SetCurrentSceneTransitionDuration",
                Some(json!({ "transitionDuration": value })),
            )),
            "record" => Some(("ToggleRecord", None)),
            "pause" => Some(("ToggleRecordPause", None)),
            "stream" => Some(("ToggleStream", None)),
            "replay" if state["replayAvailable"] == true => Some(("ToggleReplayBuffer", None)),
            "save-replay" if state["replayActive"] == true => Some(("SaveReplayBuffer", None)),
            "camera" if state["cameraAvailable"] == true => Some(("ToggleVirtualCam", None)),
            _ => None,
        };
        let Some((kind, data)) = request else {
            return DeckResult::fail("This OBS control is unavailable");
        };
        match self.request(kind, data).await {
            Ok(_) => {
                self.age_deck();
                DeckResult::ok()
            }
            Err(e) => {
                crate::log::debug(
                    super::CATEGORY,
                    &format!("OBS Control Deck action failed: {e}"),
                );
                DeckResult::fail("OBS refused this control; check its settings")
            }
        }
    }

    /// Drops the deck (`InvalidateDeck`).
    pub(crate) fn invalidate_deck(&self) {
        *lock(&self.deck.cache) = None;
    }

    /// Tests: the discovery in flight (C# reflected on `_deckRefresh`).
    #[cfg(test)]
    pub(crate) fn discovery(&self) -> Option<watch::Receiver<Option<Value>>> {
        lock(&self.deck.refresh).clone()
    }

    /// Tests: whether a deck is cached (C# reflected on `_deck`).
    #[cfg(test)]
    pub(crate) fn deck_cached(&self) -> bool {
        lock(&self.deck.cache).is_some()
    }
}
