//! Herdr event hooks. The manifest subscribes to a few lifecycle events
//! (`pane.closed`, `pane.exited`, `pane.agent_status_changed`,
//! `worktree.removed`); the hook command only forwards a *wake-up* to the
//! daemon. Events are invalidation signals, never a source of truth: the
//! run driver re-reads authoritative state from Herdr.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Event names this plugin hooks (all present in Herdr 0.9.0's
/// `PLUGIN_HOOK_EVENT_KINDS`).
pub const HOOKED_EVENTS: &[&str] = &["pane.closed", "pane.exited", "pane.agent_status_changed", "worktree.removed"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HookEvent {
    pub event: String,
    pub pane_id: Option<String>,
    pub workspace_id: Option<String>,
    pub agent_status: Option<String>,
}

fn find_str(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(o) => {
            if let Some(Value::String(s)) = o.get(key) {
                return Some(s.clone());
            }
            o.values().find_map(|x| find_str(x, key))
        }
        Value::Array(a) => a.iter().find_map(|x| find_str(x, key)),
        _ => None,
    }
}

impl HookEvent {
    /// Parse from `HERDR_PLUGIN_EVENT` / `HERDR_PLUGIN_EVENT_JSON`.
    pub fn from_env() -> Option<Self> {
        let name = std::env::var("HERDR_PLUGIN_EVENT").ok()?;
        let json = std::env::var("HERDR_PLUGIN_EVENT_JSON").ok();
        Some(Self::parse(&name, json.as_deref()))
    }

    pub fn parse(name: &str, json: Option<&str>) -> Self {
        let v: Value = json.and_then(|j| serde_json::from_str(j).ok()).unwrap_or(Value::Null);
        Self {
            event: name.to_string(),
            pane_id: find_str(&v, "pane_id"),
            workspace_id: find_str(&v, "workspace_id"),
            agent_status: find_str(&v, "agent_status"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ids_from_nested_payloads() {
        let e = HookEvent::parse(
            "pane.agent_status_changed",
            Some(r#"{"event":"pane.agent_status_changed","data":{"type":"pane_agent_status_changed","pane_id":"w2:p3","workspace_id":"w2","agent_status":"blocked"}}"#),
        );
        assert_eq!(e.pane_id.as_deref(), Some("w2:p3"));
        assert_eq!(e.agent_status.as_deref(), Some("blocked"));
        let e = HookEvent::parse("pane.closed", Some("not json"));
        assert_eq!(e.pane_id, None);
    }
}
