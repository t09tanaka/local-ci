//! Running the shard tasks of a split job against the example suites in
//! `examples/sharding`: each shard's task runs its own command in place of the
//! job's step, runs only the tests it was given, and the shards together run
//! every test once (a run of a plan, and `execute_task` shard by shard, give the
//! same). Cargo needs a toolchain; the Node suites need `npm ci` in
//! `examples/sharding/node` and Node on the PATH; without them the tests fail
//! rather than skip.

#![allow(clippy::disallowed_methods)] // the tests read the environment of the run

mod common;
use local_ci::complete::aggregate_shard_group;
use local_ci::dag::{NotSplit, ShardRequest};
use local_ci::exec::{Opts, Outcome, Workspace};
use local_ci::shard::adapters::{run_invocation, CargoTest, Jest, Safety, Vitest};
use local_ci::shard::{
    partition_tests, Discovered, History, PartitionOpts, RunnerAdapter, ShardOutcome, SplitMode,
    TestStatus,
};
use local_ci::{ExecContext, FrozenPlan, Quiet, Target, TaskInputs};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const F: &str = ".github/workflows/ci.yml";

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/sharding")
        .join(name)
}

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

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        if name == "node_modules" || name == "target" {
            continue;
        }
        let dest = to.join(name);
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}

fn ctx(home: &Path) -> ExecContext {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    env.remove("SHARD_EXAMPLE_FAIL");
    // a coverage run instruments the crate under test, not the example
    env.retain(|k, _| {
        !(k.starts_with("CARGO_LLVM_COV")
            || k.starts_with("LLVM_")
            || k.ends_with("RUSTFLAGS")
            || k.starts_with("RUSTC_")
            || k == "CARGO_TARGET_DIR")
    });
    env.insert(
        "LOCAL_CI_HOME".into(),
        home.join("cache").to_string_lossy().into_owned(),
    );
    ExecContext::new(env)
}

/// A repository whose one job runs `command` in `dir` of the example, a
/// workspace that is a checkout of it, and the plan.
struct Lab {
    tmp: common::Tmp,
    ctx: ExecContext,
    fp: FrozenPlan,
    ws: Workspace,
}

