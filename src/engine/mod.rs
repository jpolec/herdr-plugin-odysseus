//! Orchestration engine: task intake, run creation, the durable run driver,
//! the scheduler and dry-run planning.

pub mod digest;
pub mod driver;
pub mod followup;
pub mod handoff;
pub mod maintenance;
pub mod receipt;
pub mod plan;
pub mod scheduler;
mod steps;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::audit::{Actor, AuditLog, EventDraft};
use crate::config::{self, LoadedConfig, Paths};
use crate::github::Gh;
use crate::herdr::HerdrApi;
use crate::model::*;
use crate::policies::{PolicyFile, PolicySet};
use crate::process::CancelToken;
use crate::store::Store;
use crate::workflow::catalog::Catalog;
use crate::workflow::Workflow;

/// Counting semaphore for the global `max_parallel_agents` budget. Waits
/// in short slices so cancellation is observed.
#[derive(Debug)]
pub struct Slots {
    cap: usize,
    used: Mutex<usize>,
    cv: Condvar,
}

pub struct SlotGuard<'a>(&'a Slots);

impl Slots {
    pub fn new(cap: usize) -> Self {
        Self { cap: cap.max(1), used: Mutex::new(0), cv: Condvar::new() }
    }
    pub fn acquire(&self, cancel: &CancelToken) -> Option<SlotGuard<'_>> {
        let mut used = self.used.lock().unwrap();
        loop {
            if cancel.is_cancelled() {
                return None;
            }
            if *used < self.cap {
                *used += 1;
                return Some(SlotGuard(self));
            }
            used = self.cv.wait_timeout(used, Duration::from_millis(200)).unwrap().0;
        }
    }
    pub fn in_use(&self) -> usize {
        *self.used.lock().unwrap()
    }
    pub fn capacity(&self) -> usize {
        self.cap
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        *self.0.used.lock().unwrap() -= 1;
        self.0.cv.notify_one();
    }
}

/// Shared engine services. Cheap to clone via `Arc`.
pub struct EngineCtx {
    pub paths: Paths,
    pub store: Store,
    pub audit: AuditLog,
    pub herdr: Option<Arc<dyn HerdrApi>>,
    pub gh: Gh,
    pub agent_slots: Slots,
    /// How often waiting loops poll durable state.
    pub poll: Duration,
    /// Overrides merged on top of every loaded config (CLI flags, tests).
    pub config_overrides: Option<serde_json::Value>,
}

impl EngineCtx {
    pub fn new(paths: Paths, herdr: Option<Arc<dyn HerdrApi>>, max_agents: usize) -> Result<Self> {
        let store = Store::open(&paths.state_dir)?;
        let audit = AuditLog::new(store.layout.clone(), true);
        Ok(Self {
            paths,
            store,
            audit,
            herdr,
            gh: Gh::from_env(),
            agent_slots: Slots::new(max_agents),
            poll: Duration::from_millis(500),
            config_overrides: None,
        })
    }

    pub fn load_config(&self, repo: Option<&Path>) -> Result<LoadedConfig> {
        config::load(&self.paths, repo, self.config_overrides.clone())
    }

    pub fn catalog(&self, repo: Option<&Path>) -> Catalog {
        Catalog::new(&self.paths.config_dir, repo)
    }

    /// Build the effective policy set for a repository's config.
    pub fn policy_for(&self, cfg: &LoadedConfig) -> Result<PolicySet> {
        let mut set = PolicySet::empty(crate::policies::Decision::Allow);
        if cfg.config.policy.builtin_default {
            set.add(PolicyFile::parse(crate::policies::DEFAULT_POLICY_YAML)?, "builtin:default")?;
        }
        for f in &cfg.policy_files {
            set.load_file(f).with_context(|| format!("loading policy {}", f.display()))?;
        }
        if set.rule_count() == 0 && set.sources.is_empty() {
            tracing::warn!("no policy loaded; everything falls to default_decision=allow");
        }
        Ok(set)
    }

    pub fn audit(&self, d: EventDraft) {
        if let Err(e) = self.audit.append(d) {
            tracing::error!("audit append failed: {e:#}");
        }
    }

    pub fn notify(&self, enabled: bool, title: &str, body: &str, urgent: bool) {
        if !enabled {
            return;
        }
        if let Some(h) = &self.herdr {
            if let Err(e) = h.notify(title, body, urgent) {
                tracing::debug!("notification failed: {e}");
            }
        }
    }
}

