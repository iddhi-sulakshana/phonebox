//! Port of `CodexBar.cs`: the `codexbar` CLI's quota (`usage --json`) and cost (`cost --days 30 --format json`) output,
//! and the choice between a local reading and CodexBar's for each provider.

use std::path::Path;
use std::time::Duration;

use chrono::NaiveDate;
use serde::Serialize;

use indexmap::IndexMap;

use crate::dotnet_text::{format_custom, lower_invariant, parse_date_ms, upper_invariant};
use crate::json_text::Node;
use crate::quota::{QuotaLimit, QuotaProvider};
use crate::tool_process::{self, ToolCommand};

/// One provider's cost and tokens over [`QuotaOverview::window_days`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSubscriptionCost {
    pub id: String,
    pub name: String,
    pub tokens: i64,
    pub cost: f64,
    pub currency_code: String,
    pub session_cost: f64,
    pub session_tokens: i64,
}

/// One model's cost and tokens over [`QuotaOverview::window_days`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaTopModelCost {
    pub model_name: String,
    pub provider_id: String,
    pub provider_name: String,
    pub cost: f64,
    pub tokens: i64,
    pub currency_code: String,
}

/// CodexBar's cost overview over one stated rolling window.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaOverview {
    pub total_tokens: i64,
    pub total_cost: f64,
    pub currency_code: String,
    pub window_days: i32,
    pub provider_count: i32,
    pub updated_at: i64,
    pub subscriptions: Vec<QuotaSubscriptionCost>,
    pub top_models: Vec<QuotaTopModelCost>,
}

/// Why a CodexBar read failed: its output (`JsonException`), its exit code (`IOException`), or running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexBarError {
    Json(String),
    Exit(String),
    Run(String),
}

impl std::fmt::Display for CodexBarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexBarError::Json(m) | CodexBarError::Exit(m) | CodexBarError::Run(m) => {
                f.write_str(m)
            }
        }
    }
}

/// A cost overview older than this is not shown.
pub const OVERVIEW_STALE_MS: i64 = 3_600_000;
const RUN_TIMEOUT: Duration = Duration::from_secs(25);

/// The display name of a CodexBar provider id (known ids case-insensitively, else the id with a capital first letter).
pub fn display_name(raw_id: &str) -> String {
    const NAMES: [(&str, &str); 31] = [
        ("claude", "Claude"),
        ("codex", "Codex"),
        ("antigravity", "Antigravity"),
        ("cursor", "Cursor"),
        ("copilot", "Copilot"),
        ("gemini", "Gemini"),
        ("windsurf", "Windsurf"),
        ("zai", "z.ai"),
        ("openrouter", "OpenRouter"),
        ("alibaba", "Alibaba Coding Plan"),
        ("alibabatokenplan", "Alibaba Token Plan"),
        ("deepseek", "DeepSeek"),
        ("perplexity", "Perplexity"),
        ("warp", "Warp"),
        ("kimi", "Kimi"),
        ("longcat", "LongCat"),
        ("minimax", "MiniMax"),
        ("moonshot", "Moonshot"),
        ("amp", "Amp"),
        ("augment", "Augment"),
        ("factory", "Factory"),
        ("jetbrains", "JetBrains AI"),
        ("kilo", "Kilo"),
        ("kiro", "Kiro"),
        ("ollama", "Ollama"),
        ("opencode", "OpenCode"),
        ("opencodego", "OpenCode Go"),
        ("opencodex", "OpenCodex"),
        ("synthetic", "Synthetic"),
        ("vertexai", "Vertex AI"),
        ("abacus", "Abacus"),
    ];
    if let Some((_, name)) = NAMES.iter().find(|(id, _)| id.eq_ignore_ascii_case(raw_id)) {
        return (*name).into();
    }
    // FormatName: the first UTF-16 unit upper-cased
    let mut chars = raw_id.chars();
    match chars.next() {
        Some(first) => upper_invariant(&first.to_string()) + chars.as_str(),
        None => String::new(),
    }
}

/// The cached overview, only while enabled and within the hour.
pub fn select_overview(
    overview: Option<QuotaOverview>,
    enabled: bool,
    now: i64,
) -> Option<QuotaOverview> {
    overview.filter(|o| enabled && now - o.updated_at <= OVERVIEW_STALE_MS)
}

