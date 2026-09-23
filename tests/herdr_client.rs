//! The real socket client against a mock NDJSON Herdr server.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use herdr_orchestrator::herdr::{AgentStatus, HerdrApi, HerdrError, SocketHerdr};
use serde_json::{json, Value};

/// Serve canned responses keyed by method; records every request.
fn server(responder: fn(&str, &Value) -> Value) -> (tempfile::TempDir, PathBuf, Arc<Mutex<Vec<Value>>>) {
    let dir = tempfile::Builder::new().prefix("h").tempdir_in("/tmp").unwrap();
    let sock = dir.path().join("s.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let log = Arc::new(Mutex::new(vec![]));
    let l2 = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            l2.lock().unwrap().push(req.clone());
            let id = req["id"].clone();
            let body = responder(req["method"].as_str().unwrap(), &req["params"]);
            let resp = if body.get("error").is_some() { json!({"id": id, "error": body["error"]}) } else { json!({"id": id, "result": body}) };
            // An unrelated line first: the client must match ids.
            let mut w = &stream;
            w.write_all(b"{\"id\":\"other\",\"result\":{}}\n").unwrap();
            w.write_all(format!("{resp}\n").as_bytes()).unwrap();
        }
    });
    (dir, sock, log)
}

fn agent(status: &str) -> Value {
    json!({"pane_id": "w1:p2", "name": "o1-implement-abcd", "agent": "claude", "agent_status": status, "interactive_ready": true, "tab_id": "w1:t2", "workspace_id": "w1", "terminal_id": "t", "revision": 1})
}

fn respond(method: &str, params: &Value) -> Value {
    match method {
        "ping" => json!({"type": "pong", "version": "0.9.0", "protocol": 22}),
        "worktree.open" => json!({"error": {"code": "worktree_not_found", "message": "no such worktree"}}),
        "workspace.create" => json!({"type": "workspace_created", "workspace": {"workspace_id": "w9", "label": params["label"]}, "tab": {"tab_id": "w9:t1", "workspace_id": "w9"}, "root_pane": {"pane_id": "w9:p1", "workspace_id": "w9", "tab_id": "w9:t1", "agent_status": "unknown"}}),
        "tab.create" => json!({"type": "tab_created", "tab": {"tab_id": "w1:t2", "workspace_id": "w1", "label": params["label"]}, "root_pane": {"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t2", "agent_status": "unknown"}}),
        "agent.start" => json!({"type": "agent_started", "agent": agent("idle"), "argv": ["claude"]}),
        "agent.prompt" => json!({"type": "agent_prompted", "agent": agent("idle")}),
        "agent.wait" => json!({"error": {"code": "timeout", "message": "timed out"}}),
        "agent.get" => json!({"error": {"code": "agent_not_found", "message": "agent not found"}}),
        "agent.send_keys" => json!({"error": {"code": "agent_blocked", "message": "blocked"}}),
        "pane.read" => json!({"type": "pane_read", "read": {"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t2", "source": "recent-unwrapped", "format": "text", "text": "hello\n", "revision": 3, "truncated": false}}),
        _ => json!({"type": "ok"}),
    }
}

#[test]
fn socket_client_speaks_the_protocol() {
    let (_d, sock, log) = server(respond);
    let h = SocketHerdr::new(&sock);
    assert_eq!(h.ping().unwrap().protocol, 22);
    // worktree.open failure falls back to workspace.create.
    let ws = h.open_worktree_workspace(std::path::Path::new("/repo"), std::path::Path::new("/repo/wt"), "#1 x").unwrap();
    assert_eq!(ws.workspace_id, "w9");
    assert_eq!(ws.root_pane_id.as_deref(), Some("w9:p1"));
    let tab = h.create_tab("w1", std::path::Path::new("/repo/wt"), "#1 implement · claude", &Default::default()).unwrap();
    assert_eq!(tab.pane_id, "w1:p2");
    let a = h.start_agent("o1-implement-abcd", "claude", "w1:p2", &["--permission-mode".into(), "acceptEdits".into()], Duration::from_secs(1)).unwrap();
    assert_eq!(a.agent_status, AgentStatus::Idle);
    let a = h.prompt_agent("o1-implement-abcd", "do it", Some((&[AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked], Duration::from_secs(10)))).unwrap();
    assert!(a.interactive_ready);
    let e = h.wait_agent("o1-implement-abcd", &[AgentStatus::Idle], Duration::from_millis(100)).unwrap_err();
    assert!(e.is_wait_timeout());
    assert!(h.get_agent("gone").unwrap().is_none(), "not_found maps to None");
    let e = h.send_keys("x", &["esc"]).unwrap_err();
    assert!(matches!(&e, HerdrError::Api { code, .. } if code == "agent_blocked"));
    assert_eq!(h.read_pane("w1:p2", 50).unwrap(), "hello\n");

    let reqs = log.lock().unwrap().clone();
    let find = |m: &str| reqs.iter().find(|r| r["method"] == m).cloned().unwrap();
    // Request shapes match the 0.9.0 schema.
    let start = find("agent.start");
    assert_eq!(start["params"]["kind"], "claude");
    assert_eq!(start["params"]["timeout_ms"], 3001, "clamped to Herdr's (3000, 300000] range");
    let prompt = find("agent.prompt");
    assert_eq!(prompt["params"]["wait"]["until"], json!(["idle", "done", "blocked"]));
    assert_eq!(prompt["params"]["wait"]["timeout_ms"], 10000);
    let tc = find("tab.create");
    assert_eq!(tc["params"]["focus"], false, "never steal the user's focus");
    let rd = find("pane.read");
    assert_eq!(rd["params"]["source"], "recent-unwrapped");
    assert!(reqs.iter().all(|r| r["id"].as_str().unwrap().starts_with("orch-")));
}

#[test]
fn unreachable_socket_is_reported_as_unavailable() {
    let h = SocketHerdr::new("/tmp/definitely-not-a-herdr-socket.sock");
    assert!(matches!(h.ping(), Err(HerdrError::Unavailable(_))));
}