/// Parameters for creating a task.
#[derive(Debug, Clone, Default)]
pub struct NewTask {
    pub text: String,
    pub title: Option<String>,
    pub repo: PathBuf,
    pub options: TaskOptions,
    pub via: String,
    pub source: Option<TaskSource>,
    pub epic: Option<EpicLink>,
    pub depends_on: Vec<String>,
    pub acceptance: Vec<String>,
    pub manual_checks: Vec<String>,
}

fn derive_title(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("task").trim();
    let mut t: String = first.chars().take(80).collect();
    if first.chars().count() > 80 {
        t.push('…');
    }
    t
}

/// Validate and enqueue a task. Runs are created by the scheduler when the
/// task is claimed, so queue order is task order.
pub fn create_task(ctx: &EngineCtx, nt: NewTask) -> Result<Task> {
    if nt.text.trim().is_empty() {
        bail!("task text is empty");
    }
    if nt.text.len() > 64 * 1024 {
        bail!("task text is larger than 64 KiB");
    }
    let repo = crate::git::main_repo_root(&nt.repo)
        .with_context(|| format!("{} is not a git repository", nt.repo.display()))?;
    let cfg = ctx.load_config(Some(&repo))?;
    let wf_name = nt.options.workflow.clone().unwrap_or_else(|| cfg.config.defaults.workflow.clone());
    let (wf, _) = ctx.catalog(Some(&repo)).workflow(&wf_name)?;
    let variants = nt.options.variants.max(1);
    if variants > cfg.config.limits.max_variants {
        bail!("{variants} variants requested; limits.max_variants is {}", cfg.config.limits.max_variants);
    }
    if !nt.options.variant_runners.is_empty() && nt.options.variant_runners.len() != variants as usize {
        bail!("--variant-runners lists {} runners for {variants} variants", nt.options.variant_runners.len());
    }
    // Validate every runner the task could use, early.
    let factory = runner_factory(ctx, &cfg);
    let mut to_check: Vec<String> = nt.options.variant_runners.clone();
    to_check.extend(nt.options.runner.clone());
    to_check.extend(nt.options.step_runners.values().cloned());
    for s in &wf.steps {
        if let Some(r) = s.runner(&wf) {
            to_check.push(r.to_string());
        }
    }
    to_check.push(cfg.config.defaults.runner.clone());
    for r in to_check {
        factory.profile(&r).with_context(|| format!("runner {r}"))?;
    }
    for id in nt.options.step_runners.keys() {
        if wf.step_index(id).is_none() {
            bail!("--step-runner names unknown step {id:?} in workflow {}", wf.name);
        }
    }
    if let Some(b) = &nt.options.base_ref {
        crate::git::rev_parse(&repo, b)?;
    }
    for g in &nt.options.scope {
        globset::Glob::new(g).with_context(|| format!("invalid --scope glob {g:?}"))?;
    }
    let n = ctx.store.next_task_number()?;
    let now = now();
    let task = Task {
        task_id: n.to_string(),
        title: nt.title.clone().unwrap_or_else(|| derive_title(&nt.text)),
        description: nt.text.clone(),
        repo_root: repo.clone(),
        status: TaskStatus::Queued,
        options: TaskOptions { workflow: Some(wf.name.clone()), variants, ..nt.options.clone() },
        initiator: Initiator::current(&nt.via),
        source: nt.source.clone(),
        run_ids: vec![],
        selected_run: None,
        created_at: now,
        updated_at: now,
        epic: nt.epic.clone(),
        depends_on: nt.depends_on.clone(),
        acceptance: nt.acceptance.clone(),
        manual_checks: nt.manual_checks.clone(),
        waiting_on: None,
    };
    ctx.store.save_task(&task)?;
    ctx.audit(
        EventDraft::new("task_created", Actor::human(task.initiator.user.clone()))
            .task(&task.task_id)
            .data(serde_json::json!({
                "title": task.title,
                "repo": repo,
                "workflow": wf.name,
                "variants": variants,
                "via": nt.via,
                "source": task.source,
            })),
    );
    ctx.audit(EventDraft::new("task_queued", Actor::orchestrator()).task(&task.task_id));
    Ok(task)
}

