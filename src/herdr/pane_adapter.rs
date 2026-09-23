//! Pane, tab and notification operations over the socket.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use super::{HResult, HerdrError, PaneInfo, ProcessInfo, SocketHerdr, TabHandle, TabInfo, METADATA_SOURCE};

impl SocketHerdr {
    pub(super) fn pane_create_tab(&self, workspace_id: &str, cwd: &Path, label: &str, env: &BTreeMap<String, String>) -> HResult<TabHandle> {
        let r = self.client.request(
            "tab.create",
            json!({"workspace_id": workspace_id, "cwd": cwd, "label": label, "env": env, "focus": false}),
            None,
        )?;
        let tab: TabInfo = serde_json::from_value(r.get("tab").cloned().unwrap_or(Value::Null))
            .map_err(|e| HerdrError::Protocol(format!("tab.create: {e}")))?;
        let pane: PaneInfo = serde_json::from_value(r.get("root_pane").cloned().unwrap_or(Value::Null))
            .map_err(|e| HerdrError::Protocol(format!("tab.create root_pane: {e}")))?;
        Ok(TabHandle { tab_id: tab.tab_id, pane_id: pane.pane_id })
    }

    pub(super) fn pane_rename(&self, pane_id: &str, label: &str) -> HResult<()> {
        self.client.request("pane.rename", json!({"pane_id": pane_id, "label": label}), None).map(|_| ())
    }

    pub(super) fn pane_report_metadata(&self, pane_id: &str, title: &str, tokens: &BTreeMap<String, Option<String>>) -> HResult<()> {
        self.client
            .request(
                "pane.report_metadata",
                json!({"pane_id": pane_id, "source": METADATA_SOURCE, "title": title, "tokens": tokens}),
                None,
            )
            .map(|_| ())
    }

    pub(super) fn pane_get(&self, pane_id: &str) -> HResult<Option<PaneInfo>> {
        match self.client.request_field::<PaneInfo>("pane.get", json!({"pane_id": pane_id}), "pane", None) {
            Ok(p) => Ok(Some(p)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub(super) fn pane_list(&self) -> HResult<Vec<PaneInfo>> {
        self.client.request_field("pane.list", json!({}), "panes", None)
    }

    /// `source` is a Herdr `ReadSource`: `visible`, `recent`,
    /// `recent_unwrapped` or `detection` (underscore, unlike the CLI flag).
    pub(super) fn pane_read(&self, pane_id: &str, source: &str, lines: Option<u32>) -> HResult<String> {
        let mut params = json!({"pane_id": pane_id, "source": source, "format": "text", "strip_ansi": true});
        if let Some(n) = lines {
            params["lines"] = json!(n);
        }
        let r = self.client.request("pane.read", params, None)?;
        Ok(r.pointer("/read/text").and_then(Value::as_str).unwrap_or_default().to_string())
    }

    pub(super) fn pane_close(&self, pane_id: &str) -> HResult<()> {
        self.client.request("pane.close", json!({"pane_id": pane_id}), None).map(|_| ())
    }

    pub(super) fn pane_focus(&self, pane_id: &str) -> HResult<()> {
        self.client.request("pane.focus", json!({"pane_id": pane_id}), None).map(|_| ())
    }

    pub(super) fn pane_process_info(&self, pane_id: &str) -> HResult<ProcessInfo> {
        self.client.request_field("pane.process_info", json!({"pane_id": pane_id}), "process_info", None)
    }

    pub(super) fn pane_notify(&self, title: &str, body: &str, request_sound: bool) -> HResult<()> {
        self.client
            .request(
                "notification.show",
                json!({"title": title, "body": body, "sound": if request_sound { "request" } else { "none" }}),
                Some(Duration::from_secs(5)),
            )
            .map(|_| ())
    }

    pub(super) fn pane_open_plugin(&self, plugin_id: &str, entrypoint: &str, placement: &str, env: &BTreeMap<String, String>) -> HResult<()> {
        self.client
            .request(
                "plugin.pane.open",
                json!({"plugin_id": plugin_id, "entrypoint": entrypoint, "placement": placement, "focus": true, "env": env}),
                None,
            )
            .map(|_| ())
    }
}
