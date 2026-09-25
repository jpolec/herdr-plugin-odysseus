//! Epics: from an Architecture Decision Record to accepted, verified work.
//!
//! ```text
//! ADR ──► planning agent (read-only, `output: plan`) ──► proposed plan
//!     ──► human: accept all / some / edit / regenerate / reject
//!     ──► tasks with acceptance criteria and dependencies (normal workflows,
//!         `output: acceptance` review per task)
//!     ──► conformance review of the result against the ADR (`output:
//!         conformance`) ──► proposed follow-ups, accepted like the plan
//! ```
//!
//! Everything an agent proposes here is untrusted text: plan task text only
//! ever reaches prompts, planner-proposed commands become `check` steps that
//! go through the normal policy pre-flight, and nothing starts until a human
//! accepts it.

pub mod engine;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::Timestamp;

// ------------------------------------------------------------------- ADRs

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Adr {
    /// Repository-relative path.
    pub path: String,
    pub title: String,
    pub status: Option<String>,
    pub sha256: String,
}

impl Adr {
    /// `Superseded`, `Rejected`, `Deprecated` ADRs are hidden by default.
    pub fn is_inactive(&self) -> bool {
        self.status.as_deref().is_some_and(|s| {
            let s = s.to_ascii_lowercase();
            ["superseded", "rejected", "deprecated", "withdrawn"].iter().any(|w| s.starts_with(w))
        })
    }
}

