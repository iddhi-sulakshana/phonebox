//! `ControlDeck.Integrations.cs`: GitHub workflow dispatch, application audio routing (SoundVolumeView), the Voicemeeter
//! mixer's parameters, and what the phone's card says about each control.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use super::actions::shortcut_running;
use super::*;
use crate::tool_process::{RunError, ToolCommand};

static REPOSITORY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\z").expect("regex"));
static WORKFLOW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[A-Za-z0-9_.-]+\z").expect("regex"));
static MIXER_PARAMETER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\A(?:Strip|Bus)\[(?:[0-9]|[1-5][0-9]|6[0-3])\]\.(?:Gain|Mute|Solo|Mono|A[1-5]|B[1-3])\z",
    )
    .expect("regex")
});

const MIXER_UNSET: &str =
    "Configure the installed Voicemeeter Remote DLL and start its audio engine";

/// A Voicemeeter parameter the deck may read or set (`Strip[0..63]` or `Bus[0..63]`, then a known field).
pub(super) fn mixer_parameter(name: &str) -> bool {
    MIXER_PARAMETER.is_match(name)
}

impl ControlDeck {
    /// What the phone shows for a configured control. Blocks (native states, a tool lookup).
    pub(super) fn describe(&self, c: &AgentConfig, a: &DeckAction, midi: &[DeckItem]) -> DeckItem {
        let mut state = if matches!(a.kind.as_str(), "task" | "server") {
            Some(self.tasks.state(&a.id))
        } else {
            lock(&self.action_results).get(&a.id).cloned()
        };
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
                state = if a.kind == "radio" {
                    self.radio_state(a)
                } else {
                    self.native.action_state(a)
                };
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
            "mixer" | "mixer-mute" => {
                state = self.mixer_value(a);
                available = state.is_some();
                if state.is_none() {
                    state = Some("Mixer is not connected".into());
                }
            }
            "audio-route" if !mixer_executable(c) => {
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

    /// Dispatches a GitHub Actions workflow (`gh workflow run <file> --repo <owner/repo> [--ref <ref>]`, 20 s).
    pub(super) async fn workflow(
        &self,
        action: &DeckAction,
        stop: &Stop,
    ) -> Result<DeckResult, Cancelled> {
        let file = action.argument.as_deref().unwrap_or("");
        if !REPOSITORY.is_match(&action.target) || !WORKFLOW.is_match(file) {
            return Ok(DeckResult::fail(
                "Configure a repository and workflow filename",
            ));
        }
        let Some(gh) = crate::tool_process::find("gh") else {
            return Ok(DeckResult::fail(
                "Install and sign in to GitHub CLI on the computer",
            ));
        };
        let mut command =
            ToolCommand::new(gh).args(["workflow", "run", file, "--repo", &action.target]);
        if let Some(reference) = action.device.as_deref().filter(|d| !d.trim().is_empty()) {
            command = command.args(["--ref", reference]);
        }
        let ok = self
            .run_tool(
                &action.id,
                &command,
                20,
                stop,
                ("Workflow dispatched", "Dispatch failed"),
            )
            .await?;
        Ok(match ok {
            None => DeckResult::fail(CONTROL_FAILED),
            Some(true) => DeckResult::ok(),
            Some(false) => DeckResult::fail(
                "GitHub refused the workflow; check the computer's GitHub CLI account",
            ),
        })
    }

    /// Sends an application's audio to an output device through SoundVolumeView (Windows, 10 s).
    pub(super) async fn route_audio(
        &self,
        action: &DeckAction,
        stop: &Stop,
    ) -> Result<DeckResult, Cancelled> {
        let c = self.cfg.get();
        if !cfg!(windows) || !mixer_executable(&c) {
            return Ok(DeckResult::fail(
                "Configure SoundVolumeView on the computer to use application routing",
            ));
        }
        let exe = c.controls.mixer_executable.clone().unwrap_or_default();
        let device = action.device.clone();
        let output = match device.clone() {
            Some(device) => self
                .blocking(move |d| {
                    d.audio()
                        .devices
                        .iter()
                        .any(|x| x.id == device && x.direction == "output")
                })
                .await
                .unwrap_or(false),
            None => false,
        };
        if action.target.trim().is_empty() || utf16_len(&action.target) > 256 || !output {
            return Ok(DeckResult::fail(
                "Choose an application and an active output device",
            ));
        }
        let device = device.unwrap_or_default();
        let command =
            ToolCommand::new(exe).args(["/SetAppDefault", &device, "all", &action.target]);
        let ok = self
            .run_tool(
                &action.id,
                &command,
                10,
                stop,
                ("Routing requested", "Routing request failed"),
            )
            .await?;
        Ok(match ok {
            None => DeckResult::fail(CONTROL_FAILED),
            Some(true) => DeckResult::ok(),
            Some(false) => DeckResult::fail("The application routing request failed"),
        })
    }

    /// Runs a tool for a control and keeps the card's text; Some(exit code 0), None when it could not run or timed out
    /// (C#'s catch-all).
    async fn run_tool(
        &self,
        id: &str,
        command: &ToolCommand,
        seconds: u64,
        stop: &Stop,
        (worked, failed): (&str, &str),
    ) -> Result<Option<bool>, Cancelled> {
        match crate::tool_process::run(command, Duration::from_secs(seconds), stop.stopped()).await
        {
            Err(RunError::Cancelled) => Err(Cancelled),
            Err(_) => Ok(None),
            Ok(output) => {
                let ok = output.code == 0;
                lock(&self.action_results).insert(
                    id.to_string(),
                    (if ok { worked } else { failed }).to_string(),
                );
                Ok(Some(ok))
            }
        }
    }

    /// A Voicemeeter parameter's value for the card (`0.##`), or None when it cannot be read.
    pub(super) fn mixer_value(&self, action: &DeckAction) -> Option<String> {
        if !mixer_parameter(&action.target) || !self.connect_mixer() {
            return None;
        }
        self.read_mixer(&action.target)
            .filter(|v| v.is_finite())
            .map(|v| crate::dotnet_text::format_custom(f64::from(v), 2, false))
    }

    /// Sets a Voicemeeter parameter: a gain from -60 to 12 dB, anything else 0 or 1.
    pub(super) fn mixer_action(&self, action: &DeckAction, value: i32) -> DeckResult {
        if !mixer_parameter(&action.target) || !self.connect_mixer() {
            return DeckResult::fail(MIXER_UNSET);
        }
        let level = if action.target.ends_with(".Gain") {
            value.clamp(-60, 12)
        } else {
            value.clamp(0, 1)
        };
        if self.set_mixer(&action.target, level as f32) {
            DeckResult::ok()
        } else {
            DeckResult::fail("Voicemeeter refused the parameter")
        }
    }

    /// The Voicemeeter Remote DLL, logged in with its parameters readable (Windows; loaded with the Windows extras).
    fn connect_mixer(&self) -> bool {
        false
    }

    fn read_mixer(&self, _parameter: &str) -> Option<f32> {
        None
    }

    fn set_mixer(&self, _parameter: &str, _value: f32) -> bool {
        false
    }
}

/// Whether SoundVolumeView is configured and there.
fn mixer_executable(c: &AgentConfig) -> bool {
    c.controls
        .mixer_executable
        .as_deref()
        .is_some_and(|exe| !exe.trim().is_empty() && std::path::Path::new(exe).is_file())
}
