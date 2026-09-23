//! Step executors for the run driver.

use std::time::Instant;

use anyhow::{Context, Result};

use super::driver::{RunDriver, StepOutcome};
use super::{compose_prompt, orchestrator_instructions, parse_review};
use crate::approvals::{ApprovalContext, ApprovalKind, ApprovalRequest, ApprovalStatus};
use crate::audit::{redact::redact_str, Actor};
use crate::model::*;
use crate::policies::{Action, Decision, PolicyDecision, Subject};
use crate::runners::{AgentEnd, AgentEvent, AgentRequest};
use crate::workflow::{AgentOutput, GitAction, Step, StepSpec};

/// Outcome of asking a human.
enum Approval {
    Granted,
    Denied(String),
    Cancelled,
}

impl<'a> RunDriver<'a> {
    pub(super) fn execute_step(&mut self, step: &Step) -> Result<StepOutcome> {
        match &step.spec {
            StepSpec::Agent { .. } => self.agent_step(step),
            StepSpec::Command { .. } | StepSpec::Check { .. } => self.command_step(step),
            StepSpec::Approval { reason } => self.approval_step(step, reason),
            StepSpec::Policy {} => self.policy_step(step),
            StepSpec::Git { action, message } => self.git_step(step, *action, message.as_deref()),
            StepSpec::GithubPr { draft, title, body, base } => {
                self.pr_step(step, *draft, title.as_deref(), body.as_deref(), base.as_deref())
            }
            StepSpec::Parallel { .. } => Ok(StepOutcome::Failed {
                reason: "parallel steps are not supported".into(),
                feedback: String::new(),
            }),
        }
    }

    /// Latest execution of `step` if it is still open (resumption after a
    /// restart). Otherwise `None`: a fresh execution is needed.
    fn open_exec(&self, step: &Step) -> Option<StepExecution> {
        self.run.latest_exec(&step.id).filter(|e| !e.status.is_terminal()).cloned()
    }

    fn finish_exec(&mut self, exec_id: &str, status: StepStatus, error: Option<String>) -> Result<()> {
        {
            let e = self.exec_mut(exec_id);
            if let Some(err) = error {
                e.error = Some(err);
            }
        }
        // Normalize: starting → running before terminal transitions.
        if self.exec_mut(exec_id).status == StepStatus::Starting && status == StepStatus::Succeeded {
            self.set_exec_status(exec_id, StepStatus::Running)?;
        }
        if self.exec_mut(exec_id).status == StepStatus::Pending && status == StepStatus::Succeeded {
            self.set_exec_status(exec_id, StepStatus::Starting)?;
            self.set_exec_status(exec_id, StepStatus::Running)?;
        }
        self.set_exec_status(exec_id, status)?;
        let e = self.exec_mut(exec_id).clone();
        self.audit(
            "step_completed",
            Actor::orchestrator(),
            Some(&e.step_id),
            serde_json::json!({
                "exec_id": e.exec_id,
                "status": e.status.as_str(),
                "attempt": e.attempt,
                "duration_s": e.duration_secs(),
                "error": e.error,
            }),
        );
        Ok(())
    }

    fn runner_for(&self, step: &Step) -> String {
        self.run
            .step_runners
            .get(&step.id)
            .cloned()
            .or_else(|| self.run.runner_override.clone())
            .or_else(|| step.runner(&self.wf).map(String::from))
            .unwrap_or_else(|| self.cfg.config.defaults.runner.clone())
    }

    // ================================================================ agent

