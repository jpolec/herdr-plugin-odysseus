//! The durable run driver: one thread per active run, executing workflow
//! steps as an explicit state machine.
//!
//! Write-ahead rule: before every side effect the driver persists a
//! `StepExecution` in `starting` with its intent, then acts, then persists
//! the outcome. Recovery can therefore distinguish "never started" from
//! "may have happened".

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::{refresh_task_status, runner_factory, EngineCtx};
use crate::audit::{Actor, EventDraft};
use crate::config::LoadedConfig;
use crate::model::*;
use crate::policies::PolicySet;
use crate::process::CancelToken;
use crate::runners::RunnerFactory;
use crate::store::FileLock;
use crate::workflow::catalog::Catalog;
use crate::workflow::{Step, Workflow};

/// Result of executing one step.
#[derive(Debug, Clone, PartialEq)]
pub enum StepOutcome {
    Succeeded { output: Option<String> },
    Skipped(String),
    Failed { reason: String, feedback: String },
    /// Policy violation that a human must resolve (run → blocked).
    Blocked(String),
    /// Ambiguous state (run → needs_human).
    NeedsHuman(String),
    Cancelled,
}

pub struct RunDriver<'a> {
    pub(super) ctx: &'a EngineCtx,
    pub run: Run,
    pub(super) wf: Workflow,
    pub(super) cfg: LoadedConfig,
    pub(super) policy: PolicySet,
    pub(super) factory: RunnerFactory,
    pub(super) cancel: CancelToken,
    pub(super) task: Task,
    pub(super) catalog: Catalog,
    pub(super) deadline: Instant,
    _lock: FileLock,
}

/// Polls the durable control document and flips the cancel token, so a
/// cancel issued from any process reaches blocking waits promptly.
struct ControlWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ControlWatcher {
    fn spawn(store: crate::store::Store, run_id: String, cancel: CancelToken, poll: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let handle = std::thread::Builder::new()
            .name(format!("ctl-{run_id}"))
            .spawn(move || {
                while !s2.load(Ordering::SeqCst) {
                    if let Ok(c) = store.load_control(&run_id) {
                        if c.cancel_requested {
                            cancel.cancel();
                        }
                    }
                    std::thread::sleep(poll);
                }
            })
            .ok();
        Self { stop, handle }
    }
}

impl Drop for ControlWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Drive a run to a stopping point (terminal, blocked, needs_human).
pub fn drive(ctx: &EngineCtx, run_id: &str, cancel: CancelToken) -> Result<RunStatus> {
    // Another process (daemon or a foreground CLI) already drives this run:
    // step aside without touching it.
    let lock = match FileLock::try_acquire(&ctx.store.layout.lock_path(&format!("run-{run_id}")))? {
        Some(l) => l,
        None => return Ok(ctx.store.load_run(run_id)?.status),
    };
    let mut d = match RunDriver::open_locked(ctx, run_id, cancel.clone(), lock) {
        Ok(d) => d,
        Err(e) => {
            // Could not even load the run context: fail it visibly.
            if let Ok(mut run) = ctx.store.load_run(run_id) {
                if !run.status.is_terminal() && run.status.can_transition_to(RunStatus::Failed) {
                    run.status = RunStatus::Failed;
                    run.status_reason = Some(format!("{e:#}"));
                    run.completed_at = Some(now());
                    let _ = ctx.store.save_run(&run);
                    ctx.audit(
                        EventDraft::new("run_failed", Actor::orchestrator())
                            .run(run_id, &run.task_id)
                            .data(serde_json::json!({"error": format!("{e:#}")})),
                    );
                    let _ = refresh_task_status(ctx, &run.task_id);
                }
            }
            return Err(e);
        }
    };
    let _watch = ControlWatcher::spawn(ctx.store.clone(), run_id.to_string(), cancel, ctx.poll);
    let status = match d.execute() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("run {run_id} driver error: {e:#}");
            d.fail(&format!("internal error: {e:#}"))?;
            RunStatus::Failed
        }
    };
    Ok(status)
}

impl<'a> RunDriver<'a> {
    pub fn open(ctx: &'a EngineCtx, run_id: &str, cancel: CancelToken) -> Result<Self> {
        let lock = FileLock::try_acquire(&ctx.store.layout.lock_path(&format!("run-{run_id}")))?
            .with_context(|| format!("run {run_id} is already being driven by another process"))?;
        Self::open_locked(ctx, run_id, cancel, lock)
    }

