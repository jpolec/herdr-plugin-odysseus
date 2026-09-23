//! Workflow DSL: typed model, parsing and validation.

pub mod catalog;
pub mod template;

use std::collections::{BTreeMap, HashSet};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::HumanDuration;
use crate::model::StepKind;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub defaults: WorkflowDefaults,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefaults {
    pub runner: Option<String>,
    pub timeout: Option<HumanDuration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OnFailure {
    /// Step to jump back to (must not be after the failing step).
    pub retry_step: String,
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
    /// Send the failure details to the retried step as `{{feedback}}`.
    #[serde(default = "yes")]
    pub feedback: bool,
}

fn default_attempts() -> u32 {
    1
}
fn yes() -> bool {
    true
}

/// Expected structured output of an agent step.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentOutput {
    /// Free-form summary.
    #[default]
    Summary,
    /// Review verdict + findings (validated JSON).
    Review,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GitAction {
    #[default]
    Commit,
    Push,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepSpec {
    Agent {
        #[serde(default)]
        runner: Option<String>,
        #[serde(default)]
        skill: Option<String>,
        prompt: String,
        #[serde(default)]
        output: AgentOutput,
        /// For `output: review`: a non-approving verdict fails the step
        /// (so `on_failure` can route findings back to the implementer).
        #[serde(default)]
        gate: bool,
        /// Commit worktree changes after the step (default: git.auto_commit).
        #[serde(default)]
        commit: Option<bool>,
        /// Check the diff against policy after the step (default true).
        #[serde(default = "yes")]
        policy_check: bool,
    },
    Command {
        command: Vec<String>,
        #[serde(default)]
        shell: bool,
        #[serde(default)]
        env: BTreeMap<String, String>,
        /// Sub-directory of the worktree to run in.
        #[serde(default)]
        cwd: Option<String>,
    },
    /// A verification command: either explicit `command`, or a named check
    /// (`tests`, `lint`, `security`) resolved from project config or
    /// auto-detection at run time.
    Check {
        #[serde(default)]
        check: Option<String>,
        #[serde(default)]
        command: Vec<String>,
        #[serde(default)]
        shell: bool,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    Approval {
        reason: String,
    },
    /// Explicit diff-based policy gate over everything changed so far.
    Policy {},
    Git {
        #[serde(default)]
        action: GitAction,
        #[serde(default)]
        message: Option<String>,
    },
    GithubPr {
        #[serde(default)]
        draft: Option<bool>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        base: Option<String>,
    },
    /// Reserved; rejected by validation in this version.
    Parallel {
        #[serde(default)]
        steps: Vec<serde_yaml_ng::Value>,
    },
}

/// (argv, shell, env, cwd) of a command/check step.
pub type CommandParts<'a> = (&'a Vec<String>, bool, &'a BTreeMap<String, String>, Option<&'a String>);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub id: String,
    #[serde(flatten)]
    pub spec: StepSpec,
    #[serde(default)]
    pub timeout: Option<HumanDuration>,
    #[serde(default)]
    pub on_failure: Option<OnFailure>,
    /// Record the failure but keep going.
    #[serde(default)]
    pub continue_on_failure: bool,
    #[serde(default)]
    pub description: Option<String>,
}

impl Step {
    pub fn kind(&self) -> StepKind {
        match &self.spec {
            StepSpec::Agent { .. } => StepKind::Agent,
            StepSpec::Command { .. } | StepSpec::Check { .. } => StepKind::Command,
            StepSpec::Approval { .. } => StepKind::Approval,
            StepSpec::Policy {} => StepKind::Policy,
            StepSpec::Git { .. } => StepKind::Git,
            StepSpec::GithubPr { .. } => StepKind::GithubPr,
            StepSpec::Parallel { .. } => StepKind::Command,
        }
    }

    pub fn runner<'a>(&'a self, wf: &'a Workflow) -> Option<&'a str> {
        match &self.spec {
            StepSpec::Agent { runner, .. } => runner.as_deref().or(wf.defaults.runner.as_deref()),
            _ => None,
        }
    }

    /// Command argv, shell flag, env and cwd for command/check steps.
    pub fn command(&self) -> Option<CommandParts<'_>> {
        match &self.spec {
            StepSpec::Command { command, shell, env, cwd } | StepSpec::Check { command, shell, env, cwd, .. } => {
                Some((command, *shell, env, cwd.as_ref()))
            }
            _ => None,
        }
    }

    /// Named check (`tests`, `lint`, `security`) for check steps.
    pub fn named_check(&self) -> Option<&str> {
        match &self.spec {
            StepSpec::Check { check, .. } => check.as_deref(),
            _ => None,
        }
    }
}