fn lab(example_dir: &str, command: &str, env: &str) -> Lab {
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join("work");
    copy_dir(&example(example_dir), &dir.join(example_dir));
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(
        dir.join(F),
        format!(
            "name: ci\non: pull_request\njobs:\n  test:\n    runs-on: x\n    steps:\n      - run: {command}\n        working-directory: {example_dir}\n{env}  after:\n    needs: test\n    runs-on: x\n    steps:\n      - run: echo after\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "feature"]);
    std::fs::write(dir.join("a.txt"), "b\n").unwrap();
    git(&dir, &["commit", "-qam", "change"]);
    let ctx = ctx(tmp.path());
    let t = Target {
        base: Some("main".into()),
        sha: "HEAD".into(),
        all: false,
        include_disabled: true,
        workflows_dirs: vec![],
        workflows: vec![],
        jobs: vec![],
        dir: dir.clone(),
    };
    let fp = local_ci::build_frozen_plan(&ctx, &t, &mut Quiet).unwrap();
    let cache = tmp.path().join("lab");
    std::fs::create_dir_all(cache.join("locks")).unwrap();
    let wt = cache.join("wt/t");
    local_ci::git::add_worktree(&ctx, &dir, &wt, &fp.source.execution_commit).unwrap();
    // what `npm ci` made is not in the commit
    if example_dir == "node" {
        std::os::unix::fs::symlink(
            example("node").join("node_modules"),
            wt.join("node/node_modules"),
        )
        .unwrap();
    }
    let ws = Workspace {
        root: wt,
        repo_root: dir,
        run_dir: cache.join("runs/t"),
        tools: cache.join("wt/t.tools"),
        lock_file: cache.join("locks/lab.lock"),
    };
    std::fs::create_dir_all(&ws.run_dir).unwrap();
    Lab { tmp, ctx, fp, ws }
}

impl Lab {
    fn opts(&self) -> Opts {
        Opts {
            parallel: 1,
            traffic_controller: None,
            executor: None,
            portool: None,
            containers: 1,
            job_timeout_min: 5.0,
            keep: true,
            trash: self.tmp.path().join("lab/trash"),
        }
    }
}

/// List the tests of the example with `a`, split them in `n`, and ask the plan
/// to run the job as those shards.
fn split<A: RunnerAdapter>(
    lab: &Lab,
    a: &A,
    example_dir: &str,
    n: usize,
) -> (Discovered, FrozenPlan) {
    let listing = run_invocation(&lab.ctx, &a.discover_command(), &example(example_dir)).unwrap();
    let d = a.discover(&listing).unwrap();
    assert!(d.complete, "{d:?}");
    let p = partition_tests(a, &d, &History::default(), n, &PartitionOpts::default());
    assert_eq!(p.mode, SplitMode::Listed, "{p:?}");
    assert_eq!(p.shards.len(), n);
    let fp = lab
        .fp
        .with_shards(&[ShardRequest {
            workflow: F.into(),
            job: "test".into(),
            plan: p,
            step: 0,
            discovered: d.clone(),
            fail_fast: false,
        }])
        .unwrap();
    assert!(
        fp.shard_groups.iter().any(|g| g.split),
        "{:?}",
        fp.shard_groups
    );
    (d, fp)
}

/// Run each shard task of `fp` with `execute_task` and check that each ran
/// the tests it was given and no others; return what they reported.
fn run_shards(lab: &Lab, fp: &FrozenPlan) -> Vec<ShardOutcome> {
    let group = fp.shard_groups.iter().find(|g| g.split).unwrap();
    let mut outcomes = Vec::new();
    for (i, id) in group.shards.iter().enumerate() {
        let r = local_ci::execute_task(
            &lab.ctx,
            fp,
            id,
            &TaskInputs::default(),
            None,
            &lab.ws,
            lab.opts(),
        )
        .unwrap();
        let o = r.shard.expect("a shard task reports its shard");
        let mut ran: Vec<String> = o.tests.iter().map(|t| t.identity.clone()).collect();
        ran.sort();
        assert_eq!(ran, group.assignments[i], "shard {id} ran other tests");
        assert!(o.parsed, "{o:?}");
        assert_eq!(
            (o.job_id.as_str(), o.shard_id.as_str()),
            ("test", id.as_str())
        );
        outcomes.push(o);
    }
    outcomes
}

/// The shards together ran every listed test once.
fn assert_covers(fp: &FrozenPlan, d: &Discovered, outcomes: &[ShardOutcome]) {
    let group = fp.shard_groups.iter().find(|g| g.split).unwrap();
    let agg = aggregate_shard_group(group, outcomes);
    let mut ran: Vec<&str> = agg
        .merged
        .tests
        .iter()
        .map(|t| t.identity.as_str())
        .collect();
    ran.sort();
    assert_eq!(ran, d.tests);
    let reported: usize = outcomes.iter().map(|o| o.tests.len()).sum();
    assert_eq!(reported, d.tests.len(), "a test ran in two shards");
    assert!(
        agg.missing.is_empty() && agg.unexpected.is_empty(),
        "{agg:?}"
    );
}

// A39: each shard runs the tests it was given and the shards together run all of them once.
#[test]
fn cargo_shards_run_their_own_tests_and_all_of_them_once() {
    let l = lab("rust", "cargo test", "");
    let a = CargoTest::from_command("test", "cargo test", Safety::default()).unwrap();
    let (d, fp) = split(&l, &a, "rust", 2);
    assert_eq!(d.tests.len(), 6);
    let outcomes = run_shards(&l, &fp);
    assert_covers(&fp, &d, &outcomes);
    let group = fp.shard_groups.iter().find(|g| g.split).unwrap();
    let agg = aggregate_shard_group(group, &outcomes);
    assert!(agg.ok(), "{:?}", agg.problems());
    assert_eq!(agg.merged.passed, 5);
    assert_eq!(agg.merged.skipped, ["tests::slow_one"]);
}

// A40: the job a split plan is run as is its shards together: a failing test of one shard
// fails the job, and what needs the job sees that.
#[test]
fn a_failing_test_in_one_shard_fails_the_job() {
    let l = lab(
        "rust",
        "cargo test",
        "        env:\n          SHARD_EXAMPLE_FAIL: '1'\n",
    );
    let a = CargoTest::from_command("test", "cargo test", Safety::default()).unwrap();
    let (d, fp) = split(&l, &a, "rust", 3);
    let outcomes = run_shards(&l, &fp);
    assert_covers(&fp, &d, &outcomes);
    let group = fp.shard_groups.iter().find(|g| g.split).unwrap();
    let agg = aggregate_shard_group(group, &outcomes);
    assert_eq!(agg.merged.failed, ["fails_on_purpose_when_asked"]);
    assert!(!agg.ok());
}

#[test]
fn a_run_of_the_plan_runs_each_shard_and_the_job_that_needs_them_after() {
    let l = lab("rust", "cargo test", "");
    let a = CargoTest::from_command("test", "cargo test", Safety::default()).unwrap();
    let (_, fp) = split(&l, &a, "rust", 2);
    let r = local_ci::execute_plan(&l.ctx, &fp, &l.ws, l.opts()).unwrap();
    let jobs: Vec<(&str, &Outcome)> = r
        .units
        .iter()
        .flat_map(|u| u.jobs.iter())
        .map(|j| (j.display.as_str(), &j.outcome))
        .collect();
    assert_eq!(
        jobs,
        [
            ("test (shard 1/2)", &Outcome::Pass),
            ("test (shard 2/2)", &Outcome::Pass),
            ("after", &Outcome::Pass)
        ]
    );
    let after = r
        .units
        .iter()
        .flat_map(|u| u.outcomes.iter())
        .find(|o| o.job == "test")
        .unwrap();
    assert_eq!(after.status, local_ci::task::JobStatus::Success);

    // one shard failing makes the job fail, whichever shard ran last
    let l = lab(
        "rust",
        "cargo test",
        "        env:\n          SHARD_EXAMPLE_FAIL: '1'\n",
    );
    let (_, fp) = split(&l, &a, "rust", 2);
    let r = local_ci::execute_plan(&l.ctx, &fp, &l.ws, l.opts()).unwrap();
    let test = r
        .units
        .iter()
        .flat_map(|u| u.outcomes.iter())
        .find(|o| o.job == "test")
        .unwrap();
    assert_eq!(test.status, local_ci::task::JobStatus::Failure);
    let after = r
        .units
        .iter()
        .flat_map(|u| u.jobs.iter())
        .find(|j| j.job == "after")
        .unwrap();
    assert!(
        matches!(after.outcome, Outcome::Skip(_)),
        "{:?}",
        after.outcome
    );
}

#[test]
fn vitest_shards_run_their_own_files_and_all_of_them_once() {
    let node = example("node");
    let l = lab("node", "npx vitest run", "");
    let a = Vitest::from_command(
        "2.1.9",
        node.to_str().unwrap(),
        "npx vitest run",
        Safety::default(),
    )
    .unwrap();
    let (d, fp) = split(&l, &a, "node", 2);
    assert_eq!(d.tests.len(), 3);
    let outcomes = run_shards(&l, &fp);
    assert_covers(&fp, &d, &outcomes);
    assert!(
        aggregate_shard_group(fp.shard_groups.iter().find(|g| g.split).unwrap(), &outcomes).ok()
    );
}

#[test]
fn jest_shards_run_their_own_files_and_all_of_them_once() {
    let node = example("node");
    let l = lab("node", "npx jest", "");
    let a = Jest::from_command(
        "29.7.0",
        node.to_str().unwrap(),
        "npx jest",
        Safety::default(),
    )
    .unwrap();
    let (d, fp) = split(&l, &a, "node", 3);
    let outcomes = run_shards(&l, &fp);
    assert_covers(&fp, &d, &outcomes);
    assert!(
        aggregate_shard_group(fp.shard_groups.iter().find(|g| g.split).unwrap(), &outcomes).ok()
    );
    // a failing file fails its shard and is named
    let l = lab(
        "node",
        "npx jest",
        "        env:\n          SHARD_EXAMPLE_FAIL: '1'\n",
    );
    let (_, fp) = split(&l, &a, "node", 3);
    let outcomes = run_shards(&l, &fp);
    let agg = aggregate_shard_group(fp.shard_groups.iter().find(|g| g.split).unwrap(), &outcomes);
    assert_eq!(agg.merged.failed, ["jest-tests/text.test.js"]);
    assert!(outcomes.iter().any(|o| !o.exit_ok));
    assert!(outcomes
        .iter()
        .flat_map(|o| &o.tests)
        .any(|t| t.status == TestStatus::Failed));
}

// A task that cannot run its shard's command is not run as the job: it would run
// every test once per shard and report that as the shard.
#[test]
fn a_shard_task_that_cannot_run_its_command_is_refused() {
    let l = lab("rust", "cargo test", "");
    let a = CargoTest::from_command("test", "cargo test", Safety::default()).unwrap();
    let (_, fp) = split(&l, &a, "rust", 2);
    let id = fp
        .tasks
        .iter()
        .find(|t| t.shard.is_some())
        .unwrap()
        .id
        .clone();
    let run = |fp: &FrozenPlan| {
        local_ci::execute_task(
            &l.ctx,
            fp,
            &id,
            &TaskInputs::default(),
            None,
            &l.ws,
            l.opts(),
        )
        .map(|_| ())
        .unwrap_err()
        .to_string()
    };
    let edit = |f: &dyn Fn(&mut local_ci::dag::TaskShard)| {
        let mut p = fp.clone();
        let t = p.tasks.iter_mut().find(|t| t.id == id).unwrap();
        f(t.shard.as_mut().unwrap());
        p
    };
    for p in [
        edit(&|s| s.step = 7),
        edit(&|s| s.runner.runner = "mocha".into()),
        edit(&|s| s.tests.clear()),
        edit(&|s| s.mode = SplitMode::Single),
    ] {
        assert!(run(&p).contains("shard of a split job"), "{}", run(&p));
    }
    // a step that is another command than the plan's runner was made from is not connected
    let (_, fp2) = {
        let l2 = lab("rust", "cargo test -- --skip adds", "");
        split_unfiltered(&l2)
    };
    assert!(fp2
        .shard_groups
        .iter()
        .any(|g| g.not_split.contains(&NotSplit::NotConnectable)
            || g.not_split.contains(&NotSplit::JobFilter)));
}

/// A request made for a job whose command is not the one the plan's runner was made from.
fn split_unfiltered(l: &Lab) -> ((), FrozenPlan) {
    let a = CargoTest::new("test", &[], Safety::default());
    let d = Discovered {
        tests: vec!["a".into(), "b".into()],
        complete: true,
    };
    let p = partition_tests(&a, &d, &History::default(), 2, &PartitionOpts::default());
    let fp =
        l.fp.with_shards(&[ShardRequest {
            workflow: F.into(),
            job: "test".into(),
            plan: p,
            step: 0,
            discovered: d,
            fail_fast: false,
        }])
        .unwrap();
    ((), fp)
}
