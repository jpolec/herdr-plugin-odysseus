//! Plain-text rendering for CLI output (the TUI has its own widgets).

use serde::Serialize;

use crate::approvals::ApprovalRequest;
use crate::audit::AuditEvent;
use crate::model::*;

pub fn dur(secs: Option<i64>) -> String {
    match secs {
        Some(s) if s >= 3600 => format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60),
        Some(s) => format!("{}:{:02}", s / 60, s % 60),
        None => String::new(),
    }
}

pub fn step_icon(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Succeeded => "✓",
        StepStatus::Failed => "✗",
        StepStatus::Running | StepStatus::Starting => "→",
        StepStatus::Retrying => "↻",
        StepStatus::AwaitingApproval => "?",
        StepStatus::AwaitingHuman => "!",
        StepStatus::Skipped => "-",
        StepStatus::Cancelled => "⊘",
        StepStatus::Pending => "○",
    }
}

/// One-line summary: `#12   running   implement-review  review → claude  herdr/12-x`.
pub fn run_line(r: &Run) -> String {
    let current = r
        .steps
        .iter()
        .rev()
        .find(|e| !e.status.is_terminal())
        .or(r.steps.last())
        .map(|e| format!("{} {}{}", e.step_id, step_icon(e.status), e.runner.as_ref().map(|x| format!(" {x}")).unwrap_or_default()))
        .unwrap_or_default();
    format!(
        "{:<6} {:<17} {:<18} {:<28} {}{}",
        r.display_name(),
        r.status.as_str(),
        r.workflow_name,
        current,
        r.git.branch.clone().unwrap_or_default(),
        r.status_reason.as_ref().filter(|_| !r.status.is_active() && r.status != RunStatus::Succeeded).map(|s| format!("  — {s}")).unwrap_or_default()
    )
}

pub fn run_detail(r: &Run, task: &Task) -> String {
    let mut s = String::new();
    s.push_str(&format!("Run {}  ({})\n", r.display_name(), r.run_id));
    s.push_str(&format!("Task       #{} {}\n", task.task_id, task.title));
    s.push_str(&format!("Status     {}{}\n", r.status.as_str(), r.status_reason.as_ref().map(|x| format!(" — {x}")).unwrap_or_default()));
    s.push_str(&format!("Workflow   {} (sha256 {})\n", r.workflow_name, &r.workflow_sha256[..12.min(r.workflow_sha256.len())]));
    s.push_str(&format!("Repository {}\n", r.repo_root.display()));
    s.push_str(&format!("Branch     {}\n", r.git.branch.clone().unwrap_or_default()));
    s.push_str(&format!("Worktree   {}\n", r.git.worktree_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()));
    s.push_str(&format!("Base       {} {}\n", r.git.base_ref, r.git.base_sha.as_deref().map(|x| &x[..12.min(x.len())]).unwrap_or("")));
    if let Some(ws) = &r.herdr.workspace_id {
        s.push_str(&format!("Herdr      workspace {ws}\n"));
    }
    if let Some(u) = &r.pr_url {
        s.push_str(&format!("PR         {u}\n"));
    }
    s.push_str(&format!("\n{:<16} {:<18} {:<10} {:<8} {:>8}  {}\n", "STEP", "STATUS", "AGENT", "ATTEMPT", "TIME", "PANE"));
    for e in &r.steps {
        s.push_str(&format!(
            "{:<16} {} {:<16} {:<10} {:<8} {:>8}  {}\n",
            e.step_id,
            step_icon(e.status),
            e.status.as_str(),
            e.runner.clone().unwrap_or_default(),
            e.attempt,
            dur(e.duration_secs()),
            e.agent.as_ref().and_then(|a| a.pane_id.clone()).unwrap_or_default()
        ));
        if let Some(err) = &e.error {
            s.push_str(&format!("{:<16}   {}\n", "", err.lines().next().unwrap_or("")));
        }
    }
    if let Some(d) = &r.diff_stat {
        s.push_str(&format!("\nFiles changed: {}   +{} / -{}\n", d.files_changed, d.insertions, d.deletions));
        for f in d.files.iter().take(20) {
            s.push_str(&format!("  {:<10} {} (+{} -{})\n", f.change, f.path, f.insertions, f.deletions));
        }
        if d.files.len() > 20 {
            s.push_str(&format!("  … {} more\n", d.files.len() - 20));
        }
    }
    let checks: Vec<&StepExecution> = r.steps.iter().filter(|e| e.kind == StepKind::Command).collect();
    if !checks.is_empty() {
        s.push_str("\nChecks:\n");
        for c in checks {
            s.push_str(&format!("  {} {} (attempt {}, exit {})\n", step_icon(c.status), c.step_id, c.attempt, c.exit_code.map(|x| x.to_string()).unwrap_or("-".into())));
        }
    }
    if let Some(rv) = r.steps.iter().rev().find_map(|e| e.structured.as_ref()) {
        s.push_str(&format!("\nReview: {} ({} findings)\n", rv["verdict"].as_str().unwrap_or("?"), rv["findings"].as_array().map(|a| a.len()).unwrap_or(0)));
        for f in rv["findings"].as_array().into_iter().flatten().take(10) {
            s.push_str(&format!("  [{}] {}{} — {}\n", f["severity"].as_str().unwrap_or("?"), f["file"].as_str().unwrap_or(""), f["line"].as_u64().map(|l| format!(":{l}")).unwrap_or_default(), f["description"].as_str().unwrap_or("")));
        }
    }
    let (a, q, d) = r.policy_summary();
    s.push_str(&format!("\nPolicy: {a} allow · {q} approval · {d} denied\n"));
    let u = r.usage_total();
    s.push_str(&format!("Usage:  {} · cost {}\n", crate::telemetry::tokens_display(&u), u.cost_display()));
    s
}