pub fn runner_factory(ctx: &EngineCtx, cfg: &LoadedConfig) -> crate::runners::RunnerFactory {
    crate::runners::RunnerFactory {
        herdr: match cfg.config.herdr.mode {
            crate::config::HerdrMode::Disabled => None,
            _ => ctx.herdr.clone(),
        },
        herdr_required: cfg.config.herdr.mode == crate::config::HerdrMode::Required,
        overrides: cfg.config.runners.clone(),
        interrupt_on_timeout: cfg.config.herdr.interrupt_on_timeout,
        pane_read_lines: cfg.config.output.pane_read_lines,
        settle_window: cfg.config.herdr.settle_window.as_duration(),
        watchdog: cfg.config.guard.watchdog.clone(),
    }
}

/// Create the run documents for a claimed task (one per variant).
pub fn create_runs(ctx: &EngineCtx, task: &Task) -> Result<Vec<Run>> {
    let cfg = ctx.load_config(Some(&task.repo_root))?;
    let wf_name = task.options.workflow.clone().unwrap_or_else(|| cfg.config.defaults.workflow.clone());
    let (wf, mut src) = ctx.catalog(Some(&task.repo_root)).workflow(&wf_name)?;
    // Verification the human accepted with an epic plan: extra check steps.
    let wf = match augment_with_checks(&wf, &task.options.extra_checks, &task.options.extra_commands)? {
        Some(w) => {
            src.yaml = format!("# {} + verification from the accepted plan\n{}", wf.name, serde_yaml_ng::to_string(&w)?);
            Workflow::parse(&src.yaml).context("augmented workflow is invalid")?
        }
        None => wf,
    };
    let base_ref = crate::git::default_base(&task.repo_root, task.options.base_ref.as_deref().or(cfg.config.defaults.base_branch.as_deref()))?;
    // A follow-up continues an existing run's branch and worktree.
    let continued = match &task.options.continue_run {
        Some(id) => {
            let prev = ctx.store.load_run(id)?;
            if !prev.status.is_terminal() {
                bail!("run {} is still {}", prev.display_name(), prev.status.as_str());
            }
            let busy = ctx.store.list_runs()?.into_iter().find(|r| !r.status.is_terminal() && r.git.worktree_path.is_some() && r.git.worktree_path == prev.git.worktree_path);
            if let Some(b) = busy {
                bail!("worktree of {} is in use by {}", prev.display_name(), b.display_name());
            }
            Some(prev)
        }
        None => None,
    };
    let mut runs = vec![];
    let variants = task.options.variants.max(1);
    let first_agent = wf.steps.iter().find(|s| s.kind() == StepKind::Agent).map(|s| s.id.clone());
    for v in 0..variants {
        let now = now();
        let mut step_runners = task.options.step_runners.clone();
        if let (Some(r), Some(first)) = (task.options.variant_runners.get(v as usize), &first_agent) {
            step_runners.entry(first.clone()).or_insert_with(|| r.clone());
        }
        let run = Run {
            run_id: new_id("run"),
            task_id: task.task_id.clone(),
            variant_index: v,
            variant_count: variants,
            workflow_name: wf.name.clone(),
            workflow_sha256: Workflow::sha256(&src.yaml),
            workflow_yaml: src.yaml.clone(),
            runner_override: task.options.runner.clone(),
            repo_root: task.repo_root.clone(),
            git: match &continued {
                Some(p) => GitState {
                    base_ref: p.git.base_ref.clone(),
                    base_sha: p.git.base_sha.clone(),
                    branch: p.git.branch.clone(),
                    worktree_path: p.git.worktree_path.clone(),
                    ..Default::default()
                },
                None => GitState { base_ref: base_ref.clone(), ..Default::default() },
            },
            herdr: HerdrBinding::default(),
            status: RunStatus::Pending,
            status_reason: None,
            initiator: task.initiator.clone(),
            dry_run: task.options.dry_run,
            created_at: now,
            started_at: None,
            updated_at: now,
            completed_at: None,
            cursor: 0,
            steps: vec![],
            policy_decisions: vec![],
            approvals: vec![],
            artifacts: vec![],
            retry_counts: BTreeMap::new(),
            pr_url: None,
            diff_stat: None,
            pending_feedback: None,
            pending_feedback_step: None,
            outputs: BTreeMap::new(),
            approved_paths: BTreeMap::new(),
            approval_cover: None,
            step_runners,
            recovered: false,
            selected: false,
            pr_feedback_seen: None,
            contract: None,
        };
        ctx.store.save_run(&run)?;
        runs.push(run);
    }
    ctx.store.update_task(&task.task_id, |t| {
        t.run_ids.extend(runs.iter().map(|r| r.run_id.clone()));
        t.status = TaskStatus::Running;
        Ok(())
    })?;
    Ok(runs)
}