    fn open_locked(ctx: &'a EngineCtx, run_id: &str, cancel: CancelToken, lock: FileLock) -> Result<Self> {
        let run = ctx.store.load_run(run_id)?;
        let task = ctx.store.load_task(&run.task_id)?;
        // The run executes the workflow snapshot taken at creation time.
        let wf = Workflow::parse(&run.workflow_yaml).context("stored workflow snapshot is invalid")?;
        let cfg = ctx.load_config(Some(&run.repo_root))?;
        let policy = ctx.policy_for(&cfg)?;
        let factory = runner_factory(ctx, &cfg);
        let catalog = ctx.catalog(Some(&run.repo_root));
        let started = run.started_at.unwrap_or_else(now);
        let elapsed = (now() - started).to_std().unwrap_or_default();
        let deadline = Instant::now() + cfg.config.limits.max_runtime.as_duration().saturating_sub(elapsed);
        Ok(Self { ctx, run, wf, cfg, policy, factory, cancel, task, catalog, deadline, _lock: lock })
    }

    // ------------------------------------------------------------ state --

    pub(super) fn save(&mut self) -> Result<()> {
        self.run.updated_at = now();
        self.ctx.store.save_run(&self.run)
    }

    pub(super) fn audit(&self, event: &str, actor: Actor, step: Option<&str>, data: serde_json::Value) {
        let mut d = EventDraft::new(event, actor).run(&self.run.run_id, &self.run.task_id).data(data);
        if let Some(s) = step {
            d = d.step(s);
        }
        self.ctx.audit(d);
    }

    pub(super) fn set_status(&mut self, next: RunStatus, reason: Option<String>) -> Result<()> {
        let cur = self.run.status;
        if !cur.can_transition_to(next) {
            bail!("illegal run transition {} → {}", cur.as_str(), next.as_str());
        }
        self.run.status = next;
        self.run.status_reason = reason;
        if next.is_terminal() {
            self.run.completed_at = Some(now());
        }
        self.save()?;
        let _ = refresh_task_status(self.ctx, &self.run.task_id);
        Ok(())
    }

    pub(super) fn exec_mut(&mut self, exec_id: &str) -> &mut StepExecution {
        self.run.exec_mut(exec_id).expect("exec id belongs to this run")
    }

    pub(super) fn set_exec_status(&mut self, exec_id: &str, next: StepStatus) -> Result<()> {
        let e = self.exec_mut(exec_id);
        if !e.status.can_transition_to(next) {
            bail!("illegal step transition {} → {} ({})", e.status.as_str(), next.as_str(), e.step_id);
        }
        e.status = next;
        if next == StepStatus::Running && e.started_at.is_none() {
            e.started_at = Some(now());
        }
        if next.is_terminal() {
            e.ended_at = Some(now());
        }
        self.save()
    }

    /// New execution record for a step (attempt = previous count + 1).
    pub(super) fn new_exec(&mut self, step: &Step) -> Result<String> {
        let attempt = self.run.steps.iter().filter(|e| e.step_id == step.id).count() as u32 + 1;
        let mut e = StepExecution::new(&step.id, step.kind(), attempt);
        if let Some(fb) = &self.run.pending_feedback {
            e.feedback_from = Some(crate::store::sha256_hex(fb.as_bytes())[..12].to_string());
        }
        let id = e.exec_id.clone();
        self.run.steps.push(e);
        self.save()?;
        Ok(id)
    }

    pub(super) fn worktree(&self) -> Result<PathBuf> {
        self.run.git.worktree_path.clone().context("run has no worktree yet")
    }

    pub(super) fn step_timeout(&self, step: &Step, default: Duration) -> Duration {
        let t = step.timeout.or(self.wf.defaults.timeout).map(|d| d.as_duration()).unwrap_or(default);
        t.min(self.deadline.saturating_duration_since(Instant::now()))
    }

    pub(super) fn check_cancel(&self) -> bool {
        if self.cancel.is_cancelled() {
            return true;
        }
        if let Ok(c) = self.ctx.store.load_control(&self.run.run_id) {
            if c.cancel_requested {
                self.cancel.cancel();
                return true;
            }
        }
        false
    }

    pub(super) fn notify(&self, title: &str, body: &str, urgent: bool) {
        self.ctx.notify(self.cfg.config.herdr.notify, title, body, urgent);
    }

