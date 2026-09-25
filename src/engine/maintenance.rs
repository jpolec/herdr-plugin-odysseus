//! Housekeeping and local statistics: worktree garbage collection and
//! per-runner outcomes. Local data only.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use super::EngineCtx;
use crate::audit::{Actor, EventDraft};
use crate::model::*;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GcCandidate {
    pub run_id: String,
    pub run: String,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct GcOptions {
    /// Ask GitHub (`gh`) whether the PR was merged or closed.
    pub check_prs: bool,
    /// Also failed and cancelled runs finished more than `older_than_days` ago.
    pub include_failed: bool,
    pub older_than_days: i64,
}

/// Worktrees of finished runs that are safe to remove: clean, and merged,
/// closed, superseded by the selected variant, or (opt-in) old failures.
/// Branches are never deleted.
pub fn gc_candidates(ctx: &EngineCtx, o: &GcOptions) -> Result<Vec<GcCandidate>> {
    let tasks: BTreeMap<String, Task> = ctx.store.list_tasks()?.into_iter().map(|t| (t.task_id.clone(), t)).collect();
    let continued: Vec<String> = tasks.values().filter(|t| !t.status.is_terminal()).filter_map(|t| t.options.continue_run.clone()).collect();
    let mut out = vec![];
    for r in ctx.store.list_runs()? {
        let Some(w) = r.git.worktree_path.clone() else { continue };
        if !r.status.is_terminal() || !w.exists() || continued.contains(&r.run_id) {
            continue;
        }
        if crate::git::is_dirty(&w).unwrap_or(true) {
            continue;
        }
        let task = tasks.get(&r.task_id);
        let head = crate::git::head_sha(&w).ok();
        let base = crate::git::default_base(&r.repo_root, None).ok();
        let mut reason = None;
        if r.status == RunStatus::Succeeded {
            if let (Some(h), Some(b)) = (&head, &base) {
                if r.git.base_sha.as_deref() != Some(h.as_str()) && crate::git::is_ancestor(&r.repo_root, h, b) {
                    reason = Some(format!("merged into {b}"));
                }
            }
            if reason.is_none() && r.variant_count > 1 {
                if let Some(sel) = task.and_then(|t| t.selected_run.clone()) {
                    if sel != r.run_id {
                        reason = Some("another variant was selected".into());
                    }
                }
            }
            if reason.is_none() && o.check_prs {
                if let Some(url) = &r.pr_url {
                    if let Ok(fb) = ctx.gh.pr_feedback(&r.repo_root, url) {
                        if matches!(fb.state.to_ascii_uppercase().as_str(), "MERGED" | "CLOSED") {
                            reason = Some(format!("PR {}", fb.state.to_ascii_lowercase()));
                        }
                    }
                }
            }
        } else if o.include_failed && matches!(r.status, RunStatus::Failed | RunStatus::Cancelled) {
            let old = r.completed_at.is_some_and(|t| t < now() - chrono::Duration::days(o.older_than_days));
            if old {
                reason = Some(format!("{} more than {} day(s) ago", r.status.as_str(), o.older_than_days));
            }
        }
        if let Some(reason) = reason {
            out.push(GcCandidate { run_id: r.run_id.clone(), run: r.display_name(), worktree: w, branch: r.git.branch.clone(), reason });
        }
    }
    Ok(out)
}

/// Remove the candidates' worktrees (only if still clean). Returns errors
/// per run instead of stopping at the first.
pub fn gc_remove(ctx: &EngineCtx, list: &[GcCandidate], user: Option<String>) -> Vec<(String, Result<()>)> {
    let mut out = vec![];
    for c in list {
        let res = (|| -> Result<()> {
            let r = ctx.store.load_run(&c.run_id)?;
            crate::git::remove_worktree_if_clean(&r.repo_root, &c.worktree)?;
            ctx.audit(EventDraft::new("worktree_removed", Actor::human(user.clone())).run(&r.run_id, &r.task_id).data(serde_json::json!({"path": c.worktree, "reason": c.reason, "via": "gc"})));
            Ok(())
        })();
        out.push((c.run.clone(), res));
    }
    out
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct RunnerStats {
    pub runner: String,
    pub workflow: String,
    pub runs: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub blocked: usize,
    /// Average number of attempts of the first agent step.
    pub avg_attempts: f64,
    /// Runs whose first review/acceptance verdict was `approved`.
    pub first_review_approved: usize,
    pub reviewed: usize,
    pub avg_tokens: Option<u64>,
    pub avg_minutes: Option<f64>,
}

/// Outcomes per implementing runner and workflow, from local state.
pub fn runner_stats(runs: &[Run]) -> Vec<RunnerStats> {
    let mut groups: BTreeMap<(String, String), Vec<&Run>> = BTreeMap::new();
    for r in runs.iter().filter(|r| r.status.is_terminal() || matches!(r.status, RunStatus::Blocked | RunStatus::NeedsHuman)) {
        let Some(first) = r.steps.iter().find(|e| e.kind == StepKind::Agent) else { continue };
        let runner = first.runner.clone().unwrap_or_else(|| "?".into());
        groups.entry((runner, r.workflow_name.clone())).or_default().push(r);
    }
    let mut out = vec![];
    for ((runner, workflow), rs) in groups {
        let mut s = RunnerStats { runner, workflow, runs: rs.len(), ..Default::default() };
        let mut attempts = 0u32;
        let mut tokens = vec![];
        let mut minutes = vec![];
        for r in &rs {
            match r.status {
                RunStatus::Succeeded => s.succeeded += 1,
                RunStatus::Failed | RunStatus::Cancelled => s.failed += 1,
                _ => s.blocked += 1,
            }
            let first = r.steps.iter().find(|e| e.kind == StepKind::Agent).map(|e| e.step_id.clone()).unwrap_or_default();
            attempts += r.steps.iter().filter(|e| e.step_id == first).count() as u32;
            if let Some(v) = r.steps.iter().filter_map(|e| e.structured.as_ref()).find(|v| v.get("verdict").is_some()) {
                s.reviewed += 1;
                if v["verdict"] == "approved" {
                    s.first_review_approved += 1;
                }
            }
            let u = crate::telemetry::run_agent_usage(r);
            if let (Some(i), Some(o)) = (u.input_tokens, u.output_tokens) {
                tokens.push(i + o);
            }
            if let (Some(a), Some(b)) = (r.started_at, r.completed_at) {
                minutes.push((b - a).num_seconds() as f64 / 60.0);
            }
        }
        s.avg_attempts = attempts as f64 / rs.len() as f64;
        s.avg_tokens = (!tokens.is_empty()).then(|| tokens.iter().sum::<u64>() / tokens.len() as u64);
        s.avg_minutes = (!minutes.is_empty()).then(|| minutes.iter().sum::<f64>() / minutes.len() as f64);
        out.push(s);
    }
    out
}
