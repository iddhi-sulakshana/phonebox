//! Codex's quota, fully local: the last `rate_limits` record in the newest rollout files under `~/.codex/sessions`, and
//! the token usage of the same file.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{CodexUsage, QuotaLimit, QuotaProvider};
use crate::dotnet_text::{format_custom, parse_date_ms, upper_invariant};
use crate::json_text::{Node, parse};

/// The tail read from a rollout file.
const TAIL_BYTES: u64 = 1 << 20;

/// The reading from the 5 newest rollout files (all ages, every pass); None without a sessions folder.
pub fn read_codex(root: &Path) -> io::Result<Option<QuotaProvider>> {
    if !root.is_dir() {
        return Ok(None);
    }
    let found = RefCell::new(Vec::new());
    crate::session_monitor::walk_files(root, &|path, meta| {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        if is_rollout(&name)
            && let Ok(modified) = meta.modified()
        {
            found.borrow_mut().push((path.to_path_buf(), modified));
        }
        false
    });
    let mut files = found.into_inner();
    files.sort_by_key(|f| std::cmp::Reverse(f.1)); // newest first; ties keep the walk's order
    for (path, modified) in files.into_iter().take(5) {
        let Some(line) = last_line_with(&path, "\"rate_limits\"")? else {
            continue;
        };
        let Ok(record) = parse(&line, false) else {
            continue;
        };
        // a record of another shape is skipped (C# threw on it and showed .NET's message instead of a reading)
        let Some(rl) = record
            .get("payload")
            .and_then(|p| p.get("rate_limits"))
            .filter(|r| r.is_object())
        else {
            continue;
        };
        let modified_ms = unix_ms(modified);

        let mut limits = Vec::new();
        for key in ["primary", "secondary"] {
            let Some(w) = rl.get(key).filter(|w| w.is_object()) else {
                continue;
            };
            let mins = w.num("window_minutes").unwrap_or(0.0) as i64;
            let label = match mins {
                300 => "5-hour".to_string(),
                10080 => "Weekly".to_string(),
                m if m >= 1440 => format!("{}-day", m / 1440),
                m if m > 0 => format!("{}-hour", m / 60),
                _ => key.to_string(),
            };
            let resets = match w.long("resets_at") {
                Some(at) => Some(at.saturating_mul(1000)),
                None => w
                    .num("resets_in_seconds")
                    .map(|s| modified_ms + (s * 1000.0) as i64),
            };
            let pct = w.num("used_percent").unwrap_or(0.0);
            limits.push(QuotaLimit::new(
                &label,
                pct,
                resets,
                (mins > 0).then(|| mins * 60),
                severity(pct),
            ));
        }

        let mut extra = None;
        if let Some(credits) = rl.get("credits").filter(|c| c.is_object()) {
            let has = credits.get("has_credits");
            if credits.get("unlimited") == Some(&Node::Bool(true)) {
                extra = Some("Credits: unlimited".to_string());
            } else if has == Some(&Node::Bool(true))
                && let Some(balance) = credits
                    .text("balance")
                    .and_then(|b| b.trim().parse::<f64>().ok())
                && balance.is_finite()
            {
                extra = Some(format!("Credits: {}", format_custom(balance, 0, true)));
            } else if has == Some(&Node::Bool(false)) {
                extra = Some("Credits not enabled".to_string());
            }
        }
        let reached = limits.iter().any(|l| l.percent >= 100.0)
            || matches!(rl.get("rate_limit_reached_type"), Some(Node::String(_)))
            || rl.get("spend_control_reached") == Some(&Node::Bool(true));
        if reached {
            extra = Some(match extra {
                Some(e) if !e.is_empty() => format!("Limit reached · {e}"),
                _ => "Limit reached".to_string(),
            });
        }
        let plan = match rl.text("plan_type").filter(|p| !p.is_empty()) {
            Some(p) => {
                let mut chars = p.chars();
                let first = chars
                    .next()
                    .map(|c| upper_invariant(&c.to_string()))
                    .unwrap_or_default();
                first + chars.as_str()
            }
            None => "Codex".to_string(),
        };
        let mut reading = QuotaProvider::new(
            "codex",
            "Codex",
            &plan,
            limits,
            extra,
            Some(modified_ms),
            None,
        );
        reading.codex = read_codex_usage(&path)?;
        return Ok(Some(reading));
    }
    Ok(Some(QuotaProvider::new(
        "codex",
        "Codex",
        "Codex",
        Vec::new(),
        None,
        None,
        Some("No quota data in recent sessions".into()),
    )))
}

/// `rollout-*.jsonl` (case-insensitively on Windows, as .NET matches there).
fn is_rollout(name: &str) -> bool {
    if cfg!(windows) {
        let name = name.to_ascii_lowercase();
        name.starts_with("rollout-") && name.ends_with(".jsonl")
    } else {
        name.starts_with("rollout-") && name.ends_with(".jsonl")
    }
}

fn severity(pct: f64) -> &'static str {
    if pct >= 95.0 {
        "critical"
    } else if pct >= 80.0 {
        "warning"
    } else {
        "normal"
    }
}

