//! Epics: ADR → plan → accepted tasks with dependencies → acceptance
//! review → conformance review. Real git, fake agents.

mod common;

use std::time::Duration;

use common::*;
use herdr_orchestrator::epic::engine::{self as epic, AcceptOptions};
use herdr_orchestrator::epic::EpicStatus;
use herdr_orchestrator::model::*;

const ADR: &str = "# 7. Rate limiting\n\n## Status\n\nAccepted\n\n## Decision\n\nLimit webhooks per API key.\n";

/// Implement → gated acceptance review; no PR (keeps tests local).
const ACC_WF: &str = r#"  - id: implement
    type: agent
    prompt: "{{task}} {{acceptance}} {{feedback}}"
  - id: acceptance
    type: agent
    output: acceptance
    gate: true
    prompt: "{{acceptance}}"
    on_failure:
      retry_step: implement
      max_attempts: 2
"#;

fn setup(mode: &str) -> Harness {
    let h = Harness::new();
    std::fs::create_dir_all(h.repo.join("docs/adr")).unwrap();
    std::fs::write(h.repo.join("docs/adr/0007-rate-limiting.md"), ADR).unwrap();
    git(&h.repo, &["add", "."]);
    git(&h.repo, &["commit", "-q", "-m", "adr"]);
    h.workflow("acc-only", ACC_WF);
    h.project_file("config.yaml", &format!("epic:\n  task_workflow: acc-only\n  dependency_mode: {mode}\n"));
    h
}

