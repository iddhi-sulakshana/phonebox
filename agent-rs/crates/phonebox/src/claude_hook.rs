//! Port of `ClaudeHook.cs`: a PermissionRequest hook in `~/.claude/settings.json` posts each permission prompt to the
//! agent, which shows it on the phone and returns Allow / Deny as the hook's output. No answer means no output, and
//! Claude Code asks in the terminal as usual.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::app_paths::{read_all_text, write_all_text_private};
use crate::approval_store::{Approval, ApprovalStore, new_id};
use crate::config::store::ConfigStore;
use crate::dotnet_text::{cut, file_name};
use crate::json_text::{Escaping, Node, parse, to_indented};
use crate::session_monitor::SessionMonitor;
use crate::work_buddy::WorkBuddyNames;

pub const ENDPOINT: &str = "/api/hooks/claude/permission";
const MAX_DETAIL: usize = 3000;
const ALLOW: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
const DENY: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied from the PhoneBox panel."}}}"#;

/// What a hook call needs.
pub struct ClaudeHook {
    pub cfg: Arc<ConfigStore>,
    pub approvals: Arc<ApprovalStore>,
    pub sessions: Arc<SessionMonitor>,
    pub names: Arc<WorkBuddyNames>,
    pub home: PathBuf,
    /// Seconds since the last keyboard or mouse input ([`crate::idle::seconds`]; blocking).
    pub idle: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl ClaudeHook {
    /// One hook call: the JSON text for the hook to print, or None for "no decision".
    pub async fn handle(&self, input: &Node, aborted: impl Future<Output = ()>) -> Option<String> {
        if !matches!(input, Node::Object(_)) {
            return None;
        }
        let c = self.cfg.get().claude.clone();
        if !c.phone_approvals {
            return None;
        }
        if c.only_when_away {
            let idle = self.idle.clone();
            if tokio::task::spawn_blocking(move || idle())
                .await
                .unwrap_or(0.0)
                < 60.0
            {
                return None;
            }
        }

        let tool = text(input, "tool_name").unwrap_or("a tool");
        let ti = input
            .get("tool_input")
            .filter(|t| matches!(t, Node::Object(_)));
        let cwd = text(input, "cwd").unwrap_or("");
        let session_id = text(input, "session_id").unwrap_or("");
        let key = format!("claude:{session_id}");
        // the session's name, never its title: the card goes to the phone even when titles are not shared
        let project = match self.sessions.snapshot().into_iter().find(|s| s.id == key) {
            Some(s) => s.name,
            None => project_name(cwd, &self.home, &self.names),
        };
        let now = crate::server::now_ms();
        // AskUserQuestion is a question, not a permission: Allow would only open the same question in the terminal.
        let question = tool == "AskUserQuestion";
        let a = Approval {
            id: new_id(),
            source: "Claude Code".into(),
            session: session_id.into(),
            project,
            tool: tool_title(tool),
            detail: if question {
                question_detail(ti)
            } else {
                detail(tool, ti)
            },
            description: ti.and_then(|t| text(t, "description")).map(Into::into),
            created_at: now,
            expires_at: now + i64::from(c.timeout_sec.clamp(10, 3600)) * 1000,
            kind: if question { "question" } else { "permission" }.into(),
        };
        match self.approvals.ask(a, aborted).await.as_deref() {
            None | Some("skip") => None, // no decision: Claude Code asks in the terminal as usual
            Some("allow") => Some(ALLOW.into()),
            Some(_) => Some(DENY.into()),
        }
    }
}

/// `Str`: a member's text when it is a string.
fn text<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.get(name).and_then(Node::as_str)
}

/// `~/.claude/settings.json`.
pub fn settings_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

/// What the panel shows as the project: the workspace name WorkBuddy remembers for a folder, else the folder's name.
pub fn project_name(cwd: &str, home: &Path, names: &WorkBuddyNames) -> String {
    names
        .for_cwd(Some(cwd), home)
        .unwrap_or_else(|| file_name(cwd.trim_end_matches(['\\', '/'])).to_string())
}

pub fn tool_title(tool: &str) -> String {
    match tool {
        "Bash" => "Run a command".into(),
        "Edit" | "MultiEdit" => "Edit a file".into(),
        "Write" => "Write a file".into(),
        "NotebookEdit" => "Edit a notebook".into(),
        "WebFetch" => "Fetch a web page".into(),
        "WebSearch" => "Search the web".into(),
        "Read" => "Read a file".into(),
        "AskUserQuestion" => "Claude has a question".into(),
        _ => match tool.strip_prefix("mcp__") {
            Some(rest) => format!("Use {}", rest.replace("__", " › ")),
            None => format!("Use {tool}"),
        },
    }
}

