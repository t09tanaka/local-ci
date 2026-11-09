//! The engine behind the `local-ci` command: read the workflows a commit
//! triggers, plan them, run them in an isolated worktree and report.
//!
//! This is not a stable API (0.x): types, functions and modules may change in
//! any release. The `local-ci` binary is a thin adapter over it.

pub mod cache;
pub mod config;
pub mod exec;
pub mod expr;
pub mod filter;
pub mod git;
pub mod matrix;
pub mod plan;
pub mod portool;
pub mod report;
mod run;
pub mod workflow;

pub use run::{
    find_tool, post_status, resolve, run, status, Observer, Quiet, Repo, RunOptions, RunReport,
    Target,
};

use std::path::PathBuf;

/// Where runs, results, worktrees and locks live by default (`LOCAL_CI_HOME`,
/// else the platform cache directory).
pub fn cache_dir() -> PathBuf {
    if let Ok(d) = std::env::var("LOCAL_CI_HOME") {
        return PathBuf::from(d);
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
    if cfg!(target_os = "macos") {
        return home.join("Library/Caches/local-ci");
    }
    match std::env::var("XDG_CACHE_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x).join("local-ci"),
        _ => home.join(".cache/local-ci"),
    }
}

/// Stop the running processes of every [`run`] in this process and make them
/// report as cancelled. Meant for a signal handler. The tracking behind it is
/// process-global (see `exec`).
pub fn cancel() {
    exec::kill_all();
}
