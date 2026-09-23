//! Arbitrary argv agents (aider, scripts, in-house tools). Same mechanics as
//! the headless runner: direct argv execution, prompt on stdin and in
//! `$HERDR_ORCH_PROMPT_FILE`, result expected in `$HERDR_ORCH_OUTPUT_FILE`.

use std::time::Instant;

use anyhow::Result;

use super::headless::HeadlessRunner;
use super::*;

pub struct ShellRunner {
    inner: HeadlessRunner,
    name: String,
}

impl ShellRunner {
    pub fn new(name: &str) -> Self {
        Self { inner: HeadlessRunner::new(name), name: name.to_string() }
    }
}

impl AgentRunner for ShellRunner {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> RunnerCapabilities {
        RunnerCapabilities { interactive: false, reports_usage: false, needs_herdr: false, resumable_session: false }
    }
    fn start(&self, req: &AgentRequest, cancel: &CancelToken, events: &mut dyn FnMut(AgentEvent)) -> Result<AgentBinding> {
        let mut b = self.inner.start(req, cancel, events)?;
        b.mode = "shell".into();
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
        self.inner.send_and_wait(req, b, deadline, cancel, events)
    }
    fn interrupt(&self, b: &AgentBinding) -> Result<()> {
        self.inner.interrupt(b)
    }
    fn recover(&self, b: &AgentBinding) -> RecoveryAssessment {
        self.inner.recover(b)
    }
}
