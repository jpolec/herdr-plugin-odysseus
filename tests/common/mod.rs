//! Shared harness for integration tests: temp git repo, isolated state and
//! config dirs, a scheduler you can tick, and helpers to act as the human.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use herdr_orchestrator::config::Paths;
use herdr_orchestrator::engine::scheduler::Scheduler;
use herdr_orchestrator::engine::{create_task, EngineCtx, NewTask};
use herdr_orchestrator::herdr::HerdrApi;
use herdr_orchestrator::model::*;

pub struct Harness {
    pub dir: tempfile::TempDir,
    pub repo: PathBuf,
    pub ctx: Arc<EngineCtx>,
    pub sched: Scheduler,
}

fn git(cwd: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

pub fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("README.md"), "demo\n").unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo.canonicalize().unwrap()
}

impl Harness {
    pub fn new() -> Self {
        Self::with_herdr(None)
    }

    pub fn with_herdr(herdr: Option<Arc<dyn HerdrApi>>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let paths = Paths::for_test(dir.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let mut ctx = EngineCtx::new(paths, herdr, 8).unwrap();
        ctx.poll = Duration::from_millis(20);
        let ctx = Arc::new(ctx);
        let sched = Scheduler::new(ctx.clone(), 4);
        Self { dir, repo, ctx, sched }
    }

    /// Replace the engine context (e.g. to set a fake `gh`).
    pub fn rebuild(&mut self, f: impl FnOnce(&mut EngineCtx)) {
        let paths = self.ctx.paths.clone();
        let herdr = self.ctx.herdr.clone();
        let mut ctx = EngineCtx::new(paths, herdr, 8).unwrap();
        ctx.poll = Duration::from_millis(20);
        f(&mut ctx);
        self.ctx = Arc::new(ctx);
        self.sched = Scheduler::new(self.ctx.clone(), self.sched.max_parallel_runs);
    }

    pub fn project_file(&self, rel: &str, content: &str) {
        let p = self.repo.join(".ai/herdr-orchestrator").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub fn workflow(&self, name: &str, steps_yaml: &str) {
        self.project_file(&format!("workflows/{name}.yaml"), &format!("version: 1\nname: {name}\nsteps:\n{steps_yaml}"));
    }

    pub fn task(&self, text: &str, workflow: &str, runner: Option<&str>) -> Task {
        self.task_with(text, TaskOptions { workflow: Some(workflow.into()), runner: runner.map(String::from), ..Default::default() })
    }

    pub fn task_with(&self, text: &str, options: TaskOptions) -> Task {
        create_task(&self.ctx, NewTask { text: text.into(), title: None, repo: self.repo.clone(), options, via: "test".into(), source: None }).unwrap()
    }

    pub fn runs_of(&self, task_id: &str) -> Vec<Run> {
        let t = self.ctx.store.load_task(task_id).unwrap();
        t.run_ids.iter().map(|id| self.ctx.store.load_run(id).unwrap()).collect()
    }

    /// Tick the scheduler until `pred` holds or the timeout expires.
    pub fn until(&mut self, timeout: Duration, mut pred: impl FnMut(&Harness) -> bool) -> bool {
        let t0 = Instant::now();
        loop {
            self.sched.tick().unwrap();
            if pred(self) {
                return true;
            }
            if t0.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Run until the task has runs and all of them have stopped.
    pub fn settle(&mut self, task_id: &str) -> Vec<Run> {
        let id = task_id.to_string();
        let ok = self.until(Duration::from_secs(60), |h| {
            let t = h.ctx.store.load_task(&id).unwrap();
            !t.run_ids.is_empty() && h.runs_of(&id).iter().all(|r| !r.status.is_active())
        });
        assert!(ok, "task {task_id} did not settle: {:#?}", self.runs_of(task_id).iter().map(|r| (r.status, r.status_reason.clone())).collect::<Vec<_>>());
        // Let driver threads exit.
        self.until(Duration::from_secs(10), |h| h.sched.active.is_empty());
        self.runs_of(task_id)
    }

    pub fn wait_status(&mut self, task_id: &str, status: RunStatus) -> Run {
        let id = task_id.to_string();
        let ok = self.until(Duration::from_secs(60), |h| h.runs_of(&id).iter().any(|r| r.status == status));
        assert!(ok, "no run of task {task_id} reached {status:?}: {:#?}", self.runs_of(task_id).iter().map(|r| (r.status, r.status_reason.clone())).collect::<Vec<_>>());
        self.runs_of(task_id).into_iter().find(|r| r.status == status).unwrap()
    }

    pub fn pending_approval(&self, run_id: &str) -> Option<herdr_orchestrator::approvals::ApprovalRequest> {
        herdr_orchestrator::approvals::pending(&self.ctx.store).unwrap().into_iter().find(|a| a.run_id == run_id)
    }

    pub fn decide(&self, run_id: &str, approve: bool) -> herdr_orchestrator::approvals::ApprovalRequest {
        let a = self.pending_approval(run_id).expect("pending approval");
        herdr_orchestrator::approvals::decide(&self.ctx.store, &a.approval_id, approve, Some("tester".into()), None).unwrap()
    }

    pub fn events(&self, run_id: &str) -> Vec<String> {
        self.ctx.audit.read(Some(run_id)).unwrap().into_iter().map(|e| e.event).collect()
    }

    pub fn assert_audit_ok(&self, run_id: &str) {
        let r = self.ctx.audit.verify(Some(run_id)).unwrap();
        assert!(r.ok, "audit chain broken: {:?}", r.problems);
    }
}

/// A fake `gh` that records calls and answers like GitHub.
pub fn fake_gh(dir: &Path) -> PathBuf {
    let bin = dir.join("fake-gh");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$1 $2\" in\n  'pr list') if [ -f {state} ]; then cat {state}; else echo '[]'; fi;;\n  'pr create') echo '[{{\"number\":7,\"url\":\"https://github.com/o/r/pull/7\",\"state\":\"OPEN\",\"isDraft\":true}}]' > {state}; echo 'https://github.com/o/r/pull/7';;\n  '--version ') echo 'gh version 2.0.0';;\n  *) exit 1;;\nesac\n",
            log = dir.join("gh.log").display(),
            state = dir.join("gh-prs.json").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}
