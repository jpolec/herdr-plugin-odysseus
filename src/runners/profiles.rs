//! Built-in runner profiles and user overrides.
//!
//! Pane mode launches the provider's *interactive* CLI in a Herdr pane via
//! `agent.start`, with conservative permission modes so the agent asks a
//! human (Herdr shows it as `blocked`) before risky actions. Headless mode
//! runs the provider's non-interactive CLI directly (used without Herdr).

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::RunnerProfileConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerMode {
    Pane,
    Headless,
    Shell,
    Fake,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunnerProfile {
    pub name: String,
    pub mode: RunnerMode,
    /// Herdr agent kind (`claude`, `codex`, `opencode`, `gemini`…).
    pub kind: Option<String>,
    /// Args for the interactive CLI in pane mode.
    pub pane_args: Vec<String>,
    /// Headless argv. Placeholders (whole elements only): `{{prompt}}`,
    /// `{{prompt_file}}`, `{{output_file}}`. The prompt is also on stdin.
    pub headless_command: Option<Vec<String>>,
    /// Parser for headless usage output: `claude-json`, `codex-jsonl`, `none`.
    pub usage_format: String,
    /// Keys to interrupt the agent in its pane.
    pub interrupt_keys: Vec<String>,
    pub env_inherit: Vec<String>,
    pub fake_scenario: Option<String>,
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Agent kinds Herdr 0.9.0 can start with `agent.start`.
pub const HERDR_AGENT_KINDS: &[&str] = &[
    "pi", "claude", "codex", "gemini", "cursor", "devin", "agy", "cline", "omp", "mastracode", "opencode", "copilot",
    "kimi", "kiro", "droid", "amp", "grok", "hermes", "kilo", "qodercli", "qwen", "letta", "maki", "muse",
];

pub const FAKE_SCENARIOS: &[&str] = &[
    "success",
    "fail",
    "timeout",
    "review-findings",
    "review-approve",
    "review-fix",
    "review-invalid",
    "fix-on-retry",
    "touch-secret",
    "touch-migration",
    "crash",
    "noop",
    "skip-test",
    "tests-only-on-retry",
    "touch-agent-config",
    "plan",
    "plan-invalid",
    "plan-fix",
    "plan-writes",
    "acceptance-met",
    "acceptance-unmet",
    "acceptance-fix",
    "conformance",
    "contract",
    "contract-green",
    "fix-contract",
    "contract-tamper",
];

fn builtin(name: &str) -> Option<RunnerProfile> {
    let base = |kind: &str| RunnerProfile {
        name: name.to_string(),
        mode: RunnerMode::Pane,
        kind: Some(kind.to_string()),
        pane_args: vec![],
        headless_command: None,
        usage_format: "none".into(),
        interrupt_keys: s(&["esc"]),
        env_inherit: vec![],
        fake_scenario: None,
    };
    Some(match name {
        "claude" => RunnerProfile {
            // acceptEdits: file edits proceed, shell commands ask the human.
            pane_args: s(&["--permission-mode", "acceptEdits"]),
            headless_command: Some(s(&["claude", "-p", "--output-format", "json", "--permission-mode", "acceptEdits"])),
            usage_format: "claude-json".into(),
            env_inherit: s(&["ANTHROPIC_*", "CLAUDE_*"]),
            ..base("claude")
        },
        "codex" => RunnerProfile {
            // workspace-write sandbox, ask before leaving it.
            pane_args: s(&["--sandbox", "workspace-write", "--ask-for-approval", "on-request"]),
            headless_command: Some(s(&[
                "codex", "exec", "--json", "--sandbox", "workspace-write", "--skip-git-repo-check",
                "--output-last-message", "{{output_file}}", "-",
            ])),
            usage_format: "codex-jsonl".into(),
            interrupt_keys: s(&["esc"]),
            env_inherit: s(&["OPENAI_*", "CODEX_*"]),
            ..base("codex")
        },
        "opencode" => RunnerProfile {
            headless_command: Some(s(&["opencode", "run", "{{prompt}}"])),
            ..base("opencode")
        },
        "gemini" => RunnerProfile {
            headless_command: Some(s(&["gemini", "-p", "{{prompt}}"])),
            env_inherit: s(&["GEMINI_*", "GOOGLE_*"]),
            ..base("gemini")
        },
        "copilot" => base("copilot"),
        "shell" => RunnerProfile { mode: RunnerMode::Shell, kind: None, ..base("shell") },
        n if n.starts_with("fake-") => {
            let sc = &n[5..];
            if !FAKE_SCENARIOS.contains(&sc) {
                return None;
            }
            RunnerProfile { mode: RunnerMode::Fake, kind: Some("fake".into()), fake_scenario: Some(sc.to_string()), ..base("fake") }
        }
        n if HERDR_AGENT_KINDS.contains(&n) => base(n),
        _ => return None,
    })
}

pub fn builtin_names() -> Vec<String> {
    let mut v = s(&["claude", "codex", "opencode", "gemini", "copilot", "shell"]);
    v.extend(FAKE_SCENARIOS.iter().map(|x| format!("fake-{x}")));
    v
}

/// Built-in profile merged with a user override. Unknown names are allowed
/// when the override fully defines the runner (custom runners).
pub fn resolve(name: &str, o: Option<&RunnerProfileConfig>) -> Result<RunnerProfile> {
    let mut p = match (builtin(name), o) {
        (Some(p), _) => p,
        (None, Some(o)) if o.mode.is_some() => RunnerProfile {
            name: name.to_string(),
            mode: RunnerMode::Shell,
            kind: None,
            pane_args: vec![],
            headless_command: None,
            usage_format: "none".into(),
            interrupt_keys: s(&["ctrl+c"]),
            env_inherit: vec![],
            fake_scenario: None,
        },
        _ => bail!("unknown runner {name:?} (built-in: {})", builtin_names().join(", ")),
    };
    if let Some(o) = o {
        if let Some(m) = &o.mode {
            p.mode = match m.as_str() {
                "pane" => RunnerMode::Pane,
                "headless" => RunnerMode::Headless,
                "shell" => RunnerMode::Shell,
                other => bail!("runner {name}: unknown mode {other:?} (pane, headless, shell)"),
            };
        }
        if let Some(k) = &o.kind {
            p.kind = Some(k.clone());
        }
        if let Some(a) = &o.args {
            p.pane_args = a.clone();
        }
        if let Some(c) = &o.command {
            p.headless_command = Some(c.clone());
        }
        if let Some(e) = &o.env_inherit {
            p.env_inherit = e.clone();
        }
    }
    match p.mode {
        RunnerMode::Pane => {
            let k = p.kind.as_deref().unwrap_or("");
            if !HERDR_AGENT_KINDS.contains(&k) {
                bail!("runner {name}: Herdr cannot start agent kind {k:?}");
            }
        }
        RunnerMode::Headless | RunnerMode::Shell => match &p.headless_command {
            Some(c) if !c.is_empty() && !c[0].contains("{{") => {}
            _ => bail!("runner {name}: mode {:?} needs `command` (argv array)", p.mode),
        },
        RunnerMode::Fake => {}
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_resolve() {
        for n in ["claude", "codex", "opencode", "fake-success", "fake-review-findings"] {
            resolve(n, None).unwrap_or_else(|e| panic!("{n}: {e}"));
        }
        assert!(resolve("nope", None).is_err());
        assert!(resolve("fake-nope", None).is_err());
        assert!(resolve("shell", None).is_err(), "shell needs a command");
    }

    #[test]
    fn overrides_apply() {
        let o = RunnerProfileConfig { mode: Some("headless".into()), ..Default::default() };
        let p = resolve("claude", Some(&o)).unwrap();
        assert_eq!(p.mode, RunnerMode::Headless);
        let custom = RunnerProfileConfig { mode: Some("shell".into()), command: Some(vec!["aider".into(), "--yes".into()]), ..Default::default() };
        let p = resolve("aider", Some(&custom)).unwrap();
        assert_eq!(p.mode, RunnerMode::Shell);
        let bad = RunnerProfileConfig { mode: Some("pane".into()), kind: Some("vim".into()), ..Default::default() };
        assert!(resolve("x", Some(&bad)).is_err());
    }
}
