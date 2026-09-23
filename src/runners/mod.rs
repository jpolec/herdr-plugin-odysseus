//! Provider-neutral agent runners.
//!
//! The engine drives every agent through [`AgentRunner`]; nothing
//! provider-specific leaks into it. Provider details (Herdr agent kind,
//! launch args, headless argv, interrupt keys) live in [`profiles`].

pub mod fake;
pub mod headless;
pub mod pane;
pub mod profiles;
pub mod shell;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::herdr::HerdrApi;
use crate::model::{AgentBinding, UsageRecord};
use crate::process::CancelToken;
pub use profiles::{RunnerMode, RunnerProfile};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerCapabilities {
    /// A human can watch and type into the agent's terminal.
    pub interactive: bool,
    /// Provider reports token usage / cost.
    pub reports_usage: bool,
    pub needs_herdr: bool,
    /// A retry can continue the same agent session (keeps context).
    pub resumable_session: bool,
}

/// Everything a runner needs for one step execution.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub run_id: String,
    pub task_id: String,
    pub display: String,
    pub variant: String,
    pub step_id: String,
    pub exec_id: String,
    pub attempt: u32,
    pub runner_name: String,
    pub profile: RunnerProfile,
    pub worktree: PathBuf,
    pub workspace_id: Option<String>,
    /// Full prompt for a fresh agent.
    pub prompt: String,
    /// Compact prompt for an agent that already has the context (retries).
    pub followup_prompt: String,
    pub output_file: PathBuf,
    pub log_path: PathBuf,
    pub timeout: Duration,
    pub startup_timeout: Duration,
    /// Child environment for process-based runners (already filtered).
    pub env: BTreeMap<String, String>,
    /// Live agent from a previous attempt of this step (reuse on retry).
    pub previous: Option<AgentBinding>,
    /// Extra environment exported into a Herdr pane.
    pub pane_env: BTreeMap<String, String>,
}

/// Lifecycle notifications so the engine can persist and audit each
/// transition as it happens (write-ahead of the next action).
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // short-lived, passed by value once per transition
pub enum AgentEvent {
    /// Pane/process allocated and agent launched (persist the binding!).
    Started(AgentBinding),
    /// About to submit the prompt.
    PromptSending,
    PromptSent,
    /// Agent is waiting for a human inside its own UI.
    Blocked(String),
    Unblocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum AgentEnd {
    Completed,
    Failed(String),
    TimedOut,
    Cancelled,
    /// Pane/process vanished.
    Lost(String),
    /// Prompt may not have been delivered; needs a human.
    Stalled(String),
}

#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub end: AgentEnd,
    pub binding: AgentBinding,
    pub usage: UsageRecord,
    /// Content of the output file if the agent wrote it.
    pub output_file_text: Option<String>,
    /// Fallback transcript (pane tail or stdout), redacted by the caller.
    pub transcript: String,
    pub exit_code: Option<i32>,
}

/// Persisted-binding reassessment after a restart.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryAssessment {
    /// Agent still alive; status as reported by Herdr.
    Alive(String),
    /// Process/pane gone.
    Gone,
    /// Cannot tell (Herdr unreachable…).
    Unknown(String),
}

pub trait AgentRunner: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> RunnerCapabilities;

    /// Launch (or reattach to) the agent. Must be idempotent per exec id.
    fn start(&self, req: &AgentRequest, cancel: &CancelToken, events: &mut dyn FnMut(AgentEvent)) -> Result<AgentBinding>;

    /// Submit the prompt and wait until the agent settles, is cancelled,
    /// times out or disappears.
    fn send_and_wait(
        &self,
        req: &AgentRequest,
        binding: &mut AgentBinding,
        deadline: Instant,
        cancel: &CancelToken,
        events: &mut dyn FnMut(AgentEvent),
    ) -> Result<AgentOutcome>;

    /// Wait on an agent whose prompt was already sent (recovery path).
    fn attach(
        &self,
        req: &AgentRequest,
        binding: &mut AgentBinding,
        deadline: Instant,
        cancel: &CancelToken,
        events: &mut dyn FnMut(AgentEvent),
    ) -> Result<AgentOutcome> {
        let _ = (req, binding, deadline, cancel, events);
        anyhow::bail!("runner {} cannot reattach to a running agent", self.name())
    }

    fn interrupt(&self, binding: &AgentBinding) -> Result<()>;
    fn recover(&self, binding: &AgentBinding) -> RecoveryAssessment;
    /// Release resources (close pane) after success, if configured.
    fn close(&self, _binding: &AgentBinding) -> Result<()> {
        Ok(())
    }

    /// Convenience: start + send_and_wait.
    fn run(&self, req: &AgentRequest, cancel: &CancelToken, events: &mut dyn FnMut(AgentEvent)) -> Result<AgentOutcome> {
        let deadline = Instant::now() + req.timeout;
        let mut b = self.start(req, cancel, events)?;
        self.send_and_wait(req, &mut b, deadline, cancel, events)
    }
}

/// Builds runners by name. Holds the optional Herdr connection.
#[derive(Clone)]
pub struct RunnerFactory {
    pub herdr: Option<Arc<dyn HerdrApi>>,
    pub herdr_required: bool,
    pub overrides: BTreeMap<String, crate::config::RunnerProfileConfig>,
    pub interrupt_on_timeout: bool,
    pub pane_read_lines: u32,
}

impl RunnerFactory {
    pub fn profile(&self, name: &str) -> Result<RunnerProfile> {
        profiles::resolve(name, self.overrides.get(name))
    }

    /// Resolve the runner for `name`, choosing the pane or headless mode
    /// based on Herdr availability.
    pub fn build(&self, name: &str) -> Result<(Box<dyn AgentRunner>, RunnerProfile)> {
        let profile = self.profile(name)?;
        let runner: Box<dyn AgentRunner> = match profile.mode {
            RunnerMode::Fake => Box::new(fake::FakeRunner::new(name, &profile.fake_scenario.clone().unwrap_or_default())?),
            RunnerMode::Shell => Box::new(shell::ShellRunner::new(name)),
            RunnerMode::Headless => Box::new(headless::HeadlessRunner::new(name)),
            RunnerMode::Pane => match &self.herdr {
                Some(h) => Box::new(pane::PaneRunner::new(name, h.clone(), self.interrupt_on_timeout, self.pane_read_lines)),
                None if self.herdr_required => {
                    anyhow::bail!("runner {name} needs Herdr (herdr.mode: required) but Herdr is not reachable")
                }
                None if profile.headless_command.is_some() => {
                    tracing::info!("Herdr unavailable: running {name} headless");
                    let mut p = profile.clone();
                    p.mode = RunnerMode::Headless;
                    return Ok((Box::new(headless::HeadlessRunner::new(name)), p));
                }
                None => anyhow::bail!("runner {name} needs a Herdr pane but Herdr is not reachable and no headless command is configured"),
            },
        };
        Ok((runner, profile))
    }
}

/// Read an agent-written output file, bounded to 1 MiB.
pub fn read_output_file(p: &std::path::Path) -> Option<String> {
    let md = std::fs::symlink_metadata(p).ok()?;
    // Refuse symlinks: the agent must not point us at arbitrary files.
    if !md.is_file() || md.len() > 1024 * 1024 {
        return None;
    }
    std::fs::read_to_string(p).ok()
}