/// Title and status of an ADR in Nygard, MADR or similar markdown. Unknown
/// layouts still work: the file name becomes the title.
pub fn parse_adr(rel_path: &str, text: &str) -> Adr {
    let mut title = None;
    let mut status = None;
    let mut lines = text.lines().peekable();
    // YAML front matter (MADR 3+): `status: accepted`.
    if lines.peek().map(|l| l.trim()) == Some("---") {
        lines.next();
        for l in lines.by_ref() {
            if l.trim() == "---" {
                break;
            }
            if let Some(v) = l.strip_prefix("status:") {
                status = Some(v.trim().trim_matches('"').to_string());
            }
            if let Some(v) = l.strip_prefix("title:") {
                title = Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    let mut in_status = false;
    for l in lines {
        let t = l.trim();
        if title.is_none() {
            if let Some(h) = t.strip_prefix("# ") {
                title = Some(h.trim().to_string());
                continue;
            }
        }
        let lower = t.to_ascii_lowercase();
        if status.is_none() {
            // `Status: Accepted`, `* Status: accepted`, `**Status:** Accepted`
            let bare = lower.trim_start_matches(['*', '-', ' ']).replace("**", "");
            if let Some(v) = bare.strip_prefix("status:") {
                let orig = t.trim_start_matches(['*', '-', ' ']).replace("**", "");
                let v2 = orig[orig.len() - v.len()..].trim().to_string();
                if !v2.is_empty() {
                    status = Some(v2);
                    continue;
                }
                in_status = true;
                continue;
            }
            // `## Status` followed by the value on the next non-empty line.
            if lower.starts_with('#') && lower.trim_start_matches('#').trim() == "status" {
                in_status = true;
                continue;
            }
            if in_status && !t.is_empty() {
                status = Some(t.trim_start_matches(['*', '-', ' ']).to_string());
                in_status = false;
            }
        }
    }
    let fallback = Path::new(rel_path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| rel_path.to_string());
    Adr { path: rel_path.to_string(), title: title.unwrap_or(fallback), status, sha256: crate::store::sha256_hex(text.as_bytes()) }
}

/// ADR files (`*.md`) under the configured directories.
pub fn discover(repo: &Path, dirs: &[String]) -> Vec<Adr> {
    let mut out = vec![];
    let mut seen = BTreeSet::new();
    for d in dirs {
        let dir = repo.join(d);
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md")).collect();
        files.sort();
        for f in files {
            let name = f.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            if matches!(name.as_str(), "readme.md" | "index.md" | "template.md") || name.starts_with("adr-template") {
                continue;
            }
            let Ok(canon) = f.canonicalize() else { continue };
            if !seen.insert(canon) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&f) {
                let rel = f.strip_prefix(repo).unwrap_or(&f).to_string_lossy().to_string();
                out.push(parse_adr(&rel, &text));
            }
        }
    }
    out
}

/// Read an ADR given by the user (path relative to the repo or absolute,
/// but inside the repository).
pub fn load_adr(repo: &Path, path: &Path) -> Result<(Adr, String)> {
    let full = if path.is_absolute() { path.to_path_buf() } else { repo.join(path) };
    let canon = full.canonicalize().with_context(|| format!("ADR {} not found", full.display()))?;
    let root = repo.canonicalize()?;
    let rel = canon.strip_prefix(&root).map_err(|_| anyhow::anyhow!("ADR {} is outside the repository", canon.display()))?.to_string_lossy().to_string();
    let md = std::fs::metadata(&canon)?;
    if !md.is_file() || md.len() > 256 * 1024 {
        bail!("ADR {} must be a file of at most 256 KiB", canon.display());
    }
    let text = std::fs::read_to_string(&canon)?;
    Ok((parse_adr(&rel, &text), text))
}

// ------------------------------------------------------------------- plan

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Verification {
    /// Named checks (`tests`, `lint`, `security`).
    pub checks: Vec<String>,
    /// Extra check commands (argv). Untrusted: shown to the human at
    /// acceptance, and policy-checked before they run.
    pub commands: Vec<Vec<String>>,
    /// Things only a human can verify.
    pub manual: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanTask {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub verification: Verification,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub adr_refs: Vec<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    #[serde(default)]
    pub risk: Option<String>,
    /// Paths the task is expected to touch (becomes the task's scope).
    #[serde(default)]
    pub scope: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Plan {
    #[serde(default)]
    pub decision_summary: String,
    pub tasks: Vec<PlanTask>,
    #[serde(default)]
    pub out_of_scope: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
}

/// What a plan is validated against.
pub struct PlanRules<'a> {
    pub max_tasks: usize,
    pub workflows: &'a [String],
    /// Keys that already exist (follow-ups may depend on accepted tasks).
    pub existing_keys: &'a [String],
}

fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 16 && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parse and validate a plan. A plan is never partly accepted just because
/// it was partly valid: any problem rejects the whole plan with every error.
pub fn parse_plan(text: &str, rules: &PlanRules) -> Result<Plan> {
    let plan: Plan = serde_json::from_str(text.trim()).context("plan is not valid JSON of the expected shape")?;
    validate_plan(&plan, rules)?;
    Ok(plan)
}

pub fn validate_plan(plan: &Plan, rules: &PlanRules) -> Result<()> {
    let mut errs = vec![];
    if plan.tasks.is_empty() {
        errs.push("plan has no tasks".to_string());
    }
    if plan.tasks.len() > rules.max_tasks {
        errs.push(format!("plan has {} tasks; at most {} are allowed (epic.max_tasks) — merge small tasks", plan.tasks.len(), rules.max_tasks));
    }
    let mut keys = BTreeSet::new();
    for t in &plan.tasks {
        if !valid_key(&t.key) {
            errs.push(format!("task key {:?} must be 1-16 letters, digits, '-' or '_'", t.key));
        }
        if !keys.insert(t.key.clone()) || rules.existing_keys.contains(&t.key) {
            errs.push(format!("duplicate task key {:?}", t.key));
        }
        if t.title.trim().is_empty() {
            errs.push(format!("{}: empty title", t.key));
        }
        if t.acceptance.iter().all(|a| a.trim().is_empty()) {
            errs.push(format!("{}: needs at least one acceptance criterion", t.key));
        }
        if let Some(w) = &t.workflow {
            if !rules.workflows.contains(w) {
                errs.push(format!("{}: unknown workflow {w:?} (known: {})", t.key, rules.workflows.join(", ")));
            }
        }
        for c in &t.verification.checks {
            if !crate::checks::NAMED_CHECKS.contains(&c.as_str()) {
                errs.push(format!("{}: unknown named check {c:?} (known: {:?})", t.key, crate::checks::NAMED_CHECKS));
            }
        }
        for c in &t.verification.commands {
            if c.is_empty() || c[0].trim().is_empty() || c.iter().any(|a| a.contains("{{")) {
                errs.push(format!("{}: verification commands must be non-empty argv arrays without templates", t.key));
            }
        }
        for g in &t.scope {
            if globset::Glob::new(g).is_err() || g.starts_with('/') || g.contains("..") {
                errs.push(format!("{}: invalid scope glob {g:?}", t.key));
            }
        }
    }
    for t in &plan.tasks {
        for d in &t.depends_on {
            if !keys.contains(d) && !rules.existing_keys.contains(d) {
                errs.push(format!("{}: depends on unknown task {d:?}", t.key));
            }
            if d == &t.key {
                errs.push(format!("{}: depends on itself", t.key));
            }
        }
    }
    if errs.is_empty() {
        if let Err(cycle) = topo_order(&plan.tasks) {
            errs.push(format!("dependency cycle: {cycle}"));
        }
    }
    if !errs.is_empty() {
        bail!("invalid plan:\n- {}", errs.join("\n- "));
    }
    Ok(())
}

/// Tasks in dependency order (dependencies first; stable by plan order).
/// Dependencies on keys outside `tasks` are treated as already satisfied.
pub fn topo_order(tasks: &[PlanTask]) -> std::result::Result<Vec<String>, String> {
    let keys: BTreeSet<&str> = tasks.iter().map(|t| t.key.as_str()).collect();
    let mut done: Vec<String> = vec![];
    let mut left: Vec<&PlanTask> = tasks.iter().collect();
    while !left.is_empty() {
        let before = left.len();
        let mut next = vec![];
        for t in left {
            if t.depends_on.iter().all(|d| !keys.contains(d.as_str()) || done.contains(d)) {
                done.push(t.key.clone());
            } else {
                next.push(t);
            }
        }
        left = next;
        if left.len() == before {
            return Err(left.iter().map(|t| t.key.as_str()).collect::<Vec<_>>().join(" → "));
        }
    }
    Ok(done)
}

// ------------------------------------------------------------- acceptance

/// Validate an `output: acceptance` result. When `expected` > 0 every
/// criterion 1..=expected must be judged exactly once.
pub fn parse_acceptance(text: &str, expected: usize) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).context("acceptance output is not valid JSON")?;
    let crit = v.get("criteria").and_then(|c| c.as_array()).context("acceptance output has no `criteria` array")?;
    let verdict = v.get("verdict").and_then(|x| x.as_str()).context("acceptance output has no string `verdict`")?;
    if !matches!(verdict, "approved" | "changes_requested") {
        bail!("unknown acceptance verdict {verdict:?}");
    }
    let mut seen = BTreeSet::new();
    for (i, c) in crit.iter().enumerate() {
        let idx = c.get("index").and_then(|x| x.as_u64()).with_context(|| format!("criterion {i} has no numeric index"))?;
        let st = c.get("status").and_then(|x| x.as_str()).with_context(|| format!("criterion {idx} has no status"))?;
        if !matches!(st, "met" | "not_met" | "unverifiable") {
            bail!("criterion {idx} has unknown status {st:?} (met, not_met, unverifiable)");
        }
        if !seen.insert(idx) {
            bail!("criterion {idx} is judged twice");
        }
    }
    if expected > 0 {
        let want: BTreeSet<u64> = (1..=expected as u64).collect();
        if seen != want {
            bail!("expected a judgement for each of the {expected} criteria (indices 1..={expected}), got {:?}", seen);
        }
    }
    let unmet = crit.iter().filter(|c| c["status"] == "not_met").count();
    if unmet > 0 && verdict == "approved" {
        bail!("verdict `approved` with {unmet} criterion(s) not met");
    }
    Ok(v)
}

/// Criteria marked `not_met`, as feedback for the implementer.
pub fn unmet_feedback(v: &serde_json::Value, criteria: &[String]) -> String {
    let mut s = String::from("These acceptance criteria are not met yet:\n");
    for c in v["criteria"].as_array().into_iter().flatten().filter(|c| c["status"] == "not_met") {
        let i = c["index"].as_u64().unwrap_or(0) as usize;
        let text = criteria.get(i.wrapping_sub(1)).cloned().unwrap_or_default();
        s.push_str(&format!("{i}. {text}\n   reviewer: {}\n", c["evidence"].as_str().unwrap_or("")));
    }
    s
}

/// Validate an `output: contract` result: the test files exist inside the
/// worktree, and (when the task has criteria) every criterion maps to at
/// least one test.
pub fn parse_contract(text: &str, criteria: usize, worktree: &Path) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).context("contract output is not valid JSON")?;
    let files = v.get("files").and_then(|f| f.as_array()).context("contract output has no `files` array")?;
    if files.is_empty() {
        bail!("contract lists no test files");
    }
    if files.len() > 50 {
        bail!("contract lists {} files; keep it small (at most 50)", files.len());
    }
    for f in files {
        let p = f.as_str().context("`files` must be paths")?;
        let rel = Path::new(p);
        if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) || p.starts_with(crate::git::ORCH_DIR) {
            bail!("contract file {p:?} must be a relative path inside the worktree");
        }
        match crate::security::check_containment(worktree, rel)? {
            crate::security::Containment::Inside(rel) if worktree.join(&rel).is_file() => {}
            _ => bail!("contract file {p:?} does not exist in the worktree (or escapes it)"),
        }
    }
    if let Some(c) = v.get("check") {
        let argv: Vec<String> = serde_json::from_value(c.clone()).context("`check` must be an argv array of strings")?;
        if argv.is_empty() || argv[0].trim().is_empty() || argv.iter().any(|a| a.contains("{{")) {
            bail!("`check` must be a non-empty argv array without templates");
        }
    }
    if criteria > 0 {
        let map = v.get("criteria_map").and_then(|m| m.as_object()).context("contract output has no `criteria_map` object")?;
        let missing: Vec<usize> = (1..=criteria).filter(|i| map.get(&i.to_string()).and_then(|t| t.as_array()).is_none_or(|a| a.is_empty())).collect();
        if !missing.is_empty() {
            bail!("acceptance criteria {:?} have no test in `criteria_map`", missing);
        }
    }
    Ok(v)
}

