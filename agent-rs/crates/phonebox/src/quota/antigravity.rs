//! Port of `QuotaMonitor.Antigravity.cs`: Antigravity's quota from its local language server (plain HTTP on a port from
//! its command line, or HTTPS with a self-signed certificate on a port it may pick at run time), or from the signed-in
//! `agy` CLI's structured `/usage` command.

use std::collections::HashSet;
use std::future::Future;
use std::net::IpAddr;
use std::path::Path;
use std::pin::pin;
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use tokio::time::Instant;

use super::connect::{UsageHttp, UsageRequest};
use super::{QuotaLimit, QuotaProvider};
use crate::dotnet_text::parse_date_ms;
use crate::json_text::{Node, parse};
use crate::listening_ports::{Listener, url_host};
use crate::tool_process::{Output, RunError, ToolCommand};

/// The process names an Antigravity server or CLI runs under, legacy server names included.
pub const PROCESS_NAMES: [&str; 11] = [
    "agy",
    "language_server",
    "language-server",
    "Antigravity",
    "Antigravity IDE",
    "language_server_macos",
    "language_server_macos_arm",
    "language_server_linux",
    "language_server_windows_x64",
    "language_server_linux_x64",
    "language_server_linux_arm",
];

/// One loopback RPC candidate. Its text (and its debug form) never holds the CSRF token, so a candidate can't leak it
/// into a log line.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AntigravityEndpoint {
    pub port: u16,
    pub csrf_token: String,
    /// `http` or `https`.
    pub scheme: String,
    /// The loopback address in URL form: 127.0.0.1, or [::1] for a server that listens on IPv6 only.
    pub host: String,
}

impl AntigravityEndpoint {
    pub fn new(port: u16, csrf_token: &str, scheme: &str, host: &str) -> Self {
        AntigravityEndpoint {
            port,
            csrf_token: csrf_token.into(),
            scheme: scheme.into(),
            host: host.into(),
        }
    }

    /// Plain HTTP on 127.0.0.1.
    pub fn http(port: u16, csrf_token: &str) -> Self {
        Self::new(port, csrf_token, "http", "127.0.0.1")
    }
}

impl std::fmt::Display for AntigravityEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}:{}", self.scheme, self.host, self.port)
    }
}

impl std::fmt::Debug for AntigravityEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AntigravityEndpoint({self})")
    }
}

/// The caller cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// Why the CLI gave nothing: the caller cancelled, or the run failed (named by the C# exception kind, for the log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    Cancelled,
    Failed(&'static str),
}

/// Runs a tool: `tool_process::run`, or a fake in tests.
pub type CliRun =
    dyn Fn(ToolCommand, Duration) -> BoxFuture<'static, Result<Output, RunError>> + Send + Sync;

/// `(port, csrf token)` of the server's extension or hub port.
pub fn parse_endpoint_from_command_line(cmd: &str) -> Option<(u16, String)> {
    if cmd.trim().is_empty() {
        return None;
    }
    let flags = flag_matches(cmd);
    let hub_port = flag(&flags, &["hub-port", "hub_port", "port"]);
    let csrf = if hub_port.is_none() {
        flag(
            &flags,
            &["extension_server_csrf_token", "csrf_token", "csrfToken"],
        )
    } else {
        flag(&flags, &["csrf_token", "csrfToken"])
    };
    let port = hub_port.or_else(|| flag(&flags, &["extension_server_port"]))?;
    let port = dotnet_int(port).filter(|p| (1..=65535).contains(p))?;
    let csrf = csrf.filter(|c| !c.trim().is_empty())?;
    Some((u16::try_from(port).ok()?, csrf.to_string()))
}

/// `(port, csrf token)` of the server's own HTTPS port; port 0 means it picks one at start-up.
pub fn parse_https_server_from_command_line(cmd: &str) -> Option<(u16, String)> {
    if cmd.trim().is_empty() {
        return None;
    }
    let flags = flag_matches(cmd);
    // the server's own RPC checks --csrf_token; the extension token belongs to the IDE's extension server
    let csrf = flag(
        &flags,
        &["csrf_token", "csrfToken", "extension_server_csrf_token"],
    );
    let port = flag(&flags, &["https_server_port", "https-server-port"]).and_then(dotnet_int)?;
    let port = u16::try_from(port).ok()?;
    let csrf = csrf.filter(|c| !c.trim().is_empty())?;
    Some((port, csrf.to_string()))
}

