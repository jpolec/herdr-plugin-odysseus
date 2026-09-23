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
            super::open_plugin_pane("dashboard", "overlay", view.as_deref())?;
            Ok(0)
        }
        HookCmd::NewTask => {
            super::open_plugin_pane("new-task", "popup", None)?;
            Ok(0)
        }
    }
}
