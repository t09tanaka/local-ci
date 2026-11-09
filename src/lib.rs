//! The engine behind the `local-ci` command: read the workflows a commit
//! triggers, plan them, run them in an isolated worktree and report.
//!
//! This is not a stable API (0.x): types, functions and modules may change in
//! any release. The `local-ci` binary is a thin adapter over it.

pub mod cache;
pub mod config;
pub mod context;
pub mod exec;
pub mod expr;
pub mod filter;
pub mod frozen;
pub mod git;
pub mod matrix;
pub mod plan;
pub mod portool;
pub mod report;
mod run;
pub mod workflow;

pub use context::{CancelHandle, ExecContext};
pub use frozen::FrozenPlan;
pub use run::{
    build_frozen_plan, execute_plan, post_status, resolve, run, status, Observer, Quiet, Repo,
    RunOptions, RunReport, Target,
};