/// `int.TryParse`: surrounding white space and a sign allowed.
fn dotnet_int(text: &str) -> Option<i32> {
    text.trim().parse().ok()
}

/// The value of the last occurrence of the first flag name present (names compared case-insensitively).
fn flag<'a>(flags: &[(&str, &'a str)], names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| {
        flags
            .iter()
            .rev()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| *v)
    })
}

/// Every flag with its value, left to right, as .NET finds the matches of
/// `(?:^|\s)--(?<name>[a-zA-Z_-]+)(?:=|\s+)(?:"(?<double>[^"]*)"|'(?<single>[^']*)'|(?<bare>(?!--)[^\s"']+))(?=\s|$)`:
/// a match starts the string or a white-space character, the value is whole (quoted, or bare up to white space), and
/// a bare value never starts with `--`.
fn flag_matches(cmd: &str) -> Vec<(&str, &str)> {
    let mut found = Vec::new();
    let mut at = 0;
    while at <= cmd.len() {
        let attempt = if at == 0 { flag_at(cmd, 0) } else { None }.or_else(|| {
            let c = cmd[at..].chars().next()?;
            if c.is_whitespace() {
                flag_at(cmd, at + c.len_utf8())
            } else {
                None
            }
        });
        match attempt {
            Some((end, name, value)) => {
                found.push((name, value));
                at = end;
            }
            None => match cmd[at..].chars().next() {
                Some(c) => at += c.len_utf8(),
                None => break,
            },
        }
    }
    found
}

/// One `--name value` from `pos`: the end of the match, the name and the value.
fn flag_at(cmd: &str, pos: usize) -> Option<(usize, &str, &str)> {
    let rest = cmd.get(pos..)?.strip_prefix("--")?;
    let start = pos + 2;
    let name_len = rest
        .bytes()
        .take_while(|b| b.is_ascii_alphabetic() || *b == b'_' || *b == b'-')
        .count();
    if name_len == 0 {
        return None;
    }
    let name = &cmd[start..start + name_len];
    let mut p = start + name_len;
    if cmd[p..].starts_with('=') {
        p += 1;
    } else {
        let spaces: usize = cmd[p..]
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(char::len_utf8)
            .sum();
        if spaces == 0 {
            return None;
        }
        p += spaces;
    }
    let ends_cleanly = |end: usize| cmd[end..].chars().next().is_none_or(char::is_whitespace);
    let (value, end) = match cmd[p..].chars().next()? {
        quote @ ('"' | '\'') => {
            let close = p + 1 + cmd[p + 1..].find(quote)?;
            (&cmd[p + 1..close], close + 1)
        }
        _ => {
            if cmd[p..].starts_with("--") {
                return None;
            }
            let len: usize = cmd[p..]
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                .map(char::len_utf8)
                .sum();
            if len == 0 {
                return None;
            }
            (&cmd[p..p + len], p + len)
        }
    };
    ends_cleanly(end).then_some((end, name, value))
}

