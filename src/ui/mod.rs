//! Orchestrator TUI — the Herdr plugin pane.
//!
//! The UI never hosts the engine: it reads durable state every second and
//! writes decisions (approve, cancel, retry, new task) through the same
//! engine functions as the CLI, then nudges the daemon. Closing the pane
//! never affects running work.

mod view;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::approvals::{ApprovalRequest, ApprovalStatus};
use crate::cli::App as CliApp;
use crate::model::*;

pub use view::render;

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Dashboard,
    RunDetail(String),
    Approvals,
    ApprovalDetail(String),
    Text { title: String, lines: Vec<String>, scroll: usize },
    NewTask,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pending {
    CancelRun(String),
    Deny(String),
}

#[derive(Debug, Clone)]
pub struct Form {
    pub field: usize,
    pub text: String,
    pub workflows: Vec<String>,
    pub workflow: usize,
    pub runners: Vec<String>,
    pub runner: usize,
    pub base: String,
    pub variants: u32,
    pub repo: Option<PathBuf>,
}

impl Form {
    pub const FIELDS: usize = 6; // text, workflow, runner, base, variants, [create]
}

/// Snapshot of durable state plus UI selection.
pub struct State {
    pub screen: Screen,
    pub tasks: Vec<Task>,
    pub runs: Vec<Run>,
    pub approvals: Vec<ApprovalRequest>,
    pub selected: usize,
    pub message: Option<(String, Instant)>,
    pub confirm: Option<Pending>,
    pub form: Option<Form>,
    pub paused: bool,
    pub daemon_up: bool,
    pub popup_mode: bool,
}

impl State {
    pub fn new(screen: Screen) -> Self {
        Self {
            screen,
            tasks: vec![],
            runs: vec![],
            approvals: vec![],
            selected: 0,
            message: None,
            confirm: None,
            form: None,
            paused: false,
            daemon_up: false,
            popup_mode: false,
        }
    }

    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.task_id == id)
    }

    pub fn run(&self, id: &str) -> Option<&Run> {
        self.runs.iter().find(|r| r.run_id == id)
    }

    /// Runs shown on the dashboard, in display order (active first).
    pub fn dashboard_runs(&self) -> Vec<&Run> {
        let mut active: Vec<&Run> = self.runs.iter().filter(|r| !r.status.is_terminal()).collect();
        active.sort_by_key(|r| (r.task_id.parse::<u64>().unwrap_or(0), r.variant_index));
        let mut done: Vec<&Run> = self.runs.iter().filter(|r| r.status.is_terminal()).collect();
        done.sort_by_key(|r| std::cmp::Reverse(r.completed_at));
        active.extend(done.into_iter().take(8));
        active
    }

    pub fn pending_approvals(&self) -> Vec<&ApprovalRequest> {
        self.approvals.iter().filter(|a| a.status == ApprovalStatus::Pending).collect()
    }

    pub fn flash(&mut self, m: impl Into<String>) {
        self.message = Some((m.into(), Instant::now()));
    }

    fn list_len(&self) -> usize {
        match &self.screen {
            Screen::Dashboard => self.dashboard_runs().len(),
            Screen::Approvals => self.pending_approvals().len(),
            Screen::RunDetail(id) => self.run(id).map(|r| r.steps.len()).unwrap_or(0),
            _ => 0,
        }
    }

    fn clamp(&mut self) {
        let n = self.list_len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }
}

fn load(state: &mut State, ctx: &crate::engine::EngineCtx) {
    if let Ok(t) = ctx.store.list_tasks() {
        state.tasks = t;
    }
    if let Ok(r) = ctx.store.list_runs() {
        state.runs = r;
    }
    if let Ok(a) = ctx.store.list_approvals() {
        state.approvals = a;
    }
    state.paused = ctx.store.load_scheduler().map(|s| s.paused).unwrap_or(false);
    state.daemon_up = crate::daemon::is_running(&ctx.store.layout);
    state.clamp();
}

pub fn run(app: &CliApp, new_task: bool, view: Option<&str>) -> Result<()> {
    let ctx = app.ctx(false)?;
    let view = view.map(String::from).or_else(|| std::env::var("HERDR_ORCH_VIEW").ok());
    let screen = if new_task {
        Screen::NewTask
    } else if view.as_deref() == Some("approvals") {
        Screen::Approvals
    } else {
        Screen::Dashboard
    };
    let mut state = State::new(screen.clone());
    state.popup_mode = new_task;
    if new_task {
        state.form = Some(new_form(app, &ctx));
    }
    load(&mut state, &ctx);

    let mut terminal = ratatui::init();
    let res = event_loop(app, &ctx, &mut terminal, &mut state);
    ratatui::restore();
    res
}

