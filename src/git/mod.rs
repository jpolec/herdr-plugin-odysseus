//! Git adapter. All invocations are direct argv calls to `git` with
//! timeouts. Safety rules: never force-reset, never force-push, never
//! remove a dirty worktree, repository hooks disabled for orchestrator
//! commits (a repository must not get code execution through us).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::model::{ChangedFile, DiffStat};
use crate::process::run_tool;

pub const ORCH_DIR: &str = ".herdr-orchestrator";
const T: Duration = Duration::from_secs(60);

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let mut argv = vec!["git"];
    argv.extend_from_slice(args);
    run_tool(&argv, cwd, T)
}

/// Repository root for any path inside a repo or linked worktree.
pub fn discover_repo(path: &Path) -> Result<PathBuf> {
    let out = git(path, &["rev-parse", "--show-toplevel"])
        .with_context(|| format!("{} is not inside a git repository", path.display()))?;
    Ok(PathBuf::from(out.trim()))
}

/// The main repository root even when `path` is a linked worktree.
pub fn main_repo_root(path: &Path) -> Result<PathBuf> {
    let common = git(path, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = PathBuf::from(common.trim());
    if common.file_name().is_some_and(|n| n == ".git") {
        Ok(common.parent().unwrap().to_path_buf())
    } else {
        // Bare repository or unusual layout: fall back to toplevel.
        discover_repo(path)
    }
}

pub fn git_common_dir(repo: &Path) -> Result<PathBuf> {
    let out = git(repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    Ok(PathBuf::from(out.trim()))
}

pub fn rev_parse(repo: &Path, rev: &str) -> Result<String> {
    if rev.starts_with('-') {
        bail!("invalid revision {rev:?}");
    }
    let spec = format!("{rev}^{{commit}}");
    Ok(git(repo, &["rev-parse", "--verify", "--quiet", &spec])
        .with_context(|| format!("unknown revision {rev:?}"))?
        .trim()
        .to_string())
}

pub fn current_branch(repo: &Path) -> Result<Option<String>> {
    let out = git(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    Ok(out.ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
}

/// Default base: configured base branch, else current branch, else HEAD.
pub fn default_base(repo: &Path, configured: Option<&str>) -> Result<String> {
    if let Some(b) = configured {
        return Ok(b.to_string());
    }
    Ok(current_branch(repo)?.unwrap_or_else(|| "HEAD".into()))
}

/// Lowercase ASCII slug for branch/path names, max `max` chars.
pub fn slugify(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= max {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "task".into()
    } else {
        out
    }
}

pub fn validate_branch_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty() {
        return Ok(());
    }
    if prefix.starts_with('-') || prefix.starts_with('/') || prefix.contains("..") || prefix.contains("//") {
        bail!("invalid git.branch_prefix {prefix:?}");
    }
    if !prefix.chars().all(|c| c.is_ascii_alphanumeric() || "-_/.".contains(c)) {
        bail!("git.branch_prefix {prefix:?} may only contain ASCII letters, digits, '-', '_', '.', '/'");
    }
    Ok(())
}

/// Validate a branch name with our own conservative rules plus
/// `git check-ref-format --branch`.
pub fn validate_branch_name(repo: &Path, name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.contains("..")
        || name.ends_with('/')
        || name.ends_with(".lock")
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_/.".contains(c))
    {
        bail!("invalid branch name {name:?}");
    }
    git(repo, &["check-ref-format", "--branch", name]).map(|_| ())
}

pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(repo, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok()
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
}

pub fn worktree_list(repo: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut v = vec![];
    let mut cur: Option<WorktreeEntry> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(c) = cur.take() {
                v.push(c);
            }
            cur = Some(WorktreeEntry { path: PathBuf::from(p), head: None, branch: None });
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            if let Some(c) = cur.as_mut() {
                c.head = Some(h.to_string());
            }
        } else if let Some(b) = line.strip_prefix("branch ") {
            if let Some(c) = cur.as_mut() {
                c.branch = Some(b.trim_start_matches("refs/heads/").to_string());
            }
        }
    }
    if let Some(c) = cur {
        v.push(c);
    }
    Ok(v)
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorktreeOutcome {
    Created,
    /// Already existed with the expected branch (idempotent retry).
    Existing,
}

/// Create `path` on new branch `branch` from `base_sha`. Idempotent: if the
/// exact worktree already exists it is reused; any other collision errors.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str, base_sha: &str) -> Result<WorktreeOutcome> {
    validate_branch_name(repo, branch)?;
    for w in worktree_list(repo)? {
        if same_path(&w.path, path) {
            if w.branch.as_deref() == Some(branch) {
                return Ok(WorktreeOutcome::Existing);
            }
            bail!(
                "a worktree already exists at {} on branch {:?}",
                path.display(),
                w.branch
            );
        }
        if w.branch.as_deref() == Some(branch) {
            bail!("branch {branch} is already checked out at {}", w.path.display());
        }
    }
    if path.exists() && std::fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(true) {
        bail!("{} already exists and is not an empty directory", path.display());
    }
    if branch_exists(repo, branch) {
        bail!("branch {branch} already exists; refusing to reuse or reset it");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let p = path.to_string_lossy();
    git(repo, &["worktree", "add", "-b", branch, &p, base_sha])?;
    Ok(WorktreeOutcome::Created)
}

/// Check out an *existing* branch into a new worktree (follow-up work on a
/// finished run whose worktree was removed). Never moves or resets the branch.
pub fn attach_worktree(repo: &Path, path: &Path, branch: &str) -> Result<WorktreeOutcome> {
    validate_branch_name(repo, branch)?;
    for w in worktree_list(repo)? {
        if same_path(&w.path, path) && w.branch.as_deref() == Some(branch) {
            return Ok(WorktreeOutcome::Existing);
        }
        if w.branch.as_deref() == Some(branch) {
            bail!("branch {branch} is already checked out at {}", w.path.display());
        }
    }
    if !branch_exists(repo, branch) {
        bail!("branch {branch} does not exist");
    }
    if path.exists() && std::fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(true) {
        bail!("{} already exists and is not an empty directory", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git(repo, &["worktree", "add", &path.to_string_lossy(), branch])?;
    Ok(WorktreeOutcome::Created)
}

/// Remove a worktree only if it is clean. Never deletes the branch.
pub fn remove_worktree_if_clean(repo: &Path, path: &Path) -> Result<()> {
    if is_dirty(path)? {
        bail!("worktree {} has uncommitted changes; not removing", path.display());
    }
    git(repo, &["worktree", "remove", &path.to_string_lossy()]).map(|_| ())
}

pub fn head_sha(path: &Path) -> Result<String> {
    Ok(git(path, &["rev-parse", "HEAD"])?.trim().to_string())
}

pub fn is_dirty(path: &Path) -> Result<bool> {
    let out = git(path, &["status", "--porcelain", "--untracked-files=all", "--", ".", &format!(":(exclude){ORCH_DIR}")])?;
    Ok(!out.trim().is_empty())
}

/// Make sure `.herdr-orchestrator/` is ignored locally (shared by all
/// worktrees through the common dir). Returns true if the file was changed.
pub fn ensure_excluded(repo: &Path) -> Result<bool> {
    let common = git_common_dir(repo)?;
    let info = common.join("info");
    std::fs::create_dir_all(&info)?;
    let f = info.join("exclude");
    let cur = std::fs::read_to_string(&f).unwrap_or_default();
    let line = format!("/{ORCH_DIR}/");
    if cur.lines().any(|l| l.trim() == line || l.trim() == format!("{ORCH_DIR}/")) {
        return Ok(false);
    }
    let mut new = cur;
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str("# added by herdr-orchestrator (worktrees and agent handoff files)\n");
    new.push_str(&line);
    new.push('\n');
    crate::store::atomic_write(&f, new.as_bytes())?;
    Ok(true)
}

fn parse_numstat_z(s: &str) -> Vec<(u64, u64, String)> {
    // `-z --numstat`: "ins\tdel\tpath\0" or for renames "ins\tdel\t\0old\0new\0".
    let mut v = vec![];
    let mut parts = s.split('\0').peekable();
    while let Some(p) = parts.next() {
        if p.is_empty() {
            continue;
        }
        let mut f = p.splitn(3, '\t');
        let ins = f.next().unwrap_or("0");
        let del = f.next().unwrap_or("0");
        let path = f.next().unwrap_or("");
        let parse = |x: &str| x.parse::<u64>().unwrap_or(0); // "-" for binary
        if path.is_empty() {
            let _old = parts.next();
            let new = parts.next().unwrap_or("").to_string();
            v.push((parse(ins), parse(del), new));
        } else {
            v.push((parse(ins), parse(del), path.to_string()));
        }
    }
    v
}

/// All changes in the worktree relative to `base_sha`: committed,
/// staged, unstaged and untracked. Excludes the orchestrator's own dir.
pub fn changed_files(worktree: &Path, base_sha: &str) -> Result<DiffStat> {
    let exclude = format!(":(exclude){ORCH_DIR}");
    let name_status = git(worktree, &["diff", "--name-status", "-z", "-M", base_sha, "--", ".", &exclude])?;
    let numstat = git(worktree, &["diff", "--numstat", "-z", "-M", base_sha, "--", ".", &exclude])?;
    let nums = parse_numstat_z(&numstat);
    let mut files = vec![];
    let mut it = name_status.split('\0').filter(|s| !s.is_empty());
    while let Some(status) = it.next() {
        let code = status.chars().next().unwrap_or('M');
        let (old_path, path) = if code == 'R' || code == 'C' {
            let old = it.next().unwrap_or("").to_string();
            (Some(old), it.next().unwrap_or("").to_string())
        } else {
            (None, it.next().unwrap_or("").to_string())
        };
        let (ins, del) = nums
            .iter()
            .find(|(_, _, p)| *p == path)
            .map(|(i, d, _)| (*i, *d))
            .unwrap_or((0, 0));
        let change = match code {
            'A' => "added",
            'D' => "deleted",
            'R' => "renamed",
            'C' => "copied",
            'T' => "typechange",
            _ => "modified",
        };
        files.push(ChangedFile { path, change: change.into(), insertions: ins, deletions: del, old_path });
    }
    let untracked = git(worktree, &["ls-files", "--others", "--exclude-standard", "-z", "--", ".", &exclude])?;
    for p in untracked.split('\0').filter(|s| !s.is_empty()) {
        let full = worktree.join(p);
        let lines = match std::fs::symlink_metadata(&full) {
            Ok(md) if md.is_file() && md.len() < 8 * 1024 * 1024 => std::fs::read(&full)
                .map(|b| bytecount_lines(&b))
                .unwrap_or(0),
            _ => 0,
        };
        files.push(ChangedFile { path: p.to_string(), change: "untracked".into(), insertions: lines, deletions: 0, old_path: None });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DiffStat {
        files_changed: files.len(),
        insertions: files.iter().map(|f| f.insertions).sum(),
        deletions: files.iter().map(|f| f.deletions).sum(),
        files,
    })
}

/// Lines a change adds relative to `base_sha` (content-based policy
/// criteria). Bounded: at most `max_lines` lines of at most 1 KiB each;
/// untracked files are read directly (up to 1 MiB).
pub fn added_lines(worktree: &Path, base_sha: &str, f: &ChangedFile, max_lines: usize) -> Vec<String> {
    let clip = |l: &str| l.chars().take(1024).collect::<String>();
    if f.change == "deleted" {
        return vec![];
    }
    if f.change == "untracked" {
        let full = worktree.join(&f.path);
        return match std::fs::symlink_metadata(&full) {
            Ok(md) if md.is_file() && md.len() <= 1024 * 1024 => std::fs::read_to_string(&full).map(|t| t.lines().take(max_lines).map(clip).collect()).unwrap_or_default(),
            _ => vec![],
        };
    }
    let out = match git(worktree, &["diff", "--no-color", "--no-ext-diff", "-U0", base_sha, "--", &f.path]) {
        Ok(o) => o,
        Err(_) => return vec![],
    };
    out.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).take(max_lines).map(|l| clip(&l[1..])).collect()
}

/// Cheap fingerprint that changes when the worktree's content changes:
/// HEAD, per-file line counts of uncommitted changes, untracked files.
pub fn worktree_fingerprint(worktree: &Path) -> String {
    let exclude = format!(":(exclude){ORCH_DIR}");
    let head = git(worktree, &["rev-parse", "HEAD"]).unwrap_or_default();
    let num = git(worktree, &["diff", "HEAD", "--numstat", "--", ".", &exclude]).unwrap_or_default();
    let untracked = git(worktree, &["ls-files", "--others", "--exclude-standard", "-z", "--", ".", &exclude]).unwrap_or_default();
    let sizes: String = untracked
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p}:{}\n", std::fs::metadata(worktree.join(p)).map(|m| m.len()).unwrap_or(0)))
        .collect();
    crate::store::sha256_hex(format!("{head}\n{num}\n{sizes}").as_bytes())
}

/// `true` when `commit` is reachable from `rev` (it has been merged into it).
pub fn is_ancestor(repo: &Path, commit: &str, rev: &str) -> bool {
    git(repo, &["merge-base", "--is-ancestor", commit, rev]).is_ok()
}

fn bytecount_lines(b: &[u8]) -> u64 {
    let n = b.iter().filter(|c| **c == b'\n').count() as u64;
    if !b.is_empty() && !b.ends_with(b"\n") {
        n + 1
    } else {
        n
    }
}

/// Unified diff against base for display (bounded).
pub fn diff_text(worktree: &Path, base_sha: &str, max_bytes: usize) -> Result<String> {
    let exclude = format!(":(exclude){ORCH_DIR}");
    let mut s = git(worktree, &["diff", "--stat", "--patch", "-M", base_sha, "--", ".", &exclude])?;
    let untracked = git(worktree, &["ls-files", "--others", "--exclude-standard", "--", ".", &exclude])?;
    if !untracked.trim().is_empty() {
        s.push_str("\nUntracked files:\n");
        for l in untracked.lines() {
            s.push_str("  + ");
            s.push_str(l);
            s.push('\n');
        }
    }
    if s.len() > max_bytes {
        let mut cut = max_bytes;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n… diff truncated …\n");
    }
    Ok(s)
}

/// Stage everything (except our own dir) and commit. Returns the new SHA,
/// or `None` when there was nothing to commit. Hooks are disabled.
pub fn commit_all(worktree: &Path, message: &str) -> Result<Option<String>> {
    git(worktree, &["add", "-A", "--", "."])?;
    // Never commit the orchestrator's own handoff directory, even when the
    // local exclude entry is missing (manage_exclude: false).
    if worktree.join(ORCH_DIR).exists() {
        let _ = git(worktree, &["reset", "-q", "--", ORCH_DIR]);
    }
    let staged = git(worktree, &["diff", "--cached", "--name-only"])?;
    if staged.trim().is_empty() {
        return Ok(None);
    }
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let hooks = format!("core.hooksPath={null}");
    let mut argv = vec!["git", "-c", &hooks, "-c", "commit.gpgsign=false"];
    // Provide an identity only if the user has none configured.
    let has_identity = git(worktree, &["config", "user.email"]).map(|s| !s.trim().is_empty()).unwrap_or(false);
    if !has_identity {
        argv.extend(["-c", "user.name=herdr-orchestrator", "-c", "user.email=herdr-orchestrator@localhost"]);
    }
    argv.extend(["commit", "--no-verify", "-m", message]);
    run_tool(&argv, worktree, T)?;
    Ok(Some(head_sha(worktree)?))
}

/// Push a branch without force. Never passes `--force` or a `+` refspec.
pub fn push_branch(worktree: &Path, remote: &str, branch: &str) -> Result<()> {
    if remote.starts_with('-') || branch.starts_with('-') || branch.starts_with('+') || branch.contains(':') {
        bail!("refusing suspicious push arguments");
    }
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let hooks = format!("core.hooksPath={null}");
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    run_tool(&["git", "-c", &hooks, "push", "--no-verify", "--set-upstream", remote, &refspec], worktree, Duration::from_secs(300))
        .map(|_| ())
}

pub fn remote_url(repo: &Path, remote: &str) -> Option<String> {
    git(repo, &["remote", "get-url", remote]).ok().map(|s| s.trim().to_string())
}

pub fn log_oneline(worktree: &Path, base_sha: &str, max: usize) -> Result<Vec<String>> {
    let range = format!("{base_sha}..HEAD");
    let n = format!("-n{max}");
    Ok(git(worktree, &["log", "--oneline", &n, &range])?.lines().map(String::from).collect())
}

pub fn git_version() -> Result<String> {
    Ok(run_tool(&["git", "--version"], Path::new("."), Duration::from_secs(10))?.trim().to_string())
}

/// Paths for a run's worktree and branch.
pub fn plan_names(
    repo: &Path,
    worktree_root: &str,
    branch_prefix: &str,
    task_id: &str,
    title: &str,
    variant: Option<char>,
) -> Result<(PathBuf, String)> {
    let slug = slugify(title, 40);
    let suffix = variant.map(|c| format!("-v{}", c.to_ascii_lowercase())).unwrap_or_default();
    let leaf = format!("{task_id}-{slug}{suffix}");
    let root = Path::new(worktree_root);
    let base = if root.is_absolute() { root.to_path_buf() } else { repo.join(root) };
    let path = base.join(&leaf);
    let branch = format!("{branch_prefix}{leaf}");
    if !crate::security::paths::normalize_lexically(&path).starts_with(crate::security::paths::normalize_lexically(&base)) {
        return Err(anyhow!("worktree path escapes worktree root"));
    }
    Ok((path, branch))
}

#[cfg(test)]
pub mod testutil {
    use super::*;

    /// Create a repo with one commit; returns its root.
    pub fn init_repo(dir: &Path) -> PathBuf {
        let root = dir.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let g = |args: &[&str]| run_tool(&[&["git"][..], args].concat(), &root, T).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.com"]);
        g(&["config", "user.name", "T"]);
        std::fs::write(root.join("README.md"), "hello\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "init"]);
        root.canonicalize().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::init_repo;
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Add portfolio exposure clustering!", 40), "add-portfolio-exposure-clustering");
        assert_eq!(slugify("  ", 10), "task");
        assert_eq!(slugify("Zażółć gęślą", 40), "za-g-l");
        assert!(slugify(&"a".repeat(100), 40).len() <= 40);
    }

    #[test]
    fn branch_validation() {
        let d = tempfile::tempdir().unwrap();
        let repo = init_repo(d.path());
        assert!(validate_branch_name(&repo, "herdr/12-x").is_ok());
        for bad in ["-x", "a..b", "a b", "x.lock", "a/", "a~1", "a:b"] {
            assert!(validate_branch_name(&repo, bad).is_err(), "{bad}");
        }
        assert!(validate_branch_prefix("herdr/").is_ok());
        assert!(validate_branch_prefix("../x").is_err());
        assert!(validate_branch_prefix("a b").is_err());
    }

    #[test]
    fn worktree_lifecycle_and_changes() {
        let d = tempfile::tempdir().unwrap();
        let repo = init_repo(d.path());
        let base = rev_parse(&repo, "main").unwrap();
        let (path, branch) = plan_names(&repo, ".herdr-orchestrator/worktrees", "herdr/", "5", "Do thing", None).unwrap();
        assert_eq!(branch, "herdr/5-do-thing");
        assert!(ensure_excluded(&repo).unwrap());
        assert!(!ensure_excluded(&repo).unwrap());
        assert_eq!(add_worktree(&repo, &path, &branch, &base).unwrap(), WorktreeOutcome::Created);
        assert_eq!(add_worktree(&repo, &path, &branch, &base).unwrap(), WorktreeOutcome::Existing);
        // The main repo does not see the worktree dir as untracked.
        assert!(!is_dirty(&repo).unwrap());
        assert!(!is_dirty(&path).unwrap());
        std::fs::write(path.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();
        std::fs::write(path.join("new.txt"), "x\ny\n").unwrap();
        std::fs::remove_file(path.join("README.md")).unwrap();
        std::fs::create_dir_all(path.join(ORCH_DIR).join("out")).unwrap();
        std::fs::write(path.join(ORCH_DIR).join("out/r.json"), "{}").unwrap();
        let ds = changed_files(&path, &base).unwrap();
        let names: Vec<_> = ds.files.iter().map(|f| (f.path.as_str(), f.change.as_str())).collect();
        assert_eq!(names, vec![("README.md", "deleted"), ("new.txt", "untracked"), ("src/lib.rs", "modified")]);
        assert_eq!(ds.insertions, 3);
        assert!(is_dirty(&path).unwrap());
        assert!(remove_worktree_if_clean(&repo, &path).is_err());
        let sha = commit_all(&path, "change").unwrap().unwrap();
        assert_ne!(sha, base);
        assert!(commit_all(&path, "nothing").unwrap().is_none());
        // Committed changes are still reported relative to base.
        assert_eq!(changed_files(&path, &base).unwrap().files_changed, 3);
        // The handoff dir was never committed.
        let tracked = git(&path, &["ls-files"]).unwrap();
        assert!(!tracked.contains(ORCH_DIR));
        // Branch collision refused.
        let (p2, _) = plan_names(&repo, ".herdr-orchestrator/worktrees", "herdr/", "6", "x", None).unwrap();
        assert!(add_worktree(&repo, &p2, &branch, &base).is_err());
        assert_eq!(main_repo_root(&path).unwrap(), repo);
    }

    #[test]
    fn commit_ignores_repo_hooks() {
        let d = tempfile::tempdir().unwrap();
        let repo = init_repo(d.path());
        let hook = repo.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\ntouch HOOK_RAN\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(repo.join("f.txt"), "1").unwrap();
        assert!(commit_all(&repo, "m").unwrap().is_some());
        assert!(!repo.join("HOOK_RAN").exists());
    }

    #[test]
    fn numstat_parsing() {
        let v = parse_numstat_z("1\t2\ta.txt\0-\t-\tbin.png\0" );
        assert_eq!(v, vec![(1, 2, "a.txt".into()), (0, 0, "bin.png".into())]);
        let v = parse_numstat_z("3\t0\t\0old.txt\0new.txt\0");
        assert_eq!(v, vec![(3, 0, "new.txt".into())]);
    }
}
