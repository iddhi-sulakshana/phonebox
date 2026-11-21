//! GitHub for the updater (`UpdateService.cs:290-367`): one client, anonymous first, the `gh` CLI's token only on a
//! retry and only to `https://api.github.com`.

use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use tokio::io::AsyncWriteExt;

/// How a request failed, as `UpdateService.Transient` sorts it.
#[derive(Debug, Clone, PartialEq)]
pub enum FetchError {
    /// An HTTP status or a connection failure (`HttpRequestException`).
    Http(String),
    /// The client's own time limit, or our cancellation (`TaskCanceledException`).
    Canceled(String),
    /// Anything else (a body that broke off, a file that could not be written).
    Other(String),
}

impl FetchError {
    pub fn message(&self) -> &str {
        match self {
            FetchError::Http(m) | FetchError::Canceled(m) | FetchError::Other(m) => m,
        }
    }
}

/// `Transient`: worth another try through the API or with a token. Our own cancellation is not.
pub fn transient(err: &FetchError, our_cancel_requested: bool) -> bool {
    match err {
        FetchError::Http(_) => true,
        FetchError::Canceled(_) => !our_cancel_requested,
        FetchError::Other(_) => false,
    }
}

/// `IsGitHubApi`: https to `api.github.com` (any case, any port), the only place the token may go.
pub fn is_github_api(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str()
                .is_some_and(|h| h.eq_ignore_ascii_case("api.github.com"))
    })
}

/// The requests the updater makes; a fake in tests.
pub trait UpdateHttp: Send + Sync {
    /// A GET read as text. `token` is sent as `Bearer` only to the GitHub API.
    fn get_text<'a>(
        &'a self,
        url: &'a str,
        token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, FetchError>>;
    /// A GET written to `file` (`accept` replaces the Accept header when given).
    fn download<'a>(
        &'a self,
        url: &'a str,
        accept: Option<&'a str>,
        file: &'a Path,
        token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), FetchError>>;
}

const LIMIT: Duration = Duration::from_secs(300);
const TIMEOUT_TEXT: &str =
    "The request was canceled due to the configured HttpClient.Timeout of 300 seconds elapsing.";

/// The real client: `User-Agent: PhoneBox/<version>`, `Accept: application/vnd.github+json`, 5 minutes per API
/// request and per stalled download chunk (C#'s limit covered only the headers of a download). Redirects keep
/// reqwest's policy, which drops `Authorization` when the host changes (the asset host never sees the token).
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl Default for ReqwestHttp {
    fn default() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
        );
        let client = reqwest::Client::builder()
            .user_agent(format!("PhoneBox/{}", crate::app_paths::version()))
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("the update client");
        ReqwestHttp { client }
    }
}

fn status_text(status: reqwest::StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!(
            "Response status code does not indicate success: {} ({reason}).",
            status.as_u16()
        ),
        None => format!(
            "Response status code does not indicate success: {}.",
            status.as_u16()
        ),
    }
}

fn failed(e: reqwest::Error) -> FetchError {
    if e.is_timeout() {
        FetchError::Canceled(TIMEOUT_TEXT.into())
    } else {
        FetchError::Http(crate::net::error_text(&e))
    }
}

impl ReqwestHttp {
    async fn send(
        &self,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
        limit: Option<Duration>,
    ) -> Result<reqwest::Response, FetchError> {
        let mut request = self.client.get(url);
        if let Some(accept) = accept {
            request = request.header(reqwest::header::ACCEPT, accept);
        }
        if let Some(token) = token.filter(|_| is_github_api(url)) {
            request = request.bearer_auth(token);
        }
        if let Some(limit) = limit {
            request = request.timeout(limit);
        }
        let response = request.send().await.map_err(failed)?;
        if !response.status().is_success() {
            return Err(FetchError::Http(status_text(response.status())));
        }
        Ok(response)
    }
}