fn new_form(app: &CliApp, ctx: &crate::engine::EngineCtx) -> Form {
    let repo = app.repo_opt();
    let cfg = ctx.load_config(repo.as_deref()).ok();
    let workflows: Vec<String> = ctx.catalog(repo.as_deref()).workflows().map(|w| w.into_iter().map(|x| x.name).collect()).unwrap_or_default();
    let default_wf = cfg.as_ref().map(|c| c.config.defaults.workflow.clone()).unwrap_or_default();
    let mut runners = vec!["(workflow default)".to_string(), "claude".into(), "codex".into(), "opencode".into(), "gemini".into()];
    if let Some(c) = &cfg {
        runners.extend(c.config.runners.keys().cloned());
    }
    runners.push("fake-success".into());
    let base = repo.as_deref().and_then(|r| crate::git::current_branch(r).ok().flatten()).or_else(|| cfg.as_ref().and_then(|c| c.config.defaults.base_branch.clone())).unwrap_or_else(|| "HEAD".into());
    Form {
        field: 0,
        text: String::new(),
        workflow: workflows.iter().position(|w| *w == default_wf).unwrap_or(0),
        workflows,
        runners,
        runner: 0,
        base,
        variants: 1,
        repo,
    }
}

fn event_loop(app: &CliApp, ctx: &crate::engine::EngineCtx, terminal: &mut ratatui::DefaultTerminal, state: &mut State) -> Result<()> {
    let mut last_load = Instant::now();
    loop {
        if state.message.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(6)) {
            state.message = None;
        }
        terminal.draw(|f| render(f, state))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press && handle_key(app, ctx, state, k)? {
                    return Ok(());
                }
            }
        }
        if last_load.elapsed() > Duration::from_secs(1) {
            load(state, ctx);
            last_load = Instant::now();
        }
    }
}

fn selected_run(state: &State) -> Option<Run> {
    match &state.screen {
        Screen::Dashboard => state.dashboard_runs().get(state.selected).map(|r| (*r).clone()),
        Screen::RunDetail(id) => state.run(id).cloned(),
        Screen::ApprovalDetail(id) => state.approvals.iter().find(|a| &a.approval_id == id).and_then(|a| state.run(&a.run_id)).cloned(),
        Screen::Approvals => state.pending_approvals().get(state.selected).and_then(|a| state.run(&a.run_id)).cloned(),
        _ => None,
    }
}

fn focus_agent(app: &CliApp, run: &Run, step: Option<&str>) -> Result<String> {
    let ctx = app.ctx(true)?;
    let Some(h) = ctx.herdr.clone() else { return Ok("Herdr is not reachable".into()) };
    let b = run.steps.iter().rev().filter(|e| step.is_none_or(|s| e.step_id == s)).find_map(|e| e.agent.clone().filter(|a| a.mode == "pane"));
    let Some(b) = b else { return Ok("no agent pane for this run".into()) };
    if let Some(n) = &b.agent_name {
        if h.focus_agent(n).is_ok() {
            return Ok(format!("focused {n}"));
        }
    }
    if let Some(p) = &b.pane_id {
        h.focus_pane(p)?;
        return Ok(format!("focused pane {p}"));
    }
    Ok("agent pane is gone".into())
}

fn diff_screen(run: &Run) -> Screen {
    let text = match (&run.git.worktree_path, &run.git.base_sha) {
        (Some(w), Some(b)) => crate::git::diff_text(w, b, 1024 * 1024).unwrap_or_else(|e| format!("{e:#}")),
        _ => "run has no worktree yet".into(),
    };
    Screen::Text { title: format!("diff {} {}", run.display_name(), run.git.branch.clone().unwrap_or_default()), lines: text.lines().map(String::from).collect(), scroll: 0 }
}

fn log_screen(run: &Run, idx: usize) -> Screen {
    let e = run.steps.get(idx).or(run.steps.last());
    let (title, text) = match e {
        Some(e) => (
            format!("log {} {} attempt {}", run.display_name(), e.step_id, e.attempt),
            e.log_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).or_else(|| e.output_excerpt.clone()).unwrap_or_else(|| "(no output)".into()),
        ),
        None => ("log".into(), "(no steps yet)".into()),
    };
    let lines: Vec<String> = text.lines().map(String::from).collect();
    let scroll = lines.len().saturating_sub(40);
    Screen::Text { title, lines, scroll }
}