/// `codexbar cost --days 30 --format json`.
pub async fn read_cost(path: &Path) -> Result<Option<QuotaOverview>, CodexBarError> {
    let cmd = ToolCommand::new(path).args(["cost", "--days", "30", "--format", "json"]);
    let out = tool_process::run(&cmd, RUN_TIMEOUT, std::future::pending())
        .await
        .map_err(|e| CodexBarError::Run(e.to_string()))?;
    parse_cost(&out.stdout, out.code, None)
}

/// The cost output as an overview over one window and one main currency; None when it has no costs.
pub fn parse_cost(
    json: &str,
    exit_code: i32,
    reference_date: Option<NaiveDate>,
) -> Result<Option<QuotaOverview>, CodexBarError> {
    let root = parse_json(json)?;
    let Node::Array(all) = &root else {
        return Err(CodexBarError::Json("Expected a CodexBar cost array".into()));
    };

    let mut items = Vec::new();
    let mut min_window: Option<i32> = None;
    for item in all {
        let Some(raw_id) = item
            .text("provider")
            .map(str::trim)
            .filter(|r| !r.is_empty() && !r.eq_ignore_ascii_case("cli"))
        else {
            continue;
        };
        items.push((item, raw_id, lower_invariant(raw_id)));
        let totals = prop(item, "totals");
        let has_data = item
            .num("last30DaysCostUSD")
            .or_else(|| totals.and_then(|t| t.num("totalCost")))
            .unwrap_or(0.0)
            > 0.0
            || item
                .num("last30DaysTokens")
                .or_else(|| totals.and_then(|t| t.num("totalTokens")))
                .unwrap_or(0.0)
                > 0.0
            || matches!(item.get("daily"), Some(Node::Array(d)) if !d.is_empty());
        if !has_data {
            continue;
        }
        let window = item
            .num("historyDays")
            .filter(|hd| *hd >= 1.0)
            .map_or(30, |hd| (hd as i32).min(30));
        min_window = Some(min_window.map_or(window, |w| w.min(window)));
    }

    let window_days = min_window.unwrap_or(30);
    let mut subs: Vec<QuotaSubscriptionCost> = Vec::new();
    let mut models: IndexMap<(String, String, String), QuotaTopModelCost> = IndexMap::new();
    for (item, raw_id, id) in items {
        let name = display_name(raw_id);
        let currency = item
            .text("currencyCode")
            .map(|c| upper_invariant(c.trim()))
            .filter(|c| !c.is_empty());
        let currency = currency.unwrap_or_else(|| "USD".into());
        let totals = prop(item, "totals");

        let mut daily = Vec::new();
        if let Some(Node::Array(days)) = item.get("daily") {
            for d in days {
                let Some(date) = d.text("date").and_then(date_only) else {
                    continue;
                };
                daily.push((
                    date,
                    d.num("totalCost").unwrap_or(0.0),
                    d.num("totalTokens").unwrap_or(0.0) as i64,
                    d.get("modelBreakdowns"),
                ));
            }
        }
        let updated = item
            .text("updatedAt")
            .and_then(|u| parse_date_ms(u, true))
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|t| t.date_naive());
        let anchor = reference_date
            .or(updated)
            .or_else(|| daily.iter().map(|d| d.0).max())
            .unwrap_or_else(|| chrono::Utc::now().date_naive());
        let start = anchor - chrono::Days::new(u64::try_from(window_days - 1).unwrap_or(0));
        let filtered: Vec<_> = daily
            .iter()
            .filter(|d| d.0 >= start && d.0 <= anchor)
            .collect();
        let has_filtered = !filtered.is_empty();

        let (mut daily_cost, mut daily_tokens) = (0.0, 0i64);
        let mut item_models = Vec::new();
        for d in &filtered {
            daily_cost += d.1;
            daily_tokens = daily_tokens.wrapping_add(d.2);
            let Some(Node::Array(breakdowns)) = d.3 else {
                continue;
            };
            for m in breakdowns {
                let Some(model) = m.text("modelName").map(str::trim).filter(|n| !n.is_empty())
                else {
                    continue;
                };
                item_models.push((
                    model.to_string(),
                    m.num("cost").unwrap_or(0.0).max(0.0),
                    (m.num("totalTokens").unwrap_or(0.0) as i64).max(0),
                ));
            }
        }

        let hist_days = item
            .num("historyDays")
            .filter(|h| *h >= 1.0)
            .map(|h| h as i32);
        let as_long = |v: Option<f64>| v.map(|t| t as i64);
        let (raw_cost, raw_tokens) =
            if hist_days == Some(window_days) || (hist_days.is_none() && window_days == 30) {
                (
                    item.num("last30DaysCostUSD")
                        .or_else(|| totals.and_then(|t| t.num("totalCost")))
                        .or(has_filtered.then_some(daily_cost)),
                    as_long(
                        item.num("last30DaysTokens")
                            .or_else(|| totals.and_then(|t| t.num("totalTokens"))),
                    )
                    .or(has_filtered.then_some(daily_tokens)),
                )
            } else if hist_days.is_some_and(|h| h > 30) && window_days == 30 {
                (
                    item.num("last30DaysCostUSD")
                        .or(has_filtered.then_some(daily_cost)),
                    as_long(item.num("last30DaysTokens")).or(has_filtered.then_some(daily_tokens)),
                )
            } else {
                if !has_filtered {
                    continue;
                }
                (Some(daily_cost), Some(daily_tokens))
            };
        if raw_cost.is_none() && raw_tokens.is_none() {
            continue;
        }
        let cost = raw_cost.unwrap_or(0.0).max(0.0);
        let tokens = raw_tokens.unwrap_or(0).max(0);
        if cost <= 0.0 && tokens <= 0 && !has_filtered {
            continue;
        }
        subs.push(QuotaSubscriptionCost {
            id: id.clone(),
            name: name.clone(),
            tokens,
            cost,
            currency_code: currency.clone(),
            session_cost: item.num("sessionCostUSD").unwrap_or(0.0).max(0.0),
            session_tokens: (item.num("sessionTokens").unwrap_or(0.0) as i64).max(0),
        });
        for (model, model_cost, model_tokens) in item_models {
            let entry = models
                .entry((id.clone(), lower_invariant(&model), currency.clone()))
                .or_insert_with(|| QuotaTopModelCost {
                    model_name: model.clone(),
                    provider_id: id.clone(),
                    provider_name: name.clone(),
                    cost: 0.0,
                    tokens: 0,
                    currency_code: currency.clone(),
                });
            entry.cost += model_cost;
            entry.tokens = entry.tokens.wrapping_add(model_tokens);
        }
    }

    if subs.is_empty() {
        if exit_code != 0 {
            return Err(CodexBarError::Exit(format!(
                "CodexBar cost exited with code {exit_code}"
            )));
        }
        return Ok(None);
    }

    // the main currency: the most providers, then USD, then the higher cost (ties keep the first seen)
    let mut groups: Vec<(String, usize, f64)> = Vec::new();
    for s in &subs {
        match groups
            .iter_mut()
            .find(|g| g.0.eq_ignore_ascii_case(&s.currency_code))
        {
            Some(g) => {
                g.1 += 1;
                g.2 += s.cost;
            }
            None => groups.push((s.currency_code.clone(), 1, s.cost)),
        }
    }
    groups.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| {
                b.0.eq_ignore_ascii_case("USD")
                    .cmp(&a.0.eq_ignore_ascii_case("USD"))
            })
            .then_with(|| b.2.total_cmp(&a.2))
    });
    let currency = groups.swap_remove(0).0;

    let total_cost = subs
        .iter()
        .filter(|s| s.currency_code == currency)
        .map(|s| s.cost)
        .sum();
    let total_tokens = subs
        .iter()
        .filter(|s| s.currency_code == currency)
        .fold(0i64, |t, s| t.wrapping_add(s.tokens));
    subs.sort_by(|a, b| {
        (b.currency_code == currency)
            .cmp(&(a.currency_code == currency))
            .then_with(|| b.cost.total_cmp(&a.cost))
            .then_with(|| b.tokens.cmp(&a.tokens))
    });
    let mut top: Vec<QuotaTopModelCost> = models
        .into_values()
        .filter(|m| m.currency_code == currency)
        .collect();
    top.sort_by(|a, b| {
        b.cost
            .total_cmp(&a.cost)
            .then_with(|| b.tokens.cmp(&a.tokens))
    });
    top.truncate(6);

    Ok(Some(QuotaOverview {
        total_tokens,
        total_cost,
        currency_code: currency,
        window_days,
        provider_count: i32::try_from(subs.len()).unwrap_or(i32::MAX),
        updated_at: crate::server::now_ms(),
        subscriptions: subs,
        top_models: top,
    }))
}

