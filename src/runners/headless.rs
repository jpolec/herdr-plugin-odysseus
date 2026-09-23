//! Non-interactive agents run as direct child processes (no Herdr pane):
//! `claude -p --output-format json`, `codex exec --json`, custom argv.
//! Used for CI/headless operation and whenever Herdr is unavailable.
//! Provider-reported usage is parsed when the output format exposes it.

use std::time::Instant;

use anyhow::{bail, Result};

use super::*;
use crate::model::UsageSource;
use crate::process::Spec;

pub struct HeadlessRunner {
    name: String,
}

impl HeadlessRunner {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_string() }
    }
}

/// Substitute whole-element placeholders. Never splits or joins elements,
/// so prompt text cannot become extra arguments.
pub fn build_argv(template: &[String], prompt: &str, prompt_file: &str, output_file: &str) -> Vec<String> {
    template
        .iter()
        .map(|a| match a.as_str() {
            "{{prompt}}" => {
                // Keep a prompt from being parsed as an option.
                if prompt.starts_with('-') {
                    format!(" {prompt}")
                } else {
                    prompt.to_string()
                }
            }
            "{{prompt_file}}" => prompt_file.to_string(),
            "{{output_file}}" => output_file.to_string(),
            other => other.to_string(),
        })
        .collect()
}

pub fn parse_usage(format: &str, stdout: &str, runtime_ms: u64) -> (UsageRecord, Option<String>) {
    let mut u = UsageRecord::unknown(Some(runtime_ms));
    let mut result_text = None;
    match format {
        "claude-json" => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
                result_text = v.get("result").and_then(|r| r.as_str()).map(String::from);
                let us = v.get("usage");
                let g = |k: &str| us.and_then(|u| u.get(k)).and_then(|x| x.as_u64());
                u.input_tokens = g("input_tokens").map(|i| i + g("cache_creation_input_tokens").unwrap_or(0));
                u.output_tokens = g("output_tokens");
                u.cached_tokens = g("cache_read_input_tokens");
                u.cost_usd = v.get("total_cost_usd").and_then(|c| c.as_f64());
                u.model = v.get("model").and_then(|m| m.as_str()).map(String::from);
                if u.cost_usd.is_some() || u.input_tokens.is_some() {
                    u.source = UsageSource::Reported;
                }
            }
        }
        "codex-jsonl" => {
            let (mut i, mut o, mut c, mut any) = (0u64, 0u64, 0u64, false);
            for line in stdout.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                if v.get("type").and_then(|t| t.as_str()) == Some("turn.completed") {
                    if let Some(us) = v.get("usage") {
                        any = true;
                        i += us.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                        o += us.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                        c += us.get("cached_input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    }
                }
                if v.get("type").and_then(|t| t.as_str()) == Some("item.completed") {
                    if let Some(t) = v.pointer("/item/text").and_then(|t| t.as_str()) {
                        result_text = Some(t.to_string());
                    }
                }
            }
            if any {
                u.input_tokens = Some(i);
                u.output_tokens = Some(o);
                u.cached_tokens = Some(c);
                // Tokens are reported; cost is not (would be an estimate).
                u.source = UsageSource::Reported;
            }
        }
        _ => {}
    }
    (u, result_text)
}

