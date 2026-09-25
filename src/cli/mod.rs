//! Command-line interface. Also serves every Herdr plugin entrypoint
//! (`hook …`, `ui`), so one audited binary backs the whole manifest.

mod doctor;
mod hooks;
pub mod render;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::config::{Paths, PLUGIN_ID};
use crate::engine::{self, EngineCtx, NewTask};
use crate::herdr::{HerdrApi, SocketHerdr};
use crate::model::*;
use crate::policies::{Action, Subject};

#[derive(Parser, Debug)]
#[command(name = "herdr-orchestrator", version, about = "Governed multi-agent workflow orchestration for Herdr", long_about = None)]
pub struct Cli {
    /// Show what would happen without doing it (policy is still evaluated).
    #[arg(long, global = true)]
    pub dry_run: bool,
    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    pub json: bool,
    /// Repository (default: Herdr context, then the current directory).
    #[arg(long, global = true)]
    pub repo: Option<PathBuf>,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Create, inspect and manage tasks.
    #[command(subcommand)]
    Task(TaskCmd),
    /// Inspect and control runs.
    #[command(subcommand)]
    Run(RunCmd),
    /// Human approvals.
    #[command(subcommand, alias = "approvals")]
    Approval(ApprovalCmd),
    /// Workflows.
    #[command(subcommand)]
    Workflow(WorkflowCmd),
    /// Policies.
    #[command(subcommand)]
    Policy(PolicyCmd),
    /// Audit trail.
    #[command(subcommand)]
    Audit(AuditCmd),
    /// Pause or resume the queue.
    #[command(subcommand)]
    Queue(QueueCmd),
    /// Engine daemon.
    #[command(subcommand)]
    Daemon(DaemonCmd),
    /// Show configuration.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// List skills.
    Skills,
    /// List runners.
    Runners,
    /// Architecture Decision Records in this repository.
    #[command(subcommand)]
    Adr(AdrCmd),
    /// ADR → plan → accepted tasks → conformance review.
    #[command(subcommand)]
    Epic(EpicCmd),
    /// Remove worktrees of finished runs that are merged, closed or superseded.
    Gc {
        /// Remove them (default: only list).
        #[arg(long)]
        yes: bool,
        /// Also ask GitHub whether PRs were merged or closed.
        #[arg(long)]
        check_prs: bool,
        /// Also failed/cancelled runs finished more than --days ago.
        #[arg(long)]
        failed: bool,
        #[arg(long, default_value_t = 7)]
        days: i64,
    },
    /// Outcomes per implementing runner and workflow (local data only).
    Stats,
    /// Token usage per task (agents in panes are read from their own local
    /// session logs; `~` marks estimates).
    Usage {
        /// Only this task.
        #[arg(long)]
        task: Option<String>,
        /// Include finished tasks older than the last 20.
        #[arg(long)]
        all: bool,
    },
    /// Show what a task would do (alias of `--dry-run task create`).
    Plan(PlanArgs),
    /// Check the environment.
    Doctor(doctor::DoctorArgs),
    /// Orchestrator TUI (the Herdr plugin pane).
    Ui(UiArgs),
    /// Plugin entrypoints used by herdr-plugin.toml.
    #[command(subcommand, hide = true)]
    Hook(hooks::HookCmd),
}

#[derive(Args, Debug, Clone)]
pub struct TaskOpts {
    /// Workflow name or path to a workflow YAML.
    #[arg(long, short)]
    pub workflow: Option<String>,
    /// Runner for every agent step (overrides workflow defaults).
    #[arg(long, short)]
    pub runner: Option<String>,
    /// Runner for one step: `--step-runner review=claude` (repeatable).
    #[arg(long = "step-runner", value_parser = parse_kv)]
    pub step_runner: Vec<(String, String)>,
    /// Base branch or ref (default: config, then the current branch).
    #[arg(long)]
    pub base: Option<String>,
    /// Number of independent variants (tournament mode).
    #[arg(long, default_value_t = 1)]
    pub variants: u32,
    /// Implementer runner per variant, comma separated: `codex,codex,claude`.
    #[arg(long, value_delimiter = ',')]
    pub variant_runners: Vec<String>,
    /// Paths the task may change (glob, repeatable). Changes elsewhere need
    /// your approval: `--scope 'src/webhooks/**' --scope 'tests/**'`.
    #[arg(long)]
    pub scope: Vec<String>,
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=').map(|(a, b)| (a.to_string(), b.to_string())).ok_or_else(|| format!("expected STEP=RUNNER, got {s:?}"))
}

#[derive(Args, Debug)]
pub struct PlanArgs {
    /// Task text.
    pub text: Vec<String>,
    #[command(flatten)]
    pub opts: TaskOpts,
}

#[derive(Subcommand, Debug)]
pub enum TaskCmd {
    /// Queue a new task (text from args, or stdin with `-`).
    Create {
        /// Task description. Use `-` to read from stdin.
        text: Vec<String>,
        #[arg(long)]
        title: Option<String>,
        /// Create the task from a GitHub issue number (via `gh`).
        #[arg(long)]
        from_issue: Option<u64>,
        #[command(flatten)]
        opts: TaskOpts,
        /// Do not start the daemon (the task stays queued).
        #[arg(long)]
        no_start: bool,
        /// Run in this process until the task finishes (no daemon; CI).
        #[arg(long)]
        foreground: bool,
    },
    /// List tasks.
    List {
        #[arg(long)]
        all: bool,
    },
    /// Show a task and its runs.
    Show { task: String },
    /// Compare variants side by side.
    Compare { task: String },
    /// Mark a variant as the chosen one (explicit, human decision).
    Select { task: String, run: String },
    /// Cancel all runs of a task.
    Cancel {
        task: String,
        #[arg(long, default_value = "cancelled by user")]
        reason: String,
    },
    /// Start a task now even though its dependencies are not done.
    Unblock { task: String },
}