impl Workflow {
    pub fn parse(yaml: &str) -> Result<Self> {
        let wf: Workflow = serde_yaml_ng::from_str(yaml).context("parsing workflow YAML")?;
        wf.validate()?;
        Ok(wf)
    }

    pub fn sha256(yaml: &str) -> String {
        crate::store::sha256_hex(yaml.as_bytes())
    }

    pub fn step_index(&self, id: &str) -> Option<usize> {
        self.steps.iter().position(|s| s.id == id)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported workflow version {} (expected 1)", self.version);
        }
        if self.name.is_empty() || !self.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("workflow name {:?} must be ASCII letters, digits, '-' or '_'", self.name);
        }
        if self.steps.is_empty() {
            bail!("workflow {} has no steps", self.name);
        }
        let ids: Vec<String> = self.steps.iter().map(|s| s.id.clone()).collect();
        let mut seen = HashSet::new();
        for (i, s) in self.steps.iter().enumerate() {
            let ctx = || format!("workflow {} step {:?}", self.name, s.id);
            if s.id.is_empty()
                || s.id.len() > 40
                || !s.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("{}: step ids must be 1-40 ASCII letters, digits, '-' or '_'", ctx());
            }
            if !seen.insert(s.id.clone()) {
                bail!("{}: duplicate step id", ctx());
            }
            let earlier: Vec<String> = ids[..i].to_vec();
            match &s.spec {
                StepSpec::Agent { prompt, gate, output, .. } => {
                    template::check(prompt, &earlier).with_context(ctx)?;
                    if *gate && *output != AgentOutput::Review {
                        bail!("{}: `gate: true` requires `output: review`", ctx());
                    }
                }
                StepSpec::Check { check: Some(name), command, .. } => {
                    if !command.is_empty() {
                        bail!("{}: use either `check:` or `command:`, not both", ctx());
                    }
                    if !crate::checks::NAMED_CHECKS.contains(&name.as_str()) {
                        bail!("{}: unknown named check {name:?} (known: {:?})", ctx(), crate::checks::NAMED_CHECKS);
                    }
                }
                StepSpec::Command { command, shell, env, cwd } | StepSpec::Check { command, shell, env, cwd, .. } => {
                    if command.is_empty() || command[0].trim().is_empty() {
                        bail!("{}: command must be a non-empty argv array", ctx());
                    }
                    if *shell && command.len() != 1 {
                        bail!("{}: with `shell: true`, command must be a single script string", ctx());
                    }
                    for a in command.iter() {
                        let vars = template::check(a, &earlier).with_context(ctx)?;
                        if *shell && !vars.is_empty() {
                            bail!(
                                "{}: templates are not allowed in shell scripts; use the HERDR_ORCH_* environment variables instead",
                                ctx()
                            );
                        }
                        if let Some(v) = vars.iter().find(|v| template::is_untrusted(v)) {
                            bail!(
                                "{}: {{{{{v}}}}} carries untrusted text and cannot be used in a command line",
                                ctx()
                            );
                        }
                    }
                    if !template::variables(&command[0])?.is_empty() {
                        bail!("{}: templates are not allowed in the program name (argv[0])", ctx());
                    }
                    for (k, v) in env {
                        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || k.is_empty() {
                            bail!("{}: invalid env var name {k:?}", ctx());
                        }
                        let vars = template::check(v, &earlier).with_context(ctx)?;
                        if let Some(v) = vars.iter().find(|v| template::is_untrusted(v)) {
                            bail!("{}: {{{{{v}}}}} cannot be used in command env", ctx());
                        }
                    }
                    if let Some(c) = cwd {
                        let p = std::path::Path::new(c);
                        if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                            bail!("{}: cwd must be a relative path inside the worktree", ctx());
                        }
                    }
                }
                StepSpec::Approval { reason } => {
                    template::check(reason, &earlier).with_context(ctx)?;
                }
                StepSpec::Git { message, .. } => {
                    if let Some(m) = message {
                        template::check(m, &earlier).with_context(ctx)?;
                    }
                }
                StepSpec::GithubPr { title, body, .. } => {
                    for t in [title, body].into_iter().flatten() {
                        template::check(t, &earlier).with_context(ctx)?;
                    }
                }
                StepSpec::Policy {} => {}
                StepSpec::Parallel { .. } => bail!(
                    "{}: `parallel` steps are not supported in this version; use task variants (--variants N) for parallel implementations",
                    ctx()
                ),
            }
            if let Some(of) = &s.on_failure {
                let Some(target) = ids[..=i].iter().position(|x| *x == of.retry_step) else {
                    bail!(
                        "{}: on_failure.retry_step {:?} must name this step or an earlier one",
                        ctx(),
                        of.retry_step
                    );
                };
                if of.max_attempts == 0 || of.max_attempts > 10 {
                    bail!("{}: on_failure.max_attempts must be between 1 and 10", ctx());
                }
                let t = &self.steps[target];
                if matches!(t.spec, StepSpec::Approval { .. } | StepSpec::GithubPr { .. }) {
                    bail!("{}: cannot retry into an approval or github_pr step", ctx());
                }
                if s.continue_on_failure {
                    bail!("{}: on_failure and continue_on_failure are mutually exclusive", ctx());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WF: &str = r#"
version: 1
name: implement-review
defaults:
  runner: codex
  timeout: 45m
steps:
  - id: implement
    type: agent
    skill: implementation
    prompt: |
      {{task}}
      {{feedback}}
  - id: tests
    type: command
    command: ["cargo", "test", "--all"]
    timeout: 15m
    on_failure:
      retry_step: implement
      max_attempts: 2
      feedback: true
  - id: review
    type: agent
    runner: claude
    output: review
    prompt: Review {{step.implement.output}}
  - id: approval
    type: approval
    reason: "Code is ready for human review."
  - id: pr
    type: github_pr
    draft: true
"#;

    #[test]
    fn parses_example() {
        let wf = Workflow::parse(WF).unwrap();
        assert_eq!(wf.steps.len(), 5);
        assert_eq!(wf.steps[0].runner(&wf), Some("codex"));
        assert_eq!(wf.steps[2].runner(&wf), Some("claude"));
        assert_eq!(wf.steps[1].timeout.unwrap().as_secs(), 900);
        assert_eq!(wf.steps[1].on_failure.as_ref().unwrap().max_attempts, 2);
        assert_eq!(wf.steps[4].kind(), StepKind::GithubPr);
    }

    fn bad(yaml_steps: &str) -> String {
        let y = format!("version: 1\nname: t\nsteps:\n{yaml_steps}");
        format!("{:#}", Workflow::parse(&y).unwrap_err())
    }

    #[test]
    fn validation_errors() {
        assert!(bad("  - id: a\n    type: command\n    command: []\n").contains("non-empty"));
        assert!(bad("  - id: a\n    type: command\n    command: [\"echo\", \"{{task}}\"]\n").contains("untrusted"));
        assert!(bad("  - id: a\n    type: command\n    command: [\"{{branch}}\"]\n").contains("argv[0]"));
        assert!(bad("  - id: a\n    type: command\n    shell: true\n    command: [\"echo {{branch}}\"]\n").contains("shell"));
        assert!(bad("  - id: a\n    type: approval\n    reason: x\n  - id: a\n    type: approval\n    reason: y\n").contains("duplicate"));
        assert!(bad("  - id: a\n    type: command\n    command: [x]\n    on_failure: {retry_step: b}\n  - id: b\n    type: approval\n    reason: r\n").contains("earlier"));
        assert!(bad("  - id: a\n    type: parallel\n").contains("variants"));
        assert!(bad("  - id: a\n    type: agent\n    prompt: '{{ task | x }}'\n").contains("invalid template"));
        assert!(bad("  - id: a\n    type: agent\n    prompt: x\n    bogus: 1\n").contains("unknown field"));
        assert!(bad("  - id: a\n    type: frob\n").contains("frob"));
        assert!(bad("  - id: a\n    type: command\n    command: [ls]\n    cwd: ../x\n").contains("cwd"));
        assert!(bad("  - id: a\n    type: agent\n    prompt: x\n    gate: true\n").contains("review"));
        assert!(bad("  - id: a\n    type: agent\n    prompt: '{{step.b.output}}'\n  - id: b\n    type: approval\n    reason: r\n").contains("unknown template"));
    }

    #[test]
    fn trusted_templates_allowed_in_commands() {
        let y = "version: 1\nname: t\nsteps:\n  - id: a\n    type: command\n    command: [\"git\", \"diff\", \"{{base_sha}}\", \"--stat\"]\n    env: {W: \"{{worktree.path}}\"}\n";
        assert!(Workflow::parse(y).is_ok());
    }
}
