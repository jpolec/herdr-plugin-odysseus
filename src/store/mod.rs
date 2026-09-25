//! Durable state: checksummed JSON envelopes with atomic replacement,
//! advisory file locks, corruption quarantine and schema migrations.

mod fsutil;
mod lock;
mod migrate;

pub use fsutil::{atomic_write, fsync_dir};
pub use lock::FileLock;
pub use migrate::{Migration, CURRENT_SCHEMA_VERSION};

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::approvals::ApprovalRequest;
use crate::model::{Run, Task};

/// Filesystem layout under the plugin state directory.
#[derive(Debug, Clone)]
pub struct StateLayout {
    pub root: PathBuf,
}

impl StateLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join("state/tasks")
    }
    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("state/runs")
    }
    pub fn approvals_dir(&self) -> PathBuf {
        self.root.join("state/approvals")
    }
    pub fn control_dir(&self) -> PathBuf {
        self.root.join("state/control")
    }
    pub fn epics_dir(&self) -> PathBuf {
        self.root.join("state/epics")
    }
    pub fn epic_counter_file(&self) -> PathBuf {
        self.root.join("state/epic_counter")
    }
    pub fn scheduler_file(&self) -> PathBuf {
        self.root.join("state/scheduler.json")
    }
    pub fn counter_file(&self) -> PathBuf {
        self.root.join("state/task_counter")
    }
    pub fn audit_dir(&self) -> PathBuf {
        self.root.join("audit")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }
    /// Control socket. Unix socket paths are limited (~104 bytes on macOS);
    /// long state dirs fall back to a private per-user directory in /tmp
    /// (mode 0700, owner-checked) keyed by a hash of the state dir.
    pub fn daemon_socket(&self) -> PathBuf {
        let p = self.root.join("daemon.sock");
        if p.as_os_str().len() < 100 {
            return p;
        }
        #[cfg(unix)]
        {
            let uid = unsafe { libc::getuid() };
            let dir = PathBuf::from(format!("/tmp/herdr-orch-{uid}"));
            let _ = std::fs::create_dir(&dir);
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if let Ok(md) = std::fs::symlink_metadata(&dir) {
                if md.is_dir() && md.uid() == uid {
                    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
                    let h = sha256_hex(self.root.to_string_lossy().as_bytes());
                    return dir.join(format!("{}.sock", &h[..16]));
                }
            }
        }
        p
    }
    pub fn daemon_log(&self) -> PathBuf {
        self.root.join("daemon.log")
    }
    pub fn run_logs_dir(&self, run_id: &str) -> PathBuf {
        self.logs_dir().join(run_id)
    }
    pub fn lock_path(&self, name: &str) -> PathBuf {
        self.locks_dir().join(format!("{name}.lock"))
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [
            self.tasks_dir(),
            self.runs_dir(),
            self.approvals_dir(),
            self.control_dir(),
            self.epics_dir(),
            self.audit_dir(),
            self.logs_dir(),
            self.locks_dir(),
            self.backups_dir(),
            self.cache_dir(),
        ] {
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // State may contain task text and paths; keep it private to the user.
            let _ = std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    schema_version: u32,
    kind: String,
    sha256: String,
    data: serde_json::Value,
}

/// Canonical JSON: serde_json's `Value` uses a sorted map, so re-serializing
/// a `Value` yields keys in a stable order.
pub fn canonical_json(v: &serde_json::Value) -> String {
    serde_json::to_string(v).expect("serializing a Value cannot fail")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

pub fn hex(bytes: &[u8]) -> String {
    const CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(CHARS[(b >> 4) as usize] as char);
        s.push(CHARS[(b & 0xf) as usize] as char);
    }
    s
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("document {0} is corrupt: {1}")]
    Corrupt(PathBuf, String),
    #[error("document {0} not found")]
    NotFound(String),
}

