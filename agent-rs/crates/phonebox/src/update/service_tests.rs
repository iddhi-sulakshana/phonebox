use std::sync::Mutex as StdMutex;

use super::*;
use crate::update::github::FetchError;

/// Answers by URL; records every request's URL and token.
#[derive(Default)]
pub(crate) struct FakeHttp {
    pub answers: StdMutex<HashMap<String, Result<String, FetchError>>>,
    pub calls: StdMutex<Vec<(String, Option<String>)>>,
}

impl FakeHttp {
    pub fn answer(&self, url: &str, answer: Result<String, FetchError>) {
        self.answers.lock().unwrap().insert(url.into(), answer);
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
        let answer = self.answers.lock().unwrap().get(url).cloned();
        Box::pin(async move {
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

pub(crate) struct Fixture {
    pub _dir: tempfile::TempDir,
    pub state: crate::server::AppState,
    pub http: Arc<FakeHttp>,
    pub tokens: Arc<std::sync::atomic::AtomicUsize>,
}

pub(crate) fn fixture(
    agent_version: &str,
    gh: Option<&'static str>,
) -> (Fixture, Arc<UpdateService>) {
    let (dir, state) = crate::server::guard::tests::test_state();
    let http = Arc::new(FakeHttp::default());
    let tokens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = tokens.clone();
    let deps = UpdateDeps {
        http: http.clone(),
        gh_token: Arc::new(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { gh.map(String::from) })
        }),
        downloads: dir.path().join("updates"),
        api_base: API.into(),
        agent_version: agent_version.into(),
        rid: "linux-x64",
        now: Arc::new(|| 1_794_000_000_000),
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
