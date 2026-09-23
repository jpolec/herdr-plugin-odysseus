//! Typed subsets of Herdr socket API responses (verified against the
//! Herdr 0.9.0 `herdr api schema --json`, protocol 22). Unknown fields are
//! ignored so newer Herdr versions stay compatible.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }
    /// `idle` and `done` both mean "ready for input".
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Idle | Self::Done)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AgentSessionInfo {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaneInfo {
    pub pane_id: String,
    #[serde(default)]
    pub terminal_id: Option<String>,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default = "unknown")]
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
}

fn unknown() -> AgentStatus {
    AgentStatus::Unknown
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentInfo {
    pub pane_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub interactive_ready: bool,
    #[serde(default)]
    pub launch_pending: bool,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub tab_id: Option<String>,
    #[serde(default)]
    pub terminal_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub active_tab_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcessInfoProcess {
    pub pid: u32,
    pub name: String,
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub cmdline: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProcessInfo {
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub shell_pid: Option<u32>,
    #[serde(default)]
    pub foreground_processes: Vec<ProcessInfoProcess>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pong {
    pub version: String,
    pub protocol: u64,
}

/// A run's Herdr workspace (one per worktree).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceHandle {
    pub workspace_id: String,
    pub root_pane_id: Option<String>,
    pub already_open: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabHandle {
    pub tab_id: String,
    pub pane_id: String,
}

/// Parsed `HERDR_PLUGIN_CONTEXT_JSON` (fields verified in the 0.9.0 schema).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct InvocationContext {
    #[serde(default)]
    pub invocation_source: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub workspace_cwd: Option<String>,
    #[serde(default)]
    pub workspace_label: Option<String>,
    #[serde(default)]
    pub tab_id: Option<String>,
    #[serde(default)]
    pub focused_pane_id: Option<String>,
    #[serde(default)]
    pub focused_pane_cwd: Option<String>,
    #[serde(default)]
    pub focused_pane_agent: Option<String>,
    #[serde(default)]
    pub selected_text: Option<String>,
    #[serde(default)]
    pub clicked_url: Option<String>,
    #[serde(default)]
    pub worktree: Option<WorktreeProvenance>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct WorktreeProvenance {
    #[serde(default)]
    pub repo_root: Option<String>,
    #[serde(default)]
    pub checkout_path: Option<String>,
    #[serde(default)]
    pub repo_name: Option<String>,
    #[serde(default)]
    pub is_linked_worktree: bool,
}

impl InvocationContext {
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Best guess at the repository the user is looking at.
    pub fn repo_hint(&self) -> Option<std::path::PathBuf> {
        self.worktree
            .as_ref()
            .and_then(|w| w.repo_root.clone())
            .or_else(|| self.focused_pane_cwd.clone())
            .or_else(|| self.workspace_cwd.clone())
            .map(std::path::PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_shapes() {
        // Trimmed from a real `herdr pane list` on 0.9.0.
        let p: PaneInfo = serde_json::from_str(r#"{"agent":"claude","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"3ed5"},"agent_status":"idle","cwd":"/x","focused":false,"foreground_cwd":"/x","pane_id":"w1:pF","revision":2,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":67},"tab_id":"w1:t2","terminal_id":"term_6","terminal_title":"t","workspace_id":"w1","new_future_field":1}"#).unwrap();
        assert_eq!(p.agent_status, AgentStatus::Idle);
        assert_eq!(p.agent_session.unwrap().value.as_deref(), Some("3ed5"));
        let a: AgentInfo = serde_json::from_str(r#"{"agent":"codex","agent_status":"working","interactive_ready":true,"name":"qj","pane_id":"w7:p1","state_change_seq":3,"tab_id":"w7:t1","terminal_id":"t","workspace_id":"w7"}"#).unwrap();
        assert_eq!(a.name.as_deref(), Some("qj"));
        let c: InvocationContext = serde_json::from_str(r#"{"workspace_cwd":"/r","focused_pane_cwd":"/r/sub","worktree":{"repo_root":"/main","checkout_path":"/r","repo_key":"k","repo_name":"n","is_linked_worktree":true}}"#).unwrap();
        assert_eq!(c.repo_hint().unwrap(), std::path::PathBuf::from("/main"));
    }
}
