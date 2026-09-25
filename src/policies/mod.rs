//! Policy engine: ALLOW / REQUIRE_APPROVAL / DENY for orchestrator actions
//! and for file changes observed in a worktree diff.
//!
//! Evaluation collects every matching rule and returns the most restrictive
//! decision (`deny > require_approval > allow`). If nothing matches, the
//! policy's `default_decision` applies. Every matching rule is recorded so
//! the audit trail explains *why*.

pub mod agent_hook;
pub mod command;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

pub use command::{normalize, NormalizedCommand};

pub const DEFAULT_POLICY_YAML: &str = include_str!("../../policies/default.yaml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    RequireApproval,
    Deny,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::RequireApproval => "require_approval",
            Self::Deny => "deny",
        }
    }
    pub fn upper(self) -> &'static str {
        match self {
            Self::Allow => "ALLOW",
            Self::RequireApproval => "REQUIRE_APPROVAL",
            Self::Deny => "DENY",
        }
    }
}

/// What kind of operation is being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Read,
    Write,
    Delete,
    Command,
    GitCommit,
    GitPush,
    GithubPr,
    AgentStart,
    WorktreeCreate,
    Network,
    EnvMutation,
    /// Aggregate check over a whole diff (large deletes etc.).
    DiffSummary,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Delete => "delete",
            Self::Command => "command",
            Self::GitCommit => "git_commit",
            Self::GitPush => "git_push",
            Self::GithubPr => "github_pr",
            Self::AgentStart => "agent_start",
            Self::WorktreeCreate => "worktree_create",
            Self::Network => "network",
            Self::EnvMutation => "env_mutation",
            Self::DiffSummary => "diff_summary",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct RuleMatch {
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default)]
    pub paths: Vec<String>,
    /// Paths that never match this rule even if `paths` does.
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub command_tags: Vec<String>,
    #[serde(default)]
    pub runners: Vec<String>,
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub repos: Vec<String>,
    /// Require the subject to execute through a shell.
    #[serde(default)]
    pub shell: Option<bool>,
    #[serde(default)]
    pub min_lines_changed: Option<u64>,
    #[serde(default)]
    pub min_deleted_files: Option<usize>,
    #[serde(default)]
    pub min_deleted_lines: Option<u64>,
    #[serde(default)]
    pub min_files_changed: Option<usize>,
    /// Regular expressions matched against the lines a change *adds*
    /// (write subjects from the diff gate only), e.g. `#\\[ignore`.
    #[serde(default)]
    pub added_lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "match")]
    pub matcher: RuleMatch,
    /// Shorthand for `match.actions` (as in the documented examples).
    #[serde(default)]
    pub actions: Vec<Action>,
    pub decision: Decision,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    pub version: u32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub default_decision: Option<Decision>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl PolicyFile {
    pub fn parse(yaml: &str) -> Result<Self> {
        let p: PolicyFile = serde_yaml_ng::from_str(yaml).context("parsing policy YAML")?;
        if p.version != 1 {
            bail!("unsupported policy version {} (expected 1)", p.version);
        }
        Ok(p)
    }
}

/// A compiled rule ready for matching.
#[derive(Debug, Clone)]
struct CompiledRule {
    rule: Rule,
    source: String,
    actions: Vec<Action>,
    paths: Option<GlobSet>,
    exclude_paths: Option<GlobSet>,
    branches: Option<GlobSet>,
    repos: Option<GlobSet>,
    added_lines: Option<regex::RegexSet>,
}

/// The union of all loaded policy files.
#[derive(Debug, Clone)]
pub struct PolicySet {
    rules: Vec<CompiledRule>,
    pub default_decision: Decision,
    pub sources: Vec<String>,
}

fn globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(
            globset::GlobBuilder::new(p)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .or_else(|_| Glob::new(p))
                .with_context(|| format!("invalid glob {p:?}"))?,
        );
    }
    Ok(Some(b.build()?))
}