    /// Template variables available to this run.
    pub(super) fn template_ctx(&self, step_id: &str, output_file: Option<&std::path::Path>) -> crate::workflow::template::Context {
        let mut c = crate::workflow::template::Context::default();
        c.set("task", &self.task.description)
            .set("task_title", &self.task.title)
            .set("task.id", &self.task.task_id)
            .set("run.id", &self.run.run_id)
            .set("repo.root", self.run.repo_root.display().to_string())
            .set("step.id", step_id);
        if let Some(w) = &self.run.git.worktree_path {
            c.set("worktree.path", w.display().to_string());
        }
        if let Some(b) = &self.run.git.branch {
            c.set("branch", b);
        }
        if let Some(s) = &self.run.git.base_sha {
            c.set("base_sha", s);
        }
        if let Some(f) = output_file {
            c.set("output_file", f.display().to_string());
        }
        if let Some(fb) = &self.run.pending_feedback {
            c.set("feedback", format!("Feedback from the previous attempt:\n\n{fb}"));
        }
        c.set("acceptance", self.task.acceptance_text());
        c.set("contract", self.run.contract.as_ref().map(|k| k.describe()).unwrap_or_default());
        // previous.output = output of the step immediately before this one.
        if let Some(idx) = self.wf.step_index(step_id) {
            if idx > 0 {
                if let Some(o) = self.run.outputs.get(&self.wf.steps[idx - 1].id) {
                    c.set("previous.output", o);
                }
            }
        }
        for (k, v) in &self.run.outputs {
            c.set(&format!("step.{k}.output"), v);
        }
        c
    }

