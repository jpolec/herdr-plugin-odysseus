//! Interactive agents in real Herdr panes.
//!
//! Flow per step execution:
//! `tab.create` (cwd = worktree) → `pane.rename` (`#12 implement · codex`)
//! → `agent.start` → `agent.prompt` with server-side wait → chunked
//! `agent.wait` until the agent settles. `blocked` means the agent is asking
//! its human something in its own UI: we surface it and keep waiting; we
//! never answer on the human's behalf. Retries reuse the same live agent so
//! it keeps its context.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use super::*;
use crate::herdr::{agent_name, AgentStatus, HerdrApi, HerdrError};

const CHUNK: Duration = Duration::from_secs(10);
const SETTLED: &[AgentStatus] = &[AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
const NOT_BLOCKED: &[AgentStatus] = &[AgentStatus::Idle, AgentStatus::Done, AgentStatus::Working];

pub struct PaneRunner {
    name: String,
    herdr: Arc<dyn HerdrApi>,
    interrupt_on_timeout: bool,
    read_lines: u32,
    settle_window: Duration,
}

impl PaneRunner {
    pub fn new(name: &str, herdr: Arc<dyn HerdrApi>, interrupt_on_timeout: bool, read_lines: u32) -> Self {
        Self { name: name.to_string(), herdr, interrupt_on_timeout, read_lines, settle_window: Duration::from_secs(30) }
    }

    pub fn with_settle_window(mut self, d: Duration) -> Self {
        self.settle_window = d;
        self
    }

    fn label(req: &AgentRequest) -> String {
        format!("{} {} · {}", req.display, req.step_id, req.runner_name)
    }

    fn target(b: &AgentBinding) -> Result<String> {
        b.agent_name.clone().or_else(|| b.pane_id.clone()).ok_or_else(|| anyhow::anyhow!("agent binding has no target"))
    }

    /// Distinguish "agent exited, pane still there" from "pane gone".
    fn lost_reason(&self, b: &AgentBinding) -> String {
        let pane_alive = b.pane_id.as_deref().and_then(|p| self.herdr.get_pane(p).ok().flatten()).is_some();
        if pane_alive { "agent process exited in its pane".into() } else { "agent pane was closed".into() }
    }

    fn transcript(&self, b: &AgentBinding) -> String {
        b.pane_id
            .as_deref()
            .and_then(|p| self.herdr.read_pane(p, self.read_lines).ok())
            .unwrap_or_default()
    }

    fn outcome(&self, req: &AgentRequest, b: &AgentBinding, end: AgentEnd) -> AgentOutcome {
        let transcript = if matches!(end, AgentEnd::Lost(_)) { String::new() } else { self.transcript(b) };
        AgentOutcome {
            end,
            binding: b.clone(),
            usage: UsageRecord::unknown(None),
            output_file_text: read_output_file(&req.output_file),
            transcript,
            exit_code: None,
        }
    }

    /// Wait until the agent is ready for input (handles startup prompts
    /// such as "trust this folder?" which Herdr reports as blocked).
    fn wait_ready(
        &self,
        target: &str,
        b: &mut AgentBinding,
        deadline: Instant,
        cancel: &CancelToken,
        events: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<AgentEnd>> {
        let mut blocked_reported = false;
        loop {
            if cancel.is_cancelled() {
                return Ok(Some(AgentEnd::Cancelled));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Some(AgentEnd::TimedOut));
            }
            match self.herdr.wait_agent(target, &[AgentStatus::Idle, AgentStatus::Done], CHUNK.min(deadline - now)) {
                Ok(info) => {
                    b.last_status = Some(info.agent_status.as_str().into());
                    if blocked_reported {
                        // The agent's UI just closed a dialog; input sent in
                        // this instant can be swallowed. Let it settle, then
                        // confirm it is still idle.
                        std::thread::sleep(Duration::from_millis(1500));
                        match self.herdr.get_agent(target) {
                            Ok(Some(i)) if i.agent_status.is_settled() => {}
                            Ok(Some(_)) => continue,
                            Ok(None) => return Ok(Some(AgentEnd::Lost(self.lost_reason(b)))),
                            Err(e) => return Err(e.into()),
                        }
                        events(AgentEvent::Unblocked);
                    }
                    // Herdr reports whether the agent's input is ready.
                    if !info.interactive_ready {
                        let until = Instant::now() + Duration::from_secs(10).min(deadline.saturating_duration_since(Instant::now()));
                        while Instant::now() < until && !cancel.is_cancelled() {
                            match self.herdr.get_agent(target) {
                                Ok(Some(i)) if i.interactive_ready => break,
                                Ok(Some(_)) => std::thread::sleep(Duration::from_millis(250)),
                                _ => break,
                            }
                        }
                    }
                    return Ok(None);
                }
                Err(e) if e.is_wait_timeout() => {
                    if let Ok(Some(info)) = self.herdr.get_agent(target) {
                        if info.agent_status == AgentStatus::Blocked && !blocked_reported {
                            blocked_reported = true;
                            events(AgentEvent::Blocked("agent is waiting for input during startup".into()));
                        }
                    }
                }
                Err(e) if e.is_not_found() || e.code() == Some("agent_not_found") => {
                    return Ok(Some(AgentEnd::Lost(format!("agent disappeared during startup: {e}"))))
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Wait until the agent has been idle and input-ready for two
    /// consecutive observations a second apart (bounded to 20s).
    fn wait_stable_idle(&self, target: &str, cancel: &CancelToken) {
        let until = Instant::now() + Duration::from_secs(20);
        let mut stable = 0;
        while stable < 2 && Instant::now() < until && !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(1000));
            match self.herdr.get_agent(target) {
                Ok(Some(i)) if i.agent_status.is_settled() && i.interactive_ready => stable += 1,
                Ok(Some(_)) => stable = 0,
                _ => return,
            }
        }
    }

    /// Core wait loop after the prompt was submitted.
    fn wait_settled(
        &self,
        req: &AgentRequest,
        b: &mut AgentBinding,
        deadline: Instant,
        cancel: &CancelToken,
        events: &mut dyn FnMut(AgentEvent),
    ) -> Result<AgentOutcome> {
        let target = Self::target(b)?;
        let mut blocked = false;
        loop {
            if cancel.is_cancelled() {
                let _ = self.interrupt(b);
                return Ok(self.outcome(req, b, AgentEnd::Cancelled));
            }
            let now = Instant::now();
            if now >= deadline {
                if self.interrupt_on_timeout {
                    let _ = self.interrupt(b);
                }
                return Ok(self.outcome(req, b, AgentEnd::TimedOut));
            }
            let chunk = CHUNK.min(deadline - now);
            let r = if blocked {
                self.herdr.wait_agent(&target, NOT_BLOCKED, chunk)
            } else {
                self.herdr.wait_agent(&target, SETTLED, chunk)
            };
            match r {
                Ok(info) => {
                    b.last_status = Some(info.agent_status.as_str().into());
                    if let Some(s) = info.agent_session.as_ref().and_then(|s| s.value.clone()) {
                        b.agent_session = Some(s);
                    }
                    match info.agent_status {
                        AgentStatus::Blocked if !blocked => {
                            blocked = true;
                            events(AgentEvent::Blocked("agent is waiting for a human in its pane".into()));
                        }
                        AgentStatus::Working if blocked => {
                            blocked = false;
                            events(AgentEvent::Unblocked);
                        }
                        s if s.is_settled() => {
                            if blocked {
                                events(AgentEvent::Unblocked);
                            }
                            return Ok(self.outcome(req, b, AgentEnd::Completed));
                        }
                        _ => {}
                    }
                }
                Err(e) if e.is_wait_timeout() => continue,
                Err(e) if e.is_not_found() || e.code() == Some("agent_not_found") => {
                    return Ok(self.outcome(req, b, AgentEnd::Lost(self.lost_reason(b))));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl AgentRunner for PaneRunner {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> RunnerCapabilities {
        RunnerCapabilities { interactive: true, reports_usage: false, needs_herdr: true, resumable_session: true }
    }

    fn start(&self, req: &AgentRequest, cancel: &CancelToken, events: &mut dyn FnMut(AgentEvent)) -> Result<AgentBinding> {
        // Reuse the previous attempt's agent if it is alive and idle.
        if let Some(prev) = &req.previous {
            if let Some(name) = &prev.agent_name {
                if let Ok(Some(info)) = self.herdr.get_agent(name) {
                    if info.agent_status.is_settled() {
                        let mut b = prev.clone();
                        b.output_file = Some(req.output_file.clone());
                        b.prompt_sent = false;
                        b.last_status = Some(info.agent_status.as_str().into());
                        events(AgentEvent::Started(b.clone()));
                        return Ok(b);
                    }
                }
            }
        }
        let Some(ws) = req.workspace_id.clone() else {
            bail!("no Herdr workspace for run {}", req.run_id);
        };
        let kind = req.profile.kind.clone().unwrap_or_else(|| req.runner_name.clone());
        let label = Self::label(req);
        let tab = self.herdr.create_tab(&ws, &req.worktree, &label, &req.pane_env)?;
        let name = agent_name(&req.task_id, &req.variant, &req.step_id, &req.exec_id);
        let mut b = AgentBinding {
            mode: "pane".into(),
            agent_kind: Some(kind.clone()),
            agent_name: Some(name.clone()),
            pane_id: Some(tab.pane_id.clone()),
            tab_id: Some(tab.tab_id.clone()),
            workspace_id: Some(ws),
            terminal_id: None,
            agent_session: None,
            output_file: Some(req.output_file.clone()),
            prompt_sent: false,
            last_status: None,
        };
        let _ = self.herdr.rename_pane(&tab.pane_id, &label);
        let mut tokens = BTreeMap::new();
        tokens.insert("orch_run".to_string(), Some(req.display.clone()));
        tokens.insert("orch_step".to_string(), Some(req.step_id.clone()));
        let _ = self.herdr.report_metadata(&tab.pane_id, &label, &tokens);
        // Persist the pane before launching so a crash here is recoverable.
        events(AgentEvent::Started(b.clone()));
        // A freshly created pane needs a moment before its shell owns the
        // foreground; Herdr answers `agent_pane_busy` until then.
        let ready_by = Instant::now() + req.startup_timeout.min(Duration::from_secs(30));
        let started = loop {
            match self.herdr.start_agent(&name, &kind, &tab.pane_id, &req.profile.pane_args, req.startup_timeout) {
                Err(HerdrError::Api { code, .. }) if code == "agent_pane_busy" && Instant::now() < ready_by && !cancel.is_cancelled() => {
                    std::thread::sleep(Duration::from_millis(300));
                }
                other => break other,
            }
        };
        match started {
            Ok(info) => {
                b.terminal_id = info.terminal_id;
                b.last_status = Some(info.agent_status.as_str().into());
                b.agent_session = info.agent_session.and_then(|s| s.value);
                // Agents keep initializing after Herdr reports them ready
                // (update banners, MCP servers…); input typed then is lost.
                self.wait_stable_idle(&name, cancel);
            }
            Err(HerdrError::Api { code, message }) if code == "agent_not_ready" || code == "timeout" => {
                // Blocked at startup (e.g. trust prompt): let a human handle it.
                tracing::info!("agent {name} not ready after start: {message}");
                let deadline = Instant::now() + req.timeout;
                if let Some(end) = self.wait_ready(&name, &mut b, deadline, cancel, events)? {
                    bail!("agent {name} never became ready: {end:?}");
                }
            }
            Err(e) => return Err(e.into()),
        }
        events(AgentEvent::Started(b.clone()));
        Ok(b)
    }

    fn send_and_wait(
        &self,
        req: &AgentRequest,
        b: &mut AgentBinding,
        deadline: Instant,
        cancel: &CancelToken,
        events: &mut dyn FnMut(AgentEvent),
    ) -> Result<AgentOutcome> {
        let first = self.send_once(req, b, deadline, cancel, events, None)?;
        if first.end != AgentEnd::Completed || first.output_file_text.is_some() {
            return Ok(first);
        }
        // The agent settled without writing its result file: the prompt may
        // have been lost (e.g. typed while the agent was still starting).
        // Remind it once; the reminder is safe to receive twice.
        let reminder = format!(
            "You have not written the result file yet: {}\n\
             If you have not started the task below, do it now. If you already \
             finished it, do not redo any work: only write the result file.\n\n{}",
            req.output_file.display(),
            req.followup_prompt
        );
        tracing::info!("agent settled without a result file; sending one reminder");
        self.send_once(req, b, deadline, cancel, events, Some(reminder))
    }
    fn attach(
        &self,
        req: &AgentRequest,
        b: &mut AgentBinding,
        deadline: Instant,
        cancel: &CancelToken,
        events: &mut dyn FnMut(AgentEvent),
    ) -> Result<AgentOutcome> {
        self.wait_settled(req, b, deadline, cancel, events)
    }

    fn interrupt(&self, b: &AgentBinding) -> Result<()> {
        let t = Self::target(b)?;
        let keys: Vec<&str> = match b.agent_kind.as_deref() {
            Some("claude") | Some("codex") => vec!["esc"],
            _ => vec!["ctrl+c"],
        };
        self.herdr.send_keys(&t, &keys)?;
        Ok(())
    }

    fn recover(&self, b: &AgentBinding) -> RecoveryAssessment {
        let Some(name) = &b.agent_name else { return RecoveryAssessment::Gone };
        match self.herdr.get_agent(name) {
            Ok(Some(i)) => RecoveryAssessment::Alive(i.agent_status.as_str().into()),
            Ok(None) => RecoveryAssessment::Gone,
            Err(e) => RecoveryAssessment::Unknown(e.to_string()),
        }
    }

    fn close(&self, b: &AgentBinding) -> Result<()> {
        if let Some(p) = &b.pane_id {
            self.herdr.close_pane(p)?;
        }
        Ok(())
    }
}


impl PaneRunner {
    fn send_once(
        &self,
        req: &AgentRequest,
        b: &mut AgentBinding,
        deadline: Instant,
        cancel: &CancelToken,
        events: &mut dyn FnMut(AgentEvent),
        override_text: Option<String>,
    ) -> Result<AgentOutcome> {
        let target = Self::target(b)?;
        // A reused agent already knows the task; send the compact prompt.
        let reused = req.previous.as_ref().and_then(|p| p.agent_name.clone()) == b.agent_name && req.previous.is_some();
        let text = match &override_text {
            Some(t) => t,
            None if reused => &req.followup_prompt,
            None => &req.prompt,
        };
        if let Some(end) = self.wait_ready(&target, b, deadline, cancel, events)? {
            return Ok(self.outcome(req, b, end));
        }
        events(AgentEvent::PromptSending);
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.herdr.prompt_agent(&target, text, Some((SETTLED, CHUNK.min(remaining)))) {
            Ok(info) => {
                b.prompt_sent = true;
                events(AgentEvent::PromptSent);
                b.last_status = Some(info.agent_status.as_str().into());
                if info.agent_status.is_settled() && self.confirm_settled(req, &target, deadline, cancel) {
                    return Ok(self.outcome(req, b, AgentEnd::Completed));
                }
            }
            Err(e) if e.is_wait_timeout() => {
                b.prompt_sent = true;
                events(AgentEvent::PromptSent);
            }
            Err(HerdrError::Api { code, message }) if code == "agent_prompt_stalled" => {
                // Herdr saw no activity within 5s. The text may or may not
                // have reached the agent: do not resend blindly.
                b.prompt_sent = true;
                events(AgentEvent::PromptSent);
                match self.herdr.get_agent(&target) {
                    Ok(Some(i)) if matches!(i.agent_status, AgentStatus::Working | AgentStatus::Blocked) => {}
                    _ => return Ok(self.outcome(req, b, AgentEnd::Stalled(format!("prompt stalled: {message}")))),
                }
            }
            Err(HerdrError::Api { code, .. }) if code == "agent_blocked" => {
                events(AgentEvent::Blocked("agent was blocked before the prompt could be sent".into()));
                if let Some(end) = self.wait_ready(&target, b, deadline, cancel, events)? {
                    return Ok(self.outcome(req, b, end));
                }
                return self.send_once(req, b, deadline, cancel, events, override_text);
            }
            Err(e) if e.is_not_found() || e.code() == Some("agent_not_found") => {
                return Ok(self.outcome(req, b, AgentEnd::Lost(self.lost_reason(b))));
            }
            Err(e) => return Err(e.into()),
        }
        // The result file is the real "I am finished" signal. `idle` without
        // it is only accepted after the agent stays idle for a whole settle
        // window (real-run finding: Claude flickers to idle between tool
        // calls and permission prompts, and a run was committed mid-work).
        loop {
            let out = self.wait_settled(req, b, deadline, cancel, events)?;
            if out.end != AgentEnd::Completed || out.output_file_text.is_some() || self.confirm_settled(req, &target, deadline, cancel) {
                return Ok(self.outcome(req, b, out.end));
            }
        }
    }

    /// A settled state without a result file is not trusted yet: the agent
    /// must stay idle for a whole `settle_window`. Returns `true` when it
    /// did (or wrote its result file), `false` as soon as it works again.
    fn confirm_settled(&self, req: &AgentRequest, target: &str, deadline: Instant, cancel: &CancelToken) -> bool {
        let window_end = (Instant::now() + self.settle_window).min(deadline);
        loop {
            if read_output_file(&req.output_file).is_some() {
                return true;
            }
            if Instant::now() >= window_end || cancel.is_cancelled() {
                return true;
            }
            match self.herdr.get_agent(target) {
                Ok(Some(i)) if matches!(i.agent_status, AgentStatus::Working | AgentStatus::Blocked) => return false,
                Ok(Some(_)) => {}
                _ => return true,
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}
