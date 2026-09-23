//! Herdr integration boundary.
//!
//! The engine only sees the [`HerdrApi`] trait. [`SocketHerdr`] implements it
//! over the real socket (split into pane/agent/worktree adapters);
//! [`mock::MockHerdr`] implements it in memory for tests. If Herdr's API
//! changes, only this module changes.

pub mod agent_adapter;
pub mod api_types;
pub mod client;
pub mod events;
pub mod mock;
pub mod pane_adapter;
pub mod worktree_adapter;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use api_types::*;
pub use client::{resolve_socket_path, SocketClient};

/// Identifier used as `source` for metadata reports.
pub const METADATA_SOURCE: &str = "plugin:jpolec.herdr-orchestrator";

#[derive(Debug, thiserror::Error)]
pub enum HerdrError {
    #[error("Herdr is not reachable: {0}")]
    Unavailable(String),
    #[error("Herdr API error {code}: {message}")]
    Api { code: String, message: String },
    #[error("Herdr protocol error: {0}")]
    Protocol(String),
    #[error("timed out waiting for Herdr to respond")]
    ClientTimeout,
    #[error("I/O error talking to Herdr: {0}")]
    Io(String),
}

impl HerdrError {
    pub fn io(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }
    pub fn is_not_found(&self) -> bool {
        matches!(self.code(), Some("not_found" | "pane_not_found" | "agent_not_found" | "workspace_not_found"))
            || matches!(self, Self::Api { message, .. } if message.contains("not found"))
    }
    /// Server-side wait deadline (not an error in the agent).
    pub fn is_wait_timeout(&self) -> bool {
        matches!(self.code(), Some("timeout")) || matches!(self, Self::ClientTimeout)
    }
}

pub type HResult<T> = std::result::Result<T, HerdrError>;

/// Everything the orchestrator needs from Herdr. Methods map 1:1 to
/// documented socket methods; see `docs/HERDR_INTEGRATION.md`.
pub trait HerdrApi: Send + Sync {
    fn socket_path(&self) -> Option<PathBuf>;
    fn ping(&self) -> HResult<Pong>;

    // worktree_adapter
    fn open_worktree_workspace(&self, repo_root: &Path, worktree: &Path, label: &str) -> HResult<WorkspaceHandle>;

    // pane_adapter
    fn create_tab(&self, workspace_id: &str, cwd: &Path, label: &str, env: &BTreeMap<String, String>) -> HResult<TabHandle>;
    fn rename_pane(&self, pane_id: &str, label: &str) -> HResult<()>;
    fn report_metadata(&self, pane_id: &str, title: &str, tokens: &BTreeMap<String, Option<String>>) -> HResult<()>;
    fn get_pane(&self, pane_id: &str) -> HResult<Option<PaneInfo>>;
    fn list_panes(&self) -> HResult<Vec<PaneInfo>>;
    fn read_pane(&self, pane_id: &str, lines: u32) -> HResult<String>;
    fn close_pane(&self, pane_id: &str) -> HResult<()>;
    fn focus_pane(&self, pane_id: &str) -> HResult<()>;
    fn process_info(&self, pane_id: &str) -> HResult<ProcessInfo>;
    fn notify(&self, title: &str, body: &str, request_sound: bool) -> HResult<()>;
    fn open_plugin_pane(&self, plugin_id: &str, entrypoint: &str, placement: &str, env: &BTreeMap<String, String>) -> HResult<()>;

    // agent_adapter
    fn start_agent(&self, name: &str, kind: &str, pane_id: &str, args: &[String], timeout: Duration) -> HResult<AgentInfo>;
    fn get_agent(&self, target: &str) -> HResult<Option<AgentInfo>>;
    fn list_agents(&self) -> HResult<Vec<AgentInfo>>;
    /// Submit a prompt; with `wait`, block server-side until one of `until`
    /// or `timeout` (returns `HerdrError` code `timeout` on expiry).
    fn prompt_agent(&self, target: &str, text: &str, wait: Option<(&[AgentStatus], Duration)>) -> HResult<AgentInfo>;
    fn wait_agent(&self, target: &str, until: &[AgentStatus], timeout: Duration) -> HResult<AgentInfo>;
    fn send_keys(&self, target: &str, keys: &[&str]) -> HResult<()>;
    fn focus_agent(&self, target: &str) -> HResult<()>;
}

/// Real implementation over the Herdr socket.
#[derive(Debug, Clone)]
pub struct SocketHerdr {
    pub client: SocketClient,
}

impl SocketHerdr {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { client: SocketClient::new(path) }
    }

    /// Connect using the documented socket resolution; `None` if no socket
    /// path could be determined or it does not exist.
    pub fn discover(configured: Option<&str>) -> Option<Self> {
        let p = resolve_socket_path(configured)?;
        p.exists().then(|| Self::new(p))
    }
}

/// Agent names must match `[a-z][a-z0-9_-]{0,31}` (Herdr rule).
pub fn agent_name(task_id: &str, variant: &str, step_id: &str, exec_id: &str) -> String {
    let mut base: String = format!("o{task_id}{variant}-{step_id}")
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    base.truncate(26);
    let suffix: String = exec_id.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let name = format!("{base}-{}", suffix.to_ascii_lowercase());
    debug_assert!(valid_agent_name(&name));
    name
}

pub fn valid_agent_name(n: &str) -> bool {
    let mut c = n.chars();
    matches!(c.next(), Some(f) if f.is_ascii_lowercase())
        && n.len() <= 32
        && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_' || x == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_are_valid() {
        let n = agent_name("124", "B", "Implement_Step", "x-3f9a1c0b7d2e");
        assert!(valid_agent_name(&n), "{n}");
        assert!(n.starts_with("o124b-implement_step"));
        let long = agent_name("99999", "", &"s".repeat(40), "x-abcdef");
        assert!(valid_agent_name(&long) && long.len() <= 32, "{long}");
    }
}
