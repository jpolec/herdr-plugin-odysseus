//! Opening a run's git worktree as a Herdr workspace.
//!
//! The worktree itself is created by `crate::git` (explicit base SHA,
//! collision checks, no destructive cleanup). Herdr is then asked to open
//! that existing checkout with `worktree.open`, which groups it with the
//! parent repository workspace. If Herdr does not recognise the checkout,
//! we fall back to a plain `workspace.create` with the worktree as cwd.

use std::path::Path;

use serde_json::{json, Value};

use super::{HResult, HerdrError, PaneInfo, SocketHerdr, WorkspaceHandle, WorkspaceInfo};

fn handle_from(r: &Value) -> HResult<WorkspaceHandle> {
    let ws: WorkspaceInfo = serde_json::from_value(r.get("workspace").cloned().unwrap_or(Value::Null))
        .map_err(|e| HerdrError::Protocol(format!("workspace: {e}")))?;
    let root: Option<PaneInfo> = r.get("root_pane").cloned().and_then(|v| serde_json::from_value(v).ok());
    Ok(WorkspaceHandle {
        workspace_id: ws.workspace_id,
        root_pane_id: root.map(|p| p.pane_id),
        already_open: r.get("already_open").and_then(Value::as_bool).unwrap_or(false),
    })
}

impl SocketHerdr {
    pub(super) fn worktree_open_workspace(&self, repo_root: &Path, worktree: &Path, label: &str) -> HResult<WorkspaceHandle> {
        let opened = self.client.request(
            "worktree.open",
            json!({"cwd": repo_root, "path": worktree, "label": label, "focus": false}),
            None,
        );
        match opened {
            Ok(r) => handle_from(&r),
            Err(e @ HerdrError::Unavailable(_)) => Err(e),
            Err(e) => {
                tracing::warn!("worktree.open failed ({e}); falling back to workspace.create");
                let r = self.client.request(
                    "workspace.create",
                    json!({"cwd": worktree, "label": label, "focus": false}),
                    None,
                )?;
                handle_from(&r)
            }
        }
    }
}