/// Write a typed document inside a checksummed envelope, atomically.
pub fn write_doc<T: Serialize>(path: &Path, kind: &str, value: &T) -> Result<()> {
    let data = serde_json::to_value(value)?;
    let env = Envelope {
        schema_version: CURRENT_SCHEMA_VERSION,
        kind: kind.to_string(),
        sha256: sha256_hex(canonical_json(&data).as_bytes()),
        data,
    };
    let bytes = serde_json::to_vec_pretty(&env)?;
    atomic_write(path, &bytes)
}

/// Read a typed document, verifying checksum and migrating old schemas.
/// Corrupt documents are quarantined and reported as errors.
pub fn read_doc<T: DeserializeOwned>(layout: &StateLayout, path: &Path, kind: &str) -> Result<T> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let env: Envelope = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(e) => {
            quarantine(path);
            return Err(StoreError::Corrupt(path.to_path_buf(), e.to_string()).into());
        }
    };
    if env.kind != kind {
        bail!(
            "document {} has kind {:?}, expected {:?}",
            path.display(),
            env.kind,
            kind
        );
    }
    let actual = sha256_hex(canonical_json(&env.data).as_bytes());
    if actual != env.sha256 {
        quarantine(path);
        return Err(StoreError::Corrupt(path.to_path_buf(), "checksum mismatch".into()).into());
    }
    let data = if env.schema_version < CURRENT_SCHEMA_VERSION {
        let backup = layout.backups_dir().join(format!(
            "{}.v{}.{}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            env.schema_version,
            chrono::Utc::now().format("%Y%m%dT%H%M%S")
        ));
        std::fs::create_dir_all(layout.backups_dir())?;
        std::fs::copy(path, &backup)
            .with_context(|| format!("backing up {} before migration", path.display()))?;
        let migrated = migrate::migrate(kind, env.schema_version, env.data)?;
        let typed: T = serde_json::from_value(migrated.clone())?;
        write_doc(path, kind, &migrated)?;
        return Ok(typed);
    } else if env.schema_version > CURRENT_SCHEMA_VERSION {
        bail!(
            "document {} has schema v{} which is newer than this binary (v{}); upgrade herdr-orchestrator",
            path.display(),
            env.schema_version,
            CURRENT_SCHEMA_VERSION
        );
    } else {
        env.data
    };
    serde_json::from_value(data).map_err(|e| {
        anyhow!(
            "document {} does not match the {kind} schema: {e}",
            path.display()
        )
    })
}

