//! What one attempt (one call of [`crate::run`] or [`crate::execute_plan`])
//! needs from its host: the environment it sees, where it keeps its files, and
//! the handle that stops it.
//!
//! Nothing in the library reads the process environment; the caller builds an
//! [`ExecContext`] (the `local-ci` binary builds it from `std::env`). Give each
//! attempt its own context: the processes and containers an attempt starts are
//! tracked in its [`CancelHandle`], so cancelling one attempt leaves the others
//! running.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    pgids: Mutex<Vec<i32>>,
    containers: Mutex<Vec<String>>,
    compose_projects: Mutex<Vec<String>>,
}

/// Cancellation and child tracking owned by one attempt. Cloning shares the
/// state, so a signal handler can hold a clone while the attempt runs.
#[derive(Clone, Default)]
pub struct CancelHandle(Arc<CancelState>);

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stop the processes this attempt started and make it report as
    /// cancelled. Safe to call from a signal handler thread, and more than once.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        for &pg in self.0.pgids.lock().unwrap().iter() {
            // SAFETY: a plain signal to a process group this attempt started.
            unsafe {
                libc::killpg(pg, libc::SIGTERM);
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn track_pgid(&self, pg: i32) {
        self.0.pgids.lock().unwrap().push(pg);
    }

    pub(crate) fn untrack_pgid(&self, pg: i32) {
        self.0.pgids.lock().unwrap().retain(|&p| p != pg);
    }

    pub(crate) fn track_container(&self, name: String) {
        self.0.containers.lock().unwrap().push(name);
    }

    pub(crate) fn untrack_containers(&self, names: &std::collections::HashSet<&String>) {
        self.0
            .containers
            .lock()
            .unwrap()
            .retain(|n| !names.contains(n));
    }

    pub(crate) fn track_compose(&self, project: String) {
        self.0.compose_projects.lock().unwrap().push(project);
    }

    pub(crate) fn untrack_compose(&self, projects: &[String]) {
        self.0
            .compose_projects
            .lock()
            .unwrap()
            .retain(|p| !projects.contains(p));
    }

    /// Take down what this attempt still has up: compose projects, then
    /// containers.
    pub(crate) fn remove_containers(&self, ctx: &ExecContext) {
        let projects: Vec<String> = self.0.compose_projects.lock().unwrap().drain(..).collect();
        crate::exec::compose_down(ctx, &projects);
        let names: Vec<String> = self.0.containers.lock().unwrap().drain(..).collect();
        if !names.is_empty() {
            let _ = ctx
                .command("docker")
                .arg("rm")
                .arg("-f")
                .args(&names)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// The environment and the cancel handle of one attempt.
pub struct ExecContext {
    env: BTreeMap<String, String>,
    /// Stops this attempt; see [`CancelHandle`].
    pub cancel: CancelHandle,
    github_token: OnceLock<Option<String>>,
}

impl ExecContext {
    /// A context over `env` (the variables `PATH`, `HOME`, `LOCAL_CI_HOME` and
    /// `XDG_CACHE_HOME` matter; steps run with exactly these variables plus
    /// their own).
    pub fn new(env: BTreeMap<String, String>) -> Self {
        ExecContext {
            env,
            cancel: CancelHandle::new(),
            github_token: OnceLock::new(),
        }
    }

    /// The same environment with a handle of its own: for a second attempt
    /// that must not be cancelled with this one. (Not `Clone`: a plain clone
    /// would share the handle and so the cancellation.)
    pub fn fresh_attempt(&self) -> Self {
        ExecContext {
            env: self.env.clone(),
            cancel: CancelHandle::new(),
            github_token: OnceLock::new(),
        }
    }

    /// A command that sees exactly this context's environment (not the
    /// process's).
    pub(crate) fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        #[allow(clippy::disallowed_methods)] // the one place; the environment is cleared next
        let mut c = Command::new(program);
        c.env_clear().envs(&self.env);
        c
    }

    /// The GitHub token of the machine this runs on (`gh auth token`), for
    /// steps that read `github.token` or `secrets.GITHUB_TOKEN`. Asked once,
    /// and only when a step needs it; `None` when `gh` is missing or not
    /// logged in.
    pub fn github_token(&self) -> Option<&str> {
        self.github_token
            .get_or_init(|| {
                let gh = self.find_tool("gh")?;
                let out = self
                    .command(gh)
                    .args(["auth", "token"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .ok()?;
                let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
                (out.status.success() && !t.is_empty()).then_some(t)
            })
            .as_deref()
    }

    /// The token, if it has been asked for already (to mask it in output).
    pub(crate) fn github_token_if_fetched(&self) -> Option<&str> {
        self.github_token.get()?.as_deref()
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    pub fn var(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }

    /// `HOME`, when set and not empty.
    pub fn home(&self) -> Option<&str> {
        self.var("HOME").filter(|h| !h.is_empty())
    }

    /// Where runs, results, worktrees and locks live by default
    /// (`LOCAL_CI_HOME`, else the platform cache directory).
    pub fn cache_dir(&self) -> PathBuf {
        if let Some(d) = self.var("LOCAL_CI_HOME") {
            return PathBuf::from(d);
        }
        let home = PathBuf::from(self.var("HOME").unwrap_or("/tmp"));
        if cfg!(target_os = "macos") {
            return home.join("Library/Caches/local-ci");
        }
        match self.var("XDG_CACHE_HOME") {
            Some(x) if !x.is_empty() => PathBuf::from(x).join("local-ci"),
            _ => home.join(".cache/local-ci"),
        }
    }

    /// An executable on this context's `PATH`, or in `~/.cargo/bin`.
    /// `task-traffic-controller`, else its old name `canirun`.
    pub fn find_traffic_controller(&self) -> Option<PathBuf> {
        self.find_tool("task-traffic-controller")
            .or_else(|| self.find_tool("canirun"))
    }

    pub fn find_tool(&self, name: &str) -> Option<PathBuf> {
        for d in self.var("PATH").unwrap_or("").split(':') {
            if d.is_empty() {
                continue;
            }
            let p = PathBuf::from(d).join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        let p = PathBuf::from(self.var("HOME").unwrap_or(""))
            .join(".cargo/bin")
            .join(name);
        p.is_file().then_some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};

    #[test]
    fn a_command_sees_the_context_env_and_not_the_processs() {
        let ctx = ExecContext::new([("ONLY".to_string(), "1".to_string())].into());
        let out = ctx.command("/usr/bin/env").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ONLY=1\n");
    }

    #[test]
    fn the_new_controller_name_wins_over_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |n: &str| fs::write(dir.path().join(n), "").unwrap();
        let env = |home: &Path| {
            ExecContext::new(
                [
                    (
                        "PATH".to_string(),
                        dir.path().to_string_lossy().into_owned(),
                    ),
                    ("HOME".to_string(), home.to_string_lossy().into_owned()),
                ]
                .into(),
            )
        };
        let nowhere = dir.path().join("nohome");
        assert_eq!(env(&nowhere).find_traffic_controller(), None);
        mk("canirun");
        assert_eq!(
            env(&nowhere).find_traffic_controller(),
            Some(dir.path().join("canirun"))
        );
        mk("task-traffic-controller");
        assert_eq!(
            env(&nowhere).find_traffic_controller(),
            Some(dir.path().join("task-traffic-controller"))
        );
    }
}
