//! Discovery of workflows and skills.
//!
//! Lookup order (later overrides earlier by name):
//! built-in → global config dir → project `.ai/herdr-orchestrator/`.
//! Skills are additionally read from `.ai/skills/` (Cezar-compatible).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::Workflow;
use crate::config::PROJECT_DIR;

pub const BUILTIN_WORKFLOWS: &[(&str, &str)] = &[
    ("quick-task", include_str!("../../workflows/quick-task.yaml")),
    ("implement-review", include_str!("../../workflows/implement-review.yaml")),
    ("secure-change", include_str!("../../workflows/secure-change.yaml")),
    ("variant-review", include_str!("../../workflows/variant-review.yaml")),
    ("dual-review", include_str!("../../workflows/dual-review.yaml")),
    ("contract-first", include_str!("../../workflows/contract-first.yaml")),
    ("eval-task", include_str!("../../workflows/eval-task.yaml")),
    ("epic-task", include_str!("../../workflows/epic-task.yaml")),
    ("epic-plan", include_str!("../../workflows/epic-plan.yaml")),
    ("epic-conformance", include_str!("../../workflows/epic-conformance.yaml")),
];

pub const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("implementation", include_str!("../../skills/implementation.md")),
    ("code-review", include_str!("../../skills/code-review.md")),
    ("security-review", include_str!("../../skills/security-review.md")),
    ("adr-planning", include_str!("../../skills/adr-planning.md")),
    ("acceptance-review", include_str!("../../skills/acceptance-review.md")),
    ("adr-conformance", include_str!("../../skills/adr-conformance.md")),
    ("contract-writing", include_str!("../../skills/contract-writing.md")),
];

#[derive(Debug, Clone)]
pub struct WorkflowSource {
    pub name: String,
    pub origin: String,
    pub yaml: String,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub global_dir: PathBuf,
    pub repo_root: Option<PathBuf>,
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn yaml_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yaml") | Some("yml")))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

impl Catalog {
    pub fn new(global_config_dir: &Path, repo_root: Option<&Path>) -> Self {
        Self { global_dir: global_config_dir.to_path_buf(), repo_root: repo_root.map(Path::to_path_buf) }
    }

    fn workflow_dirs(&self) -> Vec<(String, PathBuf)> {
        let mut v = vec![("global".to_string(), self.global_dir.join("workflows"))];
        if let Some(r) = &self.repo_root {
            v.push(("project".to_string(), r.join(PROJECT_DIR).join("workflows")));
        }
        v
    }

    fn skill_dirs(&self) -> Vec<PathBuf> {
        let mut v = vec![self.global_dir.join("skills")];
        if let Some(r) = &self.repo_root {
            v.push(r.join(".ai/skills"));
            v.push(r.join(PROJECT_DIR).join("skills"));
        }
        v
    }

    /// All workflows, later sources overriding earlier ones by name.
    pub fn workflows(&self) -> Result<Vec<WorkflowSource>> {
        let mut out: Vec<WorkflowSource> = BUILTIN_WORKFLOWS
            .iter()
            .map(|(n, y)| WorkflowSource { name: n.to_string(), origin: "builtin".into(), yaml: y.to_string() })
            .collect();
        for (origin, dir) in self.workflow_dirs() {
            for p in yaml_files(&dir) {
                let yaml = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
                // The file's `name:` is authoritative; fall back to the stem.
                let name = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&yaml)
                    .ok()
                    .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(String::from))
                    .unwrap_or_else(|| p.file_stem().unwrap().to_string_lossy().to_string());
                let src = WorkflowSource { name: name.clone(), origin: format!("{origin}:{}", p.display()), yaml };
                if let Some(e) = out.iter_mut().find(|w| w.name == name) {
                    *e = src;
                } else {
                    out.push(src);
                }
            }
        }
        Ok(out)
    }

    /// Resolve a workflow by name, or by path to a YAML file.
    pub fn workflow(&self, name_or_path: &str) -> Result<(Workflow, WorkflowSource)> {
        let p = Path::new(name_or_path);
        if name_or_path.ends_with(".yaml") || name_or_path.ends_with(".yml") || name_or_path.contains('/') {
            let yaml = std::fs::read_to_string(p).with_context(|| format!("reading workflow {}", p.display()))?;
            let wf = Workflow::parse(&yaml).with_context(|| format!("in {}", p.display()))?;
            let src = WorkflowSource { name: wf.name.clone(), origin: format!("file:{}", p.display()), yaml };
            return Ok((wf, src));
        }
        let src = self
            .workflows()?
            .into_iter()
            .find(|w| w.name == name_or_path)
            .with_context(|| format!("no workflow named {name_or_path:?} (see `herdr-orchestrator workflow list`)"))?;
        let wf = Workflow::parse(&src.yaml).with_context(|| format!("workflow {} ({})", src.name, src.origin))?;
        Ok((wf, src))
    }

    /// Resolve a skill's Markdown text by name.
    pub fn skill(&self, name: &str) -> Result<(String, String)> {
        if !valid_name(name) {
            bail!("invalid skill name {name:?}");
        }
        for dir in self.skill_dirs().iter().rev() {
            let p = dir.join(format!("{name}.md"));
            if p.is_file() {
                let text = std::fs::read_to_string(&p)?;
                if text.len() > 256 * 1024 {
                    bail!("skill {} is larger than 256 KiB", p.display());
                }
                return Ok((text, p.display().to_string()));
            }
        }
        BUILTIN_SKILLS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| (t.to_string(), format!("builtin:{name}")))
            .with_context(|| format!("no skill named {name:?}"))
    }

    pub fn skills(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = BUILTIN_SKILLS.iter().map(|(n, _)| (n.to_string(), "builtin".to_string())).collect();
        for dir in self.skill_dirs() {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) == Some("md") {
                        let n = p.file_stem().unwrap().to_string_lossy().to_string();
                        let origin = p.display().to_string();
                        if let Some(x) = out.iter_mut().find(|(k, _)| *k == n) {
                            x.1 = origin;
                        } else {
                            out.push((n, origin));
                        }
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_valid() {
        let d = tempfile::tempdir().unwrap();
        let c = Catalog::new(d.path(), None);
        let all = c.workflows().unwrap();
        assert_eq!(all.len(), BUILTIN_WORKFLOWS.len());
        for w in all {
            Workflow::parse(&w.yaml).unwrap_or_else(|e| panic!("{}: {e:#}", w.name));
        }
        for (n, _) in BUILTIN_SKILLS {
            assert!(c.skill(n).is_ok());
        }
    }

    #[test]
    fn project_overrides_builtin() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join(PROJECT_DIR).join("workflows")).unwrap();
        std::fs::write(
            repo.join(PROJECT_DIR).join("workflows/q.yaml"),
            "version: 1\nname: quick-task\nsteps:\n  - id: only\n    type: approval\n    reason: r\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.join(".ai/skills")).unwrap();
        std::fs::write(repo.join(".ai/skills/code-review.md"), "custom").unwrap();
        let c = Catalog::new(&d.path().join("cfg"), Some(&repo));
        let (wf, src) = c.workflow("quick-task").unwrap();
        assert_eq!(wf.steps[0].id, "only");
        assert!(src.origin.starts_with("project:"));
        assert_eq!(c.skill("code-review").unwrap().0, "custom");
        assert!(c.skill("../etc/passwd").is_err());
        assert!(c.workflow("nope").is_err());
    }
}
