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
}