impl UpdateHttp for ReqwestHttp {
    fn get_text<'a>(
        &'a self,
        url: &'a str,
        token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, FetchError>> {
        Box::pin(async move {
            let response = self.send(url, None, token, Some(LIMIT)).await?;
            response.text().await.map_err(failed)
        })
    }

    fn download<'a>(
        &'a self,
        url: &'a str,
        accept: Option<&'a str>,
        file: &'a Path,
        token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), FetchError>> {
        Box::pin(async move {
            let headers = tokio::time::timeout(LIMIT, self.send(url, accept, token, None))
                .await
                .map_err(|_| FetchError::Canceled(TIMEOUT_TEXT.into()))??;
            let mut out = tokio::fs::File::create(file)
                .await
                .map_err(|e| FetchError::Other(e.to_string()))?;
            let mut body = headers.bytes_stream();
            loop {
                match tokio::time::timeout(LIMIT, body.next()).await {
                    Err(_) => return Err(FetchError::Other(TIMEOUT_TEXT.into())),
                    Ok(None) => break,
                    Ok(Some(Err(e))) => return Err(FetchError::Other(crate::net::error_text(&e))),
                    Ok(Some(Ok(chunk))) => out
                        .write_all(&chunk)
                        .await
                        .map_err(|e| FetchError::Other(e.to_string()))?,
                }
            }
            out.flush()
                .await
                .map_err(|e| FetchError::Other(e.to_string()))
        })
    }
}

/// `GhTokenAsync`: the `gh` CLI's token (10 s), or None when there is none.
pub async fn gh_token() -> Option<String> {
    let args = ["auth".to_string(), "token".to_string()];
    let output = crate::github_monitor::gh(&args, tokio::time::sleep(Duration::from_secs(10)))
        .await
        .ok()?;
    let token = output.trim();
    (!token.is_empty() && !token.contains(' ')).then(|| token.to_string())
}

/// The API's base address: GitHub's, or a test server on this machine named by `PHONEBOX_UPDATE_API` (the update
/// round trip in `agent-rs/scripts/remote-update-check.sh`); anything else in it is ignored.
pub fn api_base(env: Option<&str>) -> String {
    env.and_then(|v| reqwest::Url::parse(v).ok())
        .filter(|u| u.scheme() == "http" && matches!(u.host_str(), Some("127.0.0.1" | "localhost")))
        .map_or_else(
            || "https://api.github.com".to_string(),
            |u| u.as_str().trim_end_matches('/').to_string(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_only_goes_to_the_git_hub_api() {
        for (url, expected) in [
            (
                "https://api.github.com/repos/ersinkoc/phonebox/releases/latest",
                true,
            ),
            ("https://API.github.com/repos/x/y/releases/assets/1", true),
            ("http://api.github.com/repos/x/y", false),
            ("https://github.com/x/y/releases/download/v1/a.zip", false),
            ("https://api.github.com.evil.example/repos", false),
            ("https://release-assets.githubusercontent.com/x", false),
            ("not a url", false),
        ] {
            assert_eq!(is_github_api(url), expected, "{url}");
        }
    }

    #[test]
    fn timeouts_and_http_errors_are_transient_but_our_cancellation_is_not() {
        assert!(transient(&FetchError::Http("504".into()), false));
        assert!(transient(
            &FetchError::Canceled("HttpClient.Timeout".into()),
            false
        ));
        assert!(!transient(&FetchError::Canceled(String::new()), true));
        assert!(!transient(&FetchError::Other(String::new()), false));
    }

    #[test]
    fn the_api_base_is_github_unless_a_local_test_server_is_named() {
        assert_eq!(api_base(None), "https://api.github.com");
        assert_eq!(
            api_base(Some("http://127.0.0.1:4567/")),
            "http://127.0.0.1:4567"
        );
        assert_eq!(
            api_base(Some("https://evil.example")),
            "https://api.github.com"
        );
        assert_eq!(
            api_base(Some("http://10.0.0.2:80")),
            "https://api.github.com"
        );
    }
}
