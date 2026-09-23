//! Path containment. Never trust a path because of its string prefix:
//! resolve symlinks and compare canonical components.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Lexically normalize a path (resolve `.` and `..`) without touching the
/// filesystem. `..` never climbs above the root.
pub fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                let at_root = out.parent().is_none() && out.has_root();
                if at_root {
                    // `/..` is `/`.
                } else if out.as_os_str().is_empty() || out.ends_with("..") || !out.pop() {
                    // Relative path escaping upward: keep the marker so callers
                    // can detect it.
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Classification of a path relative to a root (usually a worktree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Containment {
    /// Inside the root; carries the root-relative path.
    Inside(PathBuf),
    /// Lexically outside the root.
    Outside,
    /// Lexically inside, but a symlink component resolves outside the root.
    SymlinkEscape { link: PathBuf, target: PathBuf },
}

/// Determine whether `rel_or_abs` (relative to `root` if relative) stays in
/// `root`, following symlinks on existing prefixes of the path.
pub fn check_containment(root: &Path, rel_or_abs: &Path) -> Result<Containment> {
    let root_canon = root
        .canonicalize()
        .with_context(|| format!("canonicalizing root {}", root.display()))?;
    let joined = if rel_or_abs.is_absolute() {
        rel_or_abs.to_path_buf()
    } else {
        root_canon.join(rel_or_abs)
    };
    let lex = normalize_lexically(&joined);
    // Absolute inputs may have been given relative to a non-canonical root
    // spelling (e.g. /var vs /private/var on macOS); compare both.
    let rel = match lex.strip_prefix(&root_canon) {
        Ok(r) => r.to_path_buf(),
        Err(_) => match lex.strip_prefix(normalize_lexically(root)) {
            Ok(r) => r.to_path_buf(),
            Err(_) => return Ok(Containment::Outside),
        },
    };
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return Ok(Containment::Outside);
    }
    // Walk each existing prefix; any symlink must resolve inside the root.
    let mut cur = root_canon.clone();
    for c in rel.components() {
        cur.push(c.as_os_str());
        match std::fs::symlink_metadata(&cur) {
            Ok(md) if md.file_type().is_symlink() => {
                let target = match cur.canonicalize() {
                    Ok(t) => t,
                    // Dangling link: resolve its literal target lexically.
                    Err(_) => {
                        let t = std::fs::read_link(&cur)?;
                        let base = cur.parent().unwrap_or(&root_canon);
                        normalize_lexically(&base.join(t))
                    }
                };
                if !target.starts_with(&root_canon) {
                    return Ok(Containment::SymlinkEscape { link: cur, target });
                }
            }
            Ok(_) => {}
            Err(_) => break, // Remaining components don't exist yet.
        }
    }
    Ok(Containment::Inside(rel))
}

/// Error unless `p` is inside `root`.
pub fn ensure_inside(root: &Path, p: &Path) -> Result<PathBuf> {
    match check_containment(root, p)? {
        Containment::Inside(rel) => Ok(rel),
        Containment::Outside => bail!("{} is outside {}", p.display(), root.display()),
        Containment::SymlinkEscape { link, target } => bail!(
            "{} escapes {} through symlink {} -> {}",
            p.display(),
            root.display(),
            link.display(),
            target.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalization() {
        assert_eq!(normalize_lexically(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(normalize_lexically(Path::new("../x")), PathBuf::from("../x"));
        assert_eq!(normalize_lexically(Path::new("/a/../../b")), PathBuf::from("/b"));
    }

    #[test]
    fn detects_traversal_and_prefix_tricks() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("wt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(d.path().join("wt-evil")).unwrap();
        assert!(matches!(check_containment(&root, Path::new("src/a.rs")).unwrap(), Containment::Inside(_)));
        assert_eq!(check_containment(&root, Path::new("../x")).unwrap(), Containment::Outside);
        assert_eq!(check_containment(&root, Path::new("a/../../x")).unwrap(), Containment::Outside);
        // String-prefix trick: /tmp/wt-evil starts with "/tmp/wt".
        assert_eq!(
            check_containment(&root, &d.path().join("wt-evil/f")).unwrap(),
            Containment::Outside
        );
    }

    #[cfg(unix)]
    #[test]
    fn detects_symlink_escape() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("wt");
        std::fs::create_dir_all(&root).unwrap();
        let outside = d.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        std::os::unix::fs::symlink(root.join("src"), root.join("inner")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        assert!(matches!(
            check_containment(&root, Path::new("link/secret")).unwrap(),
            Containment::SymlinkEscape { .. }
        ));
        assert!(matches!(
            check_containment(&root, Path::new("inner/ok.rs")).unwrap(),
            Containment::Inside(_)
        ));
        // Dangling link pointing outside.
        std::os::unix::fs::symlink("/etc/passwd-nope", root.join("dangling")).unwrap();
        assert!(matches!(
            check_containment(&root, Path::new("dangling")).unwrap(),
            Containment::SymlinkEscape { .. }
        ));
        assert!(ensure_inside(&root, Path::new("link/x")).is_err());
    }
}
