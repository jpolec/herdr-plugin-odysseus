//! Your repository's own benchmark. A finished contract-first run becomes an
//! eval case: its base commit, its task and its approved, locked contract.
//! Replaying a case runs other agents on the same task from the same base,
//! held to the same contract — so "which agent/model/skill is best *here*"
//! is answered with your code and your tests, not a public leaderboard.
//!
//! The oracle is the locked contract, never an agent's own judgement.
//! Replays spend real tokens: `eval run` estimates first and needs `--yes`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::{create_task, EngineCtx, NewTask};
use crate::model::*;
use crate::store::{read_doc, write_doc};

pub const EVAL_WORKFLOW: &str = "eval-task";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalCase {
    pub case_id: String,
    pub repo_root: PathBuf,
    pub base_sha: String,
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub acceptance: Vec<String>,
    /// Contract file path → content.
    pub contract_files: BTreeMap<String, String>,
    pub contract_check: Vec<String>,
    pub contract_sha256: String,
    pub source_run: String,
    /// Runner that produced the recorded solution.
    pub source_runner: Option<String>,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalRun {
    pub eval_id: String,
    pub runners: Vec<String>,
    /// case id → task id (one task, one variant per runner).
    pub tasks: BTreeMap<String, String>,
    pub created_at: Timestamp,
}

fn dir(ctx: &EngineCtx, kind: &str) -> PathBuf {
    ctx.store.layout.root.join("state/eval").join(kind)
}

fn next_id(ctx: &EngineCtx, kind: &str, prefix: &str) -> Result<String> {
    let d = dir(ctx, kind);
    std::fs::create_dir_all(&d)?;
    let _g = ctx.store.lock(&format!("eval-{kind}"))?;
    let n = std::fs::read_dir(&d)?.flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json").and_then(|s| s.trim_start_matches(prefix).parse::<u64>().ok())).max().unwrap_or(0) + 1;
    Ok(format!("{prefix}{n}"))
}

fn load<T: serde::de::DeserializeOwned>(ctx: &EngineCtx, kind: &str, id: &str) -> Result<T> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("invalid id {id:?}");
    }
    read_doc(&ctx.store.layout, &dir(ctx, kind).join(format!("{id}.json")), kind)
}

