//! GitHub integration through the user's authenticated `gh` CLI.
//! Draft PRs only by default; nothing merges or deploys automatically.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::process::{run, Spec};

#[derive(Debug, Clone)]
pub struct Gh {
    pub bin: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrRef {
    pub number: Option<u64>,
    pub url: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default, rename = "isDraft")]
    pub is_draft: Option<bool>,
}

impl Default for Gh {
    fn default() -> Self {
        Self::from_env()
    }
}

impl Gh {
    /// `HERDR_ORCH_GH_BIN` overrides the binary (used by tests with a fake gh).
    pub fn from_env() -> Self {
        let bin = std::env::var("HERDR_ORCH_GH_BIN").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("gh"));
        Self { bin }
    }

    pub fn available(&self) -> bool {
        crate::process::which(&self.bin.to_string_lossy()).is_some()
    }

    fn call(&self, cwd: &Path, args: &[&str], timeout: Duration) -> Result<String> {
        let mut argv = vec![self.bin.to_string_lossy().to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let mut env = crate::process::inherited_minimal_env();
        // Never let gh open an interactive prompt or pager.
        env.insert("GH_PROMPT_DISABLED".into(), "1".into());
        env.insert("GH_PAGER".into(), "cat".into());
        env.insert("NO_COLOR".into(), "1".into());
        let out = run(&Spec::new(argv, cwd).env(env).timeout(timeout), None)?;
        if out.timed_out {
            bail!("gh {} timed out", args.first().unwrap_or(&""));
        }
        if !out.success() {
            bail!("gh {} failed: {}", args.join(" "), out.stderr_str().trim());
        }
        Ok(out.stdout_str())
    }

    pub fn version(&self) -> Result<String> {
        Ok(self.call(Path::new("."), &["--version"], Duration::from_secs(10))?.lines().next().unwrap_or("").to_string())
    }

    pub fn issue_view(&self, repo_dir: &Path, number: u64) -> Result<Issue> {
        let n = number.to_string();
        let out = self.call(repo_dir, &["issue", "view", &n, "--json", "number,title,body,url"], Duration::from_secs(60))?;
        serde_json::from_str(&out).context("parsing gh issue view output")
    }

    /// Existing PR whose head is `branch` (idempotency after restarts).
    pub fn pr_for_branch(&self, repo_dir: &Path, branch: &str) -> Result<Option<PrRef>> {
        let out = self.call(
            repo_dir,
            &["pr", "list", "--head", branch, "--state", "all", "--json", "number,url,state,isDraft", "--limit", "1"],
            Duration::from_secs(60),
        )?;
        let v: Vec<PrRef> = serde_json::from_str(out.trim()).context("parsing gh pr list output")?;
        Ok(v.into_iter().next())
    }

    pub fn pr_create(
        &self,
        repo_dir: &Path,
        base: &str,
        head: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<String> {
        for a in [base, head] {
            if a.starts_with('-') {
                bail!("refusing suspicious PR argument {a:?}");
            }
        }
        let mut args = vec!["pr", "create", "--base", base, "--head", head, "--title", title, "--body", body];
        if draft {
            args.push("--draft");
        }
        let out = self.call(repo_dir, &args, Duration::from_secs(120))?;
        let url = out
            .lines()
            .rev()
            .find(|l| l.trim_start().starts_with("https://"))
            .map(|s| s.trim().to_string())
            .context("gh pr create did not print a PR URL")?;
        Ok(url)
    }
}

/// Review comments and CI results of a PR, as feedback for an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PrFeedback {
    pub url: String,
    pub state: String,
    pub failing_checks: Vec<String>,
    pub pending_checks: usize,
    pub comments: usize,
    /// Markdown-ish text for the agent (untrusted; prompt-only).
    pub text: String,
}

impl PrFeedback {
    pub fn actionable(&self) -> bool {
        !self.failing_checks.is_empty() || self.comments > 0
    }
    /// Fingerprint to notice *new* feedback.
    pub fn fingerprint(&self) -> String {
        crate::store::sha256_hex(self.text.as_bytes())[..16].to_string()
    }
}

/// `https://github.com/<owner>/<repo>/pull/<n>` → (owner/repo, n).
pub fn parse_pr_url(url: &str) -> Option<(String, u64)> {
    let rest = url.trim().trim_end_matches('/').split("://").nth(1)?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 5 || parts[3] != "pull" {
        return None;
    }
    let valid = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if !valid(parts[1]) || !valid(parts[2]) {
        return None;
    }
    Some((format!("{}/{}", parts[1], parts[2]), parts[4].parse().ok()?))
}

fn failed(conclusion: &str) -> bool {
    matches!(conclusion.to_ascii_uppercase().as_str(), "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE")
}

