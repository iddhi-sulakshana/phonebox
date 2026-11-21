//! Port of `ClaudeTranscript.cs`: the end of a Claude Code chat, `~/.claude/projects/<folder>/<sessionId>.jsonl`, for
//! the session details on the phone: the last messages and the last tool calls. Read-only, local, and only used while
//! Settings › Claude Code › "Show the conversation on the phone" is on. The cache is per instance (one for the app)
//! so tests never share it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Serialize;
use serde_json::Value;

use crate::session_monitor::AgentSession;

pub const TAIL_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_LINE_CHARS: usize = 2 * 1024 * 1024;
pub const MAX_MESSAGES: usize = 30;
pub const MAX_STEPS: usize = 12;
pub const MAX_TEXT_CHARS: usize = 4000;
/// The cache holds chat text, so it is kept small and short-lived: at most this many files, none for longer than
/// [`CACHE_IDLE_SECONDS`] without being asked for, and (see [`ClaudeTranscript::trim`]) none for a session that has
/// ended or with the setting off.
pub const MAX_CACHED_FILES: usize = 8;
pub const CACHE_IDLE_SECONDS: u64 = 60;

/// `role` is user | assistant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversationMessage {
    pub role: String,
    pub text: String,
    pub at: i64,
}

/// `detail` is a file name, the program a command starts with, or a web host: never a command line or a pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversationStep {
    pub tool: String,
    pub detail: Option<String>,
    pub at: i64,
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ConversationView {
    pub messages: Vec<ConversationMessage>,
    pub steps: Vec<ConversationStep>,
}

struct CacheEntry {
    length: u64,
    written: Option<SystemTime>,
    view: ConversationView,
    used: SystemTime,
}

#[derive(Default)]
pub struct ClaudeTranscript {
    cache: Mutex<HashMap<PathBuf, CacheEntry>>,
}

impl ClaudeTranscript {
    pub fn new() -> Self {
        Self::default()
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, CacheEntry>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_cached(&self, path: &Path) -> bool {
        self.cache().contains_key(path)
    }

    /// Drops cached chat text that can no longer be asked for: a session that is not running, nobody asking for a
    /// minute, or the setting off. The session monitor calls this on every pass.
    pub fn trim(&self, running_ids: &[String], enabled: bool, now: Option<SystemTime>) {
        let mut cache = self.cache();
        if cache.is_empty() {
            return;
        }
        let now = now.unwrap_or_else(SystemTime::now);
        cache.retain(|path, entry| {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            let idle = now
                .duration_since(entry.used)
                .is_ok_and(|d| d.as_secs_f64() > CACHE_IDLE_SECONDS as f64);
            enabled && running_ids.iter().any(|id| *id == stem) && !idle
        });
    }

    /// The answer to the phone's request for one session. With the setting off it carries no text at all; otherwise
    /// only a Claude Code session the monitor sees running is read.
    pub fn for_phone(
        &self,
        enabled: bool,
        running: &[AgentSession],
        id: &str,
        home: &Path,
    ) -> Value {
        let ids: Vec<String> = running
            .iter()
            .filter(|s| s.tool == "claude")
            .map(|s| s.id.strip_prefix("claude:").unwrap_or(&s.id).to_string())
            .collect();
        self.trim(&ids, enabled, None);
        if !enabled {
            return serde_json::json!({ "enabled": false });
        }
        let live = running.iter().any(|s| s.id == id && s.tool == "claude");
        let file = if live {
            find_file(id.strip_prefix("claude:").unwrap_or(id), home)
        } else {
            None
        };
        let view = file.map(|f| self.read(&f)).unwrap_or_default();
        serde_json::json!({ "enabled": true, "messages": view.messages, "steps": view.steps })
    }

    pub fn read(&self, path: &Path) -> ConversationView {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(info) = std::fs::metadata(path) else {
            return ConversationView::default();
        };
        if !info.is_file() {
            return ConversationView::default();
        }
        let written = info.modified().ok();
        {
            let mut cache = self.cache();
            if let Some(hit) = cache.get_mut(path)
                && hit.length == info.len()
                && hit.written == written
            {
                hit.used = SystemTime::now();
                return hit.view.clone();
            }
        }
        let read = || -> std::io::Result<(u64, Vec<u8>, bool)> {
            let mut file = std::fs::File::open(path)?;
            let length = file.metadata()?.len();
            let start = length.saturating_sub(TAIL_BYTES);
            file.seek(SeekFrom::Start(start))?;
            let mut bytes = Vec::with_capacity((length - start) as usize);
            file.take(length - start).read_to_end(&mut bytes)?;
            Ok((length, bytes, start > 0))
        };
        let Ok((length, bytes, cut)) = read() else {
            return ConversationView::default();
        };
        let text = String::from_utf8_lossy(&bytes);
        // a seek cuts the first line
        let view = parse(text.split('\n').skip(usize::from(cut)));
        let mut cache = self.cache();
        cache.insert(
            path.to_path_buf(),
            CacheEntry {
                length,
                written,
                view: view.clone(),
                used: SystemTime::now(),
            },
        );
        let extra = cache.len().saturating_sub(MAX_CACHED_FILES);
        let evict: Vec<PathBuf> = cache
            .keys()
            .filter(|k| k.as_path() != path)
            .take(extra)
            .cloned()
            .collect();
        for key in evict {
            cache.remove(&key);
        }
        view
    }
}

/// The transcript of a session id, or None: the id is checked, so it cannot point outside the projects folder.
pub fn find_file(session_id: &str, home: &Path) -> Option<PathBuf> {
    let valid = (8..=64).contains(&session_id.len())
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !valid {
        return None;
    }
    let projects = home.join(".claude").join("projects");
    std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path().join(format!("{session_id}.jsonl")))
        .find(|p| p.is_file())
}