/// Returns `true` to quit.
pub fn handle_key(app: &CliApp, ctx: &crate::engine::EngineCtx, state: &mut State, k: KeyEvent) -> Result<bool> {
    let user = std::env::var("USER").ok();
    // Confirmation modal.
    if let Some(p) = state.confirm.clone() {
        state.confirm = None;
        if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            match p {
                Pending::CancelRun(id) => match crate::engine::request_cancel(ctx, &id, "cancelled from orchestrator pane", user) {
                    Ok(()) => {
                        crate::daemon::nudge(&ctx.store.layout);
                        state.flash("cancellation requested");
                    }
                    Err(e) => state.flash(format!("{e:#}")),
                },
                Pending::Deny(id) => match crate::approvals::decide(&ctx.store, &id, false, user, None) {
                    Ok(_) => {
                        crate::daemon::nudge(&ctx.store.layout);
                        state.flash("denied");
                        state.screen = Screen::Approvals;
                    }
                    Err(e) => state.flash(format!("{e:#}")),
                },
            }
            load(state, ctx);
        } else {
            state.flash("aborted");
        }
        return Ok(false);
    }
    if state.screen == Screen::NewTask {
        return form_key(app, ctx, state, k);
    }
    let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
    if ctrl_c {
        return Ok(true);
    }
    // Text viewer.
    if let Screen::Text { lines, scroll, .. } = &mut state.screen {
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => *scroll = (*scroll + 1).min(lines.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => *scroll = (*scroll + 30).min(lines.len().saturating_sub(1)),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(30),
            KeyCode::Char('g') => *scroll = 0,
            KeyCode::Char('G') => *scroll = lines.len().saturating_sub(1),
            KeyCode::Esc | KeyCode::Char('q') => {
                state.screen = Screen::Dashboard;
                state.selected = 0;
            }
            _ => {}
        }
        return Ok(false);
    }
    match k.code {
        KeyCode::Down | KeyCode::Char('j') => {
            state.selected += 1;
            state.clamp();
        }
        KeyCode::Up | KeyCode::Char('k') => state.selected = state.selected.saturating_sub(1),
        KeyCode::Esc | KeyCode::Char('q') => match &state.screen {
            Screen::Dashboard => return Ok(true),
            Screen::ApprovalDetail(_) => {
                state.screen = Screen::Approvals;
                state.selected = 0;
            }
            _ => {
                state.screen = Screen::Dashboard;
                state.selected = 0;
            }
        },
        KeyCode::Enter => match &state.screen {
            Screen::Dashboard => {
                if let Some(r) = selected_run(state) {
                    state.screen = Screen::RunDetail(r.run_id);
                    state.selected = r.steps.len().saturating_sub(1);
                }
            }
            Screen::Approvals => {
                if let Some(a) = state.pending_approvals().get(state.selected) {
                    state.screen = Screen::ApprovalDetail(a.approval_id.clone());
                }
            }
            Screen::RunDetail(id) => {
                if let Some(r) = state.run(id).cloned() {
                    state.screen = log_screen(&r, state.selected);
                }
            }
            _ => {}
        },
        KeyCode::Char('n') if matches!(state.screen, Screen::Dashboard) => {
            state.form = Some(new_form(app, ctx));
            state.screen = Screen::NewTask;
        }
        KeyCode::Char('n') if matches!(state.screen, Screen::ApprovalDetail(_)) => {
            if let Screen::ApprovalDetail(id) = &state.screen {
                state.confirm = Some(Pending::Deny(id.clone()));
            }
        }
        KeyCode::Char('a') => {
            // From a run blocked on approval, jump straight to it.
            let target = selected_run(state).and_then(|r| state.pending_approvals().into_iter().find(|a| a.run_id == r.run_id).map(|a| a.approval_id.clone()));
            state.screen = match target {
                Some(id) if !matches!(state.screen, Screen::Dashboard) => Screen::ApprovalDetail(id),
                _ => Screen::Approvals,
            };
            state.selected = 0;
        }
        KeyCode::Char('y') => {
            if let Screen::ApprovalDetail(id) = &state.screen {
                let id = id.clone();
                match crate::approvals::decide(&ctx.store, &id, true, user, None) {
                    Ok(_) => {
                        crate::daemon::nudge(&ctx.store.layout);
                        state.flash("approved (this action only)");
                        state.screen = Screen::Approvals;
                    }
                    Err(e) => state.flash(format!("{e:#}")),
                }
                load(state, ctx);
            }
        }
        KeyCode::Char('x') | KeyCode::Char('c') if !matches!(state.screen, Screen::Dashboard) || k.code == KeyCode::Char('x') => {
            if let Some(r) = selected_run(state) {
                if r.status.is_terminal() {
                    state.flash(format!("{} is already {}", r.display_name(), r.status.as_str()));
                } else {
                    state.confirm = Some(Pending::CancelRun(r.run_id));
                }
            }
        }
        KeyCode::Char('r') => {
            if let Some(r) = selected_run(state) {
                match crate::engine::request_retry(ctx, &r.run_id, None, user) {
                    Ok(()) => {
                        let _ = crate::daemon::ensure(&ctx.store.layout, &[]);
                        crate::daemon::nudge(&ctx.store.layout);
                        state.flash(format!("retry requested for {}", r.display_name()));
                    }
                    Err(e) => state.flash(format!("{e:#}")),
                }
            }
        }
        KeyCode::Char('d') => {
            if let Some(r) = selected_run(state) {
                state.screen = diff_screen(&r);
            }
        }
        KeyCode::Char('l') => {
            if let Screen::RunDetail(id) = &state.screen {
                if let Some(r) = state.run(id).cloned() {
                    state.screen = log_screen(&r, state.selected);
                }
            }
        }
        KeyCode::Char('f') => {
            if let Some(r) = selected_run(state) {
                let step = match &state.screen {
                    Screen::RunDetail(_) => r.steps.get(state.selected).map(|e| e.step_id.clone()),
                    _ => None,
                };
                let m = focus_agent(app, &r, step.as_deref()).unwrap_or_else(|e| format!("{e:#}"));
                state.flash(m);
            }
        }
        KeyCode::Char('p') if matches!(state.screen, Screen::Dashboard) => {
            let paused = !state.paused;
            ctx.store.save_scheduler(&crate::store::SchedulerState { paused })?;
            crate::daemon::nudge(&ctx.store.layout);
            state.paused = paused;
            state.flash(if paused { "queue paused" } else { "queue resumed" });
        }
        _ => {}
    }
    Ok(false)
}

