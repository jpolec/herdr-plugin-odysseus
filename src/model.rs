//! Core domain model: tasks, runs, step executions and their explicit
//! state machines. Status values are the single source of truth; nothing
//! is ever inferred from presentation.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::policies::PolicyDecision;

pub type Timestamp = DateTime<Utc>;

pub fn now() -> Timestamp {
    Utc::now()
}

/// Short random hex identifier with a type prefix, e.g. `run-3f9a1c0b7d2e`.
pub fn new_id(prefix: &str) -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &u[..12])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Blocked,
    AwaitingApproval,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Preparing,
    Running,
    AwaitingApproval,
    Blocked,
    NeedsHuman,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    /// States in which a driver thread is (or should be) actively working.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Preparing | Self::Running | Self::AwaitingApproval
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Blocked => "blocked",
            Self::NeedsHuman => "needs_human",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Explicit transition table. Terminal states are final except that a
    /// human `run retry` may move failed/cancelled/blocked/needs_human back
    /// to `pending` (handled by [`RunStatus::can_resume`]).
    pub fn can_transition_to(self, next: RunStatus) -> bool {
        use RunStatus::*;
        if self == next {
            return true;
        }
        match self {
            Pending => matches!(next, Preparing | Running | Cancelled | Failed | NeedsHuman),
            Preparing => matches!(next, Running | Failed | Cancelled | Blocked | NeedsHuman),
            Running => matches!(
                next,
                AwaitingApproval | Blocked | NeedsHuman | Succeeded | Failed | Cancelled
            ),
            AwaitingApproval => matches!(next, Running | Blocked | Failed | Cancelled | NeedsHuman),
            Blocked | NeedsHuman => matches!(next, Pending | Running | Cancelled | Failed),
            Succeeded => false,
            Failed | Cancelled => matches!(next, Pending),
        }
    }

    pub fn can_resume(self) -> bool {
        matches!(self, Self::Failed | Self::Cancelled | Self::Blocked | Self::NeedsHuman)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Starting,
    Running,
    Retrying,
    AwaitingApproval,
    /// The agent is blocked inside its own UI (permission prompt, question).
    AwaitingHuman,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
}

