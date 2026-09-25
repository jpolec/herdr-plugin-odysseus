//! The morning inbox: everything that needs a human, with a risk level and
//! the reasons for it, plus the night shift (a time and token budget for
//! unattended work). Risk is computed from recorded facts only — policy
//! rules that fired, diff size, review findings, acceptance results,
//! retries, the watchdog, a contract — and every level lists its reasons.

use std::collections::BTreeSet;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::EngineCtx;
use crate::approvals::{ApprovalKind, ApprovalRequest, ApprovalStatus};
use crate::audit::{Actor, EventDraft};
use crate::model::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Risk {
    pub level: RiskLevel,
    pub reasons: Vec<String>,
}

/// Rules whose match makes a change high-risk by nature.
const HIGH_RULES: &[&str] = &["approve-migrations", "approve-infra", "approve-ci-cd", "approve-auth-security-config", "approve-agent-instructions-and-orchestrator-config", "approve-infra-commands", "approve-history-rewrite", "approve-privilege-escalation", "approve-large-deletes", "approve-large-line-deletes"];
/// Rules that call for a closer look.
const MEDIUM_RULES: &[&str] = &["approve-test-deletion", "approve-test-runner-config", "approve-disabled-tests", "guard-test-only-retry", "task-scope", "approve-large-lockfile-changes", "approve-network-commands", "approve-shell-mode"];

