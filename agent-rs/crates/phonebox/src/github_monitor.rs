//! Port of `GitHubMonitor.cs`: a pull-request and notification summary through the GitHub CLI (`gh`) every 3 minutes.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::Serialize;

use crate::json_text::Node;
use crate::server::Stop;
use crate::tool_process::{self, RunError, ToolCommand};

const CATEGORY: &str = "PhoneBox.Agent.GitHubMonitor";

const QUERY: &str = "query {
  viewer {
    login
    pullRequests(states: OPEN, first: 8, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes {
        number title isDraft reviewDecision updatedAt
        repository { nameWithOwner }
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
    }
  }
  search(query: \"is:pr is:open archived:false review-requested:@me\", type: ISSUE, first: 8) {
    nodes {
      ... on PullRequest { number title isDraft updatedAt author { login } repository { nameWithOwner } }
    }
  }
}";

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GhPr {
    pub repo: String,
    pub number: i32,
    pub title: String,
    pub author: Option<String>,
    pub draft: bool,
    pub review: Option<String>,
    pub ci: Option<String>,
    pub updated: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GhSnapshot {
    pub user: Option<String>,
    pub notifications: i32,
    pub review_requests: Vec<GhPr>,
    pub my_prs: Vec<GhPr>,
    pub updated_at: Option<i64>,
    pub error: Option<String>,
}

/// The GraphQL answer: the login, the review requests, and my open pull requests.
pub type GhGraphql = (Option<String>, Vec<GhPr>, Vec<GhPr>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhError {
    Cancelled,
    Failed(String),
}

/// Runs `gh` with these arguments: the real CLI, or a fake in tests.
pub type GhRunner =
    Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, Result<String, GhError>> + Send + Sync>;

pub struct GitHubMonitor {
    gh: GhRunner,
    snap: Mutex<Option<GhSnapshot>>,
}

impl GitHubMonitor {
    /// The real `gh` from PATH.
    pub fn new() -> Self {
        Self::with_gh(Arc::new(|args| {
            Box::pin(async move { gh(&args, std::future::pending()).await })
        }))
    }

    pub fn with_gh(gh: GhRunner) -> Self {
        GitHubMonitor {
            gh,
            snap: Mutex::new(None),
        }
    }

    /// None until the first try has finished.
    pub fn snapshot(&self) -> Option<GhSnapshot> {
        self.snap.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// One fetch; a failure keeps the last summary and shows the error.
    pub async fn refresh_once(&self) {
        match self.fetch().await {
            Ok(snap) => *self.snap.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap),
            Err(GhError::Cancelled) => {}
            Err(GhError::Failed(msg)) => {
                crate::log::debug(CATEGORY, &format!("github: {msg}"));
                let mut snap = self.snap.lock().unwrap_or_else(|e| e.into_inner());
                let last = snap.take().unwrap_or(GhSnapshot {
                    user: None,
                    notifications: 0,
                    review_requests: vec![],
                    my_prs: vec![],
                    updated_at: None,
                    error: None,
                });
                *snap = Some(GhSnapshot {
                    error: Some(msg),
                    ..last
                });
            }
        }
    }

    async fn fetch(&self) -> Result<GhSnapshot, GhError> {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let query = format!("query={QUERY}");
        let answer = (self.gh)(args(&["api", "graphql", "-f", &query])).await?;
        let (user, review_requests, my_prs) = parse_graphql(&answer).map_err(GhError::Failed)?;
        let notifications = match (self.gh)(args(&["api", "notifications?per_page=50"])).await {
            Err(GhError::Cancelled) => return Err(GhError::Cancelled),
            Err(_) => 0,
            Ok(text) => match crate::json_text::parse(&text, false) {
                Ok(Node::Array(items)) => i32::try_from(items.len()).unwrap_or(i32::MAX),
                _ => 0,
            },
        };
        Ok(GhSnapshot {
            user,
            notifications,
            review_requests,
            my_prs,
            updated_at: Some(crate::server::now_ms()),
            error: None,
        })
    }

    pub fn start(self: &Arc<Self>, stop: Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = stop.stopped() => return,
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            }
            while !stop.is_stopped() {
                tokio::select! {
                    _ = stop.stopped() => return,
                    _ = me.refresh_once() => {}
                }
                crate::wake::delay(Duration::from_secs(3 * 60), &stop).await;
            }
        })
    }
}

impl Default for GitHubMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// `gh <args>` (20 s): its output, or `gh: <first line of its error>` when it fails.
pub async fn gh(args: &[String], cancel: impl Future<Output = ()>) -> Result<String, GhError> {
    run_gh(&ToolCommand::new("gh").args(args.iter().cloned()), cancel).await
}