fn quarantine(path: &Path) {
    let q = path.with_extension(format!(
        "corrupt-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S")
    ));
    if let Err(e) = std::fs::rename(path, &q) {
        tracing::error!("failed to quarantine corrupt file {}: {e}", path.display());
    } else {
        tracing::error!(
            "quarantined corrupt state file {} -> {}",
            path.display(),
            q.display()
        );
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Typed repository over the state layout.
#[derive(Debug, Clone)]
pub struct Store {
    pub layout: StateLayout,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SchedulerState {
    pub paused: bool,
}

/// Human/CLI requests addressed to a specific run's driver.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct RunControl {
    pub cancel_requested: bool,
    pub cancel_reason: Option<String>,
    pub pause_requested: bool,
    /// Set by `run retry`; consumed by the scheduler.
    pub resume_requested: bool,
    pub resume_from_step: Option<String>,
    pub requested_by: Option<String>,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let layout = StateLayout::new(root);
        layout.ensure()?;
        Ok(Self { layout })
    }

    fn path(&self, dir: PathBuf, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            bail!("invalid identifier {id:?}");
        }
        Ok(dir.join(format!("{id}.json")))
    }

    pub fn lock(&self, name: &str) -> Result<FileLock> {
        FileLock::acquire(&self.layout.lock_path(name))
    }

    // ---- tasks -------------------------------------------------------

    /// Allocate the next human-friendly task number (`1`, `2`, …).
    pub fn next_task_number(&self) -> Result<u64> {
        let _g = self.lock("counter")?;
        let p = self.layout.counter_file();
        let cur: u64 = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        // Never reuse a number even if the counter file was lost.
        let max_existing = self
            .list_ids(&self.layout.tasks_dir())?
            .iter()
            .filter_map(|s| s.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        let next = cur.max(max_existing) + 1;
        atomic_write(&p, next.to_string().as_bytes())?;
        Ok(next)
    }

    pub fn save_task(&self, t: &Task) -> Result<()> {
        write_doc(&self.path(self.layout.tasks_dir(), &t.task_id)?, "task", t)
    }
    pub fn load_task(&self, id: &str) -> Result<Task> {
        let p = self.path(self.layout.tasks_dir(), id)?;
        if !p.exists() {
            return Err(StoreError::NotFound(format!("task {id}")).into());
        }
        read_doc(&self.layout, &p, "task")
    }
    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let mut out = vec![];
        for id in self.list_ids(&self.layout.tasks_dir())? {
            match self.load_task(&id) {
                Ok(t) => out.push(t),
                Err(e) => tracing::warn!("skipping task {id}: {e:#}"),
            }
        }
        out.sort_by_key(|t| t.task_id.parse::<u64>().unwrap_or(u64::MAX));
        Ok(out)
    }

    /// Read-modify-write a task under its lock.
    pub fn update_task<F: FnOnce(&mut Task) -> Result<()>>(&self, id: &str, f: F) -> Result<Task> {
        let _g = self.lock(&format!("task-{id}"))?;
        let mut t = self.load_task(id)?;
        f(&mut t)?;
        t.updated_at = crate::model::now();
        self.save_task(&t)?;
        Ok(t)
    }

    // ---- runs --------------------------------------------------------

    pub fn save_run(&self, r: &Run) -> Result<()> {
        write_doc(&self.path(self.layout.runs_dir(), &r.run_id)?, "run", r)
    }
    pub fn load_run(&self, id: &str) -> Result<Run> {
        let p = self.path(self.layout.runs_dir(), id)?;
        if !p.exists() {
            return Err(StoreError::NotFound(format!("run {id}")).into());
        }
        read_doc(&self.layout, &p, "run")
    }
    pub fn list_runs(&self) -> Result<Vec<Run>> {
        let mut out = vec![];
        for id in self.list_ids(&self.layout.runs_dir())? {
            match self.load_run(&id) {
                Ok(r) => out.push(r),
                Err(e) => tracing::warn!("skipping run {id}: {e:#}"),
            }
        }
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }

    /// Resolve a user-supplied run reference: full id, unique id prefix,
    /// or `#<task>[variant]` / `<task>` display name.
    pub fn resolve_run(&self, reference: &str) -> Result<Run> {
        let reference = reference.trim();
        if let Ok(r) = self.load_run(reference) {
            return Ok(r);
        }
        let runs = self.list_runs()?;
        let name = reference.trim_start_matches('#');
        let by_name: Vec<&Run> = runs
            .iter()
            .filter(|r| r.display_name().trim_start_matches('#') == name)
            .collect();
        if let Some(r) = by_name.last() {
            return Ok((*r).clone());
        }
        let by_prefix: Vec<&Run> = runs
            .iter()
            .filter(|r| r.run_id.starts_with(reference))
            .collect();
        match by_prefix.len() {
            1 => Ok(by_prefix[0].clone()),
            0 => Err(StoreError::NotFound(format!("run {reference}")).into()),
            n => bail!("run reference {reference:?} is ambiguous ({n} matches)"),
        }
    }

    // ---- approvals ---------------------------------------------------

    pub fn save_approval(&self, a: &ApprovalRequest) -> Result<()> {
        write_doc(
            &self.path(self.layout.approvals_dir(), &a.approval_id)?,
            "approval",
            a,
        )
    }
    pub fn load_approval(&self, id: &str) -> Result<ApprovalRequest> {
        let p = self.path(self.layout.approvals_dir(), id)?;
        if !p.exists() {
            return Err(StoreError::NotFound(format!("approval {id}")).into());
        }
        read_doc(&self.layout, &p, "approval")
    }
    pub fn list_approvals(&self) -> Result<Vec<ApprovalRequest>> {
        let mut out = vec![];
        for id in self.list_ids(&self.layout.approvals_dir())? {
            match self.load_approval(&id) {
                Ok(a) => out.push(a),
                Err(e) => tracing::warn!("skipping approval {id}: {e:#}"),
            }
        }
        out.sort_by_key(|a| a.requested_at);
        Ok(out)
    }
    pub fn resolve_approval(&self, reference: &str) -> Result<ApprovalRequest> {
        if let Ok(a) = self.load_approval(reference) {
            return Ok(a);
        }
        let all = self.list_approvals()?;
        let m: Vec<_> = all
            .into_iter()
            .filter(|a| a.approval_id.starts_with(reference))
            .collect();
        match m.len() {
            1 => Ok(m.into_iter().next().unwrap()),
            0 => Err(StoreError::NotFound(format!("approval {reference}")).into()),
            n => bail!("approval reference {reference:?} is ambiguous ({n} matches)"),
        }
    }
    pub fn update_approval<F: FnOnce(&mut ApprovalRequest) -> Result<()>>(
        &self,
        id: &str,
        f: F,
    ) -> Result<ApprovalRequest> {
        let _g = self.lock(&format!("approval-{id}"))?;
        let mut a = self.load_approval(id)?;
        f(&mut a)?;
        self.save_approval(&a)?;
        Ok(a)
    }

    // ---- epics -------------------------------------------------------

    /// Next epic id: `E1`, `E2`, … (never reused).
    pub fn next_epic_id(&self) -> Result<String> {
        let _g = self.lock("epic-counter")?;
        let p = self.layout.epic_counter_file();
        let cur: u64 = std::fs::read_to_string(&p).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let max_existing = self.list_ids(&self.layout.epics_dir())?.iter().filter_map(|s| s.trim_start_matches('E').parse::<u64>().ok()).max().unwrap_or(0);
        let next = cur.max(max_existing) + 1;
        atomic_write(&p, next.to_string().as_bytes())?;
        Ok(format!("E{next}"))
    }
    pub fn save_epic(&self, e: &crate::epic::Epic) -> Result<()> {
        write_doc(&self.path(self.layout.epics_dir(), &e.epic_id)?, "epic", e)
    }
    pub fn load_epic(&self, id: &str) -> Result<crate::epic::Epic> {
        let id = id.trim_start_matches('#');
        let id = if id.chars().all(|c| c.is_ascii_digit()) { format!("E{id}") } else { id.to_string() };
        let p = self.path(self.layout.epics_dir(), &id)?;
        if !p.exists() {
            return Err(StoreError::NotFound(format!("epic {id}")).into());
        }
        read_doc(&self.layout, &p, "epic")
    }
    pub fn list_epics(&self) -> Result<Vec<crate::epic::Epic>> {
        let mut out = vec![];
        for id in self.list_ids(&self.layout.epics_dir())? {
            match self.load_epic(&id) {
                Ok(e) => out.push(e),
                Err(e) => tracing::warn!("skipping epic {id}: {e:#}"),
            }
        }
        out.sort_by_key(|e| e.epic_id.trim_start_matches('E').parse::<u64>().unwrap_or(u64::MAX));
        Ok(out)
    }
    pub fn update_epic<F: FnOnce(&mut crate::epic::Epic) -> Result<()>>(&self, id: &str, f: F) -> Result<crate::epic::Epic> {
        let mut e = self.load_epic(id)?;
        let _g = self.lock(&format!("epic-{}", e.epic_id))?;
        e = self.load_epic(&e.epic_id)?;
        f(&mut e)?;
        e.updated_at = crate::model::now();
        self.save_epic(&e)?;
        Ok(e)
    }

    // ---- control & scheduler ----------------------------------------

    pub fn load_control(&self, run_id: &str) -> Result<RunControl> {
        let p = self.path(self.layout.control_dir(), run_id)?;
        if !p.exists() {
            return Ok(RunControl::default());
        }
        read_doc(&self.layout, &p, "control")
    }
    pub fn update_control<F: FnOnce(&mut RunControl)>(&self, run_id: &str, f: F) -> Result<RunControl> {
        let _g = self.lock(&format!("control-{run_id}"))?;
        let mut c = self.load_control(run_id)?;
        f(&mut c);
        write_doc(&self.path(self.layout.control_dir(), run_id)?, "control", &c)?;
        Ok(c)
    }
    pub fn load_scheduler(&self) -> Result<SchedulerState> {
        let p = self.layout.scheduler_file();
        if !p.exists() {
            return Ok(SchedulerState::default());
        }
        read_doc(&self.layout, &p, "scheduler")
    }
    pub fn save_scheduler(&self, s: &SchedulerState) -> Result<()> {
        write_doc(&self.layout.scheduler_file(), "scheduler", s)
    }

    fn list_ids(&self, dir: &Path) -> Result<Vec<String>> {
        let mut out = vec![];
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for e in rd {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".json") {
                out.push(id.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Files quarantined because of corruption (reported by `doctor`).
    pub fn corrupt_files(&self) -> Vec<PathBuf> {
        let mut out = vec![];
        for d in [
            self.layout.tasks_dir(),
            self.layout.runs_dir(),
            self.layout.approvals_dir(),
            self.layout.control_dir(),
            self.layout.epics_dir(),
        ] {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().contains(".corrupt-") {
                        out.push(e.path());
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Doc {
        a: u32,
        b: String,
    }

    #[test]
    fn roundtrip_and_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::new(dir.path());
        layout.ensure().unwrap();
        let p = dir.path().join("state/tasks/x.json");
        let d = Doc { a: 1, b: "x".into() };
        write_doc(&p, "doc", &d).unwrap();
        let back: Doc = read_doc(&layout, &p, "doc").unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn detects_tampering_and_quarantines() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::new(dir.path());
        layout.ensure().unwrap();
        let p = dir.path().join("state/tasks/x.json");
        write_doc(&p, "doc", &Doc { a: 1, b: "x".into() }).unwrap();
        let s = std::fs::read_to_string(&p).unwrap().replace("\"a\": 1", "\"a\": 2");
        std::fs::write(&p, s).unwrap();
        let err = read_doc::<Doc>(&layout, &p, "doc").unwrap_err();
        assert!(err.to_string().contains("corrupt"), "{err}");
        assert!(!p.exists());
        let store = Store { layout };
        assert_eq!(store.corrupt_files().len(), 1);
    }

    #[test]
    fn detects_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::new(dir.path());
        layout.ensure().unwrap();
        let p = dir.path().join("state/tasks/y.json");
        write_doc(&p, "doc", &Doc { a: 1, b: "x".into() }).unwrap();
        let s = std::fs::read(&p).unwrap();
        std::fs::write(&p, &s[..s.len() / 2]).unwrap();
        assert!(read_doc::<Doc>(&layout, &p, "doc").is_err());
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert!(s.load_task("../etc").is_err());
        assert!(s.load_run("a/b").is_err());
    }

    #[test]
    fn task_counter_monotonic() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.next_task_number().unwrap(), 1);
        assert_eq!(s.next_task_number().unwrap(), 2);
        std::fs::remove_file(s.layout.counter_file()).unwrap();
        // Without existing task docs the counter restarts, but never below
        // the highest persisted task.
        assert_eq!(s.next_task_number().unwrap(), 1);
    }
}