pub fn assess(run: &Run) -> Risk {
    let mut level = RiskLevel::Low;
    let mut reasons: Vec<String> = vec![];
    let mut bump = |l: RiskLevel, why: String, reasons: &mut Vec<String>| {
        if !reasons.contains(&why) {
            reasons.push(why);
        }
        level = level.max(l);
    };
    let fired: BTreeSet<&str> = run.policy_decisions.iter().filter(|d| d.decision.decision != crate::policies::Decision::Allow).flat_map(|d| d.decision.matched.iter().map(|m| m.rule_id.as_str())).collect();
    for r in &fired {
        if HIGH_RULES.contains(r) {
            bump(RiskLevel::High, format!("touches a sensitive area ({r})"), &mut reasons);
        } else if MEDIUM_RULES.contains(r) {
            bump(RiskLevel::Medium, format!("guardrail fired ({r})"), &mut reasons);
        }
    }
    if let Some(d) = &run.diff_stat {
        let lines = d.insertions + d.deletions;
        if lines > 1500 || d.files_changed > 40 {
            bump(RiskLevel::High, format!("large change: {} files, {lines} lines", d.files_changed), &mut reasons);
        } else if lines > 400 || d.files_changed > 15 {
            bump(RiskLevel::Medium, format!("sizeable change: {} files, {lines} lines", d.files_changed), &mut reasons);
        }
    }
    let mut reviewed = false;
    for v in run.steps.iter().filter_map(|e| e.structured.as_ref()) {
        if let Some(f) = v.get("findings").and_then(|f| f.as_array()) {
            reviewed = true;
            let sev = |s: &str| f.iter().filter(|x| x["severity"] == s).count();
            if sev("critical") + sev("high") > 0 {
                bump(RiskLevel::High, format!("review found {} critical/high issue(s)", sev("critical") + sev("high")), &mut reasons);
            } else if sev("medium") > 0 {
                bump(RiskLevel::Medium, format!("review found {} medium issue(s)", sev("medium")), &mut reasons);
            }
        }
        if let Some(c) = v.get("criteria").and_then(|c| c.as_array()) {
            reviewed = true;
            let n = |s: &str| c.iter().filter(|x| x["status"] == s).count();
            if n("not_met") > 0 {
                bump(RiskLevel::High, format!("{} acceptance criterion(s) not met", n("not_met")), &mut reasons);
            } else if n("unverifiable") > 0 {
                bump(RiskLevel::Medium, format!("{} acceptance criterion(s) need a human check", n("unverifiable")), &mut reasons);
            }
        }
    }
    // Only the latest review decides whether it is still unapproved.
    if let Some(v) = run.steps.iter().rev().filter_map(|e| e.structured.as_ref()).find(|v| v.get("verdict").is_some()) {
        if v["verdict"] != "approved" {
            bump(RiskLevel::High, format!("last review verdict: {}", v["verdict"].as_str().unwrap_or("?")), &mut reasons);
        }
    }
    let retries: u32 = run.retry_counts.values().sum();
    if retries >= 2 {
        bump(RiskLevel::Medium, format!("needed {retries} retries"), &mut reasons);
    }
    if run.steps.iter().any(|e| e.attention.is_some()) {
        bump(RiskLevel::Medium, "the watchdog flagged the agent as stuck".into(), &mut reasons);
    }
    let contract = run.contract.as_ref().filter(|c| c.approval_id.is_some());
    if run.contract.as_ref().is_some_and(|c| c.approval_id.is_none()) {
        reasons.push("contract written and proven red; waiting for your approval".into());
    } else if contract.is_none() && !reviewed && run.diff_stat.as_ref().is_some_and(|d| d.files_changed > 0) {
        bump(RiskLevel::Medium, "nobody reviewed it and no contract holds it".into(), &mut reasons);
    }
    if let Some(c) = contract {
        reasons.push(format!("held to an approved contract ({} file(s))", c.files.len()));
    }
    if reasons.is_empty() {
        reasons.push("small, reviewed, no guardrail fired".into());
    }
    Risk { level, reasons }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InboxItem {
    /// An approval is pending.
    Approval { approval_id: String, run: String, run_id: String, step: String, reason: String, workflow_step: bool, risk: Risk },
    /// A run stopped and needs a decision (blocked, needs_human, failed).
    Stopped { run: String, run_id: String, status: String, reason: String },
    /// An agent is asking something or looks stuck.
    Agent { run: String, run_id: String, step: String, reason: String },
    /// A plan waits to be accepted.
    Plan { epic_id: String, title: String, open: usize },
    /// Finished work to look at (PR or branch).
    Ready { run: String, run_id: String, title: String, pr: Option<String>, risk: Risk },
}

impl InboxItem {
    fn rank(&self) -> (u8, u8) {
        let r = |x: &Risk| 2 - x.level as u8;
        match self {
            Self::Agent { .. } => (0, 0),
            Self::Approval { risk, .. } => (1, r(risk)),
            Self::Stopped { .. } => (2, 0),
            Self::Plan { .. } => (3, 0),
            Self::Ready { risk, .. } => (4, r(risk)),
        }
    }
}

/// Everything that needs a human, most urgent first. `since` limits the
/// finished-work section (default: runs finished in the last 24 h).
pub fn inbox(ctx: &EngineCtx, since: chrono::Duration) -> Result<Vec<InboxItem>> {
    let runs = ctx.store.list_runs()?;
    let tasks = ctx.store.list_tasks()?;
    let title = |r: &Run| tasks.iter().find(|t| t.task_id == r.task_id).map(|t| t.title.clone()).unwrap_or_default();
    let mut out = vec![];
    for a in crate::approvals::pending(&ctx.store)? {
        let Some(r) = runs.iter().find(|r| r.run_id == a.run_id) else { continue };
        out.push(InboxItem::Approval { approval_id: a.approval_id.clone(), run: r.display_name(), run_id: r.run_id.clone(), step: a.step_id.clone(), reason: a.reason.clone(), workflow_step: a.kind == ApprovalKind::WorkflowStep, risk: assess(r) });
    }
    let cutoff = now() - since;
    for r in &runs {
        if let Some(e) = r.steps.iter().rev().find(|e| e.status == StepStatus::AwaitingHuman) {
            let reason = e.attention.clone().unwrap_or_else(|| "the agent is asking something in its pane".into());
            out.push(InboxItem::Agent { run: r.display_name(), run_id: r.run_id.clone(), step: e.step_id.clone(), reason });
        }
        let recent = r.completed_at.is_none_or(|t| t >= cutoff) && r.updated_at >= cutoff;
        match r.status {
            RunStatus::Blocked | RunStatus::NeedsHuman => out.push(InboxItem::Stopped { run: r.display_name(), run_id: r.run_id.clone(), status: r.status.as_str().into(), reason: r.status_reason.clone().unwrap_or_default() }),
            RunStatus::Failed if recent => out.push(InboxItem::Stopped { run: r.display_name(), run_id: r.run_id.clone(), status: "failed".into(), reason: r.status_reason.clone().unwrap_or_default() }),
            RunStatus::Succeeded if recent && r.diff_stat.as_ref().is_some_and(|d| d.files_changed > 0) => {
                out.push(InboxItem::Ready { run: r.display_name(), run_id: r.run_id.clone(), title: title(r), pr: r.pr_url.clone(), risk: assess(r) })
            }
            _ => {}
        }
    }
    for e in ctx.store.list_epics()? {
        let open = e.open_keys().len();
        if open > 0 && matches!(e.status, crate::epic::EpicStatus::Proposed | crate::epic::EpicStatus::Accepted | crate::epic::EpicStatus::Done) {
            out.push(InboxItem::Plan { epic_id: e.epic_id.clone(), title: e.adr.title.clone(), open });
        }
    }
    out.sort_by_key(|i| i.rank());
    Ok(out)
}

/// Approve every pending *workflow-step* approval ("Ship it?") whose run is
/// at most `max` risk. Policy asks are never approved in bulk.
pub fn approve_batch(ctx: &EngineCtx, max: RiskLevel, user: Option<String>) -> Result<Vec<String>> {
    let mut done = vec![];
    for a in crate::approvals::pending(&ctx.store)? {
        if a.kind != ApprovalKind::WorkflowStep {
            continue;
        }
        let Ok(run) = ctx.store.load_run(&a.run_id) else { continue };
        let risk = assess(&run);
        if risk.level > max {
            continue;
        }
        let a: ApprovalRequest = crate::approvals::decide(&ctx.store, &a.approval_id, true, user.clone(), Some(format!("batch approval (risk {}: {})", risk.level.as_str(), risk.reasons.join("; "))))?;
        if a.status == ApprovalStatus::Approved {
            ctx.audit(EventDraft::new("approval_batch", Actor::human(user.clone())).run(&run.run_id, &run.task_id).data(serde_json::json!({"approval_id": a.approval_id, "risk": risk})));
            done.push(a.approval_id);
        }
    }
    Ok(done)
}

// ------------------------------------------------------------------ shift

/// Unattended work with a deadline and a token budget.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Shift {
    pub started_at: Timestamp,
    pub until: Option<Timestamp>,
    pub budget_tokens: Option<u64>,
    #[serde(default)]
    pub ended: Option<String>,
}

