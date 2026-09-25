//! Institutional memory: recurring review findings, unmet acceptance
//! criteria, PR review comments and guardrail hits are compiled into a task
//! for an agent to propose changes to the project's own agent instructions
//! (`AGENTS.md`, `CLAUDE.md`, `.ai/skills/`). Those files are guarded by the
//! default policy, so nothing changes without a human approving the diff.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::{create_task, EngineCtx, NewTask};
use crate::model::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LearnState {
    pub last_learn: Option<Timestamp>,
    pub last_reminder: Option<Timestamp>,
}

fn state_path(ctx: &EngineCtx) -> std::path::PathBuf {
    ctx.store.layout.root.join("state/learn.json")
}

pub fn load_state(ctx: &EngineCtx) -> LearnState {
    let p = state_path(ctx);
    p.exists().then(|| crate::store::read_doc(&ctx.store.layout, &p, "learn").ok()).flatten().unwrap_or_default()
}

fn save_state(ctx: &EngineCtx, s: &LearnState) -> Result<()> {
    crate::store::write_doc(&state_path(ctx), "learn", s)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Finding {
    pub source: String,
    pub run: String,
    pub file: Option<String>,
    pub text: String,
}

/// Lessons since `since` (all history when `None`) for one repository.
pub fn collect(ctx: &EngineCtx, repo: &std::path::Path, since: Option<Timestamp>) -> Result<Vec<Finding>> {
    let mut out = vec![];
    let tasks = ctx.store.list_tasks()?;
    for r in ctx.store.list_runs()?.into_iter().filter(|r| r.repo_root == repo && since.is_none_or(|s| r.updated_at >= s)) {
        let name = r.display_name();
        for e in &r.steps {
            let Some(v) = &e.structured else { continue };
            for f in v["findings"].as_array().into_iter().flatten() {
                if matches!(f["severity"].as_str(), Some("info")) {
                    continue;
                }
                out.push(Finding {
                    source: format!("review ({})", f["severity"].as_str().unwrap_or("?")),
                    run: name.clone(),
                    file: f["file"].as_str().map(String::from),
                    text: format!("{}{}", f["description"].as_str().unwrap_or(""), f["recommendation"].as_str().map(|x| format!(" — fix: {x}")).unwrap_or_default()),
                });
            }
            for c in v["criteria"].as_array().into_iter().flatten().filter(|c| c["status"] == "not_met") {
                out.push(Finding { source: "acceptance not met".into(), run: name.clone(), file: None, text: c["evidence"].as_str().unwrap_or("").to_string() });
            }
        }
        for d in r.policy_decisions.iter().filter(|d| d.decision.decision != crate::policies::Decision::Allow) {
            for m in d.decision.matched.iter().filter(|m| m.decision != crate::policies::Decision::Allow) {
                if ["approve-disabled-tests", "approve-test-deletion", "guard-test-only-retry", "task-scope", "contract-lock", "deny-secret-files"].contains(&m.rule_id.as_str()) {
                    out.push(Finding { source: format!("guardrail {}", m.rule_id), run: name.clone(), file: None, text: d.subject.clone() });
                }
            }
        }
    }
    for t in tasks.iter().filter(|t| t.repo_root == repo && since.is_none_or(|s| t.created_at >= s)) {
        if let Some(TaskSource::PrFeedback { pr_url, .. }) = &t.source {
            if let Some(fb) = t.description.split("Feedback on the PR (from GitHub):\n").nth(1) {
                for l in fb.lines().filter(|l| l.starts_with("- ") && !l.contains("CI check")) {
                    out.push(Finding { source: format!("PR review {pr_url}"), run: format!("#{}", t.task_id), file: None, text: l.trim_start_matches("- ").chars().take(400).collect() });
                }
            }
        }
    }
    Ok(out)
}

/// Queue the learning task. Returns `None` when there is nothing to learn.
pub fn start(ctx: &EngineCtx, repo: &std::path::Path, all: bool, runner: Option<String>) -> Result<Option<(Task, usize)>> {
    let st = load_state(ctx);
    let findings = collect(ctx, repo, if all { None } else { st.last_learn })?;
    if findings.is_empty() {
        return Ok(None);
    }
    if findings.len() < 2 {
        bail!("only one finding so far; nothing recurs yet");
    }
    let mut by_file: BTreeMap<String, usize> = BTreeMap::new();
    for f in &findings {
        let dir = f.file.as_deref().map(|p| p.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| ".".into())).unwrap_or_else(|| "(no file)".into());
        *by_file.entry(dir).or_default() += 1;
    }
    let mut text = String::from(
        "Improve this project's instructions for coding agents from what went wrong recently.\n\n\
         Below are review findings, unmet acceptance criteria, PR review comments and guardrail hits from \
         recent agent runs in this repository. Find the patterns that RECUR (at least twice, or once if \
         severe) and turn each into a short, concrete rule for agents: what to do, where, and why.\n\n\
         Put the rules where agents read them: extend `AGENTS.md` (create it if missing) and, if the \
         project has one, `CLAUDE.md`; for rules about one kind of work you may add or edit a skill in \
         `.ai/skills/`. Keep existing content; add a section \"Lessons from agent runs\" or merge into an \
         existing one. Do not add rules for one-off mistakes, do not restate generic best practice, do not \
         touch any other file. A human reviews this change before it takes effect.\n\n",
    );
    text.push_str("Where findings cluster:\n");
    for (d, n) in by_file.iter().filter(|(_, n)| **n > 1) {
        text.push_str(&format!("- {d}: {n}\n"));
    }
    text.push_str("\nFindings:\n");
    for f in findings.iter().take(200) {
        text.push_str(&format!("- [{}] {}{}: {}\n", f.source, f.run, f.file.as_ref().map(|x| format!(" {x}")).unwrap_or_default(), f.text.replace('\n', " ")));
    }
    if findings.len() > 200 {
        text.push_str(&format!("… and {} more\n", findings.len() - 200));
    }
    let n = findings.len();
    let t = create_task(
        ctx,
        NewTask {
            text,
            title: Some(format!("Lessons from {n} findings → agent instructions")),
            repo: repo.to_path_buf(),
            options: TaskOptions {
                workflow: Some("quick-task".into()),
                runner,
                scope: vec!["AGENTS.md".into(), "**/AGENTS.md".into(), "CLAUDE.md".into(), ".ai/skills/**".into()],
                ..Default::default()
            },
            via: "learn".into(),
            ..Default::default()
        },
    )?;
    save_state(ctx, &LearnState { last_learn: Some(now()), last_reminder: None })?;
    ctx.audit(crate::audit::EventDraft::new("learn_started", crate::audit::Actor::human(std::env::var("USER").ok())).task(&t.task_id).data(serde_json::json!({"findings": n})));
    Ok(Some((t, n)))
}

/// Remind (once per day at most) when enough new findings piled up.
pub fn remind(ctx: &EngineCtx, threshold: usize) -> Result<()> {
    if threshold == 0 {
        return Ok(());
    }
    let mut st = load_state(ctx);
    if st.last_reminder.is_some_and(|t| now() - t < chrono::Duration::hours(24)) {
        return Ok(());
    }
    let repos: std::collections::BTreeSet<std::path::PathBuf> = ctx.store.list_runs()?.into_iter().map(|r| r.repo_root).collect();
    let n: usize = repos.iter().map(|r| collect(ctx, r, st.last_learn).map(|f| f.len()).unwrap_or(0)).sum();
    if n >= threshold {
        st.last_reminder = Some(now());
        save_state(ctx, &st)?;
        ctx.notify(true, "herdr-orchestrator: lessons to learn", &format!("{n} review findings and guardrail hits since the last `learn` — `herdr-orchestrator learn` proposes updated agent instructions"), false);
    }
    Ok(())
}