fn unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// The lines of the file's last 1 MiB, and the first index to trust: a tail that starts mid-file starts mid-line. The
/// length is read once, so an append while reading can't shift the window past a line that index trusts.
fn tail_lines(path: &Path) -> io::Result<(Vec<String>, usize)> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let len = file_len.min(TAIL_BYTES);
    file.seek(SeekFrom::Start(file_len - len))?;
    let mut buf = vec![0u8; usize::try_from(len).unwrap_or(0)];
    file.read_exact(&mut buf)?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&buf)
        .split('\n')
        .map(String::from)
        .collect();
    let min_index = usize::from(file_len > len);
    if min_index == 0
        && let Some(first) = lines.first_mut()
    {
        *first = first.trim_start_matches('\u{feff}').to_string();
    }
    Ok((lines, min_index))
}

/// The model, effort and token counts of a rollout file, from its last `token_count` and `turn_context` records.
pub fn read_codex_usage(path: &Path) -> io::Result<Option<CodexUsage>> {
    let (lines, min_index) = tail_lines(path)?;
    let mut updated_at = unix_ms(std::fs::metadata(path)?.modified()?);
    let (mut model, mut effort, mut info): (Option<String>, Option<String>, Option<Node>) =
        (None, None, None);
    for line in lines.iter().skip(min_index).rev() {
        if info.is_some() && model.is_some() {
            break;
        }
        if !line.contains("\"token_count\"") && !line.contains("\"turn_context\"") {
            continue;
        }
        // a partial line at the start of the tail, or a record of another shape
        let Ok(record) = parse(line, false) else {
            continue;
        };
        let Some(payload) = record.get("payload") else {
            continue;
        };
        if info.is_none()
            && payload.text("type") == Some("token_count")
            && let Some(token_info) = payload.get("info").filter(|i| i.is_object())
        {
            info = Some(token_info.clone());
            if let Some(at) = record
                .text("timestamp")
                .and_then(|t| parse_date_ms(t, false))
            {
                updated_at = at;
            }
        }
        if model.is_none() && record.text("type") == Some("turn_context") {
            model = payload.text("model").map(String::from);
            effort = payload.text("effort").map(String::from);
        }
    }
    if info.is_none() && model.is_none() {
        return Ok(None);
    }
    let token = info.unwrap_or(Node::Null);
    let total = token.get("total_token_usage");
    let last = token.get("last_token_usage");
    let long = |node: Option<&Node>, key: &str| node.and_then(|n| n.long(key));
    Ok(Some(CodexUsage {
        model,
        effort,
        total_input: long(total, "input_tokens"),
        total_cached: long(total, "cached_input_tokens"),
        total_output: long(total, "output_tokens"),
        total_reasoning: long(total, "reasoning_output_tokens"),
        total_tokens: long(total, "total_tokens"),
        last_input: long(last, "input_tokens"),
        last_output: long(last, "output_tokens"),
        context_window: token.long("model_context_window"),
        updated_at,
    }))
}

