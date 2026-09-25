//! Parallel work without stepping on each other.
//!
//! - **Conflict-aware scheduling**: a queued task that declares a `scope`
//!   does not start while an active run's scope or changed files overlap
//!   it; it waits with the reason.
//! - **Branch updates**: `run update` merges the moved base (e.g. `main`)
//!   into a finished run's branch — never a rebase, never a force push, so
//!   `deny-force-push` stays intact. A clean merge is verified by the tests;
//!   conflicts go to an agent that resolves them, then the tests run. Either
//!   way the result goes through the run's normal approval and push.

use anyhow::{bail, Context, Result};

use super::{create_task, EngineCtx, NewTask};
use crate::audit::{Actor, EventDraft};
use crate::model::*;

/// The literal directory prefix of a glob (`src/api/**` → `src/api/`).
fn literal_prefix(glob: &str) -> &str {
    let cut = glob.find(['*', '?', '[', '{']).unwrap_or(glob.len());
    &glob[..cut]
}

fn prefixes_overlap(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Why `task` should wait for an active run, if it should.
pub fn conflict_with_active(task: &Task, active: &[(Task, Run)]) -> Option<String> {
    if task.options.scope.is_empty() {
        return None;
    }
    let mine: Vec<&str> = task.options.scope.iter().map(|g| literal_prefix(g)).collect();
    let set = super::steps_scope_set(&task.options.scope);
    for (t, r) in active {
        if t.task_id == task.task_id {
            continue;
        }
        for g in &t.options.scope {
            let p = literal_prefix(g);
            // A glob that starts with a wildcard says nothing about where it
            // lands; only the changed-files check below can tell.
            if p.is_empty() {
                continue;
            }
            if mine.iter().any(|m| !m.is_empty() && prefixes_overlap(m, p)) {
                return Some(format!("may conflict with #{} (both change {})", t.task_id, if p.is_empty() { g.as_str() } else { p }));
            }
        }
        if let (Some(set), Some(d)) = (&set, &r.diff_stat) {
            if let Some(f) = d.files.iter().find(|f| set.is_match(&f.path)) {
                return Some(format!("may conflict with #{} (it already changed {})", t.task_id, f.path));
            }
        }
    }
    None
}

/// Merge `onto` into a finished run's branch and queue the follow-up that
/// verifies (clean) or resolves (conflicts) it. Returns the task and whether
/// the merge had conflicts.
pub fn update_branch(ctx: &EngineCtx, run_ref: &str, onto: Option<&str>, runner: Option<String>, via: &str) -> Result<(Task, Vec<String>)> {
    let run = ctx.store.resolve_run(run_ref)?;
    if !run.status.is_terminal() {
        bail!("run {} is still {}", run.display_name(), run.status.as_str());
    }
    let wt = run.git.worktree_path.clone().context("run has no worktree")?;
    let branch = run.git.branch.clone().context("run has no branch")?;
    if !wt.exists() {
        crate::git::attach_worktree(&run.repo_root, &wt, &branch)?;
    }
    if crate::git::is_dirty(&wt)? {
        bail!("worktree of {} has uncommitted changes; commit or discard them first", run.display_name());
    }
    let onto = onto.map(String::from).unwrap_or_else(|| run.git.base_ref.clone());
    let onto_sha = crate::git::rev_parse(&run.repo_root, &onto)?;
    if crate::git::is_ancestor(&wt, &onto_sha, "HEAD") {
        bail!("{} already contains {onto}", run.display_name());
    }
    let conflicts = crate::git::merge_no_ff(&wt, &onto_sha, &format!("Merge {onto} into {branch} (herdr-orchestrator)"))?;
    let task = ctx.store.load_task(&run.task_id)?;
    ctx.audit(EventDraft::new("branch_update_merged", Actor::human(std::env::var("USER").ok())).run(&run.run_id, &run.task_id).data(serde_json::json!({"onto": onto, "onto_sha": onto_sha, "conflicts": conflicts})));
    let (workflow, text) = if conflicts.is_empty() {
        ("update-verify", format!("The branch of task #{} was updated with {onto} (clean merge). Verify it still works.\n\nOriginal task:\n{}", task.task_id, task.description.trim()))
    } else {
        (
            "update-resolve",
            format!(
                "Merging {onto} into this branch ({branch}) produced conflicts in:\n{}\n\n\
                 Resolve every conflict so that both this branch's change and the incoming changes keep working: \
                 edit the files, remove all conflict markers. Do not abort the merge, do not rebase, do not reset. \
                 The orchestrator commits the merge and runs the tests.\n\nThis branch's original task:\n{}",
                conflicts.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n"),
                task.description.trim()
            ),
        )
    };
    let t = create_task(
        ctx,
        NewTask {
            text,
            title: Some(format!("update {} with {onto}", run.display_name())),
            repo: run.repo_root.clone(),
            options: TaskOptions {
                workflow: Some(workflow.into()),
                continue_run: Some(run.run_id.clone()),
                // Diff and policy against the new base: only this branch's
                // own changes, not what arrived with the merge.
                base_ref: Some(onto_sha.clone()),
                runner: runner.or_else(|| run.runner_override.clone()),
                ..Default::default()
            },
            via: via.into(),
            acceptance: task.acceptance.clone(),
            ..Default::default()
        },
    )?;
    Ok((t, conflicts))
}
