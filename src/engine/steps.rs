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
        let mut profile = profile;
        if let Err(e) = self.install_claude_hook(&mut profile, &worktree) {
            tracing::warn!("could not install the Claude policy hook: {e:#}");
        }

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
        let full = compose_prompt(skill_text.as_deref(), &body, &self.run, &step.id, attempt, &worktree, &output_file, *output);
        let followup = format!("{}\n\n---\n\n{}", body.trim(), orchestrator_instructions(&self.run, &step.id, attempt, &worktree, &output_file, *output));
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
        // Result files are per execution id, so only a *fresh* execution may
        // clear one. A resumed execution must keep it: the agent may have
        // finished while the engine was down.
        if resumed.is_none() {
            let _ = std::fs::remove_file(&output_file);
        }
        let head_before = crate::git::head_sha(&worktree).ok();
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
        let usage = self.pane_usage(&exec_id, &outcome, &worktree);
        {
            let e = self.exec_mut(&exec_id);
            e.agent = Some(outcome.binding.clone());
            e.usage = Some(usage.clone());
            e.exit_code = outcome.exit_code;
        }
        self.audit("usage_recorded", Actor::agent(&runner_name, outcome.binding.pane_id.clone()), Some(&step.id), serde_json::to_value(&usage)?);

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

        if let Some(o) = self.budget_gate(step, &exec_id)? {
            return Ok(o);
        }

        // Structured output.
        let raw = outcome.output_file_text.clone();
        let output_text;
        let mut gate_failure = None;
        let agent = Actor::agent(&runner_name, outcome.binding.pane_id.clone());
        if *output != AgentOutput::Summary {
            let kind = output.as_str();
            match raw.as_deref().map(|r| self.parse_structured(*output, r)) {
                Some(Ok(v)) => {
                    let (event, data, failure) = self.judge_structured(*output, &v, *gate)?;
                    self.audit(event, agent.clone(), Some(&step.id), data);
                    output_text = Some(serde_json::to_string_pretty(&v)?);
                    gate_failure = failure;
                    self.exec_mut(&exec_id).structured = Some(v);
                }
                other => {
                    // Keep the raw output but mark parsing as failed.
                    let err = match other {
                        Some(Err(e)) => format!("{e:#}"),
                        _ => format!("agent did not write the {kind} output file"),
                    };
                    self.exec_mut(&exec_id).parse_failed = true;
                    let event = if *output == AgentOutput::Review { "review_completed".to_string() } else { format!("{kind}_invalid") };
                    self.audit(&event, agent.clone(), Some(&step.id), serde_json::json!({"structured": false, "parse_error": err}));
                    output_text = Some(redact_str(raw.as_deref().unwrap_or(&outcome.transcript)));
                    // A review may be advisory; a plan, acceptance or
                    // conformance result that cannot be read is useless.
                    if *gate || *output != AgentOutput::Review {
                        gate_failure = Some((format!("{kind} output could not be validated: {err}"), format!("Your {kind} output was rejected:\n{err}\nWrite a corrected result file in the required format.")));
                    }
                }
            }
            if output.read_only() {
                let changed = crate::git::is_dirty(&worktree).unwrap_or(true) || crate::git::head_sha(&worktree).ok() != head_before;
                if changed {
                    let why = format!("the {kind} step must not change files, but the worktree changed");
                    self.audit("read_only_violation", agent.clone(), Some(&step.id), serde_json::json!({"output": kind}));
                    self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                    return Ok(StepOutcome::Blocked(why));
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
                // No result file and nothing changed: the agent most likely
                // never received or acted on the task. Do not report success.
                let unchanged = !crate::git::is_dirty(&worktree).unwrap_or(true) && crate::git::head_sha(&worktree).ok() == head_before;
                if unchanged {
                    let why = "agent finished without writing its result file and without changing anything (was the prompt received?)".to_string();
                    self.audit("agent_no_result", agent.clone(), Some(&step.id), serde_json::json!({"reason": why}));
                    self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                    let tail = crate::checks::excerpt(&redact_str(&outcome.transcript), 0, 40, self.cfg.config.output.feedback_max_bytes);
                    return Ok(StepOutcome::Failed { reason: why, feedback: tail });
                }
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
        if let Some(o) = self.test_only_retry_gate(step, &exec_id, &worktree, head_before.as_deref())? {
            return Ok(o);
        }
        // Commit agent changes.
        if commit.unwrap_or(self.cfg.config.git.auto_commit) {
            if let Some(o) = self.commit_changes(step, &exec_id, None)? {
                return Ok(o);
            }
        }
        if gate_failure.is_none() && *output == AgentOutput::Contract {
            let v = self.exec_mut(&exec_id).structured.clone().unwrap_or_default();
            match self.lock_contract(step, &exec_id, &v, &worktree)? {
                ContractResult::Locked => {}
                ContractResult::Retry(reason, feedback) => gate_failure = Some((reason, feedback)),
                ContractResult::Stop(o) => return Ok(o),
            }
        }
        if let Some((reason, feedback)) = gate_failure {
            self.finish_exec(&exec_id, StepStatus::Failed, Some(reason.clone()))?;
            return Ok(StepOutcome::Failed { reason, feedback });
        }
        self.finish_exec(&exec_id, StepStatus::Succeeded, None)?;
        Ok(StepOutcome::Succeeded { output: output_text })
    }

    /// Red proof and lock: the contract's check must FAIL on the code as it
    /// is (a contract that already passes proves nothing); then the files'
    /// hashes are recorded and every later change to them is denied.
    fn lock_contract(&mut self, step: &Step, exec_id: &str, v: &serde_json::Value, worktree: &std::path::Path) -> Result<ContractResult> {
        let files: Vec<String> = v["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect();
        let check: Vec<String> = match v.get("check").and_then(|c| serde_json::from_value::<Vec<String>>(c.clone()).ok()) {
            Some(c) => c,
            None => match crate::checks::resolve("tests", worktree, &self.cfg.config.checks) {
                Some(r) => r.argv,
                None => return Ok(ContractResult::Retry("the contract names no `check` and no `tests` check is configured".into(), "Add a `check` argv to the result file: the command that runs exactly your contract tests.".into())),
            },
        };
        let (passed, excerpt) = match self.run_probe(step, exec_id, &check, worktree)? {
            Ok(x) => x,
            Err(o) => return Ok(ContractResult::Stop(o)),
        };
        if passed {
            self.audit("contract_not_red", Actor::orchestrator(), Some(&step.id), serde_json::json!({"check": check}));
            return Ok(ContractResult::Retry(
                format!("contract check `{}` already passes before any implementation", crate::policies::command::display_argv(&check)),
                format!("Your contract tests pass on the current code, so they do not pin down the new behaviour. Make them test what the task adds, so they fail now and pass once it is implemented.\nCheck output:\n{excerpt}"),
            ));
        }
        let mut hashes = std::collections::BTreeMap::new();
        for f in &files {
            let b = std::fs::read(worktree.join(f)).with_context(|| format!("reading contract file {f}"))?;
            hashes.insert(f.clone(), format!("sha256:{}", crate::store::sha256_hex(&b)));
        }
        let criteria_map = v.get("criteria_map").and_then(|m| serde_json::from_value(m.clone()).ok()).unwrap_or_default();
        let c = Contract {
            sha256: Contract::combined_hash(&hashes),
            files: hashes,
            check: check.clone(),
            criteria_map,
            commit: crate::git::head_sha(worktree).ok(),
            red_excerpt: excerpt,
            locked_at: now(),
            approval_id: None,
            approved_by: None,
            approved_at: None,
        };
        self.audit("contract_locked", Actor::orchestrator(), Some(&step.id), serde_json::json!({"sha256": c.sha256, "files": c.files, "check": c.check, "commit": c.commit}));
        self.run.contract = Some(c);
        self.save()?;
        Ok(ContractResult::Locked)
    }

    /// Run a probe command (red/green proofs) with the normal policy
    /// pre-flight. `Ok(Ok((passed, excerpt)))`, or `Ok(Err(outcome))` to stop.
    fn run_probe(&mut self, step: &Step, exec_id: &str, argv: &[String], worktree: &std::path::Path) -> Result<std::result::Result<(bool, String), StepOutcome>> {
        let display = crate::policies::command::display_argv(argv);
        let mut subject = Subject::command(argv, false);
        subject.step_id = Some(step.id.clone());
        subject.branch = self.run.git.branch.clone();
        let d = self.policy.evaluate(&subject);
        self.record_policy(Some(&step.id), Some(exec_id), &d);
        match d.decision {
            Decision::Deny => {
                let why = format!("policy denied `{display}`: {}", d.reason);
                self.finish_exec(exec_id, StepStatus::Failed, Some(why.clone()))?;
                return Ok(Err(StepOutcome::Blocked(why)));
            }
            Decision::RequireApproval => match self.request_approval(step, exec_id, ApprovalKind::Policy, format!("Contract check requires approval: {}", d.reason), Some(format!("execute `{display}`")), vec![d.clone()])? {
                Approval::Granted => {}
                Approval::Denied(why) => {
                    self.finish_exec(exec_id, StepStatus::Failed, Some(why.clone()))?;
                    return Ok(Err(StepOutcome::Failed { reason: why, feedback: String::new() }));
                }
                Approval::Cancelled => return Ok(Err(StepOutcome::Cancelled)),
            },
            Decision::Allow => {}
        }
        let sandbox = crate::security::sandbox::sandbox_for(self.cfg.config.sandbox.kind)?;
        let run_argv = sandbox.wrap(argv, worktree)?;
        let env = self.cfg.config.environment.build(std::env::vars(), &self.orch_env(&step.id));
        let log = self.ctx.store.layout.run_logs_dir(&self.run.run_id).join(format!("{exec_id}.probe.log"));
        let spec = crate::process::Spec::new(run_argv, worktree).env(env).timeout(self.cfg.config.limits.command_timeout.as_duration()).log(log);
        let out = crate::process::run(&spec, Some(&self.cancel))?;
        if out.cancelled {
            return Ok(Err(StepOutcome::Cancelled));
        }
        let combined = format!("{}{}", out.stdout_str(), out.stderr_str());
        let excerpt = crate::checks::excerpt(&redact_str(&combined), 10, 30, 4000);
        self.audit("contract_probe", Actor::orchestrator(), Some(&step.id), serde_json::json!({"argv": argv, "exit_code": out.exit_code, "timed_out": out.timed_out}));
        Ok(Ok((out.success(), excerpt)))
    }

    /// Contract files whose content differs from the locked hashes.
    pub(super) fn contract_violations(&self, worktree: &std::path::Path) -> Vec<String> {
        let Some(c) = &self.run.contract else { return vec![] };
        c.files
            .iter()
            .filter(|(p, h)| std::fs::read(worktree.join(p)).map(|b| format!("sha256:{}", crate::store::sha256_hex(&b)) != **h).unwrap_or(true))
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Claude agents get a `PreToolUse` hook (via `--settings`) that checks
    /// every tool call against policy before it runs. The settings file lives
    /// in the git-excluded orchestrator dir, never in the project's `.claude/`.
    fn install_claude_hook(&mut self, profile: &mut crate::runners::RunnerProfile, worktree: &std::path::Path) -> Result<()> {
        use crate::policies::agent_hook::{claude_settings, sh_quote};
        use crate::runners::RunnerMode;
        if !self.cfg.config.guard.claude_hook || profile.kind.as_deref() != Some("claude") || !matches!(profile.mode, RunnerMode::Pane | RunnerMode::Headless) || self.run.dry_run {
            return Ok(());
        }
        let exe = std::env::current_exe()?;
        let paths = &self.ctx.paths;
        let command = format!(
            "HERDR_ORCH_STATE_DIR={} HERDR_ORCH_CONFIG_DIR={} {} hook claude-pretool --run {}",
            sh_quote(&paths.state_dir.display().to_string()),
            sh_quote(&paths.config_dir.display().to_string()),
            sh_quote(&exe.display().to_string()),
            sh_quote(&self.run.run_id)
        );
        let file = worktree.join(crate::git::ORCH_DIR).join("claude-settings.json");
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, serde_json::to_vec_pretty(&claude_settings(&command))?)?;
        let arg = file.display().to_string();
        profile.pane_args.extend(["--settings".to_string(), arg.clone()]);
        if let Some(h) = profile.headless_command.as_mut() {
            // Before the trailing prompt placeholder, if any.
            let at = h.iter().position(|a| a.starts_with("{{")).unwrap_or(h.len());
            h.splice(at..at, ["--settings".to_string(), arg]);
        }
        Ok(())
    }

    /// Pane agents report no usage themselves; read it from the agent's own
    /// session log for this execution's time window (if enabled).
    fn pane_usage(&mut self, exec_id: &str, outcome: &crate::runners::AgentOutcome, worktree: &std::path::Path) -> UsageRecord {
        let mut usage = outcome.usage.clone();
        let started = self.exec_mut(exec_id).started_at;
        if usage.runtime_ms.is_none() {
            usage.runtime_ms = started.map(|t| (now() - t).num_milliseconds().max(0) as u64);
        }
        if usage.source != UsageSource::Unknown || outcome.binding.mode != "pane" || !self.cfg.config.usage.session_logs {
            return usage;
        }
        let Some(kind) = outcome.binding.agent_kind.as_deref() else { return usage };
        // A little slack: the agent may log a line just before the state flip.
        let since = started.unwrap_or_else(now) - chrono::Duration::seconds(2);
        match crate::telemetry::sessions::pane_usage(kind, outcome.binding.agent_session.as_deref(), worktree, since, now()) {
            Some(u) => UsageRecord { runtime_ms: usage.runtime_ms, ..u },
            None => usage,
        }
    }

    /// `limits.max_tokens` / `limits.max_cost_usd` over the whole run. Only
    /// numbers a provider reported (or that were read from its logs) count;
    /// unknown usage is never treated as "under budget", it is just not
    /// enforceable. Exceeding asks a human once per run whether to continue.
    fn budget_gate(&mut self, step: &Step, exec_id: &str) -> Result<Option<StepOutcome>> {
        let l = &self.cfg.config.limits;
        let Some(why) = crate::telemetry::budget_warning(&self.run.usage_total(), l.max_cost_usd, l.max_tokens) else { return Ok(None) };
        if self.run.approved_paths.contains_key("<budget>") {
            return Ok(None);
        }
        self.audit("budget_exceeded", Actor::orchestrator(), Some(&step.id), serde_json::json!({"reason": why}));
        match self.request_approval(step, exec_id, ApprovalKind::Policy, format!("Budget exceeded: {why}"), Some("continue this run past its token/cost budget".into()), vec![])? {
            Approval::Granted => {
                self.run.approved_paths.insert("<budget>".into(), why);
                self.save()?;
                Ok(None)
            }
            Approval::Denied(d) => {
                self.finish_exec(exec_id, StepStatus::Failed, Some(d.clone()))?;
                Ok(Some(StepOutcome::Blocked(format!("stopped at the budget limit: {d}"))))
            }
            Approval::Cancelled => Ok(Some(StepOutcome::Cancelled)),
        }
    }

    /// A check failed and its feedback went back to this agent; if the new
    /// attempt changed nothing but tests or test configuration, the agent may
    /// have made the check pass by weakening it. Ask before going on.
    fn test_only_retry_gate(&mut self, step: &Step, exec_id: &str, worktree: &std::path::Path, head_before: Option<&str>) -> Result<Option<StepOutcome>> {
        if !self.cfg.config.guard.test_only_retry || self.exec_mut(exec_id).feedback_from.is_none() {
            return Ok(None);
        }
        let from_check = self
            .run
            .pending_feedback_step
            .as_deref()
            .and_then(|id| self.wf.steps.iter().find(|s| s.id == id))
            .is_some_and(|s| s.kind() == StepKind::Command);
        let Some(head) = head_before.filter(|_| from_check) else { return Ok(None) };
        let diff = crate::git::changed_files(worktree, head)?;
        if diff.files.is_empty() {
            return Ok(None);
        }
        let Some(tests) = scope_set(&self.cfg.config.guard.test_paths) else { return Ok(None) };
        if !diff.files.iter().all(|f| tests.is_match(&f.path)) {
            return Ok(None);
        }
        let paths: Vec<String> = diff.files.iter().map(|f| f.path.clone()).collect();
        self.audit("test_only_retry", Actor::policy(), Some(&step.id), serde_json::json!({"paths": paths, "failed_step": self.run.pending_feedback_step}));
        let d = PolicyDecision {
            decision: Decision::RequireApproval,
            subject: format!("retry of `{}`: {}", step.id, paths.join(", ")),
            matched: vec![crate::policies::MatchedRule {
                rule_id: "guard-test-only-retry".into(),
                decision: Decision::RequireApproval,
                reason: Some("after a failed check the agent changed only tests or test configuration".into()),
                source: "builtin:guard".into(),
            }],
            reason: "guard-test-only-retry (after a failed check the agent changed only tests or test configuration)".into(),
        };
        self.record_policy(Some(&step.id), Some(exec_id), &d);
        let reason = format!("After `{}` failed, the agent changed only tests: {}", self.run.pending_feedback_step.clone().unwrap_or_default(), paths.join(", "));
        match self.request_approval(step, exec_id, ApprovalKind::Policy, reason, Some("accept test-only changes and re-run the checks".into()), vec![d])? {
            Approval::Granted => Ok(None),
            Approval::Denied(why) => {
                self.finish_exec(exec_id, StepStatus::Failed, Some(why.clone()))?;
                Ok(Some(StepOutcome::Failed { reason: why, feedback: String::new() }))
            }
            Approval::Cancelled => Ok(Some(StepOutcome::Cancelled)),
        }
    }

    fn parse_structured(&self, output: AgentOutput, raw: &str) -> Result<serde_json::Value> {
        match output {
            AgentOutput::Review => parse_review(raw),
            AgentOutput::Acceptance => crate::epic::parse_acceptance(raw, self.task.acceptance.len()),
            AgentOutput::Conformance => crate::epic::parse_conformance(raw),
            AgentOutput::Contract => crate::epic::parse_contract(raw, self.task.acceptance.len(), &self.worktree()?),
            AgentOutput::Plan => {
                let workflows: Vec<String> = self.catalog.workflows().map(|w| w.into_iter().map(|x| x.name).collect()).unwrap_or_default();
                let existing: Vec<String> = self.task.epic.as_ref().and_then(|l| self.ctx.store.load_epic(&l.epic_id).ok()).map(|e| e.tasks.keys().cloned().collect()).unwrap_or_default();
                let rules = crate::epic::PlanRules { max_tasks: self.cfg.config.epic.max_tasks, workflows: &workflows, existing_keys: &existing };
                let plan = crate::epic::parse_plan(raw, &rules)?;
                Ok(serde_json::to_value(plan)?)
            }
            AgentOutput::Summary => unreachable!(),
        }
    }

    /// Audit event, its data, and the gate failure (reason, feedback) for a
    /// validated structured output.
    #[allow(clippy::type_complexity)]
    fn judge_structured(&self, output: AgentOutput, v: &serde_json::Value, gate: bool) -> Result<(&'static str, serde_json::Value, Option<(String, String)>)> {
        Ok(match output {
            AgentOutput::Review => {
                let verdict = v["verdict"].as_str().unwrap_or("").to_string();
                let n = v["findings"].as_array().map(|a| a.len()).unwrap_or(0);
                let failure = (gate && verdict != "approved").then(|| (format!("review verdict `{verdict}` with {n} finding(s)"), serde_json::to_string_pretty(&v["findings"]).unwrap_or_default()));
                ("review_completed", serde_json::json!({"verdict": verdict, "findings": n, "structured": true}), failure)
            }
            AgentOutput::Acceptance => {
                let count = |st: &str| v["criteria"].as_array().map(|a| a.iter().filter(|c| c["status"] == st).count()).unwrap_or(0);
                let (met, unmet, unv) = (count("met"), count("not_met"), count("unverifiable"));
                let failure = (gate && unmet > 0).then(|| (format!("{unmet} acceptance criterion(s) not met"), crate::epic::unmet_feedback(v, &self.task.acceptance)));
                ("acceptance_verified", serde_json::json!({"met": met, "not_met": unmet, "unverifiable": unv, "verdict": v["verdict"], "criteria": v["criteria"]}), failure)
            }
            AgentOutput::Plan => {
                let n = v["tasks"].as_array().map(|a| a.len()).unwrap_or(0);
                ("plan_proposed", serde_json::json!({"tasks": n, "plan_sha256": crate::store::sha256_hex(crate::store::canonical_json(v).as_bytes())}), None)
            }
            AgentOutput::Contract => ("contract_written", serde_json::json!({"files": v["files"], "check": v["check"], "criteria_map": v["criteria_map"]}), None),
            AgentOutput::Conformance => {
                let count = |st: &str| v["points"].as_array().map(|a| a.iter().filter(|c| c["status"] == st).count()).unwrap_or(0);
                ("conformance_reviewed", serde_json::json!({"covered": count("covered"), "partial": count("partial"), "missing": count("missing"), "followups": v["followups"].as_array().map(|a| a.len()).unwrap_or(0)}), None)
            }
            AgentOutput::Summary => unreachable!(),
        })
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
            AgentEvent::Stuck(why) => {
                self.exec_mut(exec_id).attention = Some(why.clone());
                if self.exec_mut(exec_id).status == StepStatus::Running {
                    self.set_exec_status(exec_id, StepStatus::AwaitingHuman)?;
                } else {
                    self.save()?;
                }
                let pane = self.exec_mut(exec_id).agent.as_ref().and_then(|a| a.pane_id.clone());
                self.audit("agent_stuck", Actor::orchestrator(), Some(step_id), serde_json::json!({"reason": why, "pane_id": pane}));
                self.notify(&format!("{} looks stuck", self.run.display_name()), &format!("{step_id} · {runner}: {why}"), true);
            }
            AgentEvent::Progressing => {
                self.exec_mut(exec_id).attention = None;
                if self.exec_mut(exec_id).status == StepStatus::AwaitingHuman {
                    self.set_exec_status(exec_id, StepStatus::Running)?;
                } else {
                    self.save()?;
                }
                self.audit("agent_progressing", Actor::orchestrator(), Some(step_id), serde_json::json!({}));
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
        let contract_check = matches!(step.spec, StepSpec::Check { contract: true, .. });
        let (mut argv, source) = if contract_check {
            match &self.run.contract {
                Some(c) => (c.check.clone(), Some("locked contract".to_string())),
                None => {
                    let exec_id = self.new_exec(step)?;
                    self.finish_exec(&exec_id, StepStatus::Failed, Some("no locked contract in this run".into()))?;
                    return Ok(StepOutcome::Blocked("`contract: true` check but no contract was locked".into()));
                }
            }
        } else if let Some(name) = step.named_check() {
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
            acceptance: crate::approvals::acceptance_rows(&self.task.acceptance, self.latest_acceptance()),
            manual_checks: self.task.manual_checks.clone(),
        }
    }

    /// The latest validated `output: acceptance` result of this run.
    fn latest_acceptance(&self) -> Option<&serde_json::Value> {
        self.run.steps.iter().rev().filter_map(|e| e.structured.as_ref()).find(|v| v.get("criteria").is_some())
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
                // The first approval after the contract was locked approves it.
                if self.run.contract.as_ref().is_some_and(|c| c.approval_id.is_none()) {
                    let a = self.ctx.store.load_approval(&approval_id).ok();
                    if let Some(c) = self.run.contract.as_mut() {
                        c.approval_id = Some(approval_id.clone());
                        c.approved_by = a.as_ref().and_then(|a| a.decided_by.clone());
                        c.approved_at = a.and_then(|a| a.decided_at).or(Some(now()));
                    }
                    let c = self.run.contract.clone().unwrap();
                    self.audit("contract_approved", Actor::human(c.approved_by.clone()), Some(&step.id), serde_json::json!({"sha256": c.sha256, "approval_id": approval_id}));
                }
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
        for p in self.contract_violations(&worktree) {
            denies.push(format!("{p} (contract file changed after it was locked)"));
        }
        let mut asks: Vec<(String, PolicyDecision)> = vec![];
        let runner = self.exec_mut(exec_id).runner.clone();
        let needs_content = self.policy.needs_added_lines();
        let scope = scope_set(&self.task.options.scope);
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
            if action == Action::Write && needs_content {
                s.added_lines = Some(crate::git::added_lines(&worktree, &base, f, 20_000));
            }
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
                    let fingerprint = content_fingerprint(&worktree, f);
                    if self.run.approved_paths.get(&f.path) != Some(&fingerprint) {
                        asks.push((f.path.clone(), d));
                    }
                }
                Decision::Allow => {
                    // The task's declared scope: anything else is asked about.
                    if let Some(set) = &scope {
                        let inside = set.is_match(&f.path) || f.old_path.as_deref().is_some_and(|p| set.is_match(p));
                        let fingerprint = content_fingerprint(&worktree, f);
                        if !inside && self.run.approved_paths.get(&f.path) != Some(&fingerprint) {
                            let d = scope_decision(&f.path, &self.task.options.scope);
                            self.record_policy(Some(&step.id), Some(exec_id), &d);
                            asks.push((f.path.clone(), d));
                        }
                    }
                }
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
                            self.run.approved_paths.insert(f.path.clone(), content_fingerprint(&worktree, f));
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
        if let Some(w) = self.run.git.worktree_path.clone() {
            let bad = self.contract_violations(&w);
            if !bad.is_empty() && !self.run.dry_run {
                let why = format!("contract files changed after they were locked: {}", bad.join(", "));
                self.finish_exec(&exec_id, StepStatus::Failed, Some(why.clone()))?;
                return Ok(StepOutcome::Blocked(why));
            }
        }
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
                // A follow-up adds commits to an existing PR: push them
                // (policy-checked; usually needs approval).
                if self.task.options.continue_run.is_some() && !self.run.dry_run {
                    if let Some(o) = self.commit_changes(step, &exec_id, None)? {
                        return Ok(o);
                    }
                    if let Some(o) = self.push_branch(step, &exec_id)? {
                        return Ok(o);
                    }
                }
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
        if let Some(v) = self.run.steps.iter().rev().filter_map(|e| e.structured.as_ref()).find(|v| v.get("findings").is_some()) {
            if let Some(verdict) = v.get("verdict").and_then(|x| x.as_str()) {
                b.push_str(&format!("Review verdict: **{verdict}**.\n"));
            }
        }
        if let Some(l) = &self.task.epic {
            if let Ok(e) = self.ctx.store.load_epic(&l.epic_id) {
                b.push_str(&format!("\nPart of epic {} (`{}` — {}), plan task {}.\n", e.epic_id, e.adr.path, e.adr.title, l.key));
            }
        }
        if !self.task.acceptance.is_empty() {
            b.push_str("\n| # | acceptance criterion | status | evidence |\n|---|---|---|---|\n");
            for (i, row) in crate::approvals::acceptance_rows(&self.task.acceptance, self.latest_acceptance()).iter().enumerate() {
                let cell = |s: &str| s.replace('|', "\\|").replace('\n', " ");
                b.push_str(&format!("| {} | {} | {} | {} |\n", i + 1, cell(&row.criterion), row.status, cell(&row.evidence)));
            }
        }
        if !self.task.manual_checks.is_empty() {
            b.push_str("\nManual checks:\n");
            for m in &self.task.manual_checks {
                b.push_str(&format!("- [ ] {m}\n"));
            }
        }
        let u = crate::telemetry::run_agent_usage(&self.run);
        b.push_str(&format!("\nAgent usage: {}.\n", crate::telemetry::tokens_display(&u)));
        if let Some(c) = &self.run.contract {
            b.push_str(&format!(
                "\n### Contract\n\nTests written before the implementation, failing on the base, approved{} and locked. The implementation did not change them.\n\n",
                match (&c.approved_by, c.approved_at) {
                    (Some(who), Some(at)) => format!(" by {who} at {}", at.format("%Y-%m-%d %H:%M UTC")),
                    _ => String::new(),
                }
            ));
            for (p, h) in &c.files {
                b.push_str(&format!("- `{p}` `{}`\n", &h[..h.len().min(19)]));
            }
            b.push_str(&format!("\nCheck: `{}`\n", crate::policies::command::display_argv(&c.check)));
            let head = self.ctx.audit.read(Some(&self.run.run_id)).ok().and_then(|e| e.last().map(|x| x.hash.clone())).unwrap_or_default();
            b.push_str(&format!(
                "\n```\nherdr-orchestrator-receipt: v1\nrun: {}\ncontract_sha256: {}\ncontract_commit: {}\napproval: {}\naudit_head: {}\n```\n",
                self.run.run_id,
                c.sha256,
                c.commit.clone().unwrap_or_default(),
                c.approval_id.clone().unwrap_or_default(),
                head
            ));
        }
        b.push_str(&format!("\nAudit: `herdr-orchestrator audit verify {}`\n", self.run.run_id));
        redact_str(&b)
    }
}

/// What a human approved for a path: its content, not its git status, so
/// committing an approved file (untracked → added) does not re-ask, while
/// any real change to it does.
fn content_fingerprint(worktree: &std::path::Path, f: &ChangedFile) -> String {
    if f.change == "deleted" {
        return "deleted".into();
    }
    match std::fs::read(worktree.join(&f.path)) {
        Ok(b) => format!("sha256:{}", crate::store::sha256_hex(&b)),
        Err(_) => format!("{}:{}:{}", f.change, f.insertions, f.deletions),
    }
}

/// Compile path globs the same way policy does (`*` stays in a segment).
pub(super) fn scope_set(globs: &[String]) -> Option<globset::GlobSet> {
    if globs.is_empty() {
        return None;
    }
    let mut b = globset::GlobSetBuilder::new();
    for g in globs {
        if let Ok(x) = globset::GlobBuilder::new(g).literal_separator(true).build() {
            b.add(x);
        }
    }
    b.build().ok()
}

fn scope_decision(path: &str, scope: &[String]) -> PolicyDecision {
    let reason = format!("outside the task's scope ({})", scope.join(", "));
    PolicyDecision {
        decision: Decision::RequireApproval,
        subject: format!("write: {path}"),
        matched: vec![crate::policies::MatchedRule { rule_id: "task-scope".into(), decision: Decision::RequireApproval, reason: Some(reason.clone()), source: "task".into() }],
        reason: format!("task-scope ({reason})"),
    }
}

enum ContractResult {
    Locked,
    /// Back to the contract writer with (reason, feedback).
    Retry(String, String),
    Stop(StepOutcome),
}

enum GateResult {
    Ok,
    Stop(StepOutcome),
}