/// `DateOnly.TryParse` with the invariant culture: ISO `yyyy-MM-dd` or `MM/dd/yyyy`.
fn date_only(text: &str) -> Option<NaiveDate> {
    let text = text.trim();
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(text, "%m/%d/%Y"))
        .ok()
}

fn parse_json(json: &str) -> Result<Node, CodexBarError> {
    crate::json_text::parse(json, false).map_err(CodexBarError::Json)
}

/// `Prop`: an object member, any kind.
fn prop<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
    node.get(key)
}

/// `codexbar usage --json`.
pub async fn read(path: &Path) -> Result<Vec<QuotaProvider>, CodexBarError> {
    let cmd = ToolCommand::new(path).args(["usage", "--json"]);
    let out = tool_process::run(&cmd, RUN_TIMEOUT, std::future::pending())
        .await
        .map_err(|e| CodexBarError::Run(e.to_string()))?;
    parse(&out.stdout, out.code)
}

/// The usage output as providers (`source` "codexbar"), the first of each id.
pub fn parse(json: &str, exit_code: i32) -> Result<Vec<QuotaProvider>, CodexBarError> {
    let root = parse_json(json)?;
    let Node::Array(all) = &root else {
        return Err(CodexBarError::Json(
            "Expected a CodexBar provider array".into(),
        ));
    };
    let mut providers: Vec<QuotaProvider> = Vec::new();
    for item in all {
        let Some(raw_id) = item
            .text("provider")
            .map(str::trim)
            .filter(|r| !r.is_empty() && !r.eq_ignore_ascii_case("cli"))
        else {
            continue;
        };
        let id = lower_invariant(raw_id);
        let name = display_name(raw_id);
        let usage = prop(item, "usage");
        let pace = prop(item, "pace");
        let error = prop(item, "error");
        let message = match error {
            Some(Node::String(m)) => Some(m.clone()),
            Some(e @ Node::Object(_)) => Some(
                e.text("message")
                    .unwrap_or("CodexBar provider unavailable")
                    .to_string(),
            ),
            _ => None,
        };
        let labels = prop(item, "rateWindowLabels");
        let mut limits = Vec::new();
        let mut add = |window: Option<&Node>, label: &str, pace: Option<&Node>| {
            let Some(window) = window else { return };
            if window.get("isSyntheticPlaceholder") == Some(&Node::Bool(true)) {
                return;
            }
            let Some(percent) = window.num("usedPercent") else {
                return;
            };
            let minutes = window.num("windowMinutes");
            let title = match labels.and_then(|l| l.text(label)) {
                Some(title) => title.to_string(),
                None if matches!(label, "primary" | "secondary" | "tertiary") => match minutes {
                    Some(300.0) => "5-hour session".into(),
                    Some(10080.0) => "Weekly".into(),
                    Some(m) if m > 0.0 => format!("{}-hour", format_custom(m / 60.0, 1, false)),
                    _ => match label {
                        "primary" => "Primary limit".into(),
                        "secondary" => "Secondary limit".into(),
                        _ => "Additional limit".into(),
                    },
                },
                None => label.to_string(),
            };
            let percent = percent.max(0.0);
            let mut limit = QuotaLimit::new(
                &title,
                percent,
                window.text("resetsAt").and_then(|r| parse_date_ms(r, true)),
                minutes
                    .filter(|m| *m > 0.0 && *m <= 525_600.0)
                    .map(|m| (m * 60.0) as i64),
                if percent >= 95.0 {
                    "critical"
                } else if percent >= 80.0 {
                    "warning"
                } else {
                    "normal"
                },
            );
            limit.reset_description = window
                .text("resetDescription")
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(String::from);
            limit.delta_percent = pace.and_then(|p| p.num("deltaPercent"));
            limit.expected_percent = pace.and_then(|p| p.num("expectedUsedPercent"));
            limit.pace_on_track = match pace.and_then(|p| p.get("willLastToReset")) {
                Some(Node::Bool(b)) => Some(*b),
                _ => None,
            };
            limits.push(limit);
        };
        let window = |key: &str| usage.and_then(|u| prop(u, key));
        let window_pace = |key: &str| pace.and_then(|p| prop(p, key));
        add(window("primary"), "primary", window_pace("primary"));
        add(window("secondary"), "secondary", window_pace("secondary"));
        add(window("tertiary"), "tertiary", window_pace("tertiary"));
        if let Some(Node::Array(extra)) = usage.and_then(|u| u.get("extraRateWindows")) {
            for w in extra {
                add(
                    prop(w, "window"),
                    w.text("title").unwrap_or("Additional limit"),
                    None,
                );
            }
        }
        let plan = usage
            .and_then(|u| u.text("loginMethod"))
            .or_else(|| {
                usage
                    .and_then(|u| prop(u, "planObservation"))
                    .and_then(|p| p.text("rawPlan"))
            })
            .map_or_else(|| name.clone(), String::from);
        let credits = prop(item, "credits").and_then(|c| c.num("remaining"));
        let updated_at = usage
            .and_then(|u| u.text("updatedAt"))
            .and_then(|t| parse_date_ms(t, true))
            .or_else(|| item.text("updatedAt").and_then(|t| parse_date_ms(t, true)))
            .unwrap_or_else(crate::server::now_ms);
        let error = message.or_else(|| {
            limits
                .is_empty()
                .then(|| "CodexBar returned no measured limits".to_string())
        });
        let mut p = QuotaProvider::new(
            &id,
            &name,
            &plan,
            limits,
            credits.map(|c| format!("Credits: {}", format_custom(c, 2, false))),
            Some(updated_at),
            error,
        );
        p.source = "codexbar".into();
        if !providers.iter().any(|q| q.id == p.id) {
            providers.push(p);
        }
    }
    if exit_code != 0 && providers.is_empty() {
        return Err(CodexBarError::Exit(format!(
            "CodexBar exited with code {exit_code}"
        )));
    }
    Ok(providers)
}