/// What the tool wants to do, as text; anything without a known field is the tool input itself, indented.
pub fn detail(tool: &str, ti: Option<&Node>) -> String {
    let Some(ti) = ti.filter(|t| matches!(t, Node::Object(_))) else {
        return String::new();
    };
    let s = match tool {
        "Bash" => text(ti, "command").map(String::from),
        "Edit" | "MultiEdit" | "Read" | "NotebookEdit" => text(ti, "file_path")
            .or_else(|| text(ti, "notebook_path"))
            .map(String::from),
        "Write" => text(ti, "file_path")
            .map(|f| format!("{f}\n\n{}", head(text(ti, "content").unwrap_or(""), 12))),
        "WebFetch" => text(ti, "url").map(String::from),
        "WebSearch" => text(ti, "query").map(String::from),
        _ => None,
    };
    let s = match (s, text(ti, "new_string")) {
        (Some(s), Some(ns)) if tool == "Edit" => format!("{s}\n\n{}", head(ns, 12)),
        (s, _) => s.unwrap_or_else(|| to_indented(ti, Escaping::Default)),
    };
    cut(&s, MAX_DETAIL)
}

/// The questions and their choices as plain text: "Header: Question" and one line per choice.
pub fn question_detail(ti: Option<&Node>) -> String {
    let mut sb = String::new();
    if let Some(Node::Array(qs)) = ti.and_then(|t| t.get("questions")) {
        for q in qs.iter().filter(|q| matches!(q, Node::Object(_))) {
            if !sb.is_empty() {
                sb.push_str("\n\n");
            }
            if let Some(header) = text(q, "header").filter(|h| !h.is_empty()) {
                sb.push_str(header);
                sb.push_str(": ");
            }
            sb.push_str(text(q, "question").unwrap_or(""));
            if q.get("multiSelect") == Some(&Node::Bool(true)) {
                sb.push_str("  (more than one can be picked)");
            }
            if let Some(Node::Array(options)) = q.get("options") {
                for o in options {
                    let Some(label) = text(o, "label").filter(|l| !l.is_empty()) else {
                        continue;
                    };
                    sb.push_str("\n  • ");
                    sb.push_str(label);
                    if let Some(d) = text(o, "description").filter(|d| !d.is_empty()) {
                        sb.push_str(" — ");
                        sb.push_str(d);
                    }
                }
            }
        }
    }
    if sb.is_empty() {
        sb = "Claude is asking something; answer it in the terminal on your PC.".into();
    }
    cut(&sb, MAX_DETAIL)
}

