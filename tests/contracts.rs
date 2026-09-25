//! Contract-first delivery: tests written first, proven red, approved,
//! locked; the implementation is held to them; the PR carries a receipt.

mod common;

use std::time::Duration;

use common::*;
use herdr_orchestrator::model::*;

const WF: &str = r#"  - id: contract
    type: agent
    output: contract
    prompt: "{{task}} {{acceptance}} {{feedback}}"
    on_failure:
      retry_step: contract
      max_attempts: 1
  - id: approve-contract
    type: approval
    reason: contract ok?
  - id: implement
    type: agent
    prompt: "{{task}} {{contract}} {{feedback}}"
  - id: green
    type: check
    contract: true
    on_failure:
      retry_step: implement
      max_attempts: 1
"#;

fn opts(implement: &str) -> TaskOptions {
    TaskOptions {
        workflow: Some("cf".into()),
        step_runners: [("contract".to_string(), "fake-contract".to_string()), ("implement".to_string(), implement.to_string())].into(),
        ..Default::default()
    }
}

#[test]
fn contract_is_red_then_approved_locked_and_green() {
    let mut h = Harness::new();
    h.workflow("cf", WF);
    let t = h.task_with("Make FIXED exist", opts("fake-fix-contract"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let c = r.contract.clone().expect("contract locked before approval");
    assert_eq!(c.files.keys().cloned().collect::<Vec<_>>(), vec!["tests/contract.txt"]);
    assert_eq!(c.check, vec!["test", "-f", "FIXED"]);
    assert!(c.approval_id.is_none());
    assert!(h.events(&r.run_id).contains(&"contract_locked".into()));
    // The reviewer sees the evidence.
    let a = h.pending_approval(&r.run_id).unwrap();
    let k = a.context.contract.clone().unwrap();
    assert_eq!(k.check, "test -f FIXED");
    assert_eq!(k.criteria_map, vec!["1 → fixed_exists"]);
    assert!(!k.approved);
    h.decide(&r.run_id, true);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let c = r.contract.clone().unwrap();
    assert_eq!(c.approved_by.as_deref(), Some("tester"));
    let ev = h.events(&r.run_id);
    assert!(ev.contains(&"contract_approved".into()));
    // The implementer saw the contract in its prompt.
    let check = herdr_orchestrator::engine::receipt::verify(&h.ctx, &r.run_id, None).unwrap();
    assert!(check.ok, "{check:?}");
    assert!(check.changed.is_empty());
}

#[test]
fn a_contract_that_already_passes_is_sent_back() {
    let mut h = Harness::new();
    h.workflow("cf", WF);
    let mut o = opts("fake-fix-contract");
    o.step_runners.insert("contract".into(), "fake-contract-green".into());
    let t = h.task_with("x", o);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert!(r.status_reason.as_deref().unwrap().contains("already passes"), "{:?}", r.status_reason);
    let contracts: Vec<_> = r.steps.iter().filter(|s| s.step_id == "contract").collect();
    assert_eq!(contracts.len(), 2, "sent back once with feedback");
    assert!(contracts[1].feedback_from.is_some());
    assert!(r.contract.is_none());
    assert!(h.events(&r.run_id).contains(&"contract_not_red".into()));
}

#[test]
fn changing_the_contract_during_implementation_blocks_the_run() {
    let mut h = Harness::new();
    h.workflow("cf", WF);
    let t = h.task_with("x", opts("fake-contract-tamper"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    h.decide(&r.run_id, true);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Blocked);
    assert!(r.status_reason.as_deref().unwrap().contains("contract file changed"), "{:?}", r.status_reason);
    // Nothing was committed after the tampering.
    let wt = r.git.worktree_path.clone().unwrap();
    let head = std::process::Command::new("git").args(["show", "HEAD:tests/contract.txt"]).current_dir(&wt).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout), "FIXED must exist\n");
}

#[test]
fn builtin_contract_first_opens_a_pr_with_a_receipt() {
    let mut h = Harness::new();
    let remote = h.dir.path().join("remote.git");
    assert!(std::process::Command::new("git").args(["init", "-q", "--bare"]).arg(&remote).status().unwrap().success());
    assert!(std::process::Command::new("git").args(["remote", "add", "origin"]).arg(&remote).current_dir(&h.repo).status().unwrap().success());
    let gh = fake_gh(h.dir.path());
    h.rebuild(|c| c.gh = herdr_orchestrator::github::Gh { bin: gh.clone() });
    let t = h.task_with(
        "Make FIXED exist",
        TaskOptions {
            workflow: Some("contract-first".into()),
            step_runners: [("contract", "fake-contract"), ("implement", "fake-fix-contract"), ("review", "fake-review-approve")].into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            ..Default::default()
        },
    );
    let tid = t.task_id.clone();
    let ok = h.until(Duration::from_secs(60), |h| {
        let r = h.runs_of(&tid)[0].clone();
        if let Some(a) = h.pending_approval(&r.run_id) {
            herdr_orchestrator::approvals::decide(&h.ctx.store, &a.approval_id, true, Some("tester".into()), None).unwrap();
        }
        r.status.is_terminal()
    });
    assert!(ok);
    let r = &h.runs_of(&tid)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let log = std::fs::read_to_string(h.dir.path().join("gh.log")).unwrap();
    assert!(log.contains("herdr-orchestrator-receipt: v1"), "{log}");
    assert!(log.contains(&r.contract.as_ref().unwrap().sha256));
    assert!(herdr_orchestrator::engine::receipt::verify(&h.ctx, r.pr_url.as_deref().unwrap(), None).unwrap().ok);
}

#[test]
fn approver_may_amend_the_contract_and_the_receipt_says_so() {
    let mut h = Harness::new();
    h.workflow("cf", WF);
    let t = h.task_with("x", opts("fake-fix-contract"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let wt = r.git.worktree_path.clone().unwrap();
    std::fs::write(wt.join("tests/contract.txt"), "FIXED must exist and be checked\n").unwrap();
    h.decide(&r.run_id, true);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let c = r.contract.clone().unwrap();
    assert!(c.amended);
    assert!(h.events(&r.run_id).contains(&"contract_amended".into()));
    assert!(herdr_orchestrator::engine::receipt::verify(&h.ctx, &r.run_id, None).unwrap().ok);
}

#[test]
fn contract_runs_become_eval_cases_replayed_on_other_runners() {
    let mut h = Harness::new();
    h.workflow("cf", WF);
    let t = h.task_with("Make FIXED exist", opts("fake-fix-contract"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    h.decide(&r.run_id, true);
    let r = h.settle(&t.task_id)[0].clone();
    assert_eq!(r.status, RunStatus::Succeeded);
    let case = herdr_orchestrator::eval::record(&h.ctx, &r.run_id).unwrap();
    assert_eq!(case.case_id, "C1");
    assert_eq!(case.contract_files["tests/contract.txt"], "FIXED must exist\n");
    assert!(herdr_orchestrator::eval::record(&h.ctx, &r.run_id).is_err(), "recorded once");
    // A plain run without a contract cannot be a case.
    let t2 = h.task("no contract", "quick-task", Some("fake-success"));
    let r2 = h.settle(&t2.task_id)[0].clone();
    assert!(format!("{:#}", herdr_orchestrator::eval::record(&h.ctx, &r2.run_id).unwrap_err()).contains("no contract"));

    let ev = herdr_orchestrator::eval::start(&h.ctx, &["C1".into()], &["fake-fix-contract".into(), "fake-success".into(), "fake-contract-tamper".into()]).unwrap();
    let tid = ev.tasks["C1"].clone();
    let runs = h.settle(&tid);
    assert_eq!(runs.len(), 3);
    for r in &runs {
        let c = r.contract.clone().expect("seeded contract");
        assert_eq!(c.sha256, case.contract_sha256, "same oracle for every runner");
        assert_eq!(r.git.base_sha.as_deref(), Some(case.base_sha.as_str()));
    }
    let rows = herdr_orchestrator::eval::report(&h.ctx, &ev.eval_id).unwrap();
    let row = |n: &str| rows.iter().find(|r| r.runner == n).unwrap().clone();
    assert_eq!(row("fake-fix-contract").passed, 1);
    assert_eq!(row("fake-success").failed, 1, "never satisfies the contract");
    assert_eq!(row("fake-contract-tamper").failed, 1, "cannot cheat by editing the contract");
    assert_eq!(rows[0].runner, "fake-fix-contract", "best first");
}
