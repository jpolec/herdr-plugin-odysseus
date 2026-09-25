//! Dry-run planning: walk a workflow and report what *would* happen,
//! including policy decisions, without creating worktrees, starting agents
//! or running commands.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use super::{runner_factory, EngineCtx};
use crate::policies::{Action, Decision, Subject};
use crate::workflow::{GitAction, StepSpec};

#[derive(Debug, Clone, Serialize)]
pub struct PlanLine {
    pub step: Option<String>,
    pub would: String,
    pub policy: Option<String>,
    pub note: Option<String>,
}

pub fn plan(ctx: &EngineCtx, repo: &Path, task_title: &str, workflow: Option<&str>, runner: Option<&str>, base: Option<&str>, variants: u32) -> Result<Vec<PlanLine>> {
    let repo = crate::git::main_repo_root(repo)?;
    let cfg = ctx.load_config(Some(&repo))?;
    let policy = ctx.policy_for(&cfg)?;
    let factory = runner_factory(ctx, &cfg);
    let wf_name = workflow.map(String::from).unwrap_or_else(|| cfg.config.defaults.workflow.clone());
    let (wf, src) = ctx.catalog(Some(&repo)).workflow(&wf_name)?;
    let base_ref = crate::git::default_base(&repo, base.or(cfg.config.defaults.base_branch.as_deref()))?;
    let base_sha = crate::git::rev_parse(&repo, &base_ref)?;
    let (path, branch) = crate::git::plan_names(&repo, &cfg.config.git.worktree_root, &cfg.config.git.branch_prefix, "<n>", task_title, None)?;
    let mut out = vec![PlanLine {
        step: None,
        would: format!(
            "create {} worktree(s) like {} on branch {} from {} ({})",
            variants.max(1),
            path.display(),
            branch,
            base_ref,
            &base_sha[..12.min(base_sha.len())]
        ),
        policy: Some(policy.evaluate(&Subject { action: Some(Action::WorktreeCreate), branch: Some(branch.clone()), ..Default::default() }).decision.upper().into()),
        note: Some(format!("workflow {} ({}, sha256 {})", wf.name, src.origin, &crate::workflow::Workflow::sha256(&src.yaml)[..12])),
    }];
    let mut prev_approval = false;
    for step in &wf.steps {
        let mut line = PlanLine { step: Some(step.id.clone()), would: String::new(), policy: None, note: None };
        match &step.spec {
            StepSpec::Agent { output, gate, .. } => {
                let name = runner.map(String::from).or_else(|| step.runner(&wf).map(String::from)).unwrap_or_else(|| cfg.config.defaults.runner.clone());
                match factory.build(&name) {
                    Ok((_, p)) => {
                        let mut launch = vec![p.kind.clone().unwrap_or_else(|| name.clone())];
                        launch.extend(p.pane_args.clone());
                        let mut s = Subject::command(&launch, false);
                        s.action = Some(Action::AgentStart);
                        s.runner = Some(name.clone());
                        line.policy = Some(policy.evaluate(&s).decision.upper().into());
                        line.would = match p.mode {
                            crate::runners::RunnerMode::Pane => format!("start {name} in a new Herdr tab \"#<n> {} · {name}\" (args: {})", step.id, p.pane_args.join(" ")),
                            m => format!("run {name} ({m:?}) as a child process"),
                        };
                        let mut notes = vec![];
                        if *output == crate::workflow::AgentOutput::Contract {
                            notes.push("writes the contract; its check must FAIL now (red proof), then it is locked".into());
                        }
                        if *output == crate::workflow::AgentOutput::Review {
                            notes.push(if *gate { "structured review, gated" } else { "structured review" }.to_string());
                        }
                        notes.push("diff checked against policy afterwards".into());
                        if cfg.config.git.auto_commit {
                            notes.push("changes committed".into());
                        }
                        line.note = Some(notes.join("; "));
                    }
                    Err(e) => line.would = format!("FAIL: {e:#}"),
                }
            }
            StepSpec::Check { contract: true, .. } => {
                line.would = "run the locked contract's own check (it must pass now)".into();
                line.note = Some("contract files are locked: any change to them is denied".into());
            }
            StepSpec::Command { .. } | StepSpec::Check { .. } => {
                let (argv, shell, source) = match step.named_check() {
                    Some(n) => match crate::checks::resolve(n, &repo, &cfg.config.checks) {
                        Some(r) => (r.argv, false, r.source),
                        None => (vec![], false, format!("no `{n}` check configured or detected → skipped")),
                    },
                    None => {
                        let (a, sh, _, _) = step.command().unwrap();
                        (a.clone(), sh, "workflow".into())
                    }
                };
                if argv.is_empty() {
                    line.would = "skip".into();
                    line.note = Some(source);
                } else {
                    let d = policy.evaluate(&Subject::command(&argv, shell));
                    line.would = format!("execute `{}`{}", crate::policies::command::display_argv(&argv), if shell { " via /bin/sh" } else { "" });
                    line.policy = Some(format!("{} — {}", d.decision.upper(), d.reason));
                    let mut notes = vec![source];
                    if let Some(of) = &step.on_failure {
                        notes.push(format!("on failure retry `{}` up to {}×{}", of.retry_step, of.max_attempts.min(cfg.config.limits.max_retries), if of.feedback { " with feedback" } else { "" }));
                    }
                    line.note = Some(notes.join("; "));
                }
            }
            StepSpec::Approval { reason } => {
                line.would = format!("request human approval: {reason}");
            }
            StepSpec::Policy {} => line.would = "evaluate the whole diff against policy".into(),
            StepSpec::Git { action, .. } => {
                line.would = match action {
                    GitAction::Commit => "commit worktree changes".into(),
                    GitAction::Push => format!("push branch to {}", cfg.config.git.remote),
                };
                if *action == GitAction::Push {
                    line.policy = Some(policy.evaluate(&Subject { action: Some(Action::GitPush), ..Default::default() }).decision.upper().into());
                }
            }
            StepSpec::GithubPr { draft, .. } => {
                let d = policy.evaluate(&Subject { action: Some(Action::GithubPr), ..Default::default() });
                line.would = format!("push the branch and open a {}PR into {}", if draft.unwrap_or(cfg.config.github.draft_pr) { "draft " } else { "" }, cfg.config.github.base.clone().unwrap_or(base_ref.clone()));
                line.policy = Some(d.decision.upper().into());
                if d.decision == Decision::RequireApproval && prev_approval {
                    line.note = Some("covered by the preceding approval step".into());
                }
            }
            StepSpec::Parallel { .. } => line.would = "unsupported".into(),
        }
        prev_approval = matches!(step.spec, StepSpec::Approval { .. });
        out.push(line);
    }
    Ok(out)
}