fn form_key(app: &CliApp, ctx: &crate::engine::EngineCtx, state: &mut State, k: KeyEvent) -> Result<bool> {
    let Some(f) = state.form.as_mut() else {
        state.screen = Screen::Dashboard;
        return Ok(false);
    };
    let submit = (k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('s')) || (f.field == Form::FIELDS - 1 && k.code == KeyCode::Enter);
    if submit {
        let Some(repo) = f.repo.clone() else {
            state.flash("no repository in context; open the pane from a repo workspace or use the CLI with --repo");
            return Ok(false);
        };
        if f.text.trim().is_empty() {
            state.flash("describe the task first");
            return Ok(false);
        }
        let runner = (f.runner > 0).then(|| f.runners[f.runner].clone());
        let opts = TaskOptions {
            workflow: f.workflows.get(f.workflow).cloned(),
            runner,
            base_ref: Some(f.base.clone()).filter(|b| !b.trim().is_empty()),
            variants: f.variants,
            ..Default::default()
        };
        let nt = crate::engine::NewTask { text: f.text.clone(), title: None, repo, options: opts, via: "ui".into(), source: None };
        match crate::engine::create_task(ctx, nt) {
            Ok(t) => {
                let _ = crate::daemon::ensure(&ctx.store.layout, &[]);
                crate::daemon::nudge(&ctx.store.layout);
                if state.popup_mode {
                    return Ok(true);
                }
                state.flash(format!("queued task #{}", t.task_id));
                state.screen = Screen::Dashboard;
                state.form = None;
                load(state, ctx);
            }
            Err(e) => state.flash(format!("{e:#}")),
        }
        let _ = app;
        return Ok(false);
    }
    match k.code {
        KeyCode::Esc => {
            if state.popup_mode {
                return Ok(true);
            }
            state.form = None;
            state.screen = Screen::Dashboard;
        }
        KeyCode::Tab | KeyCode::Down if !(f.field == 0 && k.code == KeyCode::Down) => f.field = (f.field + 1) % Form::FIELDS,
        KeyCode::BackTab | KeyCode::Up if !(f.field == 0 && k.code == KeyCode::Up) => f.field = (f.field + Form::FIELDS - 1) % Form::FIELDS,
        KeyCode::Left | KeyCode::Right => {
            let fwd = k.code == KeyCode::Right;
            let cycle = |i: usize, n: usize| if n == 0 { 0 } else if fwd { (i + 1) % n } else { (i + n - 1) % n };
            match f.field {
                1 => f.workflow = cycle(f.workflow, f.workflows.len()),
                2 => f.runner = cycle(f.runner, f.runners.len()),
                4 => f.variants = if fwd { (f.variants + 1).min(4) } else { f.variants.saturating_sub(1).max(1) },
                _ => {}
            }
        }
        KeyCode::Enter if f.field == 0 => f.text.push('\n'),
        KeyCode::Enter => f.field = (f.field + 1) % Form::FIELDS,
        KeyCode::Backspace => match f.field {
            0 => {
                f.text.pop();
            }
            3 => {
                f.base.pop();
            }
            _ => {}
        },
        KeyCode::Char(c) => match f.field {
            0 => f.text.push(c),
            3 => f.base.push(c),
            4 if c.is_ascii_digit() => f.variants = c.to_digit(10).unwrap().clamp(1, 4),
            _ => {}
        },
        _ => {}
    }
    Ok(false)
}
