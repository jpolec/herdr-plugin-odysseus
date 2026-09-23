//! Append-only, hash-chained JSONL audit trail.
//!
//! `hash = sha256(canonical(event without "hash") ‖ previous_hash)`.
//! This is *tamper-evident chaining*: editing, deleting or reordering an
//! event breaks verification of every later event. It is not a signature
//! and cannot prove who wrote the log; `signature` is reserved for a future
//! signing key.

pub mod redact;

use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{now, Timestamp};
use crate::store::{canonical_json, sha256_hex, FileLock, StateLayout};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Actor {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl Actor {
    pub fn orchestrator() -> Self {
        Self { kind: "orchestrator".into(), runner: None, pane_id: None, user: None }
    }
    pub fn human(user: Option<String>) -> Self {
        Self { kind: "human".into(), runner: None, pane_id: None, user }
    }
    pub fn agent(runner: &str, pane_id: Option<String>) -> Self {
        Self { kind: "agent".into(), runner: Some(runner.into()), pane_id, user: None }
    }
    pub fn policy() -> Self {
        Self { kind: "policy".into(), runner: None, pane_id: None, user: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditEvent {
    pub event_id: String,
    pub seq: u64,
    pub timestamp: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    pub actor: Actor,
    pub event: String,
    pub data: serde_json::Value,
    pub previous_hash: String,
    pub hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl AuditEvent {
    fn compute_hash(&self) -> Result<String> {
        let mut v = serde_json::to_value(self)?;
        if let Some(o) = v.as_object_mut() {
            o.remove("hash");
        }
        let mut s = canonical_json(&v);
        s.push_str(&self.previous_hash);
        Ok(sha256_hex(s.as_bytes()))
    }
}

/// Builder for an event about to be appended.
#[derive(Debug, Clone)]
pub struct EventDraft {
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub step_id: Option<String>,
    pub actor: Actor,
    pub event: String,
    pub data: serde_json::Value,
}

impl EventDraft {
    pub fn new(event: &str, actor: Actor) -> Self {
        Self {
            run_id: None,
            task_id: None,
            step_id: None,
            actor,
            event: event.into(),
            data: serde_json::json!({}),
        }
    }
    pub fn run(mut self, run_id: &str, task_id: &str) -> Self {
        self.run_id = Some(run_id.into());
        self.task_id = Some(task_id.into());
        self
    }
    pub fn task(mut self, task_id: &str) -> Self {
        self.task_id = Some(task_id.into());
        self
    }
    pub fn step(mut self, step_id: &str) -> Self {
        self.step_id = Some(step_id.into());
        self
    }
    pub fn data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }
}

#[derive(Debug, Clone)]
pub struct AuditLog {
    layout: StateLayout,
    pub hash_chain: bool,
}

impl AuditLog {
    pub fn new(layout: StateLayout, hash_chain: bool) -> Self {
        Self { layout, hash_chain }
    }

    pub fn path_for(&self, run_id: Option<&str>) -> PathBuf {
        match run_id {
            Some(r) => self.layout.audit_dir().join(format!("{r}.jsonl")),
            None => self.layout.audit_dir().join("global.jsonl"),
        }
    }

    /// Append an event to the run's log (or the global log). Serialized with
    /// an exclusive flock so the CLI and daemon can both append safely.
    pub fn append(&self, draft: EventDraft) -> Result<AuditEvent> {
        let path = self.path_for(draft.run_id.as_deref());
        std::fs::create_dir_all(self.layout.audit_dir())?;
        let lock_path = self
            .layout
            .lock_path(&format!("audit-{}", draft.run_id.as_deref().unwrap_or("global")));
        let _g = FileLock::acquire(&lock_path)?;
        let (prev_hash, prev_seq) = last_link(&path)?;
        let mut data = draft.data;
        redact::redact_value(&mut data);
        let mut ev = AuditEvent {
            event_id: crate::model::new_id("ev"),
            seq: prev_seq.map(|s| s + 1).unwrap_or(0),
            timestamp: now(),
            run_id: draft.run_id,
            task_id: draft.task_id,
            step_id: draft.step_id,
            actor: draft.actor,
            event: draft.event,
            data,
            previous_hash: if self.hash_chain { prev_hash } else { String::new() },
            hash: String::new(),
            signature: None,
        };
        if self.hash_chain {
            ev.hash = ev.compute_hash()?;
        }
        let mut line = serde_json::to_string(&ev)?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening audit log {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(line.as_bytes())?;
        f.sync_data()?;
        Ok(ev)
    }

    pub fn read(&self, run_id: Option<&str>) -> Result<Vec<AuditEvent>> {
        read_events(&self.path_for(run_id))
    }

    pub fn verify(&self, run_id: Option<&str>) -> Result<VerifyReport> {
        verify_file(&self.path_for(run_id))
    }
}

/// Read the last line's hash and seq without loading the whole file.
fn last_link(path: &Path) -> Result<(String, Option<u64>)> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((GENESIS.into(), None)),
        Err(e) => return Err(e.into()),
    };
    let len = f.metadata()?.len();
    if len == 0 {
        return Ok((GENESIS.into(), None));
    }
    // Read a tail window large enough for one event; grow if needed.
    let mut window: u64 = 64 * 1024;
    loop {
        let start = len.saturating_sub(window);
        f.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf);
        let trimmed = text.trim_end_matches('\n');
        if let Some(idx) = trimmed.rfind('\n') {
            let last = &trimmed[idx + 1..];
            let ev: AuditEvent = serde_json::from_str(last)
                .with_context(|| format!("audit log {} has a corrupt last line", path.display()))?;
            return Ok((if ev.hash.is_empty() { GENESIS.into() } else { ev.hash }, Some(ev.seq)));
        }
        if start == 0 {
            let ev: AuditEvent = serde_json::from_str(trimmed)
                .with_context(|| format!("audit log {} has a corrupt last line", path.display()))?;
            return Ok((if ev.hash.is_empty() { GENESIS.into() } else { ev.hash }, Some(ev.seq)));
        }
        window *= 4;
    }
}

pub fn read_events(path: &Path) -> Result<Vec<AuditEvent>> {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut out = vec![];
    for (i, line) in std::io::BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(
            serde_json::from_str(&line)
                .with_context(|| format!("{}:{}: invalid audit event", path.display(), i + 1))?,
        );
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VerifyReport {
    pub path: PathBuf,
    pub events: usize,
    pub ok: bool,
    pub chained: bool,
    pub problems: Vec<String>,
}

pub fn verify_file(path: &Path) -> Result<VerifyReport> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut problems = vec![];
    let mut prev = GENESIS.to_string();
    let mut expected_seq = 0u64;
    let mut n = 0usize;
    let mut chained = true;
    for (i, line) in std::io::BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        n += 1;
        let ev: AuditEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                problems.push(format!("line {}: unparseable event: {e}", i + 1));
                continue;
            }
        };
        if ev.hash.is_empty() {
            chained = false;
            continue;
        }
        if ev.seq != expected_seq {
            problems.push(format!(
                "line {}: sequence gap (expected {expected_seq}, found {})",
                i + 1,
                ev.seq
            ));
        }
        if ev.previous_hash != prev {
            problems.push(format!(
                "line {}: previous_hash does not link to the prior event (event {})",
                i + 1,
                ev.event_id
            ));
        }
        match ev.compute_hash() {
            Ok(h) if h == ev.hash => {}
            Ok(_) => problems.push(format!(
                "line {}: hash mismatch — event {} was modified",
                i + 1,
                ev.event_id
            )),
            Err(e) => problems.push(format!("line {}: {e}", i + 1)),
        }
        prev = ev.hash.clone();
        expected_seq = ev.seq + 1;
    }
    Ok(VerifyReport {
        path: path.to_path_buf(),
        events: n,
        ok: problems.is_empty(),
        chained,
        problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> (tempfile::TempDir, AuditLog) {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::new(dir.path());
        layout.ensure().unwrap();
        (dir, AuditLog::new(layout, true))
    }

    #[test]
    fn chain_verifies() {
        let (_d, log) = log();
        for i in 0..5 {
            log.append(
                EventDraft::new("step_started", Actor::orchestrator())
                    .run("run-a", "1")
                    .data(serde_json::json!({"i": i})),
            )
            .unwrap();
        }
        let r = log.verify(Some("run-a")).unwrap();
        assert!(r.ok, "{:?}", r.problems);
        assert_eq!(r.events, 5);
        let evs = log.read(Some("run-a")).unwrap();
        assert_eq!(evs[0].previous_hash, GENESIS);
        assert_eq!(evs[4].seq, 4);
        assert_eq!(evs[1].previous_hash, evs[0].hash);
    }

    #[test]
    fn detects_modification() {
        let (_d, log) = log();
        for i in 0..3 {
            log.append(
                EventDraft::new("x", Actor::orchestrator())
                    .run("run-b", "1")
                    .data(serde_json::json!({"i": i})),
            )
            .unwrap();
        }
        let p = log.path_for(Some("run-b"));
        let s = std::fs::read_to_string(&p).unwrap().replacen("\"i\":1", "\"i\":9", 1);
        std::fs::write(&p, s).unwrap();
        let r = log.verify(Some("run-b")).unwrap();
        assert!(!r.ok);
        assert!(r.problems.iter().any(|p| p.contains("hash mismatch")));
    }

    #[test]
    fn detects_deletion() {
        let (_d, log) = log();
        for i in 0..3 {
            log.append(
                EventDraft::new("x", Actor::orchestrator())
                    .run("run-c", "1")
                    .data(serde_json::json!({"i": i})),
            )
            .unwrap();
        }
        let p = log.path_for(Some("run-c"));
        let lines: Vec<String> = std::fs::read_to_string(&p).unwrap().lines().map(String::from).collect();
        std::fs::write(&p, format!("{}\n{}\n", lines[0], lines[2])).unwrap();
        let r = log.verify(Some("run-c")).unwrap();
        assert!(!r.ok);
    }

    #[test]
    fn redacts_before_writing() {
        let (_d, log) = log();
        log.append(
            EventDraft::new("command_started", Actor::orchestrator())
                .run("run-d", "1")
                .data(serde_json::json!({"argv": ["curl", "-H", "Authorization: Bearer abcdefghijklmnopq"]})),
        )
        .unwrap();
        let raw = std::fs::read_to_string(log.path_for(Some("run-d"))).unwrap();
        assert!(!raw.contains("abcdefghijklmnopq"));
        assert!(log.verify(Some("run-d")).unwrap().ok);
    }
}