/// The candidates: ports named on command lines first, from every process, then the listening ports of processes whose
/// HTTPS port is chosen at run time (over HTTPS with the same process's token).
pub fn find_antigravity_endpoints(
    read_commands: &dyn Fn(&str) -> Vec<(u32, String)>,
    listening_ports: &dyn Fn(u32) -> Vec<Listener>,
) -> Vec<AntigravityEndpoint> {
    const V4: &str = "127.0.0.1";
    // 127.0.0.1:62173 and [::1]:62173 are different sockets
    let mut named: HashSet<(String, u16)> = HashSet::new();
    let mut found = Vec::new();
    let mut dynamic = Vec::new();
    for name in PROCESS_NAMES {
        for (pid, cmd) in read_commands(name) {
            if name != "agy" && !cmd.to_lowercase().contains("antigravity") {
                continue;
            }
            if let Some((port, csrf)) = parse_endpoint_from_command_line(&cmd) {
                named.insert((V4.into(), port));
                found.push(AntigravityEndpoint::http(port, &csrf));
            }
            let Some((port, csrf)) = parse_https_server_from_command_line(&cmd) else {
                continue;
            };
            if port > 0 {
                named.insert((V4.into(), port));
                found.push(AntigravityEndpoint::new(port, &csrf, "https", V4));
            } else if pid > 0 {
                dynamic.push((pid, csrf));
            }
        }
    }
    for (pid, csrf) in dynamic {
        for listener in listening_ports(pid) {
            let host = url_host(listener.address);
            if !named.contains(&(host.clone(), listener.port)) {
                found.push(AntigravityEndpoint::new(
                    listener.port,
                    &csrf,
                    "https",
                    &host,
                ));
            }
        }
    }
    found
}

/// Every process's id, .NET name and command line (arguments joined by spaces), one look at the process table.
pub fn process_commands() -> Vec<(u32, String, String)> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let Ok(mut sys) = System::new() else {
        return Vec::new();
    };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    sys.processes()
        .values()
        .filter_map(|p| {
            let pid = p.pid().as_u32();
            let cmd = p
                .cmd()
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            let cmd = cmd.trim();
            (!cmd.is_empty()).then(|| {
                (
                    pid,
                    crate::process_monitor::dotnet_name(pid, &p.name().to_string_lossy()),
                    cmd.to_string(),
                )
            })
        })
        .collect()
}

/// The rows of `table` whose process name is `name` (case-insensitively, as `GetProcessesByName`).
pub fn commands_named(table: &[(u32, String, String)], name: &str) -> Vec<(u32, String)> {
    table
        .iter()
        .filter(|(_, n, _)| n.eq_ignore_ascii_case(name))
        .map(|(pid, _, cmd)| (*pid, cmd.clone()))
        .collect()
}

/// The process id and command line of each running process named `name` (as .NET's `GetProcessesByName`).
pub fn read_commands(name: &str) -> Vec<(u32, String)> {
    commands_named(&process_commands(), name)
}

/// A valid certificate always passes; an invalid one only for HTTPS to a loopback address or localhost.
pub fn accept_loopback_certificate(url: Option<&str>, has_errors: bool) -> bool {
    if !has_errors {
        return true;
    }
    let Some(uri) = url.and_then(|u| u.parse::<http::Uri>().ok()) else {
        return false;
    };
    if uri.scheme_str() != Some("https") {
        return false;
    }
    uri.host()
        .is_some_and(|host| loopback_host(host.trim_matches(['[', ']'])))
}

/// `localhost` (any case) or a loopback address (all of 127/8, and ::1).
fn loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Tries up to 8 distinct candidates within 10 s, 3 s each; an unavailable server must not hide a healthy one. HTTPS
/// candidates go through `https` (the loopback-only certificate check) when given.
pub async fn read_antigravity(
    http: &UsageHttp,
    endpoints: &[AntigravityEndpoint],
    cancel: impl Future<Output = ()>,
    https: Option<&UsageHttp>,
) -> Result<Option<QuotaProvider>, Cancelled> {
    let mut cancel = pin!(cancel);
    let budget = Instant::now() + Duration::from_secs(10);
    let mut tried: Vec<&AntigravityEndpoint> = Vec::new();
    for ep in endpoints {
        if tried.contains(&ep) {
            continue;
        }
        if tried.len() == 8 {
            break;
        }
        tried.push(ep);
        if cancel.as_mut().now_or_never().is_some() {
            return Err(Cancelled);
        }
        if Instant::now() >= budget {
            break;
        }
        if ep.scheme != "http" && ep.scheme != "https" {
            continue;
        }
        let client = if ep.scheme == "https" {
            https.unwrap_or(http)
        } else {
            http
        };
        let deadline = (Instant::now() + Duration::from_secs(3)).min(budget);
        let summary = post(
            client,
            ep,
            "RetrieveUserQuotaSummary",
            deadline,
            cancel.as_mut(),
        )
        .await?;
        let mut limits = summary
            .as_ref()
            .map(parse_antigravity_limits)
            .unwrap_or_default();
        let status = post(client, ep, "GetUserStatus", deadline, cancel.as_mut()).await?;
        let plan = status.as_ref().and_then(parse_antigravity_plan);
        if let Some(status) = &status
            && limits.is_empty()
        {
            limits = parse_antigravity_legacy_limits(status);
        }
        if !limits.is_empty() {
            let plan = plan.as_deref().unwrap_or("Antigravity");
            let now = crate::server::now_ms();
            return Ok(Some(QuotaProvider::new(
                "antigravity",
                "Antigravity",
                plan,
                limits,
                None,
                Some(now),
                None,
            )));
        }
    }
    Ok(None)
}