pub async fn run_gh(
    cmd: &ToolCommand,
    cancel: impl Future<Output = ()>,
) -> Result<String, GhError> {
    match tool_process::run(cmd, Duration::from_secs(20), cancel).await {
        Ok(out) if out.code != 0 => Err(GhError::Failed(format!(
            "gh: {}",
            out.stderr.trim().split('\n').next().unwrap_or("")
        ))),
        Ok(out) => Ok(out.stdout),
        Err(RunError::Cancelled) => Err(GhError::Cancelled),
        Err(e) => Err(GhError::Failed(e.to_string())),
    }
}

/// The GraphQL answer as (login, review requests, my pull requests); a missing property fails the whole fetch with
/// .NET's message, as `GetProperty` did.
pub fn parse_graphql(json: &str) -> Result<GhGraphql, String> {
    let root = crate::json_text::parse(json, false)?;
    let data = prop(&root, "data")?;
    let viewer = prop(data, "viewer")?;

    let mut mine = Vec::new();
    for n in items(prop(prop(viewer, "pullRequests")?, "nodes")?)? {
        let mut ci = None;
        let commits = items(prop(prop(n, "commits")?, "nodes")?)?;
        if let Some(first) = commits.first()
            && let rollup @ Node::Object(_) = prop(prop(first, "commit")?, "statusCheckRollup")?
        {
            ci = string(prop(rollup, "state")?)?;
        }
        let review = text(n, "reviewDecision")?;
        mine.push(pr(n, None, ci, review)?);
    }

    let mut reviews = Vec::new();
    for n in items(prop(prop(data, "search")?, "nodes")?)? {
        if !has(n, "number")? {
            continue;
        }
        let author = match n.get("author") {
            Some(a @ Node::Object(_)) => text(a, "login")?,
            _ => None,
        };
        reviews.push(pr(n, author, None, None)?);
    }
    Ok((text(viewer, "login")?, reviews, mine))
}

fn pr(
    n: &Node,
    author: Option<String>,
    ci: Option<String>,
    review: Option<String>,
) -> Result<GhPr, String> {
    Ok(GhPr {
        repo: string(prop(prop(n, "repository")?, "nameWithOwner")?)?.unwrap_or_default(),
        number: int32(prop(n, "number")?)?,
        title: text(n, "title")?.unwrap_or_default(),
        author,
        draft: n.get("isDraft") == Some(&Node::Bool(true)),
        review,
        ci,
        updated: text(n, "updatedAt")?
            .and_then(|u| crate::dotnet_text::parse_date_ms(&u, false))
            .unwrap_or(0),
    })
}

/// .NET's `InvalidOperationException` text for a JSON value of the wrong kind.
fn wrong(expected: &str, n: &Node) -> String {
    let kind = match n {
        Node::Null => "Null",
        Node::Bool(true) => "True",
        Node::Bool(false) => "False",
        Node::Number(_) => "Number",
        Node::String(_) => "String",
        Node::Array(_) => "Array",
        Node::Object(_) => "Object",
    };
    format!(
        "The requested operation requires an element of type '{expected}', but the target element has type '{kind}'."
    )
}

/// `GetProperty`.
fn prop<'a>(n: &'a Node, key: &str) -> Result<&'a Node, String> {
    match n {
        Node::Object(_) => n
            .get(key)
            .ok_or_else(|| "The given key was not present in the dictionary.".to_string()),
        _ => Err(wrong("Object", n)),
    }
}

/// `TryGetProperty`, which throws on anything but an object.
fn has(n: &Node, key: &str) -> Result<bool, String> {
    match n {
        Node::Object(_) => Ok(n.get(key).is_some()),
        _ => Err(wrong("Object", n)),
    }
}

/// `Str`: the member's text when it is a string.
fn text(n: &Node, key: &str) -> Result<Option<String>, String> {
    has(n, key)?;
    Ok(n.text(key).map(String::from))
}

/// `EnumerateArray`.
fn items(n: &Node) -> Result<&[Node], String> {
    match n {
        Node::Array(items) => Ok(items),
        _ => Err(wrong("Array", n)),
    }
}

/// `GetString`: null stays null.
fn string(n: &Node) -> Result<Option<String>, String> {
    match n {
        Node::String(s) => Ok(Some(s.clone())),
        Node::Null => Ok(None),
        _ => Err(wrong("String", n)),
    }
}

