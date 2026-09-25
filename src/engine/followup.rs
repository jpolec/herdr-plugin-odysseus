//! After the handoff: PR review comments and failing CI become a follow-up
//! task on the same branch. Never started without a human: `run followup`
//! (or `F` in the pane) creates it; the optional watcher only notifies.

use anyhow::{bail, Context, Result};

use super::{create_task, EngineCtx, NewTask};
use crate::audit::{Actor, EventDraft};
use crate::model::*;

/// Create a follow-up task for a finished run's PR from its current review
/// comments and failing checks (plus optional extra instructions).
pub fn pr_followup(ctx: &EngineCtx, run_ref: &str, extra: Option<String>, runner: Option<String>, via: &str) -> Result<Task> {
    let run = ctx.store.resolve_run(run_ref)?;
    if !run.status.is_terminal() {
        bail!("run {} is still {}", run.display_name(), run.status.as_str());
    }
    let url = run.pr_url.clone().context("this run has no pull request")?;
    let task = ctx.store.load_task(&run.task_id)?;
    // One follow-up at a time: they share the branch and the worktree.
    if let Some(open) = ctx.store.list_tasks()?.into_iter().find(|t| !t.status.is_terminal() && t.options.continue_run.as_deref() == Some(run.run_id.as_str())) {
        bail!("follow-up #{} of {} is still {}", open.task_id, run.display_name(), open.status.as_str());
    }
    let fb = ctx.gh.pr_feedback(&run.repo_root, &url)?;
    if fb.state.eq_ignore_ascii_case("merged") || fb.state.eq_ignore_ascii_case("closed") {
        bail!("PR {url} is {}", fb.state.to_ascii_lowercase());
    }
    if !fb.actionable() && extra.as_deref().is_none_or(|e| e.trim().is_empty()) {
        bail!("PR {url} has no failing checks or review comments{}", if fb.pending_checks > 0 { format!(" ({} check(s) still running)", fb.pending_checks) } else { String::new() });
    }
    let mut text = format!(
        "Address the review feedback and CI failures on pull request {url}.\n\n\
         The original task was:\n{}\n\n\
         Change only what the feedback asks for. Where you disagree with a comment, leave the code \
         and explain why in your summary.\n",
        task.description.trim()
    );
    if !fb.text.is_empty() {
        text.push_str(&format!("\nFeedback on the PR (from GitHub):\n{}", fb.text));
    }
    if let Some(e) = extra.filter(|e| !e.trim().is_empty()) {
        text.push_str(&format!("\nAdditional instructions:\n{}\n", e.trim()));
    }
    let t = create_task(
        ctx,
        NewTask {
            text,
            title: Some(format!("follow-up {}: {}", run.display_name(), task.title)),
            repo: run.repo_root.clone(),
            // Same agents as the original task unless told otherwise.
            options: TaskOptions {
                workflow: Some(run.workflow_name.clone()),
                continue_run: Some(run.run_id.clone()),
                scope: task.options.scope.clone(),
                runner: runner.or_else(|| run.runner_override.clone()),
                step_runners: run.step_runners.clone(),
                ..Default::default()
            },
            via: via.into(),
            source: Some(TaskSource::PrFeedback { run_id: run.run_id.clone(), pr_url: url.clone() }),
            epic: task.epic.clone(),
            acceptance: task.acceptance.clone(),
            manual_checks: task.manual_checks.clone(),
            ..Default::default()
        },
    )?;
    let mut r = ctx.store.load_run(&run.run_id)?;
    r.pr_feedback_seen = Some(fb.fingerprint());
    ctx.store.save_run(&r)?;
    ctx.audit(
        EventDraft::new("pr_followup_created", Actor::human(std::env::var("USER").ok()))
            .run(&run.run_id, &run.task_id)
            .data(serde_json::json!({"task": t.task_id, "pr": url, "failing_checks": fb.failing_checks, "comments": fb.comments})),
    );
    Ok(t)
}

/// Notify (only) about new feedback on open PRs of runs finished within the
/// last `days`. Returns the runs that were reported.
pub fn watch_prs(ctx: &EngineCtx, days: i64) -> Result<Vec<String>> {
    if !ctx.gh.available() {
        return Ok(vec![]);
    }
    let since = now() - chrono::Duration::days(days);
    let mut reported = vec![];
    for run in ctx.store.list_runs()? {
        if run.status != RunStatus::Succeeded || run.pr_url.is_none() || run.completed_at.is_none_or(|t| t < since) {
            continue;
        }
        // A newer follow-up of this run is the one to watch.
        let superseded = ctx.store.list_tasks()?.iter().any(|t| matches!(&t.source, Some(TaskSource::PrFeedback { run_id, .. }) if *run_id == run.run_id) && !t.status.is_terminal());
        if superseded {
            continue;
        }
        let url = run.pr_url.clone().unwrap();
        let Ok(fb) = ctx.gh.pr_feedback(&run.repo_root, &url) else { continue };
        if !fb.state.eq_ignore_ascii_case("open") || !fb.actionable() {
            continue;
        }
        let fp = fb.fingerprint();
        if run.pr_feedback_seen.as_deref() == Some(fp.as_str()) {
            continue;
        }
        let mut r = ctx.store.load_run(&run.run_id)?;
        r.pr_feedback_seen = Some(fp);
        ctx.store.save_run(&r)?;
        let what = match (fb.failing_checks.len(), fb.comments) {
            (0, c) => format!("{c} review comment(s)"),
            (f, 0) => format!("{f} failing check(s)"),
            (f, c) => format!("{f} failing check(s), {c} comment(s)"),
        };
        ctx.audit(EventDraft::new("pr_feedback_detected", Actor::orchestrator()).run(&run.run_id, &run.task_id).data(serde_json::json!({"pr": url, "failing_checks": fb.failing_checks, "comments": fb.comments})));
        ctx.notify(true, &format!("{} PR feedback", run.display_name()), &format!("{what} — follow up with `herdr-orchestrator run followup {}` or [F] in the pane", run.display_name()), true);
        reported.push(run.run_id);
    }
    Ok(reported)
}
