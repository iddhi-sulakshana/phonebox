# TeamClaude in the AI quotas widget: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the AI quotas widget can show a TeamClaude card: tokens used in the last 24 hours, a 24-hour usage chart, the
fleet's projected weekly use, and the top 5 users.

**Architecture:** the Rust agent's quota monitor reads two TeamClaude endpoints (`/teamclaude/usage/series`,
`/teamclaude/forecast`) with the `x-api-key` header, summarizes them into a small view, and adds it to `/api/quotas`
as `teamclaude`. The panel's quotas widget renders it as one card when the widget's providers include `teamclaude`.

**Tech Stack:** Rust (agent-rs, `UsageHttp`, serde), the panel's plain ES modules and SVG, the shared settings page.

**Spec:** none written: the owner asked for the feature in conversation (2026-11-22) and said "go ahead" without
further questions. The decisions below are mine, stated to the owner.

## Global Constraints

- The C# agent stays the parity oracle until gate 2: with TeamClaude not configured, `GET /api/settings`,
  `GET /api/config` and `GET /api/quotas` must be byte-identical to before (no new keys, no nulls).
- The TeamClaude key never leaves the computer: not in the phone's config view, not in `/api/quotas`, not in a log.
- Claude Code's key (`~/.claude/settings.json` → `env.ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`) is used only for
  the origin of Claude Code's own `ANTHROPIC_BASE_URL`; a configured server on another origin needs its own key.
- An `ANTHROPIC_BASE_URL` on `anthropic.com` is not a TeamClaude server.
- Network only while needed: requests run only while an enabled page has a quotas widget whose providers include
  `teamclaude`; every 60 s (30 s after a failure).
- Numbers: tokens = input + output (TeamClaude's input already counts the cache); the forecast series and threshold
  are used fractions 0–1; `unusedPct` is a percent.
- Tests and builds run on the test server (`agent-rs/scripts/remote-*.sh`), never on the laptop.

## Review Focus

1. A TeamClaude server that is down or answers 401: the card shows the error and the last good numbers, never a blank
   widget or a crash.
2. One endpoint fails, the other works (e.g. an old TeamClaude without `/forecast`): show what arrived.
3. More than 5 users, or none (shared-key traffic only): top 5 plus "N others"; an empty list says so.
4. A configured URL with a trailing slash or a path, and a blank key with Claude Code on the same server.
5. The card in a small widget (4×3) and with paging on: it stays legible (it takes the widget's page alone).

---

### Task 1: The `teamClaude` setting

**Files:** Modify `agent-rs/crates/phonebox/src/config/mod.rs` (struct, `AgentConfig` field, `KNOWN_FIELDS`, tests),
`agent-rs/crates/phonebox/src/config/normalize.rs`.

**Interfaces:** Produces `pub struct TeamClaudeConfig { pub url: String, pub key: String, extra }` and
`AgentConfig.team_claude: Option<TeamClaudeConfig>` (JSON `teamClaude`, skipped when `None`);
`AgentConfig::shows_teamclaude(&self) -> bool` (an enabled page's quotas widget lists `teamclaude`).

- [ ] Tests: `team_claude_is_absent_by_default` (default config serializes without `teamClaude`);
  `team_claude_round_trips` (url and key survive load and save); `a_blank_team_claude_is_dropped` (both blank after
  trim → `None`); `team_claude_never_reaches_the_phone` (`to_public` has no `teamClaude`, no key text);
  `shows_teamclaude_follows_the_quotas_widgets` (true only for an enabled page's quotas widget listing it).
- [ ] Run them RED on the server, implement, GREEN; `remote-check.sh`; commit.

### Task 2: The TeamClaude source in the quota monitor

**Files:** Create `agent-rs/crates/phonebox/src/quota/teamclaude.rs` and `quota/teamclaude_tests.rs`; modify
`quota/mod.rs` (state, loop), `server/routes_dev.rs` (`/api/quotas`).

**Interfaces:**
- `pub struct Target { pub base: String, pub key: String, pub host: String, pub from_claude_code: bool }`
- `pub fn resolve(cfg: Option<&TeamClaudeConfig>, claude_settings: Option<&serde_json::Value>) -> Option<Target>`
- `pub fn summarize(series: Option<&Value>, forecast: Option<&Value>, now: i64) -> TeamView`
- `TeamView` (camelCase JSON): `name, host, updatedAt, error, tokens {total,input,output,cache}, hourly [u64],
  bucketMs, end, users [{name,tokens,share}] (top 5), others {count,tokens}, forecast {verdict, pace, confidence,
  threshold, poolDryAt, unusedPct, generatedAt, past [[t,u]], projected [[t,u]]}` (each series at most 96 points).
- `QuotaMonitor::refresh_teamclaude(&self, target: &Target) -> bool`; `QuotaMonitor::teamclaude(&self) -> Option<TeamView>`.
- `/api/quotas` gains `teamclaude` only when the monitor has a view.

- [ ] Tests (`teamclaude_tests.rs`): resolve — config URL and key; blank config falls back to Claude Code's base URL
  and key; Claude Code's key not sent to another origin (`None` without a key); anthropic.com ignored; trailing slash
  and path trimmed to the origin. summarize — 24 h totals and hourly sums across users; top 5 by tokens with the
  rest in `others`; no users; forecast downsampled to ≤ 96 points keeping the last point; a missing forecast or series
  leaves that part empty. refresh — against `FakeHttp`: both paths requested with `x-api-key`, never `authorization`;
  a 401 keeps the last view and sets `error`; one endpoint failing keeps the other's data. Route — `/api/quotas` has
  `teamclaude` when the state has a view and no `teamclaude` key otherwise.
- [ ] RED on the server, implement, GREEN; `remote-check.sh`; commit.

### Task 3: The panel card, the catalog and the settings page

**Files:** `agent-rs/assets/widgets.json` (providers choice), `android/app/src/main/assets/www/widgets/basic.js`
(card), `www/style.css`, `agent-rs/assets/settings.html` (TeamClaude fields), `tools/ui-tests/teamclaude.spec.js`.

- [ ] UI test: with `D.quotas.teamclaude` sample data and a quotas widget listing `teamclaude`, the card shows the
  token total, 24 bars, a forecast path with a dashed projection, and five user rows plus "2 others"; with `error`
  set it shows the error line and still the numbers; the card stands alone on its page when paging is on.
- [ ] RED in the Playwright container on the server, implement, GREEN (with `panel.spec.js` and
  `i18n-coverage.spec.js`); commit.

### Task 4: Docs and the whole check

- [ ] `PRIVACY.md` (the new connection), `CHANGELOG.md` (Unreleased), `docs/GUIDE.md` (where to switch it on).
- [ ] `remote-check.sh`, `remote-cross-check.sh`, parity phases 1 and 5 on the server; commit.
