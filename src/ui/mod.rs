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
    /// Live view of an agent that is waiting for a human; keys are forwarded.
    Agent(String),
    /// ADR epics.
    Epics,
    EpicDetail { id: String, scroll: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pending {
    CancelRun(String),
    Deny(String),
    RejectEpic(String),
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
    pub epics: Vec<crate::epic::Epic>,
    /// Pre-rendered lines of the epic shown on the EpicDetail screen.
    pub epic_lines: Vec<String>,
    /// Pre-rendered one-line summaries for the Epics screen.
    pub epic_rows: Vec<String>,
    pub selected: usize,
    pub message: Option<(String, Instant)>,
    pub confirm: Option<Pending>,
    pub form: Option<Form>,
    pub paused: bool,
    pub daemon_up: bool,
    pub popup_mode: bool,
    /// Running as a Herdr popup (modal): jumping to an agent closes it.
    pub in_popup: bool,
    /// Last lines of the agent pane shown on the Agent screen.
    pub agent_lines: Vec<String>,
}

impl State {
    pub fn new(screen: Screen) -> Self {
        Self {
            screen,
            tasks: vec![],
            runs: vec![],
            approvals: vec![],
            epics: vec![],
            epic_lines: vec![],
            epic_rows: vec![],
            selected: 0,
            message: None,
            confirm: None,
            form: None,
            paused: false,
            daemon_up: false,
            popup_mode: false,
            in_popup: false,
            agent_lines: vec![],
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

    /// The step of `run` that is waiting for a human in the agent's own UI.
    pub fn waiting_step<'a>(&self, run: &'a Run) -> Option<&'a StepExecution> {
        run.steps.iter().rev().find(|e| e.status == StepStatus::AwaitingHuman)
    }

    /// Runs whose agent is asking something right now.
    pub fn agents_waiting(&self) -> usize {
        self.runs.iter().filter(|r| self.waiting_step(r).is_some()).count()
    }

    pub fn flash(&mut self, m: impl Into<String>) {
        self.message = Some((m.into(), Instant::now()));
    }

    fn list_len(&self) -> usize {
        match &self.screen {
            Screen::Dashboard => self.dashboard_runs().len(),
            Screen::Approvals => self.pending_approvals().len(),
            Screen::RunDetail(id) => self.run(id).map(|r| r.steps.len()).unwrap_or(0),
            Screen::Epics => self.epics.len(),
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
    if matches!(state.screen, Screen::Epics | Screen::EpicDetail { .. }) {
        let _ = crate::epic::engine::sync(ctx);
        if let Ok(e) = ctx.store.list_epics() {
            state.epic_rows = e.iter().map(|x| crate::cli::epic_line(ctx, x)).collect();
            state.epics = e;
        }
        if let Screen::EpicDetail { id, .. } = &state.screen {
            state.epic_lines = match ctx.store.load_epic(id) {
                Ok(e) => crate::cli::epic_detail(ctx, &e).lines().map(String::from).collect(),
                Err(e) => vec![format!("{e:#}")],
            };
        }
    }
    state.paused = ctx.store.load_scheduler().map(|s| s.paused).unwrap_or(false);
    if let Screen::Agent(id) = &state.screen {
        state.agent_lines = state
            .run(id)
            .and_then(|r| state.waiting_step(r).or(r.steps.last()))
            .and_then(|e| e.agent.as_ref())
            .and_then(|a| a.pane_id.clone())
            .and_then(|p| {
                use crate::herdr::HerdrApi;
                crate::herdr::SocketHerdr::discover(None)?.read_screen(&p).ok()
            })
            .map(|t| t.lines().map(String::from).collect())
            .unwrap_or_else(|| vec!["(the agent pane is not reachable)".into()]);
    }
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
    // Herdr popups get no HERDR_PANE_ID (they are not panes).
    state.in_popup = std::env::var_os("HERDR_PLUGIN_ENTRYPOINT_ID").is_some() && std::env::var_os("HERDR_PANE_ID").is_none();
    if new_task {
        state.form = Some(new_form(app, &ctx));
    }
    load(&mut state, &ctx);

    let mut terminal = ratatui::init();
    let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::EnableBracketedPaste);
    let res = event_loop(app, &ctx, &mut terminal, &mut state);
    let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::DisableBracketedPaste);
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
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press && handle_key(app, ctx, state, k)? => return Ok(()),
                // Bracketed paste: keep newlines as newlines.
                Event::Paste(text) => {
                    if let (Screen::NewTask, Some(f)) = (&state.screen, state.form.as_mut()) {
                        match f.field {
                            0 => f.text.push_str(&text.replace("\r\n", "\n").replace('\r', "\n")),
                            3 => f.base.push_str(text.trim()),
                            _ => {}
                        }
                    }
                }
                _ => {}
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

/// Focus the run's agent pane. Returns (message, focused).
fn focus_agent(app: &CliApp, run: &Run, step: Option<&str>) -> Result<(String, bool)> {
    let ctx = app.ctx(true)?;
    let Some(h) = ctx.herdr.clone() else { return Ok(("Herdr is not reachable".into(), false)) };
    let b = run.steps.iter().rev().filter(|e| step.is_none_or(|s| e.step_id == s)).find_map(|e| e.agent.clone().filter(|a| a.mode == "pane"));
    let Some(b) = b else { return Ok(("no agent pane for this run".into(), false)) };
    if let Some(n) = &b.agent_name {
        if h.focus_agent(n).is_ok() {
            return Ok((format!("focused {n}"), true));
        }
    }
    if let Some(p) = &b.pane_id {
        h.focus_pane(p)?;
        return Ok((format!("focused pane {p}"), true));
    }
    Ok(("agent pane is gone".into(), false))
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
                Pending::RejectEpic(id) => match crate::epic::engine::reject(ctx, &id, None, Some("rejected from orchestrator pane".into()), user) {
                    Ok(e) => state.flash(format!("epic {} {}", e.epic_id, e.status.as_str())),
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
    if let Screen::Agent(id) = state.screen.clone() {
        return agent_key(app, state, &id, k, ctx);
    }
    if matches!(state.screen, Screen::Epics | Screen::EpicDetail { .. }) {
        return epic_key(ctx, state, k);
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
                    if state.waiting_step(&r).is_some() {
                        state.screen = Screen::Agent(r.run_id);
                        load(state, ctx);
                    } else {
                        state.screen = Screen::RunDetail(r.run_id);
                        state.selected = r.steps.len().saturating_sub(1);
                    }
                }
            }
            Screen::Approvals => {
                if let Some(a) = state.pending_approvals().get(state.selected) {
                    state.screen = Screen::ApprovalDetail(a.approval_id.clone());
                }
            }
            Screen::RunDetail(id) => {
                if let Some(r) = state.run(id).cloned() {
                    if r.steps.get(state.selected).is_some_and(|e| e.status == StepStatus::AwaitingHuman) {
                        state.screen = Screen::Agent(r.run_id);
                        load(state, ctx);
                    } else {
                        state.screen = log_screen(&r, state.selected);
                    }
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
                let (m, focused) = focus_agent(app, &r, step.as_deref()).unwrap_or_else(|e| (format!("{e:#}"), false));
                if focused && state.in_popup {
                    // A modal popup would sit on top of the agent: get out of the way.
                    return Ok(true);
                }
                state.flash(m);
            }
        }
        KeyCode::Char('e') if matches!(state.screen, Screen::Dashboard) => {
            state.screen = Screen::Epics;
            state.selected = 0;
            load(state, ctx);
        }
        KeyCode::Char('F') => {
            if let Some(r) = selected_run(state) {
                match crate::engine::followup::pr_followup(ctx, &r.run_id, None, None, "ui") {
                    Ok(t) => {
                        let _ = crate::daemon::ensure(&ctx.store.layout, &[]);
                        crate::daemon::nudge(&ctx.store.layout);
                        state.flash(format!("queued follow-up #{} for {}", t.task_id, r.display_name()));
                    }
                    Err(e) => state.flash(format!("{e:#}")),
                }
                load(state, ctx);
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

/// Epics list and detail: accept, reject, verify, re-plan.
fn epic_key(ctx: &crate::engine::EngineCtx, state: &mut State, k: KeyEvent) -> Result<bool> {
    let user = std::env::var("USER").ok();
    let current = match &state.screen {
        Screen::EpicDetail { id, .. } => Some(id.clone()),
        Screen::Epics => state.epics.get(state.selected).map(|e| e.epic_id.clone()),
        _ => None,
    };
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.screen = if matches!(state.screen, Screen::EpicDetail { .. }) { Screen::Epics } else { Screen::Dashboard };
            state.selected = 0;
        }
        KeyCode::Down | KeyCode::Char('j') => match &mut state.screen {
            Screen::EpicDetail { scroll, .. } => *scroll = (*scroll + 1).min(state.epic_lines.len().saturating_sub(1)),
            _ => {
                state.selected += 1;
                state.clamp();
            }
        },
        KeyCode::Up | KeyCode::Char('k') => match &mut state.screen {
            Screen::EpicDetail { scroll, .. } => *scroll = scroll.saturating_sub(1),
            _ => state.selected = state.selected.saturating_sub(1),
        },
        KeyCode::Enter if matches!(state.screen, Screen::Epics) => {
            if let Some(id) = current {
                state.screen = Screen::EpicDetail { id, scroll: 0 };
            }
        }
        KeyCode::Char('y') => {
            if let Some(id) = current {
                let o = crate::epic::engine::AcceptOptions { user, via: "ui".into(), ..Default::default() };
                match crate::epic::engine::accept(ctx, &id, o) {
                    Ok(e) => {
                        let _ = crate::daemon::ensure(&ctx.store.layout, &[]);
                        crate::daemon::nudge(&ctx.store.layout);
                        state.flash(format!("epic {}: {} task(s) queued", e.epic_id, e.tasks.len()));
                    }
                    Err(e) => state.flash(format!("{e:#}")),
                }
            }
        }
        KeyCode::Char('n') => {
            if let Some(id) = current {
                state.confirm = Some(Pending::RejectEpic(id));
            }
        }
        KeyCode::Char('v') => {
            if let Some(id) = current {
                match crate::epic::engine::verify(ctx, &id, "ui") {
                    Ok(e) => {
                        let _ = crate::daemon::ensure(&ctx.store.layout, &[]);
                        crate::daemon::nudge(&ctx.store.layout);
                        state.flash(format!("epic {}: conformance review queued", e.epic_id));
                    }
                    Err(e) => state.flash(format!("{e:#}")),
                }
            }
        }
        KeyCode::Char('g') => {
            if let Some(id) = current {
                match crate::epic::engine::replan(ctx, &id, None, "ui") {
                    Ok(e) => {
                        let _ = crate::daemon::ensure(&ctx.store.layout, &[]);
                        crate::daemon::nudge(&ctx.store.layout);
                        state.flash(format!("epic {}: planning again (feedback: `epic replan --feedback`)", e.epic_id));
                    }
                    Err(e) => state.flash(format!("{e:#}")),
                }
            }
        }
        _ => {}
    }
    load(state, ctx);
    Ok(false)
}

/// Keys on the Agent screen go straight to the waiting agent (digits,
/// y/n/a, arrows, Enter, Tab). `Esc` goes back, `f` opens the real pane.
fn agent_key(app: &CliApp, state: &mut State, run_id: &str, k: KeyEvent, ctx: &crate::engine::EngineCtx) -> Result<bool> {
    let Some(run) = state.run(run_id).cloned() else {
        state.screen = Screen::Dashboard;
        return Ok(false);
    };
    let target = state
        .waiting_step(&run)
        .or(run.steps.last())
        .and_then(|e| e.agent.as_ref())
        .and_then(|a| a.agent_name.clone().or_else(|| a.pane_id.clone()));
    let key: Option<String> = match k.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.screen = Screen::Dashboard;
            state.selected = 0;
            return Ok(false);
        }
        KeyCode::Char('f') => {
            let (m, focused) = focus_agent(app, &run, None).unwrap_or_else(|e| (format!("{e:#}"), false));
            if focused && state.in_popup {
                return Ok(true);
            }
            state.flash(m);
            return Ok(false);
        }
        KeyCode::Char(c) if c.is_ascii_digit() || matches!(c, 'y' | 'n' | 'a') => Some(c.to_string()),
        KeyCode::Up => Some("up".into()),
        KeyCode::Down => Some("down".into()),
        KeyCode::Enter => Some("enter".into()),
        KeyCode::Tab => Some("tab".into()),
        _ => None,
    };
    let (Some(key), Some(target)) = (key, target) else { return Ok(false) };
    use crate::herdr::HerdrApi;
    match crate::herdr::SocketHerdr::discover(None).map(|h| h.send_keys(&target, &[key.as_str()])) {
        Some(Ok(())) => {
            // Record the human's answer: it changes what the agent does next.
            ctx.audit(
                crate::audit::EventDraft::new("human_input_sent", crate::audit::Actor::human(std::env::var("USER").ok()))
                    .run(&run.run_id, &run.task_id)
                    .data(serde_json::json!({"key": key, "agent": target})),
            );
            state.flash(format!("sent `{key}` to the agent"));
            std::thread::sleep(std::time::Duration::from_millis(300));
            load(state, ctx);
        }
        Some(Err(e)) => state.flash(format!("could not send: {e}")),
        None => state.flash("Herdr is not reachable"),
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
        let nt = crate::engine::NewTask { text: f.text.clone(), title: None, repo, options: opts, via: "ui".into(), source: None, ..Default::default() };
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
        // Terminals deliver pasted/typed line feeds as ctrl+j.
        KeyCode::Char('j') if k.modifiers.contains(KeyModifiers::CONTROL) && f.field == 0 => f.text.push('\n'),
        KeyCode::Char('j') if k.modifiers.contains(KeyModifiers::CONTROL) => {}
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
