//! Local task queue and run scheduler.
//!
//! Each tick: reap finished drivers → apply durable control requests
//! (cancel, human retry) → claim queued tasks → start pending runs up to
//! `max_parallel_runs`. No lock is held while a run executes; each driver
//! owns its run document through a per-run file lock.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Result;

use super::{create_runs, refresh_task_status, EngineCtx};
use crate::audit::{Actor, EventDraft};
use crate::model::*;
use crate::process::CancelToken;
use crate::workflow::Workflow;

pub struct ActiveRun {
    pub handle: JoinHandle<()>,
    pub cancel: CancelToken,
}

pub struct Scheduler {
    pub ctx: Arc<EngineCtx>,
    pub active: HashMap<String, ActiveRun>,
    pub max_parallel_runs: usize,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct TickReport {
    pub claimed_tasks: Vec<String>,
    pub started_runs: Vec<String>,
    pub finished_runs: Vec<String>,
    pub cancelled_runs: Vec<String>,
    pub resumed_runs: Vec<String>,
}

impl Scheduler {
    pub fn new(ctx: Arc<EngineCtx>, max_parallel_runs: usize) -> Self {
        Self { ctx, active: HashMap::new(), max_parallel_runs: max_parallel_runs.max(1) }
    }

    pub fn is_idle(&self) -> Result<bool> {
        if !self.active.is_empty() {
            return Ok(false);
        }
        let queued = self.ctx.store.list_tasks()?.iter().any(|t| t.status == TaskStatus::Queued);
        let pending = self.ctx.store.list_runs()?.iter().any(|r| r.status == RunStatus::Pending);
        Ok(!queued && !pending)
    }

    pub fn tick(&mut self) -> Result<TickReport> {
        let mut rep = TickReport::default();
        // 1. reap
        let done: Vec<String> = self.active.iter().filter(|(_, a)| a.handle.is_finished()).map(|(k, _)| k.clone()).collect();
        for id in done {
            if let Some(a) = self.active.remove(&id) {
                let _ = a.handle.join();
            }
            rep.finished_runs.push(id);
        }
        // 2. control requests
        let runs = self.ctx.store.list_runs()?;
        for run in &runs {
            let ctl = self.ctx.store.load_control(&run.run_id)?;
            if ctl.cancel_requested && !run.status.is_terminal() {
                if let Some(a) = self.active.get(&run.run_id) {
                    a.cancel.cancel();
                } else if matches!(run.status, RunStatus::Pending | RunStatus::Blocked | RunStatus::NeedsHuman) {
                    self.cancel_idle_run(run, ctl.cancel_reason.clone())?;
                    rep.cancelled_runs.push(run.run_id.clone());
                }
            }
            if ctl.resume_requested && !self.active.contains_key(&run.run_id) {
                if run.status.can_resume() {
                    self.resume_run(run, ctl.resume_from_step.clone())?;
                    rep.resumed_runs.push(run.run_id.clone());
                }
                self.ctx.store.update_control(&run.run_id, |c| {
                    c.resume_requested = false;
                    c.resume_from_step = None;
                })?;
            }
        }
        if let Err(e) = crate::epic::engine::sync(&self.ctx) {
            tracing::warn!("epic sync failed: {e:#}");
        }
        if let Err(e) = super::digest::enforce_shift(&self.ctx) {
            tracing::warn!("shift check failed: {e:#}");
        }
        let paused = self.ctx.store.load_scheduler()?.paused;
        if paused {
            return Ok(rep);
        }
        // 3. claim queued tasks (FIFO) whose dependencies are done
        for task in self.ctx.store.list_tasks()? {
            let waiting = matches!(task.status, TaskStatus::Queued | TaskStatus::Blocked) && task.run_ids.is_empty() && !task.depends_on.is_empty();
            if waiting && !self.dependencies_ready(&task)? {
                continue;
            }
            if task.status == TaskStatus::Queued && task.run_ids.is_empty() {
                // Claim under a lock and re-check: a foreground CLI and the
                // daemon may both be scheduling.
                let _claim = self.ctx.store.lock("queue")?;
                let task = self.ctx.store.load_task(&task.task_id)?;
                if task.status != TaskStatus::Queued || !task.run_ids.is_empty() {
                    continue;
                }
                match create_runs(&self.ctx, &task) {
                    Ok(_) => rep.claimed_tasks.push(task.task_id.clone()),
                    Err(e) => {
                        tracing::error!("cannot start task {}: {e:#}", task.task_id);
                        let _ = self.ctx.store.update_task(&task.task_id, |t| {
                            t.status = TaskStatus::Failed;
                            Ok(())
                        });
                        self.ctx.audit(
                            EventDraft::new("task_failed", Actor::orchestrator())
                                .task(&task.task_id)
                                .data(serde_json::json!({"error": format!("{e:#}")})),
                        );
                    }
                }
            }
        }
        // 4. start pending runs
        let mut pending: Vec<Run> = self.ctx.store.list_runs()?.into_iter().filter(|r| r.status == RunStatus::Pending && !self.active.contains_key(&r.run_id)).collect();
        pending.sort_by_key(|r| (r.task_id.parse::<u64>().unwrap_or(u64::MAX), r.variant_index));
        for run in pending {
            if self.active.len() >= self.max_parallel_runs {
                break;
            }
            self.spawn(&run.run_id);
            rep.started_runs.push(run.run_id.clone());
        }
        Ok(rep)
    }