#[derive(Debug, Clone, Serialize)]
pub struct CompareRow {
    pub run: String,
    pub run_id: String,
    pub runner: String,
    pub status: String,
    pub files: usize,
    pub insertions: u64,
    pub deletions: u64,
    pub tests: String,
    pub review: String,
    pub runtime: String,
    pub cost: String,
    pub branch: String,
}

pub fn compare_row(r: &Run) -> CompareRow {
    let d = r.diff_stat.clone().unwrap_or_default();
    let tests = r
        .steps
        .iter()
        .rev()
        .find(|e| e.kind == StepKind::Command)
        .map(|e| format!("{}{}", e.status.as_str(), if e.attempt > 1 { format!(" (try {})", e.attempt) } else { String::new() }))
        .unwrap_or_else(|| "-".into());
    let review = r
        .steps
        .iter()
        .rev()
        .find_map(|e| e.structured.as_ref())
        .map(|v| format!("{} ({})", v["verdict"].as_str().unwrap_or("?"), v["findings"].as_array().map(|a| a.len()).unwrap_or(0)))
        .unwrap_or_else(|| "-".into());
    let runtime = match (r.started_at, r.completed_at) {
        (Some(a), Some(b)) => dur(Some((b - a).num_seconds())),
        _ => "-".into(),
    };
    CompareRow {
        run: r.display_name(),
        run_id: r.run_id.clone(),
        runner: r.steps.iter().find(|e| e.kind == StepKind::Agent).and_then(|e| e.runner.clone()).unwrap_or_default(),
        status: r.status.as_str().into(),
        files: d.files_changed,
        insertions: d.insertions,
        deletions: d.deletions,
        tests,
        review,
        runtime,
        cost: r.usage_total().cost_display(),
        branch: r.git.branch.clone().unwrap_or_default(),
    }
}

pub fn compare_table(rows: &[CompareRow], selected: Option<&str>) -> String {
    let mut s = format!("{:<3}{:<7} {:<12} {:<10} {:>5} {:>7} {:>7}  {:<16} {:<22} {:>8}  {}\n", "", "RUN", "RUNNER", "STATUS", "FILES", "+", "-", "TESTS", "REVIEW", "TIME", "COST");
    for r in rows {
        let mark = if selected == Some(r.run_id.as_str()) { "★ " } else { "  " };
        s.push_str(&format!(
            "{:<3}{:<7} {:<12} {:<10} {:>5} {:>7} {:>7}  {:<16} {:<22} {:>8}  {}\n",
            mark, r.run, r.runner, r.status, r.files, r.insertions, r.deletions, r.tests, r.review, r.runtime, r.cost
        ));
    }
    s
}