/// One RPC: the parsed answer, or None for a refusal, an HTTP or JSON error, or the deadline. The caller's
/// cancellation stops everything.
async fn post<C: Future<Output = ()>>(
    client: &UsageHttp,
    ep: &AntigravityEndpoint,
    method: &str,
    deadline: Instant,
    mut cancel: std::pin::Pin<&mut C>,
) -> Result<Option<Node>, Cancelled> {
    if cancel.as_mut().now_or_never().is_some() {
        return Err(Cancelled);
    }
    if Instant::now() >= deadline {
        return Ok(None);
    }
    let request = UsageRequest {
        url: format!("{ep}/exa.language_server_pb.LanguageServerService/{method}"),
        headers: vec![
            ("X-Codeium-Csrf-Token".into(), ep.csrf_token.clone()),
            ("Connect-Protocol-Version".into(), "1".into()),
            (
                "Content-Type".into(),
                "application/json; charset=utf-8".into(),
            ),
        ],
        body: Some("{}".into()),
    };
    let answer = tokio::select! {
        biased;
        _ = cancel => return Err(Cancelled),
        answer = tokio::time::timeout_at(deadline, client(request)) => answer,
    };
    Ok(match answer {
        Ok(Ok(reply)) if (200..300).contains(&reply.status) => parse(&reply.body, false).ok(),
        _ => None,
    })
}

/// The CLI's structured usage command; a version before 1.1.11 must never read `/usage` as a model prompt.
pub async fn read_antigravity_cli(
    path: &Path,
    cancel: impl Future<Output = ()>,
    run: &CliRun,
) -> Result<Option<QuotaProvider>, CliError> {
    let mut cancel = pin!(cancel);
    let start = |args: &[&str]| {
        ToolCommand::new(path)
            .args(args.iter().copied())
            .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
            .env("BROWSER", "false")
    };
    let version = guarded(
        cancel.as_mut(),
        run(start(&["--version"]), Duration::from_secs(3)),
    )
    .await?;
    if version.code != 0 || !supported_cli(version.stdout.trim().trim_start_matches('v')) {
        return Ok(None);
    }
    let args = [
        "-p",
        "/usage",
        "--output-format",
        "json",
        "--print-timeout",
        "15s",
    ];
    let result = guarded(cancel.as_mut(), run(start(&args), Duration::from_secs(20))).await?;
    if result.code != 0 || crate::dotnet_text::utf16_len(&result.stdout) > 1 << 20 {
        return Ok(None);
    }
    let root = parse(&result.stdout, false).map_err(|_| CliError::Failed("JsonException"))?;
    if root.text("status") != Some("SUCCESS")
        || root.get("command").and_then(|c| c.text("name")) != Some("usage")
    {
        return Ok(None);
    }
    let limits = parse_antigravity_limits(&root);
    if limits.is_empty() {
        return Ok(None);
    }
    let now = crate::server::now_ms();
    Ok(Some(QuotaProvider::new(
        "antigravity",
        "Antigravity",
        "Antigravity",
        limits,
        None,
        Some(now),
        None,
    )))
}

