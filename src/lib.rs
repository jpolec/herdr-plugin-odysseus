//! herdr-orchestrator: governed multi-agent workflow orchestration for Herdr.
//!
//! See `docs/ARCHITECTURE.md` for the design. The crate is a library so the
//! engine can be exercised by integration tests with mock Herdr and fake
//! runners; `src/main.rs` is a thin CLI over it.

pub mod approvals;
pub mod audit;
pub mod model;
pub mod policies;
pub mod security;
pub mod store;
pub mod checks;
pub mod config;
pub mod git;
pub mod github;
pub mod process;
pub mod workflow;
pub mod herdr;
pub mod runners;
pub mod daemon;
pub mod engine;
pub mod recovery;
pub mod telemetry;
pub mod cli;
pub mod ui;
