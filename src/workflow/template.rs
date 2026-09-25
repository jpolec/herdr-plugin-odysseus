//! Constrained templating: `{{name}}` substitution only. No expressions,
//! filters, loops or code. Unknown variables are errors.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

/// Variables whose values come from humans, repositories or agents and must
/// never reach a command line (argument-injection risk).
pub const UNTRUSTED_VARS: &[&str] = &["task", "task_title", "previous.output", "feedback", "acceptance", "contract"];

pub const KNOWN_VARS: &[&str] = &[
    "task",
    "task_title",
    "task.id",
    "run.id",
    "repo.root",
    "worktree.path",
    "branch",
    "base_sha",
    "previous.output",
    "feedback",
    "output_file",
    "step.id",
    "acceptance",
    "contract",
];

pub fn is_untrusted(var: &str) -> bool {
    UNTRUSTED_VARS.contains(&var) || (var.starts_with("step.") && var.ends_with(".output"))
}

fn is_known(var: &str, step_ids: &[String]) -> bool {
    if KNOWN_VARS.contains(&var) {
        return true;
    }
    if let Some(rest) = var.strip_prefix("step.") {
        if let Some(id) = rest.strip_suffix(".output") {
            return step_ids.iter().any(|s| s == id);
        }
    }
    false
}

/// Extract variable names referenced by a template.
pub fn variables(t: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    let mut rest = t;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            bail!("unterminated '{{{{' in template");
        };
        let name = after[..end].trim();
        if name.is_empty()
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        {
            bail!("invalid template variable {{{{{name}}}}}: only plain names like {{{{task}}}} are allowed");
        }
        out.push(name.to_string());
        rest = &after[end + 2..];
    }
    Ok(out)
}

/// Static validation used by `workflow validate`.
pub fn check(t: &str, step_ids: &[String]) -> Result<Vec<String>> {
    let vars = variables(t)?;
    for v in &vars {
        if !is_known(v, step_ids) {
            bail!("unknown template variable {{{{{v}}}}}");
        }
    }
    Ok(vars)
}

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub vars: BTreeMap<String, String>,
}

impl Context {
    pub fn set(&mut self, k: &str, v: impl Into<String>) -> &mut Self {
        self.vars.insert(k.to_string(), v.into());
        self
    }

    /// Render. Missing values render as empty strings only for outputs of
    /// steps that have not run (e.g. `{{feedback}}` on a first attempt).
    pub fn render(&self, t: &str) -> Result<String> {
        let mut out = String::with_capacity(t.len());
        let mut rest = t;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                bail!("unterminated '{{{{' in template");
            };
            let name = after[..end].trim();
            match self.vars.get(name) {
                Some(v) => out.push_str(v),
                None if name == "feedback" || name == "previous.output" || (name.starts_with("step.") && name.ends_with(".output")) => {}
                None => bail!("template variable {{{{{name}}}}} has no value here"),
            }
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_known_vars() {
        let mut c = Context::default();
        c.set("task", "Fix {{run.id}} bug").set("run.id", "r1");
        // Values are never re-expanded (no template injection).
        assert_eq!(c.render("Do: {{ task }} in {{run.id}}").unwrap(), "Do: Fix {{run.id}} bug in r1");
        assert_eq!(c.render("{{feedback}}x").unwrap(), "x");
        assert!(c.render("{{branch}}").is_err());
    }

    #[test]
    fn rejects_expressions_and_unknowns() {
        let ids = vec!["implement".to_string()];
        assert!(check("{{ task | upper }}", &ids).is_err());
        assert!(check("{{ task + 1 }}", &ids).is_err());
        assert!(check("{{nope}}", &ids).is_err());
        assert!(check("{{step.implement.output}}", &ids).is_ok());
        assert!(check("{{step.other.output}}", &ids).is_err());
        assert!(check("{{task", &ids).is_err());
        assert!(is_untrusted("step.implement.output"));
        assert!(is_untrusted("task"));
        assert!(!is_untrusted("worktree.path"));
    }
}