/// Insert `check` steps for extra named checks and commands before the
/// first reviewing agent step (or at the end). Failing checks go back to the
/// first agent step with feedback, like the workflow's own checks.
pub fn augment_with_checks(wf: &Workflow, checks: &[String], commands: &[Vec<String>]) -> Result<Option<Workflow>> {
    use crate::workflow::{AgentOutput, OnFailure, Step, StepSpec};
    let present: Vec<&str> = wf.steps.iter().filter_map(|s| s.named_check()).collect();
    let checks: Vec<&String> = checks.iter().filter(|c| !present.contains(&c.as_str())).collect();
    if checks.is_empty() && commands.is_empty() {
        return Ok(None);
    }
    let first_agent = wf.steps.iter().find(|s| s.kind() == StepKind::Agent).map(|s| s.id.clone());
    let at = wf
        .steps
        .iter()
        .enumerate()
        .skip_while(|(_, s)| s.kind() != StepKind::Agent)
        .skip(1)
        .find(|(_, s)| matches!(s.spec, StepSpec::Agent { output: AgentOutput::Review | AgentOutput::Acceptance, .. } | StepSpec::Approval { .. } | StepSpec::GithubPr { .. }))
        .map(|(i, _)| i)
        .unwrap_or(wf.steps.len());
    let on_failure = first_agent.map(|id| OnFailure { retry_step: id, max_attempts: 2, feedback: true });
    let mut new_steps = vec![];
    for (i, c) in checks.iter().enumerate() {
        new_steps.push(Step {
            id: format!("plan-{c}-{}", i + 1),
            spec: StepSpec::Check { check: Some((*c).clone()), command: vec![], shell: false, env: Default::default(), cwd: None, contract: false },
            timeout: None,
            on_failure: on_failure.clone(),
            continue_on_failure: false,
            description: Some("verification from the accepted plan".into()),
        });
    }
    for (i, argv) in commands.iter().enumerate() {
        new_steps.push(Step {
            id: format!("plan-check-{}", i + 1),
            spec: StepSpec::Check { check: None, command: argv.clone(), shell: false, env: Default::default(), cwd: None, contract: false },
            timeout: None,
            on_failure: on_failure.clone(),
            continue_on_failure: false,
            description: Some("verification command from the accepted plan".into()),
        });
    }
    let mut out = wf.clone();
    out.steps.splice(at..at, new_steps);
    out.validate()?;
    Ok(Some(out))
}

/// Recompute a task's status from its runs.
pub fn refresh_task_status(ctx: &EngineCtx, task_id: &str) -> Result<Task> {
    let task = ctx.store.load_task(task_id)?;
    if task.run_ids.is_empty() {
        return Ok(task);
    }
    let runs: Vec<Run> = task.run_ids.iter().filter_map(|id| ctx.store.load_run(id).ok()).collect();
    let any = |f: &dyn Fn(RunStatus) -> bool| runs.iter().any(|r| f(r.status));
    let status = if any(&|s| s == RunStatus::AwaitingApproval) {
        TaskStatus::AwaitingApproval
    } else if any(&|s| matches!(s, RunStatus::Pending | RunStatus::Preparing | RunStatus::Running)) {
        TaskStatus::Running
    } else if any(&|s| matches!(s, RunStatus::Blocked | RunStatus::NeedsHuman)) {
        TaskStatus::Blocked
    } else if any(&|s| s == RunStatus::Succeeded) {
        TaskStatus::Succeeded
    } else if runs.iter().all(|r| r.status == RunStatus::Cancelled) {
        TaskStatus::Cancelled
    } else {
        TaskStatus::Failed
    };
    if status == task.status {
        return Ok(task);
    }
    ctx.store.update_task(task_id, |t| {
        t.status = status;
        Ok(())
    })
}