/// A run that the caller's cancellation can stop; failures named as C#'s exception kinds.
async fn guarded<C: Future<Output = ()>>(
    cancel: std::pin::Pin<&mut C>,
    run: BoxFuture<'static, Result<Output, RunError>>,
) -> Result<Output, CliError> {
    let result = tokio::select! {
        biased;
        _ = cancel => return Err(CliError::Cancelled),
        result = run => result,
    };
    result.map_err(|e| match e {
        RunError::Cancelled => CliError::Cancelled,
        RunError::Timeout(_) => CliError::Failed("TimeoutException"),
        RunError::Start(_) => CliError::Failed("Win32Exception"),
    })
}

/// `Version.TryParse(text) >= 1.1.11`: two to four non-negative numbers; missing parts count as -1.
fn supported_cli(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    if !(2..=4).contains(&parts.len()) {
        return false;
    }
    let mut version = [-1i64; 4];
    for (slot, part) in version.iter_mut().zip(&parts) {
        match dotnet_int(part) {
            Some(n) if n >= 0 => *slot = i64::from(n),
            _ => return false,
        }
    }
    version >= [1, 1, 11, -1]
}

pub fn parse_antigravity_plan(root: &Node) -> Option<String> {
    let user = root
        .get("userStatus")
        .filter(|u| u.is_object())
        .unwrap_or(root);
    let plan_info = user.get("planStatus").and_then(|p| p.get("planInfo"));
    let name = plan_info
        .and_then(|p| p.text("planName"))
        .filter(|n| !n.trim().is_empty())
        .or_else(|| user.get("userTier").and_then(|t| t.text("name")));
    name.filter(|n| !n.trim().is_empty()).map(String::from)
}

pub fn parse_antigravity_limits(root: &Node) -> Vec<QuotaLimit> {
    let mut list = Vec::new();
    let mut resp = Some(
        root.get("response")
            .filter(|r| r.is_object())
            .unwrap_or(root),
    );
    if let Some(command) = root.get("command") {
        if root.text("status") != Some("SUCCESS") || command.text("name") != Some("usage") {
            return list;
        }
        resp = command.get("data");
    }
    let Some(Node::Array(groups)) = resp.and_then(|r| r.get("groups")) else {
        return list;
    };
    let several = groups.len() > 1;
    for g in groups {
        let group_name = g
            .text("displayName")
            .or_else(|| g.text("name"))
            .unwrap_or("");
        let prefix = if contains_ci(group_name, "Gemini") {
            "Gemini"
        } else if contains_ci(group_name, "Claude") || contains_ci(group_name, "GPT") {
            "Claude/GPT"
        } else {
            group_name
        };
        let Some(Node::Array(buckets)) = g.get("buckets") else {
            continue;
        };
        for b in buckets {
            let bucket_id = b.text("bucketId").or_else(|| b.text("id")).unwrap_or("");
            let window = b.text("window").unwrap_or("");
            let (label, window_sec) = if window == "5h" || contains_ci(bucket_id, "5h") {
                (
                    if several {
                        format!("5-hour ({prefix})")
                    } else {
                        "5-hour session".into()
                    },
                    Some(5 * 3600),
                )
            } else if window == "weekly" || contains_ci(bucket_id, "weekly") {
                (
                    if several {
                        format!("Weekly ({prefix})")
                    } else {
                        "Weekly".into()
                    },
                    Some(7 * 86400),
                )
            } else {
                (
                    b.text("displayName")
                        .or_else(|| b.text("name"))
                        .unwrap_or(bucket_id)
                        .to_string(),
                    None,
                )
            };
            // current servers nest the measured fraction; older ones put it on the bucket. Unknown usage must not
            // become a healthy, unused quota.
            let fraction = b
                .get("remaining")
                .and_then(|r| r.num("remainingFraction"))
                .or_else(|| b.num("remainingFraction"))
                .or_else(|| b.num("remaining_fraction"));
            let Some(remaining) = fraction.filter(|r| (0.0..=1.0).contains(r)) else {
                continue;
            };
            let used = round1((1.0 - remaining) * 100.0);
            let resets = b
                .text("resetTime")
                .or_else(|| b.text("reset_time"))
                .and_then(|t| parse_date_ms(t, false));
            list.push(QuotaLimit::new(
                &label,
                used,
                resets,
                window_sec,
                severity(used),
            ));
        }
    }
    let third_party =
        |l: &QuotaLimit| contains_ci(&l.label, "Claude") || contains_ci(&l.label, "GPT");
    list.sort_by_key(|l| (third_party(l), l.window_sec.unwrap_or(i64::MAX)));
    list
}

