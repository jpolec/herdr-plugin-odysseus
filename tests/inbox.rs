//! Morning inbox, risk levels, batch approval and the night shift.

mod common;

use std::time::Duration;

use common::*;
use herdr_orchestrator::engine::digest::{self, InboxItem, RiskLevel};
use herdr_orchestrator::model::*;

const SHIP: &str = "  - id: implement\n    type: agent\n    prompt: x\n  - id: review\n    type: agent\n    output: review\n    prompt: r\n  - id: ship\n    type: approval\n    reason: ship it?\n";

fn opts(implement: &str) -> TaskOptions {
    TaskOptions { workflow: Some("ship".into()), step_runners: [("implement".to_string(), implement.to_string()), ("review".to_string(), "fake-review-approve".to_string())].into(), ..Default::default() }
}

#[test]
fn inbox_ranks_by_risk_and_batch_approves_only_low_risk_ship_its() {
    let mut h = Harness::new();
    h.workflow("ship", SHIP);
    let low = h.task_with("small reviewed change", opts("fake-success"));
    let high = h.task_with("adds a migration", opts("fake-touch-migration"));
    let rl = h.wait_status(&low.task_id, RunStatus::AwaitingApproval);
    let rh = h.wait_status(&high.task_id, RunStatus::AwaitingApproval);
    // The migration is a policy question first: never batch-approved.
    let items = digest::inbox(&h.ctx, chrono::Duration::hours(1)).unwrap();
    let policy_ask = items.iter().find_map(|i| match i {
        InboxItem::Approval { run_id, workflow_step, risk, .. } if *run_id == rh.run_id => Some((*workflow_step, risk.clone())),
        _ => None,
    });
    let (ws, risk) = policy_ask.unwrap();
    assert!(!ws);
    assert_eq!(risk.level, RiskLevel::High);
    assert!(risk.reasons.iter().any(|r| r.contains("approve-migrations")), "{:?}", risk.reasons);
    let low_item = items.iter().find_map(|i| match i {
        InboxItem::Approval { run_id, risk, .. } if *run_id == rl.run_id => Some(risk.clone()),
        _ => None,
    });
    assert_eq!(low_item.unwrap().level, RiskLevel::Low);
    let done = digest::approve_batch(&h.ctx, RiskLevel::Low, Some("me".into())).unwrap();
    assert_eq!(done.len(), 1);
    assert!(h.pending_approval(&rh.run_id).is_some(), "the policy question stays");
    assert_eq!(h.settle(&low.task_id)[0].status, RunStatus::Succeeded);
    assert!(h.events(&rl.run_id).contains(&"approval_batch".into()));
    // Answer the migration question; the ship-it is high risk: not batched.
    h.decide(&rh.run_id, true);
    let hid = high.task_id.clone();
    h.until(Duration::from_secs(20), |h| h.pending_approval(&h.runs_of(&hid)[0].run_id).is_some_and(|a| a.step_id == "ship"));
    assert!(digest::approve_batch(&h.ctx, RiskLevel::Low, None).unwrap().is_empty());
    assert_eq!(digest::approve_batch(&h.ctx, RiskLevel::High, None).unwrap().len(), 1);
    assert_eq!(h.settle(&high.task_id)[0].status, RunStatus::Succeeded);
    // Finished work shows up as ready, with reasons.
    let items = digest::inbox(&h.ctx, chrono::Duration::hours(1)).unwrap();
    assert_eq!(items.iter().filter(|i| matches!(i, InboxItem::Ready { .. })).count(), 2);
}

#[test]
fn shift_budget_pauses_the_queue_and_reports() {
    let mut h = Harness::new();
    digest::start_shift(&h.ctx, None, Some(1000), None).unwrap();
    let a = h.task("first", "quick-task", Some("fake-success"));
    h.settle(&a.task_id);
    // 1200 tokens used: the next tick ends the shift and pauses the queue.
    let b = h.task("second", "quick-task", Some("fake-success"));
    h.until(Duration::from_secs(5), |h| h.ctx.store.load_scheduler().unwrap().paused);
    assert!(h.ctx.store.load_scheduler().unwrap().paused);
    let s = digest::load_shift(&h.ctx).unwrap();
    assert!(s.ended.unwrap().contains("budget"));
    h.until(Duration::from_millis(500), |_| false);
    assert!(h.ctx.store.load_task(&b.task_id).unwrap().run_ids.is_empty(), "nothing new starts");
    assert!(digest::parse_until("8h").unwrap() > now());
    assert!(digest::parse_until("07:30").unwrap() > now());
    assert!(digest::parse_until("soon").is_err());
}

#[test]
fn risk_reasons_cover_contracts_and_unreviewed_changes() {
    let mut h = Harness::new();
    let t = h.task("unreviewed", "quick-task", Some("fake-success"));
    let r = h.settle(&t.task_id)[0].clone();
    let risk = digest::assess(&r);
    assert_eq!(risk.level, RiskLevel::Medium);
    assert!(risk.reasons.iter().any(|x| x.contains("nobody reviewed")));
    let mut r2 = r.clone();
    r2.contract = Some(Contract { files: Default::default(), sha256: "x".into(), check: vec![], criteria_map: Default::default(), commit: None, red_excerpt: String::new(), locked_at: now(), approval_id: Some("a".into()), approved_by: None, approved_at: None, amended: false });
    let risk = digest::assess(&r2);
    assert_eq!(risk.level, RiskLevel::Low);
    assert!(risk.reasons.iter().any(|x| x.contains("approved contract")));
}
