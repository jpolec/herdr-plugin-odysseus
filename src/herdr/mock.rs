//! In-memory [`HerdrApi`] for tests and dry demos. Simulates workspaces,
//! tabs, panes and agents with Herdr's semantic states. Agent "work" is a
//! pluggable behavior invoked when a prompt is submitted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;

/// What a simulated agent does after receiving a prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum MockReaction {
    /// Work is done; agent returns to `idle`.
    Finish,
    /// Agent shows a permission prompt (`blocked`) until [`MockHerdr::unblock`].
    Block,
    /// Agent keeps working forever.
    Hang,
    /// The agent's pane disappears.
    Crash,
}

pub struct PromptCall {
    pub agent_name: String,
    pub kind: String,
    pub cwd: PathBuf,
    pub prompt: String,
    /// 0-based count of prompts this agent received before this one.
    pub prompt_index: usize,
}

pub type Behavior = dyn Fn(&PromptCall) -> MockReaction + Send + Sync;

#[derive(Debug, Clone)]
pub struct MockPane {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub cwd: PathBuf,
    pub label: Option<String>,
    pub agent: Option<MockAgent>,
    pub metadata_title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MockAgent {
    pub name: String,
    pub kind: String,
    pub status: AgentStatus,
    pub prompts: Vec<String>,
    pub args: Vec<String>,
    /// Reaction to apply once unblocked.
    pub after_unblock: Option<MockReaction>,
}

#[derive(Default)]
struct State {
    next_ws: u32,
    next_pane: u32,
    panes: BTreeMap<String, MockPane>,
    calls: Vec<String>,
    notifications: Vec<(String, String)>,
    available: bool,
    /// Agent startup shows a trust prompt (blocked) until unblocked.
    block_on_start: bool,
    /// Number of `agent.start` calls answered with `agent_pane_busy` (a
    /// shell that is not ready yet, as real Herdr does right after
    /// `tab.create`).
    busy_starts: u32,
}

pub struct MockHerdr {
    state: Mutex<State>,
    behavior: Arc<Behavior>,
}

impl MockHerdr {
    pub fn new(behavior: Arc<Behavior>) -> Self {
        Self { state: Mutex::new(State { available: true, ..Default::default() }), behavior }
    }

    /// Agents that finish immediately after every prompt.
    pub fn finishing() -> Self {
        Self::new(Arc::new(|_| MockReaction::Finish))
    }

    pub fn set_available(&self, v: bool) {
        self.state.lock().unwrap().available = v;
    }
    pub fn set_busy_starts(&self, n: u32) {
        self.state.lock().unwrap().busy_starts = n;
    }
    pub fn set_block_on_start(&self, v: bool) {
        self.state.lock().unwrap().block_on_start = v;
    }
    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }
    pub fn notifications(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().notifications.clone()
    }
    pub fn panes(&self) -> Vec<MockPane> {
        self.state.lock().unwrap().panes.values().cloned().collect()
    }
    /// Simulate the human answering the agent's prompt in its pane.
    pub fn unblock(&self, agent_name: &str) {
        let mut st = self.state.lock().unwrap();
        for p in st.panes.values_mut() {
            if let Some(a) = p.agent.as_mut() {
                if a.name == agent_name && a.status == AgentStatus::Blocked {
                    a.status = match a.after_unblock.take() {
                        Some(MockReaction::Hang) => AgentStatus::Working,
                        _ => AgentStatus::Idle,
                    };
                }
            }
        }
    }
    /// Force an agent's semantic state (e.g. it finished while we were down).
    pub fn set_agent_status(&self, agent_name: &str, status: AgentStatus) {
        let mut st = self.state.lock().unwrap();
        for p in st.panes.values_mut() {
            if let Some(a) = p.agent.as_mut() {
                if a.name == agent_name {
                    a.status = status;
                }
            }
        }
    }
    /// Prompts received by an agent.
    pub fn prompts(&self, agent_name: &str) -> Vec<String> {
        let st = self.state.lock().unwrap();
        st.panes.values().filter_map(|p| p.agent.as_ref()).filter(|a| a.name == agent_name).flat_map(|a| a.prompts.clone()).collect()
    }
    /// Simulate the user closing a pane (or the agent process dying).
    pub fn kill_pane(&self, pane_id: &str) {
        self.state.lock().unwrap().panes.remove(pane_id);
    }
    pub fn agent_names(&self) -> Vec<String> {
        self.state.lock().unwrap().panes.values().filter_map(|p| p.agent.as_ref().map(|a| a.name.clone())).collect()
    }