/// The last complete line of the file's last 1 MiB that holds `needle` and `"primary"` and parses: a line cut off
/// mid-write (Codex stopped, or the file read while it appends) is skipped for the one before it.
pub fn last_line_with(path: &Path, needle: &str) -> io::Result<Option<String>> {
    let (lines, min_index) = tail_lines(path)?;
    Ok(lines.into_iter().skip(min_index).rev().find(|line| {
        line.contains(needle) && line.contains("\"primary\"") && parse(line, false).is_ok()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::QuotaMonitor;
    use std::path::PathBuf;

    fn reading(percent: u32) -> String {
        format!(
            r#"{{"timestamp":"2026-11-16T10:00:00Z","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"primary":{{"used_percent":{percent},"window_minutes":300,"resets_at":4102444800}},"secondary":{{"used_percent":3,"window_minutes":10080,"resets_at":4102444800}}}}}}}}"#
        )
    }

    fn log(root: &Path, lines: &[&str]) -> PathBuf {
        let dir = root.join("2026").join("11").join("16");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-11-16T10-00-00-test.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn agent(root: &Path) -> QuotaMonitor {
        QuotaMonitor {
            codex_sessions_root: root.to_path_buf(),
            ..QuotaMonitor::new(None, root, root)
        }
    }

    fn codex(agent: &QuotaMonitor) -> QuotaProvider {
        let all: Vec<_> = agent
            .snapshot(crate::server::now_ms())
            .into_iter()
            .filter(|p| p.id == "codex")
            .collect();
        assert_eq!(all.len(), 1, "{all:?}");
        all.into_iter().next().unwrap()
    }

    #[test]
    fn a_complete_last_reading_is_used() {
        let root = tempfile::tempdir().unwrap();
        log(
            root.path(),
            &[r#"{"type":"session_meta"}"#, &(reading(12) + "\n")],
        );
        let agent = agent(root.path());
        agent.codex_tick();
        assert_eq!(codex(&agent).error, None);
        assert_eq!(codex(&agent).limits[0].percent, 12.0);
    }

    #[test]
    fn a_last_line_cut_off_mid_write_falls_back_to_the_reading_before_it() {
        // Codex was stopped, or PhoneBox read the file, while the newest reading was half written
        let root = tempfile::tempdir().unwrap();
        let torn = reading(55);
        log(
            root.path(),
            &[
                r#"{"type":"session_meta"}"#,
                &reading(12),
                &torn[..torn.len() / 2 + 40],
            ],
        );
        let agent = agent(root.path());
        agent.codex_tick();
        assert_eq!(codex(&agent).error, None);
        assert_eq!(codex(&agent).limits[0].percent, 12.0);
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_log_keeps_the_last_reading() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = log(
            root.path(),
            &[r#"{"type":"session_meta"}"#, &(reading(12) + "\n")],
        );
        let agent = agent(root.path());
        agent.codex_tick();
        {
            let _lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(&path)
                .unwrap();
            agent.codex_tick();
        }
        assert_eq!(codex(&agent).limits[0].percent, 12.0);
    }

    #[test]
    fn codex_rollouts_under_one_megabyte_do_not_drop_first_line_turn_context_or_rate_limits() {
        let dir = tempfile::tempdir().unwrap();
        let usage_file = dir.path().join("rollout-line0.jsonl");
        std::fs::write(
            &usage_file,
            [
                r#"{"type":"turn_context","payload":{"model":"gpt-5","effort":"high"}}"#,
                r#"{"timestamp":"2026-11-17T10:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":1200}}}}"#,
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let usage = read_codex_usage(&usage_file).unwrap().unwrap();
        assert_eq!(usage.model.as_deref(), Some("gpt-5"));
        assert_eq!(usage.effort.as_deref(), Some("high"));
        assert_eq!(usage.total_tokens, Some(1200));
        assert_eq!(
            Some(usage.updated_at),
            crate::dotnet_text::parse_date_ms("2026-11-17T10:00:00Z", false)
        );

        let rate_file = dir.path().join("rollout-rate0.jsonl");
        std::fs::write(&rate_file, "\u{feff}{\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":42,\"window_minutes\":300}}}}\n")
            .unwrap();
        assert!(
            last_line_with(&rate_file, "\"rate_limits\"")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn windows_credits_plan_and_reached_read_like_the_csharp_agent() {
        let root = tempfile::tempdir().unwrap();
        let rl = r#"{"payload":{"rate_limits":{"plan_type":"plus",
            "primary":{"used_percent":100,"window_minutes":4320,"resets_in_seconds":60},
            "secondary":{"used_percent":81,"window_minutes":90,"resets_at":1.5},
            "credits":{"has_credits":true,"balance":" 1234567.5 "}}}}"#
            .replace('\n', " ");
        let path = log(root.path(), &[&rl]);
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mtime_ms = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let p = read_codex(root.path()).unwrap().unwrap();
        assert_eq!(p.plan, "Plus");
        assert_eq!(
            p.extra.as_deref(),
            Some("Limit reached · Credits: 1,234,568")
        );
        assert_eq!(p.updated_at, Some(mtime_ms));
        assert_eq!(p.limits[0].label, "3-day");
        assert_eq!(p.limits[0].resets_at, Some(mtime_ms + 60_000));
        assert_eq!(p.limits[0].severity, "critical");
        assert_eq!(p.limits[1].label, "1-hour");
        assert_eq!(p.limits[1].window_sec, Some(5400));
        assert_eq!(
            p.limits[1].resets_at, None,
            "1.5 is not a whole number and there is no resets_in_seconds"
        );
        assert_eq!(p.limits[1].severity, "warning");
        assert_eq!(p.codex, None, "no token_count or turn_context records");

        for (credits, extra) in [
            (r#"{"unlimited":true}"#, Some("Credits: unlimited")),
            (r#"{"has_credits":false}"#, Some("Credits not enabled")),
            (r#"{"has_credits":true,"balance":"lots"}"#, None),
        ] {
            log(
                root.path(),
                &[&format!(
                    r#"{{"payload":{{"rate_limits":{{"primary":{{"used_percent":1}},"credits":{credits}}}}}}}"#
                )],
            );
            let p = read_codex(root.path()).unwrap().unwrap();
            assert_eq!(p.extra.as_deref(), extra, "{credits}");
            assert_eq!(
                (p.plan.as_str(), p.limits[0].label.as_str()),
                ("Codex", "primary")
            );
        }
        log(
            root.path(),
            &[
                r#"{"payload":{"rate_limits":{"primary":{"used_percent":1},"spend_control_reached":true}}}"#,
            ],
        );
        assert_eq!(
            read_codex(root.path()).unwrap().unwrap().extra.as_deref(),
            Some("Limit reached")
        );

        log(root.path(), &[r#"{"type":"session_meta"}"#]);
        let none = read_codex(root.path()).unwrap().unwrap();
        assert_eq!(
            none.error.as_deref(),
            Some("No quota data in recent sessions")
        );
        assert_eq!(read_codex(&root.path().join("missing")).unwrap(), None);
    }
}