    fn agent_step(&mut self, step: &Step) -> Result<StepOutcome> {
        let StepSpec::Agent { skill, prompt, output, gate, commit, policy_check, .. } = &step.spec else { unreachable!() };
        let runner_name = self.runner_for(step);
        let (runner, profile) = match self.factory.build(&runner_name) {
            Ok(x) => x,
            Err(e) => return Ok(StepOutcome::Failed { reason: format!("{e:#}"), feedback: String::new() }),
        };
        let worktree = self.worktree()?;
        let resumed = self.open_exec(step);
        let exec_id = match &resumed {
            Some(e) => e.exec_id.clone(),
            None => self.new_exec(step)?,
        };
        let attempt = self.exec_mut(&exec_id).attempt;
        let output_file = worktree.join(crate::git::ORCH_DIR).join("out").join(format!("{exec_id}.json"));

        // Policy gate on the agent launch itself (permission-bypass flags…).
        let mut launch = vec![profile.kind.clone().unwrap_or_else(|| runner_name.clone())];
        launch.extend(profile.pane_args.iter().cloned());
        if let Some(h) = &profile.headless_command {
            launch.extend(h.iter().cloned());
        }
        let mut subject = Subject::command(&launch, false);
        subject.action = Some(Action::AgentStart);
        subject.runner = Some(runner_name.clone());
        subject.step_id = Some(step.id.clone());
        subject.branch = self.run.git.branch.clone();
        let d = self.policy.evaluate(&subject);
        self.record_policy(Some(&step.id), Some(&exec_id), &d);
        match d.decision {
            Decision::Deny => {
                self.finish_exec(&exec_id, StepStatus::Failed, Some(format!("policy denied agent launch: {}", d.reason)))?;
                return Ok(StepOutcome::Blocked(format!("policy denied launching {runner_name}: {}", d.reason)));
            }
            Decision::RequireApproval => match self.request_approval(step, &exec_id, ApprovalKind::Policy, format!("Launch {runner_name}: {}", d.reason), Some(format!("start agent {}", crate::policies::command::display_argv(&launch))), vec![d.clone()])? {
                Approval::Granted => {}
                Approval::Denied(why) => {
                    self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                    return Ok(StepOutcome::Failed { reason: why, feedback: String::new() });
                }
                Approval::Cancelled => return Ok(StepOutcome::Cancelled),
            },
            Decision::Allow => {}
        }

        // Per-run agent budget.
        let distinct: std::collections::BTreeSet<String> = self
            .run
            .steps
            .iter()
            .filter(|e| e.exec_id != exec_id)
            // Only persistent pane agents count; headless/fake processes
            // exit after each attempt.
            .filter_map(|e| e.agent.as_ref().filter(|a| a.mode == "pane").and_then(|a| a.agent_name.clone()))
            .collect();
        let previous = self
            .run
            .steps
            .iter()
            .rev()
            .filter(|e| e.step_id == step.id && e.exec_id != exec_id)
            .find_map(|e| e.agent.clone())
            .filter(|b| b.mode == "pane");
        if previous.is_none() && resumed.as_ref().and_then(|e| e.agent.as_ref()).is_none() && distinct.len() >= self.cfg.config.limits.max_agents_per_run {
            let why = format!("limits.max_agents_per_run ({}) reached", self.cfg.config.limits.max_agents_per_run);
            self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
            return Ok(StepOutcome::Failed { reason: why, feedback: String::new() });
        }

        if self.run.dry_run {
            self.audit("dry_run", Actor::orchestrator(), Some(&step.id), serde_json::json!({"would": format!("start {runner_name} ({:?}) and send the step prompt", profile.mode)}));
            self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
            return Ok(StepOutcome::Succeeded { output: Some(format!("(dry run: {runner_name} not started)")) });
        }

        // Prompt composition.
        let tctx = self.template_ctx(&step.id, Some(&output_file));
        let body = tctx.render(prompt).context("rendering prompt")?;
        let skill_text = match skill {
            Some(s) => match self.catalog.skill(s) {
                Ok((t, _)) => Some(t),
                Err(e) => return Ok(StepOutcome::Failed { reason: format!("{e:#}"), feedback: String::new() }),
            },
            None => None,
        };
        let review = *output == AgentOutput::Review;
        let full = compose_prompt(skill_text.as_deref(), &body, &self.run, &step.id, attempt, &worktree, &output_file, review);
        let followup = format!("{}\n\n---\n\n{}", body.trim(), orchestrator_instructions(&self.run, &step.id, attempt, &worktree, &output_file, review));
        let log_path = self.ctx.store.layout.run_logs_dir(&self.run.run_id).join(format!("{exec_id}.log"));
        let mut env = self.cfg.config.environment.clone();
        env.inherit.extend(profile.env_inherit.iter().cloned());
        let child_env = env.build(std::env::vars(), &self.orch_env(&step.id));
        let timeout = self.step_timeout(step, self.cfg.config.limits.agent_timeout.as_duration());
        let req = AgentRequest {
            run_id: self.run.run_id.clone(),
            task_id: self.run.task_id.clone(),
            display: self.run.display_name(),
            variant: self.run.variant_label(),
            step_id: step.id.clone(),
            exec_id: exec_id.clone(),
            attempt,
            runner_name: runner_name.clone(),
            profile: profile.clone(),
            worktree: worktree.clone(),
            workspace_id: self.run.herdr.workspace_id.clone(),
            prompt: full.clone(),
            followup_prompt: followup,
            output_file: output_file.clone(),
            log_path: log_path.clone(),
            timeout,
            startup_timeout: self.cfg.config.limits.agent_startup_timeout.as_duration(),
            env: child_env,
            previous: previous.clone(),
            pane_env: self.orch_env(&step.id),
        };
        {
            let e = self.exec_mut(&exec_id);
            e.runner = Some(runner_name.clone());
            e.intent = Some(format!("run agent {runner_name} ({:?}) in {}", profile.mode, worktree.display()));
            e.log_path = Some(log_path.clone());
        }
        let _ = std::fs::remove_file(&output_file); // never read a stale result
        let slot = match self.ctx.agent_slots.acquire(&self.cancel) {
            Some(s) => s,
            None => return Ok(StepOutcome::Cancelled),
        };
        let reattach = resumed.as_ref().and_then(|e| e.agent.clone()).filter(|b| b.prompt_sent);
        if self.exec_mut(&exec_id).status == StepStatus::Pending {
            self.set_exec_status(&exec_id, StepStatus::Starting)?;
            self.audit("step_started", Actor::orchestrator(), Some(&step.id), serde_json::json!({"exec_id": exec_id, "attempt": attempt, "runner": runner_name, "type": "agent"}));
        }
        let deadline = Instant::now() + timeout;
        let cancel = self.cancel.clone();
        let result = {
            let exec = exec_id.clone();
            let step_id = step.id.clone();
            let runner_label = runner_name.clone();
            let prompt_hash = crate::store::sha256_hex(full.as_bytes());
            let mut events = |ev: AgentEvent| {
                if let Err(e) = self.on_agent_event(&exec, &step_id, &runner_label, &prompt_hash, ev) {
                    tracing::error!("persisting agent event failed: {e:#}");
                }
            };
            match reattach {
                Some(mut b) => {
                    events(AgentEvent::Started(b.clone()));
                    runner.attach(&req, &mut b, deadline, &cancel, &mut events)
                }
                None => {
                    // Reuse the recovered binding if a crash hit between launch and prompt.
                    let mut req2 = req.clone();
                    if let Some(b) = resumed.as_ref().and_then(|e| e.agent.clone()) {
                        req2.previous = Some(b);
                    }
                    match runner.start(&req2, &cancel, &mut events) {
                        Ok(mut b) => runner.send_and_wait(&req2, &mut b, deadline, &cancel, &mut events),
                        Err(e) => Err(e),
                    }
                }
            }
        };
        drop(slot);
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                let why = format!("{e:#}");
                self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                return Ok(StepOutcome::Failed { reason: format!("agent {runner_name} error: {why}"), feedback: String::new() });
            }
        };
        // Persist transcript (redacted) and usage.
        if !outcome.transcript.is_empty() {
            if let Some(d) = log_path.parent() {
                std::fs::create_dir_all(d)?;
            }
            let mut existing = std::fs::read_to_string(&log_path).unwrap_or_default();
            if !existing.is_empty() {
                existing.push_str("\n----- transcript -----\n");
            }
            existing.push_str(&redact_str(&outcome.transcript));
            std::fs::write(&log_path, existing)?;
        }
        {
            let e = self.exec_mut(&exec_id);
            e.agent = Some(outcome.binding.clone());
            e.usage = Some(outcome.usage.clone());
            e.exit_code = outcome.exit_code;
        }
        self.audit("usage_recorded", Actor::agent(&runner_name, outcome.binding.pane_id.clone()), Some(&step.id), serde_json::to_value(&outcome.usage)?);

        match &outcome.end {
            AgentEnd::Completed => {}
            AgentEnd::Cancelled => {
                self.finish_exec(&exec_id, StepStatus::Cancelled, None)?;
                return Ok(StepOutcome::Cancelled);
            }
            AgentEnd::Stalled(why) => {
                self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                return Ok(StepOutcome::NeedsHuman(format!("agent for `{}` did not start working: {why}; inspect its pane", step.id)));
            }
            other => {
                let why = match other {
                    AgentEnd::TimedOut => format!("agent timed out after {}", crate::config::HumanDuration(timeout)),
                    AgentEnd::Failed(m) => format!("agent failed: {m}"),
                    AgentEnd::Lost(m) => format!("agent lost: {m}"),
                    _ => unreachable!(),
                };
                self.audit("agent_failed", Actor::agent(&runner_name, outcome.binding.pane_id.clone()), Some(&step.id), serde_json::json!({"reason": why}));
                self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                let tail = crate::checks::excerpt(&redact_str(&outcome.transcript), 0, 40, self.cfg.config.output.feedback_max_bytes);
                return Ok(StepOutcome::Failed { reason: why, feedback: tail });
            }
        }

        // Structured output.
        let raw = outcome.output_file_text.clone();
        let output_text;
        let mut gate_failure = None;
        if review {
            match raw.as_deref().map(parse_review) {
                Some(Ok(v)) => {
                    let verdict = v["verdict"].as_str().unwrap_or("").to_string();
                    let n = v["findings"].as_array().map(|a| a.len()).unwrap_or(0);
                    self.audit("review_completed", Actor::agent(&runner_name, outcome.binding.pane_id.clone()), Some(&step.id), serde_json::json!({"verdict": verdict, "findings": n, "structured": true}));
                    output_text = Some(serde_json::to_string_pretty(&v)?);
                    if *gate && verdict != "approved" {
                        gate_failure = Some((format!("review verdict `{verdict}` with {n} finding(s)"), serde_json::to_string_pretty(&v["findings"])?));
                    }
                    self.exec_mut(&exec_id).structured = Some(v);
                }
                other => {
                    // Keep the raw output but mark parsing as failed.
                    let err = match other {
                        Some(Err(e)) => format!("{e:#}"),
                        _ => "agent did not write the review output file".into(),
                    };
                    self.exec_mut(&exec_id).parse_failed = true;
                    self.audit("review_completed", Actor::agent(&runner_name, outcome.binding.pane_id.clone()), Some(&step.id), serde_json::json!({"structured": false, "parse_error": err}));
                    output_text = Some(redact_str(raw.as_deref().unwrap_or(&outcome.transcript)));
                    if *gate {
                        gate_failure = Some((format!("gated review output could not be validated: {err}"), String::new()));
                    }
                }
            }
        } else {
            let text = raw
                .as_deref()
                .and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok())
                .and_then(|v| v.get("summary").and_then(|s| s.as_str()).map(String::from))
                .or(raw.clone())
                .unwrap_or_else(|| crate::checks::excerpt(&outcome.transcript, 0, 30, 4000));
            if raw.is_none() {
                self.exec_mut(&exec_id).parse_failed = true;
            }
            output_text = Some(redact_str(&text));
        }
        self.exec_mut(&exec_id).output_excerpt = output_text.clone().map(|t| crate::checks::excerpt(&t, 20, 20, 4000));
        // Handoff files never leave the orchestrator dir; keep a copy in logs.
        if let Some(r) = &raw {
            let _ = std::fs::write(log_path.with_extension("output.json"), redact_str(r));
        }

        // Diff-based policy enforcement (catches files the agent wrote itself).
        if *policy_check {
            match self.diff_policy_gate(step, &exec_id)? {
                GateResult::Ok => {}
                GateResult::Stop(o) => return Ok(o),
            }
        }
        // Commit agent changes.
        if commit.unwrap_or(self.cfg.config.git.auto_commit) {
            if let Some(o) = self.commit_changes(step, &exec_id, None)? {
                return Ok(o);
            }
        }
        if let Some((reason, feedback)) = gate_failure {
            self.finish_exec(&exec_id, StepStatus::Failed, Some(reason.clone()))?;
            return Ok(StepOutcome::Failed { reason, feedback });
        }
        self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
        Ok(StepOutcome::Succeeded { output: output_text })
    }

    fn on_agent_event(&mut self, exec_id: &str, step_id: &str, runner: &str, prompt_hash: &str, ev: AgentEvent) -> Result<()> {
        match ev {
            AgentEvent::Started(b) => {
                let first = self.exec_mut(exec_id).agent.is_none();
                let pane = b.pane_id.clone();
                self.exec_mut(exec_id).agent = Some(b.clone());
                if self.exec_mut(exec_id).status == StepStatus::Starting {
                    self.set_exec_status(exec_id, StepStatus::Running)?;
                } else {
                    self.save()?;
                }
                if first {
                    self.audit(
                        "agent_started",
                        Actor::agent(runner, pane),
                        Some(step_id),
                        serde_json::json!({"mode": b.mode, "agent_name": b.agent_name, "kind": b.agent_kind, "pane_id": b.pane_id, "workspace_id": b.workspace_id}),
                    );
                } else if b.terminal_id.is_some() || b.last_status.is_some() {
                    self.audit("agent_attached", Actor::agent(runner, pane), Some(step_id), serde_json::json!({"agent_name": b.agent_name, "status": b.last_status}));
                }
            }
            AgentEvent::PromptSending => {
                // Write-ahead: a crash after this point means "maybe sent".
                if let Some(a) = self.exec_mut(exec_id).agent.as_mut() {
                    a.prompt_sent = true;
                }
                self.save()?;
            }
            AgentEvent::PromptSent => {
                let pane = self.exec_mut(exec_id).agent.as_ref().and_then(|a| a.pane_id.clone());
                self.audit("agent_prompt_sent", Actor::orchestrator(), Some(step_id), serde_json::json!({"prompt_sha256": prompt_hash, "pane_id": pane}));
            }
            AgentEvent::Blocked(why) => {
                if self.exec_mut(exec_id).status == StepStatus::Running {
                    self.set_exec_status(exec_id, StepStatus::AwaitingHuman)?;
                }
                let pane = self.exec_mut(exec_id).agent.as_ref().and_then(|a| a.pane_id.clone());
                self.audit("agent_blocked", Actor::agent(runner, pane.clone()), Some(step_id), serde_json::json!({"reason": why, "pane_id": pane}));
                self.notify(
                    &format!("{} needs you", self.run.display_name()),
                    &format!("{step_id} · {runner} is waiting for input in pane {}", pane.unwrap_or_default()),
                    true,
                );
            }
            AgentEvent::Unblocked => {
                if self.exec_mut(exec_id).status == StepStatus::AwaitingHuman {
                    self.set_exec_status(exec_id, StepStatus::Running)?;
                }
                self.audit("agent_unblocked", Actor::agent(runner, None), Some(step_id), serde_json::json!({}));
            }
        }
        Ok(())
    }

    // ============================================================ command

    fn command_step(&mut self, step: &Step) -> Result<StepOutcome> {
        let (argv_tpl, shell, env_tpl, cwd_rel) = step.command().map(|(a, s, e, c)| (a.clone(), s, e.clone(), c.cloned())).unwrap();
        let is_check = matches!(step.spec, StepSpec::Check { .. });
        let worktree = self.worktree()?;
        let tctx = self.template_ctx(&step.id, None);
        // Resolve argv (named check or explicit command).
        let (mut argv, source) = if let Some(name) = step.named_check() {
            match crate::checks::resolve(name, &worktree, &self.cfg.config.checks) {
                Some(r) => (r.argv, Some(r.source)),
                None => {
                    let exec_id = self.new_exec(step)?;
                    self.finish_exec(&exec_id, StepStatus::Skipped, Some(format!("no `{name}` command configured or detected")))?;
                    return Ok(StepOutcome::Skipped(format!("no `{name}` check configured (set checks.{name} in .ai/herdr-orchestrator/config.yaml) or detected")));
                }
            }
        } else {
            let mut v = vec![];
            for a in &argv_tpl {
                v.push(tctx.render(a)?);
            }
            (v, None)
        };
        let display = crate::policies::command::display_argv(&argv);
        let exec_id = match self.open_exec(step) {
            Some(e) => e.exec_id,
            None => self.new_exec(step)?,
        };
        let attempt = self.exec_mut(&exec_id).attempt;
        self.audit("command_requested", Actor::orchestrator(), Some(&step.id), serde_json::json!({"exec_id": exec_id, "argv": argv, "shell": shell, "source": source, "attempt": attempt}));
        // Pre-flight policy.
        let mut subject = Subject::command(&argv, shell);
        subject.step_id = Some(step.id.clone());
        subject.branch = self.run.git.branch.clone();
        subject.cwd = Some(worktree.clone());
        subject.repo = Some(self.run.repo_root.display().to_string());
        let d = self.policy.evaluate(&subject);
        self.record_policy(Some(&step.id), Some(&exec_id), &d);
        match d.decision {
            Decision::Deny => {
                let why = format!("policy denied `{display}`: {}", d.reason);
                self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                // Fail safely without retry routing: a denied command would
                // only be denied again. The run stops for a human.
                return Ok(StepOutcome::Blocked(why));
            }
            Decision::RequireApproval => {
                if !self.cover_applies(&step.id) {
                    match self.request_approval(step, &exec_id, ApprovalKind::Policy, format!("Command requires approval: {}", d.reason), Some(format!("execute `{display}` in {}", worktree.display())), vec![d.clone()])? {
                        Approval::Granted => {}
                        Approval::Denied(why) => {
                            self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                            return Ok(StepOutcome::Failed { reason: why, feedback: String::new() });
                        }
                        Approval::Cancelled => return Ok(StepOutcome::Cancelled),
                    }
                }
            }
            Decision::Allow => {}
        }
        if self.run.dry_run {
            self.audit("dry_run", Actor::orchestrator(), Some(&step.id), serde_json::json!({"would": format!("execute {display}")}));
            self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
            return Ok(StepOutcome::Succeeded { output: None });
        }
        if argv.first().map(|a| a == "herdr-orchestrator").unwrap_or(false) {
            if let Ok(exe) = std::env::current_exe() {
                argv[0] = exe.display().to_string();
            }
        }
        let run_argv = if shell { vec!["/bin/sh".to_string(), "-c".to_string(), argv.join(" ")] } else { argv.clone() };
        let sandbox = crate::security::sandbox::sandbox_for(self.cfg.config.sandbox.kind)?;
        let run_argv = sandbox.wrap(&run_argv, &worktree)?;
        let cwd = match &cwd_rel {
            Some(c) => {
                let rel = crate::security::ensure_inside(&worktree, std::path::Path::new(c))?;
                worktree.join(rel)
            }
            None => worktree.clone(),
        };
        let mut extra = self.orch_env(&step.id);
        for (k, v) in &env_tpl {
            extra.insert(k.clone(), tctx.render(v)?);
        }
        let env = self.cfg.config.environment.build(std::env::vars(), &extra);
        let log_path = self.ctx.store.layout.run_logs_dir(&self.run.run_id).join(format!("{exec_id}.log"));
        let timeout = self.step_timeout(step, self.cfg.config.limits.command_timeout.as_duration());
        let spec = crate::process::Spec::new(run_argv.clone(), &cwd).env(env).timeout(timeout).log(log_path.clone());
        {
            let e = self.exec_mut(&exec_id);
            e.intent = Some(format!("execute {display}"));
            e.log_path = Some(log_path.clone());
        }
        if self.exec_mut(&exec_id).status == StepStatus::Pending {
            self.set_exec_status(&exec_id, StepStatus::Starting)?; // write-ahead
        }
        self.audit("step_started", Actor::orchestrator(), Some(&step.id), serde_json::json!({"exec_id": exec_id, "attempt": attempt, "type": if is_check {"check"} else {"command"}}));
        let started = match crate::process::spawn(&spec) {
            Ok(s) => s,
            Err(e) => {
                let why = format!("{e:#}");
                self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                return Ok(StepOutcome::Failed { reason: why.clone(), feedback: why });
            }
        };
        self.exec_mut(&exec_id).process = Some(ProcessBinding { pid: started.pid, argv: run_argv.clone(), cwd: cwd.clone(), started_at: now() });
        self.set_exec_status(&exec_id, StepStatus::Running)?;
        self.audit("command_started", Actor::orchestrator(), Some(&step.id), serde_json::json!({"exec_id": exec_id, "argv": run_argv, "pid": started.pid, "cwd": cwd, "env_keys": spec.env.keys().collect::<Vec<_>>()}));
        let out = crate::process::wait(&spec, started, Some(&self.cancel))?;
        let combined = format!("{}{}", out.stdout_str(), if out.stderr.is_empty() { String::new() } else { format!("\n[stderr]\n{}", out.stderr_str()) });
        let o = &self.cfg.config.output;
        let excerpt = crate::checks::excerpt(&redact_str(&combined), o.excerpt_head_lines, o.excerpt_tail_lines, o.feedback_max_bytes);
        {
            let e = self.exec_mut(&exec_id);
            e.exit_code = out.exit_code;
            e.output_excerpt = Some(excerpt.clone());
            e.usage = Some(UsageRecord { source: UsageSource::Measured, runtime_ms: Some(out.duration.as_millis() as u64), ..Default::default() });
        }
        self.audit("command_completed", Actor::orchestrator(), Some(&step.id), serde_json::json!({"exec_id": exec_id, "exit_code": out.exit_code, "timed_out": out.timed_out, "cancelled": out.cancelled, "duration_ms": out.duration.as_millis() as u64}));
        if out.cancelled {
            self.finish_exec(&exec_id, StepStatus::Cancelled, None)?;
            return Ok(StepOutcome::Cancelled);
        }
        if out.success() {
            if is_check {
                self.audit("check_passed", Actor::orchestrator(), Some(&step.id), serde_json::json!({"argv": argv}));
            }
            self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
            return Ok(StepOutcome::Succeeded { output: Some(excerpt) });
        }
        let reason = if out.timed_out {
            format!("`{display}` timed out after {}", crate::config::HumanDuration(timeout))
        } else {
            format!("`{display}` exited with status {}", out.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()))
        };
        if is_check {
            self.audit("check_failed", Actor::orchestrator(), Some(&step.id), serde_json::json!({"argv": argv, "exit_code": out.exit_code, "timed_out": out.timed_out}));
        }
        self.finish_exec(&exec_id, StepStatus::Failed, Some(reason.clone()))?;
        let feedback = format!("Command: {display}\nExit status: {}\nOutput (truncated):\n{excerpt}", out.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none (timeout/signal)".into()));
        Ok(StepOutcome::Failed { reason, feedback })
    }

    // =========================================================== approval

    fn approval_context(&self, step: &Step, pending_action: Option<String>, policy: Vec<PolicyDecision>) -> ApprovalContext {
        let (files, ins, del) = match (&self.run.git.worktree_path, &self.run.git.base_sha) {
            (Some(w), Some(b)) if w.exists() => match crate::git::changed_files(w, b) {
                Ok(d) => (d.files, d.insertions, d.deletions),
                Err(_) => (vec![], 0, 0),
            },
            _ => (vec![], 0, 0),
        };
        let checks = self
            .run
            .steps
            .iter()
            .filter(|e| e.kind == StepKind::Command || e.kind == StepKind::Agent)
            .map(|e| format!("{} (attempt {}): {}", e.step_id, e.attempt, e.status.as_str()))
            .collect();
        let last_agent = self.run.steps.iter().rev().find_map(|e| e.agent.clone());
        ApprovalContext {
            task_title: self.task.title.clone(),
            workflow: self.wf.name.clone(),
            step_id: step.id.clone(),
            agent: last_agent.as_ref().and_then(|a| a.agent_kind.clone()),
            pane_id: last_agent.and_then(|a| a.pane_id),
            repository: self.run.repo_root.clone(),
            branch: self.run.git.branch.clone(),
            worktree: self.run.git.worktree_path.clone(),
            base_sha: self.run.git.base_sha.clone(),
            head_sha: self.run.git.worktree_path.as_ref().and_then(|w| crate::git::head_sha(w).ok()),
            changed_files: files,
            insertions: ins,
            deletions: del,
            checks,
            policy,
            pending_action,
        }
    }

    /// Create (or resume) an approval request and wait for the decision.
    fn request_approval(
        &mut self,
        step: &Step,
        exec_id: &str,
        kind: ApprovalKind,
        reason: String,
        pending_action: Option<String>,
        policy: Vec<PolicyDecision>,
    ) -> Result<Approval> {
        // Resume an existing pending request for this execution (restart).
        let existing = self.exec_mut(exec_id).approval_id.clone().and_then(|id| self.ctx.store.load_approval(&id).ok()).filter(|a| a.status == ApprovalStatus::Pending);
        let approval = match existing {
            Some(a) => a,
            None => {
                let mut a = ApprovalRequest::new(&self.run.run_id, &self.run.task_id, &step.id, kind, reason.clone(), self.approval_context(step, pending_action.clone(), policy));
                a.exec_id = Some(exec_id.to_string());
                if let Some(t) = self.cfg.config.limits.approval_timeout {
                    a.expires_at = Some(now() + chrono::Duration::from_std(t.as_duration()).unwrap_or_default());
                }
                self.ctx.store.save_approval(&a)?;
                self.run.approvals.push(a.approval_id.clone());
                self.exec_mut(exec_id).approval_id = Some(a.approval_id.clone());
                self.audit(
                    "approval_requested",
                    Actor::orchestrator(),
                    Some(&step.id),
                    serde_json::json!({"approval_id": a.approval_id, "kind": kind, "reason": reason, "pending_action": pending_action, "files": a.context.changed_files.len()}),
                );
                self.notify(&format!("{} awaits approval", self.run.display_name()), &format!("{}: {reason}", step.id), true);
                a
            }
        };
        if self.exec_mut(exec_id).status.can_transition_to(StepStatus::AwaitingApproval) {
            self.set_exec_status(exec_id, StepStatus::AwaitingApproval)?;
        }
        self.set_status(RunStatus::AwaitingApproval, Some(format!("approval {} pending", approval.approval_id)))?;
        let id = approval.approval_id.clone();
        let decision = loop {
            if self.check_cancel() {
                break Approval::Cancelled;
            }
            let a = self.ctx.store.load_approval(&id)?;
            match a.status {
                ApprovalStatus::Approved => {
                    self.audit("approval_granted", Actor::human(a.decided_by.clone()), Some(&step.id), serde_json::json!({"approval_id": id, "note": a.decision_note}));
                    break Approval::Granted;
                }
                ApprovalStatus::Denied => {
                    self.audit("approval_denied", Actor::human(a.decided_by.clone()), Some(&step.id), serde_json::json!({"approval_id": id, "note": a.decision_note}));
                    break Approval::Denied(format!("approval denied by {}{}", a.decided_by.clone().unwrap_or_else(|| "human".into()), a.decision_note.map(|n| format!(": {n}")).unwrap_or_default()));
                }
                ApprovalStatus::Cancelled => break Approval::Cancelled,
                ApprovalStatus::Expired => break Approval::Denied("approval expired".into()),
                ApprovalStatus::Pending => {
                    if a.expires_at.is_some_and(|t| now() >= t) {
                        let _ = self.ctx.store.update_approval(&id, |x| {
                            x.status = ApprovalStatus::Expired;
                            x.decided_at = Some(now());
                            Ok(())
                        });
                        self.audit("approval_expired", Actor::orchestrator(), Some(&step.id), serde_json::json!({"approval_id": id}));
                        break Approval::Denied("approval expired".into());
                    }
                }
            }
            std::thread::sleep(self.ctx.poll);
        };
        if !matches!(decision, Approval::Cancelled) {
            self.set_status(RunStatus::Running, None)?;
            if self.exec_mut(exec_id).status == StepStatus::AwaitingApproval && matches!(decision, Approval::Granted) {
                self.set_exec_status(exec_id, StepStatus::Running)?;
            }
        }
        Ok(decision)
    }

    /// An explicit approval step immediately before `step_id` covers its
    /// REQUIRE_APPROVAL decisions while HEAD is unchanged.
    fn cover_applies(&mut self, step_id: &str) -> bool {
        let Some(c) = self.run.approval_cover.clone() else { return false };
        if c.step_id != step_id {
            return false;
        }
        let head = self.run.git.worktree_path.as_ref().and_then(|w| crate::git::head_sha(w).ok());
        if head != c.head_sha {
            return false;
        }
        self.audit("approval_reused", Actor::orchestrator(), Some(step_id), serde_json::json!({"approval_id": c.approval_id}));
        true
    }

    fn approval_step(&mut self, step: &Step, reason_tpl: &str) -> Result<StepOutcome> {
        let reason = self.template_ctx(&step.id, None).render(reason_tpl)?;
        let exec_id = match self.open_exec(step) {
            Some(e) => e.exec_id,
            None => self.new_exec(step)?,
        };
        let idx = self.wf.step_index(&step.id).unwrap();
        let next = self.wf.steps.get(idx + 1).cloned();
        let pending_action = next.as_ref().map(|n| match &n.spec {
            StepSpec::GithubPr { draft, .. } => format!(
                "push branch {} to {} and open a {}PR",
                self.run.git.branch.clone().unwrap_or_default(),
                self.cfg.config.git.remote,
                if draft.unwrap_or(self.cfg.config.github.draft_pr) { "draft " } else { "" }
            ),
            StepSpec::Git { action: GitAction::Push, .. } => format!("push branch {}", self.run.git.branch.clone().unwrap_or_default()),
            _ => format!("continue with step `{}` ({})", n.id, n.kind().as_str()),
        });
        if self.run.dry_run {
            self.audit("dry_run", Actor::orchestrator(), Some(&step.id), serde_json::json!({"would": "request human approval", "reason": reason}));
            self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
            return Ok(StepOutcome::Succeeded { output: None });
        }
        match self.request_approval(step, &exec_id, ApprovalKind::WorkflowStep, reason, pending_action, vec![])? {
            Approval::Granted => {
                let approval_id = self.exec_mut(&exec_id).approval_id.clone().unwrap_or_default();
                if let Some(n) = next {
                    self.run.approval_cover = Some(ApprovalCover {
                        step_id: n.id.clone(),
                        approval_id,
                        head_sha: self.run.git.worktree_path.as_ref().and_then(|w| crate::git::head_sha(w).ok()),
                    });
                }
                self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
                Ok(StepOutcome::Succeeded { output: None })
            }
            Approval::Denied(why) => {
                self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                Ok(StepOutcome::Failed { reason: why, feedback: String::new() })
            }
            Approval::Cancelled => Ok(StepOutcome::Cancelled),
        }
    }

    // ============================================================= policy

    fn policy_step(&mut self, step: &Step) -> Result<StepOutcome> {
        let exec_id = match self.open_exec(step) {
            Some(e) => e.exec_id,
            None => self.new_exec(step)?,
        };
        if self.exec_mut(&exec_id).status == StepStatus::Pending {
            self.set_exec_status(&exec_id, StepStatus::Starting)?;
            self.set_exec_status(&exec_id, StepStatus::Running)?;
        }
        if self.run.dry_run {
            self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
            return Ok(StepOutcome::Succeeded { output: None });
        }
        match self.diff_policy_gate(step, &exec_id)? {
            GateResult::Ok => {
                self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
                Ok(StepOutcome::Succeeded { output: None })
            }
            GateResult::Stop(o) => Ok(o),
        }
    }

    /// Evaluate every changed path plus the aggregate diff. DENY blocks the
    /// run; REQUIRE_APPROVAL asks a human (once per path per run).
    fn diff_policy_gate(&mut self, step: &Step, exec_id: &str) -> Result<GateResult> {
        let worktree = self.worktree()?;
        let base = self.run.git.base_sha.clone().context("no base sha")?;
        let diff = crate::git::changed_files(&worktree, &base)?;
        self.run.diff_stat = Some(diff.clone());
        self.exec_mut(exec_id).changed_files = Some(diff.files_changed);
        self.audit(
            "files_changed",
            Actor::orchestrator(),
            Some(&step.id),
            serde_json::json!({"files": diff.files.iter().map(|f| serde_json::json!({"path": f.path, "change": f.change, "+": f.insertions, "-": f.deletions})).collect::<Vec<_>>(), "insertions": diff.insertions, "deletions": diff.deletions}),
        );
        let mut denies = vec![];
        let mut asks: Vec<(String, PolicyDecision)> = vec![];
        let runner = self.exec_mut(exec_id).runner.clone();
        for f in &diff.files {
            // Containment and symlink escapes are violations in themselves.
            let full = worktree.join(&f.path);
            if full.exists() || std::fs::symlink_metadata(&full).is_ok() {
                match crate::security::check_containment(&worktree, std::path::Path::new(&f.path))? {
                    crate::security::Containment::Inside(_) => {}
                    other => {
                        denies.push(format!("{} ({other:?})", f.path));
                        continue;
                    }
                }
            }
            let action = if f.change == "deleted" { Action::Delete } else { Action::Write };
            let mut s = Subject::file(action, &f.path);
            s.lines_changed = Some(f.insertions + f.deletions);
            s.runner = runner.clone();
            s.step_id = Some(step.id.clone());
            s.branch = self.run.git.branch.clone();
            let d = self.policy.evaluate(&s);
            if d.decision != Decision::Allow {
                self.record_policy(Some(&step.id), Some(exec_id), &d);
            }
            match d.decision {
                Decision::Deny => denies.push(format!("{} ({})", f.path, d.reason)),
                Decision::RequireApproval => {
                    let fingerprint = format!("{}:{}:{}", f.change, f.insertions, f.deletions);
                    if self.run.approved_paths.get(&f.path) != Some(&fingerprint) {
                        asks.push((f.path.clone(), d));
                    }
                }
                Decision::Allow => {}
            }
        }
        let summary = Subject {
            action: Some(Action::DiffSummary),
            files_changed: Some(diff.files_changed),
            deleted_files: Some(diff.files.iter().filter(|f| f.change == "deleted").count()),
            deleted_lines: Some(diff.deletions),
            step_id: Some(step.id.clone()),
            ..Default::default()
        };
        let sd = self.policy.evaluate(&summary);
        self.record_policy(Some(&step.id), Some(exec_id), &sd);
        match sd.decision {
            Decision::Deny => denies.push(format!("diff summary ({})", sd.reason)),
            Decision::RequireApproval if !self.run.approved_paths.contains_key("<diff-summary>") => asks.push(("<diff-summary>".into(), sd)),
            _ => {}
        }
        if !denies.is_empty() {
            let why = format!("policy violation in worktree: {}", denies.join("; "));
            self.audit("policy_violation", Actor::policy(), Some(&step.id), serde_json::json!({"denied": denies}));
            self.finish_exec(exec_id, StepStatus::Failed, Some(why.clone()))?;
            return Ok(GateResult::Stop(StepOutcome::Blocked(why)));
        }
        if !asks.is_empty() {
            let paths: Vec<String> = asks.iter().map(|(p, _)| p.clone()).collect();
            let reason = format!("changes need approval: {}", paths.join(", "));
            let decisions = asks.iter().map(|(_, d)| d.clone()).collect();
            match self.request_approval(step, exec_id, ApprovalKind::Policy, reason, Some(format!("accept changes to {} path(s) and continue", paths.len())), decisions)? {
                Approval::Granted => {
                    for f in &diff.files {
                        if paths.contains(&f.path) {
                            self.run.approved_paths.insert(f.path.clone(), format!("{}:{}:{}", f.change, f.insertions, f.deletions));
                        }
                    }
                    if paths.iter().any(|p| p == "<diff-summary>") {
                        self.run.approved_paths.insert("<diff-summary>".into(), String::new());
                    }
                    self.save()?;
                }
                Approval::Denied(why) => {
                    self.finish_exec(exec_id, StepStatus::Failed, Some(why.clone()))?;
                    return Ok(GateResult::Stop(StepOutcome::Failed { reason: why, feedback: String::new() }));
                }
                Approval::Cancelled => return Ok(GateResult::Stop(StepOutcome::Cancelled)),
            }
        }
        Ok(GateResult::Ok)
    }

    // ================================================================ git

    /// Commit all worktree changes. Returns `Some(outcome)` to stop the step.
    fn commit_changes(&mut self, step: &Step, exec_id: &str, message: Option<&str>) -> Result<Option<StepOutcome>> {
        let worktree = self.worktree()?;
        if !crate::git::is_dirty(&worktree)? {
            return Ok(None);
        }
        let s = Subject { action: Some(Action::GitCommit), step_id: Some(step.id.clone()), branch: self.run.git.branch.clone(), ..Default::default() };
        let d = self.policy.evaluate(&s);
        self.record_policy(Some(&step.id), Some(exec_id), &d);
        if d.decision == Decision::Deny {
            let why = format!("policy denied commit: {}", d.reason);
            self.finish_exec(exec_id, StepStatus::Failed, Some(why.clone()))?;
            return Ok(Some(StepOutcome::Blocked(why)));
        }
        let tpl = message.unwrap_or(&self.cfg.config.git.commit_message).to_string();
        let msg = self.template_ctx(&step.id, None).render(&tpl)?;
        if let Some(sha) = crate::git::commit_all(&worktree, &msg)? {
            self.run.git.commits.push(sha.clone());
            self.run.git.head_sha = Some(sha.clone());
            self.save()?;
            self.audit("git_commit_created", Actor::orchestrator(), Some(&step.id), serde_json::json!({"sha": sha, "message": msg}));
        }
        Ok(None)
    }

    fn git_step(&mut self, step: &Step, action: GitAction, message: Option<&str>) -> Result<StepOutcome> {
        let exec_id = match self.open_exec(step) {
            Some(e) => e.exec_id,
            None => self.new_exec(step)?,
        };
        if self.exec_mut(&exec_id).status == StepStatus::Pending {
            self.set_exec_status(&exec_id, StepStatus::Starting)?;
            self.set_exec_status(&exec_id, StepStatus::Running)?;
        }
        if self.run.dry_run {
            self.audit("dry_run", Actor::orchestrator(), Some(&step.id), serde_json::json!({"would": format!("git {action:?}")}));
            self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
            return Ok(StepOutcome::Succeeded { output: None });
        }
        match action {
            GitAction::Commit => {
                if let Some(o) = self.commit_changes(step, &exec_id, message)? {
                    return Ok(o);
                }
            }
            GitAction::Push => {
                if let Some(o) = self.push_branch(step, &exec_id)? {
                    return Ok(o);
                }
            }
        }
        self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
        Ok(StepOutcome::Succeeded { output: None })
    }

    fn push_branch(&mut self, step: &Step, exec_id: &str) -> Result<Option<StepOutcome>> {
        let worktree = self.worktree()?;
        let branch = self.run.git.branch.clone().context("no branch")?;
        let remote = self.cfg.config.git.remote.clone();
        let argv: Vec<String> = ["git", "push", "--set-upstream", &remote, &branch].iter().map(|s| s.to_string()).collect();
        let mut s = Subject::command(&argv, false);
        s.action = Some(Action::GitPush);
        s.branch = Some(branch.clone());
        s.step_id = Some(step.id.clone());
        let d = self.policy.evaluate(&s);
        self.record_policy(Some(&step.id), Some(exec_id), &d);
        match d.decision {
            Decision::Deny => {
                let why = format!("policy denied push: {}", d.reason);
                self.finish_exec(exec_id, StepStatus::Failed, Some(why.clone()))?;
                return Ok(Some(StepOutcome::Blocked(why)));
            }
            Decision::RequireApproval if !self.cover_applies(&step.id) => {
                match self.request_approval(step, exec_id, ApprovalKind::Policy, format!("Push requires approval: {}", d.reason), Some(format!("git push {remote} {branch}")), vec![d.clone()])? {
                    Approval::Granted => {}
                    Approval::Denied(why) => {
                        self.finish_exec(exec_id, StepStatus::Failed, Some(why.clone()))?;
                        return Ok(Some(StepOutcome::Failed { reason: why, feedback: String::new() }));
                    }
                    Approval::Cancelled => return Ok(Some(StepOutcome::Cancelled)),
                }
            }
            _ => {}
        }
        self.audit("git_push_started", Actor::orchestrator(), Some(&step.id), serde_json::json!({"remote": remote, "branch": branch}));
        crate::git::push_branch(&worktree, &remote, &branch)?;
        self.audit("git_pushed", Actor::orchestrator(), Some(&step.id), serde_json::json!({"remote": remote, "branch": branch}));
        Ok(None)
    }

    // ============================================================= github

    fn pr_step(&mut self, step: &Step, draft: Option<bool>, title: Option<&str>, body: Option<&str>, base: Option<&str>) -> Result<StepOutcome> {
        let exec_id = match self.open_exec(step) {
            Some(e) => e.exec_id,
            None => self.new_exec(step)?,
        };
        if self.exec_mut(&exec_id).status == StepStatus::Pending {
            self.set_exec_status(&exec_id, StepStatus::Starting)?;
            self.set_exec_status(&exec_id, StepStatus::Running)?;
        }
        let draft = draft.unwrap_or(self.cfg.config.github.draft_pr);
        let branch = self.run.git.branch.clone().context("no branch")?;
        let base = base.map(String::from).or(self.cfg.config.github.base.clone()).unwrap_or_else(|| {
            let b = self.run.git.base_ref.clone();
            if b == "HEAD" { "main".into() } else { b }
        });
        let d = self.policy.evaluate(&Subject { action: Some(Action::GithubPr), branch: Some(branch.clone()), step_id: Some(step.id.clone()), ..Default::default() });
        self.record_policy(Some(&step.id), Some(&exec_id), &d);
        if self.run.dry_run {
            self.audit("dry_run", Actor::orchestrator(), Some(&step.id), serde_json::json!({"would": format!("open {}PR {branch} → {base}", if draft {"draft "} else {""}), "policy": d.decision.as_str()}));
            self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
            return Ok(StepOutcome::Succeeded { output: None });
        }
        let worktree = self.worktree()?;
        if !self.ctx.gh.available() {
            let why = "GitHub CLI `gh` is not installed".to_string();
            self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
            return Ok(StepOutcome::Failed { reason: why, feedback: String::new() });
        }
        // Idempotency: an existing PR for this branch completes the step.
        match self.ctx.gh.pr_for_branch(&worktree, &branch) {
            Ok(Some(pr)) => {
                self.record_pr(step, &exec_id, &pr.url, true)?;
                return Ok(StepOutcome::Succeeded { output: Some(pr.url) });
            }
            Ok(None) => {}
            Err(e) => {
                let why = format!("could not query existing PRs: {e:#}");
                self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                return Ok(StepOutcome::Failed { reason: why, feedback: String::new() });
            }
        }
        match d.decision {
            Decision::Deny => {
                let why = format!("policy denied PR creation: {}", d.reason);
                self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                return Ok(StepOutcome::Blocked(why));
            }
            Decision::RequireApproval if !self.cover_applies(&step.id) => {
                match self.request_approval(step, &exec_id, ApprovalKind::Policy, format!("Opening a PR requires approval: {}", d.reason), Some(format!("push {branch} and open a {}PR into {base}", if draft { "draft " } else { "" })), vec![d.clone()])? {
                    Approval::Granted => {
                        // The explicit decision covers the push that follows.
                        let id = self.exec_mut(&exec_id).approval_id.clone().unwrap_or_default();
                        self.run.approval_cover = Some(ApprovalCover { step_id: step.id.clone(), approval_id: id, head_sha: crate::git::head_sha(&worktree).ok() });
                    }
                    Approval::Denied(why) => {
                        self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                        return Ok(StepOutcome::Failed { reason: why, feedback: String::new() });
                    }
                    Approval::Cancelled => return Ok(StepOutcome::Cancelled),
                }
            }
            _ => {}
        }
        if let Some(o) = self.commit_changes(step, &exec_id, None)? {
            return Ok(o);
        }
        if self.cfg.config.github.push_before_pr {
            if let Some(o) = self.push_branch(step, &exec_id)? {
                return Ok(o);
            }
        }
        let tctx = self.template_ctx(&step.id, None);
        let title = match title {
            Some(t) => tctx.render(t)?,
            None => self.task.title.clone(),
        };
        let body = match body {
            Some(b) => tctx.render(b)?,
            None => self.pr_body(),
        };
        self.exec_mut(&exec_id).intent = Some(format!("gh pr create --head {branch} --base {base}"));
        self.save()?;
        let url = match self.ctx.gh.pr_create(&worktree, &base, &branch, &title, &body, draft) {
            Ok(u) => u,
            Err(e) => {
                let why = format!("{e:#}");
                self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                return Ok(StepOutcome::Failed { reason: why, feedback: String::new() });
            }
        };
        self.record_pr(step, &exec_id, &url, false)?;
        Ok(StepOutcome::Succeeded { output: Some(url) })
    }

    fn record_pr(&mut self, step: &Step, exec_id: &str, url: &str, existing: bool) -> Result<()> {
        self.run.pr_url = Some(url.to_string());
        self.run.artifacts.push(Artifact { kind: "pull_request".into(), name: "PR".into(), path: None, url: Some(url.into()), step_id: Some(step.id.clone()), created_at: now() });
        self.audit("pr_created", Actor::orchestrator(), Some(&step.id), serde_json::json!({"url": url, "already_existed": existing}));
        self.finish_exec(exec_id, StepStatus::Succeeded, None)?;
        Ok(())
    }

    fn pr_body(&self) -> String {
        let mut b = String::new();
        b.push_str(&format!("{}\n\n", self.task.description.trim()));
        b.push_str("---\n\n");
        b.push_str(&format!("Generated by herdr-orchestrator run `{}` ({} · workflow `{}`).\n\n", self.run.run_id, self.run.display_name(), self.wf.name));
        b.push_str("| step | attempt | status | runner |\n|---|---|---|---|\n");
        for e in &self.run.steps {
            b.push_str(&format!("| {} | {} | {} | {} |\n", e.step_id, e.attempt, e.status.as_str(), e.runner.clone().unwrap_or_default()));
        }
        let (a, r, dn) = self.run.policy_summary();
        b.push_str(&format!("\nPolicy decisions: {a} allow · {r} approval · {dn} deny.\n"));
        if let Some(v) = self.run.steps.iter().rev().find_map(|e| e.structured.as_ref()) {
            if let Some(verdict) = v.get("verdict").and_then(|x| x.as_str()) {
                b.push_str(&format!("Review verdict: **{verdict}**.\n"));
            }
        }
        b.push_str(&format!("\nAudit: `herdr-orchestrator audit verify {}`\n", self.run.run_id));
        redact_str(&b)
    }
}

enum GateResult {
    Ok,
    Stop(StepOutcome),
}
