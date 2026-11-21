use std::sync::Mutex as StdMutex;

use super::*;
use crate::update::github::FetchError;

pub(crate) type DownloadHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Answers by URL; records every request's URL and token.
#[derive(Default)]
pub(crate) struct FakeHttp {
    pub answers: StdMutex<HashMap<String, Result<String, FetchError>>>,
    pub files: StdMutex<HashMap<String, Vec<u8>>>,
    pub calls: StdMutex<Vec<(String, Option<String>)>>,
    /// Called with each download's link before it is answered.
    pub on_download: StdMutex<Option<DownloadHook>>,
}

impl FakeHttp {
    pub fn answer(&self, url: &str, answer: Result<String, FetchError>) {
        self.answers.lock().unwrap().insert(url.into(), answer);
    }
    pub fn file(&self, url: &str, bytes: Vec<u8>) {
        self.files.lock().unwrap().insert(url.into(), bytes);
    }
    pub fn calls(&self) -> Vec<(String, Option<String>)> {
        self.calls.lock().unwrap().clone()
    }
}

impl UpdateHttp for FakeHttp {
    fn get_text<'a>(
        &'a self,
        url: &'a str,
        token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, FetchError>> {
        self.calls
            .lock()
            .unwrap()
            .push((url.into(), token.map(String::from)));
        let answer = self
            .answers
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .unwrap_or_else(|| {
                Err(FetchError::Http(
                    "Response status code does not indicate success: 404 (Not Found).".into(),
                ))
            });
        Box::pin(async move { answer })
    }

    fn download<'a>(
        &'a self,
        url: &'a str,
        _accept: Option<&'a str>,
        file: &'a std::path::Path,
        token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), FetchError>> {
        self.calls
            .lock()
            .unwrap()
            .push((url.into(), token.map(String::from)));
        if let Some(hook) = self.on_download.lock().unwrap().clone() {
            hook(url);
        }
        let bytes = self.files.lock().unwrap().get(url).cloned();
        let answer = self.answers.lock().unwrap().get(url).cloned();
        Box::pin(async move {
            if let Some(bytes) = bytes {
                return std::fs::write(file, bytes).map_err(|e| FetchError::Other(e.to_string()));
            }
            match answer {
                Some(Ok(body)) => {
                    std::fs::write(file, body).map_err(|e| FetchError::Other(e.to_string()))
                }
                Some(Err(e)) => Err(e),
                None => Err(FetchError::Http(
                    "Response status code does not indicate success: 404 (Not Found).".into(),
                )),
            }
        })
    }
}

pub(crate) const API: &str = "https://api.github.com";
pub(crate) const REPO: &str = "iddhi-sulakshana/phonebox";

pub(crate) fn release(tag: &str, draft: bool, rid: &str) -> Value {
    let v = tag.trim_start_matches('v');
    let link = |name: String| {
        serde_json::json!({
            "name": name,
            "browser_download_url": format!("https://github.com/{REPO}/releases/download/{tag}/{name}"),
            "url": format!("{API}/repos/{REPO}/releases/assets/{}", name.len()),
        })
    };
    serde_json::json!({
        "tag_name": tag,
        "draft": draft,
        "prerelease": tag.contains('-'),
        "html_url": format!("https://github.com/{REPO}/releases/tag/{tag}"),
        "assets": [
            link(format!("PhoneBoxAgent-{v}-{rid}.zip")),
            link(format!("PhoneBox-{v}.apk")),
            link(format!("SHA256SUMS-{v}.txt")),
        ],
    })
}

/// `adb`: "Failure [LOCKED]" on install when the mode is "fail" or names the phone, else "Success".
#[derive(Default)]
pub(crate) struct FakeInstaller {
    pub mode: StdMutex<String>,
    pub calls: StdMutex<Vec<Vec<String>>>,
}

impl crate::update::phone::PhoneInstaller for FakeInstaller {
    fn locate(&self, _configured: Option<&str>) -> Option<std::path::PathBuf> {
        Some("fake-adb".into())
    }

    fn run<'a>(
        &'a self,
        _adb: &'a std::path::Path,
        args: Vec<String>,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.calls.lock().unwrap().push(args.clone());
        let mode = self.mode.lock().unwrap().clone();
        Box::pin(async move {
            if args.get(2).map(String::as_str) == Some("install") {
                if mode == "fail" || args.get(1) == Some(&mode) {
                    return Ok("Failure [LOCKED]\n".into());
                }
                return Ok("Success\n".into());
            }
            Ok("ok\n".into())
        })
    }
}