/// Between a local reading and CodexBar's: CodexBar's while it is healthy and at most 10 minutes old, else a healthy
/// local one, else CodexBar's with its error (or "stale"); the local Codex token usage rides along.
pub fn select(
    local: Option<&QuotaProvider>,
    external: Option<&QuotaProvider>,
    now: i64,
) -> Option<QuotaProvider> {
    let codex = || local.and_then(|l| l.codex.clone());
    if let Some(e) = external
        && e.error.is_none()
        && !e.limits.is_empty()
        && e.updated_at.is_some_and(|u| now - u <= 600_000)
    {
        return Some(QuotaProvider {
            codex: codex(),
            ..e.clone()
        });
    }
    if let Some(l) = local
        && l.error.is_none()
        && !l.limits.is_empty()
    {
        return Some(l.clone());
    }
    if let Some(e) = external {
        let error = Some(
            e.error
                .clone()
                .unwrap_or_else(|| "CodexBar data is stale".into()),
        );
        return Some(QuotaProvider {
            error,
            codex: codex(),
            ..e.clone()
        });
    }
    local.cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(json: &str) -> QuotaProvider {
        let mut all = parse(json, 0).unwrap();
        assert_eq!(all.len(), 1, "{all:?}");
        all.remove(0)
    }

    #[test]
    fn parses_primary_secondary_and_supplemental_windows_without_plan_metadata() {
        let p = one(
            r#"[{"provider":"claude","usage":{"primary":{"usedPercent":0,"windowMinutes":300},
            "secondary":{"usedPercent":80,"windowMinutes":10080},
            "extraRateWindows":[{"title":"Model pool","window":{"usedPercent":25}}]}}]"#,
        );
        assert_eq!(p.limits.len(), 3);
        assert_eq!(p.limits[0].percent, 0.0);
        assert_eq!(p.limits[0].label, "5-hour session");
        assert_eq!(p.limits[1].label, "Weekly");
        assert_eq!(p.limits[1].severity, "warning");
        assert_eq!(p.limits[2].label, "Model pool");
        assert_eq!(p.error, None);
        assert_eq!((p.plan.as_str(), p.source.as_str()), ("Claude", "codexbar"));
    }

    #[test]
    fn synthetic_and_malformed_windows_are_skipped_without_dropping_later_providers() {
        let p = parse(
            r#"[{"provider":"claude","usage":{"primary":{"usedPercent":0,"isSyntheticPlaceholder":true},"secondary":{"usedPercent":"unknown"}}},
            {"provider":"antigravity","usage":{"secondary":{"usedPercent":0}}}]"#,
            0,
        )
        .unwrap();
        assert!(p[0].limits.is_empty());
        assert_eq!(
            p[0].error.as_deref(),
            Some("CodexBar returned no measured limits")
        );
        assert_eq!(p[1].limits.len(), 1);
        assert_eq!(p[1].limits[0].label, "Secondary limit");
    }

    #[test]
    fn uses_local_fallback_on_provider_error_and_keeps_it_fresh() {
        let external = one(r#"[{"provider":"codex","error":{"message":"Signed out"}}]"#);
        let first = QuotaProvider::new(
            "codex",
            "Codex",
            "Plan",
            vec![QuotaLimit::new("Weekly", 10.0, None, None, "normal")],
            None,
            Some(1),
            None,
        );
        let next = QuotaProvider {
            limits: vec![QuotaLimit::new("Weekly", 40.0, None, None, "normal")],
            updated_at: Some(2),
            ..first.clone()
        };
        assert_eq!(
            select(Some(&first), Some(&external), 2).unwrap().limits[0].percent,
            10.0
        );
        assert_eq!(
            select(Some(&next), Some(&external), 3).unwrap().limits[0].percent,
            40.0
        );
        assert_eq!(
            select(None, Some(&external), 3).unwrap().error.as_deref(),
            Some("Signed out")
        );
        let healthy = QuotaProvider {
            source: "codexbar".into(),
            updated_at: Some(3),
            ..next.clone()
        };
        assert_eq!(
            select(Some(&first), Some(&healthy), 4).unwrap().source,
            "codexbar"
        );
        assert_eq!(
            select(Some(&first), Some(&healthy), 600_004)
                .unwrap()
                .source,
            "local"
        );
        // without a healthy local reading a stale one is still shown, marked stale
        let stale = select(None, Some(&healthy), 600_004).unwrap();
        assert_eq!(stale.error.as_deref(), Some("CodexBar data is stale"));
        assert_eq!(select(None, None, 1), None);
    }

    #[test]
    fn invalid_root_is_an_error() {
        assert!(matches!(parse("{}", 0), Err(CodexBarError::Json(_))));
        assert!(matches!(parse("not json", 0), Err(CodexBarError::Json(_))));
    }

    #[test]
    fn a_failed_provider_does_not_discard_other_providers_from_a_nonzero_exit() {
        // CodexBar 0.84.0 merges provider outputs but returns failure if any provider fails.
        let providers = parse(
            r#"[{"provider":"claude","error":{"message":"Not signed in"}},
             {"provider":"codex","usage":{"primary":{"usedPercent":32,"windowMinutes":300}}}]"#,
            1,
        )
        .unwrap();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].error.as_deref(), Some("Not signed in"));
        assert_eq!(providers[1].error, None);
        assert_eq!(providers[1].limits.len(), 1);
        assert_eq!(providers[1].limits[0].percent, 32.0);
    }

    #[test]
    fn fatal_cli_errors_remain_failures() {
        let result = parse(
            r#"[{"provider":"cli","error":{"message":"Invalid arguments"}}]"#,
            1,
        );
        assert_eq!(
            result,
            Err(CodexBarError::Exit("CodexBar exited with code 1".into()))
        );
    }

    #[test]
    fn tertiary_windows_are_retained_alongside_primary_and_secondary() {
        let provider = one(
            r#"[{"provider":"claude","rateWindowLabels":{"tertiary":"Opus"},"usage":{
            "primary":{"usedPercent":10},"secondary":{"usedPercent":20},"tertiary":{"usedPercent":30}}}]"#,
        );
        assert_eq!(provider.limits.len(), 3);
        assert_eq!(provider.limits[2].label, "Opus");
        assert_eq!(provider.limits[2].percent, 30.0);
        assert_eq!(provider.limits[0].label, "Primary limit");
    }

    #[test]
    fn parses_reset_description_pace_and_additional_providers() {
        let providers = parse(
            r#"[{"provider":"cursor","pace":{"primary":{"stage":"ahead","deltaPercent":12.5,"expectedUsedPercent":30,"willLastToReset":true}},
              "usage":{"loginMethod":"Pro","primary":{
                "usedPercent":42.5,
                "windowMinutes":10080,
                "resetDescription":"Resets Sunday 12:00"
              },
              "extraRateWindows":[{"title":"Fuel pack: 750/1000","window":{"usedPercent":25,"resetDescription":"   "}}]}},
            {"provider":"alibabatokenplan","usage":{"primary":{"usedPercent":10}}},
            {"provider":"alibaba","usage":{"primary":{"usedPercent":20}}},
            {"provider":"custom_ai","displayName":"Ignored Name","usage":{"primary":{"usedPercent":75}}}]"#,
            0,
        )
        .unwrap();
        assert_eq!(providers.len(), 4);

        let cursor = &providers[0];
        assert_eq!(
            (
                cursor.id.as_str(),
                cursor.name.as_str(),
                cursor.plan.as_str()
            ),
            ("cursor", "Cursor", "Pro")
        );
        assert_eq!(cursor.limits.len(), 2);
        let limit = &cursor.limits[0];
        assert_eq!(limit.label, "Weekly");
        assert_eq!(limit.window_sec, Some(604_800));
        assert_eq!(
            limit.reset_description.as_deref(),
            Some("Resets Sunday 12:00")
        );
        assert_eq!(limit.delta_percent, Some(12.5));
        assert_eq!(limit.expected_percent, Some(30.0));
        assert_eq!(limit.pace_on_track, Some(true));

        let fuel_pack = &cursor.limits[1];
        assert_eq!(fuel_pack.label, "Fuel pack: 750/1000");
        assert_eq!(fuel_pack.reset_description, None);
        assert_eq!(fuel_pack.delta_percent, None);
        assert_eq!(fuel_pack.pace_on_track, None);

        assert_eq!(providers[1].name, "Alibaba Token Plan");
        assert_eq!(providers[2].name, "Alibaba Coding Plan");
        assert_eq!(providers[3].name, "Custom_ai");
    }

    #[test]
    fn parses_cost_overview_with_consistent_window_and_currency() {
        let april20 = NaiveDate::from_ymd_opt(2026, 4, 20);
        let sliced = parse_cost(
            r#"[
              {
                "provider": "claude", "historyDays": 90, "currencyCode": "EUR", "updatedAt": "2026-04-20T12:00:00Z",
                "totals": { "totalCost": 135.0, "totalTokens": 5900000 },
                "daily": [
                  { "date": "2026-02-01", "totalCost": 100.0, "totalTokens": 5000000,
                    "modelBreakdowns": [ { "modelName": "claude-old", "cost": 100.0, "totalTokens": 5000000 } ] },
                  { "date": "2026-04-18", "totalCost": 20.0, "totalTokens": 600000,
                    "modelBreakdowns": [
                      { "modelName": "claude-sonnet-4-5", "cost": 15.0, "totalTokens": 500000 },
                      { "modelName": "claude-opus-4-6", "cost": 5.0, "totalTokens": 100000 } ] },
                  { "date": "2026-04-19", "totalCost": 15.0, "totalTokens": 300000,
                    "modelBreakdowns": [ { "modelName": "claude-sonnet-4-5", "cost": 15.0, "totalTokens": 300000 } ] }
                ]
              },
              { "provider": "codex", "historyDays": 90, "totals": { "totalCost": 50.0, "totalTokens": 500000 } },
              { "provider": "alibabatokenplan", "historyDays": 30, "currencyCode": "eur",
                "last30DaysCostUSD": 10.0, "last30DaysTokens": 200000 },
              {
                "provider": "cursor", "historyDays": 30, "currencyCode": "USD", "last30DaysCostUSD": 99.0,
                "last30DaysTokens": 400000, "updatedAt": "2026-04-20T12:00:00Z",
                "daily": [ { "date": "2026-04-19", "totalCost": 99.0, "totalTokens": 400000,
                    "modelBreakdowns": [ { "modelName": "gpt-5", "cost": 99.0, "totalTokens": 400000 } ] } ]
              }
            ]"#,
            0,
            april20,
        )
        .unwrap()
        .unwrap();
        assert_eq!(sliced.window_days, 30);
        assert_eq!(sliced.currency_code, "EUR");
        assert_eq!(sliced.provider_count, 3);
        assert!(!sliced.subscriptions.iter().any(|s| s.id == "codex"));
        assert!((sliced.total_cost - 45.0).abs() < 1e-3);
        assert_eq!(sliced.total_tokens, 1_100_000);
        let alibaba = sliced
            .subscriptions
            .iter()
            .find(|s| s.id == "alibabatokenplan")
            .unwrap();
        assert_eq!(alibaba.name, "Alibaba Token Plan");
        assert!(
            !sliced
                .top_models
                .iter()
                .any(|m| m.model_name == "claude-old" || m.model_name == "gpt-5")
        );
        assert_eq!(sliced.top_models.len(), 2);
        assert_eq!(sliced.top_models[0].model_name, "claude-sonnet-4-5");
        assert!((sliced.top_models[0].cost - 30.0).abs() < 1e-3);
        assert_eq!(sliced.top_models[0].currency_code, "EUR");
        assert_eq!(
            sliced.subscriptions.last().unwrap().id,
            "cursor",
            "the other currency goes last"
        );

        let seven_day = parse_cost(
            r#"[
              { "provider": "codex", "historyDays": 7, "last30DaysCostUSD": 12.5, "last30DaysTokens": 250000 },
              { "provider": "claude", "historyDays": 30, "updatedAt": "2026-04-20T12:00:00Z", "last30DaysCostUSD": 80.0,
                "daily": [ { "date": "2026-04-01", "totalCost": 70.0, "totalTokens": 700000 },
                           { "date": "2026-04-18", "totalCost": 10.0, "totalTokens": 100000 } ] }
            ]"#,
            0,
            april20,
        )
        .unwrap()
        .unwrap();
        assert_eq!(seven_day.window_days, 7);
        assert_eq!(seven_day.currency_code, "USD");
        assert!((seven_day.total_cost - 22.5).abs() < 1e-3);
        assert_eq!(seven_day.total_tokens, 350_000);

        assert_eq!(parse_cost("[]", 0, april20), Ok(None));
        assert!(matches!(
            parse_cost("[]", 2, april20),
            Err(CodexBarError::Exit(_))
        ));
        assert!(matches!(
            parse_cost("{}", 0, april20),
            Err(CodexBarError::Json(_))
        ));
    }

    #[test]
    fn codex_bar_texts_do_not_depend_on_the_windows_locale() {
        let p = one(
            r#"[{"provider":"codex","usage":{"primary":{"usedPercent":12,"windowMinutes":90},"updatedAt":"2026-11-11T10:00:00Z"},"credits":{"remaining":12.5}}]"#,
        );
        assert_eq!(p.extra.as_deref(), Some("Credits: 12.5"));
        assert_eq!(p.limits.len(), 1);
        assert_eq!(p.limits[0].label, "1.5-hour");
        assert_eq!(
            p.updated_at,
            crate::dotnet_text::parse_date_ms("2026-11-11T10:00:00Z", true)
        );
    }

    #[test]
    fn an_overview_is_shown_only_while_enabled_and_fresh() {
        let overview = QuotaOverview {
            total_tokens: 1,
            total_cost: 1.0,
            currency_code: "USD".into(),
            window_days: 30,
            provider_count: 1,
            updated_at: 1000,
            subscriptions: vec![],
            top_models: vec![],
        };
        assert_eq!(
            select_overview(Some(overview.clone()), true, 1000 + OVERVIEW_STALE_MS),
            Some(overview.clone())
        );
        assert_eq!(
            select_overview(Some(overview.clone()), true, 1001 + OVERVIEW_STALE_MS),
            None
        );
        assert_eq!(select_overview(Some(overview), false, 1000), None);
        assert_eq!(display_name("ZAI"), "z.ai");
        assert_eq!(display_name("x"), "X");
    }
}
