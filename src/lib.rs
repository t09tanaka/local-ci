//! The engine behind the `local-ci` command: read the workflows a commit
//! triggers, plan them, run them in an isolated worktree and report.
//!
//! This is not a stable API (0.x): types, functions and modules may change in
//! any release. The `local-ci` binary is a thin adapter over it.

pub mod cache;
pub mod complete;
pub mod config;
pub mod context;
pub mod dag;
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
pub mod setup;
pub mod source;
pub mod task;
pub mod template;
pub mod tree;
pub mod wire;
pub mod workflow;

pub use complete::{validate_run_result, verify_task_result, Completeness, Verdict};
pub use context::{CancelHandle, ExecContext};
pub use frozen::{derive_job_dag, FrozenPlan};
pub use run::{
    build_frozen_plan, build_frozen_plan_with, execute_plan, freeze, post_status, publish_target,
    resolve, resolve_with, run, status, Observer, Quiet, Repo, RunOptions, RunReport, Target,
};
pub use task::{
    aggregate_matrix, execute_setup, execute_task, JobOutcome, JobStatus, TaskInputs, TaskResult,
};