/// `GetInt32`.
fn int32(n: &Node) -> Result<i32, String> {
    match n {
        Node::Number(t) => t
            .parse()
            .map_err(|_| "One of the identified items was in an invalid format.".to_string()),
        _ => Err(wrong("Number", n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRAPHQL: &str = r#"{"data":{
      "viewer":{"login":"octo","pullRequests":{"nodes":[
        {"number":12,"title":"Fix the thing","isDraft":false,"reviewDecision":"APPROVED","updatedAt":"2026-11-20T10:00:00Z",
         "repository":{"nameWithOwner":"me/app"},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}},
        {"number":13,"title":"Draft","isDraft":true,"reviewDecision":null,"updatedAt":"not a date",
         "repository":{"nameWithOwner":"me/app"},"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}]}},
      "search":{"nodes":[
        {"number":7,"title":"Please review","isDraft":false,"updatedAt":"2026-11-19T09:00:00Z","author":{"login":"friend"},
         "repository":{"nameWithOwner":"team/lib"}},
        {}]}}}"#;

    #[test]
    fn github_graphql_parses_mine_and_review_requests() {
        let (user, reviews, mine) = parse_graphql(GRAPHQL).unwrap();
        assert_eq!(user.as_deref(), Some("octo"));
        assert_eq!(
            mine,
            [
                GhPr {
                    repo: "me/app".into(),
                    number: 12,
                    title: "Fix the thing".into(),
                    author: None,
                    draft: false,
                    review: Some("APPROVED".into()),
                    ci: Some("SUCCESS".into()),
                    updated: crate::dotnet_text::parse_date_ms("2026-11-20T10:00:00Z", false)
                        .unwrap(),
                },
                GhPr {
                    repo: "me/app".into(),
                    number: 13,
                    title: "Draft".into(),
                    author: None,
                    draft: true,
                    review: None,
                    ci: None,
                    updated: 0,
                },
            ]
        );
        assert_eq!(
            reviews.len(),
            1,
            "a search node that is not a pull request has no number"
        );
        assert_eq!(
            (reviews[0].author.as_deref(), reviews[0].number),
            (Some("friend"), 7)
        );
        assert_eq!(
            (reviews[0].review.as_deref(), reviews[0].ci.as_deref()),
            (None, None)
        );

        assert_eq!(
            parse_graphql(r#"{"errors":[]}"#),
            Err("The given key was not present in the dictionary.".into())
        );
        assert!(parse_graphql("not json").is_err());
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_last_snapshot_with_the_error() {
        let fail = Arc::new(Mutex::new(false));
        let failing = fail.clone();
        let monitor = GitHubMonitor::with_gh(Arc::new(move |args: Vec<String>| {
            let failing = *failing.lock().unwrap();
            Box::pin(async move {
                if failing {
                    return Err(GhError::Failed("gh: not logged in".into()));
                }
                Ok(if args[1] == "graphql" {
                    GRAPHQL.to_string()
                } else {
                    "[{},{},{}]".to_string()
                })
            })
        }));
        assert_eq!(monitor.snapshot(), None);
        monitor.refresh_once().await;
        let first = monitor.snapshot().unwrap();
        assert_eq!(
            (first.notifications, first.my_prs.len(), first.error.clone()),
            (3, 2, None)
        );
        assert!(first.updated_at.is_some());

        *fail.lock().unwrap() = true;
        monitor.refresh_once().await;
        let kept = monitor.snapshot().unwrap();
        assert_eq!(kept.error.as_deref(), Some("gh: not logged in"));
        assert_eq!(
            GhSnapshot {
                error: None,
                ..kept
            },
            first
        );

        let fresh = GitHubMonitor::with_gh(Arc::new(|_| {
            Box::pin(async { Err(GhError::Failed("gh: boom".into())) })
        }));
        fresh.refresh_once().await;
        assert_eq!(
            fresh.snapshot(),
            Some(GhSnapshot {
                user: None,
                notifications: 0,
                review_requests: vec![],
                my_prs: vec![],
                updated_at: None,
                error: Some("gh: boom".into())
            })
        );
    }

    #[tokio::test]
    async fn notifications_that_fail_count_as_none() {
        let monitor = GitHubMonitor::with_gh(Arc::new(|args: Vec<String>| {
            Box::pin(async move {
                if args[1] == "graphql" {
                    Ok(GRAPHQL.to_string())
                } else {
                    Err(GhError::Failed("gh: HTTP 403".into()))
                }
            })
        }));
        monitor.refresh_once().await;
        let snap = monitor.snapshot().unwrap();
        assert_eq!((snap.notifications, snap.error), (0, None));
    }

    #[tokio::test]
    async fn cancelling_git_hub_refresh_reaps_the_cli() {
        use crate::tool_process::tests::{alive, read_pid, shell};
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pid");
        let cmd = shell(
            &format!(
                "[IO.File]::WriteAllText('{}', [string]$PID); Start-Sleep -Seconds 60",
                marker.display()
            ),
            &format!("echo $$ > '{}'; exec sleep 60", marker.display()),
        );
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let refresh = tokio::spawn(async move {
            run_gh(&cmd, async {
                let _ = cancel_rx.await;
            })
            .await
        });
        let pid = read_pid(&marker).await;
        cancel_tx.send(()).unwrap();
        assert_eq!(refresh.await.unwrap(), Err(GhError::Cancelled));
        assert!(!alive(pid), "GitHub CLI survived cancellation");
    }
}
