//! `Opts::executor`: every step is handed to a `StepExecutor` with what it
//! needs to run it, and what the executor says (exit, wait) is what the
//! result reports. Without an executor, steps run as before.

#![allow(clippy::disallowed_methods)] // the test builds an environment and starts commands

mod common;
use anyhow::{anyhow, Result};
use local_ci::exec::{Exit, JobResult, Opts, Outcome, Workspace};
use local_ci::{
    ExecContext, FrozenPlan, Quiet, ScriptPipe, StepExecutor, StepExit, StepRequest, Target,
    TaskInputs, TaskResult,
};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const W1: &str = ".github/workflows/one.yml";
const TWO_STEPS: &str = "name: w\non:\n  pull_request:\n    paths: ['x/**']\njobs:\n  a:\n    runs-on: x\n    env:\n      JOB_VAR: j\n    steps:\n      - run: echo first\n        env:\n          STEP_VAR: s\n      - run: echo second\n        working-directory: x\n        timeout-minutes: 2\n";
const TOKEN_JOB: &str = "name: w\non:\n  pull_request:\n    paths: ['x/**']\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo \"token ${{ github.token }}\" > token.out\n";

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

fn lab(workflow: &str) -> (common::Tmp, PathBuf) {
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::create_dir_all(dir.join("x")).unwrap();
    std::fs::write(dir.join("x/f"), "0\n").unwrap();
    std::fs::write(dir.join(W1), workflow).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "feature"]);
    std::fs::write(dir.join("x/f"), "1\n").unwrap();
    git(&dir, &["commit", "-qam", "change"]);
    (tmp, dir)
}

