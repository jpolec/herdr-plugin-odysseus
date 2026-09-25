//! Durable human approval requests.
//!
//! A run that needs a human writes an [`ApprovalRequest`] with full context
//! and waits. Decisions are written by the CLI/TUI (under a file lock) and
//! observed by the run driver, which records `approval_granted` /
//! `approval_denied` in the run's audit log. Approvals are one-shot: MVP
//! never persists a weakened policy.

use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::model::{now, ChangedFile, Timestamp};
use crate::policies::PolicyDecision;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    /// The run was cancelled while waiting.
    Cancelled,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// An explicit `type: approval` workflow step.
    WorkflowStep,
    /// A policy evaluation returned REQUIRE_APPROVAL.
    Policy,
}

/// Everything a human needs to decide, captured when the request is made.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ApprovalContext {
    pub task_title: String,
    pub workflow: String,
    pub step_id: String,
    pub agent: Option<String>,
    pub pane_id: Option<String>,
    pub repository: PathBuf,
    pub branch: Option<String>,
    pub worktree: Option<PathBuf>,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<ChangedFile>,
    pub insertions: u64,
    pub deletions: u64,
    /// `step_id: status` of prior checks.
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub policy: Vec<PolicyDecision>,
    /// The exact action that will happen on approval.
    pub pending_action: Option<String>,
    /// Acceptance criteria with the latest review's judgement.
    #[serde(default)]
    pub acceptance: Vec<AcceptanceRow>,
    /// Checks only a human can do (from an accepted epic plan).
    #[serde(default)]
    pub manual_checks: Vec<String>,
    /// The contract under review or in force (contract-first runs).
    #[serde(default)]
    pub contract: Option<ContractSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ContractSummary {
    pub files: Vec<String>,
    pub check: String,
    /// Tail of the failing run on the base (the red proof).
    pub red_excerpt: String,
    /// "criterion → tests" lines.
    pub criteria_map: Vec<String>,
    pub approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AcceptanceRow {
    pub criterion: String,
    /// `met`, `not_met`, `unverifiable`, or `not reviewed`.
    pub status: String,
    #[serde(default)]
    pub evidence: String,
}

/// Criteria of a task joined with the latest acceptance judgement.
pub fn acceptance_rows(criteria: &[String], judged: Option<&serde_json::Value>) -> Vec<AcceptanceRow> {
    criteria
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let j = judged.and_then(|v| v["criteria"].as_array()).and_then(|a| a.iter().find(|x| x["index"].as_u64() == Some(i as u64 + 1)));
            AcceptanceRow {
                criterion: c.clone(),
                status: j.and_then(|x| x["status"].as_str()).unwrap_or("not reviewed").to_string(),
                evidence: j.and_then(|x| x["evidence"].as_str()).unwrap_or("").to_string(),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub run_id: String,
    pub task_id: String,
    pub step_id: String,
    pub exec_id: Option<String>,
    pub kind: ApprovalKind,
    pub reason: String,
    pub context: ApprovalContext,
    pub status: ApprovalStatus,
    pub requested_at: Timestamp,
    pub decided_at: Option<Timestamp>,
    pub decided_by: Option<String>,
    pub decision_note: Option<String>,
    /// Optional deadline; expiry denies (never approves).
    pub expires_at: Option<Timestamp>,
}

impl ApprovalRequest {
    pub fn new(
        run_id: &str,
        task_id: &str,
        step_id: &str,
        kind: ApprovalKind,
        reason: String,
        context: ApprovalContext,
    ) -> Self {
        Self {
            approval_id: crate::model::new_id("ap"),
            run_id: run_id.into(),
            task_id: task_id.into(),
            step_id: step_id.into(),
            exec_id: None,
            kind,
            reason,
            context,
            status: ApprovalStatus::Pending,
            requested_at: now(),
            decided_at: None,
            decided_by: None,
            decision_note: None,
            expires_at: None,
        }
    }
}

/// Record a human decision. Refuses to change an already-decided request,
/// which makes double-approval (two terminals) harmless.
pub fn decide(
    store: &Store,
    approval_id: &str,
    approve: bool,
    user: Option<String>,
    note: Option<String>,
) -> Result<ApprovalRequest> {
    store.update_approval(approval_id, |a| {
        if a.status != ApprovalStatus::Pending {
            bail!(
                "approval {} is already {}",
                a.approval_id,
                a.status.as_str()
            );
        }
        a.status = if approve { ApprovalStatus::Approved } else { ApprovalStatus::Denied };
        a.decided_at = Some(now());
        a.decided_by = user.or_else(|| std::env::var("USER").ok());
        a.decision_note = note;
        Ok(())
    })
}

pub fn pending(store: &Store) -> Result<Vec<ApprovalRequest>> {
    Ok(store
        .list_approvals()?
        .into_iter()
        .filter(|a| a.status == ApprovalStatus::Pending)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_is_one_shot() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        let a = ApprovalRequest::new("run-1", "1", "approval", ApprovalKind::WorkflowStep, "ok?".into(), Default::default());
        s.save_approval(&a).unwrap();
        assert_eq!(pending(&s).unwrap().len(), 1);
        let a2 = decide(&s, &a.approval_id, true, Some("me".into()), None).unwrap();
        assert_eq!(a2.status, ApprovalStatus::Approved);
        assert!(decide(&s, &a.approval_id, false, None, None).is_err());
        assert_eq!(s.load_approval(&a.approval_id).unwrap().status, ApprovalStatus::Approved);
        assert!(pending(&s).unwrap().is_empty());
    }
}
