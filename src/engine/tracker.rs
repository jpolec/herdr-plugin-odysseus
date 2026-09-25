//! GitHub as the team's view of the work: an epic becomes a milestone, its
//! tasks become issues (with the acceptance checklist), status changes are
//! posted as comments, PRs close their issues, and issues labelled
//! `agent-ready` can be imported as tasks. Optionally, issues are added to a
//! Projects board (needs the `project` token scope). Everything goes through
//! `gh`; failures are reported, never fatal to runs.

use anyhow::Result;
use serde::Serialize;

use super::{create_task, EngineCtx, NewTask};
use crate::audit::{Actor, EventDraft};
use crate::model::*;

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct SyncReport {
    pub issues_created: Vec<String>,
    pub comments: usize,
    pub project_items: usize,
    pub errors: Vec<String>,
}

fn issue_body(task: &Task, epic: Option<&crate::epic::Epic>) -> String {
    let mut b = format!("{}\n", task.description.trim());
    if !task.acceptance.is_empty() {
        b.push_str("\n### Acceptance criteria\n\n");
        for a in &task.acceptance {
            b.push_str(&format!("- [ ] {a}\n"));
        }
    }
    if !task.manual_checks.is_empty() {
        b.push_str("\n### Manual checks\n\n");
        for m in &task.manual_checks {
            b.push_str(&format!("- [ ] {m}\n"));
        }
    }
    if let (Some(e), Some(l)) = (epic, &task.epic) {
        b.push_str(&format!("\nPart of epic {} (`{}` — {}), plan task {}.\n", e.epic_id, e.adr.path, e.adr.title, l.key));
    }
    b.push_str(&format!("\n_Tracked by herdr-orchestrator task #{}. Status updates are posted here; the PR closes this issue._\n", task.task_id));
    crate::audit::redact::redact_str(&b)
}

/// Status worth telling the team about, with the comment to post.
fn status_update(ctx: &EngineCtx, task: &Task) -> Option<(String, String)> {
    let runs: Vec<Run> = task.run_ids.iter().filter_map(|id| ctx.store.load_run(id).ok()).collect();
    let pr = runs.iter().find_map(|r| r.pr_url.clone());
    let reason = runs.iter().find_map(|r| r.status_reason.clone()).unwrap_or_default();
    let tokens = crate::telemetry::compact_tokens(&crate::telemetry::runs_agent_usage(runs.iter()));
    let (key, text) = match task.status {
        TaskStatus::Running | TaskStatus::AwaitingApproval if !runs.is_empty() => ("started".to_string(), format!("🤖 An agent started working on this (task #{}).", task.task_id)),
        TaskStatus::Succeeded => match &pr {
            Some(p) => (format!("done:{p}"), format!("✅ Done: {p} (agent tokens: {tokens}).")),
            None => ("done".into(), format!("✅ Done on branch `{}` (agent tokens: {tokens}).", runs.iter().find_map(|r| r.git.branch.clone()).unwrap_or_default())),
        },
        TaskStatus::Failed => ("failed".into(), format!("❌ The run failed: {reason}")),
        TaskStatus::Blocked => (format!("blocked:{}", task.waiting_on.clone().unwrap_or(reason.clone())), format!("⚠️ Blocked: {}", task.waiting_on.clone().unwrap_or(reason))),
        TaskStatus::Cancelled => ("cancelled".into(), "Cancelled.".into()),
        _ => return None,
    };
    Some((key, crate::audit::redact::redact_str(&text)))
}

