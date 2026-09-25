//! Conflict-aware scheduling and branch updates (merge, never rebase).

mod common;

use std::time::Duration;

use common::*;
use herdr_orchestrator::model::*;

fn git(cwd: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git").args(["-c", "user.email=t@e", "-c", "user.name=t"]).args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn overlapping_scopes_wait_disjoint_ones_run() {
    let mut h = Harness::new();
    h.workflow("hold", "  - id: implement\n    type: agent\n    prompt: x\n  - id: ok\n    type: approval\n    reason: ok?\n");
    let scoped = |h: &Harness, title: &str, scope: &str| h.task_with(title, TaskOptions { workflow: Some("hold".into()), runner: Some("fake-success".into()), scope: vec![scope.into()], ..Default::default() });
    let a = scoped(&h, "first", "fake/**");
    let ra = h.wait_status(&a.task_id, RunStatus::AwaitingApproval);
    let b = scoped(&h, "overlaps", "fake/implement.txt");
    let c = scoped(&h, "elsewhere", "docs/**");
    // A scope that starts with a wildcard does not block unrelated work.
    let d = scoped(&h, "markdown anywhere", "**/*.md");
    let (bid, cid, did) = (b.task_id.clone(), c.task_id.clone(), d.task_id.clone());
    assert!(h.until(Duration::from_secs(10), |h| h.ctx.store.load_task(&bid).unwrap().waiting_on.is_some() && !h.ctx.store.load_task(&cid).unwrap().run_ids.is_empty() && !h.ctx.store.load_task(&did).unwrap().run_ids.is_empty()));
    let w = h.ctx.store.load_task(&bid).unwrap().waiting_on.unwrap();
    assert!(w.contains(&format!("#{}", a.task_id)) && w.contains("may conflict"), "{w}");
    assert!(h.ctx.store.load_task(&bid).unwrap().run_ids.is_empty());
    // When the first run finishes, the second one starts.
    h.decide(&ra.run_id, true);
    h.settle(&a.task_id);
    assert!(h.until(Duration::from_secs(10), |h| !h.ctx.store.load_task(&bid).unwrap().run_ids.is_empty()));
    assert!(h.ctx.store.load_task(&bid).unwrap().waiting_on.is_none());
}

fn with_remote(h: &mut Harness) {
    let remote = h.dir.path().join("remote.git");
    assert!(std::process::Command::new("git").args(["init", "-q", "--bare"]).arg(&remote).status().unwrap().success());
    git(&h.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    let gh = fake_gh(h.dir.path());
    h.rebuild(|c| c.gh = herdr_orchestrator::github::Gh { bin: gh.clone() });
}

fn approve_all_until_done(h: &mut Harness, tid: &str) -> Run {
    let tid = tid.to_string();
    assert!(h.until(Duration::from_secs(60), |h| {
        let r = h.runs_of(&tid)[0].clone();
        if let Some(a) = h.pending_approval(&r.run_id) {
            herdr_orchestrator::approvals::decide(&h.ctx.store, &a.approval_id, true, Some("t".into()), None).unwrap();
        }
        r.status.is_terminal()
    }));
    h.runs_of(&tid)[0].clone()
}

#[test]
fn update_merges_the_base_and_resolves_conflicts_without_rebasing() {
    let mut h = Harness::new();
    with_remote(&mut h);
    let t = h.task("feature", "quick-task", Some("fake-success"));
    let r = h.settle(&t.task_id)[0].clone();
    let before: Vec<String> = r.git.commits.clone();
    // main moves: one unrelated change, one conflicting change.
    std::fs::create_dir_all(h.repo.join("fake")).unwrap();
    std::fs::write(h.repo.join("fake/implement.txt"), "main's version\n").unwrap();
    std::fs::write(h.repo.join("NEWS.md"), "news\n").unwrap();
    git(&h.repo, &["add", "."]);
    git(&h.repo, &["commit", "-q", "-m", "main moves"]);
    let (ut, conflicts) = herdr_orchestrator::engine::update::update_branch(&h.ctx, &r.run_id, Some("main"), Some("fake-resolve-conflicts".into()), "test").unwrap();
    assert_eq!(conflicts, vec!["fake/implement.txt"]);
    assert_eq!(ut.options.workflow.as_deref(), Some("update-resolve"));
    let u = approve_all_until_done(&mut h, &ut.task_id);
    assert_eq!(u.status, RunStatus::Succeeded, "{:?}", u.status_reason);
    let wt = u.git.worktree_path.clone().unwrap();
    let text = std::fs::read_to_string(wt.join("fake/implement.txt")).unwrap();
    assert!(!text.contains("<<<<<<<") && text.contains("main's version"), "{text}");
    assert!(wt.join("NEWS.md").exists());
    // History was extended, not rewritten: the old commits are ancestors.
    for c in before {
        assert!(herdr_orchestrator::git::is_ancestor(&wt, &c, "HEAD"));
    }
    // Diffed against the new base: NEWS.md (from main) is not "this branch's change".
    assert!(!u.diff_stat.unwrap().files.iter().any(|f| f.path == "NEWS.md"));
    let log = std::fs::read_to_string(h.dir.path().join("gh.log")).unwrap();
    assert!(!log.contains("--force"));
    // Nothing left to merge.
    assert!(herdr_orchestrator::engine::update::update_branch(&h.ctx, &r.run_id, Some("main"), None, "test").is_err());
}

#[test]
fn clean_update_is_retested_and_pushed() {
    let mut h = Harness::new();
    with_remote(&mut h);
    let t = h.task("feature", "quick-task", Some("fake-success"));
    let r = h.settle(&t.task_id)[0].clone();
    std::fs::write(h.repo.join("NEWS.md"), "news\n").unwrap();
    git(&h.repo, &["add", "."]);
    git(&h.repo, &["commit", "-q", "-m", "main moves"]);
    let (ut, conflicts) = herdr_orchestrator::engine::update::update_branch(&h.ctx, &r.run_id, None, None, "test").unwrap();
    assert!(conflicts.is_empty());
    assert_eq!(ut.options.workflow.as_deref(), Some("update-verify"));
    let u = approve_all_until_done(&mut h, &ut.task_id);
    assert_eq!(u.status, RunStatus::Succeeded, "{:?}", u.status_reason);
    assert!(u.git.worktree_path.unwrap().join("NEWS.md").exists());
}