impl StepStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Skipped | Self::Cancelled)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Retrying => "retrying",
            Self::AwaitingApproval => "awaiting_approval",
            Self::AwaitingHuman => "awaiting_human",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn can_transition_to(self, next: StepStatus) -> bool {
        use StepStatus::*;
        if self == next {
            return true;
        }
        match self {
            Pending => matches!(next, Starting | Skipped | Cancelled | AwaitingApproval | Failed),
            Starting => matches!(next, Running | Failed | Cancelled | AwaitingApproval),
            Running => matches!(
                next,
                AwaitingHuman | AwaitingApproval | Succeeded | Failed | Cancelled | Retrying
            ),
            AwaitingHuman => matches!(next, Running | Succeeded | Failed | Cancelled),
            AwaitingApproval => matches!(next, Running | Succeeded | Failed | Cancelled | Starting),
            Retrying => matches!(next, Starting | Running | Failed | Cancelled),
            Succeeded | Failed | Skipped | Cancelled => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskOptions {
    pub workflow: Option<String>,
    pub runner: Option<String>,
    pub base_ref: Option<String>,
    #[serde(default = "one")]
    pub variants: u32,
    /// Optional per-variant runner list, e.g. `[codex, codex, claude]`.
    #[serde(default)]
    pub variant_runners: Vec<String>,
    /// Explicit per-step runner overrides (`--step-runner review=claude`).
    #[serde(default)]
    pub step_runners: BTreeMap<String, String>,
    #[serde(default)]
    pub dry_run: bool,
}

fn one() -> u32 {
    1
}

impl Default for TaskOptions {
    fn default() -> Self {
        Self {
            workflow: None,
            runner: None,
            base_ref: None,
            variants: 1,
            variant_runners: vec![],
            step_runners: BTreeMap::new(),
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    /// Human-friendly monotonically increasing number, e.g. `124`.
    pub task_id: String,
    pub title: String,
    pub description: String,
    pub repo_root: PathBuf,
    pub status: TaskStatus,
    pub options: TaskOptions,
    pub initiator: Initiator,
    #[serde(default)]
    pub source: Option<TaskSource>,
    #[serde(default)]
    pub run_ids: Vec<String>,
    /// Variant explicitly selected by a human (tournament mode).
    #[serde(default)]
    pub selected_run: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskSource {
    Manual,
    GithubIssue { repo: String, number: u64, url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Initiator {
    /// `cli`, `ui`, `plugin_action`, `github_issue` …
    pub via: String,
    pub user: Option<String>,
}

impl Initiator {
    pub fn current(via: &str) -> Self {
        Self {
            via: via.to_string(),
            user: std::env::var("USER").ok(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct GitState {
    pub base_ref: String,
    pub base_sha: Option<String>,
    pub branch: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub head_sha: Option<String>,
    pub dirty: Option<bool>,
    #[serde(default)]
    pub commits: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HerdrBinding {
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Run {
    pub run_id: String,
    pub task_id: String,
    /// 0 for single runs; 0..n for variants (displayed A, B, C…).
    pub variant_index: u32,
    pub variant_count: u32,
    pub workflow_name: String,
    pub workflow_sha256: String,
    /// Snapshot of the workflow as executed (for audit and recovery).
    pub workflow_yaml: String,
    pub runner_override: Option<String>,
    pub repo_root: PathBuf,
    pub git: GitState,
    pub herdr: HerdrBinding,
    pub status: RunStatus,
    pub status_reason: Option<String>,
    pub initiator: Initiator,
    pub dry_run: bool,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub updated_at: Timestamp,
    pub completed_at: Option<Timestamp>,
    /// Index into the workflow's step list of the step being executed.
    pub cursor: usize,
    #[serde(default)]
    pub steps: Vec<StepExecution>,
    #[serde(default)]
    pub policy_decisions: Vec<PolicyDecisionRecord>,
    #[serde(default)]
    pub approvals: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    /// Retry counters keyed by `on_failure` source step id.
    #[serde(default)]
    pub retry_counts: BTreeMap<String, u32>,
    #[serde(default)]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub diff_stat: Option<DiffStat>,
    /// Failure feedback waiting to be delivered to the retried step.
    #[serde(default)]
    pub pending_feedback: Option<String>,
    /// Bounded step outputs for `{{step.<id>.output}}` / `{{previous.output}}`.
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    /// Paths a human approved in this run (not re-asked unless changed again).
    #[serde(default)]
    pub approved_paths: BTreeMap<String, String>,
    /// An explicit approval step covering the next step's gated actions.
    #[serde(default)]
    pub approval_cover: Option<ApprovalCover>,
    /// Per-step runner overrides (task options / variant runners).
    #[serde(default)]
    pub step_runners: BTreeMap<String, String>,
    /// Set by recovery: the driver must reattach instead of relaunching.
    #[serde(default)]
    pub recovered: bool,
    /// Human-selected variant (tournament mode).
    #[serde(default)]
    pub selected: bool,
}

/// "The human approved what the next step is about to do" — valid only for
/// that step and only while HEAD is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalCover {
    pub step_id: String,
    pub approval_id: String,
    pub head_sha: Option<String>,
}

impl Run {
    pub fn variant_label(&self) -> String {
        if self.variant_count <= 1 {
            String::new()
        } else {
            let c = (b'A' + (self.variant_index % 26) as u8) as char;
            c.to_string()
        }
    }

    pub fn display_name(&self) -> String {
        let v = self.variant_label();
        if v.is_empty() {
            format!("#{}", self.task_id)
        } else {
            format!("#{}{}", self.task_id, v)
        }
    }

    /// Latest execution of a workflow step (by id), if any.
    pub fn latest_exec(&self, step_id: &str) -> Option<&StepExecution> {
        self.steps.iter().rev().find(|e| e.step_id == step_id)
    }

    pub fn latest_exec_mut(&mut self, step_id: &str) -> Option<&mut StepExecution> {
        self.steps.iter_mut().rev().find(|e| e.step_id == step_id)
    }

    pub fn exec_mut(&mut self, exec_id: &str) -> Option<&mut StepExecution> {
        self.steps.iter_mut().find(|e| e.exec_id == exec_id)
    }

    pub fn usage_total(&self) -> UsageRecord {
        UsageRecord::sum(self.steps.iter().filter_map(|s| s.usage.as_ref()))
    }

    pub fn policy_summary(&self) -> (usize, usize, usize) {
        let mut a = (0, 0, 0);
        for d in &self.policy_decisions {
            match d.decision.decision {
                crate::policies::Decision::Allow => a.0 += 1,
                crate::policies::Decision::RequireApproval => a.1 += 1,
                crate::policies::Decision::Deny => a.2 += 1,
            }
        }
        a
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Agent,
    Command,
    Approval,
    GithubPr,
    Git,
    Policy,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Command => "command",
            Self::Approval => "approval",
            Self::GithubPr => "github_pr",
            Self::Git => "git",
            Self::Policy => "policy",
        }
    }
}

/// One attempt at executing one workflow step. Persisted *before* the
/// side effect happens (write-ahead), updated after.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepExecution {
    pub exec_id: String,
    pub step_id: String,
    pub kind: StepKind,
    pub attempt: u32,
    pub status: StepStatus,
    pub runner: Option<String>,
    /// What we are about to do; recorded before doing it.
    pub intent: Option<String>,
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    #[serde(default)]
    pub agent: Option<AgentBinding>,
    #[serde(default)]
    pub process: Option<ProcessBinding>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// Bounded, redacted excerpt; full output lives in `log_path`.
    pub output_excerpt: Option<String>,
    pub log_path: Option<PathBuf>,
    /// Parsed structured output (review verdicts etc.).
    #[serde(default)]
    pub structured: Option<serde_json::Value>,
    #[serde(default)]
    pub parse_failed: bool,
    #[serde(default)]
    pub feedback_from: Option<String>,
    #[serde(default)]
    pub approval_id: Option<String>,
    #[serde(default)]
    pub usage: Option<UsageRecord>,
    #[serde(default)]
    pub changed_files: Option<usize>,
}

impl StepExecution {
    pub fn new(step_id: &str, kind: StepKind, attempt: u32) -> Self {
        Self {
            exec_id: new_id("x"),
            step_id: step_id.to_string(),
            kind,
            attempt,
            status: StepStatus::Pending,
            runner: None,
            intent: None,
            started_at: None,
            ended_at: None,
            agent: None,
            process: None,
            exit_code: None,
            error: None,
            output_excerpt: None,
            log_path: None,
            structured: None,
            parse_failed: false,
            feedback_from: None,
            approval_id: None,
            usage: None,
            changed_files: None,
        }
    }

    pub fn duration_secs(&self) -> Option<i64> {
        let start = self.started_at?;
        let end = self.ended_at.unwrap_or_else(now);
        Some((end - start).num_seconds().max(0))
    }
}

/// Association between a step execution and a live Herdr agent/pane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AgentBinding {
    pub mode: String,
    pub agent_kind: Option<String>,
    pub agent_name: Option<String>,
    pub pane_id: Option<String>,
    pub tab_id: Option<String>,
    pub workspace_id: Option<String>,
    pub terminal_id: Option<String>,
    pub agent_session: Option<String>,
    pub output_file: Option<PathBuf>,
    #[serde(default)]
    pub prompt_sent: bool,
    pub last_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcessBinding {
    pub pid: u32,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub started_at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyDecisionRecord {
    pub at: Timestamp,
    pub step_id: Option<String>,
    pub exec_id: Option<String>,
    pub subject: String,
    pub decision: PolicyDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Artifact {
    pub kind: String,
    pub name: String,
    pub path: Option<PathBuf>,
    pub url: Option<String>,
    pub step_id: Option<String>,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DiffStat {
    pub files_changed: usize,
    pub insertions: u64,
    pub deletions: u64,
    #[serde(default)]
    pub files: Vec<ChangedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChangedFile {
    pub path: String,
    /// `added`, `modified`, `deleted`, `renamed`, `untracked`, `typechange`
    pub change: String,
    pub insertions: u64,
    pub deletions: u64,
    #[serde(default)]
    pub old_path: Option<String>,
}

/// How much we trust a usage number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    Measured,
    Reported,
    Estimated,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageRecord {
    pub source: UsageSource,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub runtime_ms: Option<u64>,
    pub model: Option<String>,
}

impl UsageRecord {
    pub fn unknown(runtime_ms: Option<u64>) -> Self {
        Self {
            runtime_ms,
            ..Default::default()
        }
    }

    /// Aggregate records. The combined source is the *weakest* source that
    /// contributed a value; a sum mixing reported and unknown values is
    /// marked `estimated` because it is a lower bound.
    pub fn sum<'a>(records: impl Iterator<Item = &'a UsageRecord>) -> UsageRecord {
        fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (None, None) => None,
                (x, y) => Some(x.unwrap_or(0) + y.unwrap_or(0)),
            }
        }
        let mut out = UsageRecord::default();
        let mut sources = vec![];
        let mut any = false;
        for r in records {
            any = true;
            sources.push(r.source);
            out.input_tokens = add(out.input_tokens, r.input_tokens);
            out.output_tokens = add(out.output_tokens, r.output_tokens);
            out.cached_tokens = add(out.cached_tokens, r.cached_tokens);
            out.runtime_ms = add(out.runtime_ms, r.runtime_ms);
            out.cost_usd = match (out.cost_usd, r.cost_usd) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
            };
        }
        if !any {
            return out;
        }
        let has_value = out.cost_usd.is_some() || out.input_tokens.is_some();
        out.source = if !has_value {
            UsageSource::Unknown
        } else if sources.iter().all(|s| *s == UsageSource::Measured) {
            UsageSource::Measured
        } else if sources
            .iter()
            .all(|s| matches!(s, UsageSource::Measured | UsageSource::Reported))
        {
            UsageSource::Reported
        } else {
            UsageSource::Estimated
        };
        out
    }

    pub fn cost_display(&self) -> String {
        match (self.cost_usd, self.source) {
            (Some(c), UsageSource::Reported) => format!("${c:.2} reported"),
            (Some(c), UsageSource::Measured) => format!("${c:.2} measured"),
            (Some(c), _) => format!("~${c:.2} estimated"),
            (None, _) => "unknown".into(),
        }
    }
}

/// Result of executing a process (command step, headless agent…).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
    pub log_path: Option<PathBuf>,
}

impl ExecutionResult {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_transitions_are_explicit() {
        use RunStatus::*;
        assert!(Pending.can_transition_to(Preparing));
        assert!(Running.can_transition_to(AwaitingApproval));
        assert!(AwaitingApproval.can_transition_to(Running));
        assert!(!Succeeded.can_transition_to(Running));
        assert!(!Pending.can_transition_to(Succeeded));
        assert!(Failed.can_transition_to(Pending));
        assert!(!Failed.can_transition_to(Running));
    }

    #[test]
    fn step_transitions_are_explicit() {
        use StepStatus::*;
        assert!(Pending.can_transition_to(Starting));
        assert!(Starting.can_transition_to(Running));
        assert!(Running.can_transition_to(AwaitingHuman));
        assert!(AwaitingHuman.can_transition_to(Running));
        assert!(!Succeeded.can_transition_to(Running));
        assert!(!Pending.can_transition_to(Succeeded));
    }

    #[test]
    fn usage_sum_marks_mixed_sources_estimated() {
        let a = UsageRecord {
            source: UsageSource::Reported,
            cost_usd: Some(1.0),
            input_tokens: Some(10),
            ..Default::default()
        };
        let b = UsageRecord::unknown(Some(5));
        let s = UsageRecord::sum([a.clone(), b].iter());
        assert_eq!(s.source, UsageSource::Estimated);
        assert_eq!(s.cost_usd, Some(1.0));
        let s2 = UsageRecord::sum([a.clone(), a].iter());
        assert_eq!(s2.source, UsageSource::Reported);
        assert_eq!(s2.cost_display(), "$2.00 reported");
        let s3 = UsageRecord::sum([UsageRecord::unknown(None)].iter());
        assert_eq!(s3.source, UsageSource::Unknown);
        assert_eq!(s3.cost_display(), "unknown");
    }

    #[test]
    fn variant_labels() {
        let mut r = test_run();
        assert_eq!(r.display_name(), "#7");
        r.variant_count = 3;
        r.variant_index = 2;
        assert_eq!(r.display_name(), "#7C");
    }

    pub(crate) fn test_run() -> Run {
        Run {
            run_id: "run-1".into(),
            task_id: "7".into(),
            variant_index: 0,
            variant_count: 1,
            workflow_name: "w".into(),
            workflow_sha256: "x".into(),
            workflow_yaml: String::new(),
            runner_override: None,
            repo_root: "/tmp".into(),
            git: GitState::default(),
            herdr: HerdrBinding::default(),
            status: RunStatus::Pending,
            status_reason: None,
            initiator: Initiator::current("test"),
            dry_run: false,
            created_at: now(),
            started_at: None,
            updated_at: now(),
            completed_at: None,
            cursor: 0,
            steps: vec![],
            policy_decisions: vec![],
            approvals: vec![],
            artifacts: vec![],
            retry_counts: BTreeMap::new(),
            pr_url: None,
            diff_stat: None,
            pending_feedback: None,
            outputs: BTreeMap::new(),
            approved_paths: BTreeMap::new(),
            approval_cover: None,
            step_runners: BTreeMap::new(),
            recovered: false,
            selected: false,
        }
    }
}
