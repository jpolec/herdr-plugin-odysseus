//! Real-time policy for Claude Code through its `PreToolUse` hook.
//!
//! When a Claude pane agent is started, the orchestrator passes it
//! `--settings <worktree>/.herdr-orchestrator/claude-settings.json`, which
//! registers `herdr-orchestrator hook claude-pretool --run <id>` for the tools
//! below. Claude runs the hook *before* each tool call and sends the call on
//! stdin; a DENY from the effective policy blocks the call and the reason
//! goes back to the agent. Everything else returns no decision, so Claude's
//! own permission mode (`acceptEdits`) and the diff gate behave as before —
//! nothing is asked twice.
//!
//! This narrows, but does not close, the gap described in SECURITY_MODEL:
//! it covers Claude only, and only the tools listed in [`MATCHER`].

use std::path::Path;

use serde_json::{json, Value};

use super::{Action, Decision, PolicyDecision, PolicySet, Subject};

/// Tools the hook is registered for.
pub const MATCHER: &str = "Bash|Write|Edit|MultiEdit|NotebookEdit|Read";

/// Settings JSON that registers the hook. `command` is a complete shell
/// command line (already quoted).
pub fn claude_settings(command: &str) -> Value {
    json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": MATCHER,
                "hooks": [{"type": "command", "command": command, "timeout": 30}]
            }]
        }
    })
}

/// Quote one word for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-:=@+,".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Policy subject for one Claude tool call, or `None` for tools we do not
/// judge. Paths inside the worktree are made worktree-relative so project
/// globs match exactly as they do at the diff gate.
pub fn subject_for(input: &Value, worktree: &Path) -> Option<Subject> {
    let tool = input.get("tool_name")?.as_str()?;
    let ti = input.get("tool_input")?;
    let rel = |p: &str| -> String {
        let path = Path::new(p);
        match path.strip_prefix(worktree) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => p.to_string(),
        }
    };
    match tool {
        "Bash" => {
            let cmd = ti.get("command")?.as_str()?;
            // Judge what the command does (its tags and text), not the fact
            // that an agent's commands go through a shell: `approve-shell-mode`
            // is about workflow steps and would flag every call.
            let mut s = Subject::command(&[cmd.to_string()], true);
            s.action = None;
            Some(s)
        }
        "Write" | "Edit" | "MultiEdit" => Some(Subject::file(Action::Write, &rel(ti.get("file_path")?.as_str()?))),
        "NotebookEdit" => {
            let p = ti.get("notebook_path").or_else(|| ti.get("file_path"))?.as_str()?;
            Some(Subject::file(Action::Write, &rel(p)))
        }
        "Read" => Some(Subject::file(Action::Read, &rel(ti.get("file_path")?.as_str()?))),
        _ => None,
    }
}

/// Decide one tool call. Returns the decision (for the audit trail) and the
/// JSON Claude expects on stdout, which is `None` when we have no objection.
pub fn decide(set: &PolicySet, input: &Value, worktree: &Path) -> Option<(PolicyDecision, Option<Value>)> {
    let subject = subject_for(input, worktree)?;
    let d = set.evaluate(&subject);
    let out = (d.decision == Decision::Deny).then(|| {
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": format!("herdr-orchestrator policy denies this: {}. Do not try to work around it; if it is needed, say so in your result.", d.reason),
            }
        })
    });
    Some((d, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tool: &str, input: Value) -> Value {
        json!({"hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": input, "cwd": "/w"})
    }

    #[test]
    fn denies_what_policy_denies_and_stays_quiet_otherwise() {
        let set = PolicySet::builtin_default().unwrap();
        let wt = Path::new("/w");
        let deny = |v: Value| decide(&set, &v, wt).unwrap().1.expect("denied")["hookSpecificOutput"]["permissionDecision"].clone();
        assert_eq!(deny(call("Bash", json!({"command": "cargo test && git push --force origin main"}))), "deny");
        assert_eq!(deny(call("Bash", json!({"command": "rm -rf ~"}))), "deny");
        assert_eq!(deny(call("Write", json!({"file_path": "/w/.env", "content": "X=1"}))), "deny");
        assert_eq!(deny(call("Edit", json!({"file_path": "/w/config/secrets/db.yml", "old_string": "a", "new_string": "b"}))), "deny");
        assert_eq!(deny(call("Read", json!({"file_path": "/home/u/.ssh/id_ed25519"}))), "deny");
        assert_eq!(deny(call("Read", json!({"file_path": "/home/u/.aws/credentials"}))), "deny");
        // Allowed and approval-level calls get no output: Claude's own
        // permission mode and the diff gate handle them.
        for v in [
            call("Bash", json!({"command": "cargo test"})),
            call("Bash", json!({"command": "curl https://example.com"})),
            call("Write", json!({"file_path": "/w/src/lib.rs", "content": ""})),
            call("Write", json!({"file_path": "/w/db/migrations/1.sql", "content": ""})),
            call("Read", json!({"file_path": "/w/src/lib.rs"})),
        ] {
            let (d, out) = decide(&set, &v, wt).unwrap();
            assert!(out.is_none(), "{v}: {}", d.reason);
        }
        // Plain commands are simply allowed (no shell-mode noise in the audit).
        for v in [call("Bash", json!({"command": "ls -la"})), call("Bash", json!({"command": "cargo build 2>&1 | tail"}))] {
            assert_eq!(decide(&set, &v, wt).unwrap().0.decision, Decision::Allow, "{v}");
        }
        assert!(decide(&set, &call("Bash", json!({"command": "true"})), wt).unwrap().1.is_none());
        assert!(decide(&set, &call("Glob", json!({"pattern": "*"})), wt).is_none());
        assert!(decide(&set, &json!({"tool_name": "Bash"}), wt).is_none());
    }

    #[test]
    fn paths_are_worktree_relative() {
        let s = subject_for(&call("Write", json!({"file_path": "/w/a/b.rs"})), Path::new("/w")).unwrap();
        assert_eq!(s.path.as_deref(), Some("a/b.rs"));
        let s = subject_for(&call("NotebookEdit", json!({"notebook_path": "/elsewhere/n.ipynb"})), Path::new("/w")).unwrap();
        assert_eq!(s.path.as_deref(), Some("/elsewhere/n.ipynb"));
    }

    #[test]
    fn quoting_and_settings() {
        assert_eq!(sh_quote("/usr/bin/x"), "/usr/bin/x");
        assert_eq!(sh_quote("/a b/it's"), r"'/a b/it'\''s'");
        let v = claude_settings("x hook claude-pretool --run r1");
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], MATCHER);
        assert_eq!(v["hooks"]["PreToolUse"][0]["hooks"][0]["type"], "command");
    }
}
