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
    /// Model, effort and advisor as configured (already folded into the
    /// launch args; kept for display).
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub advisor: Option<String>,
    /// Environment set for the agent (pane and child process).
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
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
    "resolve-conflicts",
    "leave-note",
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
        model: None,
        effort: None,
        advisor: None,
        env: Default::default(),
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
        // Presets: the strongest setup for decisions (plans, contracts)
        // and a fast one for small steps. Override any field in config.
        "claude-deep" => RunnerProfile {
            pane_args: s(&["--permission-mode", "acceptEdits"]),
            headless_command: Some(s(&["claude", "-p", "--output-format", "json", "--permission-mode", "acceptEdits"])),
            usage_format: "claude-json".into(),
            env_inherit: s(&["ANTHROPIC_*", "CLAUDE_*"]),
            model: Some("opus".into()),
            effort: Some("xhigh".into()),
            advisor: Some("opus".into()),
            ..base("claude")
        },
        "claude-fast" => RunnerProfile {
            pane_args: s(&["--permission-mode", "acceptEdits"]),
            headless_command: Some(s(&["claude", "-p", "--output-format", "json", "--permission-mode", "acceptEdits"])),
            usage_format: "claude-json".into(),
            env_inherit: s(&["ANTHROPIC_*", "CLAUDE_*"]),
            model: Some("sonnet".into()),
            effort: Some("medium".into()),
            advisor: Some("off".into()),
            ..base("claude")
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
    let mut v = s(&["claude", "claude-deep", "claude-fast", "codex", "opencode", "gemini", "copilot", "shell"]);
    v.extend(FAKE_SCENARIOS.iter().map(|x| format!("fake-{x}")));
    v
}