fn ctx(home: &Path) -> ExecContext {
    let bin = home.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("gh"),
        "#!/bin/sh\n[ \"$1 $2\" = \"auth token\" ] && echo ghs_faketoken123\n",
    )
    .unwrap();
    std::fs::set_permissions(bin.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    ExecContext::new(
        [
            (
                "PATH",
                format!("{}:/usr/bin:/bin:/usr/local/bin", bin.display()),
            ),
            ("HOME", home.to_string_lossy().into_owned()),
            (
                "LOCAL_CI_HOME",
                home.join("cache").to_string_lossy().into_owned(),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    )
}

fn target(dir: &Path) -> Target {
    Target {
        base: Some("main".into()),
        sha: "HEAD".into(),
        all: false,
        include_disabled: true,
        workflows_dirs: vec![],
        workflows: vec![],
        jobs: vec![],
        dir: dir.to_path_buf(),
    }
}

struct Lab {
    tmp: common::Tmp,
    dir: PathBuf,
    ctx: ExecContext,
    fp: FrozenPlan,
    ws: Workspace,
}

fn lab_for(workflow: &str) -> Lab {
    let (tmp, dir) = lab(workflow);
    let ctx = ctx(tmp.path());
    let fp = local_ci::build_frozen_plan(&ctx, &target(&dir), &mut Quiet).unwrap();
    let cache = tmp.path().join("lab");
    std::fs::create_dir_all(cache.join("locks")).unwrap();
    let wt = cache.join("wt/t");
    local_ci::git::add_worktree(&ctx, &dir, &wt, &fp.source.execution_commit).unwrap();
    let ws = Workspace {
        root: wt,
        repo_root: dir.clone(),
        run_dir: cache.join("runs/t"),
        tools: cache.join("wt/t.tools"),
        lock_file: cache.join("locks/lab.lock"),
    };
    std::fs::create_dir_all(&ws.run_dir).unwrap();
    Lab {
        tmp,
        dir,
        ctx,
        fp,
        ws,
    }
}

impl Lab {
    fn run(&self, executor: Option<Arc<dyn StepExecutor>>) -> TaskResult {
        let opts = Opts {
            parallel: 1,
            traffic_controller: None,
            executor,
            portool: None,
            containers: 1,
            job_timeout_min: 5.0,
            keep: true,
            trash: self.tmp.path().join("lab/trash"),
        };
        local_ci::execute_task(
            &self.ctx,
            &self.fp,
            &self.fp.tasks[0].id,
            &TaskInputs::default(),
            None,
            &self.ws,
            opts,
        )
        .unwrap()
    }
}

fn job(r: &TaskResult) -> &JobResult {
    &r.jobs[0]
}

/// Records each request; answers with `exit` after `wait` seconds of queue.
struct Recorder {
    seen: Mutex<Vec<StepRequest>>,
    exit: Exit,
    wait: Option<f64>,
}

impl Recorder {
    fn new(exit: Exit, wait: Option<f64>) -> Arc<Self> {
        Arc::new(Recorder {
            seen: Mutex::new(vec![]),
            exit,
            wait,
        })
    }
}

impl StepExecutor for Recorder {
    fn run(&self, req: &StepRequest) -> Result<StepExit> {
        self.seen.lock().unwrap().push(StepRequest {
            key: req.key.clone(),
            argv: req.argv.clone(),
            cwd: req.cwd.clone(),
            env: req.env.clone(),
            log: req.log.clone(),
            timeout: req.timeout,
            script_pipe: req.script_pipe,
            cancel: req.cancel.clone(),
        });
        Ok(StepExit {
            exit: self.exit,
            run_secs: 0.5,
            queue_wait_secs: self.wait,
        })
    }
}

#[test]
fn a_step_reaches_the_executor_with_what_it_needs_and_nothing_is_started_here() {
    let l = lab_for(TWO_STEPS);
    let rec = Recorder::new(Exit::Code(0), Some(1.5));
    let r = l.run(Some(rec.clone()));
    let seen = rec.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(job(&r).outcome, Outcome::Pass, "{:?}", job(&r));

    let first = &seen[0];
    assert_eq!(
        first.argv[..5],
        ["bash", "--noprofile", "--norc", "-eo", "pipefail"]
    );
    assert_eq!(first.key.as_deref(), Some("local-ci one.yml a echo first"));
    assert_eq!(first.cwd, l.ws.root);
    assert!(first.log.starts_with(&l.ws.run_dir), "{:?}", first.log);
    assert_eq!(first.script_pipe, None);
    assert!(first.timeout.is_some());
    // exactly the context's variables, and the job's and the step's on top
    let env: std::collections::BTreeMap<_, _> = first.env.iter().cloned().collect();
    assert_eq!(env.len(), first.env.len(), "one entry per name");
    assert_eq!(
        env.get("HOME"),
        l.ctx.var("HOME").map(str::to_string).as_ref()
    );
    assert_eq!(env["JOB_VAR"], "j");
    assert_eq!(env["STEP_VAR"], "s");
    assert!(!env.contains_key("CARGO_MANIFEST_DIR"), "{env:?}");
    let names: Vec<_> = first.env.iter().map(|(k, _)| k.clone()).collect();
    let sorted = {
        let mut n = names.clone();
        n.sort();
        n
    };
    assert_eq!(names, sorted);

    let second = &seen[1];
    assert_eq!(second.cwd, l.ws.root.join("x"));
    assert_eq!(second.timeout, Some(std::time::Duration::from_secs(120)));
    assert_ne!(first.log, second.log);
    assert!(!second.env.iter().any(|(k, _)| k == "STEP_VAR"));
    // the executor's request shares the attempt's cancel state
    assert!(!second.cancel.is_cancelled());
    l.ctx.cancel.cancel();
    assert!(second.cancel.is_cancelled());
    // nothing ran here: the commands did not write their logs
    assert!(!first.log.exists());
    // the executor's wait is the job's queue time, and is not part of its work
    let t = job(&r).time.as_ref().unwrap();
    assert_eq!(t.wait_queue_secs, 3.0);
    assert!(t.work_secs <= job(&r).secs);
    assert!(l.dir.exists());
}

#[test]
fn what_the_executor_returns_is_how_the_step_ended() {
    // a failing exit
    let l = lab_for(TWO_STEPS);
    let rec = Recorder::new(Exit::Code(3), None);
    let r = l.run(Some(rec.clone()));
    assert_eq!(job(&r).outcome, Outcome::Fail);
    assert_eq!(job(&r).steps[0].exit, Some(3));
    // the job stops at the first failing step
    assert_eq!(rec.seen.lock().unwrap().len(), 1);
    assert_eq!(job(&r).time.as_ref().unwrap().wait_queue_secs, 0.0);

    // a timeout
    let l = lab_for(TWO_STEPS);
    let r = l.run(Some(Recorder::new(Exit::Timeout, None)));
    assert!(matches!(&job(&r).steps[0].outcome, Outcome::Error(e) if e.contains("timed out")));

    // an executor that cannot run the step is an error, not a failure
    struct Broken;
    impl StepExecutor for Broken {
        fn run(&self, _: &StepRequest) -> Result<StepExit> {
            Err(anyhow!("no capacity"))
        }
    }
    let l = lab_for(TWO_STEPS);
    let r = l.run(Some(Arc::new(Broken)));
    assert!(
        matches!(&job(&r).outcome, Outcome::Error(e) if e.contains("no capacity")),
        "{:?}",
        job(&r)
    );
}

#[test]
fn a_cancel_seen_by_the_executor_is_a_cancelled_step() {
    struct Cancelling;
    impl StepExecutor for Cancelling {
        fn run(&self, req: &StepRequest) -> Result<StepExit> {
            req.cancel.cancel();
            // even an executor that reports the exit it saw is a cancelled step
            Ok(StepExit {
                exit: Exit::Code(143),
                run_secs: 0.0,
                queue_wait_secs: None,
            })
        }
    }
    let l = lab_for(TWO_STEPS);
    let r = l.run(Some(Arc::new(Cancelling)));
    assert!(l.ctx.cancel.is_cancelled());
    assert!(job(&r).cancelled, "{:?}", job(&r));
    assert_ne!(job(&r).outcome, Outcome::Pass);
}

/// An executor that follows the contract: the command is its own child, in a
/// group of its own, with the script pipe at `child_fd`.
struct Spawning {
    argv0_seen: Mutex<Vec<Vec<String>>>,
}

impl StepExecutor for Spawning {
    fn run(&self, req: &StepRequest) -> Result<StepExit> {
        self.argv0_seen.lock().unwrap().push(req.argv.clone());
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&req.log)?;
        let mut cmd = Command::new(&req.argv[0]);
        cmd.args(&req.argv[1..])
            .env_clear()
            .envs(req.env.iter().cloned())
            .current_dir(&req.cwd)
            .stdin(std::process::Stdio::null())
            .stderr(out.try_clone()?)
            .stdout(out)
            .process_group(0);
        if let Some(ScriptPipe { read_fd, child_fd }) = req.script_pipe {
            unsafe {
                cmd.pre_exec(move || {
                    if read_fd == child_fd {
                        let fl = libc::fcntl(read_fd, libc::F_GETFD);
                        libc::fcntl(read_fd, libc::F_SETFD, fl & !libc::FD_CLOEXEC);
                    } else if libc::dup2(read_fd, child_fd) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn()?;
        let pid = child.id() as i32;
        req.cancel.track_pgid(pid);
        let st = child.wait()?;
        req.cancel.untrack_pgid(pid);
        Ok(StepExit {
            exit: Exit::Code(st.code().unwrap_or(1)),
            run_secs: 0.0,
            queue_wait_secs: None,
        })
    }
}

#[test]
fn an_executor_that_starts_the_command_runs_a_step_including_one_that_reads_its_script_from_a_pipe()
{
    let l = lab_for(TWO_STEPS);
    let ex = Arc::new(Spawning {
        argv0_seen: Mutex::new(vec![]),
    });
    let r = l.run(Some(ex.clone()));
    assert_eq!(job(&r).outcome, Outcome::Pass, "{:?}", job(&r));
    let log = std::fs::read_to_string(job(&r).steps[0].log.as_ref().unwrap()).unwrap();
    assert_eq!(log, "first\n");

    // a script with the token is read from fd 3, which local-ci writes
    let l = lab_for(TOKEN_JOB);
    let ex = Arc::new(Spawning {
        argv0_seen: Mutex::new(vec![]),
    });
    let r = l.run(Some(ex.clone()));
    assert_eq!(job(&r).outcome, Outcome::Pass, "{:?}", job(&r));
    assert!(ex.argv0_seen.lock().unwrap()[0].contains(&"/dev/fd/3".to_string()));
    let out = std::fs::read_to_string(l.ws.root.join("token.out")).unwrap();
    assert_eq!(out, "token ghs_faketoken123\n");
}

#[test]
fn without_an_executor_a_step_is_started_here_as_before() {
    let l = lab_for(TWO_STEPS);
    let r = l.run(None);
    assert_eq!(job(&r).outcome, Outcome::Pass, "{:?}", job(&r));
    let log = std::fs::read_to_string(job(&r).steps[1].log.as_ref().unwrap()).unwrap();
    assert_eq!(log, "second\n");
    assert_eq!(job(&r).time.as_ref().unwrap().wait_queue_secs, 0.0);
}
