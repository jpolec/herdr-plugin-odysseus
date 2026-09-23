//! Agent lifecycle over the socket: start, prompt, wait, keys, focus.
//! Completion is detected from Herdr's semantic agent state; nothing here
//! parses terminal output.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use super::{AgentInfo, AgentStatus, HResult, HerdrApi, PaneInfo, Pong, ProcessInfo, SocketHerdr, TabHandle, WorkspaceHandle};

/// Extra slack on the client read timeout beyond a server-side wait.
const SLACK: Duration = Duration::from_secs(15);

impl SocketHerdr {
    fn agent_start_impl(&self, name: &str, kind: &str, pane_id: &str, args: &[String], timeout: Duration) -> HResult<AgentInfo> {
        // Herdr requires 3000 < timeout_ms <= 300000.
        let ms = timeout.as_millis().clamp(3_001, 300_000) as u64;
        self.client.request_field(
            "agent.start",
            json!({"name": name, "kind": kind, "pane_id": pane_id, "args": args, "timeout_ms": ms}),
            "agent",
            Some(Duration::from_millis(ms) + SLACK),
        )
    }

    fn agent_get_impl(&self, target: &str) -> HResult<Option<AgentInfo>> {
        match self.client.request_field::<AgentInfo>("agent.get", json!({"target": target}), "agent", None) {
            Ok(a) => Ok(Some(a)),
            Err(e) if e.is_not_found() || e.code() == Some("agent_not_found") => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn agent_prompt_impl(&self, target: &str, text: &str, wait: Option<(&[AgentStatus], Duration)>) -> HResult<AgentInfo> {
        let (params, timeout) = match wait {
            Some((until, t)) => (
                json!({"target": target, "text": text, "wait": {"until": until, "timeout_ms": t.as_millis() as u64}}),
                Some(t + SLACK),
            ),
            None => (json!({"target": target, "text": text}), None),
        };
        self.client.request_field("agent.prompt", params, "agent", timeout)
    }

    fn agent_wait_impl(&self, target: &str, until: &[AgentStatus], timeout: Duration) -> HResult<AgentInfo> {
        self.client.request_field(
            "agent.wait",
            json!({"target": target, "until": until, "timeout_ms": timeout.as_millis() as u64}),
            "agent",
            Some(timeout + SLACK),
        )
    }
}

impl HerdrApi for SocketHerdr {
    fn socket_path(&self) -> Option<PathBuf> {
        Some(self.client.path.clone())
    }
    fn ping(&self) -> HResult<Pong> {
        let r = self.client.request("ping", json!({}), Some(Duration::from_secs(3)))?;
        serde_json::from_value(r).map_err(|e| super::HerdrError::Protocol(format!("ping: {e}")))
    }
    fn open_worktree_workspace(&self, repo_root: &Path, worktree: &Path, label: &str) -> HResult<WorkspaceHandle> {
        self.worktree_open_workspace(repo_root, worktree, label)
    }
    fn create_tab(&self, workspace_id: &str, cwd: &Path, label: &str, env: &std::collections::BTreeMap<String, String>) -> HResult<TabHandle> {
        self.pane_create_tab(workspace_id, cwd, label, env)
    }
    fn rename_pane(&self, pane_id: &str, label: &str) -> HResult<()> {
        self.pane_rename(pane_id, label)
    }
    fn report_metadata(&self, pane_id: &str, title: &str, tokens: &std::collections::BTreeMap<String, Option<String>>) -> HResult<()> {
        self.pane_report_metadata(pane_id, title, tokens)
    }
    fn get_pane(&self, pane_id: &str) -> HResult<Option<PaneInfo>> {
        self.pane_get(pane_id)
    }
    fn list_panes(&self) -> HResult<Vec<PaneInfo>> {
        self.pane_list()
    }
    fn read_pane(&self, pane_id: &str, lines: u32) -> HResult<String> {
        self.pane_read(pane_id, lines)
    }
    fn close_pane(&self, pane_id: &str) -> HResult<()> {
        self.pane_close(pane_id)
    }
    fn focus_pane(&self, pane_id: &str) -> HResult<()> {
        self.pane_focus(pane_id)
    }
    fn process_info(&self, pane_id: &str) -> HResult<ProcessInfo> {
        self.pane_process_info(pane_id)
    }
    fn notify(&self, title: &str, body: &str, request_sound: bool) -> HResult<()> {
        self.pane_notify(title, body, request_sound)
    }
    fn open_plugin_pane(&self, plugin_id: &str, entrypoint: &str, placement: &str, env: &std::collections::BTreeMap<String, String>) -> HResult<()> {
        self.pane_open_plugin(plugin_id, entrypoint, placement, env)
    }
    fn start_agent(&self, name: &str, kind: &str, pane_id: &str, args: &[String], timeout: Duration) -> HResult<AgentInfo> {
        self.agent_start_impl(name, kind, pane_id, args, timeout)
    }
    fn get_agent(&self, target: &str) -> HResult<Option<AgentInfo>> {
        self.agent_get_impl(target)
    }
    fn list_agents(&self) -> HResult<Vec<AgentInfo>> {
        self.client.request_field("agent.list", json!({}), "agents", None)
    }
    fn prompt_agent(&self, target: &str, text: &str, wait: Option<(&[AgentStatus], Duration)>) -> HResult<AgentInfo> {
        self.agent_prompt_impl(target, text, wait)
    }
    fn wait_agent(&self, target: &str, until: &[AgentStatus], timeout: Duration) -> HResult<AgentInfo> {
        self.agent_wait_impl(target, until, timeout)
    }
    fn send_keys(&self, target: &str, keys: &[&str]) -> HResult<()> {
        self.client.request("agent.send_keys", json!({"target": target, "keys": keys}), None).map(|_| ())
    }
    fn focus_agent(&self, target: &str) -> HResult<()> {
        self.client.request("agent.focus", json!({"target": target}), None).map(|_| ())
    }
}
