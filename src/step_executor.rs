//! A hook for running a step somewhere other than a child process of
//! local-ci.
//!
//! By default a step is started by local-ci itself (behind
//! `task-traffic-controller exec` when that tool is found). A caller that
//! queues and starts commands on its own, such as a supervisor that admits
//! each step against a machine's resources, sets
//! [`Opts::executor`](crate::exec::Opts::executor) and local-ci hands it every
//! step as a [`StepRequest`] instead. local-ci does not depend on any such
//! queue: the caller implements [`StepExecutor`].

use crate::context::CancelHandle;
use crate::exec::Exit;
use anyhow::Result;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::time::Duration;

/// Runs the shell command of one step.
///
/// local-ci calls [`run`](StepExecutor::run) once per step, from the thread
/// that runs the step; several steps may be running at once, so an
/// implementation must be `Send + Sync` and keep nothing per step outside the
/// call. `run` blocks until the command has ended, and an implementation is
/// bound by this contract:
///
/// - **Environment.** The command sees exactly `req.env` and nothing of the
///   implementation's own process environment (clear it before starting).
/// - **Working directory and stdin.** `req.cwd`, and a closed stdin
///   (`/dev/null`).
/// - **Log.** stdout and stderr of the command both go to `req.log`, opened
///   for appending (create it when missing, keep what is there). Whatever the
///   implementation itself prints about the step (a queue message) may be
///   appended to the same file too.
/// - **Process group.** The command runs as the leader of a new process
///   group, and the implementation calls
///   [`CancelHandle::track_pgid`] with its pid as soon as it has one and
///   [`CancelHandle::untrack_pgid`] when the command has ended, so that
///   [`CancelHandle::cancel`] (which sends `SIGTERM` to the tracked groups,
///   possibly from a signal handler thread) reaches it. Only the pid of a
///   process group leader (greater than 1) is tracked; `track_pgid` ignores
///   anything else. The order is: spawn, `track_pgid`, then check
///   `req.cancel.is_cancelled()` again and, when it is already cancelled,
///   send `SIGTERM` to the group yourself (a `cancel` that came before
///   `track_pgid` did not reach it). `untrack_pgid` must be called on every
///   way out of `run` (`Ok`, `Err` and a panic); a guard that untracks on
///   drop is the way to do it.
/// - **Concurrency.** A matrix job runs its legs at once, up to its
///   `max_parallel`, so that many calls of `run` on the same instance can be
///   in progress together. Limiting how many commands run at a time, and the
///   resources they use, is the implementation's job: `Opts::parallel` does
///   not do it.
/// - **Script pipe.** When `req.script_pipe` is set, the command must be
///   started with `child_fd` being the read end `read_fd` (a `dup2` between
///   fork and exec, which clears close-on-exec on the copy; when
///   `read_fd == child_fd`, clear close-on-exec on it instead). Anything
///   started in between, such as a wrapper, must pass the descriptor on to
///   the command. local-ci writes the script to the pipe on a thread of its
///   own, owns `read_fd` and closes it after `run` returns: do not close it,
///   and do not use it after `run` returns.
/// - **Timeout.** `req.timeout`, when set, counts from the moment the command
///   starts, not from the time spent waiting to be admitted. On expiry the
///   implementation sends `SIGTERM` to the process group, `SIGKILL` if it is
///   still there after 10 seconds, and returns [`Exit::Timeout`].
/// - **Cancel.** When `req.cancel.is_cancelled()` is seen, whether while
///   waiting or while the command runs, the implementation stops waiting,
///   makes sure the process group is gone (`SIGTERM`, then `SIGKILL` after 10
///   seconds) and only then returns [`Exit::Cancelled`]. A step cancelled
///   before it started must not be started.
/// - **Exit.** A command that ended on a signal is `Exit::Code(128 + signal)`.
///   Return `Err` only when the step could not be run at all (the step is then
///   reported as an error, not as a failure).
/// - **Waiting.** When the command had to wait for resources, report that time
///   in [`StepExit::queue_wait_secs`], so it is counted apart from the step's
///   running time (`wait_queue_secs` of the job's time). A value that is
///   negative, NaN or too large to be a duration counts as 0. local-ci uses
///   only `queue_wait_secs` for waiting time: [`StepExit::run_secs`] is for
///   information and is not used.
pub trait StepExecutor: Send + Sync {
    fn run(&self, req: &StepRequest) -> Result<StepExit>;
}

/// One step to run; see [`StepExecutor`] for what the implementation owes.
pub struct StepRequest {
    /// What the step's history is kept under (the same key
    /// `task-traffic-controller exec --key` gets); `None` for a step that has
    /// no history.
    pub key: Option<String>,
    /// The program and its arguments, the shell and the script included.
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// The environment of the command: the execution context's variables with
    /// the step's own on top, one entry per name, sorted by name.
    pub env: Vec<(String, String)>,
    /// Where stdout and stderr go (appended).
    pub log: PathBuf,
    pub timeout: Option<Duration>,
    /// Set for a script that holds the GitHub token, which is not a file but
    /// is read from a pipe (`argv` then names `/dev/fd/3`).
    pub script_pipe: Option<ScriptPipe>,
    /// The attempt's cancel handle: track the process group on it, and watch
    /// it. A clone shares the attempt's state.
    pub cancel: CancelHandle,
}

/// A descriptor the command must be started with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptPipe {
    /// The read end, owned by local-ci and valid until `run` returns.
    pub read_fd: RawFd,
    /// The descriptor number the command reads the script from.
    pub child_fd: RawFd,
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StepExit {
    pub exit: Exit,
    /// Seconds the command ran, from when it started.
    pub run_secs: f64,
    /// Seconds spent waiting to be admitted, when the executor queues.
    pub queue_wait_secs: Option<f64>,
}