#[derive(Subcommand, Debug)]
pub enum AdrCmd {
    /// List ADRs with their status and epic.
    List {
        /// Include superseded, rejected and deprecated ADRs.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum EpicCmd {
    /// Plan an ADR (or every active ADR without an epic in a directory).
    Create {
        #[arg(long)]
        from: PathBuf,
        /// Runner for the planning agent.
        #[arg(long, short)]
        runner: Option<String>,
        /// Do not start the daemon.
        #[arg(long)]
        no_start: bool,
    },
    List,
    Show { epic: String },
    /// Accept the plan (or some tasks): they are queued with dependencies.
    Accept {
        epic: String,
        /// Only these task keys, comma separated.
        #[arg(long, value_delimiter = ',')]
        only: Option<Vec<String>>,
        /// Runner for every agent step of the created tasks.
        #[arg(long, short)]
        runner: Option<String>,
        /// Runner for one step: `--step-runner acceptance=claude` (repeatable).
        #[arg(long = "step-runner", value_parser = parse_kv)]
        step_runner: Vec<(String, String)>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Decline the plan, or some of its tasks.
    Reject {
        epic: String,
        #[arg(long, value_delimiter = ',')]
        only: Option<Vec<String>>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Plan again (with the current ADR text) taking your feedback into account.
    Replan {
        epic: String,
        #[arg(long)]
        feedback: Option<String>,
    },
    /// Edit the plan in $EDITOR (YAML); it is validated on save.
    Edit { epic: String },
    /// Check the combined result against the ADR; gaps become proposed follow-ups.
    Verify { epic: String },
}

#[derive(Subcommand, Debug)]
pub enum RunCmd {
    /// Ensure the daemon is running so queued tasks start.
    Start {
        /// Optional task id (just for feedback).
        task: Option<String>,
        /// Drive queued work in this process until it finishes.
        #[arg(long)]
        foreground: bool,
    },
    List {
        /// Only non-terminal runs.
        #[arg(long)]
        active: bool,
    },
    Show { run: String },
    Cancel {
        run: String,
        #[arg(long, default_value = "cancelled by user")]
        reason: String,
    },
    /// Resume a failed/blocked/cancelled/needs_human run.
    Retry {
        run: String,
        /// Restart from this step (default: the step that stopped).
        #[arg(long)]
        from_step: Option<String>,
    },
    /// Pause at the next step boundary.
    Pause { run: String },
    Resume { run: String },
    /// Diff of the run's worktree against its base.
    Diff { run: String },
    /// Logs of a step (default: the latest step).
    Logs {
        run: String,
        #[arg(long)]
        step: Option<String>,
        #[arg(long)]
        tail: Option<usize>,
    },
    /// Focus the agent pane of a run in Herdr.
    Focus {
        run: String,
        #[arg(long)]
        step: Option<String>,
    },
    /// Close the Herdr panes/workspace of a finished run (files stay).
    Close {
        /// Run reference; omit with --finished.
        run: Option<String>,
        /// All succeeded, failed and cancelled runs.
        #[arg(long)]
        finished: bool,
    },
    /// Open a draft PR for a finished run (e.g. the selected variant).
    Pr {
        run: String,
        #[arg(long)]
        ready: bool,
    },
    /// Queue a follow-up on the run's PR from its review comments and
    /// failing checks (same branch, same PR).
    Followup {
        run: String,
        /// Extra instructions for the agent.
        #[arg(long)]
        note: Option<String>,
        /// Runner for every agent step (default: the original run's).
        #[arg(long, short)]
        runner: Option<String>,
        #[arg(long)]
        no_start: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ApprovalCmd {
    List {
        #[arg(long)]
        all: bool,
    },
    Show { approval: String },
    /// Approve once.
    Approve {
        approval: String,
        #[arg(long)]
        note: Option<String>,
    },
    Deny {
        approval: String,
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorkflowCmd {
    List,
    Show { name: String },
    /// Validate workflow files or names.
    Validate { files: Vec<String> },
}

#[derive(Subcommand, Debug)]
pub enum PolicyCmd {
    /// Evaluate a hypothetical action against the effective policy.
    Check {
        /// A command line (split on whitespace; quote-aware).
        #[arg(long)]
        command: Option<String>,
        /// Treat the command as a shell script (`shell: true`).
        #[arg(long)]
        shell: bool,
        /// A file path (worktree-relative).
        #[arg(long)]
        path: Option<String>,
        /// Action for --path (write, delete, read) or a standalone action
        /// (git_push, github_pr, agent_start…).
        #[arg(long)]
        action: Option<String>,
        #[arg(long)]
        runner: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        lines: Option<u64>,
    },
    /// Validate policy files (default: the effective set).
    Validate { files: Vec<PathBuf> },
    /// Show the effective policy sources and rules.
    Show,
}

#[derive(Subcommand, Debug)]
pub enum AuditCmd {
    /// Show a run's audit events (`global` for task-level events).
    Show {
        run: String,
        #[arg(long)]
        tail: Option<usize>,
    },
    /// Verify the hash chain of one run (or `--all`).
    Verify {
        run: Option<String>,
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum QueueCmd {
    Pause,
    Resume,
    Status,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Run in the foreground.
    Run {
        /// Never exit when idle.
        #[arg(long)]
        no_idle_exit: bool,
    },
    /// Start detached if not running.
    Ensure,
    Stop,
    Status,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Effective configuration (after all layers).
    Show {
        #[arg(long)]
        layers: bool,
    },
    /// Print config/state locations.
    Paths,
    /// Write a commented project config to .ai/herdr-orchestrator/config.yaml.
    Init {
        #[arg(long)]
        force: bool,
    },
}

#[derive(Args, Debug)]
pub struct UiArgs {
    /// Open straight into the new-task form.
    #[arg(long)]
    pub new_task: bool,
    /// Initial view: dashboard, approvals.
    #[arg(long)]
    pub view: Option<String>,
}

// ------------------------------------------------------------------ context

pub struct App {
    pub cli_json: bool,
    pub dry_run: bool,
    pub repo_arg: Option<PathBuf>,
    pub paths: Paths,
}

impl App {
    /// Repository in scope: `--repo`, Herdr invocation context, then cwd.
    pub fn repo(&self) -> Result<PathBuf> {
        let candidate = self
            .repo_arg
            .clone()
            .or_else(|| crate::herdr::InvocationContext::from_env().and_then(|c| c.repo_hint()))
            .unwrap_or(std::env::current_dir()?);
        let repo = crate::git::main_repo_root(&candidate).with_context(|| format!("{} is not inside a git repository (use --repo)", candidate.display()))?;
        // Plugin commands run with the plugin checkout as cwd; never mistake
        // the plugin's own source for the user's project.
        if let Some(root) = &self.paths.plugin_root {
            let same = |a: &std::path::Path, b: &std::path::Path| a.canonicalize().ok() == b.canonicalize().ok();
            if self.repo_arg.is_none() && same(&repo, root) {
                bail!("no project repository in context (open the orchestrator from a repository workspace, or pass --repo)");
            }
        }
        Ok(repo)
    }

    pub fn repo_opt(&self) -> Option<PathBuf> {
        self.repo().ok()
    }

    /// Engine context. Herdr is connected when reachable (and not disabled).
    pub fn ctx(&self, with_herdr: bool) -> Result<EngineCtx> {
        let cfg = crate::config::load(&self.paths, self.repo_opt().as_deref(), None)?;
        let herdr: Option<Arc<dyn HerdrApi>> = if with_herdr && cfg.config.herdr.mode != crate::config::HerdrMode::Disabled {
            SocketHerdr::discover(cfg.config.herdr.socket.as_deref())
                .filter(|h| h.ping().is_ok())
                .map(|h| Arc::new(h) as Arc<dyn HerdrApi>)
        } else {
            None
        };
        let mut ctx = EngineCtx::new(self.paths.clone(), herdr, cfg.config.scheduler.max_parallel_agents)?;
        ctx.audit.hash_chain = cfg.config.audit.hash_chain;
        Ok(ctx)
    }

    pub fn print_json<T: serde::Serialize + ?Sized>(&self, v: &T) -> Result<()> {
        println!("{}", serde_json::to_string_pretty(v)?);
        Ok(())
    }

    fn ensure_daemon(&self) -> Result<()> {
        let layout = crate::store::StateLayout::new(&self.paths.state_dir);
        let mut env = vec![];
        for k in ["HERDR_SOCKET_PATH", "HERDR_PLUGIN_ID", "HERDR_PLUGIN_ROOT", "HERDR_PLUGIN_CONFIG_DIR", "HERDR_PLUGIN_STATE_DIR", "HERDR_BIN_PATH"] {
            if let Ok(v) = std::env::var(k) {
                env.push((k, v));
            }
        }
        env.push(("HERDR_ORCH_CONFIG_DIR", self.paths.config_dir.display().to_string()));
        let started = crate::daemon::ensure(&layout, &env)?;
        crate::daemon::nudge(&layout);
        if started && !self.cli_json {
            eprintln!("started herdr-orchestrator daemon (log: {})", layout.daemon_log().display());
        }
        Ok(())
    }
}

fn user() -> Option<String> {
    std::env::var("USER").ok()
}

fn read_text(parts: &[String]) -> Result<String> {
    if parts.len() == 1 && parts[0] == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        return Ok(s);
    }
    Ok(parts.join(" "))
}

fn task_options(o: &TaskOpts, dry_run: bool) -> TaskOptions {
    TaskOptions {
        workflow: o.workflow.clone(),
        runner: o.runner.clone(),
        base_ref: o.base.clone(),
        variants: o.variants,
        variant_runners: o.variant_runners.clone(),
        step_runners: o.step_runner.iter().cloned().collect::<BTreeMap<_, _>>(),
        dry_run,
        scope: o.scope.clone(),
        ..Default::default()
    }
}

// ------------------------------------------------------------------ main

pub fn main() -> Result<i32> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;
    let app = App { cli_json: cli.json, dry_run: cli.dry_run, repo_arg: cli.repo.clone(), paths };
    match cli.cmd {
        Cmd::Task(c) => task_cmd(&app, c),
        Cmd::Run(c) => run_cmd(&app, c),
        Cmd::Approval(c) => approval_cmd(&app, c),
        Cmd::Workflow(c) => workflow_cmd(&app, c),
        Cmd::Policy(c) => policy_cmd(&app, c),
        Cmd::Audit(c) => audit_cmd(&app, c),
        Cmd::Queue(c) => queue_cmd(&app, c),
        Cmd::Daemon(c) => daemon_cmd(&app, c),
        Cmd::Config(c) => config_cmd(&app, c),
        Cmd::Skills => {
            let cat = app.ctx(false)?.catalog(app.repo_opt().as_deref());
            for (n, o) in cat.skills() {
                println!("{n:<20} {o}");
            }
            Ok(0)
        }
        Cmd::Runners => runners_cmd(&app),
        Cmd::Usage { task, all } => usage_cmd(&app, task, all),
        Cmd::Adr(c) => adr_cmd(&app, c),
        Cmd::Epic(c) => epic_cmd(&app, c),
        Cmd::Gc { yes, check_prs, failed, days } => gc_cmd(&app, yes, check_prs, failed, days),
        Cmd::Stats => stats_cmd(&app),
        Cmd::Plan(p) => plan_cmd(&app, &read_text(&p.text)?, &p.opts),
        Cmd::Doctor(a) => doctor::run(&app, a),
        Cmd::Ui(a) => {
            crate::ui::run(&app, a.new_task, a.view.as_deref())?;
            Ok(0)
        }
        Cmd::Hook(h) => hooks::run(&app, h),
    }
}

fn plan_cmd(app: &App, text: &str, o: &TaskOpts) -> Result<i32> {
    // Connect to Herdr (read-only ping) so the plan shows pane vs headless
    // exactly as a real run would choose.
    let ctx = app.ctx(true)?;
    let repo = app.repo()?;
    let title = text.lines().next().unwrap_or("task");
    let lines = engine::plan::plan(&ctx, &repo, title, o.workflow.as_deref(), o.runner.as_deref(), o.base.as_deref(), o.variants)?;
    if app.cli_json {
        app.print_json(&lines)?;
    } else {
        println!("DRY RUN — nothing will be created or executed\n");
        for l in lines {
            let step = l.step.map(|s| format!("[{s}] ")).unwrap_or_default();
            println!("{step}would {}", l.would);
            if let Some(p) = l.policy {
                println!("    policy: {p}");
            }
            if let Some(n) = l.note {
                println!("    note:   {n}");
            }
        }
    }
    Ok(0)
}

fn task_cmd(app: &App, c: TaskCmd) -> Result<i32> {
    match c {
        TaskCmd::Create { text, title, from_issue, opts, no_start, foreground } => {
            let ctx = app.ctx(foreground)?;
            let repo = app.repo()?;
            let (mut body, mut source, mut ttl) = (read_text(&text)?, None, title);
            if let Some(n) = from_issue {
                let issue = ctx.gh.issue_view(&repo, n)?;
                // Issue text is untrusted input; it is passed as the task
                // description, never interpreted by the orchestrator.
                body = format!("GitHub issue #{}: {}\n\n{}\n\n{}\n{}", issue.number, issue.title, issue.body.trim(), issue.url, if body.trim().is_empty() { String::new() } else { format!("\nAdditional instructions:\n{body}") });
                ttl = ttl.or(Some(issue.title.clone()));
                let repo_slug = crate::git::remote_url(&repo, "origin").unwrap_or_default();
                source = Some(TaskSource::GithubIssue { repo: repo_slug, number: issue.number, url: issue.url });
            }
            if app.dry_run {
                return plan_cmd(app, &body, &opts);
            }
            let task = engine::create_task(&ctx, NewTask { text: body, title: ttl, repo, options: task_options(&opts, false), via: "cli".into(), source, ..Default::default() })?;
            if app.cli_json {
                app.print_json(&task)?;
            } else {
                println!("queued task #{} — {} (workflow {})", task.task_id, task.title, task.options.workflow.clone().unwrap_or_default());
            }
            if foreground {
                return foreground_until(app, Arc::new(ctx), Some(&task.task_id));
            }
            if !no_start {
                app.ensure_daemon()?;
            }
            Ok(0)
        }
        TaskCmd::List { all } => {
            let ctx = app.ctx(false)?;
            let tasks: Vec<Task> = ctx.store.list_tasks()?.into_iter().filter(|t| all || !t.status.is_terminal()).collect();
            if app.cli_json {
                return app.print_json(&tasks).map(|_| 0);
            }
            if tasks.is_empty() {
                println!("no {}tasks", if all { "" } else { "open " });
            }
            for t in tasks {
                let runs: Vec<Run> = t.run_ids.iter().filter_map(|id| ctx.store.load_run(id).ok()).collect();
                let tokens = crate::telemetry::compact_tokens(&crate::telemetry::runs_agent_usage(runs.iter()));
                println!("#{:<5} {:<18} {:<18} {:>7}  {}", t.task_id, t.status.as_str(), t.options.workflow.clone().unwrap_or_default(), tokens, t.title);
            }
            Ok(0)
        }
        TaskCmd::Show { task } => {
            let ctx = app.ctx(false)?;
            let t = ctx.store.load_task(task.trim_start_matches('#'))?;
            let runs: Vec<Run> = t.run_ids.iter().filter_map(|id| ctx.store.load_run(id).ok()).collect();
            if app.cli_json {
                return app.print_json(&serde_json::json!({"task": t, "runs": runs})).map(|_| 0);
            }
            println!("Task #{} — {}\nStatus: {}\nRepo: {}\nWorkflow: {}\nCreated: {} by {}\n", t.task_id, t.title, t.status.as_str(), t.repo_root.display(), t.options.workflow.clone().unwrap_or_default(), t.created_at.format("%Y-%m-%d %H:%M"), t.initiator.user.clone().unwrap_or_default());
            println!("{}\n", t.description.trim());
            for r in &runs {
                println!("{}", render::run_line(r));
            }
            let u = crate::telemetry::runs_agent_usage(runs.iter());
            println!("\nTokens: {}", crate::telemetry::tokens_display(&u));
            Ok(0)
        }
        TaskCmd::Compare { task } => {
            let ctx = app.ctx(false)?;
            let t = ctx.store.load_task(task.trim_start_matches('#'))?;
            let runs: Vec<Run> = t.run_ids.iter().filter_map(|id| ctx.store.load_run(id).ok()).collect();
            let rows: Vec<render::CompareRow> = runs.iter().map(render::compare_row).collect();
            if app.cli_json {
                return app.print_json(&rows).map(|_| 0);
            }
            print!("{}", render::compare_table(&rows, t.selected_run.as_deref()));
            println!("\nNo winner is chosen automatically. Select one with: herdr-orchestrator task select {} <run>", t.task_id);
            Ok(0)
        }
        TaskCmd::Select { task, run } => {
            let ctx = app.ctx(false)?;
            let r = engine::handoff::select_variant(&ctx, task.trim_start_matches('#'), &run, user())?;
            println!("selected {} ({}) for task #{}; open a PR with: herdr-orchestrator run pr {}", r.display_name(), r.git.branch.clone().unwrap_or_default(), r.task_id, r.run_id);
            Ok(0)
        }
        TaskCmd::Cancel { task, reason } => {
            let ctx = app.ctx(false)?;
            let t = ctx.store.load_task(task.trim_start_matches('#'))?;
            if t.run_ids.is_empty() && t.status == TaskStatus::Queued {
                ctx.store.update_task(&t.task_id, |t| {
                    t.status = TaskStatus::Cancelled;
                    Ok(())
                })?;
            }
            for id in &t.run_ids {
                if let Err(e) = engine::request_cancel(&ctx, id, &reason, user()) {
                    eprintln!("{id}: {e}");
                }
            }
            crate::daemon::nudge(&ctx.store.layout);
            println!("cancellation requested for task #{}", t.task_id);
            Ok(0)
        }
        TaskCmd::Unblock { task } => {
            let ctx = app.ctx(false)?;
            let t = crate::epic::engine::unblock(&ctx, task.trim_start_matches('#'), user())?;
            crate::daemon::nudge(&ctx.store.layout);
            println!("task #{} will start without waiting for its dependencies", t.task_id);
            Ok(0)
        }
    }
}

/// Drive queued work in-process until `task` (or everything) settles.
fn foreground_until(app: &App, ctx: Arc<EngineCtx>, task: Option<&str>) -> Result<i32> {
    let global = ctx.load_config(None)?;
    let mut sched = engine::scheduler::Scheduler::new(ctx.clone(), global.config.scheduler.max_parallel_runs);
    let reports = crate::recovery::recover_all(&ctx)?;
    for r in reports {
        eprintln!("recovery {}: {:?} — {}", r.display, r.classification, r.reason);
    }
    let mut last: BTreeMap<String, String> = BTreeMap::new();
    loop {
        sched.tick()?;
        let runs: Vec<Run> = ctx.store.list_runs()?.into_iter().filter(|r| task.is_none_or(|t| r.task_id == t)).collect();
        for r in &runs {
            let line = render::run_line(r);
            if last.get(&r.run_id) != Some(&line) {
                if !app.cli_json {
                    eprintln!("{line}");
                }
                last.insert(r.run_id.clone(), line);
            }
        }
        let settled = match task {
            Some(t) => {
                let tk = ctx.store.load_task(t)?;
                !tk.run_ids.is_empty() && runs.iter().all(|r| !r.status.is_active())
            }
            None => sched.is_idle()?,
        };
        if settled && sched.active.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let runs: Vec<Run> = ctx.store.list_runs()?.into_iter().filter(|r| task.is_none_or(|t| r.task_id == t)).collect();
    if app.cli_json {
        app.print_json(&runs)?;
    }
    Ok(if runs.iter().all(|r| r.status == RunStatus::Succeeded) { 0 } else { 1 })
}

fn run_cmd(app: &App, c: RunCmd) -> Result<i32> {
    match c {
        RunCmd::Start { task, foreground } => {
            if foreground {
                let ctx = Arc::new(app.ctx(true)?);
                return foreground_until(app, ctx, task.as_deref().map(|t| t.trim_start_matches('#')));
            }
            app.ensure_daemon()?;
            println!("daemon running; queued tasks will start (see `herdr-orchestrator run list`)");
            Ok(0)
        }
        RunCmd::List { active } => {
            let ctx = app.ctx(false)?;
            let runs: Vec<Run> = ctx.store.list_runs()?.into_iter().filter(|r| !active || !r.status.is_terminal()).collect();
            if app.cli_json {
                return app.print_json(&runs).map(|_| 0);
            }
            if runs.is_empty() {
                println!("no runs");
            }
            for r in runs {
                println!("{}", render::run_line(&r));
            }
            Ok(0)
        }
        RunCmd::Show { run } => {
            let ctx = app.ctx(false)?;
            let r = ctx.store.resolve_run(&run)?;
            if app.cli_json {
                return app.print_json(&r).map(|_| 0);
            }
            let task = ctx.store.load_task(&r.task_id)?;
            print!("{}", render::run_detail(&r, &task));
            Ok(0)
        }
        RunCmd::Cancel { run, reason } => {
            let ctx = app.ctx(false)?;
            let r = ctx.store.resolve_run(&run)?;
            engine::request_cancel(&ctx, &r.run_id, &reason, user())?;
            crate::daemon::nudge(&ctx.store.layout);
            println!("cancellation requested for {} ({})", r.display_name(), r.run_id);
            Ok(0)
        }
        RunCmd::Retry { run, from_step } => {
            let ctx = app.ctx(false)?;
            let r = ctx.store.resolve_run(&run)?;
            engine::request_retry(&ctx, &r.run_id, from_step, user())?;
            app.ensure_daemon()?;
            println!("retry requested for {}", r.display_name());
            Ok(0)
        }
        RunCmd::Pause { run } => {
            let ctx = app.ctx(false)?;
            let r = ctx.store.resolve_run(&run)?;
            ctx.store.update_control(&r.run_id, |c| c.pause_requested = true)?;
            println!("{} will pause at the next step boundary", r.display_name());
            Ok(0)
        }
        RunCmd::Resume { run } => {
            let ctx = app.ctx(false)?;
            let r = ctx.store.resolve_run(&run)?;
            ctx.store.update_control(&r.run_id, |c| c.pause_requested = false)?;
            println!("{} resumed", r.display_name());
            Ok(0)
        }
        RunCmd::Diff { run } => {
            let ctx = app.ctx(false)?;
            let r = ctx.store.resolve_run(&run)?;
            let (Some(w), Some(b)) = (&r.git.worktree_path, &r.git.base_sha) else { bail!("run has no worktree yet") };
            print!("{}", crate::git::diff_text(w, b, 2 * 1024 * 1024)?);
            Ok(0)
        }
        RunCmd::Logs { run, step, tail } => {
            let ctx = app.ctx(false)?;
            let r = ctx.store.resolve_run(&run)?;
            let execs: Vec<&StepExecution> = match &step {
                Some(s) => r.steps.iter().filter(|e| &e.step_id == s).collect(),
                None => r.steps.last().into_iter().collect(),
            };
            if execs.is_empty() {
                bail!("no executions{}", step.map(|s| format!(" for step {s}")).unwrap_or_default());
            }
            for e in execs {
                println!("===== {} attempt {} ({}) =====", e.step_id, e.attempt, e.status.as_str());
                let text = e.log_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).or_else(|| e.output_excerpt.clone()).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                let from = tail.map(|n| lines.len().saturating_sub(n)).unwrap_or(0);
                for l in &lines[from..] {
                    println!("{l}");
                }
            }
            Ok(0)
        }
        RunCmd::Focus { run, step } => {
            let ctx = app.ctx(true)?;
            let r = ctx.store.resolve_run(&run)?;
            let herdr = ctx.herdr.clone().context("Herdr is not reachable")?;
            let b = r
                .steps
                .iter()
                .rev()
                .filter(|e| step.as_ref().is_none_or(|s| &e.step_id == s))
                .find_map(|e| e.agent.clone().filter(|a| a.mode == "pane"))
                .context("no agent pane for this run")?;
            match (&b.agent_name, &b.pane_id) {
                (Some(n), _) if herdr.focus_agent(n).is_ok() => {}
                (_, Some(p)) => herdr.focus_pane(p)?,
                _ => bail!("agent pane is gone"),
            }
            Ok(0)
        }
        RunCmd::Close { run, finished } => {
            let ctx = app.ctx(true)?;
            let herdr = ctx.herdr.clone().context("Herdr is not reachable")?;
            let runs: Vec<Run> = match (&run, finished) {
                (Some(r), _) => vec![ctx.store.resolve_run(r)?],
                (None, true) => ctx.store.list_runs()?.into_iter().filter(|r| r.status.is_terminal()).collect(),
                (None, false) => bail!("give a run or --finished"),
            };
            for r in runs {
                if !r.status.is_terminal() {
                    eprintln!("{} is {}; cancel it first", r.display_name(), r.status.as_str());
                    continue;
                }
                let closed = engine::handoff::close_run_panes(&ctx, herdr.as_ref(), &r, crate::audit::Actor::human(user()));
                println!("{}: {}", r.display_name(), if closed.is_empty() { "nothing open".to_string() } else { closed.join(", ") });
            }
            Ok(0)
        }
        RunCmd::Pr { run, ready } => {
            let ctx = app.ctx(false)?;
            if app.dry_run {
                let r = ctx.store.resolve_run(&run)?;
                println!("would push {} and open a {}PR", r.git.branch.unwrap_or_default(), if ready { "" } else { "draft " });
                return Ok(0);
            }
            let url = engine::handoff::create_pr_for_run(&ctx, &run, !ready, user())?;
            println!("{url}");
            Ok(0)
        }
        RunCmd::Followup { run, note, runner, no_start } => {
            let ctx = app.ctx(false)?;
            let t = engine::followup::pr_followup(&ctx, &run, note, runner, "cli")?;
            println!("queued follow-up task #{} — {}", t.task_id, t.title);
            if !no_start {
                app.ensure_daemon()?;
            }
            Ok(0)
        }
    }
}

fn approval_cmd(app: &App, c: ApprovalCmd) -> Result<i32> {
    let ctx = app.ctx(false)?;
    match c {
        ApprovalCmd::List { all } => {
            let list: Vec<_> = ctx.store.list_approvals()?.into_iter().filter(|a| all || a.status == crate::approvals::ApprovalStatus::Pending).collect();
            if app.cli_json {
                return app.print_json(&list).map(|_| 0);
            }
            if list.is_empty() {
                println!("no {}approvals", if all { "" } else { "pending " });
            }
            for a in list {
                let run = ctx.store.load_run(&a.run_id).map(|r| r.display_name()).unwrap_or_default();
                println!("{:<16} {:<9} {:<6} {:<12} {}", a.approval_id, a.status.as_str(), run, a.step_id, a.reason);
            }
            Ok(0)
        }
        ApprovalCmd::Show { approval } => {
            let a = ctx.store.resolve_approval(&approval)?;
            if app.cli_json {
                return app.print_json(&a).map(|_| 0);
            }
            print!("{}", render::approval_detail(&a));
            Ok(0)
        }
        ApprovalCmd::Approve { approval, note } => decide(app, &ctx, &approval, true, note),
        ApprovalCmd::Deny { approval, note } => decide(app, &ctx, &approval, false, note),
    }
}

fn decide(app: &App, ctx: &EngineCtx, approval: &str, approve: bool, note: Option<String>) -> Result<i32> {
    let a = ctx.store.resolve_approval(approval)?;
    if app.dry_run {
        println!("would {} {}", if approve { "approve" } else { "deny" }, a.approval_id);
        return Ok(0);
    }
    let a = crate::approvals::decide(&ctx.store, &a.approval_id, approve, user(), note)?;
    crate::daemon::nudge(&ctx.store.layout);
    println!("{} {}", a.status.as_str(), a.approval_id);
    Ok(0)
}

fn workflow_cmd(app: &App, c: WorkflowCmd) -> Result<i32> {
    let ctx = app.ctx(false)?;
    let cat = ctx.catalog(app.repo_opt().as_deref());
    match c {
        WorkflowCmd::List => {
            for w in cat.workflows()? {
                let desc = crate::workflow::Workflow::parse(&w.yaml).map(|wf| wf.description.unwrap_or_default()).unwrap_or_else(|e| format!("INVALID: {e:#}"));
                println!("{:<20} {:<10} {}", w.name, w.origin.split(':').next().unwrap_or(""), desc);
            }
            Ok(0)
        }
        WorkflowCmd::Show { name } => {
            let (_, src) = cat.workflow(&name)?;
            println!("# {} ({})\n{}", src.name, src.origin, src.yaml);
            Ok(0)
        }
        WorkflowCmd::Validate { files } => {
            let mut bad = 0;
            let targets: Vec<String> = if files.is_empty() { cat.workflows()?.into_iter().map(|w| w.name).collect() } else { files };
            for f in targets {
                match cat.workflow(&f) {
                    Ok((wf, _)) => {
                        // Also check that every named runner resolves.
                        let cfg = ctx.load_config(app.repo_opt().as_deref())?;
                        let factory = engine::runner_factory(&ctx, &cfg);
                        let mut problems = vec![];
                        for s in &wf.steps {
                            if let Some(r) = s.runner(&wf) {
                                if let Err(e) = factory.profile(r) {
                                    problems.push(format!("step {}: {e:#}", s.id));
                                }
                            }
                            if let crate::workflow::StepSpec::Agent { skill: Some(sk), .. } = &s.spec {
                                if let Err(e) = cat.skill(sk) {
                                    problems.push(format!("step {}: {e:#}", s.id));
                                }
                            }
                        }
                        if problems.is_empty() {
                            println!("OK     {f} ({} steps)", wf.steps.len());
                        } else {
                            bad += 1;
                            println!("ERROR  {f}: {}", problems.join("; "));
                        }
                    }
                    Err(e) => {
                        bad += 1;
                        println!("ERROR  {f}: {e:#}");
                    }
                }
            }
            Ok(if bad > 0 { 1 } else { 0 })
        }
    }
}

fn policy_cmd(app: &App, c: PolicyCmd) -> Result<i32> {
    let ctx = app.ctx(false)?;
    let cfg = ctx.load_config(app.repo_opt().as_deref())?;
    match c {
        PolicyCmd::Check { command, shell, path, action, runner, branch, lines } => {
            let set = ctx.policy_for(&cfg)?;
            let act: Option<Action> = match &action {
                Some(a) => Some(serde_json::from_value(serde_json::Value::String(a.clone())).with_context(|| format!("unknown action {a:?}"))?),
                None => None,
            };
            let mut s = if let Some(c) = command {
                let argv = if shell { vec![c] } else { crate::policies::command::tokenize(&c) };
                let mut s = Subject::command(&argv, shell);
                if let Some(a) = act {
                    s.action = Some(a);
                }
                s
            } else if let Some(p) = path {
                let mut s = Subject::file(act.unwrap_or(Action::Write), &p);
                s.lines_changed = lines;
                s
            } else if let Some(a) = act {
                Subject { action: Some(a), ..Default::default() }
            } else {
                bail!("give --command, --path or --action");
            };
            s.runner = runner;
            s.branch = branch;
            let d = set.evaluate(&s);
            if app.cli_json {
                app.print_json(&serde_json::json!({"decision": d, "normalized": s.command}))?;
            } else {
                println!("{}  {}", d.decision.upper(), d.subject);
                println!("reason: {}", d.reason);
                if let Some(c) = &s.command {
                    println!("normalized: {}", c.all_texts().join(" | "));
                    let t = c.all_tags();
                    if !t.is_empty() {
                        println!("tags: {}", t.join(", "));
                    }
                }
                for m in &d.matched {
                    println!("  matched {} → {} ({})", m.rule_id, m.decision.as_str(), m.source);
                }
            }
            Ok(match d.decision {
                crate::policies::Decision::Allow => 0,
                crate::policies::Decision::RequireApproval => 2,
                crate::policies::Decision::Deny => 3,
            })
        }
        PolicyCmd::Validate { files } => {
            if files.is_empty() {
                let set = ctx.policy_for(&cfg)?;
                println!("OK  effective policy: {} rules from {}", set.rule_count(), set.sources.join(", "));
                return Ok(0);
            }
            let mut bad = 0;
            for f in files {
                let mut set = crate::policies::PolicySet::empty(crate::policies::Decision::Allow);
                match set.load_file(&f) {
                    Ok(()) => println!("OK     {} ({} rules)", f.display(), set.rule_count()),
                    Err(e) => {
                        bad += 1;
                        println!("ERROR  {}: {e:#}", f.display());
                    }
                }
            }
            Ok(if bad > 0 { 1 } else { 0 })
        }
        PolicyCmd::Show => {
            let set = ctx.policy_for(&cfg)?;
            println!("default decision: {}", set.default_decision.as_str());
            println!("sources: {}", set.sources.join(", "));
            println!("rules: {}", set.rule_count());
            if cfg.config.policy.builtin_default {
                println!("\n--- builtin:default ---\n{}", crate::policies::DEFAULT_POLICY_YAML);
            }
            for f in &cfg.policy_files {
                println!("\n--- {} ---\n{}", f.display(), std::fs::read_to_string(f).unwrap_or_default());
            }
            Ok(0)
        }
    }
}

fn audit_cmd(app: &App, c: AuditCmd) -> Result<i32> {
    let ctx = app.ctx(false)?;
    match c {
        AuditCmd::Show { run, tail } => {
            let id = if run == "global" { None } else { Some(ctx.store.resolve_run(&run)?.run_id) };
            let evs = ctx.audit.read(id.as_deref())?;
            let from = tail.map(|n| evs.len().saturating_sub(n)).unwrap_or(0);
            if app.cli_json {
                return app.print_json(&evs[from..]).map(|_| 0);
            }
            for e in &evs[from..] {
                println!("{}", render::audit_line(e));
            }
            Ok(0)
        }
        AuditCmd::Verify { run, all } => {
            let mut targets = vec![];
            if all {
                for e in std::fs::read_dir(ctx.store.layout.audit_dir())? {
                    let p = e?.path();
                    if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                        targets.push(p);
                    }
                }
                targets.sort();
            } else {
                let r = run.context("give a run id or --all")?;
                let id = if r == "global" { None } else { Some(ctx.store.resolve_run(&r)?.run_id) };
                targets.push(ctx.audit.path_for(id.as_deref()));
            }
            let mut failed = 0;
            for t in targets {
                let rep = crate::audit::verify_file(&t)?;
                if app.cli_json {
                    app.print_json(&rep)?;
                } else if rep.ok {
                    println!("OK       {} ({} events, {})", t.display(), rep.events, if rep.chained { "hash-chained" } else { "NOT chained" });
                } else {
                    println!("TAMPERED {} ({} events)", t.display(), rep.events);
                    for p in &rep.problems {
                        println!("         {p}");
                    }
                }
                if !rep.ok {
                    failed += 1;
                }
            }
            if !app.cli_json {
                println!("\nNote: hash chaining is tamper-evident, not a signature; it cannot prove who wrote the log.");
            }
            Ok(if failed > 0 { 1 } else { 0 })
        }
    }
}

fn queue_cmd(app: &App, c: QueueCmd) -> Result<i32> {
    let ctx = app.ctx(false)?;
    match c {
        QueueCmd::Pause | QueueCmd::Resume => {
            let paused = matches!(c, QueueCmd::Pause);
            ctx.store.save_scheduler(&crate::store::SchedulerState { paused })?;
            ctx.audit(crate::audit::EventDraft::new(if paused { "queue_paused" } else { "queue_resumed" }, crate::audit::Actor::human(user())));
            crate::daemon::nudge(&ctx.store.layout);
            println!("queue {}", if paused { "paused (running runs continue; no new runs start)" } else { "resumed" });
        }
        QueueCmd::Status => {
            let s = ctx.store.load_scheduler()?;
            let tasks = ctx.store.list_tasks()?;
            let runs = ctx.store.list_runs()?;
            let q = tasks.iter().filter(|t| t.status == TaskStatus::Queued).count();
            let active = runs.iter().filter(|r| r.status.is_active()).count();
            let appr = crate::approvals::pending(&ctx.store)?.len();
            let daemon = crate::daemon::is_running(&ctx.store.layout);
            if app.cli_json {
                app.print_json(&serde_json::json!({"paused": s.paused, "queued": q, "active": active, "approvals": appr, "daemon": daemon}))?;
            } else {
                println!("queue: {}   queued {}   active {}   approvals {}   daemon {}", if s.paused { "PAUSED" } else { "running" }, q, active, appr, if daemon { "up" } else { "down" });
            }
        }
    }
    Ok(0)
}

fn daemon_cmd(app: &App, c: DaemonCmd) -> Result<i32> {
    let layout = crate::store::StateLayout::new(&app.paths.state_dir);
    match c {
        DaemonCmd::Run { no_idle_exit } => {
            let ctx = Arc::new(app.ctx(true)?);
            let mut opts = crate::daemon::DaemonOptions::default();
            if no_idle_exit {
                opts.idle_exit = None;
            }
            crate::daemon::run(ctx, opts)?;
        }
        DaemonCmd::Ensure => {
            app.ensure_daemon()?;
            println!("daemon running");
        }
        DaemonCmd::Stop => {
            if crate::daemon::stop(&layout)? {
                println!("daemon stopping (active runs will be recovered on next start)");
            } else {
                println!("daemon not running");
            }
        }
        DaemonCmd::Status => match crate::daemon::send(&layout, serde_json::json!({"cmd": "status"}))? {
            Some(v) => println!("{}", if app.cli_json { v.to_string() } else { format!("daemon up (pid {}), agents {}/{}", v["pid"], v["agents_in_use"], v["agent_capacity"]) }),
            None => println!("daemon not running"),
        },
    }
    Ok(0)
}

fn config_cmd(app: &App, c: ConfigCmd) -> Result<i32> {
    match c {
        ConfigCmd::Show { layers } => {
            let l = crate::config::load(&app.paths, app.repo_opt().as_deref(), None)?;
            if layers {
                for layer in &l.layers {
                    println!("# layer: {} {}", layer.name, layer.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default());
                    println!("{}", serde_yaml_ng::to_string(&layer.value)?);
                }
            } else {
                println!("{}", serde_yaml_ng::to_string(&l.config)?);
            }
        }
        ConfigCmd::Paths => {
            println!("state:          {} ({})", app.paths.state_dir.display(), app.paths.state_source);
            println!("global config:  {}", app.paths.global_config_file().display());
            if let Some(r) = app.repo_opt() {
                println!("project config: {}", r.join(crate::config::PROJECT_DIR).join("config.yaml").display());
            }
            if let Some(p) = &app.paths.plugin_root {
                println!("plugin root:    {}", p.display());
            }
        }
        ConfigCmd::Init { force } => {
            let repo = app.repo()?;
            let p = repo.join(crate::config::PROJECT_DIR).join("config.yaml");
            if p.exists() && !force {
                bail!("{} exists (use --force)", p.display());
            }
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(&p, render::SAMPLE_PROJECT_CONFIG)?;
            println!("wrote {}", p.display());
            // The sample config references the project policy file.
            let pol = p.parent().unwrap().join("policy.yaml");
            if !pol.exists() {
                std::fs::write(&pol, "# Project policy, added to the built-in default (it can only make things stricter).\n# See docs/POLICY_ENGINE.md. Example:\n#\n# rules:\n#   - id: approve-payments\n#     match:\n#       paths: [\"src/payments/**\"]\n#     actions: [write, delete]\n#     decision: require_approval\nversion: 1\nrules: []\n")?;
                println!("wrote {}", pol.display());
            }
        }
    }
    Ok(0)
}

fn adr_cmd(app: &App, c: AdrCmd) -> Result<i32> {
    let ctx = app.ctx(false)?;
    let repo = app.repo()?;
    let cfg = ctx.load_config(Some(&repo))?;
    match c {
        AdrCmd::List { all } => {
            let _ = crate::epic::engine::sync(&ctx);
            let epics = ctx.store.list_epics()?;
            let adrs: Vec<_> = crate::epic::discover(&repo, &cfg.config.epic.adr_dirs).into_iter().filter(|a| all || !a.is_inactive()).collect();
            if app.cli_json {
                return app.print_json(&adrs).map(|_| 0);
            }
            if adrs.is_empty() {
                println!("no ADRs found in {} (set epic.adr_dirs)", cfg.config.epic.adr_dirs.join(", "));
            }
            for a in adrs {
                let e = epics.iter().rev().find(|e| e.adr.path == a.path && e.repo_root == repo);
                let epic = match e {
                    Some(e) => format!("{} {}{}", e.epic_id, e.status.as_str(), if e.adr.sha256 != a.sha256 { " · ADR changed since the plan" } else { "" }),
                    None => "-".into(),
                };
                println!("{:<48} {:<14} {:<36} {}", a.path, a.status.clone().unwrap_or_else(|| "?".into()), epic, a.title);
            }
        }
    }
    Ok(0)
}

pub(crate) fn epic_line(ctx: &EngineCtx, e: &crate::epic::Epic) -> String {
    let done = e.tasks.values().filter(|id| ctx.store.load_task(id).map(|t| t.status == TaskStatus::Succeeded).unwrap_or(false)).count();
    let runs: Vec<Run> = e.tasks.values().filter_map(|id| ctx.store.load_task(id).ok()).flat_map(|t| t.run_ids).filter_map(|id| ctx.store.load_run(&id).ok()).collect();
    let tokens = crate::telemetry::compact_tokens(&crate::telemetry::runs_agent_usage(runs.iter()));
    format!(
        "{:<4} {:<12} {:>2}/{:<2} done {:>3} open {:>7} tok  {}{}",
        e.epic_id,
        e.status.as_str(),
        done,
        e.tasks.len(),
        e.open_keys().len(),
        tokens,
        e.adr.title,
        if crate::epic::engine::adr_drifted(e) == Some(true) { "  (ADR changed since the plan)" } else { "" }
    )
}

pub(crate) fn epic_detail(ctx: &EngineCtx, e: &crate::epic::Epic) -> String {
    let mut s = format!("Epic {} — {}\nADR      {} ({})\nStatus   {}{}\n", e.epic_id, e.adr.title, e.adr.path, e.adr.status.clone().unwrap_or_else(|| "no status".into()), e.status.as_str(), e.status_reason.as_ref().map(|r| format!(" — {r}")).unwrap_or_default());
    if crate::epic::engine::adr_drifted(e) == Some(true) {
        s.push_str("Warning  the ADR changed since it was planned — `epic replan` proposes an updated plan\n");
    }
    if let Some(p) = &e.plan {
        if !p.decision_summary.is_empty() {
            s.push_str(&format!("Decision {}\n", p.decision_summary));
        }
        if !p.open_questions.is_empty() {
            s.push_str("\nOPEN QUESTIONS (answer them with `epic replan --feedback` or `epic edit`)\n");
            for q in &p.open_questions {
                s.push_str(&format!("  ? {q}\n"));
            }
        }
        s.push_str("\nTASKS\n");
        let order = crate::epic::topo_order(&p.tasks).unwrap_or_else(|_| p.tasks.iter().map(|t| t.key.clone()).collect());
        for k in order {
            let Some(t) = e.plan_task(&k) else { continue };
            let state = match e.tasks.get(&k) {
                Some(id) => ctx.store.load_task(id).map(|x| format!("#{id} {}{}", x.status.as_str(), x.waiting_on.map(|w| format!(" ({w})")).unwrap_or_default())).unwrap_or_else(|_| format!("#{id}")),
                None if e.declined.contains(&k) => "declined".into(),
                None => "proposed".into(),
            };
            s.push_str(&format!("  {:<4} {:<42} {}\n", t.key, t.title.chars().take(42).collect::<String>(), state));
            if !t.depends_on.is_empty() {
                s.push_str(&format!("       after: {}\n", t.depends_on.join(", ")));
            }
            for (i, a) in t.acceptance.iter().enumerate() {
                s.push_str(&format!("       {}. {a}\n", i + 1));
            }
            for c in &t.verification.commands {
                s.push_str(&format!("       runs: {}\n", crate::policies::command::display_argv(c)));
            }
            if !t.verification.checks.is_empty() {
                s.push_str(&format!("       checks: {}\n", t.verification.checks.join(", ")));
            }
            for m in &t.verification.manual {
                s.push_str(&format!("       manual: {m}\n"));
            }
            if !t.scope.is_empty() {
                s.push_str(&format!("       scope: {}\n", t.scope.join(", ")));
            }
        }
        if !p.out_of_scope.is_empty() {
            s.push_str(&format!("\nOut of scope: {}\n", p.out_of_scope.join("; ")));
        }
    }
    if let Some(c) = &e.conformance {
        s.push_str("\nCONFORMANCE\n");
        if let Some(err) = c.get("error").and_then(|x| x.as_str()) {
            s.push_str(&format!("  failed: {err}\n"));
        }
        for p in c["points"].as_array().into_iter().flatten() {
            s.push_str(&format!("  [{}] {}{}\n", p["status"].as_str().unwrap_or("?"), p["point"].as_str().unwrap_or(""), p["evidence"].as_str().filter(|x| !x.is_empty()).map(|x| format!(" — {x}")).unwrap_or_default()));
        }
    }
    let open = e.open_keys();
    s.push('\n');
    match e.status {
        crate::epic::EpicStatus::Proposed | crate::epic::EpicStatus::Accepted | crate::epic::EpicStatus::Done if !open.is_empty() => {
            s.push_str(&format!("Proposed and not decided: {}\n  accept:  herdr-orchestrator epic accept {} [--only {}]\n  decline: herdr-orchestrator epic reject {} [--only …]\n  change:  herdr-orchestrator epic replan {} --feedback \"…\"   or   epic edit {}\n", open.join(", "), e.epic_id, open.join(","), e.epic_id, e.epic_id, e.epic_id));
            s.push_str("Accepting approves the listed `runs:` commands as check steps (they are still policy-checked).\n");
        }
        crate::epic::EpicStatus::Done => s.push_str(&format!("All tasks finished. Check the result against the ADR: herdr-orchestrator epic verify {}\n", e.epic_id)),
        _ => {}
    }
    s
}

fn epic_cmd(app: &App, c: EpicCmd) -> Result<i32> {
    let ctx = app.ctx(false)?;
    let _ = crate::epic::engine::sync(&ctx);
    match c {
        EpicCmd::Create { from, runner, no_start } => {
            let repo = app.repo()?;
            let full = if from.is_absolute() { from.clone() } else { std::env::current_dir()?.join(&from) };
            let targets: Vec<PathBuf> = if full.is_dir() {
                let cfg = ctx.load_config(Some(&repo))?;
                let _ = cfg;
                let rel = full.strip_prefix(&repo).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| full.display().to_string());
                let existing: Vec<String> = ctx.store.list_epics()?.into_iter().filter(|e| e.status != crate::epic::EpicStatus::Rejected).map(|e| e.adr.path).collect();
                crate::epic::discover(&repo, &[rel]).into_iter().filter(|a| !a.is_inactive() && !existing.contains(&a.path)).map(|a| repo.join(a.path)).collect()
            } else {
                vec![full]
            };
            if targets.is_empty() {
                bail!("no active ADR without an epic in {}", from.display());
            }
            for t in targets {
                let e = crate::epic::engine::create_epic(&ctx, &repo, &t, runner.clone(), "cli")?;
                println!("epic {} — planning {} (task #{})", e.epic_id, e.adr.path, e.planning_tasks.last().cloned().unwrap_or_default());
            }
            if !no_start {
                app.ensure_daemon()?;
            }
            println!("when the plan is ready: herdr-orchestrator epic show <id>");
        }
        EpicCmd::List => {
            let epics = ctx.store.list_epics()?;
            if app.cli_json {
                return app.print_json(&epics).map(|_| 0);
            }
            if epics.is_empty() {
                println!("no epics — start one with: herdr-orchestrator epic create --from docs/adr/<file>.md");
            }
            for e in &epics {
                println!("{}", epic_line(&ctx, e));
            }
        }
        EpicCmd::Show { epic } => {
            let e = ctx.store.load_epic(&epic)?;
            if app.cli_json {
                return app.print_json(&e).map(|_| 0);
            }
            print!("{}", epic_detail(&ctx, &e));
        }
        EpicCmd::Accept { epic, only, runner, step_runner, base, note } => {
            let e = crate::epic::engine::accept(
                &ctx,
                &epic,
                crate::epic::engine::AcceptOptions { only, runner, step_runners: step_runner.into_iter().collect(), base_ref: base, note, user: user(), via: "cli".into() },
            )?;
            println!("epic {}: {} task(s) queued", e.epic_id, e.tasks.len());
            app.ensure_daemon()?;
        }
        EpicCmd::Reject { epic, only, note } => {
            let e = crate::epic::engine::reject(&ctx, &epic, only, note, user())?;
            println!("epic {} is {}; open: {}", e.epic_id, e.status.as_str(), e.open_keys().join(", "));
        }
        EpicCmd::Replan { epic, feedback } => {
            let e = crate::epic::engine::replan(&ctx, &epic, feedback, "cli")?;
            println!("epic {}: planning again (task #{})", e.epic_id, e.planning_tasks.last().cloned().unwrap_or_default());
            app.ensure_daemon()?;
        }
        EpicCmd::Edit { epic } => {
            let e = ctx.store.load_epic(&epic)?;
            let plan = e.plan.clone().context("epic has no plan yet")?;
            let dir = ctx.store.layout.cache_dir();
            std::fs::create_dir_all(&dir)?;
            let file = dir.join(format!("{}-plan.yaml", e.epic_id));
            std::fs::write(&file, format!("# Plan of epic {} ({}). Save and quit to apply; it is validated.\n{}", e.epic_id, e.adr.path, serde_yaml_ng::to_string(&plan)?))?;
            let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
            let mut parts = crate::policies::command::tokenize(&editor);
            if parts.is_empty() {
                bail!("set $EDITOR");
            }
            let prog = parts.remove(0);
            let st = std::process::Command::new(prog).args(parts).arg(&file).status()?;
            if !st.success() {
                bail!("editor exited with {st}; plan unchanged");
            }
            let e = crate::epic::engine::set_plan(&ctx, &e.epic_id, &std::fs::read_to_string(&file)?, user())?;
            println!("plan of epic {} updated ({} task(s))", e.epic_id, e.plan.map(|p| p.tasks.len()).unwrap_or(0));
        }
        EpicCmd::Verify { epic } => {
            let e = crate::epic::engine::verify(&ctx, &epic, "cli")?;
            println!("epic {}: conformance review queued (task #{})", e.epic_id, e.conformance_task.clone().unwrap_or_default());
            app.ensure_daemon()?;
        }
    }
    Ok(0)
}

fn gc_cmd(app: &App, yes: bool, check_prs: bool, failed: bool, days: i64) -> Result<i32> {
    let ctx = app.ctx(false)?;
    let list = engine::maintenance::gc_candidates(&ctx, &engine::maintenance::GcOptions { check_prs, include_failed: failed, older_than_days: days })?;
    if app.cli_json && !yes {
        return app.print_json(&list).map(|_| 0);
    }
    if list.is_empty() {
        println!("nothing to clean up");
        return Ok(0);
    }
    for c in &list {
        println!("{:<6} {:<40} {}", c.run, c.worktree.display(), c.reason);
    }
    if !yes {
        println!("\n{} worktree(s) can be removed (branches stay). Run again with --yes to remove them.", list.len());
        return Ok(0);
    }
    let mut bad = 0;
    for (run, res) in engine::maintenance::gc_remove(&ctx, &list, user()) {
        match res {
            Ok(()) => println!("removed worktree of {run}"),
            Err(e) => {
                bad += 1;
                println!("kept {run}: {e:#}");
            }
        }
    }
    Ok(if bad > 0 { 1 } else { 0 })
}

fn stats_cmd(app: &App) -> Result<i32> {
    let ctx = app.ctx(false)?;
    let runs = ctx.store.list_runs()?;
    let stats = engine::maintenance::runner_stats(&runs);
    if app.cli_json {
        return app.print_json(&stats).map(|_| 0);
    }
    if stats.is_empty() {
        println!("no finished runs yet");
        return Ok(0);
    }
    println!("{:<16} {:<18} {:>5} {:>8} {:>8} {:>9} {:>14} {:>9} {:>8}", "RUNNER", "WORKFLOW", "RUNS", "SUCCESS", "FAILED", "ATTEMPTS", "1ST REVIEW OK", "TOKENS", "MINUTES");
    for s in stats {
        println!(
            "{:<16} {:<18} {:>5} {:>7.0}% {:>8} {:>9.1} {:>14} {:>9} {:>8}",
            s.runner,
            s.workflow,
            s.runs,
            100.0 * s.succeeded as f64 / s.runs as f64,
            s.failed,
            s.avg_attempts,
            if s.reviewed > 0 { format!("{}/{}", s.first_review_approved, s.reviewed) } else { "-".into() },
            s.avg_tokens.map(|t| crate::telemetry::compact_tokens(&UsageRecord { source: UsageSource::Reported, input_tokens: Some(t), output_tokens: Some(0), ..Default::default() })).unwrap_or_else(|| "–".into()),
            s.avg_minutes.map(|m| format!("{m:.1}")).unwrap_or_else(|| "-".into())
        );
    }
    println!("\nLocal data only. ATTEMPTS = tries of the implementing step per run; TOKENS = average per run where known.");
    Ok(0)
}

fn usage_cmd(app: &App, task: Option<String>, all: bool) -> Result<i32> {
    let ctx = app.ctx(false)?;
    let mut tasks = ctx.store.list_tasks()?;
    if let Some(t) = &task {
        let id = t.trim_start_matches('#');
        tasks.retain(|x| x.task_id == id);
        if tasks.is_empty() {
            bail!("no task {t}");
        }
    } else if !all && tasks.len() > 20 {
        tasks.drain(..tasks.len() - 20);
    }
    let rows: Vec<render::UsageRow> = tasks
        .iter()
        .map(|t| {
            let runs: Vec<Run> = t.run_ids.iter().filter_map(|id| ctx.store.load_run(id).ok()).collect();
            render::usage_row(t, &runs)
        })
        .collect();
    if app.cli_json {
        return app.print_json(&rows).map(|_| 0);
    }
    print!("{}", render::usage_table(&rows));
    Ok(0)
}

fn runners_cmd(app: &App) -> Result<i32> {
    let ctx = app.ctx(false)?;
    let cfg = ctx.load_config(app.repo_opt().as_deref())?;
    let factory = engine::runner_factory(&ctx, &cfg);
    let mut names = crate::runners::profiles::builtin_names();
    names.extend(cfg.config.runners.keys().cloned());
    names.sort();
    names.dedup();
    for n in names {
        match factory.profile(&n) {
            Ok(p) => {
                let bin = p.kind.clone().filter(|_| p.mode == crate::runners::RunnerMode::Pane).or_else(|| p.headless_command.as_ref().and_then(|c| c.first().cloned()));
                let installed = match (&p.mode, &bin) {
                    (crate::runners::RunnerMode::Fake, _) => "built-in".to_string(),
                    (_, Some(b)) => if crate::process::which(b).is_some() { "installed".into() } else { format!("{b} not on PATH") },
                    _ => "-".into(),
                };
                println!("{:<24} {:<9} {:<10} {}", n, format!("{:?}", p.mode).to_lowercase(), p.kind.unwrap_or_default(), installed);
            }
            Err(e) => println!("{n:<24} ERROR {e:#}"),
        }
    }
    Ok(0)
}

/// Used by hooks and the UI.
pub fn open_plugin_pane(entry: &str, placement: &str, view: Option<&str>) -> Result<()> {
    let herdr = SocketHerdr::discover(None).context("Herdr socket not found")?;
    let mut env = BTreeMap::new();
    if let Some(v) = view {
        env.insert("HERDR_ORCH_VIEW".to_string(), v.to_string());
    }
    let id = std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| PLUGIN_ID.to_string());
    herdr.open_plugin_pane(&id, entry, placement, &env)?;
    Ok(())
}
