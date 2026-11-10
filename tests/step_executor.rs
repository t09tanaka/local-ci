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

impl Lab {
    fn run_plan(
        &self,
        executor: Arc<dyn StepExecutor>,
        parallel: usize,
    ) -> local_ci::report::RunResult {
        let opts = Opts {
            parallel,
            traffic_controller: None,
            executor: Some(executor),
            portool: None,
            containers: 1,
            job_timeout_min: 5.0,
            keep: true,
            trash: self.tmp.path().join("lab/trash"),
        };
        local_ci::execute_plan(&self.ctx, &self.fp, &self.ws, opts).unwrap()
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
    // the fake queued 1.5 s on each of two steps; the work is what is left of
    // local-ci's own clock for the job (the fake runs instantly), never below 0
    assert_eq!(t.work_secs, (job(&r).secs - 3.0).max(0.0));
    assert!(l.dir.exists());
}

const SHARDED: &str = "name: w\non:\n  pull_request:\n    paths: ['x/**']\njobs:\n  t:\n    runs-on: x\n    defaults:\n      run:\n        shell: sh\n    steps:\n      - run: cargo test\n";

/// The step that runs the tests of a shard is listed as its shell on the
/// script, as every step is, and that is what the executor is asked to start.
#[test]
fn the_demand_of_a_shard_step_is_the_shell_the_executor_starts() {
    use local_ci::shard::adapters::{CargoTest, Safety};
    use local_ci::shard::{partition_tests, Discovered, History, PartitionOpts};
    let mut l = lab_for(SHARDED);
    let a = CargoTest::from_command("1", "cargo test", Safety::default()).unwrap();
    let d = Discovered {
        tests: ["a", "b", "c", "d"].map(String::from).to_vec(),
        complete: true,
    };
    let plan = partition_tests(&a, &d, &History::default(), 2, &PartitionOpts::default());
    l.fp =
        l.fp.with_shards(&[local_ci::dag::ShardRequest {
            workflow: W1.into(),
            job: "t".into(),
            plan,
            step: 0,
            dir: String::new(),
            discovered: d,
            fail_fast: true,
        }])
        .unwrap();
    let id =
        l.fp.tasks
            .iter()
            .find(|t| t.shard.is_some())
            .unwrap()
            .id
            .clone();
    let demand = local_ci::plan_task_demand(&l.fp, &id).unwrap();
    assert_eq!(demand.nodes.len(), 1);
    assert_eq!(demand.nodes[0].argv, ["sh", "-e", local_ci::demand::SCRIPT]);
    let rec = Recorder::new(Exit::Code(0), None);
    let opts = Opts {
        parallel: 1,
        traffic_controller: None,
        executor: Some(rec.clone()),
        portool: None,
        containers: 1,
        job_timeout_min: 5.0,
        keep: true,
        trash: l.tmp.path().join("lab/trash"),
    };
    local_ci::execute_task(
        &l.ctx,
        &l.fp,
        &id,
        &TaskInputs::default(),
        None,
        &l.ws,
        opts,
    )
    .unwrap();
    let seen = rec.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (script, shell) = seen[0].argv.split_last().unwrap();
    assert!(script.ends_with(".sh"), "{script}");
    assert_eq!(shell, &demand.nodes[0].argv[..2]);
    assert_eq!(l.ws.root.join(&demand.nodes[0].cwd), seen[0].cwd);
}

#[test]
fn the_demand_of_a_task_is_what_its_steps_ask_the_executor() {
    let l = lab_for(TWO_STEPS);
    let demand = local_ci::plan_task_demand(&l.fp, &l.fp.tasks[0].id).unwrap();
    let rec = Recorder::new(Exit::Code(0), None);
    l.run(Some(rec.clone()));
    let seen = rec.seen.lock().unwrap();
    assert_eq!(demand.nodes.len(), seen.len());
    for (n, req) in demand.nodes.iter().zip(seen.iter()) {
        assert_eq!(n.key, req.key, "{}", n.step_id);
        assert_eq!(l.ws.root.join(&n.cwd), req.cwd, "{}", n.step_id);
        // the script file is the one word the host fills in
        let (script, shell) = req.argv.split_last().unwrap();
        assert!(script.ends_with(".sh"), "{script}");
        assert_eq!(n.argv.split_last().unwrap().1, shell);
        assert_eq!(n.argv.last().unwrap(), local_ci::demand::SCRIPT);
        assert_eq!(
            n.weight.timeout_minutes.map(|m| (m * 60.0) as u64),
            n.weight
                .timeout_minutes
                .and(req.timeout.map(|t| t.as_secs())),
            "{}",
            n.step_id
        );
    }
}

#[test]
fn a_task_of_another_wire_version_is_not_started_and_its_result_travels_without_paths() {
    use local_ci::wire::{Digested, WIRE_SCHEMA_VERSION};
    use local_ci::WireTaskResult;
    let l = lab_for(TWO_STEPS);
    let bad = WIRE_SCHEMA_VERSION + 1;
    let opts = || Opts {
        parallel: 1,
        traffic_controller: None,
        executor: Some(Recorder::new(Exit::Code(0), None)),
        portool: None,
        containers: 1,
        job_timeout_min: 5.0,
        keep: true,
        trash: l.tmp.path().join("lab/trash"),
    };
    let id = &l.fp.tasks[0].id;
    let inputs = TaskInputs {
        schema_version: bad,
        ..TaskInputs::default()
    };
    let e = local_ci::execute_task(&l.ctx, &l.fp, id, &inputs, None, &l.ws, opts())
        .unwrap_err()
        .to_string();
    assert!(e.contains("task inputs: schema version"), "{e}");
    let mut receipt = local_ci::setup::PreparedReceipt::empty("k", "s");
    receipt.schema_version = bad;
    let tctx = local_ci::setup::TemplateContext::local(&l.fp, "r");
    let e = local_ci::execute_task(
        &l.ctx,
        &l.fp,
        id,
        &TaskInputs::default(),
        Some((&receipt, &tctx)),
        &l.ws,
        opts(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("receipt: schema version"), "{e}");

    let recipe = local_ci::setup::SetupRecipe {
        schema_version: bad,
        ..Default::default()
    };
    let key = recipe.template_key(&tctx);
    let e = local_ci::execute_setup(&l.ctx, &l.fp, &recipe, &key, &l.ws, opts())
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("setup recipe: schema version"), "{e}");

    // a result as the worker made it, and as it crosses: no path of the host
    let r = l.run(Some(Recorder::new(Exit::Code(0), None)));
    assert_eq!(r.schema_version, WIRE_SCHEMA_VERSION);
    assert!(r.jobs[0].steps[0].log.is_some());
    let wire = WireTaskResult::from_task_result(&r, &l.ws.run_dir).unwrap();
    let text = serde_json::to_string(&wire).unwrap();
    assert!(!text.contains(l.ws.run_dir.to_str().unwrap()), "{text}");
    assert!(
        text.contains("\"log\":\"one-a/01-echo-first.log\""),
        "{text}"
    );
    let back = WireTaskResult::from_json(&text)
        .unwrap()
        .into_task_result(&l.ws.run_dir)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&back).unwrap(),
        serde_json::to_value(&r).unwrap()
    );
    assert_eq!(wire.digest().unwrap(), wire.digest().unwrap());
    // the coordinator puts its own directory back
    let elsewhere = std::path::Path::new("/coordinator/run");
    let moved = WireTaskResult::from_json(&text)
        .unwrap()
        .into_task_result(elsewhere)
        .unwrap();
    assert!(moved.jobs[0].steps[0]
        .log
        .as_ref()
        .unwrap()
        .starts_with(elsewhere));
    // a log of another directory is not carried
    let e = WireTaskResult::from_task_result(&r, std::path::Path::new("/else/where"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("outside"), "{e}");
    // a version of another build is refused
    let mut other = wire.clone();
    other.schema_version = bad;
    assert!(other.into_task_result(&l.ws.run_dir).is_err());
    // and a result that says it is of another version is a problem of the task
    let mut r2 = r;
    r2.schema_version = bad;
    let problems = local_ci::verify_task_result(&l.fp.tasks[0], &r2);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("task result: schema version")),
        "{problems:?}"
    );
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

    // an exit by signal, as 128 + the signal
    let l = lab_for(TWO_STEPS);
    let r = l.run(Some(Recorder::new(Exit::Code(143), None)));
    assert_eq!(job(&r).outcome, Outcome::Fail);
    assert_eq!(job(&r).steps[0].exit, Some(143));

    // a queue wait that is NaN, negative or infinite counts as 0
    for bad in [f64::NAN, -2.0, f64::INFINITY, f64::NEG_INFINITY] {
        let l = lab_for(TWO_STEPS);
        let r = l.run(Some(Recorder::new(Exit::Code(0), Some(bad))));
        assert_eq!(job(&r).outcome, Outcome::Pass, "{bad}");
        assert_eq!(job(&r).time.as_ref().unwrap().wait_queue_secs, 0.0, "{bad}");
    }

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
    /// Told once the process group is tracked.
    tracked: Option<std::sync::mpsc::Sender<i32>>,
}

