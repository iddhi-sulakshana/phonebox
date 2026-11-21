//! Port of `SessionMonitor.cs`: the AI coding sessions running on this PC (Claude Code, Codex, Cursor), for the
//! phone's Sessions widget (`/api/work` and the stream's `work` event).

use serde::Serialize;

/// One running session. `status` is busy | waiting | shell | idle; `title` is the chat's title (Claude Code only),
/// set only while Settings › Claude Code › "Show session titles on the phone" is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentSession {
    pub id: String,
    pub tool: String,
    pub name: String,
    pub project: String,
    pub status: String,
    pub waiting_for: Option<String>,
    pub since: i64,
    pub started_at: i64,
    pub pid: Option<i32>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub total_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub last_input_tokens: Option<i64>,
    pub context_window: Option<i64>,
    pub turn_started_at: Option<i64>,
    pub outcome: Option<String>,
    pub last_tool: Option<String>,
    pub title: Option<String>,
}

impl AgentSession {
    /// The C# record's positional part; everything else starts out null.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        tool: &str,
        name: &str,
        project: &str,
        status: &str,
        waiting_for: Option<&str>,
        since: i64,
        started_at: i64,
    ) -> Self {
        AgentSession {
            id: id.into(),
            tool: tool.into(),
            name: name.into(),
            project: project.into(),
            status: status.into(),
            waiting_for: waiting_for.map(Into::into),
            since,
            started_at,
            ..Default::default()
        }
    }
}