pub(crate) struct Fixture {
    pub _dir: tempfile::TempDir,
    pub state: crate::server::AppState,
    pub http: Arc<FakeHttp>,
    pub tokens: Arc<std::sync::atomic::AtomicUsize>,
    pub adb: Arc<FakeInstaller>,
}

pub(crate) fn fixture(
    agent_version: &str,
    gh: Option<&'static str>,
) -> (Fixture, Arc<UpdateService>) {
    let (dir, state) = crate::server::guard::tests::test_state();
    let http = Arc::new(FakeHttp::default());
    let adb = Arc::new(FakeInstaller::default());
    let tokens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = tokens.clone();
    let deps = UpdateDeps {
        http: http.clone(),
        installer: adb.clone(),
        gh_token: Arc::new(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { gh.map(String::from) })
        }),
        downloads: dir.path().join("updates"),
        api_base: API.into(),
        agent_version: agent_version.into(),
        rid: "linux-x64",
        now: Arc::new(|| 1_794_000_000_000),
        exe: dir.path().join("app").join(crate::update::swap::EXE_NAME),
        install_dir: dir.path().join("app"),
    };
    let service = UpdateService::new(
        state.cfg.clone(),
        state.adb.clone(),
        state.phone.clone(),
        state.stop.clone(),
        deps,
    );
    (
        Fixture {
            _dir: dir,
            state,
            http,
            tokens,
            adb,
        },
        service,
    )
}

#[tokio::test]
async fn pre_release_agents_take_the_highest_published_release() {
    let (f, service) = fixture("1.0.0-alpha.2", None);
    let list = serde_json::json!([
        release("v1.0.0-alpha.9", true, "linux-x64"),
        release("v1.0.0-alpha.3", false, "linux-x64"),
        release("v1.0.0-alpha.2", false, "linux-x64"),
        release("v0.9.23", false, "linux-x64"),
        { "tag_name": "nightly", "draft": false, "html_url": "x", "assets": [] },
    ]);
    let url = format!("{API}/repos/{REPO}/releases?per_page=30");
    f.http.answer(&url, Ok(list.to_string()));
    service.check().await.unwrap();
    let s = service.status();
    assert_eq!(s.latest.as_deref(), Some("1.0.0-alpha.3"));
    assert_eq!(
        s.release_url.as_deref(),
        Some("https://github.com/iddhi-sulakshana/phonebox/releases/tag/v1.0.0-alpha.3")
    );
    assert!(s.agent_update);
    assert_eq!(
        (s.state.as_str(), s.error.as_deref(), s.last_check),
        ("idle", None, Some(1_794_000_000_000))
    );
    assert_eq!(f.http.calls(), vec![(url, None)]);
}

#[tokio::test]
async fn stable_agents_ask_for_the_latest_release_only() {
    let (f, service) = fixture("1.0.0", None);
    let url = format!("{API}/repos/{REPO}/releases/latest");
    f.http
        .answer(&url, Ok(release("v1.0.1", false, "linux-x64").to_string()));
    service.check().await.unwrap();
    assert_eq!(service.status().latest.as_deref(), Some("1.0.1"));
    assert_eq!(f.http.calls(), vec![(url, None)]);
}