impl AgentRunner for HeadlessRunner {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> RunnerCapabilities {
        RunnerCapabilities { interactive: false, reports_usage: true, needs_herdr: false, resumable_session: false }
    }
    fn start(&self, req: &AgentRequest, _cancel: &CancelToken, events: &mut dyn FnMut(AgentEvent)) -> Result<AgentBinding> {
        let b = AgentBinding {
            mode: "headless".into(),
            agent_kind: req.profile.kind.clone(),
            output_file: Some(req.output_file.clone()),
            ..Default::default()
        };
        events(AgentEvent::Started(b.clone()));
        Ok(b)
    }
    fn send_and_wait(
        &self,
        req: &AgentRequest,
        b: &mut AgentBinding,
        deadline: Instant,
        cancel: &CancelToken,
        events: &mut dyn FnMut(AgentEvent),
    ) -> Result<AgentOutcome> {
        let Some(tpl) = &req.profile.headless_command else {
            bail!("runner {} has no headless command", self.name);
        };
        let prompt_file = req.output_file.with_extension("prompt.md");
        if let Some(d) = prompt_file.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&prompt_file, &req.prompt)?;
        let argv = build_argv(tpl, &req.prompt, &prompt_file.to_string_lossy(), &req.output_file.to_string_lossy());
        let mut env = req.env.clone();
        env.insert("HERDR_ORCH_PROMPT_FILE".into(), prompt_file.display().to_string());
        env.insert("HERDR_ORCH_OUTPUT_FILE".into(), req.output_file.display().to_string());
        let spec = Spec::new(argv, &req.worktree)
            .env(env)
            .stdin(req.prompt.clone().into_bytes())
            .timeout(deadline.saturating_duration_since(Instant::now()))
            .log(req.log_path.clone());
        events(AgentEvent::PromptSending);
        let started = crate::process::spawn(&spec)?;
        b.prompt_sent = true;
        events(AgentEvent::PromptSent);
        let out = crate::process::wait(&spec, started, Some(cancel))?;
        let stdout = out.stdout_str();
        let (usage, result_text) = parse_usage(&req.profile.usage_format, &stdout, out.duration.as_millis() as u64);
        let end = if out.cancelled {
            AgentEnd::Cancelled
        } else if out.timed_out {
            AgentEnd::TimedOut
        } else if out.success() {
            AgentEnd::Completed
        } else {
            AgentEnd::Failed(format!("exit status {:?}: {}", out.exit_code, crate::checks::excerpt(&out.stderr_str(), 5, 20, 2000)))
        };
        let mut transcript = result_text.unwrap_or(stdout);
        if transcript.trim().is_empty() {
            transcript = out.stderr_str();
        }
        Ok(AgentOutcome {
            end,
            binding: b.clone(),
            usage,
            output_file_text: read_output_file(&req.output_file),
            transcript,
            exit_code: out.exit_code,
        })
    }
    fn interrupt(&self, _b: &AgentBinding) -> Result<()> {
        Ok(()) // cancellation goes through the CancelToken
    }
    fn recover(&self, _b: &AgentBinding) -> RecoveryAssessment {
        // Child processes die with the daemon; never re-run automatically.
        RecoveryAssessment::Gone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_whole_elements() {
        let t: Vec<String> = ["x", "{{prompt}}", "--o", "{{output_file}}", "a{{prompt}}"].iter().map(|s| s.to_string()).collect();
        let a = build_argv(&t, "--evil do", "/p", "/o");
        assert_eq!(a, vec!["x", " --evil do", "--o", "/o", "a{{prompt}}"]);
    }

    #[test]
    fn parses_claude_and_codex_usage() {
        let (u, r) = parse_usage(
            "claude-json",
            r#"{"type":"result","result":"done","total_cost_usd":0.25,"usage":{"input_tokens":10,"cache_creation_input_tokens":5,"cache_read_input_tokens":100,"output_tokens":20}}"#,
            1000,
        );
        assert_eq!(u.source, UsageSource::Reported);
        assert_eq!(u.cost_usd, Some(0.25));
        assert_eq!(u.input_tokens, Some(15));
        assert_eq!(u.cached_tokens, Some(100));
        assert_eq!(r.as_deref(), Some("done"));
        let (u, _) = parse_usage(
            "codex-jsonl",
            "{\"type\":\"thread.started\"}\n{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":7,\"cached_input_tokens\":3,\"output_tokens\":2}}\n",
            5,
        );
        assert_eq!(u.input_tokens, Some(7));
        assert_eq!(u.cost_usd, None);
        let (u, _) = parse_usage("none", "whatever", 5);
        assert_eq!(u.source, UsageSource::Unknown);
    }
}