pub fn sync(ctx: &EngineCtx, repo: &std::path::Path) -> Result<SyncReport> {
    let cfg = ctx.load_config(Some(repo))?.config.github.tracker;
    let mut rep = SyncReport::default();
    if !ctx.gh.available() {
        rep.errors.push("GitHub CLI `gh` is not installed".into());
        return Ok(rep);
    }
    let slug = ctx.gh.repo_slug(repo)?;
    let mut label_ok = false;
    let project = cfg.project.as_deref().and_then(|p| p.split_once('/')).and_then(|(o, n)| Some((o.to_string(), n.parse::<u64>().ok()?)));
    // Issues for accepted epic tasks.
    for e in ctx.store.list_epics()?.into_iter().filter(|e| e.repo_root == repo && !e.tasks.is_empty()) {
        let milestone_title = format!("{}: {}", e.epic_id, e.adr.title);
        let mut milestone: Option<String> = None;
        for tid in e.tasks.values() {
            let Ok(task) = ctx.store.load_task(tid) else { continue };
            if task.issue.is_some() {
                continue;
            }
            if milestone.is_none() {
                match ctx.gh.ensure_milestone(repo, &slug, &milestone_title, &format!("ADR {}", e.adr.path)) {
                    Ok(_) => milestone = Some(milestone_title.clone()),
                    Err(err) => rep.errors.push(format!("milestone for {}: {err:#}", e.epic_id)),
                }
            }
            if !label_ok {
                if let Err(err) = ctx.gh.ensure_label(repo, &cfg.label) {
                    rep.errors.push(format!("label {}: {err:#}", cfg.label));
                }
                label_ok = true;
            }
            match ctx.gh.issue_create(repo, &task.title, &issue_body(&task, Some(&e)), &cfg.label, milestone.as_deref()) {
                Ok(link) => {
                    if let Some((owner, n)) = &project {
                        match ctx.gh.project_add(repo, owner, *n, &link.url) {
                            Ok(()) => rep.project_items += 1,
                            Err(err) => rep.errors.push(format!("project {owner}/{n}: {err:#} (the token may need the project scope: gh auth refresh -s project)")),
                        }
                    }
                    ctx.audit(EventDraft::new("issue_created", Actor::orchestrator()).task(&task.task_id).data(serde_json::json!({"url": link.url, "epic": e.epic_id})));
                    rep.issues_created.push(link.url.clone());
                    ctx.store.update_task(&task.task_id, |t| {
                        t.issue = Some(link.clone());
                        Ok(())
                    })?;
                }
                Err(err) => rep.errors.push(format!("issue for #{}: {err:#}", task.task_id)),
            }
        }
    }
    // Status comments on every linked issue.
    for task in ctx.store.list_tasks()?.into_iter().filter(|t| t.repo_root == repo) {
        let Some(link) = task.issue.clone() else { continue };
        let Some((key, text)) = status_update(ctx, &task) else { continue };
        if task.issue_synced.as_deref() == Some(key.as_str()) {
            continue;
        }
        match ctx.gh.issue_comment(repo, link.number, &text) {
            Ok(()) => {
                rep.comments += 1;
                ctx.store.update_task(&task.task_id, |t| {
                    t.issue_synced = Some(key.clone());
                    Ok(())
                })?;
            }
            Err(err) => rep.errors.push(format!("comment on #{}: {err:#}", link.number)),
        }
    }
    Ok(rep)
}

/// Issues labelled for agents that are not tasks yet.
pub fn importable(ctx: &EngineCtx, repo: &std::path::Path, label: &str) -> Result<Vec<crate::github::Issue>> {
    let known: Vec<u64> = ctx.store.list_tasks()?.iter().filter(|t| t.repo_root == repo).filter_map(|t| t.issue.as_ref().map(|i| i.number)).collect();
    Ok(ctx.gh.issue_list(repo, label)?.into_iter().filter(|i| !known.contains(&i.number)).collect())
}

pub fn import(ctx: &EngineCtx, repo: &std::path::Path, issues: &[crate::github::Issue], options: TaskOptions) -> Result<Vec<Task>> {
    let slug = ctx.gh.repo_slug(repo).unwrap_or_default();
    let mut out = vec![];
    for i in issues {
        let t = create_task(
            ctx,
            NewTask {
                // Issue text is untrusted input; it only ever reaches prompts.
                text: format!("GitHub issue #{}: {}\n\n{}\n\n{}", i.number, i.title, i.body.trim(), i.url),
                title: Some(i.title.clone()),
                repo: repo.to_path_buf(),
                options: options.clone(),
                via: "tracker".into(),
                source: Some(TaskSource::GithubIssue { repo: slug.clone(), number: i.number, url: i.url.clone() }),
                ..Default::default()
            },
        )?;
        out.push(t);
    }
    Ok(out)
}
