//! Configuration: typed schema, layered deterministic merge, path discovery.
//!
//! Precedence (later wins):
//! built-in defaults → global config → project config → workflow defaults
//! → task options → CLI flags.
//!
//! Layers are merged as JSON trees: objects merge key by key, scalars and
//! arrays replace — except `policy.files`, which *concatenates* so a project
//! layer can add policy files but cannot silently drop global ones.

mod duration;

pub use duration::HumanDuration;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::security::{EnvironmentConfig, SandboxKind};

pub const PLUGIN_ID: &str = "jpolec.herdr-orchestrator";
pub const PROJECT_DIR: &str = ".ai/herdr-orchestrator";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub version: u32,
    pub project: ProjectConfig,
    pub defaults: DefaultsConfig,
    pub scheduler: SchedulerConfig,
    pub limits: LimitsConfig,
    pub git: GitConfig,
    pub policy: PolicyConfig,
    pub github: GithubConfig,
    pub environment: EnvironmentConfig,
    pub audit: AuditConfig,
    pub herdr: HerdrConfig,
    pub output: OutputConfig,
    pub sandbox: SandboxConfig,
    pub usage: UsageConfig,
    pub guard: GuardConfig,
    pub epic: EpicConfig,
    /// Runner profile overrides keyed by runner name.
    pub runners: BTreeMap<String, RunnerProfileConfig>,
    /// Named check commands (`tests`, `lint`, `security`) as argv arrays.
    /// An empty list disables the check; absent means auto-detect.
    pub checks: BTreeMap<String, Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            project: Default::default(),
            defaults: Default::default(),
            scheduler: Default::default(),
            limits: Default::default(),
            git: Default::default(),
            policy: Default::default(),
            github: Default::default(),
            environment: Default::default(),
            audit: Default::default(),
            herdr: Default::default(),
            output: Default::default(),
            sandbox: Default::default(),
            usage: Default::default(),
            guard: Default::default(),
            epic: Default::default(),
            runners: BTreeMap::new(),
            checks: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields, default)]
