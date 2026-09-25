//! Rendering. Pure functions of [`State`]; tested with ratatui's TestBackend.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::{Form, Pending, Screen, State};
use crate::cli::render::{dur, step_icon};
use crate::model::*;

fn status_color(s: RunStatus) -> Color {
    match s {
        RunStatus::Succeeded => Color::Green,
        RunStatus::Failed => Color::Red,
        RunStatus::Blocked | RunStatus::NeedsHuman => Color::Magenta,
        RunStatus::AwaitingApproval => Color::Yellow,
        RunStatus::Cancelled => Color::DarkGray,
        _ => Color::Cyan,
    }
}

fn step_color(s: StepStatus) -> Color {
    match s {
        StepStatus::Succeeded => Color::Green,
        StepStatus::Failed => Color::Red,
        StepStatus::AwaitingApproval | StepStatus::AwaitingHuman => Color::Yellow,
        StepStatus::Running | StepStatus::Starting | StepStatus::Retrying => Color::Cyan,
        _ => Color::DarkGray,
    }
}

pub fn render(f: &mut Frame, s: &State) {
    let area = f.area();
    let [main, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(area);
    match &s.screen {
        Screen::Dashboard => dashboard(f, main, s),
        Screen::RunDetail(id) => run_detail(f, main, s, id),
        Screen::Approvals => approvals(f, main, s),
        Screen::ApprovalDetail(id) => approval_detail(f, main, s, id),
        Screen::Text { title, lines, scroll } => text(f, main, title, lines, *scroll),
        Screen::NewTask => new_task(f, main, s.form.as_ref()),
        Screen::Agent(id) => agent_screen(f, main, s, id),
        Screen::Epics => epics(f, main, s),
        Screen::EpicDetail { id, scroll } => text(f, main, &format!("epic {id}"), &s.epic_lines, *scroll),
    }
    footer_bar(f, footer, s);
    if let Some(p) = &s.confirm {
        let msg = match p {
            Pending::CancelRun(id) => format!("Cancel run {}? The agent is interrupted; its worktree is kept.  [y] yes  [any] no", s.run(id).map(|r| r.display_name()).unwrap_or(id.clone())),
            Pending::Deny(id) => format!("Deny approval {id}? The run will fail at this step.  [y] yes  [any] no"),
            Pending::RejectEpic(id) => format!("Reject the open plan tasks of epic {id}? Accepted tasks keep running.  [y] yes  [any] no"),
        };
        let w = (msg.len() as u16 + 4).min(area.width.saturating_sub(4));
        let r = Rect { x: area.x + (area.width.saturating_sub(w)) / 2, y: area.y + area.height / 2 - 2, width: w, height: 4 };
        f.render_widget(Clear, r);
        f.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }).block(Block::bordered().title(" confirm ").border_style(Style::new().fg(Color::Yellow))), r);
    }
}