/// Validate an `output: conformance` result (epic vs. its ADR).
pub fn parse_conformance(text: &str) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).context("conformance output is not valid JSON")?;
    let points = v.get("points").and_then(|p| p.as_array()).context("conformance output has no `points` array")?;
    for (i, p) in points.iter().enumerate() {
        let st = p.get("status").and_then(|x| x.as_str()).with_context(|| format!("point {i} has no status"))?;
        if !matches!(st, "covered" | "partial" | "missing") {
            bail!("point {i} has unknown status {st:?} (covered, partial, missing)");
        }
        if p.get("point").and_then(|x| x.as_str()).is_none() {
            bail!("point {i} has no `point` text");
        }
    }
    if let Some(f) = v.get("followups") {
        let tasks: Vec<PlanTask> = serde_json::from_value(f.clone()).context("`followups` must be plan tasks")?;
        let _ = tasks;
    }
    Ok(v)
}

// ------------------------------------------------------------------ epics

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpicStatus {
    /// The planning agent is working.
    Planning,
    /// A valid plan waits for a human.
    Proposed,
    /// Planning failed; see `status_reason`.
    PlanFailed,
    /// Tasks were created; work is in progress.
    Accepted,
    /// Every accepted task has finished.
    Done,
    Rejected,
}

impl EpicStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Proposed => "proposed",
            Self::PlanFailed => "plan_failed",
            Self::Accepted => "accepted",
            Self::Done => "done",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Epic {
    pub epic_id: String,
    pub repo_root: PathBuf,
    pub adr: Adr,
    pub status: EpicStatus,
    #[serde(default)]
    pub status_reason: Option<String>,
    /// Runner for the planning and conformance agents.
    #[serde(default)]
    pub planner_runner: Option<String>,
    /// Planning tasks, newest last (regenerating adds one).
    #[serde(default)]
    pub planning_tasks: Vec<String>,
    #[serde(default)]
    pub plan: Option<Plan>,
    #[serde(default)]
    pub plan_sha256: Option<String>,
    /// Plan key → created task id.
    #[serde(default)]
    pub tasks: BTreeMap<String, String>,
    /// Keys a human rejected (not created, not proposed again).
    #[serde(default)]
    pub declined: Vec<String>,
    #[serde(default)]
    pub conformance_task: Option<String>,
    #[serde(default)]
    pub conformance: Option<serde_json::Value>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default)]
    pub decided_by: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Epic {
    /// Plan tasks nobody has accepted or declined yet.
    pub fn open_keys(&self) -> Vec<String> {
        self.plan.as_ref().map(|p| p.tasks.iter().map(|t| t.key.clone()).filter(|k| !self.tasks.contains_key(k) && !self.declined.contains(k)).collect()).unwrap_or_default()
    }

    pub fn plan_task(&self, key: &str) -> Option<&PlanTask> {
        self.plan.as_ref()?.tasks.iter().find(|t| t.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules<'a>(w: &'a [String]) -> PlanRules<'a> {
        PlanRules { max_tasks: 5, workflows: w, existing_keys: &[] }
    }

    fn task(key: &str, deps: &[&str]) -> serde_json::Value {
        serde_json::json!({"key": key, "title": format!("do {key}"), "acceptance": ["works"], "depends_on": deps})
    }

    #[test]
    fn adr_formats() {
        let nygard = "# 7. Rate limiting\n\nDate: 2026-01-01\n\n## Status\n\nAccepted\n\n## Context\n...";
        let a = parse_adr("docs/adr/0007-rate-limiting.md", nygard);
        assert_eq!(a.title, "7. Rate limiting");
        assert_eq!(a.status.as_deref(), Some("Accepted"));
        let madr = "---\nstatus: proposed\ndate: 2026-01-01\n---\n# Use Postgres\n\n## Context and Problem Statement\n";
        let a = parse_adr("docs/decisions/0002-db.md", madr);
        assert_eq!((a.title.as_str(), a.status.as_deref()), ("Use Postgres", Some("proposed")));
        let inline = "# Cache\n\n* Status: superseded by ADR-9\n";
        let a = parse_adr("adr/3.md", inline);
        assert_eq!(a.status.as_deref(), Some("superseded by ADR-9"));
        assert!(a.is_inactive());
        let bold = "# X\n**Status:** Accepted\n";
        assert_eq!(parse_adr("a.md", bold).status.as_deref(), Some("Accepted"));
        let plain = "no heading at all";
        let a = parse_adr("docs/adr/0001-thing.md", plain);
        assert_eq!(a.title, "0001-thing");
        assert!(a.status.is_none() && !a.is_inactive());
    }

    #[test]
    fn plan_validation() {
        let w = vec!["epic-task".to_string(), "quick-task".to_string()];
        let ok = serde_json::json!({"tasks": [task("T1", &[]), task("T2", &["T1"]), task("T3", &["T1", "T2"])]}).to_string();
        let p = parse_plan(&ok, &rules(&w)).unwrap();
        assert_eq!(topo_order(&p.tasks).unwrap(), vec!["T1", "T2", "T3"]);
        let cyc = serde_json::json!({"tasks": [task("A", &["B"]), task("B", &["A"])]}).to_string();
        assert!(format!("{:#}", parse_plan(&cyc, &rules(&w)).unwrap_err()).contains("cycle"));
        let bad = serde_json::json!({"tasks": [
            {"key": "T1", "title": "", "acceptance": [], "workflow": "nope", "verification": {"checks": ["fuzz"], "commands": [[]]}, "depends_on": ["T9"]},
            task("T1", &[]),
        ]})
        .to_string();
        let e = format!("{:#}", parse_plan(&bad, &rules(&w)).unwrap_err());
        for want in ["empty title", "acceptance criterion", "unknown workflow", "unknown named check", "argv", "unknown task \"T9\"", "duplicate task key"] {
            assert!(e.contains(want), "{want} missing in {e}");
        }
        let many = serde_json::json!({"tasks": (0..6).map(|i| task(&format!("T{i}"), &[])).collect::<Vec<_>>()}).to_string();
        assert!(format!("{:#}", parse_plan(&many, &rules(&w)).unwrap_err()).contains("at most 5"));
        assert!(parse_plan("not json", &rules(&w)).is_err());
        // Follow-ups may depend on tasks accepted earlier.
        let existing = vec!["T1".to_string()];
        let r = PlanRules { max_tasks: 5, workflows: &w, existing_keys: &existing };
        let f = serde_json::json!({"tasks": [task("F1", &["T1"])]}).to_string();
        assert!(parse_plan(&f, &r).is_ok());
        let dup = serde_json::json!({"tasks": [task("T1", &[])]}).to_string();
        assert!(parse_plan(&dup, &r).is_err());
    }

    #[test]
    fn acceptance_validation() {
        let ok = r#"{"criteria":[{"index":1,"status":"met","evidence":"t"},{"index":2,"status":"unverifiable"}],"verdict":"approved"}"#;
        assert!(parse_acceptance(ok, 2).is_ok());
        assert!(parse_acceptance(ok, 3).is_err(), "criterion 3 missing");
        let lie = r#"{"criteria":[{"index":1,"status":"not_met"}],"verdict":"approved"}"#;
        assert!(parse_acceptance(lie, 1).is_err());
        let unmet = r#"{"criteria":[{"index":1,"status":"not_met","evidence":"no 429"}],"verdict":"changes_requested"}"#;
        let v = parse_acceptance(unmet, 1).unwrap();
        let fb = unmet_feedback(&v, &["returns 429".to_string()]);
        assert!(fb.contains("1. returns 429") && fb.contains("no 429"));
        assert!(parse_acceptance(r#"{"criteria":[{"index":1,"status":"meh"}],"verdict":"approved"}"#, 0).is_err());
        assert!(parse_acceptance(r#"{"criteria":[{"index":1,"status":"met"},{"index":1,"status":"met"}],"verdict":"approved"}"#, 0).is_err());
    }

    #[test]
    fn conformance_validation() {
        let ok = r#"{"points":[{"point":"limit per key","status":"covered","evidence":"x"}],"followups":[{"key":"F1","title":"t","acceptance":["a"]}],"summary":"s"}"#;
        assert!(parse_conformance(ok).is_ok());
        assert!(parse_conformance(r#"{"points":[{"point":"x","status":"done"}]}"#).is_err());
        assert!(parse_conformance(r#"{"points":[{"status":"covered"}]}"#).is_err());
    }

    #[test]
    fn discovery_skips_readmes_and_dedupes() {
        let d = tempfile::tempdir().unwrap();
        let adr = d.path().join("docs/adr");
        std::fs::create_dir_all(&adr).unwrap();
        std::fs::write(adr.join("0001-a.md"), "# A\n\nStatus: Accepted\n").unwrap();
        std::fs::write(adr.join("README.md"), "# index").unwrap();
        std::fs::write(adr.join("notes.txt"), "x").unwrap();
        let found = discover(d.path(), &["docs/adr".into(), "docs/adr/".into(), "missing".into()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "docs/adr/0001-a.md");
        let (a, text) = load_adr(d.path(), Path::new("docs/adr/0001-a.md")).unwrap();
        assert_eq!(a.title, "A");
        assert!(text.contains("Accepted"));
        assert!(load_adr(d.path(), Path::new("/etc/hosts")).is_err());
    }
}
