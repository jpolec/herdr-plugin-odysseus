//! Process execution primitives: direct argv spawning (never through a
//! shell), explicit environments, timeouts, cooperative cancellation, and
//! graceful-then-forceful termination of the whole process group.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::audit::redact::redact_str;

/// Shared cancellation flag checked by every blocking loop.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// Complete child environment (the parent env is NOT inherited).
    pub env: BTreeMap<String, String>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
    /// Stream combined output here (redacted) as it is produced.
    pub log_path: Option<PathBuf>,
    /// Grace period between SIGTERM and SIGKILL.
    pub kill_grace: Duration,
}

impl Spec {
    pub fn new(argv: Vec<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            argv,
            cwd: cwd.into(),
            env: inherited_minimal_env(),
            stdin: None,
            timeout: None,
            log_path: None,
            kill_grace: Duration::from_secs(5),
        }
    }
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }
    pub fn env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }
    pub fn log(mut self, p: PathBuf) -> Self {
        self.log_path = Some(p);
        self
    }
    pub fn stdin(mut self, b: Vec<u8>) -> Self {
        self.stdin = Some(b);
        self
    }
}

/// Environment for internal tool invocations (git, gh, herdr): the default
/// allowlist from [`crate::security::EnvironmentConfig`] plus `GH_*`/`GIT_*`
/// config variables that those tools legitimately need.
pub fn inherited_minimal_env() -> BTreeMap<String, String> {
    let mut cfg = crate::security::EnvironmentConfig::default();
    cfg.inherit.extend(
        ["GH_*", "GITHUB_TOKEN", "GH_TOKEN", "GIT_*", "SSH_*", "GNUPGHOME", "http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "no_proxy"]
            .iter()
            .map(|s| s.to_string()),
    );
    cfg.build(std::env::vars(), &BTreeMap::new())
}

#[derive(Debug, Clone)]
pub struct Output {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration: Duration,
    pub pid: u32,
}

impl Output {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
    }
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Keep at most `cap` bytes: the first half and the last half.
struct Capture {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    cap: usize,
    dropped: usize,
}

impl Capture {
    fn new(cap: usize) -> Self {
        Self { head: vec![], tail: Default::default(), cap, dropped: 0 }
    }
    fn push(&mut self, buf: &[u8]) {
        let half = self.cap / 2;
        for &b in buf {
            if self.head.len() < half {
                self.head.push(b);
            } else {
                self.tail.push_back(b);
                if self.tail.len() > half {
                    self.tail.pop_front();
                    self.dropped += 1;
                }
            }
        }
    }
    fn into_vec(self) -> Vec<u8> {
        let mut v = self.head;
        if self.dropped > 0 {
            v.extend_from_slice(format!("\n… [{} bytes truncated] …\n", self.dropped).as_bytes());
        }
        v.extend(self.tail);
        v
    }
}

const CAPTURE_CAP: usize = 4 * 1024 * 1024;

pub struct Started {
    pub child: Child,
    pub pid: u32,
}

/// Spawn without waiting (used when the pid must be persisted first).
pub fn spawn(spec: &Spec) -> Result<Started> {
    if spec.argv.is_empty() {
        bail!("empty argv");
    }
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..])
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .stdin(if spec.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group so timeouts/cancels reach grandchildren too.
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("failed to start {:?} in {}", spec.argv[0], spec.cwd.display()))?;
    let pid = child.id();
    Ok(Started { child, pid })
}

/// Run to completion honoring timeout and cancellation.
pub fn run(spec: &Spec, cancel: Option<&CancelToken>) -> Result<Output> {
    let started = spawn(spec)?;
    wait(spec, started, cancel)
}

pub fn wait(spec: &Spec, started: Started, cancel: Option<&CancelToken>) -> Result<Output> {
    let Started { mut child, pid } = started;
    let t0 = Instant::now();
    if let Some(input) = &spec.stdin {
        if let Some(mut w) = child.stdin.take() {
            let input = input.clone();
            std::thread::spawn(move || {
                let _ = w.write_all(&input);
            });
        }
    }
    let log = match &spec.log_path {
        Some(p) => {
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d)?;
            }
            Some(Arc::new(Mutex::new(
                std::fs::OpenOptions::new().create(true).append(true).open(p)?,
            )))
        }
        None => None,
    };
    let out_cap = Arc::new(Mutex::new(Capture::new(CAPTURE_CAP)));
    let err_cap = Arc::new(Mutex::new(Capture::new(CAPTURE_CAP)));
    let mut readers = vec![];
    if let Some(s) = child.stdout.take() {
        readers.push(pump(s, out_cap.clone(), log.clone(), ""));
    }
    if let Some(s) = child.stderr.take() {
        readers.push(pump(s, err_cap.clone(), log.clone(), ""));
    }
    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break Some(st);
        }
        if spec.timeout.is_some_and(|t| t0.elapsed() >= t) {
            timed_out = true;
            terminate_group(pid, &mut child, spec.kill_grace);
            break child.try_wait()?;
        }
        if cancel.is_some_and(|c| c.is_cancelled()) {
            cancelled = true;
            terminate_group(pid, &mut child, spec.kill_grace);
            break child.try_wait()?;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let status = match status {
        Some(s) => Some(s),
        None => child.wait().ok(),
    };
    for r in readers {
        let _ = r.join();
    }
    let exit_code = status.and_then(|s| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            s.code().or_else(|| s.signal().map(|sig| 128 + sig))
        }
        #[cfg(not(unix))]
        {
            s.code()
        }
    });
    let stdout = Arc::try_unwrap(out_cap).map(|m| m.into_inner().unwrap().into_vec()).unwrap_or_default();
    let stderr = Arc::try_unwrap(err_cap).map(|m| m.into_inner().unwrap().into_vec()).unwrap_or_default();
    Ok(Output { exit_code, stdout, stderr, timed_out, cancelled, duration: t0.elapsed(), pid })
}

