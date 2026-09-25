//! Epic lifecycle on top of the ordinary task/run machinery: planning and
//! conformance reviews are tasks with built-in read-only workflows; accepted
//! plan tasks are tasks with acceptance criteria and dependencies.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{Epic, EpicStatus, Plan, PlanRules, PlanTask};
use crate::audit::{Actor, EventDraft};
use crate::config::DependencyMode;
use crate::engine::{create_task, EngineCtx, NewTask};
use crate::model::*;

pub const PLAN_WORKFLOW: &str = "epic-plan";
pub const CONFORMANCE_WORKFLOW: &str = "epic-conformance";

fn audit(ctx: &EngineCtx, event: &str, actor: Actor, e: &Epic, data: serde_json::Value) {
    let mut d = serde_json::json!({"epic_id": e.epic_id, "adr": e.adr.path});
    if let (Some(o), serde_json::Value::Object(extra)) = (d.as_object_mut(), data) {
        o.extend(extra);
    }
    ctx.audit(EventDraft::new(event, actor).data(d));
}

fn plan_prompt(adr_path: &str, adr_text: &str, max_tasks: usize, workflows: &[String], previous: Option<&Plan>, feedback: Option<&str>, accepted: &BTreeMap<String, String>) -> String {
    let mut p = format!(
        "Plan the implementation of the Architecture Decision Record below.\n\n\
         Break its Decision into small, independently reviewable tasks (at most {max_tasks}). \
         Each task must be something one agent can finish and a reviewer can verify in one pull request.\n\
         For every task give acceptance criteria a reviewer can check against the code and the tests \
         (behaviour, not effort), how to verify it (named checks: tests, lint, security; extra check \
         commands as argv arrays; manual checks only when a human is really needed), the keys of the \
         tasks it depends on, the ADR sections it implements, and path globs for the files it will change.\n\
         Workflows you may name per task (omit for the default): {}.\n\
         List what the ADR defers or excludes under out_of_scope, and anything a human must decide under open_questions.\n\n\
         This is a planning step: read the repository as much as you need, but do not change any file. \
         The orchestrator fails this step if the worktree changes.\n\n\
         ADR: {adr_path}\n----- ADR begins -----\n{}\n----- ADR ends -----\n",
        workflows.join(", "),
        adr_text.trim()
    );
    if !accepted.is_empty() {
        p.push_str(&format!(
            "\nThese plan tasks are already accepted and must keep their keys; do not propose them again, \
             but new tasks may depend on them: {}\n",
            accepted.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if let Some(prev) = previous {
        p.push_str(&format!("\nYour previous plan:\n{}\n", serde_json::to_string_pretty(prev).unwrap_or_default()));
    }
    if let Some(f) = feedback {
        p.push_str(&format!("\nThe human reviewing the plan asks for these changes:\n{}\n", f.trim()));
    }
    p
}

/// Start an epic: plan the ADR with a read-only planning agent.
pub fn create_epic(ctx: &EngineCtx, repo: &Path, adr_path: &Path, runner: Option<String>, via: &str) -> Result<Epic> {
    let repo = crate::git::main_repo_root(repo)?;
    let cfg = ctx.load_config(Some(&repo))?;
    let (adr, text) = super::load_adr(&repo, adr_path)?;
    let now = now();
    let mut epic = Epic {
        epic_id: ctx.store.next_epic_id()?,
        repo_root: repo.clone(),
        adr,
        status: EpicStatus::Planning,
        status_reason: None,
        planner_runner: runner.or(cfg.config.epic.planner_runner.clone()),
        planning_tasks: vec![],
        plan: None,
        plan_sha256: None,
        tasks: BTreeMap::new(),
        declined: vec![],
        conformance_task: None,
        conformance: None,
        created_at: now,
        updated_at: now,
        decided_by: None,
        note: None,
    };
    ctx.store.save_epic(&epic)?;
    audit(ctx, "epic_created", Actor::human(std::env::var("USER").ok()), &epic, serde_json::json!({"adr_sha256": epic.adr.sha256, "title": epic.adr.title}));
    let t = start_planning(ctx, &mut epic, &text, None, via)?;
    epic.planning_tasks.push(t);
    ctx.store.save_epic(&epic)?;
    Ok(epic)
}

fn start_planning(ctx: &EngineCtx, epic: &mut Epic, adr_text: &str, feedback: Option<&str>, via: &str) -> Result<String> {
    let cfg = ctx.load_config(Some(&epic.repo_root))?;
    let workflows: Vec<String> = ctx.catalog(Some(&epic.repo_root)).workflows()?.into_iter().map(|w| w.name).filter(|n| n != PLAN_WORKFLOW && n != CONFORMANCE_WORKFLOW).collect();
    let text = plan_prompt(&epic.adr.path, adr_text, cfg.config.epic.max_tasks, &workflows, epic.plan.as_ref(), feedback, &epic.tasks);
    let task = create_task(
        ctx,
        NewTask {
            text,
            title: Some(format!("[{}] plan: {}", epic.epic_id, epic.adr.title)),
            repo: epic.repo_root.clone(),
            options: TaskOptions { workflow: Some(PLAN_WORKFLOW.into()), runner: epic.planner_runner.clone(), ..Default::default() },
            via: via.into(),
            source: Some(TaskSource::Epic { epic_id: epic.epic_id.clone(), adr_path: epic.adr.path.clone(), adr_sha256: epic.adr.sha256.clone(), purpose: "plan".into() }),
            ..Default::default()
        },
    )?;
    Ok(task.task_id)
}

/// Structured output of a finished helper task (planning, conformance).
fn task_result(ctx: &EngineCtx, task_id: &str) -> Result<(TaskStatus, Option<serde_json::Value>, Option<String>)> {
    let t = ctx.store.load_task(task_id)?;
    let runs: Vec<Run> = t.run_ids.iter().filter_map(|id| ctx.store.load_run(id).ok()).collect();
    let value = runs.iter().filter(|r| r.status == RunStatus::Succeeded).flat_map(|r| r.steps.iter().rev()).find_map(|e| e.structured.clone());
    let reason = runs.iter().find_map(|r| r.status_reason.clone());
    Ok((t.status, value, reason))
}

/// Advance epics from what their tasks did. Cheap; called every scheduler
/// tick and before showing epics.
pub fn sync(ctx: &EngineCtx) -> Result<()> {
    for e in ctx.store.list_epics()? {
        if let Err(err) = sync_one(ctx, &e) {
            tracing::warn!("epic {}: {err:#}", e.epic_id);
        }
    }
    Ok(())
}

fn sync_one(ctx: &EngineCtx, e: &Epic) -> Result<()> {
    match e.status {
        EpicStatus::Planning => {
            let Some(tid) = e.planning_tasks.last() else { return Ok(()) };
            let (st, value, reason) = task_result(ctx, tid)?;
            if !st.is_terminal() && st != TaskStatus::Blocked {
                return Ok(());
            }
            let plan: Option<Plan> = value.and_then(|v| serde_json::from_value(v).ok()).filter(|_| st == TaskStatus::Succeeded);
            let mut changed = false;
            let e2 = ctx.store.update_epic(&e.epic_id, |x| {
                // Another process (daemon, CLI, pane) may have synced first.
                if x.status != EpicStatus::Planning || x.planning_tasks.last() != Some(tid) {
                    return Ok(());
                }
                changed = true;
                match &plan {
                    Some(p) => {
                        x.plan_sha256 = Some(crate::store::sha256_hex(crate::store::canonical_json(&serde_json::to_value(p)?).as_bytes()));
                        // Keep accepted tasks' plan entries when re-planning.
                        let mut merged = p.clone();
                        if let Some(old) = &x.plan {
                            for t in old.tasks.iter().filter(|t| x.tasks.contains_key(&t.key)) {
                                if !merged.tasks.iter().any(|n| n.key == t.key) {
                                    merged.tasks.insert(0, t.clone());
                                }
                            }
                        }
                        x.plan = Some(merged);
                        x.status = EpicStatus::Proposed;
                        x.status_reason = None;
                    }
                    None => {
                        x.status = EpicStatus::PlanFailed;
                        x.status_reason = Some(reason.clone().unwrap_or_else(|| format!("planning task #{tid} ended {}", st.as_str())));
                    }
                }
                Ok(())
            })?;
            if !changed {
                return Ok(());
            }
            let (event, msg) = if plan.is_some() { ("plan_proposed", format!("{}: plan ready — {} task(s) to review", e2.epic_id, e2.open_keys().len())) } else { ("plan_failed", format!("{}: planning failed", e2.epic_id)) };
            audit(ctx, event, Actor::orchestrator(), &e2, serde_json::json!({"plan_sha256": e2.plan_sha256, "reason": e2.status_reason}));
            ctx.notify(true, &format!("epic {}", e2.adr.title), &msg, plan.is_some());
        }
        EpicStatus::Accepted | EpicStatus::Done => {
            // Conformance review results.
            if let (Some(tid), None) = (&e.conformance_task, &e.conformance) {
                let (st, value, reason) = task_result(ctx, tid)?;
                if st.is_terminal() || st == TaskStatus::Blocked {
                    let mut changed = false;
                    let e2 = ctx.store.update_epic(&e.epic_id, |x| {
                        if x.conformance.is_some() || x.conformance_task.as_ref() != Some(tid) {
                            return Ok(());
                        }
                        changed = true;
                        match &value {
                            Some(v) => {
                                x.conformance = Some(v.clone());
                                add_followups(x, v);
                            }
                            None => x.conformance = Some(serde_json::json!({"error": reason.clone().unwrap_or_else(|| format!("conformance task #{tid} ended {}", st.as_str()))})),
                        }
                        Ok(())
                    })?;
                    if changed {
                        audit(ctx, "conformance_reviewed", Actor::orchestrator(), &e2, serde_json::json!({"followups": e2.open_keys()}));
                        let open = e2.open_keys().len();
                        ctx.notify(true, &format!("epic {}", e2.adr.title), &format!("{}: conformance review done{}", e2.epic_id, if open > 0 { format!(", {open} follow-up(s) proposed") } else { String::new() }), open > 0);
                    }
                }
            }
            // Done when every accepted task has finished.
            if e.status == EpicStatus::Accepted && !e.tasks.is_empty() {
                let all_done = e.tasks.values().all(|id| ctx.store.load_task(id).map(|t| t.status.is_terminal()).unwrap_or(true));
                if all_done && e.open_keys().is_empty() {
                    let mut changed = false;
                    let e2 = ctx.store.update_epic(&e.epic_id, |x| {
                        if x.status == EpicStatus::Accepted {
                            x.status = EpicStatus::Done;
                            changed = true;
                        }
                        Ok(())
                    })?;
                    if !changed {
                        return Ok(());
                    }
                    audit(ctx, "epic_done", Actor::orchestrator(), &e2, serde_json::json!({"tasks": e2.tasks}));
                    ctx.notify(true, &format!("epic {}", e2.adr.title), &format!("{}: all tasks finished — run `epic verify {}` to check the result against the ADR", e2.epic_id, e2.epic_id), false);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Proposed follow-ups from a conformance review become open plan tasks
/// (accepted like any other; never started automatically).
fn add_followups(e: &mut Epic, v: &serde_json::Value) {
    let Some(arr) = v.get("followups").and_then(|f| f.as_array()) else { return };
    let plan = e.plan.get_or_insert_with(Plan::default);
    let mut n = plan.tasks.iter().filter(|t| t.key.starts_with('F')).count() + 1;
    for f in arr {
        let Ok(mut t) = serde_json::from_value::<PlanTask>(f.clone()) else { continue };
        if t.title.trim().is_empty() || t.acceptance.is_empty() {
            continue;
        }
        while plan.tasks.iter().any(|x| x.key == format!("F{n}")) {
            n += 1;
        }
        t.key = format!("F{n}");
        n += 1;
        // Dependencies may only point at known tasks.
        let known: Vec<String> = plan.tasks.iter().map(|x| x.key.clone()).collect();
        t.depends_on.retain(|d| known.contains(d));
        plan.tasks.push(t);
    }
}

/// Options for turning plan tasks into queued tasks.
#[derive(Debug, Clone, Default)]
pub struct AcceptOptions {
    /// Only these keys (default: every open key).
    pub only: Option<Vec<String>>,
    pub runner: Option<String>,
    pub step_runners: BTreeMap<String, String>,
    pub base_ref: Option<String>,
    pub note: Option<String>,
    pub user: Option<String>,
    pub via: String,
}

/// Accept (part of) the plan: create the tasks, dependencies first.
pub fn accept(ctx: &EngineCtx, epic_id: &str, o: AcceptOptions) -> Result<Epic> {
    sync(ctx)?;
    let e = ctx.store.load_epic(epic_id)?;
    if !matches!(e.status, EpicStatus::Proposed | EpicStatus::Accepted | EpicStatus::Done) {
        bail!("epic {} is {}; only a proposed plan (or open follow-ups) can be accepted", e.epic_id, e.status.as_str());
    }
    let plan = e.plan.clone().context("epic has no plan")?;
    let open = e.open_keys();
    let selected: Vec<String> = match &o.only {
        Some(keys) => {
            for k in keys {
                if !open.contains(k) {
                    bail!("{k} is not an open task of epic {} (open: {})", e.epic_id, open.join(", "));
                }
            }
            keys.clone()
        }
        None => open.clone(),
    };
    if selected.is_empty() {
        bail!("nothing to accept in epic {}", e.epic_id);
    }
    // Every dependency must be accepted already or accepted now.
    for k in &selected {
        let t = e.plan_task(k).unwrap();
        for d in &t.depends_on {
            if !selected.contains(d) && !e.tasks.contains_key(d) {
                bail!("{k} depends on {d}, which is not accepted; accept both (--only {k},{d}) or decline {k}");
            }
        }
    }
    // Revalidate the whole plan (a human may have edited it).
    let cfg = ctx.load_config(Some(&e.repo_root))?;
    let workflows: Vec<String> = ctx.catalog(Some(&e.repo_root)).workflows()?.into_iter().map(|w| w.name).collect();
    super::validate_plan(&plan, &PlanRules { max_tasks: cfg.config.epic.max_tasks.max(plan.tasks.len()), workflows: &workflows, existing_keys: &[] })?;
    let chosen: Vec<PlanTask> = plan.tasks.iter().filter(|t| selected.contains(&t.key)).cloned().collect();
    let order = super::topo_order(&chosen).map_err(|c| anyhow::anyhow!("dependency cycle: {c}"))?;
    let mut created: BTreeMap<String, String> = BTreeMap::new();
    for key in &order {
        let t = chosen.iter().find(|t| &t.key == key).unwrap();
        let depends_on: Vec<String> = t.depends_on.iter().filter_map(|d| created.get(d).or_else(|| e.tasks.get(d)).cloned()).collect();
        let text = task_text(&e, &plan, t);
        let options = TaskOptions {
            workflow: Some(t.workflow.clone().unwrap_or_else(|| cfg.config.epic.task_workflow.clone())),
            runner: o.runner.clone(),
            base_ref: o.base_ref.clone(),
            step_runners: o.step_runners.clone(),
            scope: t.scope.clone(),
            extra_checks: t.verification.checks.clone(),
            extra_commands: t.verification.commands.clone(),
            ..Default::default()
        };
        let task = create_task(
            ctx,
            NewTask {
                text,
                title: Some(format!("[{}/{}] {}", e.epic_id, t.key, t.title)),
                repo: e.repo_root.clone(),
                options,
                via: o.via.clone(),
                source: None,
                epic: Some(EpicLink { epic_id: e.epic_id.clone(), key: t.key.clone() }),
                depends_on,
                acceptance: t.acceptance.clone(),
                manual_checks: t.verification.manual.clone(),
            },
        )?;
        created.insert(key.clone(), task.task_id);
    }
    let e2 = ctx.store.update_epic(&e.epic_id, |x| {
        x.tasks.extend(created.clone());
        x.status = EpicStatus::Accepted;
        x.decided_by = o.user.clone();
        if o.note.is_some() {
            x.note = o.note.clone();
        }
        Ok(())
    })?;
    let commands: Vec<String> = chosen.iter().flat_map(|t| t.verification.commands.iter().map(|c| crate::policies::command::display_argv(c))).collect();
    audit(ctx, "plan_accepted", Actor::human(o.user.clone()), &e2, serde_json::json!({"keys": order, "tasks": created, "approved_commands": commands, "plan_sha256": e2.plan_sha256, "note": o.note}));
    Ok(e2)
}

fn task_text(e: &Epic, plan: &Plan, t: &PlanTask) -> String {
    let mut s = format!("{}\n\n{}\n", t.title, t.description.trim());
    s.push_str(&format!("\nThis task is part of epic {} implementing {} (\"{}\")", e.epic_id, e.adr.path, e.adr.title));
    if !t.adr_refs.is_empty() {
        s.push_str(&format!(", sections: {}", t.adr_refs.join(", ")));
    }
    s.push_str(".\n");
    if !plan.decision_summary.trim().is_empty() {
        s.push_str(&format!("ADR decision: {}\n", plan.decision_summary.trim()));
    }
    if !t.scope.is_empty() {
        s.push_str(&format!("Expected to change: {}\n", t.scope.join(", ")));
    }
    if !plan.out_of_scope.is_empty() {
        s.push_str(&format!("Out of scope for the whole epic: {}\n", plan.out_of_scope.join("; ")));
    }
    s
}

/// Decline open plan tasks, or the whole proposed plan.
pub fn reject(ctx: &EngineCtx, epic_id: &str, only: Option<Vec<String>>, note: Option<String>, user: Option<String>) -> Result<Epic> {
    let e = ctx.store.load_epic(epic_id)?;
    let e2 = ctx.store.update_epic(&e.epic_id, |x| {
        match &only {
            Some(keys) => {
                let open = x.open_keys();
                for k in keys {
                    if !open.contains(k) {
                        bail!("{k} is not an open task");
                    }
                    x.declined.push(k.clone());
                }
            }
            None => {
                if x.tasks.is_empty() {
                    x.status = EpicStatus::Rejected;
                } else {
                    let open = x.open_keys();
                    x.declined.extend(open);
                }
            }
        }
        x.note = note.clone().or(x.note.clone());
        x.decided_by = user.clone();
        Ok(())
    })?;
    audit(ctx, "plan_rejected", Actor::human(user), &e2, serde_json::json!({"keys": only, "note": note}));
    Ok(e2)
}

/// Plan again with the human's feedback (and the current ADR text).
pub fn replan(ctx: &EngineCtx, epic_id: &str, feedback: Option<String>, via: &str) -> Result<Epic> {
    let mut e = ctx.store.load_epic(epic_id)?;
    if e.status == EpicStatus::Planning {
        bail!("epic {} is already planning", e.epic_id);
    }
    let (adr, text) = super::load_adr(&e.repo_root, Path::new(&e.adr.path))?;
    let changed = adr.sha256 != e.adr.sha256;
    e.adr = adr;
    let t = start_planning(ctx, &mut e, &text, feedback.as_deref(), via)?;
    let e2 = ctx.store.update_epic(&e.epic_id, |x| {
        x.adr = e.adr.clone();
        x.planning_tasks.push(t.clone());
        x.status = EpicStatus::Planning;
        // Open proposals are replaced by the new plan.
        x.declined.clear();
        Ok(())
    })?;
    audit(ctx, "plan_regenerated", Actor::human(std::env::var("USER").ok()), &e2, serde_json::json!({"feedback": feedback, "adr_changed": changed, "task": t}));
    Ok(e2)
}

/// Replace the plan with a human-edited one (YAML or JSON). Accepted tasks
/// keep their keys; the edited plan is validated in full.
pub fn set_plan(ctx: &EngineCtx, epic_id: &str, text: &str, user: Option<String>) -> Result<Epic> {
    let e = ctx.store.load_epic(epic_id)?;
    let plan: Plan = serde_yaml_ng::from_str(text).context("plan must be YAML or JSON in the plan format")?;
    let cfg = ctx.load_config(Some(&e.repo_root))?;
    let workflows: Vec<String> = ctx.catalog(Some(&e.repo_root)).workflows()?.into_iter().map(|w| w.name).collect();
    super::validate_plan(&plan, &PlanRules { max_tasks: cfg.config.epic.max_tasks, workflows: &workflows, existing_keys: &[] })?;
    for k in e.tasks.keys() {
        if !plan.tasks.iter().any(|t| &t.key == k) {
            bail!("{k} is already accepted and cannot be removed from the plan");
        }
    }
    let e2 = ctx.store.update_epic(&e.epic_id, |x| {
        x.plan_sha256 = Some(crate::store::sha256_hex(crate::store::canonical_json(&serde_json::to_value(&plan)?).as_bytes()));
        x.plan = Some(plan.clone());
        if matches!(x.status, EpicStatus::PlanFailed | EpicStatus::Planning) {
            x.status = EpicStatus::Proposed;
        }
        Ok(())
    })?;
    audit(ctx, "plan_edited", Actor::human(user), &e2, serde_json::json!({"plan_sha256": e2.plan_sha256}));
    Ok(e2)
}

/// Review the combined result against the ADR (read-only agent).
pub fn verify(ctx: &EngineCtx, epic_id: &str, via: &str) -> Result<Epic> {
    sync(ctx)?;
    let e = ctx.store.load_epic(epic_id)?;
    if e.tasks.is_empty() {
        bail!("epic {} has no accepted tasks yet", e.epic_id);
    }
    let (_, adr_text) = super::load_adr(&e.repo_root, Path::new(&e.adr.path))?;
    let plan = e.plan.clone().unwrap_or_default();
    let order = super::topo_order(&plan.tasks.iter().filter(|t| e.tasks.contains_key(&t.key)).cloned().collect::<Vec<_>>()).unwrap_or_default();
    let mut lines = vec![];
    for k in &order {
        let Some(tid) = e.tasks.get(k) else { continue };
        let t = ctx.store.load_task(tid)?;
        let run = t.run_ids.iter().filter_map(|id| ctx.store.load_run(id).ok()).find(|r| r.status == RunStatus::Succeeded);
        let branch = run.as_ref().and_then(|r| r.git.branch.clone());
        lines.push(format!(
            "- {k} (task #{tid}, {}): {}{}{}",
            t.status.as_str(),
            t.title,
            branch.map(|b| format!(" — branch {b}")).unwrap_or_default(),
            run.and_then(|r| r.pr_url).map(|u| format!(" — {u}")).unwrap_or_default()
        ));
    }
    // Start from the base branch: merged work is there; unmerged work is on
    // the task branches listed below (dependencies form a DAG, so no single
    // branch has to contain everything).
    let text = format!(
        "Check whether the implemented work fulfils the Architecture Decision Record below.\n\n\
         Go through every statement in its Decision and Consequences sections and mark it covered, \
         partial or missing, with evidence (files, tests, PRs). For every gap propose a follow-up task \
         with acceptance criteria. Do not change any file.\n\n\
         You are on the base branch. Work that is not merged yet is on the task branches below; \
         inspect it with `git log` / `git diff HEAD...<branch>` (read-only).\n\n\
         Accepted tasks of epic {}:\n{}\n\n\
         ADR: {}\n----- ADR begins -----\n{}\n----- ADR ends -----\n",
        e.epic_id,
        lines.join("\n"),
        e.adr.path,
        adr_text.trim()
    );
    let task = create_task(
        ctx,
        NewTask {
            text,
            title: Some(format!("[{}] conformance: {}", e.epic_id, e.adr.title)),
            repo: e.repo_root.clone(),
            options: TaskOptions { workflow: Some(CONFORMANCE_WORKFLOW.into()), runner: e.planner_runner.clone(), ..Default::default() },
            via: via.into(),
            source: Some(TaskSource::Epic { epic_id: e.epic_id.clone(), adr_path: e.adr.path.clone(), adr_sha256: e.adr.sha256.clone(), purpose: "conformance".into() }),
            ..Default::default()
        },
    )?;
    let e2 = ctx.store.update_epic(&e.epic_id, |x| {
        x.conformance_task = Some(task.task_id.clone());
        x.conformance = None;
        Ok(())
    })?;
    audit(ctx, "conformance_requested", Actor::human(std::env::var("USER").ok()), &e2, serde_json::json!({"task": task.task_id}));
    Ok(e2)
}

/// Whether the ADR changed since the epic was planned.
pub fn adr_drifted(e: &Epic) -> Option<bool> {
    let text = std::fs::read_to_string(e.repo_root.join(&e.adr.path)).ok()?;
    Some(crate::store::sha256_hex(text.as_bytes()) != e.adr.sha256)
}

// ------------------------------------------------------------ dependencies

/// What the scheduler may do with a queued task that has dependencies.
#[derive(Debug, Clone, PartialEq)]
pub enum DepState {
    /// Start it; optionally from this base ref (stacked mode).
    Ready(Option<String>),
    /// Not yet; the reason is shown to the human.
    Waiting(String),
    /// A dependency failed or was cancelled; a human decides.
    Blocked(String),
}

pub fn dependency_state(ctx: &EngineCtx, task: &Task) -> Result<DepState> {
    if task.depends_on.is_empty() {
        return Ok(DepState::Ready(None));
    }
    let cfg = ctx.load_config(Some(&task.repo_root))?;
    let mode = cfg.config.epic.dependency_mode;
    let base = crate::git::default_base(&task.repo_root, task.options.base_ref.as_deref().or(cfg.config.defaults.base_branch.as_deref()))?;
    let mut unmerged: Vec<(String, String)> = vec![];
    for dep_id in &task.depends_on {
        let dep = ctx.store.load_task(dep_id).with_context(|| format!("dependency #{dep_id}"))?;
        match dep.status {
            TaskStatus::Failed | TaskStatus::Cancelled => return Ok(DepState::Blocked(format!("dependency #{dep_id} {}", dep.status.as_str()))),
            TaskStatus::Succeeded => {}
            _ => return Ok(DepState::Waiting(format!("waiting for #{dep_id} ({})", dep.status.as_str().replace('_', " ")))),
        }
        let run = dep
            .selected_run
            .clone()
            .and_then(|id| ctx.store.load_run(&id).ok())
            .or_else(|| dep.run_ids.iter().filter_map(|id| ctx.store.load_run(id).ok()).find(|r| r.status == RunStatus::Succeeded));
        let Some(run) = run else { return Ok(DepState::Waiting(format!("#{dep_id} has no successful run selected"))) };
        let head = run.git.head_sha.clone().or_else(|| run.git.commits.last().cloned());
        let merged = match &head {
            Some(h) => run.git.base_sha.as_deref() == Some(h.as_str()) || crate::git::is_ancestor(&task.repo_root, h, &base),
            None => true, // nothing was committed: nothing to wait for
        };
        if !merged {
            unmerged.push((dep_id.clone(), run.git.branch.clone().unwrap_or_default()));
        }
    }
    match (mode, unmerged.len()) {
        (_, 0) => Ok(DepState::Ready(None)),
        (DependencyMode::Merged, _) => Ok(DepState::Waiting(format!(
            "waiting for {} to be merged into {} (merge the PR and update the local branch, or `task unblock`)",
            unmerged.iter().map(|(id, _)| format!("#{id}")).collect::<Vec<_>>().join(", "),
            task.options.base_ref.clone().or(cfg.config.defaults.base_branch.clone()).unwrap_or_else(|| "the base branch".into())
        ))),
        (DependencyMode::Stacked, 1) => Ok(DepState::Ready(Some(unmerged[0].1.clone()))),
        (DependencyMode::Stacked, _) => Ok(DepState::Waiting(format!(
            "stacked mode can build on one unmerged branch, but {} are unmerged; merge all but one",
            unmerged.iter().map(|(id, _)| format!("#{id}")).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// Human override: start a task regardless of its dependencies.
pub fn unblock(ctx: &EngineCtx, task_id: &str, user: Option<String>) -> Result<Task> {
    let t = ctx.store.load_task(task_id)?;
    if t.depends_on.is_empty() {
        bail!("task #{} has no dependencies", t.task_id);
    }
    if !t.run_ids.is_empty() {
        bail!("task #{} has already started", t.task_id);
    }
    let deps = t.depends_on.clone();
    let t = ctx.store.update_task(&t.task_id, |x| {
        x.depends_on.clear();
        x.waiting_on = None;
        if x.status == TaskStatus::Blocked {
            x.status = TaskStatus::Queued;
        }
        Ok(())
    })?;
    ctx.audit(EventDraft::new("dependencies_overridden", Actor::human(user)).task(&t.task_id).data(serde_json::json!({"depends_on": deps})));
    Ok(t)
}