/// Request cancellation (durable; the scheduler and driver observe it).
pub fn request_cancel(ctx: &EngineCtx, run_id: &str, reason: &str, user: Option<String>) -> Result<()> {
    let run = ctx.store.load_run(run_id)?;
    if run.status.is_terminal() {
        bail!("run {} is already {}", run.display_name(), run.status.as_str());
    }
    ctx.store.update_control(run_id, |c| {
        c.cancel_requested = true;
        c.cancel_reason = Some(reason.to_string());
        c.requested_by = user.clone();
    })?;
    ctx.audit(
        EventDraft::new("cancel_requested", Actor::human(user))
            .run(run_id, &run.task_id)
            .data(serde_json::json!({"reason": reason})),
    );
    Ok(())
}

/// Human-initiated retry of a stopped run, optionally from a given step.
pub fn request_retry(ctx: &EngineCtx, run_id: &str, from_step: Option<String>, user: Option<String>) -> Result<()> {
    let run = ctx.store.load_run(run_id)?;
    if !run.status.can_resume() {
        bail!("run {} is {}; only failed, cancelled, blocked or needs_human runs can be retried", run.display_name(), run.status.as_str());
    }
    if let Some(s) = &from_step {
        let wf = Workflow::parse(&run.workflow_yaml)?;
        if wf.step_index(s).is_none() {
            bail!("workflow {} has no step {s:?}", wf.name);
        }
    }
    ctx.store.update_control(run_id, |c| {
        c.resume_requested = true;
        c.resume_from_step = from_step.clone();
        c.cancel_requested = false;
        c.requested_by = user.clone();
    })?;
    ctx.audit(
        EventDraft::new("retry_requested", Actor::human(user))
            .run(run_id, &run.task_id)
            .data(serde_json::json!({"from_step": from_step})),
    );
    Ok(())
}

/// Compose the full prompt sent to an agent.
#[allow(clippy::too_many_arguments)]
pub fn compose_prompt(
    skill: Option<&str>,
    body: &str,
    run: &Run,
    step_id: &str,
    attempt: u32,
    worktree: &Path,
    output_file: &Path,
    output: crate::workflow::AgentOutput,
) -> String {
    let mut p = String::new();
    if let Some(s) = skill {
        p.push_str(s.trim());
        p.push_str("\n\n---\n\n");
    }
    p.push_str(body.trim());
    p.push_str("\n\n---\n\n");
    p.push_str(&orchestrator_instructions(run, step_id, attempt, worktree, output_file, output));
    p
}

pub fn orchestrator_instructions(run: &Run, step_id: &str, attempt: u32, worktree: &Path, output_file: &Path, output: crate::workflow::AgentOutput) -> String {
    use crate::workflow::AgentOutput;
    let format = match output {
        AgentOutput::Review => {
            r#"{"verdict": "approved" | "changes_requested" | "rejected",
 "summary": "<one paragraph>",
 "findings": [{"severity": "critical|high|medium|low|info", "file": "<path>", "line": <number or null>,
               "description": "<what is wrong>", "recommendation": "<how to fix>"}]}"#
        }
        AgentOutput::Plan => {
            r#"{"decision_summary": "<the ADR's decision in two sentences>",
 "tasks": [{"key": "T1", "title": "<short imperative title>", "description": "<what to do and where>",
            "acceptance": ["<criterion a reviewer can check against code and tests>", "..."],
            "verification": {"checks": ["tests" | "lint" | "security"], "commands": [["<argv>", "..."]], "manual": ["<only what a human can check>"]},
            "depends_on": ["<keys of tasks that must land first>"], "adr_refs": ["<section of the ADR>"],
            "scope": ["<path globs this task will change, e.g. src/webhooks/**>"], "risk": "low" | "medium" | "high"}],
 "out_of_scope": ["<what the ADR defers or excludes>"],
 "open_questions": ["<questions a human must answer>"]}"#
        }
        AgentOutput::Acceptance => {
            r#"{"criteria": [{"index": <1-based number of the criterion>, "status": "met" | "not_met" | "unverifiable",
               "evidence": "<test name, file:line, or command and its result>"}],
 "verdict": "approved" | "changes_requested",
 "summary": "<one paragraph>"}"#
        }
        AgentOutput::Conformance => {
            r#"{"summary": "<one paragraph>",
 "points": [{"point": "<one statement from the ADR's Decision or Consequences>", "status": "covered" | "partial" | "missing",
             "evidence": "<files, tests or PRs that show it>"}],
 "followups": [<a task object in the same format as a plan task, for each gap; key F1, F2, ...>]}"#
        }
        AgentOutput::Contract => {
            r#"{"files": ["<test files you wrote, relative to the worktree>"],
 "check": ["<argv of the command that runs exactly these tests>", "..."],
 "criteria_map": {"1": ["<test name>"], "2": ["..."]},
 "summary": "<what the tests pin down>"}"#
        }
        AgentOutput::Summary => r#"{"summary": "<what you changed and why>", "status": "done" | "blocked", "notes": "<optional>"}"#,
    };
    format!(
        "Orchestrator instructions (herdr-orchestrator run {run} step `{step}`, attempt {attempt}):\n\
         - Working directory: {wt} (an isolated git worktree on branch {branch}). Stay inside it.\n\
         - Do not commit, push, open pull requests or rewrite git history; the orchestrator does that.\n\
         - When you are finished, write exactly one JSON object to this file (create directories as needed).\n\
           Use your file-writing tool for it, not a shell command, so no permission prompt is needed:\n\
         Output file: {out}\n\
         Format:\n{format}\n\
         - After writing the file, stop and wait.",
        run = run.display_name(),
        step = step_id,
        wt = worktree.display(),
        branch = run.git.branch.as_deref().unwrap_or("?"),
        out = output_file.display(),
    )
}