fn pump(
    mut r: impl Read + Send + 'static,
    cap: Arc<Mutex<Capture>>,
    log: Option<Arc<Mutex<std::fs::File>>>,
    _prefix: &'static str,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut pending = String::new();
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    cap.lock().unwrap().push(&buf[..n]);
                    if let Some(l) = &log {
                        // Redact line by line so secrets split across reads
                        // are still caught at line granularity.
                        pending.push_str(&String::from_utf8_lossy(&buf[..n]));
                        while let Some(i) = pending.find('\n') {
                            let line: String = pending.drain(..=i).collect();
                            let _ = l.lock().unwrap().write_all(redact_str(&line).as_bytes());
                        }
                        if pending.len() > 64 * 1024 {
                            let chunk = std::mem::take(&mut pending);
                            let _ = l.lock().unwrap().write_all(redact_str(&chunk).as_bytes());
                        }
                    }
                }
            }
        }
        if let Some(l) = &log {
            if !pending.is_empty() {
                let _ = l.lock().unwrap().write_all(redact_str(&pending).as_bytes());
            }
        }
    })
}

/// SIGTERM the process group, wait `grace`, then SIGKILL.
pub fn terminate_group(pid: u32, child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    let t0 = Instant::now();
    while t0.elapsed() < grace {
        if let Ok(Some(_)) = child.try_wait() {
            #[cfg(unix)]
            unsafe {
                // Reap stragglers in the group.
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Whether a pid is alive (best-effort, used by recovery).
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Signal a whole process group (recovery / cancel of orphaned commands).
pub fn kill_group(pid: u32, sig: i32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

/// Convenience for short internal tool calls (git, gh): capture, timeout,
/// error on non-zero exit with stderr context.
pub fn run_tool(argv: &[&str], cwd: &Path, timeout: Duration) -> Result<String> {
    let spec = Spec::new(argv.iter().map(|s| s.to_string()).collect(), cwd).timeout(timeout);
    let out = run(&spec, None)?;
    if out.timed_out {
        bail!("`{}` timed out after {:?}", argv.join(" "), timeout);
    }
    if !out.success() {
        bail!(
            "`{}` failed ({}): {}",
            argv.join(" "),
            out.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            out.stderr_str().trim()
        );
    }
    Ok(out.stdout_str())
}

/// Find an executable on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = std::fs::metadata(&p) {
                if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                    return Some(p);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn captures_output_and_exit_code() {
        let d = tempfile::tempdir().unwrap();
        let out = run(&Spec::new(sh("echo hi; echo err >&2; exit 3"), d.path()), None).unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert_eq!(out.stdout_str().trim(), "hi");
        assert_eq!(out.stderr_str().trim(), "err");
    }

    #[test]
    fn times_out_and_kills_group() {
        let d = tempfile::tempdir().unwrap();
        let mut spec = Spec::new(sh("sleep 30 & sleep 30; wait"), d.path()).timeout(Duration::from_millis(300));
        spec.kill_grace = Duration::from_millis(200);
        let t0 = Instant::now();
        let out = run(&spec, None).unwrap();
        assert!(out.timed_out);
        assert!(!out.success());
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancellation() {
        let d = tempfile::tempdir().unwrap();
        let c = CancelToken::new();
        let c2 = c.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.cancel();
        });
        let mut spec = Spec::new(sh("sleep 30"), d.path());
        spec.kill_grace = Duration::from_millis(200);
        let out = run(&spec, Some(&c)).unwrap();
        assert!(out.cancelled);
    }

    #[test]
    fn env_is_not_inherited_wholesale() {
        let d = tempfile::tempdir().unwrap();
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), std::env::var("PATH").unwrap());
        env.insert("ONLY".to_string(), "1".to_string());
        let out = run(&Spec::new(vec!["env".into()], d.path()).env(env), None).unwrap();
        let s = out.stdout_str();
        assert!(s.contains("ONLY=1"));
        assert!(!s.contains("HOME="));
    }

    #[test]
    fn log_is_redacted() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("x.log");
        run(&Spec::new(sh("echo token=ghp_abcdefghijklmnopqrstuvwxyz123456"), d.path()).log(log.clone()), None).unwrap();
        let s = std::fs::read_to_string(log).unwrap();
        assert!(!s.contains("ghp_abcdef"), "{s}");
    }

    #[test]
    fn capture_truncates_middle() {
        let mut c = Capture::new(10);
        c.push(b"0123456789abcdefghij");
        let v = String::from_utf8(c.into_vec()).unwrap();
        assert!(v.starts_with("01234"));
        assert!(v.ends_with("fghij"));
        assert!(v.contains("truncated"));
    }
}
