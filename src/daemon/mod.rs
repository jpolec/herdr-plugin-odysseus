//! Single-instance engine daemon.
//!
//! Herdr plugin v1 has no supervised background processes (startup hooks
//! are one-shot), so the orchestrator hosts its scheduler in a detached
//! daemon, started on demand (`daemon ensure`). A `flock` on
//! `locks/daemon.lock` guarantees one instance per state directory.
//!
//! Communication is state-first: clients write durable documents, then
//! *nudge* the daemon over `daemon.sock` (NDJSON). The daemon also polls,
//! so a lost nudge only costs latency.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::audit::{Actor, EventDraft};
use crate::engine::scheduler::Scheduler;
use crate::engine::EngineCtx;
use crate::store::{FileLock, StateLayout};

pub struct DaemonOptions {
    /// Exit after this long with nothing active or queued (None = never).
    pub idle_exit: Option<Duration>,
    pub poll: Duration,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self { idle_exit: Some(Duration::from_secs(30 * 60)), poll: Duration::from_millis(1000) }
    }
}

fn pid_file(layout: &StateLayout) -> PathBuf {
    layout.root.join("daemon.pid")
}

/// Run the daemon in the foreground (used by `daemon run` and by the
/// detached child spawned from `daemon ensure`).
pub fn run(ctx: Arc<EngineCtx>, opts: DaemonOptions) -> Result<()> {
    let layout = ctx.store.layout.clone();
    let Some(_lock) = FileLock::try_acquire(&layout.lock_path("daemon"))? else {
        bail!("another herdr-orchestrator daemon is already running for {}", layout.root.display());
    };
    std::fs::write(pid_file(&layout), std::process::id().to_string())?;
    ctx.audit(EventDraft::new("daemon_started", Actor::orchestrator()).data(json!({"pid": std::process::id(), "version": env!("CARGO_PKG_VERSION"), "herdr": ctx.herdr.as_ref().and_then(|h| h.socket_path())})));

    let reports = crate::recovery::recover_all(&ctx)?;
    for r in &reports {
        tracing::info!("recovery {}: {:?} — {}", r.display, r.classification, r.reason);
    }

    let (tx, rx) = mpsc::channel::<()>();
    let sock = layout.daemon_socket();
    let _ = std::fs::remove_file(&sock);
    let listener = bind(&sock)?;
    let ctx2 = ctx.clone();
    std::thread::Builder::new().name("control-socket".into()).spawn(move || serve(listener, ctx2, tx))?;

    let global = ctx.load_config(None)?;
    let mut sched = Scheduler::new(ctx.clone(), global.config.scheduler.max_parallel_runs);
    let mut idle_since: Option<Instant> = None;
    let mut last_watch: Option<Instant> = None;
    loop {
        if global.config.github.watch_prs && last_watch.is_none_or(|t| t.elapsed() >= global.config.github.watch_interval.as_duration()) {
            last_watch = Some(Instant::now());
            match crate::engine::followup::watch_prs(&ctx, 14) {
                Ok(v) if !v.is_empty() => tracing::info!("PR feedback on {} run(s)", v.len()),
                Ok(_) => {}
                Err(e) => tracing::warn!("PR watch failed: {e:#}"),
            }
        }
        match sched.tick() {
            Ok(rep) => {
                for id in &rep.started_runs {
                    tracing::info!("started run {id}");
                }
            }
            Err(e) => tracing::error!("scheduler tick failed: {e:#}"),
        }
        if sched.is_idle().unwrap_or(false) {
            let since = *idle_since.get_or_insert_with(Instant::now);
            // The PR watcher is a reason to stay up.
            if opts.idle_exit.is_some_and(|d| since.elapsed() >= d) && no_pending_approvals(&ctx) && !global.config.github.watch_prs {
                tracing::info!("idle; daemon exiting");
                break;
            }
        } else {
            idle_since = None;
        }
        if SHUTDOWN.load(std::sync::atomic::Ordering::SeqCst) {
            // Runs are NOT cancelled (the user didn't ask for that) and not
            // joined (an agent step can take hours). Every transition is
            // already durable; the next start reconciles them.
            tracing::info!("shutdown requested; {} active run(s) will be recovered on next start", sched.active.len());
            break;
        }
        let _ = rx.recv_timeout(opts.poll);
    }
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(pid_file(&layout));
    ctx.audit(EventDraft::new("daemon_stopped", Actor::orchestrator()).data(json!({"pid": std::process::id()})));
    Ok(())
}