    pub(super) fn orch_env(&self, step_id: &str) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("HERDR_ORCH_RUN_ID".into(), self.run.run_id.clone());
        m.insert("HERDR_ORCH_TASK_ID".into(), self.run.task_id.clone());
        m.insert("HERDR_ORCH_STEP_ID".into(), step_id.to_string());
        m.insert("HERDR_ORCH_REPO_ROOT".into(), self.run.repo_root.display().to_string());
        if let Some(w) = &self.run.git.worktree_path {
            m.insert("HERDR_ORCH_WORKTREE".into(), w.display().to_string());
        }
        if let Some(b) = &self.run.git.branch {
            m.insert("HERDR_ORCH_BRANCH".into(), b.clone());
        }
        if let Some(s) = &self.run.git.base_sha {
            m.insert("HERDR_ORCH_BASE_SHA".into(), s.clone());
        }
        m
    }

    pub(super) fn record_output(&mut self, step_id: &str, text: &str) {
        let mut t = text.trim().to_string();
        if t.len() > 8 * 1024 {
            let mut cut = 8 * 1024;
            while !t.is_char_boundary(cut) {
                cut -= 1;
            }
            t.truncate(cut);
            t.push_str("\n… [truncated]");
        }
        self.run.outputs.insert(step_id.to_string(), t);
    }

    // --------------------------------------------------------- lifecycle --

    fn execute(&mut self) -> Result<RunStatus> {
        if self.run.status.is_terminal() {
            return Ok(self.run.status);
        }
        if self.check_cancel() {
            return self.cancelled("cancelled before start");
        }
        let first_start = self.run.started_at.is_none();
        if self.run.status == RunStatus::Pending {
            self.set_status(RunStatus::Preparing, None)?;
        }
        if first_start {
            self.run.started_at = Some(now());
            self.save()?;
            self.audit(
                "run_started",
                Actor::orchestrator(),
                None,
                serde_json::json!({
                    "workflow": self.wf.name,
                    "workflow_sha256": self.run.workflow_sha256,
                    "variant": self.run.variant_label(),
                    "repo": self.run.repo_root,
                    "dry_run": self.run.dry_run,
                }),
            );
        }
        if self.run.status == RunStatus::Preparing {
            if let Some(stop) = self.prepare()? {
                return Ok(stop);
            }
            self.set_status(RunStatus::Running, None)?;
        } else if self.run.status == RunStatus::AwaitingApproval {
            // Resumed while waiting: the approval step re-attaches below.
            self.set_status(RunStatus::Running, None)?;
        }
        if let Some(w) = &self.run.git.worktree_path {
            if !self.run.dry_run && !w.exists() {
                return self.stop(RunStatus::NeedsHuman, format!("worktree {} is missing", w.display()));
            }
        }
        while self.run.cursor < self.wf.steps.len() {
            if self.check_cancel() {
                return self.cancelled("cancelled by request");
            }
            self.wait_while_paused()?;
            if Instant::now() >= self.deadline {
                return self.fail_run(&format!("max_runtime {} exceeded", self.cfg.config.limits.max_runtime));
            }
            let step = self.wf.steps[self.run.cursor].clone();
            let outcome = self.execute_step(&step)?;
            match outcome {
                StepOutcome::Succeeded { output } => {
                    if let Some(o) = output {
                        self.record_output(&step.id, &o);
                    }
                    // Feedback is consumed by the step that received it.
                    if self.run.pending_feedback.is_some() && step.kind() == StepKind::Agent {
                        self.run.pending_feedback = None;
                        self.run.pending_feedback_step = None;
                    }
                    self.run.cursor += 1;
                    self.save()?;
                }
                StepOutcome::Skipped(why) => {
                    self.audit("step_skipped", Actor::orchestrator(), Some(&step.id), serde_json::json!({"reason": why}));
                    self.run.cursor += 1;
                    self.save()?;
                }
                StepOutcome::Failed { reason, feedback } => {
                    if let Some(next) = self.handle_failure(&step, &reason, &feedback)? {
                        return Ok(next);
                    }
                }
                StepOutcome::Blocked(why) => return self.stop(RunStatus::Blocked, why),
                StepOutcome::NeedsHuman(why) => return self.stop(RunStatus::NeedsHuman, why),
                StepOutcome::Cancelled => return self.cancelled("cancelled during step"),
            }
        }
        self.complete()
    }

    fn wait_while_paused(&mut self) -> Result<()> {
        let mut announced = false;
        loop {
            let c = self.ctx.store.load_control(&self.run.run_id)?;
            if !c.pause_requested || c.cancel_requested {
                if announced {
                    self.audit("run_resumed", Actor::orchestrator(), None, serde_json::json!({}));
                }
                return Ok(());
            }
            if !announced {
                announced = true;
                self.run.status_reason = Some("paused".into());
                self.save()?;
                self.audit("run_paused", Actor::orchestrator(), None, serde_json::json!({}));
            }
            std::thread::sleep(self.ctx.poll);
        }
    }

    /// Retry routing. Returns `Some(status)` when the run stops.
    fn handle_failure(&mut self, step: &Step, reason: &str, feedback: &str) -> Result<Option<RunStatus>> {
        if step.continue_on_failure {
            self.audit(
                "step_failure_ignored",
                Actor::orchestrator(),
                Some(&step.id),
                serde_json::json!({"reason": reason, "continue_on_failure": true}),
            );
            self.record_output(&step.id, feedback);
            self.run.cursor += 1;
            self.save()?;
            return Ok(None);
        }
        if let Some(of) = &step.on_failure {
            let used = *self.run.retry_counts.get(&step.id).unwrap_or(&0);
            let cap = of.max_attempts.min(self.cfg.config.limits.max_retries);
            if used < cap {
                let target = self.wf.step_index(&of.retry_step).context("retry_step vanished")?;
                self.run.retry_counts.insert(step.id.clone(), used + 1);
                self.run.pending_feedback = of.feedback.then(|| {
                    format!("The `{}` step failed: {reason}\n\n{feedback}", step.id)
                });
                self.run.pending_feedback_step = Some(step.id.clone());
                self.run.cursor = target;
                self.save()?;
                self.audit(
                    "retry_started",
                    Actor::orchestrator(),
                    Some(&step.id),
                    serde_json::json!({
                        "retry_step": of.retry_step,
                        "attempt": used + 1,
                        "max_attempts": cap,
                        "reason": reason,
                        "feedback_bytes": self.run.pending_feedback.as_ref().map(|f| f.len()),
                    }),
                );
                return Ok(None);
            }
            self.audit(
                "retries_exhausted",
                Actor::orchestrator(),
                Some(&step.id),
                serde_json::json!({"attempts": used, "max_attempts": cap}),
            );
            return Ok(Some(self.fail_run(&format!("step `{}` failed after {used} retr{}: {reason}", step.id, if used == 1 { "y" } else { "ies" }))?));
        }
        Ok(Some(self.fail_run(&format!("step `{}` failed: {reason}", step.id))?))
    }

    fn prepare(&mut self) -> Result<Option<RunStatus>> {
        let repo = self.run.repo_root.clone();
        if self.run.git.worktree_path.is_none() {
            let variant = (self.run.variant_count > 1).then(|| self.run.variant_label().chars().next().unwrap());
            let (path, branch) = crate::git::plan_names(
                &repo,
                &self.cfg.config.git.worktree_root,
                &self.cfg.config.git.branch_prefix,
                &self.run.task_id,
                &self.task.title,
                variant,
            )?;
            let base_sha = crate::git::rev_parse(&repo, &self.run.git.base_ref)?;
            self.run.git.branch = Some(branch);
            self.run.git.worktree_path = Some(path);
            self.run.git.base_sha = Some(base_sha);
            self.save()?; // write-ahead: names are fixed before creation
        }
        let path = self.worktree()?;
        let branch = self.run.git.branch.clone().unwrap();
        let base_sha = self.run.git.base_sha.clone().unwrap();
        let subject = crate::policies::Subject {
            action: Some(crate::policies::Action::WorktreeCreate),
            branch: Some(branch.clone()),
            repo: Some(repo.display().to_string()),
            ..Default::default()
        };
        let d = self.policy.evaluate(&subject);
        self.record_policy(None, None, &d);
        if d.decision == crate::policies::Decision::Deny {
            return Ok(Some(self.stop(RunStatus::Blocked, format!("policy denied worktree creation: {}", d.reason))?));
        }
        if self.run.dry_run {
            self.audit("dry_run", Actor::orchestrator(), None, serde_json::json!({"would": "create worktree", "path": path, "branch": branch, "base_sha": base_sha}));
            return Ok(None);
        }
        if self.cfg.config.git.manage_exclude && crate::git::ensure_excluded(&repo)? {
            self.audit("git_exclude_updated", Actor::orchestrator(), None, serde_json::json!({"entry": "/.herdr-orchestrator/"}));
        }
        let outcome = if self.task.options.continue_run.is_some() {
            crate::git::attach_worktree(&repo, &path, &branch)?
        } else {
            crate::git::add_worktree(&repo, &path, &branch, &base_sha)?
        };
        if let (Some(case), None, crate::git::WorktreeOutcome::Created) = (self.task.options.eval_case.clone(), &self.run.contract, &outcome) {
            let c = crate::eval::seed_contract(self.ctx, &case, &path)?;
            self.audit("contract_locked", Actor::orchestrator(), None, serde_json::json!({"sha256": c.sha256, "files": c.files, "check": c.check, "eval_case": case}));
            self.run.contract = Some(c);
        }
        self.run.git.head_sha = crate::git::head_sha(&path).ok();
        self.run.git.dirty = Some(false);
        self.save()?;
        self.audit(
            "worktree_created",
            Actor::orchestrator(),
            None,
            serde_json::json!({"path": path, "branch": branch, "base_sha": base_sha, "reused": outcome == crate::git::WorktreeOutcome::Existing}),
        );
        if self.run.herdr.workspace_id.is_none() {
            if let Some(h) = &self.factory.herdr {
                let label = format!("{} {}", self.run.display_name(), crate::git::slugify(&self.task.title, 30));
                match h.open_worktree_workspace(&repo, &path, &label) {
                    Ok(ws) => {
                        self.run.herdr.workspace_id = Some(ws.workspace_id.clone());
                        self.save()?;
                        self.audit("herdr_workspace_opened", Actor::orchestrator(), None, serde_json::json!({"workspace_id": ws.workspace_id, "root_pane": ws.root_pane_id}));
                    }
                    Err(e) if self.factory.herdr_required => bail!("could not open Herdr workspace: {e}"),
                    Err(e) => tracing::warn!("could not open Herdr workspace for {}: {e}", self.run.run_id),
                }
            }
        }
        Ok(None)
    }

    pub(super) fn record_policy(&mut self, step: Option<&str>, exec: Option<&str>, d: &crate::policies::PolicyDecision) {
        self.run.policy_decisions.push(PolicyDecisionRecord {
            at: now(),
            step_id: step.map(String::from),
            exec_id: exec.map(String::from),
            subject: d.subject.clone(),
            decision: d.clone(),
        });
        self.audit(
            "policy_evaluated",
            Actor::policy(),
            step,
            serde_json::json!({
                "subject": d.subject,
                "decision": d.decision.as_str(),
                "reason": d.reason,
                "rules": d.matched.iter().map(|m| &m.rule_id).collect::<Vec<_>>(),
            }),
        );
    }

    fn complete(&mut self) -> Result<RunStatus> {
        if let Some(w) = self.run.git.worktree_path.clone() {
            if !self.run.dry_run {
                if let (Some(base), true) = (self.run.git.base_sha.clone(), w.exists()) {
                    self.run.diff_stat = crate::git::changed_files(&w, &base).ok();
                    self.run.git.head_sha = crate::git::head_sha(&w).ok();
                    self.run.git.dirty = crate::git::is_dirty(&w).ok();
                }
            }
        }
        let usage = self.run.usage_total();
        self.set_status(RunStatus::Succeeded, None)?;
        self.audit(
            "run_completed",
            Actor::orchestrator(),
            None,
            serde_json::json!({
                "head_sha": self.run.git.head_sha,
                "branch": self.run.git.branch,
                "pr_url": self.run.pr_url,
                "files_changed": self.run.diff_stat.as_ref().map(|d| d.files_changed),
                "usage": usage,
            }),
        );
        self.notify(&format!("✓ {} done", self.run.display_name()), &self.task.title, false);
        if self.cfg.config.herdr.close_panes_on_success {
            self.close_agent_panes();
        }
        if self.cfg.config.git.cleanup_on_success && self.run.pr_url.is_none() && self.run.variant_count == 1 {
            // Only clean worktrees whose work is committed; the branch stays.
            if let Some(w) = &self.run.git.worktree_path {
                match crate::git::remove_worktree_if_clean(&self.run.repo_root, w) {
                    Ok(()) => self.audit("worktree_removed", Actor::orchestrator(), None, serde_json::json!({"path": w})),
                    Err(e) => tracing::info!("keeping worktree: {e}"),
                }
            }
        }
        Ok(RunStatus::Succeeded)
    }

    pub(super) fn close_agent_panes(&self) {
        if let Some(h) = &self.factory.herdr {
            super::handoff::close_run_panes(self.ctx, h.as_ref(), &self.run, Actor::orchestrator());
        }
    }

    pub(super) fn stop(&mut self, status: RunStatus, reason: String) -> Result<RunStatus> {
        self.set_status(status, Some(reason.clone()))?;
        let ev = match status {
            RunStatus::Blocked => "run_blocked",
            RunStatus::NeedsHuman => "run_needs_human",
            _ => "run_stopped",
        };
        self.audit(ev, Actor::orchestrator(), None, serde_json::json!({"reason": reason}));
        self.notify(&format!("⚠ {} {}", self.run.display_name(), status.as_str().replace('_', " ")), &reason, true);
        Ok(status)
    }

    fn fail_run(&mut self, reason: &str) -> Result<RunStatus> {
        self.set_status(RunStatus::Failed, Some(reason.to_string()))?;
        self.audit("run_failed", Actor::orchestrator(), None, serde_json::json!({"reason": reason}));
        self.notify(&format!("✗ {} failed", self.run.display_name()), reason, true);
        Ok(RunStatus::Failed)
    }

    /// Fail from an internal error (best effort).
    pub fn fail(&mut self, reason: &str) -> Result<()> {
        if self.run.status.is_terminal() {
            return Ok(());
        }
        // Close any open step executions.
        for e in self.run.steps.iter_mut() {
            if !e.status.is_terminal() {
                e.status = StepStatus::Failed;
                e.ended_at = Some(now());
                e.error.get_or_insert_with(|| reason.to_string());
            }
        }
        if self.run.status.can_transition_to(RunStatus::Failed) {
            self.fail_run(reason)?;
        }
        Ok(())
    }

    fn cancelled(&mut self, reason: &str) -> Result<RunStatus> {
        for e in self.run.steps.iter_mut() {
            if !e.status.is_terminal() {
                e.status = StepStatus::Cancelled;
                e.ended_at = Some(now());
            }
        }
        // Cancel any approval we were waiting on.
        for id in self.run.approvals.clone() {
            let _ = self.ctx.store.update_approval(&id, |a| {
                if a.status == crate::approvals::ApprovalStatus::Pending {
                    a.status = crate::approvals::ApprovalStatus::Cancelled;
                    a.decided_at = Some(now());
                }
                Ok(())
            });
        }
        let reason = self
            .ctx
            .store
            .load_control(&self.run.run_id)
            .ok()
            .and_then(|c| c.cancel_reason)
            .unwrap_or_else(|| reason.to_string());
        self.set_status(RunStatus::Cancelled, Some(reason.clone()))?;
        self.audit("run_cancelled", Actor::orchestrator(), None, serde_json::json!({"reason": reason}));
        Ok(RunStatus::Cancelled)
    }
}