/// Parse `07:00` (next occurrence, local time) or a duration like `8h`.
pub fn parse_until(s: &str) -> Result<Timestamp> {
    if let Some((h, m)) = s.split_once(':') {
        let (h, m): (u32, u32) = (h.parse()?, m.parse()?);
        let local = chrono::Local::now();
        let today = local.date_naive().and_hms_opt(h, m, 0).ok_or_else(|| anyhow::anyhow!("invalid time {s}"))?;
        let mut t = today.and_local_timezone(chrono::Local).single().ok_or_else(|| anyhow::anyhow!("ambiguous local time {s}"))?;
        if t <= local {
            t += chrono::Duration::days(1);
        }
        return Ok(t.with_timezone(&chrono::Utc));
    }
    let d: crate::config::HumanDuration = serde_json::from_value(serde_json::Value::String(s.into())).map_err(|_| anyhow::anyhow!("--until must be HH:MM or a duration like 8h"))?;
    Ok(now() + chrono::Duration::from_std(d.as_duration())?)
}

fn shift_path(ctx: &EngineCtx) -> std::path::PathBuf {
    ctx.store.layout.root.join("state/shift.json")
}

pub fn load_shift(ctx: &EngineCtx) -> Option<Shift> {
    let p = shift_path(ctx);
    p.exists().then(|| crate::store::read_doc(&ctx.store.layout, &p, "shift").ok()).flatten()
}

pub fn start_shift(ctx: &EngineCtx, until: Option<Timestamp>, budget_tokens: Option<u64>, user: Option<String>) -> Result<Shift> {
    if until.is_none() && budget_tokens.is_none() {
        bail!("give --until and/or --budget");
    }
    let s = Shift { started_at: now(), until, budget_tokens, ended: None };
    crate::store::write_doc(&shift_path(ctx), "shift", &s)?;
    ctx.store.save_scheduler(&crate::store::SchedulerState { paused: false })?;
    ctx.audit(EventDraft::new("shift_started", Actor::human(user)).data(serde_json::to_value(&s)?));
    Ok(s)
}

pub fn stop_shift(ctx: &EngineCtx, reason: &str) -> Result<Option<Shift>> {
    let Some(mut s) = load_shift(ctx) else { return Ok(None) };
    if s.ended.is_some() {
        return Ok(Some(s));
    }
    s.ended = Some(reason.to_string());
    crate::store::write_doc(&shift_path(ctx), "shift", &s)?;
    Ok(Some(s))
}

/// Tokens used by agent steps that started during the shift.
pub fn shift_tokens(ctx: &EngineCtx, s: &Shift) -> Result<u64> {
    let mut n = 0;
    for r in ctx.store.list_runs()? {
        for e in r.steps.iter().filter(|e| e.kind == StepKind::Agent && e.started_at.is_some_and(|t| t >= s.started_at)) {
            if let Some(u) = &e.usage {
                n += u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0);
            }
        }
    }
    Ok(n)
}

/// Called every scheduler tick: end the shift at its deadline or budget by
/// pausing the queue (running work finishes; nothing new starts) and
/// telling the human the inbox is ready. Returns `true` if it ended now.
pub fn enforce_shift(ctx: &EngineCtx) -> Result<bool> {
    let Some(s) = load_shift(ctx) else { return Ok(false) };
    if s.ended.is_some() {
        return Ok(false);
    }
    let reason = if s.until.is_some_and(|u| now() >= u) {
        Some("shift time is over".to_string())
    } else if let Some(b) = s.budget_tokens {
        let used = shift_tokens(ctx, &s)?;
        (used >= b).then(|| format!("shift token budget reached ({used} of {b})"))
    } else {
        None
    };
    let Some(reason) = reason else { return Ok(false) };
    ctx.store.save_scheduler(&crate::store::SchedulerState { paused: true })?;
    stop_shift(ctx, &reason)?;
    let items = inbox(ctx, now() - s.started_at)?;
    ctx.audit(EventDraft::new("shift_ended", Actor::orchestrator()).data(serde_json::json!({"reason": reason, "inbox": items.len()})));
    ctx.notify(true, "herdr-orchestrator: shift ended", &format!("{reason}; queue paused. {} item(s) in your inbox — `herdr-orchestrator inbox` or [i] in the pane", items.len()), true);
    Ok(true)
}