fn head(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.split('\n').collect();
    let mut out = all
        .iter()
        .take(lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if all.len() > lines {
        out.push_str(&format!("\n… ({} more lines)", all.len() - lines));
    }
    out
}

// ---------------------------------------------------------------- install / remove the hook in ~/.claude/settings.json

pub fn hook_command(port: u16) -> String {
    let curl = if cfg!(windows) { "curl.exe" } else { "curl" };
    format!(
        "{curl} -s --max-time 3600 -X POST http://127.0.0.1:{port}{ENDPOINT} -H \"Content-Type: application/json\" --data-binary \"@-\""
    )
}

/// `JsonObject` builds its dictionary on first use and throws on a repeated name there; objects the code never opens
/// are written back as they were, repeats included.
fn opened(members: &[(String, Node)]) -> Result<(), String> {
    let mut seen = HashSet::new();
    match members.iter().find(|(k, _)| !seen.insert(k.as_str())) {
        Some((k, _)) => Err(format!(
            "An item with the same key has already been added. Key: {k}"
        )),
        None => Ok(()),
    }
}

/// `IsOurs`: a hook entry whose command posts to our endpoint.
fn is_ours(hook: &Node) -> Result<bool, String> {
    let Node::Object(members) = hook else {
        return Ok(false);
    };
    opened(members)?;
    Ok(text(hook, "command").is_some_and(|c| c.contains(ENDPOINT)))
}

/// True when the settings file has our hook (comments and trailing commas allowed; anything unreadable is false).
pub fn installed(path: &Path) -> bool {
    let check = || -> Result<bool, String> {
        let root = parse(&read_all_text(path).map_err(|e| e.to_string())?, true)?;
        let Node::Object(members) = &root else {
            return Ok(false);
        };
        opened(members)?;
        let Some(hooks @ Node::Object(hook_members)) = root.get("hooks") else {
            return Ok(false);
        };
        opened(hook_members)?;
        let Some(Node::Array(groups)) = hooks.get("PermissionRequest") else {
            return Ok(false);
        };
        for g in groups {
            let Node::Object(gm) = g else { continue };
            opened(gm)?;
            if let Some(Node::Array(hs)) = g.get("hooks") {
                for h in hs {
                    if is_ours(h)? {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    };
    check().unwrap_or(false)
}

/// Adds (or, with `install` false, removes) the PhoneBox hook, keeping everything else and a backup; None, or why not.
pub fn configure(install: bool, port: u16, timeout_sec: i32, path: &Path) -> Option<String> {
    try_configure(install, port, timeout_sec, path).err()
}

fn try_configure(install: bool, port: u16, timeout_sec: i32, path: &Path) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let mut root_members = Vec::new();
    if path.is_file() {
        let text = read_all_text(path).map_err(io)?;
        if !text.chars().all(char::is_whitespace) {
            let Node::Object(members) = parse(&text, true)? else {
                return Err("settings.json is not a JSON object".into());
            };
            // owner-only like the replacement below: the file can hold API keys (its "env" section)
            let mut backup = path.as_os_str().to_owned();
            backup.push(".phonebox.bak");
            write_all_text_private(Path::new(&backup), &text).map_err(io)?;
            root_members = members;
        }
    }
    opened(&root_members)?;

    // a value of another shape is the user's to fix: replacing or dropping it would lose what they wrote
    let hooks_at = root_members.iter().position(|(k, _)| k == "hooks");
    let mut hooks = match hooks_at.map(|i| &mut root_members[i].1) {
        Some(Node::Object(m)) => std::mem::take(m),
        None | Some(Node::Null) => Vec::new(),
        Some(_) => return Err("\"hooks\" in settings.json is not a JSON object".into()),
    };
    opened(&hooks)?;
    let groups_at = hooks.iter().position(|(k, _)| k == "PermissionRequest");
    let groups = match groups_at.map(|i| &mut hooks[i].1) {
        Some(Node::Array(g)) => std::mem::take(g),
        None | Some(Node::Null) => Vec::new(),
        Some(_) => return Err("\"hooks.PermissionRequest\" in settings.json is not a list".into()),
    };

    // drop our old entries (and groups that become empty)
    let mut kept = Vec::with_capacity(groups.len() + 1);
    for mut g in groups {
        if let Node::Object(gm) = &g {
            opened(gm)?;
        }
        if let Some(Node::Array(hs)) = g.get_mut("hooks") {
            let mut others = Vec::with_capacity(hs.len());
            for h in std::mem::take(hs) {
                if !is_ours(&h)? {
                    others.push(h);
                }
            }
            if others.is_empty() {
                continue;
            }
            *hs = others;
        }
        kept.push(g);
    }
    if install {
        let entry = Node::Object(vec![
            ("type".into(), Node::String("command".into())),
            ("command".into(), Node::String(hook_command(port))),
            (
                "timeout".into(),
                Node::Number((timeout_sec.clamp(10, 3600) + 15).to_string()),
            ),
        ]);
        kept.push(Node::Object(vec![
            ("matcher".into(), Node::String("*".into())),
            ("hooks".into(), Node::Array(vec![entry])),
        ]));
    }

    put_or_remove(
        &mut hooks,
        groups_at,
        "PermissionRequest",
        (!kept.is_empty()).then_some(Node::Array(kept)),
    );
    let hooks = (!hooks.is_empty()).then_some(Node::Object(hooks));
    put_or_remove(&mut root_members, hooks_at, "hooks", hooks);

    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    // created owner-only: moving a 0644 file over the original would widen a private settings file
    write_all_text_private(
        &tmp,
        &to_indented(&Node::Object(root_members), Escaping::Relaxed),
    )
    .map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// `obj[key] = value` (in place when the member exists, else at the end) or `obj.Remove(key)`.
fn put_or_remove(
    members: &mut Vec<(String, Node)>,
    at: Option<usize>,
    key: &str,
    value: Option<Node>,
) {
    match (at, value) {
        (Some(i), Some(v)) => members[i].1 = v,
        (None, Some(v)) => members.push((key.into(), v)),
        (Some(i), None) => {
            members.remove(i);
        }
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_text::parse;
    use std::sync::atomic::Ordering;

    fn json(s: &str) -> Node {
        parse(s, false).unwrap()
    }

    #[test]
    fn the_question_and_its_choices_are_listed_as_text() {
        let text = question_detail(Some(&json(
            r#"{"questions":[
              {"header":"Framework","question":"Which framework?","multiSelect":false,
               "options":[{"label":"React","description":"Component library"},{"label":"Vue","description":""}]},
              {"header":"Extras","question":"Which extras?","multiSelect":true,"options":[{"label":"Tests","description":"xUnit"}]}]}"#,
        )));
        assert_eq!(
            text,
            "Framework: Which framework?\n  • React — Component library\n  • Vue\n\nExtras: Which extras?  (more than one can be picked)\n  • Tests — xUnit"
        );
    }

    #[test]
    fn without_questions_it_still_tells_you_to_look_at_the_terminal() {
        for input in ["{}", r#"{"questions":[]}"#, r#"{"questions":"nope"}"#] {
            assert!(
                question_detail(Some(&json(input))).contains("terminal"),
                "{input}"
            );
        }
        assert!(question_detail(None).contains("terminal"));
    }

    #[test]
    fn invalid_question_entries_do_not_hide_their_valid_neighbors() {
        for entry in ["null", "0", "false", "[]", "\"text\""] {
            assert!(
                question_detail(Some(&json(&format!("{{\"questions\":[{entry}]}}"))))
                    .contains("terminal")
            );
            let text = question_detail(Some(&json(&format!(
                "{{\"questions\":[{entry},{{\"question\":\"Continue?\"}},{entry}]}}"
            ))));
            assert_eq!(text, "Continue?", "{entry}");
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        hook: ClaudeHook,
    }

    fn fixture(config: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.json");
        std::fs::write(&cfg_path, config).unwrap();
        let cfg = Arc::new(ConfigStore::new(cfg_path));
        let push = crate::push_store::PushStore::new(&dir.path().join("push.json"));
        let phone = Arc::new(crate::server::PhoneSeenState::default());
        phone
            .last_seen
            .store(crate::server::now_ms(), Ordering::SeqCst);
        let hook = ClaudeHook {
            approvals: ApprovalStore::new(push, phone),
            sessions: SessionMonitor::new(Some(cfg.clone()), dir.path().into()),
            cfg,
            names: Arc::new(WorkBuddyNames::new()),
            home: dir.path().into(),
            idle: Arc::new(|| 0.0),
        };
        Fixture { dir, hook }
    }

    async fn listed(store: &ApprovalStore) -> crate::approval_store::Approval {
        for _ in 0..200 {
            if let Some(a) = store.snapshot().into_iter().next() {
                return a;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("nothing reached the phone");
    }

    #[tokio::test]
    async fn got_it_ends_the_request_without_an_answer_so_the_terminal_keeps_the_question() {
        let f = fixture(r#"{"claude":{"phoneApprovals":true,"timeoutSec":30}}"#);
        let input = json(
            r#"{"session_id":"s","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Go?"}]}}"#,
        );
        let hook = Arc::new(f.hook);
        let asking = tokio::spawn({
            let hook = hook.clone();
            async move { hook.handle(&input, std::future::pending()).await }
        });
        let card = listed(&hook.approvals).await;
        assert_eq!(card.kind, "question");
        assert_eq!(card.tool, "Claude has a question");
        assert_eq!(card.detail, "Go?");
        assert!(hook.approvals.decide(&card.id, "skip"));
        assert_eq!(
            asking.await.unwrap(),
            None,
            "no answer: the terminal keeps the question"
        );
        assert!(!hook.approvals.decide(&card.id, "skip"));
        assert!(!hook.approvals.decide("q2", "maybe"));
    }

    #[tokio::test]
    async fn non_object_payloads_edit_without_file_path_malformed_options_and_settings_hook_configure_behave_safely()
     {
        let f =
            fixture(r#"{"claude":{"phoneApprovals":true,"onlyWhenAway":false,"timeoutSec":30}}"#);
        let hook = Arc::new(f.hook);
        assert_eq!(hook.handle(&json("[]"), std::future::pending()).await, None);
        assert_eq!(
            hook.handle(&json("\"ping\""), std::future::pending()).await,
            None
        );

        let edit = json(
            r#"{"tool_name":"Edit","tool_input":{"path":"src/App.cs","old_string":"oldVal","new_string":"newVal"}}"#,
        );
        let asking = tokio::spawn({
            let hook = hook.clone();
            async move { hook.handle(&edit, std::future::pending()).await }
        });
        let card = listed(&hook.approvals).await;
        assert!(
            card.detail.contains("oldVal") && card.detail.contains("newVal"),
            "{}",
            card.detail
        );
        assert!(!card.detail.starts_with("\n\n"));
        hook.approvals.decide(&card.id, "skip");
        asking.await.unwrap();

        let q = question_detail(Some(&json(
            r#"{"questions":[{"header":"Pick","question":"Which?","options":[null, 42, {}, {"label":""}, {"label":"A","description":"ok"}]}]}"#,
        )));
        assert_eq!(q, "Pick: Which?\n  • A — ok");

        let settings = f.dir.path().join("settings.json");
        std::fs::write(&settings, "  ").unwrap();
        assert_eq!(configure(true, 18765, 30, &settings), None);
        assert!(installed(&settings));
        assert!(
            !f.dir.path().join("settings.json.phonebox.bak").exists(),
            "no backup of a blank file"
        );

        std::fs::write(
            &settings,
            r#"{
  "theme": "dark",
  "hooks": {
    "PermissionRequest": [
      "bad-entry",
      { "matcher": "Bash", "hooks": [ ["not-obj"], { "command": 123 }, { "type": "command", "command": "echo user-hook" } ] }
    ]
  }
}"#,
        )
        .unwrap();
        assert!(!installed(&settings));
        assert_eq!(configure(true, 18765, 30, &settings), None);
        assert!(installed(&settings));
        assert_eq!(configure(false, 18765, 30, &settings), None);
        assert!(!installed(&settings));
        let text = std::fs::read_to_string(&settings).unwrap();
        assert!(
            text.contains("echo user-hook") && text.contains("bad-entry"),
            "{text}"
        );
    }

    #[test]
    fn replacing_claude_settings_keeps_them_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{ "env": { "ANTHROPIC_API_KEY": "not-a-real-key" } }"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(configure(true, 18765, 30, &path), None);
        assert!(installed(&path));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("not-a-real-key")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&dir.path().join("settings.json.phonebox.bak")), 0o600);
        }
    }

    #[test]
    fn a_malformed_hooks_value_is_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        for text in [
            r#"{ "hooks": { "PermissionRequest": { "matcher": "*" } } }"#,
            r#"{ "hooks": ["not", "an", "object"] }"#,
        ] {
            for install in [false, true] {
                std::fs::write(&path, text).unwrap();
                assert!(configure(install, 18765, 30, &path).is_some(), "{text}");
                assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
            }
        }
        for (text, error) in [
            ("[1]", "settings.json is not a JSON object"),
            (
                r#"{"hooks": 1}"#,
                "\"hooks\" in settings.json is not a JSON object",
            ),
            (
                r#"{"hooks": {"PermissionRequest": "x"}}"#,
                "\"hooks.PermissionRequest\" in settings.json is not a list",
            ),
        ] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(configure(true, 18765, 30, &path).as_deref(), Some(error));
        }
        std::fs::write(&path, "{ not json").unwrap();
        assert!(configure(true, 18765, 30, &path).is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn configure_keeps_key_order_and_number_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = "// mine\n{\"z\": 1.0, \"a\": {\"big\": 1e3}, \"hooks\": {\"Other\": [], \"PermissionRequest\": [\n  {\"matcher\": \"*\", \"hooks\": []},\n]}, \"after\": \"é\"}";
        std::fs::write(&path, original).unwrap();
        assert_eq!(configure(true, 9999, 5, &path), None);
        let text = std::fs::read_to_string(&path).unwrap();
        let installed_node = parse(&text, false).unwrap();
        let Node::Object(members) = &installed_node else {
            panic!()
        };
        assert_eq!(
            members.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["z", "a", "hooks", "after"]
        );
        assert!(
            text.contains("\"z\": 1.0")
                && text.contains("\"big\": 1e3")
                && text.contains("\"after\": \"é\""),
            "{text}"
        );
        let groups = installed_node
            .get("hooks")
            .unwrap()
            .get("PermissionRequest")
            .unwrap();
        let Node::Array(groups) = groups else {
            panic!()
        };
        assert_eq!(
            groups.len(),
            1,
            "the empty user group is dropped as in C#, ours added"
        );
        let entry = groups[0].get("hooks").unwrap();
        let Node::Array(entries) = entry else {
            panic!()
        };
        assert_eq!(
            entries[0].get("command").and_then(Node::as_str),
            Some(hook_command(9999).as_str())
        );
        assert_eq!(
            entries[0].get("timeout"),
            Some(&Node::Number("25".into())),
            "clamped to 10, plus 15"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("settings.json.phonebox.bak")).unwrap(),
            original
        );

        assert_eq!(configure(false, 9999, 5, &path), None);
        let removed = parse(&std::fs::read_to_string(&path).unwrap(), false).unwrap();
        assert_eq!(removed.get("hooks").unwrap().get("PermissionRequest"), None);
        assert_eq!(
            removed.get("hooks").unwrap().get("Other"),
            Some(&Node::Array(vec![]))
        );
    }

    #[test]
    fn repeated_members_on_the_hook_path_are_refused_like_json_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // JsonObject throws when it builds a dictionary with a repeated name; objects the hook never opens keep theirs
        let text = r#"{"env": {"A": "1", "A": "2"}, "hooks": {}, "hooks": {}}"#;
        std::fs::write(&path, text).unwrap();
        assert_eq!(
            configure(true, 1, 30, &path).as_deref(),
            Some("An item with the same key has already been added. Key: hooks")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(!installed(&path));
        std::fs::write(&path, r#"{"env": {"A": "1", "A": "2"}, "hooks": null}"#).unwrap();
        assert_eq!(configure(true, 1, 30, &path), None);
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written.matches("\"A\"").count(), 2, "{written}");
        assert!(installed(&path));
        assert_eq!(configure(false, 1, 30, &path), None);
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("hooks"),
            "a null hooks member goes with the last hook"
        );
    }

    #[tokio::test]
    async fn the_permission_card_never_shows_the_title() {
        let f = fixture(
            r#"{"claude":{"phoneApprovals":true,"onlyWhenAway":false,"timeoutSec":30,"shareSessionTitles":true}}"#,
        );
        f.hook
            .sessions
            .set_sessions(vec![crate::session_monitor::AgentSession {
                pid: Some(7),
                title: Some("Quarterly invoice numbers for Acme".into()),
                ..crate::session_monitor::AgentSession::new(
                    "claude:sess-1",
                    "claude",
                    "develop-int-61",
                    "develop-int",
                    "idle",
                    None,
                    1,
                    1,
                )
            }]);
        let hook = Arc::new(f.hook);
        let input =
            json(r#"{"session_id":"sess-1","tool_name":"Bash","tool_input":{"command":"ls"}}"#);
        let asking = tokio::spawn({
            let hook = hook.clone();
            async move { hook.handle(&input, std::future::pending()).await }
        });
        let card = listed(&hook.approvals).await;
        assert!(!serde_json::to_string(&card).unwrap().contains("Quarterly"));
        assert_eq!(card.project, "develop-int-61");
        hook.approvals.decide(&card.id, "skip");
        asking.await.unwrap();
    }

    #[test]
    fn the_permission_card_prefers_the_workspace_name_over_the_folder() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".workbuddy");
        std::fs::create_dir_all(&dir).unwrap();
        let folder = r"C:\Users\me\WorkBuddy\2026-11-16-08-55-00";
        std::fs::write(
            dir.join("workspace-display-names.json"),
            serde_json::json!({"workspaces": {"c:/users/me/workbuddy/2026-11-16-08-55-00": {"path": folder, "displayName": "季度报告整理"}}})
                .to_string(),
        )
        .unwrap();
        let names = WorkBuddyNames::new();
        assert_eq!(project_name(folder, home.path(), &names), "季度报告整理");
        let plain = Path::new("work").join("project");
        assert_eq!(
            project_name(&plain.display().to_string(), home.path(), &names),
            "project"
        );
    }

    #[tokio::test]
    async fn an_answer_from_the_phone_is_the_hook_output() {
        for (decision, output) in [
            (
                "allow",
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#,
            ),
            (
                "deny",
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied from the PhoneBox panel."}}}"#,
            ),
        ] {
            let f = fixture(r#"{"claude":{"timeoutSec":30}}"#);
            let hook = Arc::new(f.hook);
            let input = json(
                r#"{"session_id":"x","cwd":"/w/my-app/","tool_name":"mcp__github__create_issue","tool_input":{"title":"T","description":"why"}}"#,
            );
            let asking = tokio::spawn({
                let hook = hook.clone();
                async move { hook.handle(&input, std::future::pending()).await }
            });
            let card = listed(&hook.approvals).await;
            assert_eq!(
                (
                    card.source.as_str(),
                    card.session.as_str(),
                    card.project.as_str()
                ),
                ("Claude Code", "x", "my-app")
            );
            assert_eq!(card.tool, "Use github › create_issue");
            assert_eq!(card.description.as_deref(), Some("why"));
            assert_eq!(card.expires_at - card.created_at, 30_000);
            assert!(hook.approvals.decide(&card.id, decision));
            assert_eq!(asking.await.unwrap().as_deref(), Some(output));
        }
        // the setting off, or the user at the PC with "only when away": no prompt at all
        let off = fixture(r#"{"claude":{"phoneApprovals":false}}"#);
        assert_eq!(
            off.hook
                .handle(&json(r#"{"tool_name":"Bash"}"#), std::future::pending())
                .await,
            None
        );
        assert!(off.hook.approvals.snapshot().is_empty());
        let mut here = fixture(r#"{"claude":{"onlyWhenAway":true}}"#);
        here.hook.idle = Arc::new(|| 59.0);
        assert_eq!(
            here.hook
                .handle(&json(r#"{"tool_name":"Bash"}"#), std::future::pending())
                .await,
            None
        );
    }

    #[test]
    fn titles_and_details_say_what_the_tool_wants() {
        for (tool, title) in [
            ("Bash", "Run a command"),
            ("Edit", "Edit a file"),
            ("MultiEdit", "Edit a file"),
            ("Write", "Write a file"),
            ("NotebookEdit", "Edit a notebook"),
            ("WebFetch", "Fetch a web page"),
            ("WebSearch", "Search the web"),
            ("Read", "Read a file"),
            ("AskUserQuestion", "Claude has a question"),
            ("mcp__x__y__z", "Use x › y › z"),
            ("Task", "Use Task"),
        ] {
            assert_eq!(tool_title(tool), title);
        }
        let ti = |s: &str| json(s);
        assert_eq!(
            detail("Bash", Some(&ti(r#"{"command":"ls -la"}"#))),
            "ls -la"
        );
        assert_eq!(
            detail("Read", Some(&ti(r#"{"notebook_path":"/n.ipynb"}"#))),
            "/n.ipynb"
        );
        assert_eq!(
            detail("WebSearch", Some(&ti(r#"{"query":"rust"}"#))),
            "rust"
        );
        let content = (1..=14)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let write = detail(
            "Write",
            Some(&ti(
                &serde_json::json!({"file_path": "/a.txt", "content": content}).to_string(),
            )),
        );
        assert!(
            write.starts_with("/a.txt\n\nline 1\n") && write.ends_with("line 12\n… (2 more lines)"),
            "{write}"
        );
        let edit = detail(
            "Edit",
            Some(&ti(r#"{"file_path":"/a.cs","new_string":"x"}"#)),
        );
        assert_eq!(edit, "/a.cs\n\nx");
        assert_eq!(detail("Bash", None), "");
        // anything else is the input itself, as System.Text.Json indents it with its default escaping
        let nl = if cfg!(windows) { "\r\n" } else { "\n" };
        let fallback = detail("Glob", Some(&ti(r#"{"pattern":"<é>","n":1.50}"#)));
        assert_eq!(
            fallback,
            format!("{{{nl}  \"pattern\": \"\\u003C\\u00E9\\u003E\",{nl}  \"n\": 1.50{nl}}}")
        );
        let long = detail(
            "Bash",
            Some(&ti(
                &serde_json::json!({"command": "x".repeat(5000)}).to_string()
            )),
        );
        assert_eq!(crate::dotnet_text::utf16_len(&long), 3001);
        let curl = if cfg!(windows) { "curl.exe" } else { "curl" };
        assert_eq!(
            hook_command(8765),
            format!(
                "{curl} -s --max-time 3600 -X POST http://127.0.0.1:8765/api/hooks/claude/permission -H \"Content-Type: application/json\" --data-binary \"@-\""
            )
        );
    }
}
