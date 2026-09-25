//! Engine integration tests with real git, fake runners and a mock Herdr.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use herdr_orchestrator::herdr::mock::{MockHerdr, MockReaction};
use herdr_orchestrator::herdr::{AgentStatus, HerdrApi};
use herdr_orchestrator::model::*;

const RETRY_WF: &str = r#"  - id: implement
    type: agent
    prompt: "{{task}} {{feedback}}"
  - id: tests
    type: check
    command: ["test", "-f", "FIXED"]
    on_failure:
      retry_step: implement
      max_attempts: 2
"#;

#[test]
fn successful_run_commits_and_audits() {
    let mut h = Harness::new();
    let t = h.task("Add a feature", "quick-task", Some("fake-success"));
    let runs = h.settle(&t.task_id);
    let r = &runs[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let wt = r.git.worktree_path.clone().unwrap();
    assert!(wt.starts_with(h.repo.join(".herdr-orchestrator/worktrees")));
    assert_eq!(r.git.branch.as_deref(), Some(format!("herdr/{}-add-a-feature", t.task_id).as_str()));
    assert!(wt.join("fake/implement.txt").exists());
    assert_eq!(r.git.commits.len(), 1, "agent changes are committed");
    assert_eq!(r.diff_stat.as_ref().unwrap().files_changed, 1);
    // The handoff output never lands in the commit.
    let out = std::process::Command::new("git").args(["show", "--stat", "HEAD"]).current_dir(&wt).output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains(".herdr-orchestrator"));
    let ev = h.events(&r.run_id);
    for e in ["run_started", "worktree_created", "step_started", "agent_started", "agent_prompt_sent", "files_changed", "git_commit_created", "step_completed", "run_completed"] {
        assert!(ev.contains(&e.to_string()), "missing {e} in {ev:?}");
    }
    h.assert_audit_ok(&r.run_id);
    assert_eq!(h.ctx.store.load_task(&t.task_id).unwrap().status, TaskStatus::Succeeded);
    assert_eq!(r.usage_total().source, UsageSource::Reported);
    // Main repo stays clean (worktree dir is excluded).
    let st = std::process::Command::new("git").args(["status", "--porcelain"]).current_dir(&h.repo).output().unwrap();
    assert!(String::from_utf8_lossy(&st.stdout).trim().is_empty());
}