fn list<T: serde::de::DeserializeOwned>(ctx: &EngineCtx, kind: &str) -> Result<Vec<T>> {
    let mut ids: Vec<String> = std::fs::read_dir(dir(ctx, kind)).map(|rd| rd.flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json").map(String::from)).collect()).unwrap_or_default();
    ids.sort_by_key(|s| s.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse::<u64>().unwrap_or(0));
    Ok(ids.iter().filter_map(|id| load(ctx, kind, id).ok()).collect())
}

pub fn load_case(ctx: &EngineCtx, id: &str) -> Result<EvalCase> {
    load(ctx, "cases", id)
}
pub fn list_cases(ctx: &EngineCtx) -> Result<Vec<EvalCase>> {
    list(ctx, "cases")
}
pub fn load_run(ctx: &EngineCtx, id: &str) -> Result<EvalRun> {
    load(ctx, "runs", id)
}
pub fn list_runs(ctx: &EngineCtx) -> Result<Vec<EvalRun>> {
    list(ctx, "runs")
}

/// Record a finished, approved contract-first run as an eval case.
pub fn record(ctx: &EngineCtx, run_ref: &str) -> Result<EvalCase> {
    let run = ctx.store.resolve_run(run_ref)?;
    if run.status != RunStatus::Succeeded {
        bail!("run {} is {}; record only succeeded runs", run.display_name(), run.status.as_str());
    }
    let c = run.contract.clone().with_context(|| format!("run {} has no contract; eval cases need a locked contract as the oracle (use the contract-first workflow)", run.display_name()))?;
    if c.approval_id.is_none() {
        bail!("the contract of {} was never approved", run.display_name());
    }
    if let Some(existing) = list_cases(ctx)?.into_iter().find(|x| x.source_run == run.run_id) {
        bail!("run {} is already case {}", run.display_name(), existing.case_id);
    }
    let base = run.git.base_sha.clone().context("run has no base commit")?;
    let at = c.commit.clone().or(run.git.head_sha.clone()).context("run has no commit")?;
    let mut files = BTreeMap::new();
    for p in c.files.keys() {
        let b = crate::git::file_at(&run.repo_root, &at, p).with_context(|| format!("contract file {p} not found at {}", &at[..12.min(at.len())]))?;
        if b.len() > 256 * 1024 {
            bail!("contract file {p} is larger than 256 KiB");
        }
        files.insert(p.clone(), String::from_utf8(b).with_context(|| format!("contract file {p} is not UTF-8"))?);
    }
    let task = ctx.store.load_task(&run.task_id)?;
    let case = EvalCase {
        case_id: next_id(ctx, "cases", "C")?,
        repo_root: run.repo_root.clone(),
        base_sha: base,
        title: task.title.clone(),
        text: task.description.clone(),
        acceptance: task.acceptance.clone(),
        contract_files: files,
        contract_check: c.check.clone(),
        contract_sha256: c.sha256.clone(),
        source_run: run.run_id.clone(),
        source_runner: run.steps.iter().find(|e| e.kind == StepKind::Agent && e.step_id != "contract").and_then(|e| e.runner.clone()),
        created_at: now(),
    };
    write_doc(&dir(ctx, "cases").join(format!("{}.json", case.case_id)), "cases", &case)?;
    ctx.audit(crate::audit::EventDraft::new("eval_case_recorded", crate::audit::Actor::human(std::env::var("USER").ok())).run(&run.run_id, &run.task_id).data(serde_json::json!({"case": case.case_id, "contract_sha256": case.contract_sha256})));
    Ok(case)
}

/// What a replay would cost, from local history (average tokens per run of
/// each runner on any workflow). `None` for runners without history.
pub fn estimate(ctx: &EngineCtx, runners: &[String], cases: usize) -> Result<Vec<(String, Option<u64>)>> {
    let stats = crate::engine::maintenance::runner_stats(&ctx.store.list_runs()?);
    Ok(runners
        .iter()
        .map(|r| {
            let per: Vec<u64> = stats.iter().filter(|s| &s.runner == r).filter_map(|s| s.avg_tokens).collect();
            (r.clone(), (!per.is_empty()).then(|| per.iter().sum::<u64>() / per.len() as u64 * cases as u64))
        })
        .collect())
}

/// Queue a replay: one task per case, one variant per runner, each from the
/// case's base commit with its contract pre-approved and locked.
pub fn start(ctx: &EngineCtx, case_ids: &[String], runners: &[String]) -> Result<EvalRun> {
    if runners.is_empty() {
        bail!("give at least one runner (--runners claude,codex)");
    }
    let mut tasks = BTreeMap::new();
    let eval_id = next_id(ctx, "runs", "V")?;
    for id in case_ids {
        let case = load_case(ctx, id)?;
        if crate::git::rev_parse(&case.repo_root, &case.base_sha).is_err() {
            tracing::warn!("eval case {id}: base {} is gone; skipped", case.base_sha);
            continue;
        }
        let t = create_task(
            ctx,
            NewTask {
                text: case.text.clone(),
                title: Some(format!("[{eval_id}/{}] {}", case.case_id, case.title)),
                repo: case.repo_root.clone(),
                options: TaskOptions {
                    workflow: Some(EVAL_WORKFLOW.into()),
                    base_ref: Some(case.base_sha.clone()),
                    variants: runners.len() as u32,
                    variant_runners: runners.to_vec(),
                    runner: (runners.len() == 1).then(|| runners[0].clone()),
                    eval_case: Some(case.case_id.clone()),
                    ..Default::default()
                },
                via: "eval".into(),
                source: Some(TaskSource::Eval { eval_id: eval_id.clone(), case_id: case.case_id.clone() }),
                acceptance: case.acceptance.clone(),
                ..Default::default()
            },
        )?;
        tasks.insert(case.case_id.clone(), t.task_id);
    }
    if tasks.is_empty() {
        bail!("no runnable cases");
    }
    let run = EvalRun { eval_id, runners: runners.to_vec(), tasks, created_at: now() };
    write_doc(&dir(ctx, "runs").join(format!("{}.json", run.eval_id)), "runs", &run)?;
    Ok(run)
}

/// Put a case's contract into a fresh worktree, committed and locked as if
/// a human had approved it (the approval is the original run's).
pub fn seed_contract(ctx: &EngineCtx, case_id: &str, worktree: &Path) -> Result<Contract> {
    let case = load_case(ctx, case_id)?;
    let mut hashes = BTreeMap::new();
    for (p, content) in &case.contract_files {
        match crate::security::check_containment(worktree, Path::new(p))? {
            crate::security::Containment::Inside(_) => {}
            _ => bail!("contract path {p} escapes the worktree"),
        }
        let full = worktree.join(p);
        if let Some(d) = full.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&full, content)?;
        hashes.insert(p.clone(), format!("sha256:{}", crate::store::sha256_hex(content.as_bytes())));
    }
    let commit = crate::git::commit_all(worktree, &format!("eval {case_id}: contract"))?;
    Ok(Contract {
        sha256: Contract::combined_hash(&hashes),
        files: hashes,
        check: case.contract_check.clone(),
        criteria_map: BTreeMap::new(),
        commit,
        red_excerpt: String::new(),
        locked_at: now(),
        approval_id: Some(format!("eval:{case_id}")),
        approved_by: Some(format!("eval case {case_id}")),
        approved_at: Some(case.created_at),
        amended: false,
    })
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct EvalRow {
    pub runner: String,
    pub cases: usize,
    pub passed: usize,
    pub failed: usize,
    pub unfinished: usize,
    pub avg_attempts: f64,
    pub avg_tokens: Option<u64>,
    pub avg_minutes: Option<f64>,
}

/// Per-runner results of one replay: passed = the locked contract and the
/// suite went green.
pub fn report(ctx: &EngineCtx, eval_id: &str) -> Result<Vec<EvalRow>> {
    let ev = load_run(ctx, eval_id)?;
    let mut by: BTreeMap<String, Vec<Run>> = BTreeMap::new();
    for tid in ev.tasks.values() {
        let t = ctx.store.load_task(tid)?;
        for rid in &t.run_ids {
            let r = ctx.store.load_run(rid)?;
            let runner = r.steps.iter().find(|e| e.kind == StepKind::Agent).and_then(|e| e.runner.clone()).or_else(|| ev.runners.get(r.variant_index as usize).cloned()).unwrap_or_else(|| "?".into());
            by.entry(runner).or_default().push(r);
        }
    }
    let mut out = vec![];
    for (runner, runs) in by {
        let mut row = EvalRow { runner, cases: runs.len(), ..Default::default() };
        let (mut attempts, mut tokens, mut minutes) = (0u32, vec![], vec![]);
        for r in &runs {
            match r.status {
                RunStatus::Succeeded => row.passed += 1,
                s if s.is_terminal() || matches!(s, RunStatus::Blocked | RunStatus::NeedsHuman) => row.failed += 1,
                _ => row.unfinished += 1,
            }
            attempts += r.steps.iter().filter(|e| e.step_id == "implement").count() as u32;
            let u = crate::telemetry::run_agent_usage(r);
            if let (Some(i), Some(o)) = (u.input_tokens, u.output_tokens) {
                tokens.push(i + o);
            }
            if let (Some(a), Some(b)) = (r.started_at, r.completed_at) {
                minutes.push((b - a).num_seconds() as f64 / 60.0);
            }
        }
        row.avg_attempts = attempts as f64 / runs.len().max(1) as f64;
        row.avg_tokens = (!tokens.is_empty()).then(|| tokens.iter().sum::<u64>() / tokens.len() as u64);
        row.avg_minutes = (!minutes.is_empty()).then(|| minutes.iter().sum::<f64>() / minutes.len() as f64);
        out.push(row);
    }
    out.sort_by(|a, b| b.passed.cmp(&a.passed).then(a.avg_tokens.cmp(&b.avg_tokens)));
    Ok(out)
}