pub fn approval_detail(a: &ApprovalRequest) -> String {
    let c = &a.context;
    let mut s = String::new();
    s.push_str(&format!("Approval   {} ({})\n", a.approval_id, a.status.as_str()));
    s.push_str(&format!("Task       #{} {}\n", a.task_id, c.task_title));
    s.push_str(&format!("Workflow   {}   step {}\n", c.workflow, c.step_id));
    s.push_str(&format!("Reason     {}\n", a.reason));
    if let Some(p) = &c.pending_action {
        s.push_str(&format!("On approve {p}\n"));
    }
    s.push_str(&format!("Agent      {}{}\n", c.agent.clone().unwrap_or("-".into()), c.pane_id.as_ref().map(|p| format!(" (pane {p})")).unwrap_or_default()));
    s.push_str(&format!("Repository {}\n", c.repository.display()));
    s.push_str(&format!("Branch     {}\n", c.branch.clone().unwrap_or_default()));
    s.push_str(&format!("Changes    {} files, +{} / -{}\n", c.changed_files.len(), c.insertions, c.deletions));
    for f in c.changed_files.iter().take(30) {
        s.push_str(&format!("  {:<10} {}\n", f.change, f.path));
    }
    if !c.checks.is_empty() {
        s.push_str("Checks\n");
        for ch in &c.checks {
            s.push_str(&format!("  {ch}\n"));
        }
    }
    for p in &c.policy {
        s.push_str(&format!("Policy     {} {} — {}\n", p.decision.upper(), p.subject, p.reason));
    }
    s.push_str(&format!("\napprove: herdr-orchestrator approval approve {}\ndeny:    herdr-orchestrator approval deny {}\n", a.approval_id, a.approval_id));
    s
}

pub fn audit_line(e: &AuditEvent) -> String {
    let mut data = e.data.to_string();
    if data.len() > 160 {
        let mut cut = 160;
        while !data.is_char_boundary(cut) {
            cut -= 1;
        }
        data.truncate(cut);
        data.push('…');
    }
    format!(
        "{} {:>4} {:<22} {:<12} {:<12} {}",
        e.timestamp.format("%Y-%m-%d %H:%M:%S"),
        e.seq,
        e.event,
        e.actor.runner.clone().or(e.actor.user.clone()).map(|x| format!("{}:{x}", e.actor.kind)).unwrap_or(e.actor.kind.clone()),
        e.step_id.clone().unwrap_or_default(),
        data
    )
}

pub const SAMPLE_PROJECT_CONFIG: &str = r#"# herdr-orchestrator project configuration.
# Precedence: built-in defaults → global config → this file → workflow
# defaults → task options → CLI flags. See docs/ARCHITECTURE.md §16.
version: 1

project:
  name: my-project

defaults:
  workflow: implement-review
  runner: claude
  # base_branch: main

scheduler:
  max_parallel_runs: 4
  max_parallel_agents: 8

limits:
  max_agents_per_run: 4
  max_runtime: 2h
  max_retries: 3
  agent_timeout: 45m
  command_timeout: 15m
  # Advisory only; enforced solely when a runner reports usage.
  # max_cost_usd: 10
  # max_tokens: 2000000

# Named checks used by `type: check` steps (`check: tests`). Omit to
# auto-detect; set to [] to disable.
# checks:
#   tests: ["cargo", "test", "--all"]
#   lint: ["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]
#   security: ["cargo", "audit"]

git:
  worktree_root: .herdr-orchestrator/worktrees
  branch_prefix: herdr/
  auto_commit: true

policy:
  # The built-in conservative policy is always included unless disabled.
  builtin_default: true
  files:
    - .ai/herdr-orchestrator/policy.yaml

github:
  draft_pr: true
  auto_merge: false

# Child processes get only these variables (plus HERDR_ORCH_*).
environment:
  inherit: [PATH, HOME, USER, LANG, "LC_*", TERM, TMPDIR, SSH_AUTH_SOCK]

# runners:
#   claude:
#     args: ["--permission-mode", "acceptEdits"]
#   aider:
#     mode: shell
#     command: ["aider", "--yes-always", "--message-file", "{{prompt_file}}"]
"#;
