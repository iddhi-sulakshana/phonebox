//! `ApplyAsync` and the downloads behind it (`UpdateService.cs:133-176, 259-319, 488-519`).

use std::path::PathBuf;

use super::assets::{Release, asset_name, expected_sha256, hash_matches};
use super::github::transient;
use super::service::UpdateService;
use super::swap;

const CATEGORY: &str = "UpdateService";

impl UpdateService {
    /// `DownloadAsync`: into the downloads folder under `name` (an existing file there is used as it is); a link that
    /// fails is tried through the GitHub API, then once more with the `gh` CLI's token.
    pub(super) async fn download(&self, url: &str, name: &str) -> Result<PathBuf, String> {
        let dir = &self.deps.downloads;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let path = dir.join(name);
        if path.exists() {
            return Ok(path);
        }
        let part = dir.join(format!("{name}.part"));
        let http = &self.deps.http;
        let stopped = || self.stop.is_stopped();
        match http.download(url, None, &part, None).await {
            Ok(()) => {}
            Err(e) => {
                let api = self.lock().api_urls.get(url).cloned();
                let (true, Some(api)) = (transient(&e, stopped()), api) else {
                    return Err(e.message().to_string());
                };
                crate::log::info(
                    CATEGORY,
                    &format!(
                        "download of {name} failed ({}); trying the GitHub API",
                        e.message()
                    ),
                );
                let octets = Some("application/octet-stream");
                if let Err(e) = http.download(&api, octets, &part, None).await {
                    if !transient(&e, stopped()) {
                        return Err(e.message().to_string());
                    }
                    let Some(token) = (self.deps.gh_token)().await else {
                        return Err(e.message().to_string());
                    };
                    let token = super::github::is_github_api(&api).then_some(token.as_str());
                    http.download(&api, octets, &part, token)
                        .await
                        .map_err(|e| e.message().to_string())?;
                }
            }
        }
        std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// `VerifiedDownloadAsync`: a download the release's `SHA256SUMS` vouches for (each tried twice); a release without
    /// one installs with a warning, as C# did.
    pub(super) async fn verified_download(
        &self,
        release: &Release,
        url: &str,
        name: &str,
    ) -> Result<PathBuf, String> {
        let Some(sums_url) = release.sums.as_deref() else {
            crate::log::warn(
                CATEGORY,
                &format!(
                    "release {} has no SHA256SUMS file; {name} is installed without a checksum check",
                    release.version
                ),
            );
            return self.download(url, name).await;
        };
        let asset = asset_name(url).unwrap_or_default();
        let sums_name = asset_name(sums_url).unwrap_or_default();
        let mut expected = None;
        for _ in 0..2 {
            let sums_path = self.download(sums_url, &sums_name).await?;
            let text = std::fs::read(&sums_path)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            expected = expected_sha256(&text, &asset);
            if expected.is_some() {
                break;
            }
            // an error page or an older listing: fetch it again
            let _ = std::fs::remove_file(&sums_path);
        }
        let Some(expected) = expected else {
            return Err(format!(
                "{asset} is not listed in {sums_name}; the update was not installed"
            ));
        };
        for _ in 0..2 {
            let path = self.download(url, name).await?;
            if hash_matches(&path, &expected) {
                return Ok(path);
            }
            let _ = std::fs::remove_file(&path);
            crate::log::warn(
                CATEGORY,
                &format!("{asset} does not match the release checksum; downloading it again"),
            );
        }
        Err(format!(
            "{asset} does not match the release checksum; the update was not installed"
        ))
    }

    /// `UpdateAgentAsync`: download, check, unpack, make sure it starts, swap, then restart.
    async fn update_agent(&self, release: &Release) -> Result<(), String> {
        self.lock().state = "updating agent".into();
        let url = release.agent_zip.clone().unwrap_or_default();
        let zip = self
            .verified_download(
                release,
                &url,
                &format!("PhoneBoxAgent-{}.zip", release.version),
            )
            .await?;
        let staging = self
            .deps
            .downloads
            .join(format!("agent-{}", release.version));
        let install_dir = self.deps.install_dir.clone();
        tokio::task::spawn_blocking(move || {
            let exe = swap::stage(&zip, &staging)?;
            swap::self_test(&exe)?;
            swap::replace(&staging, &install_dir)
        })
        .await
        .map_err(|e| e.to_string())??;
        crate::log::info(
            CATEGORY,
            &format!("agent updated to {}, restarting", release.version),
        );
        self.lock().state = "restarting".into();
        crate::app_restart::request();
        self.stop.stop();
        Ok(())
    }

    /// `UpdatePhoneAsync`: the new app on every phone in use (or, when the phones already report it, on those whose
    /// update failed before); the phones that fail stay pending for the next round.
    async fn update_phone(&self, release: &Release) -> Result<(), String> {
        self.lock().state = "updating phone app".into();
        let app_current = self
            .app_version()
            .is_some_and(|a| !super::version::newer(&release.version, &a));
        let url = release.apk.clone().unwrap_or_default();
        let apk = self
            .verified_download(release, &url, &format!("PhoneBox-{}.apk", release.version))
            .await?;
        let configured = self.cfg.get().adb_path.clone();
        let installer = &self.deps.installer;
        let adb = installer
            .locate(configured.as_deref())
            .ok_or("adb not found")?;
        let connected = self.adb.devices();
        let retry = {
            let inner = self.lock();
            if inner.phone_retry_for.as_deref() == Some(release.version.as_str()) {
                inner.phone_retry.clone()
            } else {
                vec![]
            }
        };
        let targets = super::phone::phone_targets(&connected, app_current, &retry);
        let apk_text = apk.to_string_lossy().into_owned();
        let mut failed: Vec<(String, String)> = Vec::new();
        for serial in &targets {
            let args = ["-s", serial, "install", "-r", &apk_text]
                .map(String::from)
                .to_vec();
            match installer.run(&adb, args).await {
                // a phone that hangs (the time limit) must not cost the others their update
                Err(e) => failed.push((serial.clone(), e)),
                Ok(output) if !output.contains("Success") => {
                    failed.push((serial.clone(), super::phone::last_line(&output).to_string()))
                }
                Ok(_) => {
                    crate::log::info(
                        CATEGORY,
                        &format!("phone app on {serial} updated to {}", release.version),
                    );
                    let panel = [
                        "-s",
                        serial,
                        "shell",
                        "am",
                        "start",
                        "-n",
                        crate::adb::PANEL_ACTIVITY,
                    ]
                    .map(String::from)
                    .to_vec();
                    if let Err(e) = installer.run(&adb, panel).await {
                        crate::log::info(
                            CATEGORY,
                            &format!("{serial}: could not open the panel after the update ({e})"),
                        );
                    }
                }
            }
        }
        let failed_serials: Vec<String> = failed.iter().map(|(s, _)| s.clone()).collect();
        {
            let mut inner = self.lock();
            inner.phone_retry = super::phone::next_phone_retry(&retry, &targets, &failed_serials);
            inner.phone_retry_for = Some(release.version.clone());
        }
        if !failed.is_empty() {
            let why: Vec<String> = failed.iter().map(|(s, w)| format!("{s}: {w}")).collect();
            return Err(format!("phone update failed on {}", why.join("; ")));
        }
        if !targets.is_empty() {
            *self
                .phone
                .app_version
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(release.version.clone());
        }
        Ok(())
    }

    /// `InstallAppAsync` (Settings › Connection › Install the phone app): the app on the phones that lack it; None when
    /// it worked, else why not.
    pub async fn install_missing(&self) -> Option<String> {
        let targets = self.adb.missing_app();
        if targets.is_empty() {
            return Some("no connected phone is missing the app".into());
        }
        let Ok(_gate) = self.gate.try_lock() else {
            return Some("an update is already running".into());
        };
        self.lock().state = "installing the phone app".into();
        let result: Result<Option<String>, String> = async {
            if self.lock().latest.is_none() {
                self.check_core()
                    .await
                    .map_err(|_| "The operation was canceled.".to_string())?;
                self.lock().state = "installing the phone app".into();
            }
            let Some(release) = self.lock().latest.clone().filter(|r| r.apk.is_some()) else {
                return Ok(Some(
                    "could not find the phone app in the GitHub releases".into(),
                ));
            };
            let url = release.apk.clone().unwrap_or_default();
            let apk = self
                .verified_download(&release, &url, &format!("PhoneBox-{}.apk", release.version))
                .await?;
            let configured = self.cfg.get().adb_path.clone();
            let installer = &self.deps.installer;
            let adb = installer
                .locate(configured.as_deref())
                .ok_or("adb not found")?;
            let apk_text = apk.to_string_lossy().into_owned();
            let mut failed = Vec::new();
            for serial in &targets {
                let args = ["-s", serial, "install", &apk_text]
                    .map(String::from)
                    .to_vec();
                match installer.run(&adb, args).await {
                    Err(e) => failed.push(format!("installing on {serial} failed: {e}")),
                    Ok(output) if !output.contains("Success") => failed.push(format!(
                        "installing on {serial} failed: {}",
                        super::phone::last_line(&output)
                    )),
                    Ok(_) => crate::log::info(
                        CATEGORY,
                        &format!("phone app {} installed on {serial}", release.version),
                    ),
                }
            }
            Ok((!failed.is_empty()).then(|| failed.join("; ")))
        }
        .await;
        self.lock().state = "idle".into();
        result.unwrap_or_else(Some)
    }

    /// `ApplyAsync`: the phone app and this agent when newer ones are out; a busy gate (a check, an install, another
    /// apply) makes it a no-op. Only the copy in the install folder replaces itself.
    pub async fn apply(&self) {
        let status = self.status();
        if !status.app_update && !status.agent_update {
            return;
        }
        let Ok(_gate) = self.gate.try_lock() else {
            return;
        };
        if self.lock().state == "restarting" {
            return;
        }
        let release = self.lock().latest.clone();
        let result: Result<(), String> = async {
            let Some(release) = release else {
                return Ok(());
            };
            let mut attempted = false;
            let mut failed = false;
            // a phone that refuses the app must not keep the agent itself from updating
            if status.app_update && self.adb.connected() {
                attempted = true;
                if let Err(message) = self.update_phone(&release).await {
                    failed = true;
                    let mut inner = self.lock();
                    if super::phone::log_once(&mut inner.phone_warning, &message) {
                        crate::log::warn(CATEGORY, &format!("phone update failed: {message}"));
                    }
                    inner.error = Some(message);
                }
            }
            if status.agent_update && self.installed() {
                attempted = true;
                self.update_agent(&release).await?;
            }
            let mut inner = self.lock();
            if attempted && !failed && !inner.phone_retry_pending() {
                inner.error = None;
            }
            Ok(())
        }
        .await;
        let mut inner = self.lock();
        if let Err(message) = result {
            crate::log::warn(CATEGORY, &format!("update failed: {message}"));
            inner.error = Some(message);
        }
        if inner.state != "restarting" {
            inner.state = "idle".into();
        }
    }
}
