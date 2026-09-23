//! Opt-in end-to-end test against a real, *named* Herdr session with a real
//! agent CLI. Never runs by default and never touches the default session.
//!
//! ```sh
//! herdr --session orch-e2e server &            # isolated session
//! HERDR_ORCH_E2E=1 HERDR_ORCH_E2E_SOCKET=~/.config/herdr/sessions/orch-e2e/herdr.sock \
//!   HERDR_ORCH_E2E_RUNNER=claude cargo test --test e2e_herdr -- --nocapture
//! ```
//!
//! The first agent launch in a new folder may show the agent's own trust
//! prompt; answer it in the pane (the run waits, as designed).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::Harness;
use herdr_orchestrator::herdr::{HerdrApi, SocketHerdr};
use herdr_orchestrator::model::*;

#[test]
fn real_agent_in_real_herdr_pane() {
    if std::env::var("HERDR_ORCH_E2E").as_deref() != Ok("1") {
        eprintln!("skipped (set HERDR_ORCH_E2E=1 to run)");
        return;
    }
    let sock = std::env::var("HERDR_ORCH_E2E_SOCKET").expect("HERDR_ORCH_E2E_SOCKET must point at a NAMED session socket");
    assert!(!sock.ends_with("/.config/herdr/herdr.sock"), "refusing to run against the default session");
    let runner = std::env::var("HERDR_ORCH_E2E_RUNNER").unwrap_or_else(|_| "claude".into());
    let herdr = SocketHerdr::new(&sock);
    herdr.ping().expect("named Herdr session is not running");
    let mut h = Harness::with_herdr(Some(Arc::new(herdr) as Arc<dyn HerdrApi>));
    h.project_file("config.yaml", "herdr:\n  mode: required\nlimits:\n  agent_timeout: 10m\n");
    let t = h.task("Create a file named hello.txt in the repository root containing exactly one line: hello from herdr-orchestrator. Do nothing else.", "quick-task", Some(&runner));
    let id = t.task_id.clone();
    let ok = h.until(Duration::from_secs(900), |h| h.runs_of(&id).first().is_some_and(|r| !r.status.is_active()));
    assert!(ok, "timed out");
    let r = &h.runs_of(&id)[0];
    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.status_reason);
    let hello = std::fs::read_to_string(r.git.worktree_path.as_ref().unwrap().join("hello.txt")).unwrap();
    assert!(hello.contains("hello from herdr-orchestrator"));
    h.assert_audit_ok(&r.run_id);
}
