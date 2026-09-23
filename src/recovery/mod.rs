//! Startup reconciliation of runs that were active when the engine stopped.
//!
//! Each interrupted run is classified as:
//! - `recoverable`: provably safe to continue (reattach to a live agent,
//!   re-wait on a pending approval, re-run a read-only check, or redo an
//!   idempotent git/PR operation guarded by an existence check);
//! - `needs_human`: a side effect may or may not have happened;
//! - `completed_externally`: the side effect is visibly done (e.g. the PR
//!   exists) — recorded, then the run continues;
//! - `stale`: the run document says active but nothing is left to do;
//! - `failed`: the run cannot continue (e.g. its worktree is gone).
//!
//! Nothing is re-executed blindly.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::audit::{Actor, EventDraft};
use crate::engine::{refresh_task_status, runner_factory, EngineCtx};
use crate::model::*;
use crate::runners::RecoveryAssessment;
use crate::store::FileLock;
use crate::workflow::{StepSpec, Workflow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Recoverable,
    NeedsHuman,
    Stale,
    CompletedExternally,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecoveryReport {
    pub run_id: String,
    pub display: String,
    pub previous_status: String,
    pub classification: Classification,
    pub reason: String,
}

fn classify(ctx: &EngineCtx, run: &mut Run) -> Result<(Classification, String)> {
    if let Some(w) = &run.git.worktree_path {
        if !run.dry_run && run.status != RunStatus::Preparing && !w.exists() {
            return Ok((Classification::Failed, format!("worktree {} no longer exists", w.display())));
        }
    }
    let wf = Workflow::parse(&run.workflow_yaml)?;
    let Some(open) = run.steps.iter().rev().find(|e| !e.status.is_terminal()).cloned() else {
        if run.status == RunStatus::Preparing {
            return Ok((Classification::Recoverable, "worktree preparation is idempotent".into()));
        }
        if run.cursor >= wf.steps.len() {
            return Ok((Classification::Stale, "all steps finished; completing".into()));
        }
        return Ok((Classification::Recoverable, "no step was in flight; resuming at the next step".into()));
    };
    let step = wf.steps.iter().find(|s| s.id == open.step_id);
    let Some(step) = step else {
        return Ok((Classification::NeedsHuman, format!("in-flight step {} is not in the workflow snapshot", open.step_id)));
    };
    // A pending approval can always be re-awaited.
    if let Some(aid) = &open.approval_id {
        if let Ok(a) = ctx.store.load_approval(aid) {
            if a.status == crate::approvals::ApprovalStatus::Pending || open.status == StepStatus::AwaitingApproval {
                return Ok((Classification::Recoverable, format!("re-waiting on approval {aid}")));
            }
        }
    }
    match &step.spec {
        StepSpec::Agent { .. } => {
            let Some(binding) = &open.agent else {
                return Ok((Classification::Recoverable, "agent had not been launched; launching now".into()));
            };
            let cfg = ctx.load_config(Some(&run.repo_root))?;
            let factory = runner_factory(ctx, &cfg);
            let name = open.runner.clone().unwrap_or_default();
            let assessment = match factory.build(&name) {
                Ok((r, _)) => r.recover(binding),
                Err(e) => RecoveryAssessment::Unknown(format!("{e:#}")),
            };
            match (assessment, binding.prompt_sent) {
                (RecoveryAssessment::Alive(s), true) => Ok((Classification::Recoverable, format!("agent {} is alive ({s}); reattaching without resending", binding.agent_name.clone().unwrap_or_default()))),
                (RecoveryAssessment::Alive(s), false) => Ok((Classification::Recoverable, format!("agent alive ({s}) and never prompted; sending the prompt"))),
                (RecoveryAssessment::Gone, false) => Ok((Classification::Recoverable, "agent never received its prompt; relaunching".into())),
                (RecoveryAssessment::Gone, true) => Ok((
                    Classification::NeedsHuman,
                    format!("agent for `{}` was prompted and is gone; its partial work is in the worktree. Inspect, then `run retry`.", open.step_id),
                )),
                (RecoveryAssessment::Unknown(e), _) => Ok((Classification::NeedsHuman, format!("cannot reach the agent to verify its state: {e}"))),
            }
        }
        StepSpec::Check { .. } => {
            kill_orphan(&open);
            Ok((Classification::Recoverable, "read-only check interrupted; re-running it".into()))
        }
        StepSpec::Command { .. } => {
            let alive = open.process.as_ref().is_some_and(|p| crate::process::pid_alive(p.pid));
            if open.status == StepStatus::Starting && open.process.is_none() {
                return Ok((Classification::Recoverable, "command was never spawned; running it".into()));
            }
            kill_orphan(&open);
            Ok((
                Classification::NeedsHuman,
                format!(
                    "command step `{}` was interrupted{}; it may have had side effects. Use `run retry` to re-run or `run retry --from-step <next>` to skip.",
                    open.step_id,
                    if alive { " (orphaned process terminated)" } else { "" }
                ),
            ))
        }
        StepSpec::GithubPr { .. } => {
            if let (Some(w), Some(b)) = (&run.git.worktree_path, &run.git.branch) {
                if ctx.gh.available() {
                    match ctx.gh.pr_for_branch(w, b) {
                        Ok(Some(pr)) => {
                            run.pr_url = Some(pr.url.clone());
                            return Ok((Classification::CompletedExternally, format!("PR already exists: {}", pr.url)));
                        }
                        Ok(None) => return Ok((Classification::Recoverable, "no PR exists yet; the step re-checks before creating".into())),
                        Err(e) => return Ok((Classification::NeedsHuman, format!("cannot verify PR state: {e:#}"))),
                    }
                }
            }
            Ok((Classification::NeedsHuman, "cannot verify whether the PR was created".into()))
        }
        // Commits/pushes are idempotent (no force, nothing-to-commit is a no-op).
        StepSpec::Git { .. } | StepSpec::Policy {} | StepSpec::Approval { .. } => Ok((Classification::Recoverable, "idempotent step; re-running".into())),
        StepSpec::Parallel { .. } => Ok((Classification::Failed, "unsupported step".into())),
    }
}

fn kill_orphan(e: &StepExecution) {
    if let Some(p) = &e.process {
        if crate::process::pid_alive(p.pid) {
            tracing::warn!("terminating orphaned process group {} ({:?})", p.pid, p.argv);
            #[cfg(unix)]
            crate::process::kill_group(p.pid, libc::SIGTERM);
        }
    }
}

/// Reconcile all runs left active. Runs currently driven by another live
/// process (per-run lock held) are skipped.
pub fn recover_all(ctx: &Arc<EngineCtx>) -> Result<Vec<RecoveryReport>> {
    let mut out = vec![];
    let candidates: Vec<Run> = ctx
        .store
        .list_runs()?
        .into_iter()
        .filter(|r| matches!(r.status, RunStatus::Preparing | RunStatus::Running | RunStatus::AwaitingApproval))
        .collect();
    if candidates.is_empty() {
        return Ok(out);
    }
    for mut run in candidates {
        let lock_path = ctx.store.layout.lock_path(&format!("run-{}", run.run_id));
        let Some(_lock) = FileLock::try_acquire(&lock_path)? else {
            continue; // someone is driving it right now
        };
        ctx.audit(EventDraft::new("recovery_started", Actor::orchestrator()).run(&run.run_id, &run.task_id).data(serde_json::json!({"status": run.status.as_str()})));
        let prev = run.status;
        let (class, reason) = match classify(ctx, &mut run) {
            Ok(x) => x,
            Err(e) => (Classification::NeedsHuman, format!("recovery check failed: {e:#}")),
        };
        match class {
            Classification::Recoverable | Classification::Stale => {
                run.status = RunStatus::Pending;
                run.recovered = true;
                run.status_reason = Some(format!("recovered: {reason}"));
            }
            Classification::CompletedExternally => {
                // Close the in-flight step as done and move on.
                if let Some(e) = run.steps.iter_mut().rev().find(|e| !e.status.is_terminal()) {
                    e.status = StepStatus::Succeeded;
                    e.ended_at = Some(now());
                    e.error = Some(format!("completed externally: {reason}"));
                }
                run.cursor += 1;
                run.status = RunStatus::Pending;
                run.recovered = true;
                run.status_reason = Some(reason.clone());
            }
            Classification::NeedsHuman => {
                run.status = RunStatus::NeedsHuman;
                run.status_reason = Some(reason.clone());
            }
            Classification::Failed => {
                for e in run.steps.iter_mut().filter(|e| !e.status.is_terminal()) {
                    e.status = StepStatus::Failed;
                    e.ended_at = Some(now());
                }
                run.status = RunStatus::Failed;
                run.completed_at = Some(now());
                run.status_reason = Some(reason.clone());
            }
        }
        run.updated_at = now();
        ctx.store.save_run(&run)?;
        ctx.audit(
            EventDraft::new("recovery_completed", Actor::orchestrator())
                .run(&run.run_id, &run.task_id)
                .data(serde_json::json!({"classification": class, "reason": reason, "previous_status": prev.as_str(), "new_status": run.status.as_str()})),
        );
        let _ = refresh_task_status(ctx, &run.task_id);
        if class == Classification::NeedsHuman {
            ctx.notify(true, &format!("{} needs you after restart", run.display_name()), &reason, true);
        }
        out.push(RecoveryReport { run_id: run.run_id.clone(), display: run.display_name(), previous_status: prev.as_str().into(), classification: class, reason });
    }
    Ok(out)
}