fn str_of<'a>(e: &'a Value, key: &str) -> Option<&'a str> {
    e.get(key).and_then(Value::as_str)
}

pub fn parse<'a>(lines: impl IntoIterator<Item = &'a str>) -> ConversationView {
    use std::collections::HashSet;
    let mut messages: Vec<ConversationMessage> = Vec::new();
    let mut steps: Vec<(String, String, Option<String>, i64)> = Vec::new();
    let mut pending: HashSet<String> = HashSet::new(); // tool calls that have not come back yet
    let mut unnamed = 0u64;
    fn add_user(
        messages: &mut Vec<ConversationMessage>,
        pending: &mut HashSet<String>,
        text: Option<&str>,
        at: i64,
    ) {
        let Some(text) = clean(text) else { return };
        messages.push(ConversationMessage {
            role: "user".into(),
            text,
            at,
        });
        pending.clear(); // a new message means the turn before it is over, whatever it left unanswered
    }
    fn add_assistant(messages: &mut Vec<ConversationMessage>, text: Option<&str>, at: i64) {
        let Some(text) = clean(text) else { return };
        // Claude Code writes each block of one reply as its own record: show them as one message
        match messages.last_mut() {
            Some(last) if last.role == "assistant" => {
                last.text = limit(&format!("{}\n\n{text}", last.text))
            }
            _ => messages.push(ConversationMessage {
                role: "assistant".into(),
                text: limit(&text),
                at,
            }),
        }
    }
    for line in lines {
        if line.is_empty()
            || (line.len() > MAX_LINE_CHARS && crate::dotnet_text::utf16_len(line) > MAX_LINE_CHARS)
        {
            continue;
        }
        if !line.contains("\"user\"") && !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue; // a line still being written
        };
        let Some(kind @ ("user" | "assistant")) = str_of(&rec, "type") else {
            continue;
        };
        if rec.get("isSidechain") == Some(&Value::Bool(true)) {
            continue; // a sub-agent's own chat
        }
        let Some(content) = rec
            .get("message")
            .filter(|m| m.is_object())
            .and_then(|m| m.get("content"))
        else {
            continue;
        };
        let at = str_of(&rec, "timestamp")
            .and_then(|t| crate::dotnet_text::parse_date_ms(t, true))
            .unwrap_or(0);
        let meta = rec.get("isMeta") == Some(&Value::Bool(true));
        let user = kind == "user";
        if let Value::String(s) = content {
            if user && !meta {
                add_user(&mut messages, &mut pending, Some(s), at);
            }
            continue;
        }
        let Value::Array(blocks) = content else {
            continue;
        };
        for block in blocks {
            match str_of(block, "type") {
                Some("text") if user && !meta => {
                    add_user(&mut messages, &mut pending, str_of(block, "text"), at)
                }
                Some("text") if !user => add_assistant(&mut messages, str_of(block, "text"), at),
                Some("image") if user && !meta => {
                    add_user(&mut messages, &mut pending, Some("[image]"), at)
                }
                Some("tool_use") if !user => {
                    let Some(tool) = str_of(block, "name").filter(|n| !n.is_empty()) else {
                        continue;
                    };
                    let id = match str_of(block, "id") {
                        Some(id) => id.to_string(),
                        None => {
                            unnamed += 1;
                            format!("\u{0}unnamed-{unnamed}")
                        }
                    };
                    steps.push((
                        id.clone(),
                        tool.to_string(),
                        detail(tool, block.get("input")),
                        at,
                    ));
                    pending.insert(id);
                }
                Some("tool_result") if user => {
                    if let Some(done) = str_of(block, "tool_use_id") {
                        pending.remove(done);
                    }
                }
                _ => {}
            }
        }
    }
    let skip_messages = messages.len().saturating_sub(MAX_MESSAGES);
    let skip_steps = steps.len().saturating_sub(MAX_STEPS);
    ConversationView {
        messages: messages.into_iter().skip(skip_messages).collect(),
        steps: steps
            .into_iter()
            .skip(skip_steps)
            .map(|(id, tool, detail, at)| ConversationStep {
                running: pending.contains(&id),
                tool,
                detail,
                at,
            })
            .collect(),
    }
}