/// Validate a review agent's structured output.
pub fn parse_review(text: &str) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).context("review output is not valid JSON")?;
    let verdict = v.get("verdict").and_then(|x| x.as_str()).context("review output has no string `verdict`")?;
    if !matches!(verdict, "approved" | "changes_requested" | "rejected") {
        bail!("unknown review verdict {verdict:?}");
    }
    let findings = v.get("findings").and_then(|f| f.as_array()).context("review output has no `findings` array")?;
    for (i, f) in findings.iter().enumerate() {
        let sev = f.get("severity").and_then(|s| s.as_str()).with_context(|| format!("finding {i} has no severity"))?;
        if !matches!(sev, "critical" | "high" | "medium" | "low" | "info") {
            bail!("finding {i} has unknown severity {sev:?}");
        }
        if f.get("description").and_then(|s| s.as_str()).is_none() {
            bail!("finding {i} has no description");
        }
        if let Some(l) = f.get("line") {
            if !(l.is_null() || l.is_u64()) {
                bail!("finding {i} has a non-numeric line");
            }
        }
    }
    Ok(v)
}

pub fn wait_until<F: FnMut() -> Result<bool>>(poll: Duration, deadline: Option<Instant>, cancel: &CancelToken, mut f: F) -> Result<bool> {
    loop {
        if f()? {
            return Ok(true);
        }
        if cancel.is_cancelled() || deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(false);
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_validation() {
        assert!(parse_review(r#"{"verdict":"approved","findings":[]}"#).is_ok());
        assert!(parse_review(r#"{"verdict":"meh","findings":[]}"#).is_err());
        assert!(parse_review(r#"{"verdict":"approved"}"#).is_err());
        assert!(parse_review(r#"{"verdict":"changes_requested","findings":[{"severity":"high","description":"x","line":3}]}"#).is_ok());
        assert!(parse_review(r#"{"verdict":"changes_requested","findings":[{"severity":"urgent","description":"x"}]}"#).is_err());
        assert!(parse_review(r#"{"verdict":"changes_requested","findings":[{"severity":"low","description":"x","line":"3"}]}"#).is_err());
        assert!(parse_review("nope").is_err());
    }

    #[test]
    fn slots_limit_concurrency() {
        let s = Slots::new(1);
        let c = CancelToken::new();
        let g = s.acquire(&c).unwrap();
        assert_eq!(s.in_use(), 1);
        let c2 = CancelToken::new();
        c2.cancel();
        assert!(s.acquire(&c2).is_none());
        drop(g);
        assert!(s.acquire(&c).is_some());
    }

    #[test]
    fn titles() {
        assert_eq!(derive_title("\n  Fix pagination\nmore"), "Fix pagination");
        assert!(derive_title(&"x".repeat(200)).ends_with('…'));
    }
}