fn git(cwd: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn wait_epic(h: &mut Harness, id: &str, want: EpicStatus) -> herdr_orchestrator::epic::Epic {
    let id2 = id.to_string();
    let ok = h.until(Duration::from_secs(60), |h| h.ctx.store.load_epic(&id2).map(|e| e.status == want).unwrap_or(false));
    let e = h.ctx.store.load_epic(id).unwrap();
    assert!(ok, "epic {id} is {:?} ({:?}), wanted {want:?}", e.status, e.status_reason);
    e
}

fn accept_opts(implement: &str, acceptance: &str) -> AcceptOptions {
    AcceptOptions {
        runner: None,
        step_runners: [("implement".to_string(), implement.to_string()), ("acceptance".to_string(), acceptance.to_string())].into(),
        via: "test".into(),
        ..Default::default()
    }
}

fn task_status(h: &Harness, id: &str) -> TaskStatus {
    h.ctx.store.load_task(id).unwrap().status
}

#[test]
fn adr_to_plan_to_stacked_tasks_to_conformance() {
    let mut h = setup("stacked");
    let e = epic::create_epic(&h.ctx, &h.repo, std::path::Path::new("docs/adr/0007-rate-limiting.md"), Some("fake-plan".into()), "test").unwrap();
    assert_eq!(e.epic_id, "E1");
    assert_eq!(e.adr.title, "7. Rate limiting");
    let e = wait_epic(&mut h, "E1", EpicStatus::Proposed);
    let plan = e.plan.clone().unwrap();
    assert_eq!(plan.tasks.len(), 3);
    assert_eq!(e.open_keys(), vec!["T1", "T2", "T3"]);
    // The planning run changed nothing and committed nothing.
    let prun = &h.runs_of(&e.planning_tasks[0])[0];
    assert!(prun.git.commits.is_empty());
    assert!(h.events(&prun.run_id).contains(&"plan_proposed".into()));

    // Accepting T2 without its dependency is refused.
    let err = epic::accept(&h.ctx, "E1", AcceptOptions { only: Some(vec!["T2".into()]), ..accept_opts("fake-success", "fake-acceptance-met") }).unwrap_err();
    assert!(format!("{err:#}").contains("depends on T1"), "{err:#}");

    let e = epic::accept(&h.ctx, "E1", accept_opts("fake-success", "fake-acceptance-met")).unwrap();
    assert_eq!(e.status, EpicStatus::Accepted);
    let (t1, t2, t3) = (e.tasks["T1"].clone(), e.tasks["T2"].clone(), e.tasks["T3"].clone());
    let task2 = h.ctx.store.load_task(&t2).unwrap();
    assert_eq!(task2.depends_on, vec![t1.clone()]);
    assert_eq!(task2.acceptance, vec!["part T2 works"]);
    assert_eq!(task2.manual_checks, vec!["look at part T2"]);
    assert_eq!(task2.options.scope, vec!["fake/**"]);
    assert_eq!(task2.epic.as_ref().unwrap().key, "T2");

    // T1 and T3 start; T2 waits for T1.
    h.until(Duration::from_secs(5), |h| h.ctx.store.load_task(&t2).unwrap().waiting_on.is_some());
    assert!(h.ctx.store.load_task(&t2).unwrap().waiting_on.unwrap().contains(&format!("#{t1}")));
    for t in [&t1, &t3, &t2] {
        let runs = h.settle(t);
        assert_eq!(runs[0].status, RunStatus::Succeeded, "task {t}: {:?}", runs[0].status_reason);
    }
    // Stacked: T2 branched from T1's branch.
    let r1 = &h.runs_of(&t1)[0];
    let r2 = &h.runs_of(&t2)[0];
    assert_eq!(r2.git.base_ref, r1.git.branch.clone().unwrap());
    // The acceptance review ran and was recorded, and the plan's named check
    // (`tests`, already in the workflow? no: acc-only has none) was inserted.
    assert!(r2.steps.iter().any(|s| s.step_id.starts_with("plan-tests")), "{:?}", r2.steps.iter().map(|s| &s.step_id).collect::<Vec<_>>());
    assert!(h.events(&r2.run_id).contains(&"acceptance_verified".into()));
    assert!(r2.workflow_yaml.contains("verification from the accepted plan"));

    let e = wait_epic(&mut h, "E1", EpicStatus::Done);
    // Conformance review proposes follow-ups, accepted like the plan.
    h.ctx.store.update_epic("E1", |x| {
        x.planner_runner = Some("fake-conformance".into());
        Ok(())
    })
    .unwrap();
    let e2 = epic::verify(&h.ctx, &e.epic_id, "test").unwrap();
    let ct = e2.conformance_task.clone().unwrap();
    let cr = h.settle(&ct);
    assert_eq!(cr[0].status, RunStatus::Succeeded, "{:?}", cr[0].status_reason);
    assert_eq!(cr[0].git.base_ref, "main", "the review starts from the base and inspects task branches");
    let ctask = h.ctx.store.load_task(&ct).unwrap();
    assert!(ctask.description.contains(&h.runs_of(&t3)[0].git.branch.clone().unwrap()), "every task branch is listed");
    h.until(Duration::from_secs(5), |h| h.ctx.store.load_epic("E1").unwrap().conformance.is_some());
    let e = h.ctx.store.load_epic("E1").unwrap();
    assert_eq!(e.open_keys(), vec!["F1"]);
    assert_eq!(e.plan_task("F1").unwrap().depends_on, vec!["T1"]);
    let e = epic::accept(&h.ctx, "E1", AcceptOptions { only: Some(vec!["F1".into()]), ..accept_opts("fake-success", "fake-acceptance-met") }).unwrap();
    assert_eq!(e.status, EpicStatus::Accepted);
    let f1 = e.tasks["F1"].clone();
    assert_eq!(h.ctx.store.load_task(&f1).unwrap().depends_on, vec![t1]);
    assert!(h.ctx.audit.verify(None).unwrap().ok);
}

#[test]
fn merged_mode_waits_for_the_dependency_to_reach_the_base() {
    let mut h = setup("merged");
    epic::create_epic(&h.ctx, &h.repo, std::path::Path::new("docs/adr/0007-rate-limiting.md"), Some("fake-plan".into()), "test").unwrap();
    wait_epic(&mut h, "E1", EpicStatus::Proposed);
    let e = epic::accept(&h.ctx, "E1", AcceptOptions { only: Some(vec!["T1".into(), "T2".into()]), ..accept_opts("fake-success", "fake-acceptance-met") }).unwrap();
    let (t1, t2) = (e.tasks["T1"].clone(), e.tasks["T2"].clone());
    assert_eq!(h.settle(&t1)[0].status, RunStatus::Succeeded);
    h.until(Duration::from_secs(5), |h| h.ctx.store.load_task(&t2).unwrap().waiting_on.as_deref().is_some_and(|w| w.contains("merged")));
    assert_eq!(task_status(&h, &t2), TaskStatus::Queued);
    assert!(h.ctx.store.load_task(&t2).unwrap().run_ids.is_empty());
    // A human merges T1 into main; T2 starts from the updated base.
    let b1 = h.runs_of(&t1)[0].git.branch.clone().unwrap();
    git(&h.repo, &["-c", "user.email=t@e", "-c", "user.name=t", "merge", "-q", "--no-ff", "-m", "merge", &b1]);
    let r2 = &h.settle(&t2)[0];
    assert_eq!(r2.status, RunStatus::Succeeded, "{:?}", r2.status_reason);
    assert_eq!(r2.git.base_ref, "main");
}

#[test]
fn failed_dependency_blocks_until_a_human_unblocks() {
    let mut h = setup("stacked");
    epic::create_epic(&h.ctx, &h.repo, std::path::Path::new("docs/adr/0007-rate-limiting.md"), Some("fake-plan".into()), "test").unwrap();
    wait_epic(&mut h, "E1", EpicStatus::Proposed);
    let e = epic::accept(&h.ctx, "E1", AcceptOptions { only: Some(vec!["T1".into(), "T2".into()]), ..accept_opts("fake-fail", "fake-acceptance-met") }).unwrap();
    let (t1, t2) = (e.tasks["T1"].clone(), e.tasks["T2"].clone());
    assert_eq!(h.settle(&t1)[0].status, RunStatus::Failed);
    h.until(Duration::from_secs(5), |h| task_status(h, &t2) == TaskStatus::Blocked);
    let t = h.ctx.store.load_task(&t2).unwrap();
    assert!(t.waiting_on.unwrap().contains("failed"));
    assert!(t.run_ids.is_empty(), "nothing cascades: the dependent never started");
    epic::unblock(&h.ctx, &t2, Some("tester".into())).unwrap();
    h.until(Duration::from_secs(10), |h| !h.ctx.store.load_task(&t2).unwrap().run_ids.is_empty());
    let r = &h.settle(&t2)[0];
    assert_eq!(r.status, RunStatus::Failed, "still the failing fake runner");
}

#[test]
fn invalid_plan_goes_back_to_the_planner_then_fails_or_succeeds() {
    let mut h = setup("stacked");
    epic::create_epic(&h.ctx, &h.repo, std::path::Path::new("docs/adr/0007-rate-limiting.md"), Some("fake-plan-fix".into()), "test").unwrap();
    let e = wait_epic(&mut h, "E1", EpicStatus::Proposed);
    let r = &h.runs_of(&e.planning_tasks[0])[0];
    let plans: Vec<_> = r.steps.iter().filter(|s| s.step_id == "plan").collect();
    assert_eq!(plans.len(), 2, "the invalid plan was sent back once");
    assert!(plans[1].feedback_from.is_some());
    assert!(h.events(&r.run_id).contains(&"plan_invalid".into()));

    epic::create_epic(&h.ctx, &h.repo, std::path::Path::new("docs/adr/0007-rate-limiting.md"), Some("fake-plan-invalid".into()), "test").unwrap();
    let e = wait_epic(&mut h, "E2", EpicStatus::PlanFailed);
    assert!(e.status_reason.unwrap().contains("plan"), "reason names the plan step");

    // A planner that writes files is stopped, whatever it proposes.
    epic::create_epic(&h.ctx, &h.repo, std::path::Path::new("docs/adr/0007-rate-limiting.md"), Some("fake-plan-writes".into()), "test").unwrap();
    let e = wait_epic(&mut h, "E3", EpicStatus::PlanFailed);
    assert!(e.status_reason.unwrap().contains("must not change files"));
}

#[test]
fn unmet_acceptance_goes_back_to_the_implementer() {
    let mut h = setup("stacked");
    epic::create_epic(&h.ctx, &h.repo, std::path::Path::new("docs/adr/0007-rate-limiting.md"), Some("fake-plan".into()), "test").unwrap();
    wait_epic(&mut h, "E1", EpicStatus::Proposed);
    let e = epic::accept(&h.ctx, "E1", AcceptOptions { only: Some(vec!["T3".into()]), ..accept_opts("fake-success", "fake-acceptance-fix") }).unwrap();
    let r = &h.settle(&e.tasks["T3"])[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let implements: Vec<_> = r.steps.iter().filter(|s| s.step_id == "implement").collect();
    assert_eq!(implements.len(), 2);
    assert!(implements[1].feedback_from.is_some(), "unmet criteria were fed back");

    let e = epic::accept(&h.ctx, "E1", AcceptOptions { only: Some(vec!["T1".into()]), ..accept_opts("fake-success", "fake-acceptance-unmet") }).unwrap();
    let r = &h.settle(&e.tasks["T1"])[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert!(r.status_reason.as_deref().unwrap().contains("not met"), "{:?}", r.status_reason);
}

#[test]
fn reject_decline_and_edit_plans() {
    let mut h = setup("stacked");
    epic::create_epic(&h.ctx, &h.repo, std::path::Path::new("docs/adr/0007-rate-limiting.md"), Some("fake-plan".into()), "test").unwrap();
    wait_epic(&mut h, "E1", EpicStatus::Proposed);
    let e = epic::reject(&h.ctx, "E1", Some(vec!["T3".into()]), Some("not now".into()), None).unwrap();
    assert_eq!(e.open_keys(), vec!["T1", "T2"]);
    // An edited plan must still be valid.
    assert!(epic::set_plan(&h.ctx, "E1", "tasks:\n  - key: A\n    title: a\n    acceptance: [x]\n    depends_on: [B]\n", None).is_err());
    let e = epic::set_plan(&h.ctx, "E1", "tasks:\n  - key: A\n    title: only this\n    acceptance: [it works]\n", None).unwrap();
    assert_eq!(e.plan.unwrap().tasks.len(), 1);
    let e = epic::reject(&h.ctx, "E1", None, None, None).unwrap();
    assert_eq!(e.status, EpicStatus::Rejected);
    assert!(epic::accept(&h.ctx, "E1", accept_opts("fake-success", "fake-acceptance-met")).is_err());
    // ADR drift is visible.
    assert_eq!(epic::adr_drifted(&e), Some(false));
    std::fs::write(h.repo.join("docs/adr/0007-rate-limiting.md"), format!("{ADR}\nAmended.\n")).unwrap();
    assert_eq!(epic::adr_drifted(&e), Some(true));
}

#[test]
fn epic_tasks_become_github_issues_with_status_comments() {
    let mut h = setup("stacked");
    let gh = fake_gh(h.dir.path());
    h.rebuild(|c| c.gh = herdr_orchestrator::github::Gh { bin: gh.clone() });
    h.project_file("config.yaml", "epic:\n  task_workflow: acc-only\n  dependency_mode: stacked\ngithub:\n  tracker:\n    project: o/3\n");
    epic::create_epic(&h.ctx, &h.repo, std::path::Path::new("docs/adr/0007-rate-limiting.md"), Some("fake-plan".into()), "test").unwrap();
    wait_epic(&mut h, "E1", EpicStatus::Proposed);
    let e = epic::accept(&h.ctx, "E1", AcceptOptions { only: Some(vec!["T3".into()]), ..accept_opts("fake-success", "fake-acceptance-met") }).unwrap();
    let r = herdr_orchestrator::engine::tracker::sync(&h.ctx, &h.repo).unwrap();
    assert_eq!(r.issues_created, vec!["https://github.com/o/r/issues/11"]);
    assert!(r.errors.iter().any(|e| e.contains("gh auth refresh -s project")), "{:?}", r.errors);
    let tid = e.tasks["T3"].clone();
    assert_eq!(h.ctx.store.load_task(&tid).unwrap().issue.unwrap().number, 11);
    let log = std::fs::read_to_string(h.dir.path().join("gh.log")).unwrap();
    assert!(log.contains("--milestone E1: 7. Rate limiting") && log.contains("- [ ] part T3 works"), "{log}");
    h.settle(&tid);
    let r = herdr_orchestrator::engine::tracker::sync(&h.ctx, &h.repo).unwrap();
    assert_eq!(r.comments, 1);
    assert!(std::fs::read_to_string(h.dir.path().join("gh.log")).unwrap().contains("issue comment 11 --body ✅ Done"));
    // Posted once.
    assert_eq!(herdr_orchestrator::engine::tracker::sync(&h.ctx, &h.repo).unwrap().comments, 0);
    // Import of agent-ready issues skips linked ones.
    std::fs::write(h.dir.path().join("gh-issues.json"), r#"[{"number":11,"title":"linked","body":"","url":"https://github.com/o/r/issues/11"},{"number":42,"title":"Fix the thing","body":"details","url":"https://github.com/o/r/issues/42"}]"#).unwrap();
    let todo = herdr_orchestrator::engine::tracker::importable(&h.ctx, &h.repo, "agent-ready").unwrap();
    assert_eq!(todo.iter().map(|i| i.number).collect::<Vec<_>>(), vec![42]);
    let t = herdr_orchestrator::engine::tracker::import(&h.ctx, &h.repo, &todo, TaskOptions { workflow: Some("quick-task".into()), runner: Some("fake-success".into()), ..Default::default() }).unwrap();
    assert_eq!(t[0].issue.as_ref().unwrap().number, 42);
    assert!(herdr_orchestrator::engine::tracker::importable(&h.ctx, &h.repo, "agent-ready").unwrap().is_empty());
}