fn footer_bar(f: &mut Frame, area: Rect, s: &State) {
    let running = s.runs.iter().filter(|r| r.status.is_active()).count();
    let queued = s.tasks.iter().filter(|t| t.status == TaskStatus::Queued).count();
    let agents = s.runs.iter().flat_map(|r| r.steps.iter()).filter(|e| e.kind == StepKind::Agent && matches!(e.status, StepStatus::Running | StepStatus::AwaitingHuman | StepStatus::Starting)).count();
    let appr = s.pending_approvals().len();
    let keys = match &s.screen {
        Screen::Dashboard => "[n] new  [enter] inspect  [a] approvals  [e] epics  [r] retry  [x] cancel  [d] diff  [f] focus agent  [F] PR follow-up  [p] pause queue  [q] quit",
        Screen::RunDetail(_) => "[↑↓] step  [enter/l] log  [d] diff  [f] focus agent  [a] approval  [F] PR follow-up  [r] retry  [x] cancel  [esc] back",
        Screen::Epics => "[↑↓] select  [enter] plan  [y] accept open tasks  [n] reject  [g] re-plan  [v] verify vs ADR  [esc] back",
        Screen::EpicDetail { .. } => "[↑↓] scroll  [y] accept open tasks  [n] reject  [g] re-plan  [v] verify vs ADR  [esc] back",
        Screen::Approvals => "[↑↓] select  [enter] open  [esc] back",
        Screen::ApprovalDetail(_) => "[y] approve once  [n] deny  [c] cancel run  [d] diff  [f] open agent pane  [esc] back",
        Screen::Text { .. } => "[↑↓/space] scroll  [g/G] top/bottom  [esc] back",
        Screen::NewTask => "[tab] next field  [←→] choose  [ctrl+s] create  [esc] cancel",
        Screen::Agent(_) => "keys go to the agent: [1-9] [y] [n] [a] [↑↓] [enter] [tab]   ·   [f] open its pane   [esc] back",
    };
    let stats = Line::from(vec![
        Span::styled(" Running ", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!("{running}  ")),
        Span::styled("Queued ", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!("{queued}  ")),
        Span::styled("Agents ", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!("{agents}  ")),
        Span::styled("Tokens ", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!("{}  ", crate::telemetry::compact_tokens(&crate::telemetry::runs_agent_usage(s.dashboard_runs().into_iter())))),
        Span::styled("Approvals ", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!("{appr}  "), if appr > 0 { Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD) } else { Style::new() }),
        Span::styled(if s.agents_waiting() > 0 { format!("Agents asking {}  ", s.agents_waiting()) } else { String::new() }, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(if s.paused { "QUEUE PAUSED  " } else { "" }, Style::new().fg(Color::Magenta)),
        Span::styled(if s.daemon_up { "engine up" } else { "engine idle" }, Style::new().fg(Color::DarkGray)),
        Span::raw("   "),
        Span::styled(s.message.as_ref().map(|(m, _)| m.clone()).unwrap_or_default(), Style::new().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(vec![stats, Line::styled(format!(" {keys}"), Style::new().fg(Color::DarkGray))]), area);
}

fn dashboard(f: &mut Frame, area: Rect, s: &State) {
    let mut lines: Vec<Line> = vec![Line::styled(" HERDR ORCHESTRATOR", Style::new().add_modifier(Modifier::BOLD)), Line::raw("")];
    let runs = s.dashboard_runs();
    let (active, done): (Vec<_>, Vec<_>) = runs.iter().enumerate().partition(|(_, r)| !r.status.is_terminal());
    let section = |title: &str, items: &[(usize, &&Run)], lines: &mut Vec<Line>, expand: bool| {
        if items.is_empty() {
            return;
        }
        lines.push(Line::styled(format!(" {title}"), Style::new().fg(Color::DarkGray).add_modifier(Modifier::BOLD)));
        lines.push(Line::raw(""));
        for (i, r) in items {
            let sel = *i == s.selected;
            let title = s.task(&r.task_id).map(|t| t.title.clone()).unwrap_or_default();
            let marker = if sel { "▶" } else { " " };
            let st = Style::new().fg(status_color(r.status));
            let tokens = crate::telemetry::compact_tokens(&crate::telemetry::run_agent_usage(r));
            let mut head = vec![
                Span::raw(format!("{marker}{:<6} ", r.display_name())),
                Span::styled(format!("{:<18}", r.status.as_str()), st),
                Span::styled(format!("{tokens:>6} tok  "), Style::new().fg(Color::DarkGray)),
                Span::styled(title, if sel { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() }),
            ];
            if let Some(reason) = r.status_reason.as_ref().filter(|_| matches!(r.status, RunStatus::Blocked | RunStatus::NeedsHuman | RunStatus::Failed)) {
                head.push(Span::styled(format!("  — {}", reason.chars().take(70).collect::<String>()), Style::new().fg(Color::DarkGray)));
            }
            lines.push(Line::from(head));
            if let Some(w) = s.waiting_step(r) {
                let msg = match &w.attention {
                    Some(why) => format!("! {} · {} looks stuck: {why} — enter to look, [x] to cancel", w.step_id, w.runner.clone().unwrap_or_default()),
                    None => format!("! {} · {} is asking you something — press enter to see and answer", w.step_id, w.runner.clone().unwrap_or_default()),
                };
                lines.push(Line::from(vec![Span::raw("        "), Span::styled(msg, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))]));
            }
            if expand {
                let wf = crate::workflow::Workflow::parse(&r.workflow_yaml).ok();
                let ids: Vec<String> = wf.map(|w| w.steps.iter().map(|x| x.id.clone()).collect()).unwrap_or_default();
                for id in ids {
                    let e = r.latest_exec(&id);
                    let (icon, color, who, t) = match e {
                        Some(e) => (step_icon(e.status), step_color(e.status), e.runner.clone().unwrap_or_default(), dur(e.duration_secs())),
                        None => ("○", Color::DarkGray, String::new(), String::new()),
                    };
                    let attempt = e.filter(|e| e.attempt > 1).map(|e| format!(" ×{}", e.attempt)).unwrap_or_default();
                    // Tokens of every attempt of this step (retries add up).
                    let step_tokens = e
                        .filter(|e| e.kind == StepKind::Agent)
                        .map(|_| crate::telemetry::compact_tokens(&UsageRecord::sum(r.steps.iter().filter(|x| x.step_id == id).filter_map(|x| x.usage.as_ref()))))
                        .map(|t| format!("  {t} tok"))
                        .unwrap_or_default();
                    lines.push(Line::from(vec![
                        Span::raw(format!("        {:<16}", id)),
                        Span::styled(format!("{icon}  "), Style::new().fg(color)),
                        Span::raw(format!("{:<14}{:>7}{}", who, t, attempt)),
                        Span::styled(step_tokens, Style::new().fg(Color::DarkGray)),
                    ]));
                }
            }
            lines.push(Line::raw(""));
        }
    };
    section("RUNNING", &active, &mut lines, true);
    let queued: Vec<&Task> = s.tasks.iter().filter(|t| t.status == TaskStatus::Queued).collect();
    if !queued.is_empty() {
        lines.push(Line::styled(" QUEUED", Style::new().fg(Color::DarkGray).add_modifier(Modifier::BOLD)));
        lines.push(Line::raw(""));
        for t in queued {
            lines.push(Line::from(vec![
                Span::raw(format!("  #{:<5} {}", t.task_id, t.title)),
                Span::styled(t.waiting_on.as_ref().map(|w| format!("  — {w}")).unwrap_or_default(), Style::new().fg(Color::DarkGray)),
            ]));
        }
        lines.push(Line::raw(""));
    }
    section("RECENT", &done, &mut lines, false);
    if runs.is_empty() && s.tasks.is_empty() {
        lines.push(Line::raw("  No tasks yet. Press [n] to create one, or run:"));
        lines.push(Line::styled("    herdr-orchestrator task create \"Implement X\"", Style::new().fg(Color::Cyan)));
    }
    // Keep the selection visible.
    let sel_line = lines.iter().position(|l| l.spans.first().is_some_and(|sp| sp.content.starts_with('▶'))).unwrap_or(0);
    let h = area.height.saturating_sub(2) as usize;
    let scroll = sel_line.saturating_sub(h.saturating_sub(8)) as u16;
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(Block::new().borders(Borders::NONE)), area);
}

