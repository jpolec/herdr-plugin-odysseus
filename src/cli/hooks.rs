//! Herdr plugin entrypoints (see herdr-plugin.toml). Each is short-lived:
//! startup ensures the daemon; events only nudge an already-running daemon;
//! actions open plugin panes.

use anyhow::Result;
use clap::Subcommand;

use super::App;

#[derive(Subcommand, Debug)]
pub enum HookCmd {
    /// `[[startup]]`: start the daemon (which runs recovery).
    Startup,
    /// `[[events]]`: forward a wake-up to a running daemon.
    Event,
    /// Action: open the orchestrator pane.
    Open {
        #[arg(long)]
        view: Option<String>,
    },
    /// Action: open the new-task popup.
    NewTask,
    /// Claude Code `PreToolUse` hook (registered via `--settings` when the
    /// orchestrator starts Claude): policy check before each tool call.
    ClaudePretool {
        #[arg(long)]
        run: String,
    },
    /// Action (opt-in): symlink the CLI into ~/.local/bin.
    InstallCli {
        /// Target directory (default: ~/.local/bin).
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
}

pub fn run(app: &App, h: HookCmd) -> Result<i32> {
    match h {
        HookCmd::Startup => {
            // Only start if there is something to do or recover; otherwise
            // stay out of the user's way.
            let ctx = app.ctx(false)?;
            let pending = ctx.store.list_runs()?.iter().any(|r| !r.status.is_terminal()) || ctx.store.list_tasks()?.iter().any(|t| t.status == crate::model::TaskStatus::Queued);
            if pending {
                app.ensure_daemon()?;
                println!("herdr-orchestrator: daemon ensured (recovery runs on start)");
            } else {
                println!("herdr-orchestrator: nothing to recover");
            }
            Ok(0)
        }
        HookCmd::Event => {
            let layout = crate::store::StateLayout::new(&app.paths.state_dir);
            if let Some(ev) = crate::herdr::events::HookEvent::from_env() {
                // Never spawn the daemon for an event: most events are about
                // panes that have nothing to do with us.
                let _ = crate::daemon::send(&layout, serde_json::json!({"cmd": "event", "event": ev.event, "pane_id": ev.pane_id}));
            }
            Ok(0)
        }
        HookCmd::Open { view } => {
            super::open_plugin_pane("dashboard", "popup", view.as_deref())?;
            Ok(0)
        }
        HookCmd::NewTask => {
            super::open_plugin_pane("new-task", "popup", None)?;
            Ok(0)
        }
        HookCmd::ClaudePretool { run } => {
            // Never fail the agent's tool call because of us: any internal
            // error means "no decision" (the diff gate still applies).
            match claude_pretool(app, &run) {
                Ok(Some(out)) => println!("{out}"),
                Ok(None) => {}
                Err(e) => eprintln!("herdr-orchestrator hook: {e:#}"),
            }
            Ok(0)
        }
        HookCmd::InstallCli { dir } => {
            let msg = install_cli(dir)?;
            println!("{msg}");
            // Actions run in the background; tell the user in Herdr too.
            if let Some(h) = crate::herdr::SocketHerdr::discover(None) {
                use crate::herdr::HerdrApi;
                let _ = h.notify("herdr-orchestrator CLI", &msg, false);
            }
            Ok(0)
        }
    }
}

fn claude_pretool(app: &App, run_id: &str) -> Result<Option<serde_json::Value>> {
    use std::io::Read;
    let mut raw = String::new();
    std::io::stdin().take(4 * 1024 * 1024).read_to_string(&mut raw)?;
    let input: serde_json::Value = serde_json::from_str(&raw)?;
    let ctx = app.ctx(false)?;
    let run = ctx.store.load_run(run_id)?;
    let cfg = ctx.load_config(Some(&run.repo_root))?;
    let set = ctx.policy_for(&cfg)?;
    let worktree = run.git.worktree_path.clone().unwrap_or_else(|| run.repo_root.clone());
    let locked: Vec<String> = run.contract.as_ref().map(|c| c.files.keys().cloned().collect()).unwrap_or_default();
    let Some((d, out)) = crate::policies::agent_hook::decide_with_locks(&set, &input, &worktree, &locked) else { return Ok(None) };
    if d.decision != crate::policies::Decision::Allow {
        let tool = input.get("tool_name").and_then(|t| t.as_str()).unwrap_or("?");
        ctx.audit(
            crate::audit::EventDraft::new("agent_tool_checked", crate::audit::Actor::policy())
                .run(&run.run_id, &run.task_id)
                .data(serde_json::json!({"tool": tool, "subject": d.subject, "decision": d.decision.as_str(), "reason": d.reason, "blocked": out.is_some()})),
        );
    }
    Ok(out)
}

/// Symlink the running binary into `dir` (default `~/.local/bin`). Only
/// ever replaces a symlink; never overwrites a real file.
fn install_cli(dir: Option<std::path::PathBuf>) -> Result<String> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = match dir {
        Some(d) => d,
        None => std::path::PathBuf::from(std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME is not set"))?).join(".local/bin"),
    };
    std::fs::create_dir_all(&dir)?;
    let link = dir.join("herdr-orchestrator");
    match std::fs::symlink_metadata(&link) {
        Ok(md) if md.file_type().is_symlink() => std::fs::remove_file(&link)?,
        Ok(_) => anyhow::bail!("{} exists and is not a symlink; not touching it", link.display()),
        Err(_) => {}
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&exe, &link)?;
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir));
    Ok(format!(
        "linked {} -> {}{}",
        link.display(),
        exe.display(),
        if on_path { String::new() } else { format!(" ({} is not on your PATH; add it to use `herdr-orchestrator`)", dir.display()) }
    ))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn install_cli_only_replaces_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let m = super::install_cli(Some(d.path().to_path_buf())).unwrap();
        assert!(m.contains("linked"));
        assert!(super::install_cli(Some(d.path().to_path_buf())).is_ok(), "re-running replaces its own link");
        std::fs::remove_file(d.path().join("herdr-orchestrator")).unwrap();
        std::fs::write(d.path().join("herdr-orchestrator"), "mine").unwrap();
        assert!(super::install_cli(Some(d.path().to_path_buf())).is_err());
        assert_eq!(std::fs::read_to_string(d.path().join("herdr-orchestrator")).unwrap(), "mine");
    }
}