/// Wildcard match where `*` matches any run of characters (including spaces
/// and `/`) and `?` matches one character. Used for command patterns.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Everything a rule may inspect. Fields that are not observable for a given
/// action are `None` and rules requiring them do not match.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Subject {
    pub action: Option<Action>,
    /// Worktree-relative path, `/`-separated.
    pub path: Option<String>,
    pub command: Option<NormalizedCommand>,
    pub runner: Option<String>,
    pub branch: Option<String>,
    pub step_id: Option<String>,
    pub repo: Option<String>,
    pub cwd: Option<PathBuf>,
    pub task_id: Option<String>,
    pub lines_changed: Option<u64>,
    pub deleted_files: Option<usize>,
    pub deleted_lines: Option<u64>,
    pub files_changed: Option<usize>,
    /// Lines the change adds (diff gate, only when a rule needs them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_lines: Option<Vec<String>>,
}

impl Subject {
    pub fn describe(&self) -> String {
        let action = self.action.map(|a| a.as_str()).unwrap_or("?");
        if let Some(c) = &self.command {
            return format!("{action}: {}", c.text);
        }
        if let Some(p) = &self.path {
            return format!("{action}: {p}");
        }
        if let (Some(f), Some(d)) = (self.files_changed, self.deleted_files) {
            return format!("{action}: {f} files changed, {d} deleted");
        }
        action.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MatchedRule {
    pub rule_id: String,
    pub decision: Decision,
    pub reason: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyDecision {
    pub decision: Decision,
    pub subject: String,
    pub matched: Vec<MatchedRule>,
    /// Human-readable explanation of the final decision.
    pub reason: String,
}

impl PolicyDecision {
    pub fn allowed(&self) -> bool {
        self.decision == Decision::Allow
    }
}

impl PolicySet {
    pub fn empty(default_decision: Decision) -> Self {
        Self { rules: vec![], default_decision, sources: vec![] }
    }

    pub fn builtin_default() -> Result<Self> {
        let mut s = Self::empty(Decision::Allow);
        s.add(PolicyFile::parse(DEFAULT_POLICY_YAML)?, "builtin:default")?;
        Ok(s)
    }

    /// Add a policy file. Rules are concatenated; the default decision
    /// becomes the most restrictive seen, so later layers can only tighten it.
    pub fn add(&mut self, file: PolicyFile, source: &str) -> Result<()> {
        let mut ids = std::collections::HashSet::new();
        for r in &file.rules {
            if !ids.insert(r.id.clone()) {
                bail!("{source}: duplicate rule id {:?}", r.id);
            }
        }
        if let Some(d) = file.default_decision {
            if self.sources.is_empty() {
                self.default_decision = d;
            } else {
                self.default_decision = self.default_decision.max(d);
            }
        }
        for rule in file.rules {
            let mut actions = rule.actions.clone();
            actions.extend(rule.matcher.actions.iter().copied());
            let m = &rule.matcher;
            for t in &m.command_tags {
                if !command::TAGS.contains(&t.as_str()) {
                    bail!("{source}: rule {:?} uses unknown command tag {t:?}", rule.id);
                }
            }
            if m.paths.is_empty()
                && m.commands.is_empty()
                && m.command_tags.is_empty()
                && actions.is_empty()
                && m.runners.is_empty()
                && m.min_deleted_files.is_none()
                && m.min_deleted_lines.is_none()
                && m.min_files_changed.is_none()
                && m.added_lines.is_empty()
            {
                bail!("{source}: rule {:?} matches everything; add at least one criterion", rule.id);
            }
            self.rules.push(CompiledRule {
                paths: globset(&m.paths).with_context(|| format!("{source}: rule {}", rule.id))?,
                exclude_paths: globset(&m.exclude_paths)?,
                branches: globset(&m.branches)?,
                repos: globset(&m.repos)?,
                added_lines: if m.added_lines.is_empty() {
                    None
                } else {
                    Some(regex::RegexSet::new(&m.added_lines).with_context(|| format!("{source}: rule {}: invalid added_lines regex", rule.id))?)
                },
                actions,
                source: source.to_string(),
                rule,
            });
        }
        self.sources.push(source.to_string());
        Ok(())
    }

    pub fn load_file(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading policy {}", path.display()))?;
        let f = PolicyFile::parse(&text).with_context(|| format!("in {}", path.display()))?;
        self.add(f, &path.display().to_string())
    }

    /// Whether any rule inspects added lines (so the diff gate only reads
    /// file contents when a policy asks for it).
    pub fn needs_added_lines(&self) -> bool {
        self.rules.iter().any(|r| r.added_lines.is_some())
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub fn evaluate(&self, s: &Subject) -> PolicyDecision {
        let mut matched = vec![];
        for r in &self.rules {
            if rule_matches(r, s) {
                matched.push(MatchedRule {
                    rule_id: r.rule.id.clone(),
                    decision: r.rule.decision,
                    reason: r.rule.reason.clone().or_else(|| r.rule.description.clone()),
                    source: r.source.clone(),
                });
            }
        }
        let decision = matched
            .iter()
            .map(|m| m.decision)
            .max()
            .unwrap_or(self.default_decision);
        let reason = if matched.is_empty() {
            format!("no rule matched; default decision is {}", self.default_decision.as_str())
        } else {
            matched
                .iter()
                .filter(|m| m.decision == decision)
                .map(|m| match &m.reason {
                    Some(r) => format!("{} ({})", m.rule_id, r),
                    None => m.rule_id.clone(),
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        PolicyDecision { decision, subject: s.describe(), matched, reason }
    }
}

fn rule_matches(r: &CompiledRule, s: &Subject) -> bool {
    let m = &r.rule.matcher;
    if !r.actions.is_empty() {
        match s.action {
            Some(a) if r.actions.contains(&a) => {}
            _ => return false,
        }
    }
    if let Some(gs) = &r.paths {
        match &s.path {
            Some(p) if gs.is_match(p) => {}
            _ => return false,
        }
    }
    if let (Some(ex), Some(p)) = (&r.exclude_paths, &s.path) {
        if ex.is_match(p) {
            return false;
        }
    }
    if !m.commands.is_empty() {
        let Some(c) = &s.command else { return false };
        let texts = c.all_texts();
        if !m
            .commands
            .iter()
            .any(|pat| texts.iter().any(|t| wildcard_match(pat, t)))
        {
            return false;
        }
    }
    if !m.command_tags.is_empty() {
        let Some(c) = &s.command else { return false };
        let tags = c.all_tags();
        if !m.command_tags.iter().any(|t| tags.contains(&t.as_str())) {
            return false;
        }
    }
    if let Some(want) = m.shell {
        match &s.command {
            Some(c) if c.shell == want => {}
            _ => return false,
        }
    }
    if !m.runners.is_empty() {
        match &s.runner {
            Some(x) if m.runners.iter().any(|p| wildcard_match(p, x)) => {}
            _ => return false,
        }
    }
    if !m.steps.is_empty() {
        match &s.step_id {
            Some(x) if m.steps.iter().any(|p| wildcard_match(p, x)) => {}
            _ => return false,
        }
    }
    if let Some(gs) = &r.branches {
        match &s.branch {
            Some(b) if gs.is_match(b) => {}
            _ => return false,
        }
    }
    if let Some(gs) = &r.repos {
        match &s.repo {
            Some(b) if gs.is_match(b) => {}
            _ => return false,
        }
    }
    if let Some(min) = m.min_lines_changed {
        if s.lines_changed.unwrap_or(0) < min {
            return false;
        }
    }
    if let Some(min) = m.min_deleted_files {
        if s.deleted_files.unwrap_or(0) < min {
            return false;
        }
    }
    if let Some(min) = m.min_deleted_lines {
        if s.deleted_lines.unwrap_or(0) < min {
            return false;
        }
    }
    if let Some(min) = m.min_files_changed {
        if s.files_changed.unwrap_or(0) < min {
            return false;
        }
    }
    if let Some(set) = &r.added_lines {
        match &s.added_lines {
            Some(lines) if lines.iter().any(|l| set.is_match(l)) => {}
            _ => return false,
        }
    }
    true
}

/// Convenience constructors for common subjects.
impl Subject {
    pub fn command(argv: &[String], shell: bool) -> Self {
        Subject {
            action: Some(Action::Command),
            command: Some(normalize(argv, shell)),
            ..Default::default()
        }
    }
    pub fn file(action: Action, path: &str) -> Self {
        Subject { action: Some(action), path: Some(path.to_string()), ..Default::default() }
    }
    pub fn with_context(
        mut self,
        runner: Option<&str>,
        branch: Option<&str>,
        step: Option<&str>,
        repo: Option<&str>,
    ) -> Self {
        self.runner = runner.map(String::from);
        self.branch = branch.map(String::from);
        self.step_id = step.map(String::from);
        self.repo = repo.map(String::from);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn default_set() -> PolicySet {
        PolicySet::builtin_default().unwrap()
    }

    #[test]
    fn wildcard() {
        assert!(wildcard_match("git push --force*", "git push --force origin"));
        assert!(wildcard_match("*kubectl*prod*", "kubectl --context prod apply"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("git push*", "git pull"));
        assert!(wildcard_match("*", ""));
    }

    #[test]
    fn default_policy_parses() {
        let s = default_set();
        assert!(s.rule_count() > 10);
        assert_eq!(s.default_decision, Decision::Allow);
    }

    #[test]
    fn secret_files_denied() {
        let s = default_set();
        for p in [".env", "app/.env", ".env.local", "config/secrets/db.yml", "id_rsa", "keys/server.pem"] {
            let d = s.evaluate(&Subject::file(Action::Write, p));
            assert_eq!(d.decision, Decision::Deny, "{p}: {}", d.reason);
        }
        let d = s.evaluate(&Subject::file(Action::Write, "src/env.rs"));
        assert_eq!(d.decision, Decision::Allow, "{}", d.reason);
        let d = s.evaluate(&Subject::file(Action::Write, ".env.example"));
        assert_eq!(d.decision, Decision::Allow, ".env.example is a template: {}", d.reason);
    }

    #[test]
    fn sensitive_areas_require_approval() {
        let s = default_set();
        for p in [
            "db/migrations/001_init.sql",
            "infra/main.tf",
            "deploy/k8s/app.yaml",
            ".github/workflows/ci.yml",
            "charts/helm/values.yaml",
            "Dockerfile",
        ] {
            let d = s.evaluate(&Subject::file(Action::Write, p));
            assert_eq!(d.decision, Decision::RequireApproval, "{p}: {}", d.reason);
        }
        let d = s.evaluate(&Subject::file(Action::Write, "src/lib.rs"));
        assert!(d.allowed());
    }

    #[test]
    fn agent_rules_and_tests_are_guarded() {
        let s = default_set();
        for p in [".ai/herdr-orchestrator/policy.yaml", ".ai/skills/x.md", "CLAUDE.md", "sub/AGENTS.md", ".claude/settings.json", ".codex/config.toml", ".cursor/rules/a.mdc", ".mcp.json", "jest.config.ts", "pytest.ini"] {
            let d = s.evaluate(&Subject::file(Action::Write, p));
            assert_eq!(d.decision, Decision::RequireApproval, "{p}: {}", d.reason);
        }
        for p in ["tests/api.rs", "src/foo_test.go", "web/a.spec.ts", "test_x.py"] {
            assert_eq!(s.evaluate(&Subject::file(Action::Delete, p)).decision, Decision::RequireApproval, "{p}");
            assert!(s.evaluate(&Subject::file(Action::Write, p)).allowed(), "editing tests is fine: {p}");
        }
        assert!(s.needs_added_lines());
        let with = |lines: &[&str]| {
            let mut x = Subject::file(Action::Write, "src/lib.rs");
            x.added_lines = Some(v(lines));
            s.evaluate(&x).decision
        };
        for l in ["    #[ignore]", "  it.skip('x', () => {})", "describe.only(\"a\", f)", "@pytest.mark.skip(reason='x')", "    t.Skip(\"flaky\")", "  @Disabled", "xit('a', f)", "self.skipTest('x')"] {
            assert_eq!(with(&[l]), Decision::RequireApproval, "{l}");
        }
        for l in ["model.fit(x, y)", "let skip = 3;", "fn ignore_case() {}", "it('works', f)", "// we no longer #[ignore] this"] {
            // The last one is a comment but still adds the marker text; only
            // the others must stay allowed.
            if l.starts_with("//") {
                continue;
            }
            assert_eq!(with(&[l]), Decision::Allow, "{l}");
        }
        // Without content the rule cannot match (e.g. `policy check --path`).
        assert!(s.evaluate(&Subject::file(Action::Write, "src/lib.rs")).allowed());
    }

    #[test]
    fn commands() {
        let s = default_set();
        let cases: &[(&[&str], Decision)] = &[
            (&["cargo", "test", "--all"], Decision::Allow),
            (&["npm", "test"], Decision::Allow),
            (&["git", "status"], Decision::Allow),
            (&["git", "push", "--force"], Decision::Deny),
            (&["/usr/bin/git", "-C", "x", "push", "-f"], Decision::Deny),
            (&["git", "push", "origin", "herdr/1-x"], Decision::RequireApproval),
            (&["git", "reset", "--hard"], Decision::Deny),
            (&["git", "clean", "-fd"], Decision::Deny),
            (&["rm", "-rf", "/"], Decision::Deny),
            (&["terraform", "destroy"], Decision::Deny),
            (&["terraform", "apply"], Decision::RequireApproval),
            (&["kubectl", "delete", "namespace", "x"], Decision::Deny),
            (&["kubectl", "--context", "prod", "apply", "-f", "x.yaml"], Decision::Deny),
            (&["cat", "/home/u/.aws/credentials"], Decision::Deny),
            (&["curl", "https://example.com"], Decision::RequireApproval),
        ];
        for (argv, want) in cases {
            let d = s.evaluate(&Subject::command(&v(argv), false));
            assert_eq!(d.decision, *want, "{argv:?}: {}", d.reason);
        }
    }

    #[test]
    fn shell_commands_evaluate_each_segment() {
        let s = default_set();
        let d = s.evaluate(&Subject::command(&v(&["cargo test && git push --force"]), true));
        assert_eq!(d.decision, Decision::Deny);
        let d = s.evaluate(&Subject::command(&v(&["cargo test"]), true));
        assert_eq!(d.decision, Decision::RequireApproval, "shell mode is flagged by default: {}", d.reason);
    }

    #[test]
    fn large_deletes_and_lockfiles() {
        let s = default_set();
        let sub = Subject {
            action: Some(Action::DiffSummary),
            deleted_files: Some(60),
            deleted_lines: Some(10),
            files_changed: Some(70),
            ..Default::default()
        };
        assert_eq!(s.evaluate(&sub).decision, Decision::RequireApproval);
        let mut lock = Subject::file(Action::Write, "Cargo.lock");
        lock.lines_changed = Some(10);
        assert_eq!(s.evaluate(&lock).decision, Decision::Allow);
        lock.lines_changed = Some(5000);
        assert_eq!(s.evaluate(&lock).decision, Decision::RequireApproval);
    }

    #[test]
    fn github_pr_and_agent_flags() {
        let s = default_set();
        let d = s.evaluate(&Subject { action: Some(Action::GithubPr), ..Default::default() });
        assert_eq!(d.decision, Decision::RequireApproval);
        let mut a = Subject::command(&v(&["claude", "--dangerously-skip-permissions"]), false);
        a.action = Some(Action::AgentStart);
        assert_eq!(s.evaluate(&a).decision, Decision::Deny);
    }

    #[test]
    fn most_restrictive_wins_and_layers_only_tighten() {
        let mut s = PolicySet::empty(Decision::Allow);
        s.add(
            PolicyFile::parse(
                "version: 1\ndefault_decision: allow\nrules:\n  - id: a\n    match: {paths: ['src/**']}\n    decision: allow\n",
            )
            .unwrap(),
            "g",
        )
        .unwrap();
        s.add(
            PolicyFile::parse(
                "version: 1\ndefault_decision: require_approval\nrules:\n  - id: b\n    match: {paths: ['src/secret/**']}\n    decision: deny\n",
            )
            .unwrap(),
            "p",
        )
        .unwrap();
        assert_eq!(s.default_decision, Decision::RequireApproval);
        let d = s.evaluate(&Subject::file(Action::Write, "src/secret/k"));
        assert_eq!(d.decision, Decision::Deny);
        assert_eq!(d.matched.len(), 2);
        // A later layer that tries to loosen the default cannot.
        s.add(PolicyFile::parse("version: 1\ndefault_decision: allow\n").unwrap(), "x").unwrap();
        assert_eq!(s.default_decision, Decision::RequireApproval);
    }

    #[test]
    fn rejects_bad_policies() {
        assert!(PolicyFile::parse("version: 2\n").is_err());
        assert!(PolicyFile::parse("version: 1\nrules:\n  - id: x\n    decision: nope\n").is_err());
        assert!(PolicyFile::parse("version: 1\nbogus: 1\n").is_err());
        let mut s = PolicySet::empty(Decision::Allow);
        assert!(s
            .add(PolicyFile::parse("version: 1\nrules:\n  - id: x\n    decision: deny\n").unwrap(), "t")
            .is_err());
        let mut s = PolicySet::empty(Decision::Allow);
        assert!(s
            .add(
                PolicyFile::parse("version: 1\nrules:\n  - id: x\n    match: {command_tags: [nope]}\n    decision: deny\n").unwrap(),
                "t"
            )
            .is_err());
    }
}