/// Built-in profile merged with a user override. Unknown names are allowed
/// when the override fully defines the runner (custom runners).
pub fn resolve(name: &str, o: Option<&RunnerProfileConfig>) -> Result<RunnerProfile> {
    let mut p = match (builtin(name), o) {
        (Some(p), _) => p,
        // A named runner that is just a preset of a known agent kind
        // (`kind: claude, model: sonnet`) is a pane runner of that kind.
        (None, Some(o)) if o.mode.is_none() && o.kind.as_deref().is_some_and(|k| HERDR_AGENT_KINDS.contains(&k)) => match builtin(o.kind.as_deref().unwrap()) {
            Some(mut b) => {
                b.name = name.to_string();
                b
            }
            None => bail!("unknown runner {name:?}"),
        },
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
            model: None,
            effort: None,
            advisor: None,
            env: Default::default(),
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
        if o.model.is_some() {
            p.model = o.model.clone();
        }
        if o.effort.is_some() {
            p.effort = o.effort.clone();
        }
        if o.advisor.is_some() {
            p.advisor = o.advisor.clone();
        }
    }
    apply_model_settings(&mut p).map_err(|e| anyhow::anyhow!("runner {name}: {e}"))?;
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

fn safe_value(v: &str) -> bool {
    !v.is_empty() && v.len() <= 80 && !v.starts_with('-') && v.chars().all(|c| c.is_ascii_alphanumeric() || "._:/-[]".contains(c))
}

/// Translate `model` / `effort` / `advisor` into the agent's own flags, for
/// the interactive (pane) launch and the headless command.
fn apply_model_settings(p: &mut RunnerProfile) -> Result<()> {
    let kind = p.kind.clone().unwrap_or_default();
    for (what, v) in [("model", &p.model), ("effort", &p.effort), ("advisor", &p.advisor)] {
        if let Some(v) = v {
            if !safe_value(v) {
                bail!("invalid {what} {v:?}");
            }
        }
    }
    if matches!(p.mode, RunnerMode::Fake | RunnerMode::Shell) {
        return Ok(());
    }
    let mut flags: Vec<String> = vec![];
    if let Some(m) = &p.model {
        match kind.as_str() {
            "claude" | "opencode" => flags.extend(["--model".to_string(), m.clone()]),
            "codex" | "gemini" => flags.extend(["-m".to_string(), m.clone()]),
            k => bail!("`model` is not supported for agent kind {k:?}; use `args`"),
        }
    }
    if let Some(e) = &p.effort {
        match kind.as_str() {
            "claude" => {
                if !matches!(e.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
                    bail!("effort {e:?}: use low, medium, high, xhigh or max");
                }
                flags.extend(["--effort".to_string(), e.clone()]);
            }
            "codex" => {
                if !matches!(e.as_str(), "minimal" | "low" | "medium" | "high" | "xhigh") {
                    bail!("effort {e:?}: use minimal, low, medium, high or xhigh");
                }
                flags.extend(["-c".to_string(), format!("model_reasoning_effort={e}")]);
            }
            k => bail!("`effort` is not supported for agent kind {k:?}"),
        }
    }
    if let Some(a) = &p.advisor {
        if kind != "claude" {
            bail!("`advisor` is only supported for Claude");
        }
        if a == "off" {
            p.env.insert("CLAUDE_CODE_DISABLE_ADVISOR_TOOL".into(), "1".into());
        } else {
            flags.extend(["--advisor".to_string(), a.clone()]);
        }
    }
    if flags.is_empty() {
        return Ok(());
    }
    p.pane_args.extend(flags.iter().cloned());
    if let Some(h) = p.headless_command.as_mut() {
        // After the subcommand (`codex exec`, `claude -p`), before the prompt.
        let at = if kind == "codex" { 2.min(h.len()) } else { h.iter().position(|a| a.starts_with("{{") || a == "-").unwrap_or(h.len()) };
        h.splice(at..at, flags);
    }
    Ok(())
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
    fn model_effort_and_advisor_become_agent_flags() {
        let p = resolve("claude-fast", None).unwrap();
        assert_eq!(p.kind.as_deref(), Some("claude"));
        assert_eq!(p.pane_args, vec!["--permission-mode", "acceptEdits", "--model", "sonnet", "--effort", "medium"]);
        assert_eq!(p.env.get("CLAUDE_CODE_DISABLE_ADVISOR_TOOL").map(String::as_str), Some("1"));
        let d = resolve("claude-deep", None).unwrap();
        assert!(d.pane_args.ends_with(&["--advisor".to_string(), "opus".to_string()]));
        // Headless: flags go before the prompt placeholder / stdin marker.
        let o = RunnerProfileConfig { model: Some("gpt-x".into()), effort: Some("high".into()), ..Default::default() };
        let c = resolve("codex", Some(&o)).unwrap();
        assert_eq!(&c.headless_command.as_ref().unwrap()[..6], &["codex", "exec", "-m", "gpt-x", "-c", "model_reasoning_effort=high"]);
        assert!(c.pane_args.ends_with(&["-c".to_string(), "model_reasoning_effort=high".to_string()]));
        // A named preset of a known kind needs no `mode`.
        let custom = RunnerProfileConfig { kind: Some("claude".into()), model: Some("claude-opus-5-5".into()), advisor: Some("fable".into()), ..Default::default() };
        let x = resolve("my-claude", Some(&custom)).unwrap();
        assert_eq!(x.mode, RunnerMode::Pane);
        assert!(x.pane_args.contains(&"claude-opus-5-5".to_string()) && x.pane_args.contains(&"fable".to_string()));
        // Invalid values are refused.
        for bad in [
            RunnerProfileConfig { effort: Some("insane".into()), ..Default::default() },
            RunnerProfileConfig { model: Some("--dangerously".into()), ..Default::default() },
            RunnerProfileConfig { model: Some("a b".into()), ..Default::default() },
        ] {
            assert!(resolve("claude", Some(&bad)).is_err());
        }
        assert!(resolve("codex", Some(&RunnerProfileConfig { advisor: Some("opus".into()), ..Default::default() })).is_err());
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