fn run_detail(f: &mut Frame, area: Rect, s: &State, id: &str) {
    let Some(r) = s.run(id) else {
        f.render_widget(Paragraph::new("run not found"), area);
        return;
    };
    let task = s.task(&r.task_id);
    let [head, steps, bottom] = Layout::vertical([Constraint::Length(10), Constraint::Min(5), Constraint::Length(9)]).areas(area);
    let kv = |k: &str, v: String| Line::from(vec![Span::styled(format!(" {k:<11}"), Style::new().fg(Color::DarkGray)), Span::raw(v)]);
    let head_lines = vec![
        Line::from(vec![Span::styled(format!(" {} ", r.display_name()), Style::new().add_modifier(Modifier::BOLD)), Span::styled(r.status.as_str(), Style::new().fg(status_color(r.status))), Span::raw(r.status_reason.as_ref().map(|x| format!(" — {x}")).unwrap_or_default())]),
        kv("Task", task.map(|t| t.title.clone()).unwrap_or_default()),
        kv("Workflow", r.workflow_name.clone()),
        kv("Repository", r.repo_root.display().to_string()),
        kv("Branch", r.git.branch.clone().unwrap_or_default()),
        kv("Worktree", r.git.worktree_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
        kv("Base SHA", format!("{} ({})", r.git.base_sha.as_deref().map(|x| &x[..12.min(x.len())]).unwrap_or(""), r.git.base_ref)),
        kv("PR", r.pr_url.clone().unwrap_or("-".into())),
    ];
    let mut head_lines = head_lines;
    let hint = if s.waiting_step(r).is_some() {
        Some("The agent is asking you something: select its step and press enter to answer, or [f] to open its pane.")
    } else {
        match r.status {
            RunStatus::NeedsHuman | RunStatus::Failed | RunStatus::Cancelled => Some("What now: [r] retry from the stopped step · [l] log · [d] diff · [f] open the agent pane · [x] cancel"),
            RunStatus::Blocked => Some("Blocked by policy: fix the files in the worktree ([d] diff), then [r] retry — or [x] cancel."),
            RunStatus::AwaitingApproval => Some("Waiting for your approval: press [a]."),
            _ => None,
        }
    };
    if let Some(h) = hint {
        head_lines.insert(1, Line::styled(format!(" {h}"), Style::new().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(head_lines), head);
    let rows: Vec<Row> = r
        .steps
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let st = if i == s.selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
            Row::new(vec![
                Cell::from(e.step_id.clone()),
                Cell::from(Span::styled(format!("{} {}", step_icon(e.status), e.status.as_str()), Style::new().fg(step_color(e.status)))),
                Cell::from(e.runner.clone().unwrap_or_default()),
                Cell::from(e.attempt.to_string()),
                Cell::from(dur(e.duration_secs())),
                Cell::from(if e.kind == StepKind::Agent { e.usage.as_ref().map(crate::telemetry::compact_tokens).unwrap_or_else(|| "–".into()) } else { String::new() }),
                Cell::from(e.agent.as_ref().and_then(|a| a.pane_id.clone()).unwrap_or_default()),
                Cell::from(e.error.clone().unwrap_or_default()),
            ])
            .style(st)
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(16), Constraint::Length(20), Constraint::Length(12), Constraint::Length(7), Constraint::Length(8), Constraint::Length(8), Constraint::Length(8), Constraint::Min(10)])
        .header(Row::new(vec!["STEP", "STATUS", "AGENT", "ATTEMPT", "TIME", "TOKENS", "PANE", "NOTE"]).style(Style::new().fg(Color::DarkGray)))
        .block(Block::new().borders(Borders::TOP));
    f.render_widget(table, steps);
    let mut b = vec![];
    if let Some(d) = &r.diff_stat {
        b.push(Line::from(vec![Span::raw(format!(" Files changed: {}   ", d.files_changed)), Span::styled(format!("+{}", d.insertions), Style::new().fg(Color::Green)), Span::raw(" / "), Span::styled(format!("-{}", d.deletions), Style::new().fg(Color::Red))]));
    }
    let checks: Vec<String> = r.steps.iter().filter(|e| e.kind == StepKind::Command).map(|e| format!("{} {}", step_icon(e.status), e.step_id)).collect();
    if !checks.is_empty() {
        b.push(Line::raw(format!(" Checks: {}", checks.join("  "))));
    }
    if let Some(v) = r.steps.iter().rev().filter_map(|e| e.structured.as_ref()).find(|v| v.get("findings").is_some()) {
        b.push(Line::raw(format!(" Review: {} ({} findings)", v["verdict"].as_str().unwrap_or("?"), v["findings"].as_array().map(|a| a.len()).unwrap_or(0))));
    }
    let (a, q, dn) = r.policy_summary();
    b.push(Line::raw(format!(" Policy: {a} allow · {q} approval · {dn} denied")));
    let u = crate::telemetry::run_agent_usage(r);
    b.push(Line::raw(format!(" Usage:  {} · cost {}", crate::telemetry::tokens_display(&u), u.cost_display())));
    if let Some(e) = r.steps.get(s.selected) {
        if let Some(o) = &e.output_excerpt {
            b.push(Line::styled(format!(" {}: {}", e.step_id, o.lines().last().unwrap_or("")), Style::new().fg(Color::DarkGray)));
        }
    }
    f.render_widget(Paragraph::new(b).block(Block::new().borders(Borders::TOP)), bottom);
}

fn epics(f: &mut Frame, area: Rect, s: &State) {
    let mut lines = vec![Line::styled(" EPICS", Style::new().add_modifier(Modifier::BOLD)), Line::raw("")];
    if s.epics.is_empty() {
        lines.push(Line::raw("  No epics yet. Plan an ADR with:"));
        lines.push(Line::styled("    herdr-orchestrator epic create --from docs/adr/0007-something.md", Style::new().fg(Color::Cyan)));
    }
    for (i, row) in s.epic_rows.iter().enumerate() {
        let st = if i == s.selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
        lines.push(Line::styled(format!("  {row}"), st));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn approvals(f: &mut Frame, area: Rect, s: &State) {
    let list = s.pending_approvals();
    let mut lines = vec![Line::styled(" APPROVALS", Style::new().add_modifier(Modifier::BOLD)), Line::raw("")];
    if list.is_empty() {
        lines.push(Line::raw("  Nothing is waiting for you."));
    }
    for (i, a) in list.iter().enumerate() {
        let run = s.run(&a.run_id).map(|r| r.display_name()).unwrap_or_default();
        let st = if i == s.selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
        lines.push(Line::styled(format!("  {:<6} {:<12} {:<14} {}", run, a.step_id, a.context.task_title.chars().take(30).collect::<String>(), a.reason), st));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn approval_detail(f: &mut Frame, area: Rect, s: &State, id: &str) {
    let Some(a) = s.approvals.iter().find(|a| a.approval_id == id) else {
        f.render_widget(Paragraph::new("approval not found"), area);
        return;
    };
    let c = &a.context;
    let kv = |k: &str, v: String| Line::from(vec![Span::styled(format!(" {k:<12}"), Style::new().fg(Color::DarkGray)), Span::raw(v)]);
    let mut lines = vec![
        Line::from(vec![Span::styled(" APPROVAL REQUIRED ", Style::new().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)), Span::raw(format!("  {} ({})", a.approval_id, a.status.as_str()))]),
        Line::raw(""),
        kv("Task", format!("#{} {}", a.task_id, c.task_title)),
        kv("Step", format!("{} ({})", c.step_id, c.workflow)),
        kv("Reason", a.reason.clone()),
        kv("On approve", c.pending_action.clone().unwrap_or("-".into())),
        kv("Agent", format!("{}{}", c.agent.clone().unwrap_or("-".into()), c.pane_id.as_ref().map(|p| format!("  (pane {p})")).unwrap_or_default())),
        kv("Repository", c.repository.display().to_string()),
        kv("Branch", c.branch.clone().unwrap_or_default()),
        kv("Diff", format!("{} files  +{} / -{}", c.changed_files.len(), c.insertions, c.deletions)),
    ];
    for fch in c.changed_files.iter().take(12) {
        lines.push(Line::raw(format!("               {:<10} {}", fch.change, fch.path)));
    }
    if c.changed_files.len() > 12 {
        lines.push(Line::raw(format!("               … {} more", c.changed_files.len() - 12)));
    }
    if !c.checks.is_empty() {
        lines.push(kv("Checks", c.checks.join(" · ")));
    }
    for p in &c.policy {
        lines.push(Line::from(vec![Span::styled(" Policy      ", Style::new().fg(Color::DarkGray)), Span::styled(format!("{} ", p.decision.upper()), Style::new().fg(Color::Yellow)), Span::raw(format!("{} — {}", p.subject, p.reason))]));
    }
    if !c.acceptance.is_empty() {
        lines.push(kv("Acceptance", String::new()));
        for (i, r) in c.acceptance.iter().enumerate() {
            let color = match r.status.as_str() {
                "met" => Color::Green,
                "not_met" => Color::Red,
                _ => Color::Yellow,
            };
            lines.push(Line::from(vec![
                Span::raw(format!("               {}. ", i + 1)),
                Span::styled(format!("{:<13}", r.status), Style::new().fg(color)),
                Span::raw(r.criterion.clone()),
                Span::styled(if r.evidence.is_empty() { String::new() } else { format!("  — {}", r.evidence) }, Style::new().fg(Color::DarkGray)),
            ]));
        }
    }
    if !c.manual_checks.is_empty() {
        lines.push(kv("Check yourself", String::new()));
        for m in &c.manual_checks {
            lines.push(Line::raw(format!("               [ ] {m}")));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(" Approval applies to this action only; policies are not changed.", Style::new().fg(Color::DarkGray)));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn text(f: &mut Frame, area: Rect, title: &str, lines: &[String], scroll: usize) {
    let styled: Vec<Line> = lines
        .iter()
        .skip(scroll)
        .take(area.height as usize)
        .map(|l| {
            let color = if l.starts_with("+++") || l.starts_with("---") {
                Color::White
            } else if l.starts_with('+') {
                Color::Green
            } else if l.starts_with('-') {
                Color::Red
            } else if l.starts_with("@@") {
                Color::Cyan
            } else {
                Color::Reset
            };
            Line::styled(l.clone(), Style::new().fg(color))
        })
        .collect();
    f.render_widget(Paragraph::new(styled).block(Block::new().borders(Borders::TOP).title(format!(" {title}  ({}/{}) ", scroll + 1, lines.len().max(1)))), area);
}

fn agent_screen(f: &mut Frame, area: Rect, s: &State, id: &str) {
    let Some(r) = s.run(id) else {
        f.render_widget(Paragraph::new("run not found"), area);
        return;
    };
    let step = s.waiting_step(r).or(r.steps.last());
    let [head, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(area);
    let title = match step {
        Some(e) if e.status == StepStatus::AwaitingHuman => Line::from(vec![
            Span::styled(" AGENT IS ASKING ", Style::new().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(format!("  {} · {} · {} (pane {})", r.display_name(), e.step_id, e.runner.clone().unwrap_or_default(), e.agent.as_ref().and_then(|a| a.pane_id.clone()).unwrap_or_default())),
        ]),
        Some(e) => Line::raw(format!(" {} · {} · {} — no longer waiting ({})", r.display_name(), e.step_id, e.runner.clone().unwrap_or_default(), e.status.as_str())),
        None => Line::raw(" no agent step"),
    };
    f.render_widget(
        Paragraph::new(vec![title, Line::styled(" Live view of the agent's pane. Keys below go straight to the agent; your answer is recorded in the audit log.", Style::new().fg(Color::DarkGray))]),
        head,
    );
    let h = body.height.saturating_sub(2) as usize;
    let lines: Vec<&String> = s.agent_lines.iter().filter(|l| !l.trim().is_empty()).collect();
    let from = lines.len().saturating_sub(h);
    let shown: Vec<Line> = lines[from..].iter().map(|l| Line::raw((*l).clone())).collect();
    f.render_widget(Paragraph::new(shown).block(Block::bordered().border_style(Style::new().fg(Color::Yellow))), body);
}

fn new_task(f: &mut Frame, area: Rect, form: Option<&Form>) {
    let Some(fm) = form else { return };
    let active = |i: usize| if fm.field == i { Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD) } else { Style::new().fg(Color::DarkGray) };
    let [h, t, rest] = Layout::vertical([Constraint::Length(2), Constraint::Min(5), Constraint::Length(8)]).areas(area);
    f.render_widget(Paragraph::new(vec![Line::styled(" NEW TASK", Style::new().add_modifier(Modifier::BOLD)), Line::raw(format!(" repo: {}", fm.repo.as_ref().map(|r| r.display().to_string()).unwrap_or("(none — open from a repository workspace)".into())))]), h);
    let mut text = fm.text.clone();
    if fm.field == 0 {
        text.push('▏');
    }
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::bordered().title(" Task ").border_style(active(0))), t);
    let pick = |v: &[String], i: usize| v.get(i).cloned().unwrap_or_default();
    let lines = vec![
        Line::from(vec![Span::styled(" Workflow    ", active(1)), Span::raw(format!("◀ {} ▶", pick(&fm.workflows, fm.workflow)))]),
        Line::from(vec![Span::styled(" Runner      ", active(2)), Span::raw(format!("◀ {} ▶", pick(&fm.runners, fm.runner)))]),
        Line::from(vec![Span::styled(" Base branch ", active(3)), Span::raw(format!("{}{}", fm.base, if fm.field == 3 { "▏" } else { "" }))]),
        Line::from(vec![Span::styled(" Variants    ", active(4)), Span::raw(format!("◀ {} ▶", fm.variants))]),
        Line::raw(""),
        Line::styled("   [ Create task ]", active(5)),
    ];
    f.render_widget(Paragraph::new(lines), rest);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn buffer_text(t: &Terminal<TestBackend>) -> String {
        let b = t.backend().buffer();
        let mut s = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                s.push_str(b[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    fn sample() -> State {
        let mut s = State::new(Screen::Dashboard);
        let task = Task {
            task_id: "124".into(),
            title: "Add portfolio exposure clustering".into(),
            description: "x".into(),
            repo_root: "/r".into(),
            status: TaskStatus::Running,
            options: Default::default(),
            initiator: Initiator::current("test"),
            source: None,
            run_ids: vec!["run-a".into()],
            selected_run: None,
            created_at: now(),
            updated_at: now(),
            epic: None,
            depends_on: vec![],
            acceptance: vec![],
            manual_checks: vec![],
            waiting_on: None,
        };
        let mut run: Run = serde_json::from_value(serde_json::json!({
            "run_id": "run-a", "task_id": "124", "variant_index": 0, "variant_count": 1,
            "workflow_name": "implement-review", "workflow_sha256": "abcdef1234567890",
            "workflow_yaml": crate::workflow::catalog::BUILTIN_WORKFLOWS[1].1,
            "runner_override": null, "repo_root": "/r", "git": {"base_ref": "main", "base_sha": "0123456789abcdef", "branch": "herdr/124-add", "worktree_path": "/r/.herdr-orchestrator/worktrees/124-add"},
            "herdr": {}, "status": "running", "status_reason": null, "initiator": {"via": "t", "user": null}, "dry_run": false,
            "created_at": now(), "started_at": now(), "updated_at": now(), "completed_at": null, "cursor": 2
        }))
        .unwrap();
        let mut e = StepExecution::new("implement", StepKind::Agent, 1);
        e.status = StepStatus::Succeeded;
        e.runner = Some("codex".into());
        e.started_at = Some(now());
        e.ended_at = Some(now());
        run.steps.push(e);
        let mut e = StepExecution::new("review", StepKind::Agent, 1);
        e.status = StepStatus::Running;
        e.runner = Some("claude".into());
        e.started_at = Some(now());
        run.steps.push(e);
        s.tasks = vec![task];
        s.runs = vec![run];
        s
    }

    #[test]
    fn dashboard_renders_runs_and_steps() {
        let s = sample();
        let mut t = Terminal::new(TestBackend::new(110, 30)).unwrap();
        t.draw(|f| render(f, &s)).unwrap();
        let out = buffer_text(&t);
        assert!(out.contains("HERDR ORCHESTRATOR"));
        assert!(out.contains("#124"));
        assert!(out.contains("Add portfolio exposure clustering"));
        assert!(out.contains("implement") && out.contains("codex"));
        assert!(out.contains("review") && out.contains("claude"));
        assert!(out.contains("Running 1"));
        assert!(out.contains("[n] new"));
    }

    #[test]
    fn run_detail_renders() {
        let mut s = sample();
        s.screen = Screen::RunDetail("run-a".into());
        let mut t = Terminal::new(TestBackend::new(120, 34)).unwrap();
        t.draw(|f| render(f, &s)).unwrap();
        let out = buffer_text(&t);
        assert!(out.contains("herdr/124-add"));
        assert!(out.contains("STEP") && out.contains("ATTEMPT"));
        assert!(out.contains("Policy: 0 allow"));
        assert!(out.contains("cost unknown"));
    }

    #[test]
    fn approval_detail_shows_context() {
        let mut s = sample();
        let mut ctx = crate::approvals::ApprovalContext { task_title: "Add clustering".into(), workflow: "implement-review".into(), step_id: "approval".into(), pending_action: Some("push herdr/124-add and open a draft PR".into()), ..Default::default() };
        ctx.changed_files.push(ChangedFile { path: "src/a.rs".into(), change: "modified".into(), insertions: 3, deletions: 1, old_path: None });
        let a = crate::approvals::ApprovalRequest::new("run-a", "124", "approval", crate::approvals::ApprovalKind::WorkflowStep, "Ready for review".into(), ctx);
        s.screen = Screen::ApprovalDetail(a.approval_id.clone());
        s.approvals = vec![a];
        let mut t = Terminal::new(TestBackend::new(110, 30)).unwrap();
        t.draw(|f| render(f, &s)).unwrap();
        let out = buffer_text(&t);
        assert!(out.contains("APPROVAL REQUIRED"));
        assert!(out.contains("open a draft PR"));
        assert!(out.contains("src/a.rs"));
        assert!(out.contains("[y] approve once"));
        assert!(out.contains("Approvals 1"));
    }

    #[test]
    fn epics_screen_and_tokens() {
        let mut s = sample();
        s.runs[0].steps[0].usage = Some(UsageRecord { source: UsageSource::Reported, input_tokens: Some(12_000), output_tokens: Some(3_000), ..Default::default() });
        let mut t = Terminal::new(TestBackend::new(140, 30)).unwrap();
        t.draw(|f| render(f, &s)).unwrap();
        let out = buffer_text(&t);
        assert!(out.contains("15k tok"), "{out}");
        assert!(out.contains("[e] epics"));
        s.screen = Screen::Epics;
        s.epic_rows = vec!["E1   proposed      0/0  done   3 open       – tok  7. Rate limiting".into()];
        s.epics = vec![];
        let mut t = Terminal::new(TestBackend::new(140, 20)).unwrap();
        t.draw(|f| render(f, &s)).unwrap();
        let out = buffer_text(&t);
        assert!(out.contains("EPICS") && out.contains("7. Rate limiting") && out.contains("[y] accept open tasks"));
    }

    #[test]
    fn waiting_agent_is_visible_and_answerable() {
        let mut s = sample();
        s.runs[0].steps[1].status = StepStatus::AwaitingHuman;
        s.runs[0].steps[1].agent = Some(AgentBinding { mode: "pane".into(), pane_id: Some("w13:p2".into()), agent_name: Some("o124-review-abcd".into()), ..Default::default() });
        let mut t = Terminal::new(TestBackend::new(130, 32)).unwrap();
        t.draw(|f| render(f, &s)).unwrap();
        let out = buffer_text(&t);
        assert!(out.contains("is asking you something"), "{out}");
        assert!(out.contains("Agents asking 1"));
        s.screen = Screen::Agent("run-a".into());
        s.agent_lines = vec![" Do you want to proceed?".into(), " ❯ 1. Yes".into(), "   2. No".into()];
        let mut t = Terminal::new(TestBackend::new(130, 20)).unwrap();
        t.draw(|f| render(f, &s)).unwrap();
        let out = buffer_text(&t);
        assert!(out.contains("AGENT IS ASKING"));
        assert!(out.contains("Do you want to proceed?"));
        assert!(out.contains("keys go to the agent"));
    }
}
