//! Verification commands: named-check resolution and bounded output
//! excerpts for retry feedback.

use std::collections::BTreeMap;
use std::path::Path;

use crate::process::which;

pub const NAMED_CHECKS: &[&str] = &["tests", "lint", "security"];

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCheck {
    pub argv: Vec<String>,
    /// `config` or `detected: <reason>`.
    pub source: String,
}

fn v(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| x.to_string()).collect()
}

fn package_json_script(root: &Path, name: &str) -> bool {
    std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|j| j.get("scripts").and_then(|x| x.get(name)).cloned())
        .and_then(|x| x.as_str().map(|s| !s.contains("no test specified")))
        .unwrap_or(false)
}

fn js_runner(root: &Path) -> &'static str {
    if root.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if root.join("yarn.lock").exists() {
        "yarn"
    } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
        "bun"
    } else {
        "npm"
    }
}

fn makefile_has(root: &Path, target: &str) -> bool {
    std::fs::read_to_string(root.join("Makefile"))
        .map(|s| s.lines().any(|l| l.starts_with(&format!("{target}:"))))
        .unwrap_or(false)
}

/// Resolve a named check. Project/global config (`checks.<name>`) wins over
/// detection. `None` means nothing applicable was found.
pub fn resolve(name: &str, root: &Path, configured: &BTreeMap<String, Vec<String>>) -> Option<ResolvedCheck> {
    if let Some(argv) = configured.get(name) {
        if !argv.is_empty() {
            return Some(ResolvedCheck { argv: argv.clone(), source: "config".into() });
        }
        return None; // explicitly disabled with []
    }
    let d = |argv: Vec<String>, why: &str| Some(ResolvedCheck { argv, source: format!("detected: {why}") });
    let cargo = root.join("Cargo.toml").exists();
    let node = root.join("package.json").exists();
    let python = root.join("pyproject.toml").exists() || root.join("pytest.ini").exists() || root.join("setup.cfg").exists() || root.join("tox.ini").exists();
    let go = root.join("go.mod").exists();
    match name {
        "tests" => {
            if cargo {
                return d(v(&["cargo", "test", "--all"]), "Cargo.toml");
            }
            if node && package_json_script(root, "test") {
                let r = js_runner(root);
                return d(if r == "npm" { v(&["npm", "test", "--silent"]) } else { v(&[r, "test"]) }, "package.json scripts.test");
            }
            if go {
                return d(v(&["go", "test", "./..."]), "go.mod");
            }
            if python {
                return d(v(&["python3", "-m", "pytest", "-q"]), "python project");
            }
            if makefile_has(root, "test") {
                return d(v(&["make", "test"]), "Makefile test target");
            }
            None
        }
        "lint" => {
            if cargo {
                return d(v(&["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]), "Cargo.toml");
            }
            if node && package_json_script(root, "lint") {
                let r = js_runner(root);
                return d(v(&[r, "run", "lint"]), "package.json scripts.lint");
            }
            if go {
                return d(v(&["go", "vet", "./..."]), "go.mod");
            }
            if python && which("ruff").is_some() {
                return d(v(&["ruff", "check", "."]), "python + ruff");
            }
            if makefile_has(root, "lint") {
                return d(v(&["make", "lint"]), "Makefile lint target");
            }
            None
        }
        "security" => {
            if cargo && which("cargo-audit").is_some() {
                return d(v(&["cargo", "audit"]), "Cargo.toml + cargo-audit");
            }
            if node && root.join("package-lock.json").exists() {
                return d(v(&["npm", "audit", "--audit-level=high"]), "package-lock.json");
            }
            if python && which("pip-audit").is_some() {
                return d(v(&["pip-audit"]), "python + pip-audit");
            }
            if go && which("govulncheck").is_some() {
                return d(v(&["govulncheck", "./..."]), "go.mod + govulncheck");
            }
            None
        }
        _ => None,
    }
}

/// Intelligent truncation for retry feedback: first N lines, last M lines
/// and lines that look like errors from the middle, capped at `max_bytes`.
pub fn excerpt(text: &str, head: usize, tail: usize, max_bytes: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    if lines.len() <= head + tail {
        out = lines.join("\n");
    } else {
        let is_err = |l: &str| {
            let l = l.to_ascii_lowercase();
            l.contains("error") || l.contains("failed") || l.contains("panicked") || l.contains("assert") || l.contains("exception") || l.contains("traceback")
        };
        out.push_str(&lines[..head].join("\n"));
        let middle = &lines[head..lines.len() - tail];
        let errs: Vec<&str> = middle.iter().copied().filter(|l| is_err(l)).take(40).collect();
        out.push_str(&format!("\n… [{} lines omitted", middle.len()));
        if !errs.is_empty() {
            out.push_str(&format!("; {} error-looking lines kept] …\n", errs.len()));
            out.push_str(&errs.join("\n"));
            out.push_str("\n…\n");
        } else {
            out.push_str("] …\n");
        }
        out.push_str(&lines[lines.len() - tail..].join("\n"));
    }
    if out.len() > max_bytes {
        let keep_head = max_bytes / 3;
        let keep_tail = max_bytes - keep_head;
        let mut h = keep_head.min(out.len());
        while !out.is_char_boundary(h) {
            h -= 1;
        }
        let mut t = out.len().saturating_sub(keep_tail);
        while !out.is_char_boundary(t) {
            t += 1;
        }
        out = format!("{}\n… [truncated] …\n{}", &out[..h], &out[t..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_and_config_precedence() {
        let d = tempfile::tempdir().unwrap();
        assert!(resolve("tests", d.path(), &BTreeMap::new()).is_none());
        std::fs::write(d.path().join("Cargo.toml"), "[package]").unwrap();
        let r = resolve("tests", d.path(), &BTreeMap::new()).unwrap();
        assert_eq!(r.argv, v(&["cargo", "test", "--all"]));
        let mut cfg = BTreeMap::new();
        cfg.insert("tests".to_string(), v(&["just", "test"]));
        assert_eq!(resolve("tests", d.path(), &cfg).unwrap().source, "config");
        cfg.insert("tests".to_string(), vec![]);
        assert!(resolve("tests", d.path(), &cfg).is_none());
    }

    #[test]
    fn node_detection() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#).unwrap();
        assert!(resolve("tests", d.path(), &BTreeMap::new()).is_none());
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"test":"vitest","lint":"eslint ."}}"#).unwrap();
        std::fs::write(d.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(resolve("tests", d.path(), &BTreeMap::new()).unwrap().argv, v(&["pnpm", "test"]));
        assert_eq!(resolve("lint", d.path(), &BTreeMap::new()).unwrap().argv, v(&["pnpm", "run", "lint"]));
    }

    #[test]
    fn excerpt_keeps_head_tail_and_errors() {
        let mut t = String::new();
        for i in 0..500 {
            if i == 250 {
                t.push_str("error[E0308]: mismatched types\n");
            } else {
                t.push_str(&format!("line {i}\n"));
            }
        }
        let e = excerpt(&t, 5, 5, 10_000);
        assert!(e.contains("line 0"));
        assert!(e.contains("line 499"));
        assert!(e.contains("mismatched types"));
        assert!(!e.contains("line 100\n"));
        let small = excerpt(&t, 200, 200, 300);
        assert!(small.len() < 400);
    }
}