#[tokio::test]
async fn a_failed_check_keeps_the_release_found_before_and_says_why() {
    let (f, service) = fixture("1.0.0", None);
    let url = format!("{API}/repos/{REPO}/releases/latest");
    f.http
        .answer(&url, Ok(release("v1.0.1", false, "linux-x64").to_string()));
    service.check().await.unwrap();
    f.http.answer(
        &url,
        Err(FetchError::Http(
            "Response status code does not indicate success: 403 (Forbidden).".into(),
        )),
    );
    service.check().await.unwrap();
    let s = service.status();
    assert_eq!(s.latest.as_deref(), Some("1.0.1"));
    assert_eq!(
        s.error.as_deref(),
        Some(
            "Update check failed: Response status code does not indicate success: 403 (Forbidden)."
        )
    );
    // no token from gh: one try only; a JSON object without assets fails as .NET did
    assert_eq!(f.tokens.load(std::sync::atomic::Ordering::SeqCst), 1);
    f.http.answer(&url, Ok(r#"{"tag_name":"v2.0.0"}"#.into()));
    service.check().await.unwrap();
    assert_eq!(
        service.status().error.as_deref(),
        Some("Update check failed: The given key was not present in the dictionary.")
    );
}

#[tokio::test]
async fn a_failed_anonymous_request_is_retried_with_the_gh_token_on_the_api_only() {
    let (f, service) = fixture("1.0.0", Some("gho_secret"));
    let url = format!("{API}/repos/{REPO}/releases/latest");
    f.http.answer(
        &url,
        Err(FetchError::Http(
            "Response status code does not indicate success: 403 (Forbidden).".into(),
        )),
    );
    service.check().await.unwrap();
    assert_eq!(
        f.http.calls(),
        vec![(url.clone(), None), (url, Some("gho_secret".to_string()))]
    );
    // not the API (a local test server): the retry goes without the token
    let (f, mut service) = fixture("1.0.0", Some("gho_secret"));
    Arc::get_mut(&mut service).unwrap().deps.api_base = "http://127.0.0.1:9".into();
    let local = format!("http://127.0.0.1:9/repos/{REPO}/releases/latest");
    service.check().await.unwrap();
    assert_eq!(f.http.calls(), vec![(local.clone(), None), (local, None)]);
    // a failure not worth another try (a broken body) is not retried
    let (f, service) = fixture("1.0.0", Some("gho_secret"));
    f.http.answer(
        &format!("{API}/repos/{REPO}/releases/latest"),
        Err(FetchError::Other("disk".into())),
    );
    service.check().await.unwrap();
    assert_eq!(f.http.calls().len(), 1);
}

#[tokio::test]
async fn the_status_keeps_c_sharps_order_and_the_phone_app_compares_with_the_release() {
    let (f, service) = fixture("1.0.0-alpha.1", None);
    *f.state.phone.app_version.lock().unwrap() = Some("0.9.23".into());
    f.http.answer(
        &format!("{API}/repos/{REPO}/releases?per_page=30"),
        Ok(serde_json::json!([release("v1.0.0-alpha.1", false, "osx-arm64")]).to_string()),
    );
    service.check().await.unwrap();
    let json = serde_json::to_string(&service.status()).unwrap();
    assert_eq!(
        json,
        r#"{"agentVersion":"1.0.0-alpha.1","appVersion":"0.9.23","latest":"1.0.0-alpha.1","releaseUrl":"https://github.com/iddhi-sulakshana/phonebox/releases/tag/v1.0.0-alpha.1","agentUpdate":false,"appUpdate":true,"lastCheck":1794000000000,"state":"idle","error":null}"#
    );
}

#[test]
fn the_state_reads_null_until_the_service_exists() {
    use crate::state::StateSource;
    assert_eq!(UpdateSlot::default().snapshot(), Value::Null);
}

// ---------------------------------------------------------------- installing the agent

fn zip_with(files: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        for (name, body) in files {
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o644);
            writer.start_file(*name, options).unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap();
    }
    buffer.into_inner()
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const ZIP_URL: &str = "https://github.com/iddhi-sulakshana/phonebox/releases/download/v1.0.0-alpha.3/PhoneBoxAgent-1.0.0-alpha.3-linux-x64.zip";
const SUMS_URL: &str = "https://github.com/iddhi-sulakshana/phonebox/releases/download/v1.0.0-alpha.3/SHA256SUMS-1.0.0-alpha.3.txt";

/// An alpha.2 agent installed in the fixture's install folder, alpha.3 out with `zip` (listed with `listed_hash`).
async fn updatable(zip: Vec<u8>, listed_hash: Option<String>) -> (Fixture, Arc<UpdateService>) {
    let (f, service) = fixture("1.0.0-alpha.2", None);
    let app = f._dir.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join(crate::update::swap::EXE_NAME), "old").unwrap();
    f.http.answer(
        &format!("{API}/repos/{REPO}/releases?per_page=30"),
        Ok(serde_json::json!([release("v1.0.0-alpha.3", false, "linux-x64")]).to_string()),
    );
    let hash = listed_hash.unwrap_or_else(|| sha256(&zip));
    f.http.file(
        SUMS_URL,
        format!("{hash}  PhoneBoxAgent-1.0.0-alpha.3-linux-x64.zip\n").into_bytes(),
    );
    f.http.file(ZIP_URL, zip);
    service.check().await.unwrap();
    assert!(service.status().agent_update);
    (f, service)
}

fn installed(f: &Fixture) -> (String, Option<String>) {
    let exe = f
        ._dir
        .path()
        .join("app")
        .join(crate::update::swap::EXE_NAME);
    let mut old = exe.as_os_str().to_owned();
    old.push(".old");
    (
        std::fs::read_to_string(&exe).unwrap(),
        std::fs::read_to_string(old).ok(),
    )
}

#[tokio::test]
async fn a_bad_checksum_leaves_the_install_untouched() {
    let zip = zip_with(&[(crate::update::swap::EXE_NAME, b"new")]);
    let (f, service) = updatable(zip, Some("0".repeat(64))).await;
    service.apply().await;
    let s = service.status();
    assert_eq!(
        s.error.as_deref(),
        Some(
            "PhoneBoxAgent-1.0.0-alpha.3-linux-x64.zip does not match the release checksum; the update was not installed"
        )
    );
    assert_eq!(s.state, "idle");
    assert_eq!(installed(&f), ("old".to_string(), None));
    assert!(!f.state.stop.is_stopped());
    assert_eq!(
        f.http.calls().iter().filter(|(u, _)| u == ZIP_URL).count(),
        2
    );
}

#[tokio::test]
async fn a_zip_without_the_agent_is_refused() {
    let (f, service) = updatable(zip_with(&[("README.txt", b"hello")]), None).await;
    service.apply().await;
    assert_eq!(
        service.status().error,
        Some(format!(
            "the agent package has no {}",
            crate::update::swap::EXE_NAME
        ))
    );
    assert_eq!(installed(&f), ("old".to_string(), None));
}

#[tokio::test]
async fn a_copy_outside_the_install_folder_never_replaces_itself() {
    let zip = zip_with(&[(crate::update::swap::EXE_NAME, b"new")]);
    let (f, mut service) = updatable(zip, None).await;
    Arc::get_mut(&mut service).unwrap().deps.exe =
        f._dir.path().join("elsewhere").join("PhoneBoxAgent");
    let before = f.http.calls().len();
    service.apply().await;
    assert_eq!(f.http.calls().len(), before);
    assert_eq!(service.status().error, None);
    assert_eq!(installed(&f), ("old".to_string(), None));
}

#[cfg(unix)]
#[tokio::test]
async fn a_staged_binary_that_does_not_start_is_never_swapped_in() {
    let zip = zip_with(&[(crate::update::swap::EXE_NAME, b"#!/bin/sh\nexit 1\n")]);
    let (f, service) = updatable(zip, None).await;
    service.apply().await;
    let s = service.status();
    let error = s.error.unwrap();
    assert!(
        error.starts_with("the new agent does not start on this computer (")
            && error.ends_with("); the update was not installed"),
        "{error}"
    );
    assert_eq!(s.state, "idle");
    assert_eq!(installed(&f), ("old".to_string(), None));
    assert!(!f.state.stop.is_stopped());
}

#[cfg(unix)]
#[tokio::test]
async fn the_swap_keeps_the_old_binary_as_old() {
    use std::os::unix::fs::PermissionsExt;
    let script = b"#!/bin/sh\nexit 0\n";
    let zip = zip_with(&[
        (crate::update::swap::EXE_NAME, script),
        ("THIRD-PARTY-NOTICES.txt", b"notices"),
    ]);
    let (f, service) = updatable(zip, None).await;
    service.apply().await;
    let s = service.status();
    assert_eq!((s.state.as_str(), s.error.as_deref()), ("restarting", None));
    assert_eq!(
        installed(&f),
        (
            String::from_utf8(script.to_vec()).unwrap(),
            Some("old".to_string())
        )
    );
    let app = f._dir.path().join("app");
    assert_eq!(
        std::fs::read_to_string(app.join("THIRD-PARTY-NOTICES.txt")).unwrap(),
        "notices"
    );
    let mode = std::fs::metadata(app.join(crate::update::swap::EXE_NAME))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(f.state.stop.is_stopped());
    assert!(crate::app_restart::requested());
    // the old process still sees the update pending: another apply does nothing
    service.apply().await;
    assert_eq!(service.status().state, "restarting");
}

// ---------------------------------------------------------------- the phone app

/// A release of `version` with only the phone app and its checksum (no agent zip).
fn app_release(f: &Fixture, version: &str) {
    let apk = b"apk-v-test".to_vec();
    let base = format!("https://example.invalid/{version}");
    let release = serde_json::json!({
        "tag_name": format!("v{version}"),
        "html_url": "https://example.invalid/release",
        "assets": [
            { "name": format!("PhoneBox-{version}.apk"), "browser_download_url": format!("{base}/PhoneBox-{version}.apk") },
            { "name": format!("SHA256SUMS-{version}.txt"), "browser_download_url": format!("{base}/SHA256SUMS-{version}.txt") },
        ],
    });
    f.http.answer(
        &format!("{API}/repos/{REPO}/releases?per_page=30"),
        Ok(serde_json::json!([release]).to_string()),
    );
    f.http.answer(
        &format!("{API}/repos/{REPO}/releases/latest"),
        Ok(release.to_string()),
    );
    f.http.file(
        &format!("{base}/SHA256SUMS-{version}.txt"),
        format!("{}  PhoneBox-{version}.apk\n", sha256(&apk)).into_bytes(),
    );
    f.http.file(&format!("{base}/PhoneBox-{version}.apk"), apk);
}

fn set_app(f: &Fixture, version: &str) {
    *f.state.phone.app_version.lock().unwrap() = Some(version.into());
}

#[tokio::test]
async fn apply_clears_error_after_retry_success_and_zero_targets_preserve_app_version() {
    let ver = crate::app_paths::version();
    // a phone that refuses, then takes the update
    let (f, svc1) = fixture(ver, None);
    app_release(&f, ver);
    set_app(&f, "0.0.1");
    f.state.adb.set_seen_for_test(&["SER1"], &[]);
    *f.adb.mode.lock().unwrap() = "fail".into();
    svc1.check().await.unwrap();
    svc1.apply().await;
    assert!(svc1.status().error.is_some());
    assert!(svc1.status().app_update);
    *f.adb.mode.lock().unwrap() = "ok".into();
    svc1.apply().await;
    let s = svc1.status();
    assert_eq!(
        (s.error, s.app_update, s.app_version.as_deref()),
        (None, false, Some(ver))
    );

    // the phone left while the app downloaded: nothing installed, the reported version stays
    let (f, svc2) = fixture(ver, None);
    app_release(&f, ver);
    set_app(&f, "0.0.1");
    f.state.adb.set_seen_for_test(&["SER1"], &[]);
    let adb = f.state.adb.clone();
    *f.http.on_download.lock().unwrap() = Some(Arc::new(move |url: &str| {
        if url.ends_with(".apk") {
            adb.set_seen_for_test(&[], &[]);
        }
    }));
    *f.adb.mode.lock().unwrap() = "ok".into();
    svc2.check().await.unwrap();
    svc2.apply().await;
    assert_eq!(svc2.status().app_version.as_deref(), Some("0.0.1"));
    assert!(svc2.status().app_update);

    // one of two phones fails; it stays pending after the other reports the new app and it is unplugged
    let (f, svc3) = fixture(ver, None);
    app_release(&f, ver);
    set_app(&f, "0.0.1");
    f.state.adb.set_seen_for_test(&["SERA", "SERB"], &[]);
    *f.adb.mode.lock().unwrap() = "SERA".into();
    svc3.check().await.unwrap();
    svc3.apply().await;
    let reason = svc3.status().error.unwrap();
    assert!(reason.contains("SERA"), "{reason}");
    set_app(&f, ver);
    f.state.adb.set_seen_for_test(&["SERB"], &[]);
    svc3.apply().await;
    let s = svc3.status();
    assert!(s.app_update);
    assert_eq!(s.error.as_deref(), Some(reason.as_str()));
}

#[tokio::test]
async fn installs_go_to_the_phones_missing_the_app_with_the_apk_path_as_one_argument() {
    let ver = crate::app_paths::version();
    let (f, service) = fixture(ver, None);
    assert_eq!(
        service.install_missing().await.as_deref(),
        Some("no connected phone is missing the app")
    );
    f.state.adb.set_seen_for_test(&["SER1"], &["SER1", "SER2"]);
    *f.adb.mode.lock().unwrap() = "SER2".into();
    // no release yet: the install checks first
    app_release(&f, ver);
    let error = service.install_missing().await.unwrap();
    assert_eq!(error, "installing on SER2 failed: Failure [LOCKED]");
    let calls = f.adb.calls.lock().unwrap().clone();
    let apk = f
        ._dir
        .path()
        .join("updates")
        .join(format!("PhoneBox-{ver}.apk"));
    assert_eq!(
        calls[0],
        vec![
            "-s".to_string(),
            "SER1".into(),
            "install".into(),
            apk.to_string_lossy().into_owned()
        ]
    );
    assert_eq!(service.status().state, "idle");
    *f.adb.mode.lock().unwrap() = "ok".into();
    assert_eq!(service.install_missing().await, None);
}

#[tokio::test]
async fn the_state_shows_only_an_available_update() {
    use crate::state::StateSource;
    let (f, service) = fixture("1.0.0-alpha.1", None);
    let slot = UpdateSlot::default();
    slot.set(service.clone());
    assert_eq!(slot.snapshot(), Value::Null);
    f.http.answer(
        &format!("{API}/repos/{REPO}/releases?per_page=30"),
        Ok(serde_json::json!([release("v1.0.0-alpha.2", false, "linux-x64")]).to_string()),
    );
    service.check().await.unwrap();
    assert_eq!(
        slot.snapshot(),
        serde_json::json!({ "latest": "1.0.0-alpha.2", "app": false, "agent": true })
    );
}