/// Build feedback from `gh pr view --json …` output and the PR's inline
/// review comments (`gh api …/pulls/<n>/comments`).
pub fn feedback_from_json(url: &str, view: &serde_json::Value, inline: &serde_json::Value) -> PrFeedback {
    let mut f = PrFeedback { url: url.to_string(), state: view["state"].as_str().unwrap_or("").to_string(), ..Default::default() };
    let mut text = String::new();
    for c in view["statusCheckRollup"].as_array().into_iter().flatten() {
        let name = c["name"].as_str().or(c["context"].as_str()).unwrap_or("check");
        let concl = c["conclusion"].as_str().filter(|s| !s.is_empty()).or(c["state"].as_str()).unwrap_or("");
        if failed(concl) {
            let link = c["detailsUrl"].as_str().or(c["targetUrl"].as_str()).unwrap_or("");
            f.failing_checks.push(name.to_string());
            text.push_str(&format!("- CI check `{name}` {}: {link}\n", concl.to_ascii_lowercase()));
        } else if concl.is_empty() || matches!(concl.to_ascii_uppercase().as_str(), "PENDING" | "IN_PROGRESS" | "QUEUED" | "EXPECTED") {
            f.pending_checks += 1;
        }
    }
    let author = |v: &serde_json::Value| v["author"]["login"].as_str().or(v["user"]["login"].as_str()).unwrap_or("someone").to_string();
    for r in view["reviews"].as_array().into_iter().flatten() {
        let body = r["body"].as_str().unwrap_or("").trim();
        let state = r["state"].as_str().unwrap_or("");
        if !body.is_empty() || state == "CHANGES_REQUESTED" {
            f.comments += 1;
            text.push_str(&format!("- Review by {} ({}): {}\n", author(r), state.to_ascii_lowercase().replace('_', " "), body));
        }
    }
    for c in view["comments"].as_array().into_iter().flatten() {
        let body = c["body"].as_str().unwrap_or("").trim();
        if !body.is_empty() {
            f.comments += 1;
            text.push_str(&format!("- Comment by {}: {}\n", author(c), body));
        }
    }
    for c in inline.as_array().into_iter().flatten() {
        let body = c["body"].as_str().unwrap_or("").trim();
        if !body.is_empty() {
            f.comments += 1;
            let line = c["line"].as_u64().or(c["original_line"].as_u64()).map(|l| format!(":{l}")).unwrap_or_default();
            text.push_str(&format!("- {}{} — {}: {}\n", c["path"].as_str().unwrap_or("?"), line, author(c), body));
        }
    }
    f.text = text;
    f
}

impl Gh {
    pub fn pr_feedback(&self, repo_dir: &Path, url: &str) -> Result<PrFeedback> {
        let (slug, n) = parse_pr_url(url).with_context(|| format!("not a GitHub pull request URL: {url}"))?;
        let view = self.call(repo_dir, &["pr", "view", url, "--json", "number,url,state,reviews,comments,statusCheckRollup"], Duration::from_secs(60))?;
        let view: serde_json::Value = serde_json::from_str(view.trim()).context("parsing gh pr view output")?;
        let inline = self
            .call(repo_dir, &["api", &format!("repos/{slug}/pulls/{n}/comments"), "--paginate"], Duration::from_secs(60))
            .ok()
            .and_then(|s| serde_json::from_str(s.trim()).ok())
            .unwrap_or(serde_json::Value::Null);
        Ok(feedback_from_json(url, &view, &inline))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn fake_gh_roundtrip() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("gh");
        std::fs::write(
            &bin,
            "#!/bin/sh\ncase \"$1 $2\" in\n  'issue view') echo '{\"number\":3,\"title\":\"Bug\",\"body\":\"b\",\"url\":\"https://github.com/o/r/issues/3\"}';;\n  'pr list') echo '[]';;\n  'pr create') echo 'https://github.com/o/r/pull/9';;\n  *) exit 1;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let gh = Gh { bin };
        assert_eq!(gh.issue_view(d.path(), 3).unwrap().title, "Bug");
        assert!(gh.pr_for_branch(d.path(), "b").unwrap().is_none());
        assert_eq!(gh.pr_create(d.path(), "main", "b", "t", "body", true).unwrap(), "https://github.com/o/r/pull/9");
        assert!(gh.pr_create(d.path(), "--x", "b", "t", "body", true).is_err());
    }

    #[test]
    fn pr_feedback_parsing() {
        assert_eq!(parse_pr_url("https://github.com/o/r/pull/7"), Some(("o/r".into(), 7)));
        assert_eq!(parse_pr_url("https://github.com/o/r/issues/7"), None);
        assert_eq!(parse_pr_url("https://github.com/o;x/r/pull/7"), None);
        let view = serde_json::json!({
            "state": "OPEN",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "name": "test", "conclusion": "FAILURE", "detailsUrl": "https://ci/1"},
                {"__typename": "CheckRun", "name": "lint", "conclusion": "SUCCESS"},
                {"__typename": "StatusContext", "context": "deploy", "state": "PENDING"}
            ],
            "reviews": [{"author": {"login": "ana"}, "state": "CHANGES_REQUESTED", "body": "Handle the 0 case."}, {"author": {"login": "bo"}, "state": "APPROVED", "body": ""}],
            "comments": [{"author": {"login": "cy"}, "body": "Also update the docs"}]
        });
        let inline = serde_json::json!([{"path": "src/a.rs", "line": 12, "user": {"login": "ana"}, "body": "off by one"}]);
        let f = feedback_from_json("https://github.com/o/r/pull/7", &view, &inline);
        assert_eq!(f.failing_checks, vec!["test"]);
        assert_eq!(f.pending_checks, 1);
        assert_eq!(f.comments, 3);
        assert!(f.actionable());
        assert!(f.text.contains("src/a.rs:12 — ana: off by one"));
        assert!(f.text.contains("changes requested"));
        let quiet = feedback_from_json("u", &serde_json::json!({"state": "OPEN"}), &serde_json::Value::Null);
        assert!(!quiet.actionable());
    }
}