/// `Math.Round(x, 1)`: half to even.
fn round1(x: f64) -> f64 {
    (x * 10.0).round_ties_even() / 10.0
}

fn severity(percent: f64) -> &'static str {
    if percent >= 95.0 {
        "critical"
    } else if percent >= 80.0 {
        "warning"
    } else {
        "normal"
    }
}

/// `Contains(…, StringComparison.OrdinalIgnoreCase)`.
fn contains_ci(text: &str, part: &str) -> bool {
    text.to_lowercase().contains(&part.to_lowercase())
}

/// Measured model quotas of older IDE servers, without inventing weekly limits.
pub fn parse_antigravity_legacy_limits(root: &Node) -> Vec<QuotaLimit> {
    let models = root
        .get("userStatus")
        .and_then(|s| s.get("cascadeModelConfigData"))
        .and_then(|d| d.get("clientModelConfigs"));
    let Some(Node::Array(models)) = models else {
        return Vec::new();
    };
    let mut list = Vec::new();
    for model in models {
        let quota = model.get("quotaInfo");
        let Some(label) = model.text("label").filter(|l| !l.trim().is_empty()) else {
            continue;
        };
        let Some(remaining) = quota
            .and_then(|q| q.num("remainingFraction"))
            .filter(|r| (0.0..=1.0).contains(r))
        else {
            continue;
        };
        let percent = round1((1.0 - remaining) * 100.0);
        let resets = quota
            .and_then(|q| q.text("resetTime"))
            .and_then(|t| parse_date_ms(t, false));
        list.push(QuotaLimit::new(
            label,
            percent,
            resets,
            None,
            severity(percent),
        ));
    }
    list
}

/// The loopback TLS client: certificate checks as usual, except that a certificate that fails them still passes for
/// HTTPS to a loopback address or localhost (the Antigravity server's is self-signed). No proxy, no redirects.
#[cfg(feature = "tls")]
pub fn loopback_tls_http() -> UsageHttp {
    let config = rustls::ClientConfig::builder_with_provider(super::connect::tls_provider())
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(loopback_verifier())
        .with_no_client_auth();
    super::connect::http_with_tls(config)
}

#[cfg(feature = "tls")]
pub(crate) fn loopback_verifier() -> std::sync::Arc<dyn rustls::client::danger::ServerCertVerifier>
{
    let provider = super::connect::tls_provider();
    let webpki = rustls::client::WebPkiServerVerifier::builder_with_provider(
        std::sync::Arc::new(super::connect::webpki_roots()),
        provider.clone(),
    )
    .build()
    .expect("the webpki roots build a verifier");
    std::sync::Arc::new(LoopbackVerifier {
        webpki,
        algorithms: provider.signature_verification_algorithms,
    })
}

/// The usual checks; a certificate that fails them passes only when the request goes to this computer.
#[cfg(feature = "tls")]
#[derive(Debug)]
struct LoopbackVerifier {
    webpki: std::sync::Arc<rustls::client::WebPkiServerVerifier>,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

#[cfg(feature = "tls")]
impl rustls::client::danger::ServerCertVerifier for LoopbackVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use rustls::pki_types::ServerName;
        let checked = self.webpki.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        );
        let loopback = match server_name {
            ServerName::DnsName(name) => loopback_host(name.as_ref()),
            ServerName::IpAddress(ip) => IpAddr::from(*ip).is_loopback(),
            _ => false,
        };
        match checked {
            Err(_) if loopback => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
#[path = "antigravity_tests.rs"]
mod tests;
