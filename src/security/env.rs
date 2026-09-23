//! Explicit environment inheritance for child processes.
//!
//! Children never receive the full parent environment by default. They get
//! an allowlist (`environment.inherit`) plus explicit `environment.set`
//! values plus orchestrator variables. Agent CLIs often authenticate through
//! files in `$HOME` (keychain, `~/.claude`, `~/.codex`), so `HOME` is in the
//! default allowlist; API-key variables must be opted into explicitly.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentConfig {
    /// Variable names (exact, or with a trailing `*` prefix wildcard).
    #[serde(default = "default_inherit")]
    pub inherit: Vec<String>,
    /// Literal values to set (never logged).
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Escape hatch: pass the whole parent environment. Discouraged; audited.
    #[serde(default)]
    pub inherit_all: bool,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self {
            inherit: default_inherit(),
            set: BTreeMap::new(),
            inherit_all: false,
        }
    }
}

pub fn default_inherit() -> Vec<String> {
    [
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_*", "TERM", "COLORTERM", "TMPDIR",
        "TZ", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR", "SSH_AUTH_SOCK", "CARGO_HOME", "RUSTUP_HOME", "GOPATH", "NVM_DIR",
        "HERDR_*",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

impl EnvironmentConfig {
    /// Build the child environment from `parent`.
    pub fn build(
        &self,
        parent: impl Iterator<Item = (String, String)>,
        extra: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (k, v) in parent {
            if self.inherit_all || self.inherit.iter().any(|p| matches(p, &k)) {
                out.insert(k, v);
            }
        }
        for (k, v) in &self.set {
            out.insert(k.clone(), v.clone());
        }
        for (k, v) in extra {
            out.insert(k.clone(), v.clone());
        }
        out
    }

    /// Names only — safe to log.
    pub fn describe(&self, env: &BTreeMap<String, String>) -> Vec<String> {
        env.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_filters_parent_env() {
        let cfg = EnvironmentConfig::default();
        let parent = vec![
            ("PATH".to_string(), "/bin".to_string()),
            ("AWS_SECRET_ACCESS_KEY".to_string(), "x".to_string()),
            ("LC_ALL".to_string(), "C".to_string()),
            ("OPENAI_API_KEY".to_string(), "sk".to_string()),
        ];
        let mut extra = BTreeMap::new();
        extra.insert("HERDR_ORCH_RUN_ID".to_string(), "r".to_string());
        let env = cfg.build(parent.into_iter(), &extra);
        assert!(env.contains_key("PATH"));
        assert!(env.contains_key("LC_ALL"));
        assert!(!env.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!env.contains_key("OPENAI_API_KEY"));
        assert_eq!(env["HERDR_ORCH_RUN_ID"], "r");
    }

    #[test]
    fn inherit_all_escape_hatch() {
        let cfg = EnvironmentConfig { inherit_all: true, ..Default::default() };
        let env = cfg.build(vec![("X".into(), "1".into())].into_iter(), &BTreeMap::new());
        assert!(env.contains_key("X"));
    }
}