    /// Apply the dependency state of a not-yet-started task. Returns `true`
    /// when it may start now (its base may have been set for stacking).
    fn dependencies_ready(&self, task: &Task) -> Result<bool> {
        use crate::epic::engine::DepState;
        let state = match crate::epic::engine::dependency_state(&self.ctx, task) {
            Ok(s) => s,
            Err(e) => DepState::Waiting(format!("cannot check dependencies: {e:#}")),
        };
        let (status, waiting_on, base) = match &state {
            DepState::Ready(base) => (TaskStatus::Queued, None, base.clone()),
            DepState::Waiting(why) => (TaskStatus::Queued, Some(why.clone()), None),
            DepState::Blocked(why) => (TaskStatus::Blocked, Some(why.clone()), None),
        };
        if task.status != status || task.waiting_on != waiting_on || base.is_some() {
            let _ = self.ctx.store.update_task(&task.task_id, |t| {
                t.status = status;
                t.waiting_on = waiting_on.clone();
                if let Some(b) = &base {
                    t.options.base_ref = Some(b.clone());
                }
                Ok(())
            });
            if task.status != status {
                let ev = if status == TaskStatus::Blocked { "task_blocked_by_dependency" } else { "task_unblocked" };
                self.ctx.audit(EventDraft::new(ev, Actor::orchestrator()).task(&task.task_id).data(serde_json::json!({"reason": waiting_on})));
            }
        }
        Ok(matches!(state, DepState::Ready(_)))
    }

    fn spawn(&mut self, run_id: &str) {
        let ctx = self.ctx.clone();
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        let id = run_id.to_string();
        let handle = std::thread::Builder::new()
            .name(format!("run-{id}"))
            .spawn(move || {
                if let Err(e) = super::driver::drive(&ctx, &id, c2) {
                    tracing::error!("run {id}: {e:#}");
                }
            })
            .expect("spawning run thread");
        self.active.insert(run_id.to_string(), ActiveRun { handle, cancel });
    }

    fn cancel_idle_run(&self, run: &Run, reason: Option<String>) -> Result<()> {
        let mut r = self.ctx.store.load_run(&run.run_id)?;
        for e in r.steps.iter_mut() {
            if !e.status.is_terminal() {
                e.status = StepStatus::Cancelled;
                e.ended_at = Some(now());
            }
        }
        r.status = RunStatus::Cancelled;
        r.status_reason = reason.clone().or(Some("cancelled".into()));
        r.completed_at = Some(now());
        r.updated_at = now();
        self.ctx.store.save_run(&r)?;
        for id in &r.approvals {
            let _ = self.ctx.store.update_approval(id, |a| {
                if a.status == crate::approvals::ApprovalStatus::Pending {
                    a.status = crate::approvals::ApprovalStatus::Cancelled;
                }
                Ok(())
            });
        }
        self.ctx.audit(EventDraft::new("run_cancelled", Actor::orchestrator()).run(&r.run_id, &r.task_id).data(serde_json::json!({"reason": reason})));
        refresh_task_status(&self.ctx, &r.task_id)?;
        Ok(())
    }

    /// Human retry: back to `pending` at the failed (or requested) step.
    fn resume_run(&self, run: &Run, from_step: Option<String>) -> Result<()> {
        let mut r = self.ctx.store.load_run(&run.run_id)?;
        let wf = Workflow::parse(&r.workflow_yaml)?;
        if let Some(s) = &from_step {
            if let Some(i) = wf.step_index(s) {
                r.cursor = i;
            }
        }
        for e in r.steps.iter_mut() {
            if !e.status.is_terminal() {
                e.status = StepStatus::Cancelled;
                e.ended_at = Some(now());
                e.error.get_or_insert_with(|| "superseded by human retry".into());
            }
        }
        let later: Vec<String> = wf.steps[r.cursor.min(wf.steps.len())..].iter().map(|s| s.id.clone()).collect();
        r.retry_counts.retain(|k, _| !later.contains(k));
        r.status = RunStatus::Pending;
        r.status_reason = Some("retry requested".into());
        r.completed_at = None;
        r.recovered = false;
        r.updated_at = now();
        self.ctx.store.save_run(&r)?;
        self.ctx.store.update_control(&r.run_id, |c| {
            c.cancel_requested = false;
            c.cancel_reason = None;
        })?;
        self.ctx.audit(
            EventDraft::new("retry_started", Actor::human(None))
                .run(&r.run_id, &r.task_id)
                .data(serde_json::json!({"from_step": wf.steps.get(r.cursor).map(|s| s.id.clone()), "human": true})),
        );
        refresh_task_status(&self.ctx, &r.task_id)?;
        Ok(())
    }

    /// Wait for all active runs (tests / foreground mode).
    pub fn join_all(&mut self) {
        for (_, a) in self.active.drain() {
            let _ = a.handle.join();
        }
    }

    pub fn cancel_all(&self) {
        for a in self.active.values() {
            a.cancel.cancel();
        }
    }
}
