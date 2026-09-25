//! Receipts: what a contract-locked PR asserts, checked again from git and
//! the audit trail. Anyone with the repository and this machine's state can
//! re-run the check; with signed audit heads (ROADMAP) it would extend to
//! other machines.

use anyhow::{Context, Result};
use serde::Serialize;

use super::EngineCtx;
use crate::model::*;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReceiptCheck {
    pub run: String,
    pub contract_sha256: String,
    pub approved_by: Option<String>,
    /// Commit the files were checked at.
    pub at: String,
    /// Files whose content at `at` differs from the locked hash.
    pub changed: Vec<String>,
    pub audit_ok: bool,
    pub audit_problems: Vec<String>,
    pub ok: bool,
}

/// Re-check a run's contract at `rev` (default: the run's head) and its
/// audit chain.
pub fn verify(ctx: &EngineCtx, run_ref: &str, rev: Option<&str>) -> Result<ReceiptCheck> {
    let run = match ctx.store.resolve_run(run_ref) {
        Ok(r) => r,
        Err(_) => ctx.store.list_runs()?.into_iter().find(|r| r.pr_url.as_deref() == Some(run_ref)).with_context(|| format!("no run or PR {run_ref}"))?,
    };
    let c = run.contract.clone().with_context(|| format!("run {} has no contract", run.display_name()))?;
    let at = match rev {
        Some(r) => crate::git::rev_parse(&run.repo_root, r)?,
        None => run.git.head_sha.clone().or_else(|| run.git.branch.as_deref().and_then(|b| crate::git::rev_parse(&run.repo_root, b).ok())).context("run has no head commit")?,
    };
    let mut changed = vec![];
    for (path, want) in &c.files {
        let got = crate::git::file_at(&run.repo_root, &at, path).map(|b| format!("sha256:{}", crate::store::sha256_hex(&b)));
        if got.as_deref() != Some(want.as_str()) {
            changed.push(path.clone());
        }
    }
    let audit = ctx.audit.verify(Some(&run.run_id))?;
    let ok = changed.is_empty() && audit.ok && c.approval_id.is_some() && Contract::combined_hash(&c.files) == c.sha256;
    Ok(ReceiptCheck {
        run: run.display_name(),
        contract_sha256: c.sha256,
        approved_by: c.approved_by,
        at,
        changed,
        audit_ok: audit.ok,
        audit_problems: audit.problems,
        ok,
    })
}