impl Spawning {
    fn new() -> Arc<Self> {
        Arc::new(Spawning {
            argv0_seen: Mutex::new(vec![]),
            tracked: None,
        })
    }
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
        if let Some(tx) = &self.tracked {
            let _ = tx.send(pid);
        }
        // a cancel that came before the group was tracked did not reach it
        if req.cancel.is_cancelled() {
            unsafe {
                libc::killpg(pid, libc::SIGTERM);
            }
        }
        let st = child.wait()?;
        req.cancel.untrack_pgid(pid);
        // a command that ended on a signal is 128 + the signal
        let code = st.code().unwrap_or_else(|| {
            128 + std::os::unix::process::ExitStatusExt::signal(&st).unwrap_or(0)
        });
        Ok(StepExit {
            exit: Exit::Code(code),
            run_secs: 0.0,
            queue_wait_secs: None,
        })
    }
}

#[test]
fn an_executor_that_starts_the_command_runs_a_step_including_one_that_reads_its_script_from_a_pipe()
{
    let l = lab_for(TWO_STEPS);
    let ex = Spawning::new();
    let r = l.run(Some(ex.clone()));
    assert_eq!(job(&r).outcome, Outcome::Pass, "{:?}", job(&r));
    let log = std::fs::read_to_string(job(&r).steps[0].log.as_ref().unwrap()).unwrap();
    assert_eq!(log, "first\n");

    // a script with the token is read from fd 3, which local-ci writes
    let l = lab_for(TOKEN_JOB);
    let ex = Spawning::new();
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

const SLEEPER: &str = "name: w\non:\n  pull_request:\n    paths: ['x/**']\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: sleep 60\n";
const MATRIX: &str = "name: w\non:\n  pull_request:\n    paths: ['x/**']\njobs:\n  a:\n    runs-on: x\n    strategy:\n      matrix:\n        n: [1, 2, 3]\n    steps:\n      - run: echo ${{ matrix.n }}\n";

#[test]
fn a_command_that_ends_on_a_signal_is_128_plus_the_signal() {
    let l = lab_for(
        "name: w\non:\n  pull_request:\n    paths: ['x/**']\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: kill -KILL $$\n",
    );
    let r = l.run(Some(Spawning::new()));
    assert_eq!(job(&r).outcome, Outcome::Fail, "{:?}", job(&r));
    assert_eq!(job(&r).steps[0].exit, Some(128 + 9));
}

#[test]
fn a_cancel_stops_the_tracked_group_and_the_step_is_cancelled() {
    let l = lab_for(SLEEPER);
    let (tx, rx) = std::sync::mpsc::channel();
    let ex = Arc::new(Spawning {
        argv0_seen: Mutex::new(vec![]),
        tracked: Some(tx),
    });
    let cancel = l.ctx.cancel.clone();
    let canceller = std::thread::spawn(move || {
        let pg = rx.recv().unwrap();
        cancel.cancel();
        pg
    });
    let started = std::time::Instant::now();
    let r = l.run(Some(ex));
    let pg = canceller.join().unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    assert!(job(&r).cancelled, "{:?}", job(&r));
    assert_ne!(job(&r).outcome, Outcome::Pass);
    // the group is gone (reaped by the executor's wait)
    assert_eq!(unsafe { libc::killpg(pg, 0) }, -1);
}

#[test]
fn when_the_executor_fails_the_script_pipe_is_closed_and_its_writer_is_done() {
    type Ident = Option<(u64, u64)>;
    #[allow(clippy::unnecessary_cast)] // the field types differ by platform
    fn ident(fd: i32) -> Option<(u64, u64)> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        (unsafe { libc::fstat(fd, &mut st) } == 0).then_some((st.st_dev as u64, st.st_ino as u64))
    }
    struct FailsWithPipe {
        fd: Mutex<Option<(i32, Ident)>>,
    }
    impl StepExecutor for FailsWithPipe {
        fn run(&self, req: &StepRequest) -> Result<StepExit> {
            *self.fd.lock().unwrap() = req.script_pipe.map(|p| (p.read_fd, ident(p.read_fd)));
            Err(anyhow!("refused"))
        }
    }
    let l = lab_for(TOKEN_JOB);
    let ex = Arc::new(FailsWithPipe {
        fd: Mutex::new(None),
    });
    let r = l.run(Some(ex.clone()));
    assert!(
        matches!(&job(&r).outcome, Outcome::Error(e) if e.contains("refused")),
        "{:?}",
        job(&r)
    );
    let (fd, was) = ex
        .fd
        .lock()
        .unwrap()
        .expect("a token step has a script pipe");
    assert!(was.is_some(), "the pipe was open during run");
    // local-ci closed its read end: the number is either not open any more
    // (EBADF) or, when another thread has reused it, is no longer that pipe
    match ident(fd) {
        None => assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        ),
        Some(now) => assert_ne!(Some(now), was, "the pipe's read end is still open"),
    }
    // the writer thread was joined before the result came back: this thread
    // would not have returned otherwise, and no thread of ours is left writing
    assert!(!l.ws.root.join("token.out").exists());
}

#[test]
fn the_legs_of_a_matrix_call_the_executor_at_once() {
    struct Gate {
        now: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }
    impl StepExecutor for Gate {
        fn run(&self, _: &StepRequest) -> Result<StepExit> {
            use std::sync::atomic::Ordering::SeqCst;
            let n = self.now.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(n, SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(300));
            self.now.fetch_sub(1, SeqCst);
            Ok(StepExit {
                exit: Exit::Code(0),
                run_secs: 0.3,
                queue_wait_secs: None,
            })
        }
    }
    let l = lab_for(MATRIX);
    let ex = Arc::new(Gate {
        now: 0.into(),
        peak: 0.into(),
    });
    let r = l.run_plan(ex.clone(), 3);
    assert_eq!(r.state, "success", "{}", r.description);
    assert!(
        ex.peak.load(std::sync::atomic::Ordering::SeqCst) > 1,
        "legs ran one after another"
    );
}
