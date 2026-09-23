//! Execution sandbox seam.
//!
//! MVP ships only [`NoSandbox`]: commands run as the current user with the
//! worktree as cwd and a filtered environment. That is **not** isolation.
//! Future backends (bubblewrap, containers, macOS sandbox profiles, remote
//! VMs) implement [`ExecutionSandbox::wrap`] to rewrite the argv, without
//! the engine changing.

use std::path::Path;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    #[default]
    None,
    Bubblewrap,
    Container,
    MacosProfile,
}

pub trait ExecutionSandbox: Send + Sync {
    fn kind(&self) -> SandboxKind;
    /// Whether this backend actually restricts filesystem/network access.
    fn isolates(&self) -> bool;
    /// Rewrite `argv` so it executes inside the sandbox rooted at `worktree`.
    fn wrap(&self, argv: &[String], worktree: &Path) -> Result<Vec<String>>;
}

#[derive(Debug, Default, Clone)]
pub struct NoSandbox;

impl ExecutionSandbox for NoSandbox {
    fn kind(&self) -> SandboxKind {
        SandboxKind::None
    }
    fn isolates(&self) -> bool {
        false
    }
    fn wrap(&self, argv: &[String], _worktree: &Path) -> Result<Vec<String>> {
        Ok(argv.to_vec())
    }
}

pub fn sandbox_for(kind: SandboxKind) -> Result<Box<dyn ExecutionSandbox>> {
    match kind {
        SandboxKind::None => Ok(Box::new(NoSandbox)),
        other => bail!(
            "sandbox backend {other:?} is not implemented in this version; use `none` (see docs/SECURITY_MODEL.md)"
        ),
    }
}
