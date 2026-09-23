//! Deterministic fake agents for tests, CI and demos (`fake-<scenario>`).
//! They edit the worktree and write the output file exactly as a real
//! agent is instructed to, without any account or network access.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use super::*;
use crate::model::UsageSource;

pub struct FakeRunner {
    name: String,
    scenario: String,
}

impl FakeRunner {
    pub fn new(name: &str, scenario: &str) -> Result<Self> {
        if !profiles::FAKE_SCENARIOS.contains(&scenario) {
            bail!("unknown fake scenario {scenario:?}");
        }
        Ok(Self { name: name.to_string(), scenario: scenario.to_string() })
    }
}

/// What the fake did, for [`perform`] callers.
#[derive(Debug, Clone, PartialEq)]
pub enum FakeResult {
    Done,
    Fail(String),
    Hang,
    Crash,
}

fn write(p: &Path, s: &str) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, s)?;
    Ok(())
}

/// Perform a scenario in `cwd`. `attempt` is 1-based. Shared by
/// [`FakeRunner`] and the mock Herdr agent behavior in tests.
pub fn perform(scenario: &str, cwd: &Path, output_file: &Path, step_id: &str, attempt: u32) -> Result<FakeResult> {
    let summary = |s: &str| serde_json::json!({"summary": s, "status": "done"}).to_string();
    let review = |verdict: &str, findings: serde_json::Value| {
        serde_json::json!({"verdict": verdict, "summary": format!("fake review: {verdict}"), "findings": findings}).to_string()
    };
    let finding = serde_json::json!([{
        "severity": "high", "file": "src/fake.rs", "line": 3,
        "description": "fake finding: missing error handling",
        "recommendation": "handle the error"
    }]);
    match scenario {
        "success" => {
            write(&cwd.join(format!("fake/{step_id}.txt")), &format!("change by {step_id} attempt {attempt}\n"))?;
            write(output_file, &summary(&format!("implemented in attempt {attempt}")))?;
            Ok(FakeResult::Done)
        }
        "noop" => {
            write(output_file, &summary("nothing to do"))?;
            Ok(FakeResult::Done)
        }
        "fail" => Ok(FakeResult::Fail("fake agent failed".into())),
        "timeout" => Ok(FakeResult::Hang),
        "crash" => Ok(FakeResult::Crash),
        "fix-on-retry" => {
            write(&cwd.join("src/fake.rs"), &format!("// attempt {attempt}\n"))?;
            if attempt >= 2 {
                write(&cwd.join("FIXED"), "yes\n")?;
            }
            write(output_file, &summary(&format!("attempt {attempt}")))?;
            Ok(FakeResult::Done)
        }
        "review-findings" => {
            write(output_file, &review("changes_requested", finding))?;
            Ok(FakeResult::Done)
        }
        "review-approve" => {
            write(output_file, &review("approved", serde_json::json!([])))?;
            Ok(FakeResult::Done)
        }
        "review-fix" => {
            if attempt >= 2 {
                write(output_file, &review("approved", serde_json::json!([])))?;
            } else {
                write(output_file, &review("changes_requested", finding))?;
            }
            Ok(FakeResult::Done)
        }
        "review-invalid" => {
            write(output_file, "this is not json {")?;
            Ok(FakeResult::Done)
        }
        "touch-secret" => {
            write(&cwd.join(".env"), "API_KEY=fake\n")?;
            write(output_file, &summary("wrote .env"))?;
            Ok(FakeResult::Done)
        }
        "touch-migration" => {
            write(&cwd.join("db/migrations/001_init.sql"), "create table t(id int);\n")?;
            write(output_file, &summary("added migration"))?;
            Ok(FakeResult::Done)
        }
        other => bail!("unknown fake scenario {other}"),
    }
}

impl AgentRunner for FakeRunner {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> RunnerCapabilities {
        RunnerCapabilities { interactive: false, reports_usage: true, needs_herdr: false, resumable_session: false }
    }
    fn start(&self, req: &AgentRequest, _cancel: &CancelToken, events: &mut dyn FnMut(AgentEvent)) -> Result<AgentBinding> {
        let b = AgentBinding {
            mode: "fake".into(),
            agent_kind: Some("fake".into()),
            agent_name: Some(format!("fake-{}", req.exec_id)),
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
        events(AgentEvent::PromptSending);
        b.prompt_sent = true;
        events(AgentEvent::PromptSent);
        let t0 = Instant::now();
        // Demo/recording knob: make fake agents take visible time.
        if let Some(ms) = std::env::var("HERDR_ORCH_FAKE_DELAY_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            let until = Instant::now() + Duration::from_millis(ms.min(600_000));
            while Instant::now() < until {
                if cancel.is_cancelled() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let r = perform(&self.scenario, &req.worktree, &req.output_file, &req.step_id, req.attempt)?;
        let end = match r {
            FakeResult::Done => AgentEnd::Completed,
            FakeResult::Fail(m) => AgentEnd::Failed(m),
            FakeResult::Crash => AgentEnd::Lost("fake agent crashed".into()),
            FakeResult::Hang => loop {
                if cancel.is_cancelled() {
                    break AgentEnd::Cancelled;
                }
                if Instant::now() >= deadline {
                    break AgentEnd::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(20));
            },
        };
        Ok(AgentOutcome {
            end,
            binding: b.clone(),
            usage: UsageRecord {
                source: UsageSource::Reported,
                input_tokens: Some(1000),
                output_tokens: Some(200),
                cached_tokens: Some(0),
                cost_usd: Some(0.01),
                runtime_ms: Some(t0.elapsed().as_millis() as u64),
                model: Some("fake".into()),
            },
            output_file_text: read_output_file(&req.output_file),
            transcript: format!("[fake agent {} scenario {}]\n{}", self.name, self.scenario, req.prompt),
            exit_code: None,
        })
    }
    fn interrupt(&self, _b: &AgentBinding) -> Result<()> {
        Ok(())
    }
    fn recover(&self, _b: &AgentBinding) -> RecoveryAssessment {
        RecoveryAssessment::Gone
    }
}