#[test]
fn failed_check_retries_with_feedback_then_succeeds() {
    let mut h = Harness::new();
    h.workflow("retry", RETRY_WF);
    let t = h.task("Make tests pass", "retry", Some("fake-fix-on-retry"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let implements: Vec<_> = r.steps.iter().filter(|e| e.step_id == "implement").collect();
    assert_eq!(implements.len(), 2);
    assert!(implements[1].feedback_from.is_some(), "second attempt received feedback");
    assert_eq!(r.retry_counts.get("tests"), Some(&1));
    assert!(r.pending_feedback.is_none());
    let ev = h.events(&r.run_id);
    assert!(ev.contains(&"retry_started".into()));
    assert!(ev.contains(&"check_failed".into()) && ev.contains(&"check_passed".into()));
}

#[test]
fn retries_are_bounded() {
    let mut h = Harness::new();
    h.workflow("retry", RETRY_WF);
    let t = h.task("Never fixed", "retry", Some("fake-success"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert!(r.status_reason.as_deref().unwrap().contains("after 2 retries"), "{:?}", r.status_reason);
    assert_eq!(r.steps.iter().filter(|e| e.step_id == "implement").count(), 3);
    assert!(h.events(&r.run_id).contains(&"retries_exhausted".into()));
    assert_eq!(h.ctx.store.load_task(&t.task_id).unwrap().status, TaskStatus::Failed);
}

#[test]
fn global_max_retries_caps_workflow() {
    let mut h = Harness::new();
    h.project_file("config.yaml", "limits:\n  max_retries: 1\n");
    h.workflow("retry", RETRY_WF);
    let t = h.task("Never fixed", "retry", Some("fake-success"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert_eq!(r.steps.iter().filter(|e| e.step_id == "implement").count(), 2);
}

const APPROVAL_WF: &str = r#"  - id: implement
    type: agent
    prompt: "{{task}}"
  - id: approval
    type: approval
    reason: "Ship {{task_title}}?"
  - id: after
    type: command
    command: ["true"]
"#;

#[test]
fn approval_approve() {
    let mut h = Harness::new();
    h.workflow("appr", APPROVAL_WF);
    let t = h.task("Needs a human", "appr", Some("fake-success"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let tid = t.task_id.clone();
    assert!(h.until(Duration::from_secs(10), |h| h.ctx.store.load_task(&tid).unwrap().status == TaskStatus::AwaitingApproval));
    let a = h.pending_approval(&r.run_id).unwrap();
    assert_eq!(a.reason, "Ship Needs a human?");
    assert_eq!(a.context.changed_files.len(), 1, "approval shows the changed files");
    assert!(a.context.pending_action.as_deref().unwrap().contains("after"));
    h.decide(&r.run_id, true);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded);
    let ev = h.events(&r.run_id);
    assert!(ev.contains(&"approval_requested".into()) && ev.contains(&"approval_granted".into()));
    let granted = h.ctx.audit.read(Some(&r.run_id)).unwrap().into_iter().find(|e| e.event == "approval_granted").unwrap();
    assert_eq!(granted.actor.kind, "human");
    assert_eq!(granted.actor.user.as_deref(), Some("tester"));
}

#[test]
fn approval_deny_fails_run() {
    let mut h = Harness::new();
    h.workflow("appr", APPROVAL_WF);
    let t = h.task("Denied", "appr", Some("fake-success"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    h.decide(&r.run_id, false);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert!(r.status_reason.as_deref().unwrap().contains("denied"));
    assert!(r.latest_exec("after").is_none(), "nothing after a denied approval runs");
    assert!(h.events(&r.run_id).contains(&"approval_denied".into()));
}

#[test]
fn policy_deny_blocks_run_on_secret_write() {
    let mut h = Harness::new();
    let t = h.task("Touch secrets", "quick-task", Some("fake-touch-secret"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Blocked, "{:?}", r.status_reason);
    assert!(r.status_reason.as_deref().unwrap().contains(".env"));
    assert!(r.git.commits.is_empty(), "denied changes are never committed");
    assert!(h.events(&r.run_id).contains(&"policy_violation".into()));
    assert_eq!(h.ctx.store.load_task(&t.task_id).unwrap().status, TaskStatus::Blocked);
}

#[test]
fn policy_ask_requires_approval_for_migrations() {
    let mut h = Harness::new();
    let t = h.task("Add migration", "quick-task", Some("fake-touch-migration"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let a = h.pending_approval(&r.run_id).unwrap();
    assert!(a.reason.contains("db/migrations/001_init.sql"), "{}", a.reason);
    assert!(!a.context.policy.is_empty());
    h.decide(&r.run_id, true);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded);
    assert!(r.approved_paths.contains_key("db/migrations/001_init.sql"));
    assert_eq!(r.git.commits.len(), 1);
}

#[test]
fn approved_file_is_not_reasked_after_commit() {
    // Real-run finding: approval was re-requested once the approved
    // untracked file became "added" after the commit.
    let mut h = Harness::new();
    h.workflow("two", "  - id: implement\n    type: agent\n    runner: fake-touch-migration\n    prompt: x\n  - id: review\n    type: agent\n    runner: fake-noop\n    prompt: y\n");
    let t = h.task("Migration then review", "two", None);
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    h.decide(&r.run_id, true);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    assert_eq!(r.approvals.len(), 1, "same content, one approval");
}

#[test]
fn command_policy_deny_fails_safely_without_retry() {
    let mut h = Harness::new();
    h.workflow(
        "bad",
        r#"  - id: implement
    type: agent
    prompt: x
  - id: push
    type: command
    command: ["git", "push", "--force", "origin", "main"]
    on_failure:
      retry_step: implement
      max_attempts: 3
"#,
    );
    let t = h.task("Force it", "bad", Some("fake-success"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Blocked);
    assert!(r.status_reason.as_deref().unwrap().contains("deny-force-push"), "{:?}", r.status_reason);
    assert_eq!(r.steps.iter().filter(|e| e.step_id == "implement").count(), 1, "deny does not trigger retries");
    let pe = h.ctx.audit.read(Some(&r.run_id)).unwrap().into_iter().filter(|e| e.event == "policy_evaluated").any(|e| e.data["decision"] == "deny");
    assert!(pe);
}

#[test]
fn runner_crash_fails_step() {
    let mut h = Harness::new();
    let t = h.task("Crash", "quick-task", Some("fake-crash"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert!(r.status_reason.as_deref().unwrap().contains("agent lost"));
}

#[test]
fn agent_timeout_fails_without_implicit_retry() {
    let mut h = Harness::new();
    h.workflow("slow", "  - id: implement\n    type: agent\n    prompt: x\n    timeout: 1s\n");
    let t = h.task("Slow", "slow", Some("fake-timeout"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert!(r.status_reason.as_deref().unwrap().contains("timed out"));
    assert_eq!(r.steps.len(), 1);
}

#[test]
fn cancel_stops_a_running_agent() {
    let mut h = Harness::new();
    let t = h.task("Long", "quick-task", Some("fake-timeout"));
    let r = h.wait_status(&t.task_id, RunStatus::Running);
    let id = r.run_id.clone();
    assert!(h.until(Duration::from_secs(10), |h| h.ctx.store.load_run(&id).unwrap().latest_exec("implement").is_some_and(|e| e.status == StepStatus::Running)));
    herdr_orchestrator::engine::request_cancel(&h.ctx, &r.run_id, "user asked", Some("tester".into())).unwrap();
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Cancelled);
    assert_eq!(r.status_reason.as_deref(), Some("user asked"));
    assert_eq!(r.steps[0].status, StepStatus::Cancelled);
    assert_eq!(h.ctx.store.load_task(&t.task_id).unwrap().status, TaskStatus::Cancelled);
}

#[test]
fn cancel_queued_run_before_start_and_pause_queue() {
    let mut h = Harness::new();
    h.ctx.store.save_scheduler(&herdr_orchestrator::store::SchedulerState { paused: true }).unwrap();
    let t = h.task("Queued", "quick-task", Some("fake-success"));
    h.sched.tick().unwrap();
    assert_eq!(h.ctx.store.load_task(&t.task_id).unwrap().status, TaskStatus::Queued, "paused queue claims nothing");
    h.ctx.store.save_scheduler(&herdr_orchestrator::store::SchedulerState { paused: false }).unwrap();
    // Claim but don't start: saturate capacity.
    h.sched.max_parallel_runs = 1;
    let blocker = h.task("Blocker", "quick-task", Some("fake-timeout"));
    let _ = blocker;
    h.sched.tick().unwrap();
    let runs = h.runs_of(&t.task_id);
    herdr_orchestrator::engine::request_cancel(&h.ctx, &runs[0].run_id, "not needed", None).unwrap();
    h.sched.tick().unwrap();
    let r = h.ctx.store.load_run(&runs[0].run_id).unwrap();
    // It either never started or was cancelled while running; both end cancelled.
    let id = r.run_id.clone();
    assert!(h.until(Duration::from_secs(20), |h| h.ctx.store.load_run(&id).unwrap().status == RunStatus::Cancelled));
    for r in h.ctx.store.list_runs().unwrap() {
        if !r.status.is_terminal() {
            herdr_orchestrator::engine::request_cancel(&h.ctx, &r.run_id, "cleanup", None).unwrap();
        }
    }
    assert!(h.until(Duration::from_secs(20), |h| h.sched.active.is_empty()));
}

#[test]
fn parallel_runs_respect_limit() {
    let mut h = Harness::new();
    h.sched.max_parallel_runs = 2;
    h.workflow("sleepy", "  - id: implement\n    type: agent\n    prompt: x\n  - id: wait\n    type: command\n    command: [\"sleep\", \"0.3\"]\n");
    let tasks: Vec<_> = (0..4).map(|i| h.task(&format!("Task {i}"), "sleepy", Some("fake-success"))).collect();
    let mut max_seen = 0;
    let ok = h.until(Duration::from_secs(60), |h| {
        max_seen = max_seen.max(h.sched.active.len());
        tasks.iter().all(|t| h.ctx.store.load_task(&t.task_id).unwrap().status == TaskStatus::Succeeded)
    });
    assert!(ok, "{:#?}", h.ctx.store.list_runs().unwrap().iter().map(|r| (r.display_name(), r.status, r.status_reason.clone())).collect::<Vec<_>>());
    assert!(max_seen <= 2, "saw {max_seen} concurrent runs");
    assert!(max_seen >= 1);
    let branches: std::collections::BTreeSet<_> = h.ctx.store.list_runs().unwrap().into_iter().map(|r| r.git.branch.unwrap()).collect();
    assert_eq!(branches.len(), 4);
}

#[test]
fn variants_get_separate_worktrees_and_explicit_selection() {
    let mut h = Harness::new();
    let t = h.task_with(
        "Try three ways",
        TaskOptions {
            workflow: Some("quick-task".into()),
            variants: 3,
            variant_runners: vec!["fake-success".into(), "fake-noop".into(), "fake-success".into()],
            ..Default::default()
        },
    );
    let runs = h.settle(&t.task_id);
    assert_eq!(runs.len(), 3);
    let names: Vec<_> = runs.iter().map(|r| r.display_name()).collect();
    assert_eq!(names, vec![format!("#{}A", t.task_id), format!("#{}B", t.task_id), format!("#{}C", t.task_id)]);
    let wts: std::collections::BTreeSet<_> = runs.iter().map(|r| r.git.worktree_path.clone().unwrap()).collect();
    assert_eq!(wts.len(), 3);
    assert!(runs.iter().all(|r| r.status == RunStatus::Succeeded));
    assert_eq!(runs[1].diff_stat.as_ref().unwrap().files_changed, 0, "noop variant changed nothing");
    assert_eq!(runs[1].steps[0].runner.as_deref(), Some("fake-noop"));
    // No automatic winner.
    assert!(h.ctx.store.load_task(&t.task_id).unwrap().selected_run.is_none());
}

#[test]
fn review_gate_routes_findings_back_to_implementer() {
    let mut h = Harness::new();
    h.workflow(
        "gated",
        r#"  - id: implement
    type: agent
    runner: fake-success
    prompt: "{{task}} {{feedback}}"
  - id: review
    type: agent
    runner: fake-review-fix
    output: review
    gate: true
    prompt: review
    on_failure:
      retry_step: implement
      max_attempts: 2
"#,
    );
    let t = h.task("Gated", "gated", None);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let reviews: Vec<_> = r.steps.iter().filter(|e| e.step_id == "review").collect();
    assert_eq!(reviews.len(), 2);
    assert_eq!(reviews[0].status, StepStatus::Failed);
    assert_eq!(reviews[0].structured.as_ref().unwrap()["verdict"], "changes_requested");
    assert_eq!(reviews[1].structured.as_ref().unwrap()["verdict"], "approved");
    assert_eq!(r.steps.iter().filter(|e| e.step_id == "implement").count(), 2);
}

#[test]
fn invalid_review_output_is_kept_and_flagged() {
    let mut h = Harness::new();
    h.workflow("rev", "  - id: review\n    type: agent\n    output: review\n    prompt: r\n");
    let t = h.task("Review", "rev", Some("fake-review-invalid"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded);
    let e = &r.steps[0];
    assert!(e.parse_failed);
    assert!(e.structured.is_none());
    assert!(e.output_excerpt.as_deref().unwrap().contains("not json"));
}

#[test]
fn skipped_named_check_when_nothing_detected() {
    let mut h = Harness::new();
    h.workflow("chk", "  - id: tests\n    type: check\n    check: tests\n");
    let t = h.task("No tests", "chk", Some("fake-success"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded);
    assert_eq!(r.steps[0].status, StepStatus::Skipped);
    // Configured check wins over detection.
    h.project_file("config.yaml", "checks:\n  tests: [\"false\"]\n");
    let t = h.task("Configured tests", "chk", Some("fake-success"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
}

#[test]
fn shell_commands_need_approval_and_use_env_not_templates() {
    let mut h = Harness::new();
    h.workflow("sh", "  - id: s\n    type: command\n    shell: true\n    command: [\"test -n \\\"$HERDR_ORCH_WORKTREE\\\" && test \\\"$HERDR_ORCH_STEP_ID\\\" = s\"]\n");
    let t = h.task("Shell", "sh", Some("fake-success"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    assert!(h.pending_approval(&r.run_id).unwrap().reason.contains("shell"));
    h.decide(&r.run_id, true);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.steps);
}

#[test]
fn human_retry_resumes_from_failed_step() {
    let mut h = Harness::new();
    h.workflow("retry", RETRY_WF.replace("max_attempts: 2", "max_attempts: 1").as_str());
    let t = h.task("Retry by hand", "retry", Some("fake-success"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    // The human fixes the problem in the worktree, then retries the check.
    std::fs::write(r.git.worktree_path.as_ref().unwrap().join("FIXED"), "by hand").unwrap();
    herdr_orchestrator::engine::request_retry(&h.ctx, &r.run_id, Some("tests".into()), Some("tester".into())).unwrap();
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    assert_eq!(h.ctx.store.load_task(&t.task_id).unwrap().status, TaskStatus::Succeeded);
}

#[test]
fn pr_flow_with_fake_gh_and_single_approval() {
    let mut h = Harness::new();
    // Local bare remote so `git push` works offline.
    let remote = h.dir.path().join("remote.git");
    assert!(std::process::Command::new("git").args(["init", "-q", "--bare"]).arg(&remote).status().unwrap().success());
    assert!(std::process::Command::new("git").args(["remote", "add", "origin"]).arg(&remote).current_dir(&h.repo).status().unwrap().success());
    let gh = fake_gh(h.dir.path());
    h.rebuild(|c| c.gh = herdr_orchestrator::github::Gh { bin: gh.clone() });
    h.workflow(
        "pr",
        "  - id: implement\n    type: agent\n    prompt: x\n  - id: approval\n    type: approval\n    reason: ok?\n  - id: pr\n    type: github_pr\n",
    );
    let t = h.task("Open a PR", "pr", Some("fake-success"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let a = h.pending_approval(&r.run_id).unwrap();
    assert!(a.context.pending_action.as_deref().unwrap().contains("draft PR"));
    h.decide(&r.run_id, true);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    assert_eq!(r.pr_url.as_deref(), Some("https://github.com/o/r/pull/7"));
    assert_eq!(r.approvals.len(), 1, "the explicit approval covered push + PR");
    let log = std::fs::read_to_string(h.dir.path().join("gh.log")).unwrap();
    assert!(log.contains("--draft"));
    // The branch really was pushed (no force).
    let out = std::process::Command::new("git").args(["branch", "--list"]).current_dir(&remote).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains(r.git.branch.as_deref().unwrap()));
    let ev = h.events(&r.run_id);
    assert!(ev.contains(&"approval_reused".into()) && ev.contains(&"pr_created".into()) && ev.contains(&"git_pushed".into()));
}

#[test]
fn dry_run_plan_reports_policy_without_side_effects() {
    let h = Harness::new();
    let lines = herdr_orchestrator::engine::plan::plan(&h.ctx, &h.repo, "Plan me", Some("implement-review"), None, None, 1).unwrap();
    let text = serde_json::to_string(&lines).unwrap();
    assert!(text.contains("create 1 worktree"));
    assert!(text.contains("codex") && text.contains("claude"));
    assert!(text.contains("REQUIRE_APPROVAL"), "{text}");
    assert!(text.contains("covered by the preceding approval step"));
    assert!(!h.repo.join(".herdr-orchestrator").exists(), "planning creates nothing");
}

// -------------------------------------------------------------- Herdr panes

fn output_path(prompt: &str) -> std::path::PathBuf {
    let line = prompt.lines().find(|l| l.starts_with("Output file: ")).expect("prompt names the output file");
    std::path::PathBuf::from(line.trim_start_matches("Output file: ").trim())
}

fn pane_behavior(scenario: &'static str) -> Arc<herdr_orchestrator::herdr::mock::Behavior> {
    Arc::new(move |c: &herdr_orchestrator::herdr::mock::PromptCall| {
        herdr_orchestrator::runners::fake::perform(scenario, &c.cwd, &output_path(&c.prompt), "implement", c.prompt_index as u32 + 1).unwrap();
        MockReaction::Finish
    })
}

#[test]
fn pane_runner_uses_herdr_panes() {
    let mock = Arc::new(MockHerdr::new(pane_behavior("success")));
    // Real Herdr reports agent_pane_busy until the new pane's shell is ready.
    mock.set_busy_starts(3);
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    // Keep panes so this test can inspect them (the default closes them).
    h.project_file("config.yaml", "herdr:\n  close_panes_on_success: false\n");
    let t = h.task("Pane work", "quick-task", Some("claude"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let b = r.steps[0].agent.clone().unwrap();
    assert_eq!(b.mode, "pane");
    assert!(herdr_orchestrator::herdr::valid_agent_name(b.agent_name.as_deref().unwrap()));
    let panes = mock.panes();
    let p = panes.iter().find(|p| Some(&p.pane_id) == b.pane_id.as_ref()).unwrap();
    assert_eq!(p.label.as_deref(), Some(format!("#{} implement · claude", t.task_id).as_str()));
    assert_eq!(p.cwd, r.git.worktree_path.clone().unwrap());
    let a = p.agent.as_ref().unwrap();
    assert_eq!(a.kind, "claude");
    // Claude starts with the policy hook registered through --settings.
    let wt = r.git.worktree_path.clone().unwrap();
    let settings = wt.join(".herdr-orchestrator/claude-settings.json");
    assert_eq!(a.args, vec!["--permission-mode".to_string(), "acceptEdits".into(), "--settings".into(), settings.display().to_string()]);
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let cmd = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.contains("hook claude-pretool --run") && cmd.contains(&r.run_id), "{cmd}");
    // The settings file is never part of the change.
    assert!(!r.diff_stat.as_ref().unwrap().files.iter().any(|f| f.path.contains("claude-settings")));
    assert!(a.prompts[0].contains("You are an implementation agent"), "skill included");
    assert!(r.herdr.workspace_id.is_some());
    assert!(mock.calls().iter().any(|c| c.starts_with("worktree.open")));
    assert_eq!(r.usage_total().source, UsageSource::Unknown, "pane agents do not fake usage");
}

#[test]
fn pane_retry_reuses_live_agent_session() {
    let mock = Arc::new(MockHerdr::new(pane_behavior("fix-on-retry")));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.keep_panes();
    h.workflow("retry", RETRY_WF);
    let t = h.task("Pane retry", "retry", Some("codex"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let names: std::collections::BTreeSet<_> = r.steps.iter().filter_map(|e| e.agent.as_ref().and_then(|a| a.agent_name.clone())).collect();
    assert_eq!(names.len(), 1, "same agent for both attempts");
    let prompts = mock.prompts(names.iter().next().unwrap());
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("Feedback from the previous attempt"));
    assert!(prompts[1].contains("test -f FIXED"));
    // A reused agent already has the skill in context: follow-ups are compact.
    assert!(!prompts[1].contains("You are an implementation agent"));
}

#[test]
fn blocked_agent_waits_for_human_then_continues() {
    let mock = Arc::new(MockHerdr::new(Arc::new(|c: &herdr_orchestrator::herdr::mock::PromptCall| {
        herdr_orchestrator::runners::fake::perform("success", &c.cwd, &output_path(&c.prompt), "implement", 1).unwrap();
        MockReaction::Block
    })));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    let t = h.task("Blocks", "quick-task", Some("claude"));
    let tid = t.task_id.clone();
    assert!(h.until(Duration::from_secs(20), |h| h.runs_of(&tid).first().and_then(|r| r.latest_exec("implement").cloned()).is_some_and(|e| e.status == StepStatus::AwaitingHuman)));
    assert!(h.until(Duration::from_secs(10), |_| mock.notifications().iter().any(|(t, _)| t.contains("needs you"))));
    let name = mock.agent_names()[0].clone();
    mock.unblock(&name);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded);
    let ev = h.events(&r.run_id);
    assert!(ev.contains(&"agent_blocked".into()) && ev.contains(&"agent_unblocked".into()));
}

#[test]
fn pane_disappearing_fails_the_step() {
    let mock = Arc::new(MockHerdr::new(Arc::new(|_c: &herdr_orchestrator::herdr::mock::PromptCall| MockReaction::Hang)));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    let t = h.task("Vanish", "quick-task", Some("claude"));
    let tid = t.task_id.clone();
    assert!(h.until(Duration::from_secs(20), |h| h.runs_of(&tid).first().is_some_and(|r| h.events(&r.run_id).contains(&"agent_prompt_sent".to_string()))));
    let pane = h.runs_of(&tid)[0].latest_exec("implement").unwrap().agent.clone().unwrap().pane_id.unwrap();
    mock.kill_pane(&pane);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert!(r.status_reason.as_deref().unwrap().contains("pane was closed"), "{:?}", r.status_reason);
}

#[test]
fn agent_start_blocked_by_trust_prompt_waits_for_human() {
    let mock = Arc::new(MockHerdr::new(pane_behavior("success")));
    mock.set_block_on_start(true);
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    let t = h.task("Trust", "quick-task", Some("claude"));
    let tid = t.task_id.clone();
    assert!(h.until(Duration::from_secs(20), |_| !mock.agent_names().is_empty()));
    assert!(h.until(Duration::from_secs(20), |h| h.runs_of(&tid)[0].latest_exec("implement").is_some_and(|e| e.status == StepStatus::AwaitingHuman)));
    mock.unblock(&mock.agent_names()[0]);
    let r = &h.settle(&tid)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
}

#[test]
fn herdr_unavailable_falls_back_or_fails_clearly() {
    let mock = Arc::new(MockHerdr::finishing());
    mock.set_available(false);
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    let t = h.task("No herdr", "quick-task", Some("claude"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert!(r.status_reason.as_deref().unwrap().contains("no Herdr workspace") || r.status_reason.as_deref().unwrap().contains("not reachable"), "{:?}", r.status_reason);
}

// ----------------------------------------------------------------- recovery

#[test]
fn recovery_reattaches_to_live_agent_without_resending() {
    let mock = Arc::new(MockHerdr::new(Arc::new(|c: &herdr_orchestrator::herdr::mock::PromptCall| {
        herdr_orchestrator::runners::fake::perform("success", &c.cwd, &output_path(&c.prompt), "implement", 1).unwrap();
        MockReaction::Hang
    })));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.keep_panes();
    let t = h.task("Survive restart", "quick-task", Some("claude"));
    let tid = t.task_id.clone();
    assert!(h.until(Duration::from_secs(20), |h| h.runs_of(&tid).first().and_then(|r| r.latest_exec("implement").cloned()).is_some_and(|e| e.agent.as_ref().is_some_and(|a| a.prompt_sent) && e.status == StepStatus::Running)));
    // Snapshot the durable state as it was at "crash" time.
    let run = h.runs_of(&tid)[0].clone();
    let doc = h.ctx.store.layout.runs_dir().join(format!("{}.json", run.run_id));
    let snapshot = std::fs::read(&doc).unwrap();
    // Stop the driver thread (simulating the process dying)…
    herdr_orchestrator::engine::request_cancel(&h.ctx, &run.run_id, "crash sim", None).unwrap();
    assert!(h.until(Duration::from_secs(20), |h| h.sched.active.is_empty()));
    // …and put the world back the way a crash would have left it.
    std::fs::write(&doc, snapshot).unwrap();
    h.ctx.store.update_control(&run.run_id, |c| *c = Default::default()).unwrap();
    let name = run.latest_exec("implement").unwrap().agent.clone().unwrap().agent_name.unwrap();
    mock.set_agent_status(&name, AgentStatus::Idle); // agent finished while we were down
    let reports = herdr_orchestrator::recovery::recover_all(&h.ctx).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].classification, herdr_orchestrator::recovery::Classification::Recoverable, "{}", reports[0].reason);
    let r = &h.settle(&tid)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    assert_eq!(mock.prompts(&name).len(), 1, "the prompt was not resent");
    let ev = h.events(&r.run_id);
    assert!(ev.contains(&"recovery_started".into()) && ev.contains(&"recovery_completed".into()));
    h.assert_audit_ok(&r.run_id);
}

#[test]
fn recovery_marks_ambiguous_states_needs_human() {
    let mock = Arc::new(MockHerdr::new(Arc::new(|_c: &herdr_orchestrator::herdr::mock::PromptCall| MockReaction::Hang)));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    let t = h.task("Lost agent", "quick-task", Some("claude"));
    let tid = t.task_id.clone();
    assert!(h.until(Duration::from_secs(20), |h| h.runs_of(&tid).first().and_then(|r| r.latest_exec("implement").cloned()).is_some_and(|e| e.agent.as_ref().is_some_and(|a| a.prompt_sent))));
    let run = h.runs_of(&tid)[0].clone();
    let doc = h.ctx.store.layout.runs_dir().join(format!("{}.json", run.run_id));
    let snapshot = std::fs::read(&doc).unwrap();
    herdr_orchestrator::engine::request_cancel(&h.ctx, &run.run_id, "crash sim", None).unwrap();
    assert!(h.until(Duration::from_secs(20), |h| h.sched.active.is_empty()));
    std::fs::write(&doc, snapshot).unwrap();
    h.ctx.store.update_control(&run.run_id, |c| *c = Default::default()).unwrap();
    // The agent's pane is gone after the "restart".
    let pane = run.latest_exec("implement").unwrap().agent.clone().unwrap().pane_id.unwrap();
    mock.kill_pane(&pane);
    let reports = herdr_orchestrator::recovery::recover_all(&h.ctx).unwrap();
    assert_eq!(reports[0].classification, herdr_orchestrator::recovery::Classification::NeedsHuman);
    let r = h.ctx.store.load_run(&run.run_id).unwrap();
    assert_eq!(r.status, RunStatus::NeedsHuman);
    h.sched.tick().unwrap();
    assert!(h.sched.active.is_empty(), "needs_human runs are not restarted automatically");
}

#[test]
fn recovery_of_interrupted_command_needs_human_but_check_reruns() {
    let mut h = Harness::new();
    h.workflow("cmds", "  - id: c\n    type: command\n    command: [\"sleep\", \"30\"]\n");
    let t = h.task("Cmd", "cmds", Some("fake-success"));
    let tid = t.task_id.clone();
    assert!(h.until(Duration::from_secs(20), |h| h.runs_of(&tid).first().and_then(|r| r.latest_exec("c").cloned()).is_some_and(|e| e.status == StepStatus::Running)));
    let run = h.runs_of(&tid)[0].clone();
    let doc = h.ctx.store.layout.runs_dir().join(format!("{}.json", run.run_id));
    let snapshot = std::fs::read(&doc).unwrap();
    herdr_orchestrator::engine::request_cancel(&h.ctx, &run.run_id, "crash sim", None).unwrap();
    assert!(h.until(Duration::from_secs(20), |h| h.sched.active.is_empty()));
    std::fs::write(&doc, snapshot).unwrap();
    h.ctx.store.update_control(&run.run_id, |c| *c = Default::default()).unwrap();
    let reports = herdr_orchestrator::recovery::recover_all(&h.ctx).unwrap();
    assert_eq!(reports[0].classification, herdr_orchestrator::recovery::Classification::NeedsHuman, "{}", reports[0].reason);
}

#[test]
fn lost_first_prompt_is_recovered_by_one_reminder() {
    // Real-Herdr finding: Codex dropped the first prompt while still starting.
    let mock = Arc::new(MockHerdr::new(Arc::new(|c: &herdr_orchestrator::herdr::mock::PromptCall| {
        if c.prompt_index > 0 {
            assert!(c.prompt.contains("You have not written the result file yet"));
            herdr_orchestrator::runners::fake::perform("success", &c.cwd, &output_path(&c.prompt), "implement", 1).unwrap();
        }
        MockReaction::Finish
    })));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.keep_panes();
    let t = h.task("Deaf first", "quick-task", Some("codex"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    assert_eq!(mock.prompts(&mock.agent_names()[0]).len(), 2);
    assert!(!r.steps[0].parse_failed);
}

#[test]
fn agent_that_never_acts_fails_instead_of_passing_empty() {
    let mock = Arc::new(MockHerdr::new(Arc::new(|_c: &herdr_orchestrator::herdr::mock::PromptCall| MockReaction::Finish)));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    let t = h.task("Never acts", "quick-task", Some("codex"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert!(r.status_reason.as_deref().unwrap().contains("without writing its result file"), "{:?}", r.status_reason);
    assert!(h.events(&r.run_id).contains(&"agent_no_result".to_string()));
}

#[test]
fn early_idle_after_prompt_is_not_trusted() {
    // Real-run finding: Herdr reported Claude idle in the same second as the
    // prompt; the review file appeared 48s later and was ignored.
    let mock = Arc::new(MockHerdr::new(Arc::new(|c: &herdr_orchestrator::herdr::mock::PromptCall| {
        let out = output_path(&c.prompt);
        let cwd = c.cwd.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            herdr_orchestrator::runners::fake::perform("review-approve", &cwd, &out, "review", 1).unwrap();
        });
        MockReaction::Finish // idle immediately, before the work is done
    })));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.keep_panes();
    h.workflow("rev", "  - id: review\n    type: agent\n    output: review\n    prompt: r\n");
    let t = h.task("Slow reviewer", "rev", Some("claude"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    assert!(!r.steps[0].parse_failed, "review result must be picked up");
    assert_eq!(r.steps[0].structured.as_ref().unwrap()["verdict"], "approved");
    assert_eq!(mock.prompts(&mock.agent_names()[0]).len(), 1, "no reminder needed");
}

#[test]
fn successful_run_closes_its_agent_panes_by_default() {
    // Real-run finding: an agent left alive after its run pushed to main on
    // its own. Succeeded runs now close their panes and Herdr workspace.
    let mock = Arc::new(MockHerdr::new(pane_behavior("success")));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    let t = h.task("Close after", "quick-task", Some("claude"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    assert!(mock.agent_names().is_empty(), "no agent stays alive after success");
    let calls = mock.calls();
    assert!(calls.iter().any(|c| c.starts_with("pane.close")));
    assert!(calls.iter().any(|c| c.starts_with("workspace.close")));
    assert!(h.events(&r.run_id).contains(&"herdr_panes_closed".to_string()));
    assert!(r.git.worktree_path.as_ref().unwrap().exists(), "files stay");
}

#[test]
fn failed_run_keeps_its_agent_pane_for_inspection() {
    let mock = Arc::new(MockHerdr::new(Arc::new(|_c: &herdr_orchestrator::herdr::mock::PromptCall| MockReaction::Finish)));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    let t = h.task("Never acts", "quick-task", Some("codex"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
    assert_eq!(mock.agent_names().len(), 1, "failed runs keep the agent to look at");
}

#[test]
fn idle_flicker_mid_task_does_not_end_the_step() {
    // Real-run finding (#5): Claude went idle for moments between tool calls
    // and permission prompts; the step was committed mid-work and the agent
    // kept editing for 30 more minutes.
    let slot: Arc<std::sync::Mutex<Option<Arc<MockHerdr>>>> = Arc::new(std::sync::Mutex::new(None));
    let s2 = slot.clone();
    let mock = Arc::new(MockHerdr::new(Arc::new(move |c: &herdr_orchestrator::herdr::mock::PromptCall| {
        let m = s2.lock().unwrap().clone().unwrap();
        let (name, cwd, out) = (c.agent_name.clone(), c.cwd.clone(), output_path(&c.prompt));
        std::thread::spawn(move || {
            std::fs::write(cwd.join("part1.txt"), "early\n").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(400));
            m.set_agent_status(&name, AgentStatus::Working); // resumes after a brief idle
            std::thread::sleep(std::time::Duration::from_millis(1200));
            std::fs::write(cwd.join("part2.txt"), "late\n").unwrap();
            herdr_orchestrator::runners::fake::perform("noop", &cwd, &out, "implement", 1).unwrap();
            m.set_agent_status(&name, AgentStatus::Idle);
        });
        MockReaction::Finish // reports idle immediately, mid-task
    })));
    *slot.lock().unwrap() = Some(mock.clone());
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.keep_panes();
    let t = h.task("Flicker", "quick-task", Some("claude"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let files: Vec<_> = r.diff_stat.as_ref().unwrap().files.iter().map(|f| f.path.clone()).collect();
    assert!(files.contains(&"part2.txt".to_string()), "late work must be in the commit: {files:?}");
    assert!(!r.steps[0].parse_failed);
    assert_eq!(mock.prompts(&mock.agent_names()[0]).len(), 1, "no reminder while the agent is still working");
}

/// Run until the implement/review step has a live, prompted agent, then
/// simulate an engine crash at that moment (restore the durable snapshot).
fn crash_while_agent_works(h: &mut Harness, tid: &str, step: &str) -> Run {
    let t = tid.to_string();
    let st = step.to_string();
    assert!(h.until(Duration::from_secs(20), |h| h.runs_of(&t).first().and_then(|r| r.latest_exec(&st).cloned()).is_some_and(|e| e.agent.as_ref().is_some_and(|a| a.prompt_sent) && e.status == StepStatus::Running)));
    let run = h.runs_of(tid)[0].clone();
    let doc = h.ctx.store.layout.runs_dir().join(format!("{}.json", run.run_id));
    let snapshot = std::fs::read(&doc).unwrap();
    herdr_orchestrator::engine::request_cancel(&h.ctx, &run.run_id, "crash sim", None).unwrap();
    assert!(h.until(Duration::from_secs(20), |h| h.sched.active.is_empty()));
    std::fs::write(&doc, snapshot).unwrap();
    h.ctx.store.update_control(&run.run_id, |c| *c = Default::default()).unwrap();
    run
}

#[test]
fn resume_keeps_a_result_written_while_the_engine_was_down() {
    // Review finding P1: resuming the same execution deleted its result file.
    let slot: Arc<std::sync::Mutex<Option<std::path::PathBuf>>> = Arc::new(std::sync::Mutex::new(None));
    let s2 = slot.clone();
    let mock = Arc::new(MockHerdr::new(Arc::new(move |c: &herdr_orchestrator::herdr::mock::PromptCall| {
        *s2.lock().unwrap() = Some(output_path(&c.prompt));
        MockReaction::Hang
    })));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.keep_panes();
    h.workflow("rev", "  - id: review\n    type: agent\n    output: review\n    prompt: r\n");
    let t = h.task("Review during outage", "rev", Some("claude"));
    let run = crash_while_agent_works(&mut h, &t.task_id, "review");
    // While the engine is down the agent finishes and writes its verdict.
    let out = slot.lock().unwrap().clone().unwrap();
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, r#"{"verdict":"approved","summary":"ok","findings":[]}"#).unwrap();
    let name = run.latest_exec("review").unwrap().agent.clone().unwrap().agent_name.unwrap();
    mock.set_agent_status(&name, AgentStatus::Idle);
    herdr_orchestrator::recovery::recover_all(&h.ctx).unwrap();
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    assert_eq!(r.latest_exec("review").unwrap().structured.as_ref().unwrap()["verdict"], "approved");
}

#[test]
fn reattach_does_not_trust_a_brief_idle() {
    // Review finding P1: attach() after a restart skipped the result-file /
    // sustained-idle check and accepted a momentary idle as done.
    let slot: Arc<std::sync::Mutex<Option<std::path::PathBuf>>> = Arc::new(std::sync::Mutex::new(None));
    let s2 = slot.clone();
    let mock = Arc::new(MockHerdr::new(Arc::new(move |c: &herdr_orchestrator::herdr::mock::PromptCall| {
        *s2.lock().unwrap() = Some(output_path(&c.prompt));
        MockReaction::Hang
    })));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.keep_panes();
    let t = h.task("Reattach flicker", "quick-task", Some("claude"));
    let run = crash_while_agent_works(&mut h, &t.task_id, "implement");
    let name = run.latest_exec("implement").unwrap().agent.clone().unwrap().agent_name.unwrap();
    let wt = run.git.worktree_path.clone().unwrap();
    let out = slot.lock().unwrap().clone().unwrap();
    // After the restart the agent is momentarily idle, then keeps working.
    mock.set_agent_status(&name, AgentStatus::Idle);
    let m2 = mock.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(600));
        m2.set_agent_status(&name, AgentStatus::Working);
        std::thread::sleep(std::time::Duration::from_millis(900));
        std::fs::write(wt.join("late.txt"), "late\n").unwrap();
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(&out, r#"{"summary":"done","status":"done"}"#).unwrap();
        m2.set_agent_status(&name, AgentStatus::Idle);
    });
    herdr_orchestrator::recovery::recover_all(&h.ctx).unwrap();
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let files: Vec<_> = r.diff_stat.as_ref().unwrap().files.iter().map(|f| f.path.clone()).collect();
    assert!(files.contains(&"late.txt".to_string()), "late work must be committed: {files:?}");
}

#[test]
fn cancel_during_the_settle_check_is_a_cancel_not_a_completion() {
    // Review finding P2: confirm_settled() reported "done" on cancellation.
    // Exercised on the reattach path, which has no reminder to mask it.
    let mock = Arc::new(MockHerdr::new(Arc::new(|_c: &herdr_orchestrator::herdr::mock::PromptCall| MockReaction::Hang)));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.project_file("config.yaml", "herdr:\n  settle_window: 30s\n  close_panes_on_success: false\n");
    let t = h.task("Cancel while settling", "quick-task", Some("claude"));
    let run = crash_while_agent_works(&mut h, &t.task_id, "implement");
    let name = run.latest_exec("implement").unwrap().agent.clone().unwrap().agent_name.unwrap();
    mock.set_agent_status(&name, AgentStatus::Idle); // idle, no result file: must be double-checked
    herdr_orchestrator::recovery::recover_all(&h.ctx).unwrap();
    let id = run.run_id.clone();
    assert!(h.until(Duration::from_secs(10), |h| h.ctx.store.load_run(&id).unwrap().status == RunStatus::Running));
    std::thread::sleep(Duration::from_millis(1500)); // now inside the 30s settle check
    herdr_orchestrator::engine::request_cancel(&h.ctx, &run.run_id, "user", None).unwrap();
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Cancelled, "{:?}", r.status_reason);
}

// ------------------------------------------------------------ guardrails

#[test]
fn agent_instruction_files_need_approval() {
    let mut h = Harness::new();
    let t = h.task("Tweak instructions", "quick-task", Some("fake-touch-agent-config"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let a = h.pending_approval(&r.run_id).unwrap();
    assert!(a.reason.contains("CLAUDE.md"), "{}", a.reason);
    assert!(a.context.policy.iter().any(|p| p.matched.iter().any(|m| m.rule_id == "approve-agent-instructions-and-orchestrator-config")));
    h.decide(&r.run_id, false);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
}

#[test]
fn added_test_skip_needs_approval() {
    let mut h = Harness::new();
    let t = h.task("Speed up tests", "quick-task", Some("fake-skip-test"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let a = h.pending_approval(&r.run_id).unwrap();
    assert!(a.context.policy.iter().any(|p| p.matched.iter().any(|m| m.rule_id == "approve-disabled-tests")), "{:?}", a.context.policy);
    h.decide(&r.run_id, true);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
}

#[test]
fn retry_that_only_touches_tests_is_stopped() {
    let mut h = Harness::new();
    h.workflow("retry", RETRY_WF);
    let t = h.task("Make tests pass", "retry", Some("fake-tests-only-on-retry"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let a = h.pending_approval(&r.run_id).unwrap();
    assert!(a.reason.contains("changed only tests"), "{}", a.reason);
    assert!(a.reason.contains("tests/fake_test.rs"));
    assert!(h.events(&r.run_id).contains(&"test_only_retry".into()));
    h.decide(&r.run_id, false);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Failed);
}

#[test]
fn test_only_retry_guard_can_be_disabled() {
    let mut h = Harness::new();
    h.project_file("config.yaml", "guard:\n  test_only_retry: false\n");
    h.workflow("retry", RETRY_WF);
    let t = h.task("Make tests pass", "retry", Some("fake-tests-only-on-retry"));
    let r = &h.settle(&t.task_id)[0];
    // Never fixed, but never asked either: retries run out.
    assert_eq!(r.status, RunStatus::Failed);
    assert!(!h.events(&r.run_id).contains(&"test_only_retry".into()));
}

#[test]
fn changes_outside_the_task_scope_need_approval() {
    let mut h = Harness::new();
    let t = h.task_with("Scoped", TaskOptions { workflow: Some("quick-task".into()), runner: Some("fake-success".into()), scope: vec!["src/**".into()], ..Default::default() });
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let a = h.pending_approval(&r.run_id).unwrap();
    assert!(a.reason.contains("fake/implement.txt"), "{}", a.reason);
    assert!(a.context.policy.iter().any(|p| p.matched.iter().any(|m| m.rule_id == "task-scope")));
    h.decide(&r.run_id, true);
    assert_eq!(h.settle(&t.task_id)[0].status, RunStatus::Succeeded);
    // Inside the scope nothing is asked.
    let t2 = h.task_with("Scoped ok", TaskOptions { workflow: Some("quick-task".into()), runner: Some("fake-success".into()), scope: vec!["fake/**".into()], ..Default::default() });
    assert_eq!(h.settle(&t2.task_id)[0].status, RunStatus::Succeeded);
}

#[test]
fn token_budget_asks_before_going_on() {
    let mut h = Harness::new();
    // Fake agents report 1200 tokens per step.
    h.project_file("config.yaml", "limits:\n  max_tokens: 1000\n");
    let t = h.task("Expensive", "quick-task", Some("fake-success"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let a = h.pending_approval(&r.run_id).unwrap();
    assert!(a.reason.contains("Budget exceeded"), "{}", a.reason);
    assert!(h.events(&r.run_id).contains(&"budget_exceeded".into()));
    h.decide(&r.run_id, false);
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Blocked, "{:?}", r.status_reason);
    assert_eq!(herdr_orchestrator::telemetry::compact_tokens(&herdr_orchestrator::telemetry::run_agent_usage(r)), "1.2k");
}

#[test]
fn claude_hook_can_be_switched_off() {
    let mock = Arc::new(MockHerdr::new(pane_behavior("success")));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.project_file("config.yaml", "herdr:\n  close_panes_on_success: false\nguard:\n  claude_hook: false\n");
    let t = h.task("Pane work", "quick-task", Some("claude"));
    let r = &h.settle(&t.task_id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let a = mock.panes().into_iter().find_map(|p| p.agent).unwrap();
    assert_eq!(a.args, vec!["--permission-mode", "acceptEdits"]);
}

// ------------------------------------------------- after the handoff

fn pr_harness() -> (Harness, std::path::PathBuf) {
    let mut h = Harness::new();
    let remote = h.dir.path().join("remote.git");
    assert!(std::process::Command::new("git").args(["init", "-q", "--bare"]).arg(&remote).status().unwrap().success());
    assert!(std::process::Command::new("git").args(["remote", "add", "origin"]).arg(&remote).current_dir(&h.repo).status().unwrap().success());
    let gh = fake_gh(h.dir.path());
    h.rebuild(|c| c.gh = herdr_orchestrator::github::Gh { bin: gh.clone() });
    h.workflow("pr", "  - id: implement\n    type: agent\n    prompt: \"{{task}}\"\n  - id: approval\n    type: approval\n    reason: ok?\n  - id: pr\n    type: github_pr\n");
    (h, remote)
}

#[test]
fn pr_feedback_becomes_a_follow_up_on_the_same_branch() {
    let (mut h, remote) = pr_harness();
    let t = h.task("Open a PR", "pr", Some("fake-success"));
    h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let r0 = h.runs_of(&t.task_id)[0].clone();
    h.decide(&r0.run_id, true);
    let r = h.settle(&t.task_id)[0].clone();
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    // No feedback yet: nothing to follow up, and the watcher stays quiet.
    let err = herdr_orchestrator::engine::followup::pr_followup(&h.ctx, &r.run_id, None, None, "test").unwrap_err();
    assert!(format!("{err:#}").contains("no failing checks"), "{err:#}");
    std::fs::write(
        h.dir.path().join("gh-view.json"),
        r#"{"state":"OPEN","statusCheckRollup":[{"name":"test","conclusion":"FAILURE","detailsUrl":"https://ci/1"}],"reviews":[{"author":{"login":"bo"},"state":"CHANGES_REQUESTED","body":"handle errors"}],"comments":[]}"#,
    )
    .unwrap();
    std::fs::write(h.dir.path().join("gh-inline.json"), r#"[{"path":"src/lib.rs","line":1,"user":{"login":"ana"},"body":"please rename a"}]"#).unwrap();
    let seen = herdr_orchestrator::engine::followup::watch_prs(&h.ctx, 14).unwrap();
    assert_eq!(seen, vec![r.run_id.clone()]);
    assert!(herdr_orchestrator::engine::followup::watch_prs(&h.ctx, 14).unwrap().is_empty(), "reported once");
    assert!(h.events(&r.run_id).contains(&"pr_feedback_detected".into()));

    let ft = herdr_orchestrator::engine::followup::pr_followup(&h.ctx, &r.run_id, Some("keep the API".into()), Some("fake-skip-test".into()), "test").unwrap();
    // Only one follow-up at a time.
    assert!(herdr_orchestrator::engine::followup::pr_followup(&h.ctx, &r.run_id, None, None, "test").is_err());
    assert!(ft.description.contains("handle errors") && ft.description.contains("please rename a") && ft.description.contains("CI check `test` failure"));
    assert!(ft.description.contains("keep the API"));
    assert_eq!(ft.options.continue_run.as_deref(), Some(r.run_id.as_str()));
    // Pushing new commits to the existing PR needs approval.
    let fr = h.wait_status(&ft.task_id, RunStatus::AwaitingApproval);
    assert_eq!(fr.git.branch, r.git.branch, "same branch");
    assert_eq!(fr.git.worktree_path, r.git.worktree_path, "same worktree");
    assert!(h.pending_approval(&fr.run_id).is_some());
    // The explicit approval step covers the PR step; the push that adds
    // commits to the existing PR asks separately.
    let ok = h.until(Duration::from_secs(30), |h| {
        if let Some(p) = h.pending_approval(&fr.run_id) {
            herdr_orchestrator::approvals::decide(&h.ctx.store, &p.approval_id, true, Some("t".into()), None).unwrap();
        }
        h.ctx.store.load_run(&fr.run_id).unwrap().status.is_terminal()
    });
    assert!(ok);
    let fr = h.ctx.store.load_run(&fr.run_id).unwrap();
    assert_eq!(fr.status, RunStatus::Succeeded, "{:?}", fr.status_reason);
    assert_eq!(fr.pr_url, r.pr_url, "same PR");
    let remote_head = std::process::Command::new("git").args(["rev-parse", r.git.branch.as_deref().unwrap()]).current_dir(&remote).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&remote_head.stdout).trim(), fr.git.head_sha.clone().unwrap(), "follow-up commits pushed");
    assert!(fr.git.commits.len() == 1 && fr.git.head_sha != r.git.head_sha);
    // Without --runner a follow-up keeps the original agents (never the
    // default runner).
    h.ctx.store.update_task(&ft.task_id, |t| {
        t.status = TaskStatus::Succeeded;
        Ok(())
    })
    .unwrap();
    let again = herdr_orchestrator::engine::followup::pr_followup(&h.ctx, &r.run_id, None, None, "test").unwrap();
    assert_eq!(again.options.runner.as_deref(), Some("fake-success"));
}

#[test]
fn gc_removes_only_clean_merged_worktrees_and_stats_summarize() {
    let mut h = Harness::new();
    let t1 = h.task("Merged later", "quick-task", Some("fake-success"));
    let t2 = h.task("Not merged", "quick-task", Some("fake-success"));
    let t3 = h.task("Failed", "quick-task", Some("fake-fail"));
    let r1 = h.settle(&t1.task_id)[0].clone();
    let r2 = h.settle(&t2.task_id)[0].clone();
    h.settle(&t3.task_id);
    let git = |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(&h.repo).status().unwrap().success());
    git(&["-c", "user.email=t@e", "-c", "user.name=t", "merge", "-q", "--no-ff", "-m", "m", r1.git.branch.as_deref().unwrap()]);
    use herdr_orchestrator::engine::maintenance::*;
    let c = gc_candidates(&h.ctx, &GcOptions::default()).unwrap();
    assert_eq!(c.iter().map(|x| x.run_id.clone()).collect::<Vec<_>>(), vec![r1.run_id.clone()]);
    assert!(c[0].reason.contains("merged into main"));
    // With --failed, old failures count too (0 days = any age).
    let c2 = gc_candidates(&h.ctx, &GcOptions { include_failed: true, older_than_days: -1, ..Default::default() }).unwrap();
    assert_eq!(c2.len(), 2);
    // A dirty worktree is never a candidate.
    std::fs::write(r2.git.worktree_path.clone().unwrap().join("wip.txt"), "x").unwrap();
    for (_, res) in gc_remove(&h.ctx, &c, None) {
        res.unwrap();
    }
    assert!(!r1.git.worktree_path.clone().unwrap().exists());
    assert!(r2.git.worktree_path.clone().unwrap().exists());
    let out = std::process::Command::new("git").args(["branch", "--list", r1.git.branch.as_deref().unwrap()]).current_dir(&h.repo).output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).trim().is_empty(), "branches are kept");
    let stats = runner_stats(&h.ctx.store.list_runs().unwrap());
    let s = stats.iter().find(|s| s.runner == "fake-success").unwrap();
    assert_eq!((s.runs, s.succeeded, s.avg_tokens), (2, 2, Some(1200)));
    assert_eq!(stats.iter().find(|s| s.runner == "fake-fail").unwrap().failed, 1);
}

#[test]
fn watchdog_hands_a_stuck_agent_to_a_human() {
    let mock = Arc::new(MockHerdr::new(Arc::new(|_c: &herdr_orchestrator::herdr::mock::PromptCall| MockReaction::Hang)));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.project_file("config.yaml", "guard:\n  claude_hook: false\n  watchdog:\n    check_every: 1s\n    stall_after: 3s\n    idle_after: 2s\n");
    let t = h.task("Spin forever", "quick-task", Some("claude"));
    let tid = t.task_id.clone();
    let ok = h.until(Duration::from_secs(40), |h| h.runs_of(&tid).first().is_some_and(|r| r.steps.iter().any(|e| e.attention.is_some()) && h.events(&r.run_id).contains(&"agent_stuck".into())));
    assert!(ok, "watchdog did not fire: {:?}", h.runs_of(&tid).first().map(|r| r.steps.clone()));
    let r = h.runs_of(&tid)[0].clone();
    let e = r.steps.iter().find(|e| e.attention.is_some()).unwrap();
    assert_eq!(e.status, StepStatus::AwaitingHuman);
    assert!(e.attention.as_deref().unwrap().contains("no file changes"), "{:?}", e.attention);
    assert!(h.events(&r.run_id).contains(&"agent_stuck".into()));
    assert!(h.until(Duration::from_secs(5), |_| mock.notifications().iter().any(|(t, _)| t.contains("looks stuck"))));
    // Never failed by the watchdog: a human decides.
    assert!(!r.status.is_terminal());
    herdr_orchestrator::engine::request_cancel(&h.ctx, &r.run_id, "test", None).unwrap();
    assert_eq!(h.settle(&tid)[0].status, RunStatus::Cancelled);
}

#[test]
fn run_state_is_shown_in_the_herdr_sidebar() {
    let mock = Arc::new(MockHerdr::new(pane_behavior("success")));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.workflow("ask", "  - id: implement\n    type: agent\n    prompt: x\n  - id: ship\n    type: approval\n    reason: ok?\n");
    let t = h.task("Sidebar", "ask", Some("claude"));
    let r = h.wait_status(&t.task_id, RunStatus::AwaitingApproval);
    let ws = r.herdr.workspace_id.clone().unwrap();
    let seen = |m: &MockHerdr| m.workspace_tokens().into_iter().filter(|(w, k, _)| *w == ws && k == "orch").filter_map(|(_, _, v)| v).collect::<Vec<_>>();
    assert!(h.until(Duration::from_secs(5), |_| seen(&mock).last().map(String::as_str) == Some("⏳ approve: ship")), "{:?}", seen(&mock));
    let got = seen(&mock);
    assert!(got.iter().any(|v| v.starts_with("→ implement · claude")), "{got:?}");
    assert_eq!(got.last().map(String::as_str), Some("⏳ approve: ship"), "{got:?}");
    // Reported only on change.
    let mut dedup = got.clone();
    dedup.dedup();
    assert_eq!(dedup, got);
    h.decide(&r.run_id, true);
    h.settle(&t.task_id);
    assert!(h.until(Duration::from_secs(5), |_| seen(&mock).last().is_some_and(|v| v.starts_with("✓ done"))), "{:?}", seen(&mock));
    // Off when the token is empty.
    h.project_file("config.yaml", "herdr:\n  sidebar_token: \"\"\n");
    let before = mock.workspace_tokens().len();
    let t2 = h.task("No sidebar", "quick-task", Some("claude"));
    h.settle(&t2.task_id);
    assert_eq!(mock.workspace_tokens().len(), before);
}

#[test]
fn runner_presets_set_model_effort_and_advisor() {
    let mock = Arc::new(MockHerdr::new(pane_behavior("success")));
    let mut h = Harness::with_herdr(Some(mock.clone() as Arc<dyn HerdrApi>));
    h.project_file(
        "config.yaml",
        "herdr:\n  close_panes_on_success: false\nguard:\n  claude_hook: false\nrunners:\n  claude-deep:\n    advisor: fable\n",
    );
    let fast = h.task("Fast", "quick-task", Some("claude-fast"));
    assert_eq!(h.settle(&fast.task_id)[0].status, RunStatus::Succeeded);
    let deep = h.task("Deep", "quick-task", Some("claude-deep"));
    assert_eq!(h.settle(&deep.task_id)[0].status, RunStatus::Succeeded);
    let panes = mock.panes();
    let agent = |label: &str| panes.iter().find(|p| p.label.as_deref().is_some_and(|l| l.contains(label))).unwrap().clone();
    let f = agent("claude-fast");
    assert_eq!(f.agent.as_ref().unwrap().kind, "claude");
    assert_eq!(f.agent.as_ref().unwrap().args, vec!["--permission-mode", "acceptEdits", "--model", "sonnet", "--effort", "medium"]);
    assert_eq!(f.env.get("CLAUDE_CODE_DISABLE_ADVISOR_TOOL").map(String::as_str), Some("1"), "advisor off for the fast preset");
    let d = agent("claude-deep");
    assert!(d.agent.as_ref().unwrap().args.ends_with(&["--advisor".to_string(), "fable".to_string()]), "config overrides the preset's advisor: {:?}", d.agent);
    assert!(!d.env.contains_key("CLAUDE_CODE_DISABLE_ADVISOR_TOOL"));
}