    fn check(&self, call: String) -> HResult<std::sync::MutexGuard<'_, State>> {
        let mut st = self.state.lock().unwrap();
        if !st.available {
            return Err(HerdrError::Unavailable("mock unavailable".into()));
        }
        st.calls.push(call);
        Ok(st)
    }

    fn new_pane(st: &mut State, ws: &str, cwd: &Path, label: Option<&str>) -> String {
        st.next_pane += 1;
        let pane_id = format!("{ws}:p{}", st.next_pane);
        st.panes.insert(
            pane_id.clone(),
            MockPane {
                pane_id: pane_id.clone(),
                workspace_id: ws.to_string(),
                tab_id: format!("{ws}:t{}", st.next_pane),
                cwd: cwd.to_path_buf(),
                label: label.map(String::from),
                agent: None,
                metadata_title: None,
            },
        );
        pane_id
    }

    fn find_agent<'a>(st: &'a mut State, target: &str) -> Option<&'a mut MockPane> {
        st.panes.values_mut().find(|p| {
            p.pane_id == target || p.agent.as_ref().is_some_and(|a| a.name == target)
        })
    }

    fn info(p: &MockPane) -> AgentInfo {
        let a = p.agent.as_ref();
        AgentInfo {
            pane_id: p.pane_id.clone(),
            name: a.map(|a| a.name.clone()),
            agent: a.map(|a| a.kind.clone()),
            agent_status: a.map(|a| a.status).unwrap_or(AgentStatus::Unknown),
            interactive_ready: a.is_some_and(|a| a.status != AgentStatus::Blocked),
            launch_pending: false,
            workspace_id: Some(p.workspace_id.clone()),
            tab_id: Some(p.tab_id.clone()),
            terminal_id: Some(format!("term-{}", p.pane_id)),
            cwd: Some(p.cwd.display().to_string()),
            agent_session: a.map(|a| AgentSessionInfo {
                source: Some(format!("herdr:{}", a.kind)),
                agent: Some(a.kind.clone()),
                kind: Some("id".into()),
                value: Some(format!("session-{}", a.name)),
            }),
        }
    }

    fn pane_info(p: &MockPane) -> PaneInfo {
        PaneInfo {
            pane_id: p.pane_id.clone(),
            terminal_id: Some(format!("term-{}", p.pane_id)),
            workspace_id: p.workspace_id.clone(),
            tab_id: p.tab_id.clone(),
            focused: false,
            agent: p.agent.as_ref().map(|a| a.kind.clone()),
            agent_status: p.agent.as_ref().map(|a| a.status).unwrap_or(AgentStatus::Unknown),
            cwd: Some(p.cwd.display().to_string()),
            foreground_cwd: None,
            label: p.label.clone(),
            agent_session: None,
        }
    }

    fn wait_loop(&self, target: &str, until: &[AgentStatus], timeout: Duration) -> HResult<AgentInfo> {
        let t0 = Instant::now();
        loop {
            {
                let mut st = self.state.lock().unwrap();
                let Some(p) = Self::find_agent(&mut st, target) else {
                    return Err(HerdrError::Api { code: "agent_not_found".into(), message: format!("agent {target} not found") });
                };
                let info = Self::info(p);
                if until.contains(&info.agent_status) {
                    return Ok(info);
                }
            }
            if t0.elapsed() >= timeout {
                return Err(HerdrError::Api { code: "timeout".into(), message: "wait timed out".into() });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl HerdrApi for MockHerdr {
    fn socket_path(&self) -> Option<PathBuf> {
        None
    }
    fn ping(&self) -> HResult<Pong> {
        let _st = self.check("ping".into())?;
        Ok(Pong { version: "0.9.0-mock".into(), protocol: 22 })
    }
    fn open_worktree_workspace(&self, _repo_root: &Path, worktree: &Path, label: &str) -> HResult<WorkspaceHandle> {
        let mut st = self.check(format!("worktree.open {label}"))?;
        st.next_ws += 1;
        let ws = format!("w{}", st.next_ws);
        let root = Self::new_pane(&mut st, &ws, worktree, None);
        Ok(WorkspaceHandle { workspace_id: ws, root_pane_id: Some(root), already_open: false })
    }
    fn close_workspace(&self, workspace_id: &str) -> HResult<()> {
        let mut st = self.check(format!("workspace.close {workspace_id}"))?;
        st.panes.retain(|_, p| p.workspace_id != workspace_id);
        Ok(())
    }
    fn create_tab(&self, workspace_id: &str, cwd: &Path, label: &str, _env: &BTreeMap<String, String>) -> HResult<TabHandle> {
        let mut st = self.check(format!("tab.create {label}"))?;
        let pane = Self::new_pane(&mut st, workspace_id, cwd, Some(label));
        let tab = st.panes[&pane].tab_id.clone();
        Ok(TabHandle { tab_id: tab, pane_id: pane })
    }
    fn rename_pane(&self, pane_id: &str, label: &str) -> HResult<()> {
        let mut st = self.check(format!("pane.rename {pane_id} {label}"))?;
        match st.panes.get_mut(pane_id) {
            Some(p) => {
                p.label = Some(label.into());
                Ok(())
            }
            None => Err(HerdrError::Api { code: "pane_not_found".into(), message: "pane not found".into() }),
        }
    }
    fn report_metadata(&self, pane_id: &str, title: &str, _tokens: &BTreeMap<String, Option<String>>) -> HResult<()> {
        let mut st = self.check(format!("pane.report_metadata {pane_id}"))?;
        if let Some(p) = st.panes.get_mut(pane_id) {
            p.metadata_title = Some(title.into());
        }
        Ok(())
    }
    fn get_pane(&self, pane_id: &str) -> HResult<Option<PaneInfo>> {
        let st = self.check(format!("pane.get {pane_id}"))?;
        Ok(st.panes.get(pane_id).map(Self::pane_info))
    }
    fn list_panes(&self) -> HResult<Vec<PaneInfo>> {
        let st = self.check("pane.list".into())?;
        Ok(st.panes.values().map(Self::pane_info).collect())
    }
    fn read_pane(&self, pane_id: &str, _lines: u32) -> HResult<String> {
        let st = self.check(format!("pane.read {pane_id}"))?;
        let p = st.panes.get(pane_id).ok_or_else(|| HerdrError::Api { code: "pane_not_found".into(), message: "pane not found".into() })?;
        Ok(p.agent.as_ref().map(|a| format!("[mock {} transcript]\n{}", a.kind, a.prompts.join("\n---\n"))).unwrap_or_default())
    }
    fn close_pane(&self, pane_id: &str) -> HResult<()> {
        let mut st = self.check(format!("pane.close {pane_id}"))?;
        st.panes.remove(pane_id);
        Ok(())
    }
    fn focus_pane(&self, pane_id: &str) -> HResult<()> {
        self.check(format!("pane.focus {pane_id}")).map(|_| ())
    }
    fn process_info(&self, pane_id: &str) -> HResult<ProcessInfo> {
        let _st = self.check(format!("pane.process_info {pane_id}"))?;
        Ok(ProcessInfo { pane_id: Some(pane_id.into()), shell_pid: None, foreground_processes: vec![] })
    }
    fn notify(&self, title: &str, body: &str, _request_sound: bool) -> HResult<()> {
        let mut st = self.check(format!("notification.show {title}"))?;
        st.notifications.push((title.into(), body.into()));
        Ok(())
    }
    fn open_plugin_pane(&self, plugin_id: &str, entrypoint: &str, _placement: &str, _env: &BTreeMap<String, String>) -> HResult<()> {
        self.check(format!("plugin.pane.open {plugin_id} {entrypoint}")).map(|_| ())
    }
    fn start_agent(&self, name: &str, kind: &str, pane_id: &str, args: &[String], _timeout: Duration) -> HResult<AgentInfo> {
        let mut st = self.check(format!("agent.start {name} {kind} {pane_id}"))?;
        if st.busy_starts > 0 {
            st.busy_starts -= 1;
            return Err(HerdrError::Api { code: "agent_pane_busy".into(), message: format!("agent target pane {pane_id} is not an available shell") });
        }
        let block = st.block_on_start;
        if st.panes.values().any(|p| p.agent.as_ref().is_some_and(|a| a.name == name)) {
            return Err(HerdrError::Api { code: "agent_name_taken".into(), message: format!("agent name {name} is in use") });
        }
        let Some(p) = st.panes.get_mut(pane_id) else {
            return Err(HerdrError::Api { code: "pane_not_found".into(), message: "pane not found".into() });
        };
        if p.agent.is_some() {
            return Err(HerdrError::Api { code: "pane_not_available".into(), message: "pane is busy".into() });
        }
        p.agent = Some(MockAgent {
            name: name.into(),
            kind: kind.into(),
            status: if block { AgentStatus::Blocked } else { AgentStatus::Idle },
            prompts: vec![],
            args: args.to_vec(),
            after_unblock: None,
        });
        let info = Self::info(p);
        if block {
            return Err(HerdrError::Api { code: "agent_not_ready".into(), message: "agent is blocked during startup".into() });
        }
        Ok(info)
    }
    fn get_agent(&self, target: &str) -> HResult<Option<AgentInfo>> {
        let mut st = self.check(format!("agent.get {target}"))?;
        Ok(Self::find_agent(&mut st, target).filter(|p| p.agent.is_some()).map(|p| Self::info(p)))
    }
    fn list_agents(&self) -> HResult<Vec<AgentInfo>> {
        let st = self.check("agent.list".into())?;
        Ok(st.panes.values().filter(|p| p.agent.is_some()).map(Self::info).collect())
    }
    fn prompt_agent(&self, target: &str, text: &str, wait: Option<(&[AgentStatus], Duration)>) -> HResult<AgentInfo> {
        let call = {
            let mut st = self.check(format!("agent.prompt {target}"))?;
            let Some(p) = Self::find_agent(&mut st, target) else {
                return Err(HerdrError::Api { code: "agent_not_found".into(), message: "agent not found".into() });
            };
            let cwd = p.cwd.clone();
            let Some(a) = p.agent.as_mut() else {
                return Err(HerdrError::Api { code: "agent_not_found".into(), message: "no agent".into() });
            };
            if a.status == AgentStatus::Blocked {
                return Err(HerdrError::Api { code: "agent_blocked".into(), message: "agent is blocked".into() });
            }
            a.status = AgentStatus::Working;
            let idx = a.prompts.len();
            a.prompts.push(text.to_string());
            PromptCall { agent_name: a.name.clone(), kind: a.kind.clone(), cwd, prompt: text.into(), prompt_index: idx }
        };
        // Run the behavior outside the lock (it may write files).
        let reaction = (self.behavior)(&call);
        {
            let mut st = self.state.lock().unwrap();
            match reaction {
                MockReaction::Crash => {
                    let id = Self::find_agent(&mut st, target).map(|p| p.pane_id.clone());
                    if let Some(id) = id {
                        st.panes.remove(&id);
                    }
                }
                r => {
                    if let Some(a) = Self::find_agent(&mut st, target).and_then(|p| p.agent.as_mut()) {
                        a.status = match r {
                            MockReaction::Finish => AgentStatus::Idle,
                            MockReaction::Block => {
                                a.after_unblock = Some(MockReaction::Finish);
                                AgentStatus::Blocked
                            }
                            MockReaction::Hang => AgentStatus::Working,
                            MockReaction::Crash => unreachable!(),
                        };
                    }
                }
            }
        }
        match wait {
            Some((until, timeout)) => self.wait_loop(target, until, timeout),
            None => {
                let mut st = self.state.lock().unwrap();
                Self::find_agent(&mut st, target)
                    .map(|p| Self::info(p))
                    .ok_or_else(|| HerdrError::Api { code: "agent_not_found".into(), message: "agent vanished".into() })
            }
        }
    }
    fn wait_agent(&self, target: &str, until: &[AgentStatus], timeout: Duration) -> HResult<AgentInfo> {
        drop(self.check(format!("agent.wait {target}"))?);
        self.wait_loop(target, until, timeout)
    }
    fn send_keys(&self, target: &str, keys: &[&str]) -> HResult<()> {
        let mut st = self.check(format!("agent.send_keys {target} {}", keys.join(" ")))?;
        if let Some(a) = Self::find_agent(&mut st, target).and_then(|p| p.agent.as_mut()) {
            if keys.iter().any(|k| *k == "ctrl+c" || *k == "esc") && a.status == AgentStatus::Working {
                a.status = AgentStatus::Idle;
            }
        }
        Ok(())
    }
    fn focus_agent(&self, target: &str) -> HResult<()> {
        self.check(format!("agent.focus {target}")).map(|_| ())
    }
}