pub struct ProjectConfig {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct DefaultsConfig {
    pub workflow: String,
    pub runner: String,
    pub base_branch: Option<String>,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self { workflow: "implement-review".into(), runner: "claude".into(), base_branch: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SchedulerConfig {
    pub max_parallel_runs: usize,
    pub max_parallel_agents: usize,
    pub poll_interval_ms: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self { max_parallel_runs: 4, max_parallel_agents: 8, poll_interval_ms: 1000 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    pub max_agents_per_run: usize,
    pub max_runtime: HumanDuration,
    /// Upper bound on any single step's `on_failure.max_attempts`.
    pub max_retries: u32,
    pub max_variants: u32,
    /// Advisory only: enforced solely when a runner *reports* cost.
    pub max_cost_usd: Option<f64>,
    /// Advisory only: enforced solely when a runner *reports* tokens.
    pub max_tokens: Option<u64>,
    pub agent_timeout: HumanDuration,
    pub command_timeout: HumanDuration,
    pub agent_startup_timeout: HumanDuration,
    /// `None` = wait forever for a human.
    pub approval_timeout: Option<HumanDuration>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_agents_per_run: 4,
            max_runtime: HumanDuration::from_secs(2 * 3600),
            max_retries: 3,
            max_variants: 4,
            max_cost_usd: None,
            max_tokens: None,
            agent_timeout: HumanDuration::from_secs(45 * 60),
            command_timeout: HumanDuration::from_secs(15 * 60),
            agent_startup_timeout: HumanDuration::from_secs(60),
            approval_timeout: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct GitConfig {
    /// Relative to the repository root unless absolute.
    pub worktree_root: String,
    pub branch_prefix: String,
    /// Commit agent changes at the end of each agent step.
    pub auto_commit: bool,
    pub commit_message: String,
    /// Remove worktrees of succeeded runs automatically (only if clean).
    pub cleanup_on_success: bool,
    pub remote: String,
    /// Add `.herdr-orchestrator/` to `.git/info/exclude`.
    pub manage_exclude: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            worktree_root: ".herdr-orchestrator/worktrees".into(),
            branch_prefix: "herdr/".into(),
            auto_commit: true,
            commit_message: "{{step.id}}: {{task_title}} (herdr-orchestrator {{run.id}})".into(),
            cleanup_on_success: false,
            remote: "origin".into(),
            manage_exclude: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PolicyConfig {
    /// Policy files (relative to the config file that declared them).
    pub files: Vec<String>,
    /// Include the built-in conservative policy.
    pub builtin_default: bool,
    /// Explicit, audited escape hatch: project policy replaces global.
    pub replace_global: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self { files: vec![], builtin_default: true, replace_global: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct GithubConfig {
    pub draft_pr: bool,
    /// Must stay false; present so configs can state it explicitly.
    pub auto_merge: bool,
    pub base: Option<String>,
    pub push_before_pr: bool,
    /// While the engine runs, check open PRs of recent runs for failing CI
    /// and review comments and notify (never starts anything by itself).
    pub watch_prs: bool,
    pub watch_interval: HumanDuration,
    /// Issues, milestones and a Projects board for epics and tasks.
    pub tracker: TrackerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TrackerConfig {
    /// Sync automatically while the engine runs (otherwise `tracker sync`).
    pub auto_sync: bool,
    /// Label put on issues the orchestrator creates.
    pub label: String,
    /// Label that marks issues ready for agents (`tracker import`).
    pub import_label: String,
    /// Projects (v2) board as `owner/number`, e.g. `jpolec/3`.
    pub project: Option<String>,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self { auto_sync: false, label: "herdr-orchestrator".into(), import_label: "agent-ready".into(), project: None }
    }
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self { draft_pr: true, auto_merge: false, base: None, push_before_pr: true, watch_prs: false, watch_interval: HumanDuration::from_secs(10 * 60), tracker: Default::default() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AuditConfig {
    pub hash_chain: bool,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self { hash_chain: true }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HerdrMode {
    /// Use Herdr when its socket answers, otherwise headless fallbacks.
    #[default]
    Auto,
    /// Require Herdr (fail agent steps if unavailable).
    Required,
    /// Never talk to Herdr.
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct HerdrConfig {
    pub mode: HerdrMode,
    /// Explicit socket path (default: Herdr's own resolution order).
    pub socket: Option<String>,
    /// Show Herdr toasts for approvals/blocked agents/completions.
    pub notify: bool,
    /// Close the run's agent panes and its Herdr workspace when the run
    /// succeeds, so no agent stays alive with the task's instructions.
    /// Failed/blocked runs keep theirs for inspection. Files are untouched.
    pub close_panes_on_success: bool,
    /// Interrupt (ctrl+c) an agent whose step timed out.
    pub interrupt_on_timeout: bool,
    /// After a prompt, a "finished" report without a result file is
    /// double-checked for this long (agents may not have started yet).
    pub settle_window: HumanDuration,
}

impl Default for HerdrConfig {
    fn default() -> Self {
        Self { mode: HerdrMode::Auto, socket: None, notify: true, close_panes_on_success: true, interrupt_on_timeout: true, settle_window: HumanDuration::from_secs(30) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct OutputConfig {
    pub excerpt_head_lines: usize,
    pub excerpt_tail_lines: usize,
    pub feedback_max_bytes: usize,
    pub pane_read_lines: u32,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self { excerpt_head_lines: 20, excerpt_tail_lines: 60, feedback_max_bytes: 12_000, pane_read_lines: 200 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields, default)]
pub struct SandboxConfig {
    pub kind: SandboxKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct UsageConfig {
    /// Read token usage of pane agents from their own local session logs
    /// (`~/.claude/projects`, `~/.codex/sessions`). Read-only, local.
    pub session_logs: bool,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self { session_logs: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct GuardConfig {
    /// Start Claude pane agents with a `PreToolUse` hook that checks each
    /// tool call against policy *before* it runs (DENY blocks it).
    pub claude_hook: bool,
    /// After a failed check routes back to an agent, ask a human before
    /// continuing if the new attempt changed only tests or check config.
    pub test_only_retry: bool,
    /// Globs that count as tests or test/check configuration.
    pub test_paths: Vec<String>,
    /// Notice pane agents that are busy without making progress.
    pub watchdog: crate::runners::watchdog::WatchdogConfig,
    /// Remind about `learn` once this many findings piled up (0 = never).
    pub learn_reminder: usize,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            claude_hook: true,
            test_only_retry: true,
            test_paths: [
                "**/tests/**",
                "**/test/**",
                "**/__tests__/**",
                "**/spec/**",
                "**/*_test.*",
                "**/*_spec.*",
                "**/*.test.*",
                "**/*.spec.*",
                "**/test_*.py",
                "**/conftest.py",
                "**/pytest.ini",
                "**/tox.ini",
                "**/setup.cfg",
                "**/jest.config*",
                "**/vitest.config*",
                "**/karma.conf*",
                "**/.mocharc*",
                "**/phpunit.xml*",
                "**/.nycrc*",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            watchdog: Default::default(),
            learn_reminder: 20,
        }
    }
}

/// When a dependency of an epic task counts as done.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DependencyMode {
    /// The dependency's commits are in the dependent's base branch (checked
    /// locally with `git merge-base --is-ancestor`; pull after merging).
    #[default]
    Merged,
    /// The dependent task branches from the dependency's branch and its PR
    /// targets that branch. At most one unmerged dependency.
    Stacked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct EpicConfig {
    /// Where ADRs live, relative to the repository root.
    pub adr_dirs: Vec<String>,
    /// Upper bound on the number of tasks a plan may propose.
    pub max_tasks: usize,
    /// Runner for the planning and conformance agents (default: defaults.runner).
    pub planner_runner: Option<String>,
    /// Workflow for accepted tasks that do not name one.
    pub task_workflow: String,
    pub dependency_mode: DependencyMode,
}

impl Default for EpicConfig {
    fn default() -> Self {
        Self {
            adr_dirs: ["docs/adr", "doc/adr", "docs/decisions", "docs/architecture/decisions", "adr", ".ai/adr"].iter().map(|s| s.to_string()).collect(),
            max_tasks: 12,
            planner_runner: None,
            task_workflow: "epic-task".into(),
            dependency_mode: DependencyMode::Merged,
        }
    }
}

/// Overrides for a runner. See `runners::profiles` for built-ins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields, default)]
pub struct RunnerProfileConfig {
    /// `pane` (interactive agent in a Herdr pane), `headless`, `shell`, `fake`.
    pub mode: Option<String>,
    /// Herdr agent kind for pane mode (`claude`, `codex`, `opencode`…).
    pub kind: Option<String>,
    /// Extra args passed to the agent at launch (pane mode).
    pub args: Option<Vec<String>>,
    /// Headless / shell argv. `{{prompt_file}}` and `{{output_file}}` are
    /// substituted as whole elements; the prompt is also sent on stdin.
    pub command: Option<Vec<String>>,
    /// Extra env var names this runner may inherit (e.g. `ANTHROPIC_API_KEY`).
    pub env_inherit: Option<Vec<String>>,
}

// ---------------------------------------------------------------- paths --

/// Resolved filesystem locations.
#[derive(Debug, Clone)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    pub plugin_root: Option<PathBuf>,
    /// Where the value came from, for `doctor`.
    pub state_source: String,
}

fn herdr_app_state_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("XDG_STATE_HOME") {
        return Some(PathBuf::from(d).join("herdr"));
    }
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/state/herdr"))
}

fn herdr_app_config_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(d).join("herdr"));
    }
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config/herdr"))
}

impl Paths {
    /// Resolve paths. Order: explicit override env → Herdr-provided plugin env
    /// → the location Herdr uses for this plugin id (mirrors Herdr's
    /// `plugin_paths.rs`; isolated here and checked by `doctor`).
    pub fn discover() -> Result<Self> {
        let (state_dir, state_source) = if let Ok(d) = std::env::var("HERDR_ORCH_STATE_DIR") {
            (PathBuf::from(d), "HERDR_ORCH_STATE_DIR".to_string())
        } else if let Ok(d) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
            (PathBuf::from(d), "HERDR_PLUGIN_STATE_DIR".to_string())
        } else {
            let base = herdr_app_state_dir().context("cannot determine state directory (HOME unset)")?;
            (base.join("plugins").join(PLUGIN_ID), "derived from Herdr layout".to_string())
        };
        let config_dir = if let Ok(d) = std::env::var("HERDR_ORCH_CONFIG_DIR") {
            PathBuf::from(d)
        } else if let Ok(d) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
            PathBuf::from(d)
        } else {
            herdr_app_config_dir()
                .context("cannot determine config directory (HOME unset)")?
                .join("plugins/config")
                .join(PLUGIN_ID)
        };
        let plugin_root = std::env::var("HERDR_PLUGIN_ROOT").ok().map(PathBuf::from);
        Ok(Self { state_dir, config_dir, plugin_root, state_source })
    }

    pub fn for_test(root: &Path) -> Self {
        Self {
            state_dir: root.join("state"),
            config_dir: root.join("config"),
            plugin_root: None,
            state_source: "test".into(),
        }
    }

    pub fn global_config_file(&self) -> PathBuf {
        self.config_dir.join("config.yaml")
    }
}

// ---------------------------------------------------------------- loading --

/// A config layer as it was loaded, kept for `config show --layers`.
#[derive(Debug, Clone)]
pub struct Layer {
    pub name: String,
    pub path: Option<PathBuf>,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub layers: Vec<Layer>,
    /// Policy files resolved to absolute paths, in load order.
    pub policy_files: Vec<PathBuf>,
    pub project_dir: Option<PathBuf>,
}

pub fn deep_merge(base: &mut Value, overlay: Value, path: &str) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                let child = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                match b.get_mut(&k) {
                    Some(existing) => {
                        if child == "policy.files" {
                            if let (Value::Array(a), Value::Array(n)) = (existing, v.clone()) {
                                a.extend(n);
                                continue;
                            }
                        }
                        deep_merge(b.get_mut(&k).unwrap(), v, &child)
                    }
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

fn read_yaml_value(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let v: Value = serde_yaml_ng::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if !v.is_object() {
        bail!("{} must contain a YAML mapping", path.display());
    }
    Ok(v)
}

/// Resolve relative `policy.files` of one layer against its directory.
fn absolutize_policy_files(v: &mut Value, base_dir: &Path) {
    if let Some(files) = v.pointer_mut("/policy/files").and_then(Value::as_array_mut) {
        for f in files.iter_mut() {
            if let Some(s) = f.as_str() {
                let p = Path::new(s);
                if p.is_relative() {
                    *f = Value::String(base_dir.join(p).display().to_string());
                }
            }
        }
    }
}

/// Load configuration for `repo_root` (may be `None` outside a repository).
pub fn load(paths: &Paths, repo_root: Option<&Path>, cli_overrides: Option<Value>) -> Result<LoadedConfig> {
    let mut layers = vec![Layer {
        name: "builtin".into(),
        path: None,
        value: serde_json::to_value(Config::default())?,
    }];
    let global = paths.global_config_file();
    if global.exists() {
        let mut v = read_yaml_value(&global)?;
        absolutize_policy_files(&mut v, &paths.config_dir);
        layers.push(Layer { name: "global".into(), path: Some(global), value: v });
    }
    let mut project_dir = None;
    if let Some(root) = repo_root {
        let dir = root.join(PROJECT_DIR);
        for name in ["config.yaml", "config.yml"] {
            let p = dir.join(name);
            if p.exists() {
                let mut v = read_yaml_value(&p)?;
                // Project policy paths are relative to the repository root,
                // matching the documented example (`policies/default.yaml`).
                absolutize_policy_files(&mut v, root);
                if v.pointer("/policy/replace_global").and_then(Value::as_bool) == Some(true) {
                    // Drop earlier policy files; recorded in the layer list.
                    for l in layers.iter_mut() {
                        if let Some(files) = l.value.pointer_mut("/policy/files") {
                            *files = Value::Array(vec![]);
                        }
                    }
                }
                layers.push(Layer { name: "project".into(), path: Some(p), value: v });
                break;
            }
        }
        project_dir = Some(dir);
    }
    if let Some(o) = cli_overrides {
        layers.push(Layer { name: "cli".into(), path: None, value: o });
    }
    let mut merged = Value::Object(Default::default());
    for l in &layers {
        deep_merge(&mut merged, l.value.clone(), "");
    }
    let config: Config = serde_json::from_value(merged).context("invalid configuration")?;
    validate(&config)?;
    let mut policy_files = vec![];
    for f in &config.policy.files {
        let p = PathBuf::from(f);
        if !policy_files.contains(&p) {
            policy_files.push(p);
        }
    }
    Ok(LoadedConfig { config, layers, policy_files, project_dir })
}

pub fn validate(c: &Config) -> Result<()> {
    if c.version != 1 {
        bail!("unsupported config version {}", c.version);
    }
    if c.github.auto_merge {
        bail!("github.auto_merge: true is not supported; herdr-orchestrator never merges automatically");
    }
    if c.scheduler.max_parallel_runs == 0 || c.scheduler.max_parallel_agents == 0 {
        bail!("scheduler limits must be at least 1");
    }
    if c.limits.max_agents_per_run == 0 {
        bail!("limits.max_agents_per_run must be at least 1");
    }
    crate::git::validate_branch_prefix(&c.git.branch_prefix)?;
    if c.epic.max_tasks == 0 || c.epic.max_tasks > 50 {
        bail!("epic.max_tasks must be between 1 and 50");
    }
    if Path::new(&c.git.worktree_root).components().any(|p| matches!(p, std::path::Component::ParentDir)) {
        bail!("git.worktree_root must not contain '..'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_and_policy_concat() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(d.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.global_config_file(),
            "scheduler:\n  max_parallel_runs: 2\npolicy:\n  files: [global.yaml]\ndefaults:\n  runner: codex\n",
        )
        .unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join(PROJECT_DIR)).unwrap();
        std::fs::write(
            repo.join(PROJECT_DIR).join("config.yaml"),
            "version: 1\nscheduler:\n  max_parallel_agents: 3\npolicy:\n  files: [.ai/herdr-orchestrator/policy.yaml]\ndefaults:\n  runner: claude\n",
        )
        .unwrap();
        let cli = serde_json::json!({"scheduler": {"max_parallel_runs": 7}});
        let l = load(&paths, Some(&repo), Some(cli)).unwrap();
        assert_eq!(l.config.scheduler.max_parallel_runs, 7); // cli
        assert_eq!(l.config.scheduler.max_parallel_agents, 3); // project
        assert_eq!(l.config.defaults.runner, "claude"); // project over global
        assert_eq!(l.config.limits.max_retries, 3); // builtin
        assert_eq!(l.policy_files.len(), 2);
        assert!(l.policy_files[0].ends_with("config/global.yaml"));
        assert!(l.policy_files[1].ends_with("repo/.ai/herdr-orchestrator/policy.yaml"));
        assert_eq!(l.layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["builtin", "global", "project", "cli"]);
    }

    #[test]
    fn replace_global_drops_global_policy_files() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(d.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.global_config_file(), "policy:\n  files: [g.yaml]\n").unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join(PROJECT_DIR)).unwrap();
        std::fs::write(repo.join(PROJECT_DIR).join("config.yaml"), "policy:\n  replace_global: true\n  files: [p.yaml]\n").unwrap();
        let l = load(&paths, Some(&repo), None).unwrap();
        assert_eq!(l.policy_files.len(), 1);
        assert!(l.policy_files[0].ends_with("p.yaml"));
    }

    #[test]
    fn rejects_unknown_keys_and_auto_merge() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(d.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.global_config_file(), "schedulr: {}\n").unwrap();
        assert!(load(&paths, None, None).is_err());
        std::fs::write(paths.global_config_file(), "github:\n  auto_merge: true\n").unwrap();
        let e = load(&paths, None, None).unwrap_err();
        assert!(format!("{e:#}").contains("auto_merge"));
    }

    #[test]
    fn durations_parse() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(d.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.global_config_file(), "limits:\n  max_runtime: 1h30m\n  command_timeout: 90\n").unwrap();
        let l = load(&paths, None, None).unwrap();
        assert_eq!(l.config.limits.max_runtime.as_secs(), 5400);
        assert_eq!(l.config.limits.command_timeout.as_secs(), 90);
    }
}