/// Drops what Claude Code adds around a message (system reminders, slash-command and background-task markers).
fn clean(text: Option<&str>) -> Option<String> {
    static REMINDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").expect("valid")
    });
    let text = text.filter(|t| !t.trim().is_empty())?;
    let text = REMINDER.replace_all(text, "");
    let text = text.trim();
    let noise = [
        "<local-command",
        "<command-",
        "<task-notification",
        "[Request interrupted",
    ];
    if text.is_empty() || noise.iter().any(|n| text.starts_with(n)) {
        return None;
    }
    Some(limit(text))
}

fn limit(text: &str) -> String {
    crate::dotnet_text::cut(text, MAX_TEXT_CHARS)
}

/// Only what says what a step touched: a file name, the program a command starts with, a web host.
pub fn detail(tool: &str, input: Option<&Value>) -> Option<String> {
    let input = input.filter(|i| i.is_object())?;
    match tool {
        "Edit" | "MultiEdit" | "Write" | "Read" | "NotebookEdit" => {
            let file = str_of(input, "file_path").or_else(|| str_of(input, "notebook_path"))?;
            let name = crate::dotnet_text::file_name(file.trim_end_matches(['\\', '/']));
            (!name.is_empty()).then(|| name.to_string())
        }
        "Bash" => program(str_of(input, "command")),
        "WebFetch" => reqwest::Url::parse(str_of(input, "url")?)
            .ok()
            .map(|u| u.host_str().unwrap_or("").to_string()),
        _ => None,
    }
}