static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn no_pending_approvals(ctx: &EngineCtx) -> bool {
    crate::approvals::pending(&ctx.store).map(|v| v.is_empty()).unwrap_or(true)
}

#[cfg(unix)]
fn bind(sock: &Path) -> Result<std::os::unix::net::UnixListener> {
    let l = std::os::unix::net::UnixListener::bind(sock).with_context(|| format!("binding {}", sock.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

#[cfg(unix)]
fn serve(listener: std::os::unix::net::UnixListener, ctx: Arc<EngineCtx>, tx: mpsc::Sender<()>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        let mut reader = BufReader::new(&stream);
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
        let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("");
        let resp = match cmd {
            "ping" => json!({"ok": true, "pid": std::process::id(), "version": env!("CARGO_PKG_VERSION")}),
            "nudge" | "event" => {
                let _ = tx.send(());
                json!({"ok": true})
            }
            "status" => json!({
                "ok": true,
                "pid": std::process::id(),
                "agents_in_use": ctx.agent_slots.in_use(),
                "agent_capacity": ctx.agent_slots.capacity(),
            }),
            "shutdown" => {
                SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = tx.send(());
                json!({"ok": true})
            }
            _ => json!({"ok": false, "error": "unknown command"}),
        };
        let mut w = &stream;
        let _ = w.write_all(format!("{resp}\n").as_bytes());
    }
}

/// Send one command to a running daemon. `Ok(None)` if none is listening.
pub fn send(layout: &StateLayout, cmd: Value) -> Result<Option<Value>> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let sock = layout.daemon_socket();
        let Ok(stream) = UnixStream::connect(&sock) else { return Ok(None) };
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        (&stream).write_all(format!("{cmd}\n").as_bytes())?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        Ok(serde_json::from_str(line.trim()).ok())
    }
    #[cfg(not(unix))]
    {
        let _ = (layout, cmd);
        Ok(None)
    }
}

pub fn nudge(layout: &StateLayout) -> bool {
    matches!(send(layout, json!({"cmd": "nudge"})), Ok(Some(_)))
}

pub fn is_running(layout: &StateLayout) -> bool {
    matches!(send(layout, json!({"cmd": "ping"})), Ok(Some(_)))
}

/// Start the daemon detached if it is not running. The child gets its own
/// session (`setsid`) so closing the Herdr client or pane doesn't kill it.
pub fn ensure(layout: &StateLayout, extra_env: &[(&str, String)]) -> Result<bool> {
    if is_running(layout) {
        return Ok(false);
    }
    let exe = std::env::current_exe().context("locating own executable")?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(layout.daemon_log())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").arg("run").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    // Make the state dir explicit for the child.
    cmd.env("HERDR_ORCH_STATE_DIR", &layout.root);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.spawn().context("spawning daemon")?;
    // Wait briefly for the socket so callers can nudge immediately.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        if is_running(layout) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    tracing::warn!("daemon did not answer within 5s; see {}", layout.daemon_log().display());
    Ok(true)
}

pub fn stop(layout: &StateLayout) -> Result<bool> {
    Ok(send(layout, json!({"cmd": "shutdown"}))?.is_some())
}

pub fn daemon_pid(layout: &StateLayout) -> Option<u32> {
    std::fs::read_to_string(pid_file(layout)).ok().and_then(|s| s.trim().parse().ok())
}