/// NAME=value, where the name and the `=` were written as they are (a quoted "NAME=value" is a command's name).
fn is_assignment(word: &str, plain_prefix: usize) -> bool {
    let Some(eq) = word.find('=') else {
        return false;
    };
    let eq_units = crate::dotnet_text::utf16_len(&word[..eq]);
    eq_units > 0
        && eq_units < plain_prefix
        && !word.chars().next().is_some_and(char::is_numeric)
        && word[..eq].chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Words that start a command line without saying what it runs.
const SKIPPED: [&str; 15] = [
    "cd", "export", "set", "unset", "source", "for", "if", "while", "do", "then", "else", "time",
    "sudo", "env", "rtk",
];
/// Words after which the rest of the segment is an argument list or a loop header, never the program.
const SKIP_SEGMENT: [&str; 8] = [
    "cd", "export", "set", "unset", "source", "for", "if", "while",
];

/// The program a command line starts, past a leading cd, subshell, assignment or wrapper; never its arguments. The
/// line is read with quotes in mind, so a quoted `;` or `&` does not start a new command and nothing inside quotes can
/// come out. When it cannot tell (an unclosed quote, a `$( )` or backtick, an escape) the answer is None.
pub fn program(command: Option<&str>) -> Option<String> {
    struct Word {
        text: String,
        plain: bool,
        prefix: usize,
    }
    let command = command.filter(|c| !c.trim().is_empty())?;
    let chars: Vec<char> = command.chars().collect();
    let mut segments: Vec<Vec<Word>> = vec![Vec::new()];
    let mut text = String::new();
    let mut plain = true;
    let mut prefix = 0usize; // how much of the word (UTF-16 units) was written without any quote or escape
    let mut in_word = false;
    let mut quote = '\0';
    let taint = |plain: &mut bool, prefix: &mut usize, text: &String| {
        if *plain {
            *prefix = crate::dotnet_text::utf16_len(text);
            *plain = false;
        }
    };
    let end_word = |segments: &mut Vec<Vec<Word>>,
                    text: &mut String,
                    plain: &mut bool,
                    prefix: &mut usize,
                    in_word: &mut bool| {
        if *in_word {
            let units = crate::dotnet_text::utf16_len(text);
            segments.last_mut().expect("one segment").push(Word {
                text: std::mem::take(text),
                plain: *plain,
                prefix: if *plain { units } else { *prefix },
            });
        }
        text.clear();
        *plain = true;
        *prefix = 0;
        *in_word = false;
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if quote != '\0' {
            if c == quote {
                quote = '\0';
            } else if c == '\\' && quote == '"' && next.is_some() {
                i += 1;
                text.push(chars[i]);
            } else if quote == '"' && (c == '`' || (c == '$' && matches!(next, Some('(' | '{')))) {
                // inside double quotes a substitution still runs, and its own quotes would end this one
                return None;
            } else {
                text.push(c);
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                taint(&mut plain, &mut prefix, &text);
                quote = c;
                in_word = true;
            }
            '\\' => {
                taint(&mut plain, &mut prefix, &text);
                in_word = true;
                if next.is_some() {
                    i += 1;
                    text.push(chars[i]);
                }
            }
            '`' => return None, // command substitution: what it holds cannot be told apart from a program
            '$' if matches!(next, Some('(' | '{')) => return None, // $( ) and ${ }, also $(( ))
            '$' => {
                taint(&mut plain, &mut prefix, &text);
                in_word = true;
                text.push(c);
            }
            // an array assignment, NAME=(a b): its values are not programs
            '(' if in_word && text.ends_with('=') => return None,
            ' ' | '\t' | '(' | ')' | '{' | '}' => end_word(
                &mut segments,
                &mut text,
                &mut plain,
                &mut prefix,
                &mut in_word,
            ),
            ';' | '&' | '|' | '\n' => {
                end_word(
                    &mut segments,
                    &mut text,
                    &mut plain,
                    &mut prefix,
                    &mut in_word,
                );
                if segments.last().is_some_and(|s| !s.is_empty()) {
                    segments.push(Vec::new());
                }
            }
            _ => {
                text.push(c);
                in_word = true;
            }
        }
        i += 1;
    }
    if quote != '\0' {
        return None; // an unclosed quote: there is no telling what is a command
    }
    end_word(
        &mut segments,
        &mut text,
        &mut plain,
        &mut prefix,
        &mut in_word,
    );
    for segment in &segments {
        for word in segment {
            if is_assignment(&word.text, word.prefix) {
                continue; // VAR=value before the command
            }
            if SKIPPED.contains(&word.text.as_str()) {
                if SKIP_SEGMENT.contains(&word.text.as_str()) {
                    break;
                }
                continue;
            }
            if word.text.starts_with('-') {
                return None; // an option of a wrapper (time -f.., sudo -u ..): not something to name
            }
            let len = crate::dotnet_text::utf16_len(&word.text);
            let simple = word
                .text
                .chars()
                .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/'));
            if !word.plain || len == 0 || len > 40 || !simple {
                return None;
            }
            let name = crate::dotnet_text::file_name(&word.text);
            return Some(if name.is_empty() {
                word.text.clone()
            } else {
                name.to_string()
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TS: &str = "2026-11-19T08:00:00.000Z";

    fn user(content: Value, sidechain: bool, meta: bool) -> String {
        json!({"type": "user", "isSidechain": sidechain, "isMeta": meta, "timestamp": TS,
               "message": {"role": "user", "content": content}})
        .to_string()
    }
    fn u(text: &str) -> String {
        user(json!(text), false, false)
    }
    fn assistant(blocks: Vec<Value>) -> String {
        json!({"type": "assistant", "isSidechain": false, "timestamp": TS,
               "message": {"role": "assistant", "content": blocks}})
        .to_string()
    }
    fn text(t: &str) -> Value {
        json!({"type": "text", "text": t})
    }
    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": input})
    }
    fn tool_result(id: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": "OUTPUT"})
    }
    fn parse_all(lines: &[String]) -> ConversationView {
        parse(lines.iter().map(String::as_str))
    }

    #[test]
    fn messages_come_in_order_tool_results_and_reminders_are_left_out_and_replies_are_merged() {
        let view = parse_all(&[
            u("first question\n<system-reminder>SECRET REMINDER</system-reminder>"),
            assistant(vec![text("part one")]),
            assistant(vec![tool_use(
                "t1",
                "Read",
                json!({"file_path": "/a/b/file.cs"}),
            )]),
            user(json!([tool_result("t1")]), false, false),
            assistant(vec![text("part two")]),
            user(json!([text("second question")]), false, false),
            u("<local-command-stdout>noise</local-command-stdout>"),
            user(json!("hidden by Claude Code"), false, true),
            user(json!("a sub-agent's own chat"), true, false),
        ]);
        let got: Vec<(&str, &str)> = view
            .messages
            .iter()
            .map(|m| (m.role.as_str(), m.text.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("user", "first question"),
                ("assistant", "part one\n\npart two"),
                ("user", "second question")
            ]
        );
        assert!(view.messages.iter().all(|m| {
            !["SECRET", "OUTPUT", "noise"]
                .iter()
                .any(|s| m.text.contains(s))
        }));
        assert_ne!(view.messages[0].at, 0);
    }

    #[test]
    fn steps_show_only_names_and_never_arguments_or_output() {
        let view = parse_all(&[
            assistant(vec![tool_use(
                "a",
                "Bash",
                json!({"command": "git push --force https://user:PASSWORD@example.com/x.git"}),
            )]),
            assistant(vec![tool_use(
                "b",
                "Edit",
                json!({"file_path": "/home/me/project/Secret Plan.md", "old_string": "OLD", "new_string": "NEW"}),
            )]),
            assistant(vec![tool_use(
                "c",
                "WebFetch",
                json!({"url": "https://example.com/private/path?token=abc"}),
            )]),
            assistant(vec![tool_use("d", "Grep", json!({"pattern": "PASSWORD"}))]),
        ]);
        let details: Vec<Option<&str>> = view.steps.iter().map(|s| s.detail.as_deref()).collect();
        assert_eq!(
            details,
            [
                Some("git"),
                Some("Secret Plan.md"),
                Some("example.com"),
                None
            ]
        );
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("PASSWORD") && !json.contains("OLD"));
    }

    #[test]
    fn a_command_is_shown_as_the_program_it_runs() {
        for (command, expected) in [
            ("cd /x/y && docker compose up -d", Some("docker")),
            ("rtk grep -rn secret .", Some("grep")),
            ("FOO=1 BAR=2 dotnet test", Some("dotnet")),
            ("(docker ps; echo done)", Some("docker")),
            ("for f in *.cs; do rm $f; done", Some("rm")),
            ("/usr/bin/git status", Some("git")),
            ("cd /only/this", None),
            ("echo 'hi there' | tr a b", Some("echo")),
            ("   ", None),
            ("export TOKEN='SECRET_VALUE; git status'", None),
            (
                "export TOKEN='SECRET_VALUE; git status'; git status",
                Some("git"),
            ),
            ("echo \"a; b && c\"", Some("echo")),
            ("API_KEY='abc def' dotnet test", Some("dotnet")),
            ("echo 'unclosed; git push", None),
            ("$(rm -rf /tmp/x)", None),
            ("`whoami` --version", None),
            ("\"git\" status", None),
            ("VALUE=$((123456789)) git status", None),
            ("VALUE=$(whoami) git status", None),
            ("A=${SECRET_PART} git status", None),
            ("time -fPRIVATE_TAG git status", None),
            ("sudo -u root git status", None),
            ("env --unset=PATH git status", None),
            ("time git status", Some("git")),
            ("FOO=1 git status", Some("git")),
            ("rtk grep -rn x .", Some("grep")),
            ("rm $f", Some("rm")),
            ("VALUES=(PRIVATE_VALUE); git status", None),
            ("VALUES+=(PRIVATE_VALUE a b) git status", None),
            ("\"HELPER=prod\" PRIVATE_TOKEN", None),
            ("'HELPER=prod' PRIVATE_TOKEN", None),
            ("\\HELPER=prod PRIVATE_TOKEN", None),
            ("HELPER=\"prod x\" git status", Some("git")),
            ("A=1 B='x y' dotnet test", Some("dotnet")),
            ("VALUE=\"$(printf \"x; 123456789 \" z)\" git status", None),
            ("echo \"$(date)\"", None),
            ("echo \"`date`\"", None),
            ("echo \"${HOME}\"", None),
            ("echo \"$HOME\"", Some("echo")),
            ("echo '$(not run); ${x}'", Some("echo")),
            ("echo \"\\$(literal)\"", Some("echo")),
            ("(git status)", Some("git")),
            ("ls\\ -la", None), // an escaped space makes one word: not something to name
        ] {
            assert_eq!(program(Some(command)).as_deref(), expected, "{command}");
        }
        assert_eq!(program(None), None);
    }

    #[test]
    fn a_call_without_an_answer_is_running_until_a_new_message_ends_that_turn() {
        let running = parse_all(&[
            assistant(vec![tool_use("a", "Read", json!({"file_path": "/x/a.cs"}))]),
            user(json!([tool_result("a")]), false, false),
            assistant(vec![tool_use(
                "b",
                "Bash",
                json!({"command": "dotnet test"}),
            )]),
        ]);
        assert_eq!(
            running.steps.iter().map(|s| s.running).collect::<Vec<_>>(),
            [false, true]
        );

        let abandoned = parse_all(&[
            assistant(vec![tool_use(
                "b",
                "Bash",
                json!({"command": "dotnet test"}),
            )]),
            u("never mind"),
        ]);
        assert_eq!(abandoned.steps.len(), 1);
        assert!(!abandoned.steps[0].running);
    }

    #[test]
    fn only_the_last_messages_are_kept_and_long_text_is_cut() {
        let mut lines: Vec<String> = (0..80).map(|i| u(&format!("message {i}"))).collect();
        lines.push(assistant(vec![text(&"x".repeat(9000))]));
        let view = parse_all(&lines);
        assert_eq!(view.messages.len(), MAX_MESSAGES);
        assert_eq!(
            crate::dotnet_text::utf16_len(&view.messages[MAX_MESSAGES - 1].text),
            MAX_TEXT_CHARS + 1
        );
        assert_eq!(view.messages[MAX_MESSAGES - 2].text, "message 79");
    }

    #[test]
    fn broken_lines_are_skipped() {
        let lines = [
            "{\"type\":\"user\",\"message\":{\"content\":\"cut off".to_string(),
            "not json at all \"user\"".to_string(),
            u("fine"),
        ];
        let view = parse_all(&lines);
        assert_eq!(view.messages.len(), 1);
        assert_eq!(view.messages[0].text, "fine");
    }

    fn projects(root: &Path) -> PathBuf {
        let dir = root.join(".claude").join("projects").join("-some-folder");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn find_file_refuses_ids_that_could_leave_the_projects_folder() {
        let root = tempfile::tempdir().unwrap();
        let dir = projects(root.path());
        std::fs::write(dir.join("abcdef12-3456.jsonl"), u("hi")).unwrap();
        std::fs::write(root.path().join("outside.jsonl"), "x").unwrap();
        assert!(find_file("abcdef12-3456", root.path()).is_some());
        assert_eq!(find_file("../../outside", root.path()), None);
        assert_eq!(find_file("..\\..\\outside", root.path()), None);
        assert_eq!(find_file("short", root.path()), None);
        assert_eq!(find_file("abcdef12-9999", root.path()), None);
    }

    fn running(id: &str, tool: &str) -> AgentSession {
        AgentSession::new(id, tool, "x", "p", "idle", None, 1, 1)
    }

    #[test]
    fn the_chat_cache_is_bounded_and_drops_a_session_that_has_ended_or_the_setting_being_off() {
        let root = tempfile::tempdir().unwrap();
        let dir = projects(root.path());
        let chats = ClaudeTranscript::new();
        let id = |i: usize| format!("aaaaaaaa-{i:04}");
        let path = |i: usize| dir.join(format!("{}.jsonl", id(i)));
        for i in 0..MAX_CACHED_FILES + 5 {
            std::fs::write(path(i), u(&format!("text {i}")) + "\n").unwrap();
            chats.read(&path(i));
        }
        let cached = (0..MAX_CACHED_FILES + 5)
            .filter(|&i| chats.is_cached(&path(i)))
            .count();
        assert!(cached <= MAX_CACHED_FILES, "{cached} chats cached");

        // one is read for the phone: the others, whose sessions are not running, are dropped
        let keep = path(0);
        let live = format!("claude:{}", id(0));
        chats.for_phone(true, &[running(&live, "claude")], &live, root.path());
        assert!(chats.is_cached(&keep));
        assert_eq!(
            (0..MAX_CACHED_FILES + 5)
                .filter(|&i| chats.is_cached(&path(i)))
                .count(),
            1
        );
        // the session ends, or the setting goes off: nothing stays in memory
        chats.for_phone(true, &[], &live, root.path());
        assert!(!chats.is_cached(&keep));
        chats.read(&keep);
        chats.for_phone(false, &[running(&live, "claude")], &live, root.path());
        assert!(!chats.is_cached(&keep));
    }

    #[test]
    fn trim_drops_cached_chats_without_any_further_request() {
        let root = tempfile::tempdir().unwrap();
        let dir = projects(root.path());
        let chats = ClaudeTranscript::new();
        let make = |id: &str| {
            let p = dir.join(format!("{id}.jsonl"));
            std::fs::write(&p, u(&format!("text for {id}")) + "\n").unwrap();
            chats.read(&p);
            p
        };
        let running_chat = make("bbbbbbbb-0001");
        let ended = make("bbbbbbbb-0002");
        let idle = make("bbbbbbbb-0003");
        assert!(
            chats.is_cached(&running_chat) && chats.is_cached(&ended) && chats.is_cached(&idle)
        );

        let ids = ["bbbbbbbb-0001".to_string(), "bbbbbbbb-0003".to_string()];
        chats.trim(&ids, true, None);
        assert!(chats.is_cached(&running_chat));
        assert!(!chats.is_cached(&ended));
        let later = SystemTime::now() + std::time::Duration::from_secs(CACHE_IDLE_SECONDS + 5);
        chats.trim(&ids, true, Some(later));
        assert!(!chats.is_cached(&running_chat));
        assert!(!chats.is_cached(&idle));
        let again = make("bbbbbbbb-0001");
        chats.trim(&["bbbbbbbb-0001".to_string()], false, None);
        assert!(!chats.is_cached(&again));
    }

    fn chat(root: &Path, id: &str, text: &str) -> String {
        std::fs::write(projects(root).join(format!("{id}.jsonl")), u(text) + "\n").unwrap();
        id.to_string()
    }

    #[test]
    fn for_phone_sends_no_text_while_the_setting_is_off() {
        let root = tempfile::tempdir().unwrap();
        let id = format!(
            "claude:{}",
            chat(root.path(), "abcdef12-3456", "PRIVATE PROMPT")
        );
        let answer =
            ClaudeTranscript::new().for_phone(false, &[running(&id, "claude")], &id, root.path());
        assert_eq!(
            serde_json::to_string(&answer).unwrap(),
            r#"{"enabled":false}"#
        );
    }

    #[test]
    fn for_phone_reads_only_a_running_claude_session() {
        let root = tempfile::tempdir().unwrap();
        let id = chat(root.path(), "abcdef12-3456", "the question");
        let chats = ClaudeTranscript::new();
        let ask = |asked: &str, live: &[AgentSession]| {
            serde_json::to_string(&chats.for_phone(true, live, asked, root.path())).unwrap()
        };
        let claude = format!("claude:{id}");
        assert!(ask(&claude, &[running(&claude, "claude")]).contains("the question"));
        assert!(!ask(&claude, &[]).contains("the question"));
        let codex = format!("codex:{id}");
        assert!(!ask(&codex, &[running(&codex, "codex")]).contains("the question"));
        assert!(
            !ask("claude:../../x", &[running("claude:../../x", "claude")]).contains("the question")
        );
        let empty = ask(&claude, &[]);
        assert!(empty.contains(r#""messages":[]"#), "{empty}");
        assert_eq!(
            serde_json::from_str::<Value>(&ask(&claude, &[running(&claude, "claude")])).unwrap()["messages"]
                [0],
            json!({"role": "user", "text": "the question", "at": 1_795_075_200_000_i64})
        );
    }

    #[test]
    fn read_follows_the_file_as_it_grows() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("chat.jsonl");
        let chats = ClaudeTranscript::new();
        std::fs::write(&path, u("one") + "\n").unwrap();
        assert_eq!(chats.read(&path).messages.len(), 1);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut f, (assistant(vec![text("two")]) + "\n").as_bytes())
            .unwrap();
        assert_eq!(chats.read(&path).messages.len(), 2);
    }

    #[test]
    fn only_the_tail_of_a_huge_transcript_is_read() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("big.jsonl");
        let filler = u(&"y".repeat(1000)) + "\n";
        let mut body = filler.repeat((TAIL_BYTES as usize / filler.len()) + 50);
        body.push_str(&(u("the end") + "\n"));
        std::fs::write(&path, body).unwrap();
        let view = ClaudeTranscript::new().read(&path);
        assert_eq!(view.messages.len(), MAX_MESSAGES);
        assert_eq!(view.messages.last().unwrap().text, "the end");
    }
}
