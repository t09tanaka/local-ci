//! `plan_task_demand`: the commands a task will start, listed from the plan.

#![allow(clippy::disallowed_methods)] // the tests build their own commands

mod common;
use local_ci::dag::ShardRequest;
use local_ci::demand::{DemandNode, SCRIPT};
use local_ci::shard::adapters::{CargoTest, Safety, Vitest};
use local_ci::shard::{partition_tests, Discovered, History, PartitionOpts, RunnerAdapter};
use local_ci::wire::Digested;
use local_ci::{plan_task_demand, ExecContext, FrozenPlan, Quiet, Target};
use std::path::{Path, PathBuf};
use std::process::Command;

const F: &str = ".github/workflows/ci.yml";

const JOBS: &str = "  plain:
    runs-on: x
    steps:
      - name: First
        run: echo one
      - uses: actions/checkout@v4
      - run: git push origin HEAD
      - name: Second ${{ github.sha }}
        run: echo two
        shell: sh
        timeout-minutes: 3
        working-directory: sub
      - run: print(1)
        shell: python
      - if: always()
        run: echo cleanup
      - run: sudo apt-get install -y foo
      - run: echo custom
        shell: bash {0}
  gated:
    if: github.ref == 'x'
    runs-on: x
    steps:
      - run: echo gated
  dynmx:
    runs-on: x
    strategy:
      matrix: ${{ fromJSON('[1, 2]') }}
    steps:
      - run: echo dyn
  inst:
    runs-on: x
    steps:
      - run: npm ci
      - run: npm test
  svc:
    runs-on: x
    services:
      pg:
        image: postgres:16
    steps:
      - run: cargo test
  test:
    runs-on: x
    steps:
      - run: cargo test
  vt:
    runs-on: x
    defaults:
      run:
        working-directory: web
    steps:
      - run: npx vitest run
  chain:
    runs-on: x
    steps:
      - run: echo a
      - run: echo b
  twice:
    runs-on: x
    steps:
      - run: echo t
";

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

fn plan() -> (common::Tmp, FrozenPlan) {
    let tmp = common::tempdir().unwrap();
    let dir: PathBuf = tmp.path().join("work");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(
        dir.join(F),
        format!("name: ci\non: pull_request\njobs:\n{JOBS}"),
    )
    .unwrap();
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "feature"]);
    std::fs::write(dir.join("a.txt"), "b\n").unwrap();
    git(&dir, &["commit", "-qam", "change"]);
    let ctx = ExecContext::new(
        [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", &tmp.path().to_string_lossy()),
            ("LOCAL_CI_HOME", &tmp.path().join("cache").to_string_lossy()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    );
    let t = Target {
        base: Some("main".into()),
        sha: "HEAD".into(),
        all: false,
        include_disabled: true,
        workflows_dirs: vec![],
        workflows: vec![],
        jobs: vec![],
        dir,
    };
    let fp = local_ci::build_frozen_plan(&ctx, &t, &mut Quiet).unwrap();
    (tmp, fp)
}

fn id_of(job: &str) -> String {
    format!("{F}::{job}")
}

fn nodes(fp: &FrozenPlan, job: &str) -> Vec<DemandNode> {
    plan_task_demand(fp, &id_of(job)).unwrap().nodes
}

fn request(job: &str, a: &dyn RunnerAdapter, tests: &[&str], dir: &str) -> ShardRequest {
    let d = Discovered {
        tests: tests.iter().map(|s| s.to_string()).collect(),
        complete: true,
    };
    let plan = partition_tests(a, &d, &History::default(), 2, &PartitionOpts::default());
    ShardRequest {
        workflow: F.into(),
        job: job.into(),
        plan,
        step: 0,
        dir: dir.into(),
        discovered: d,
        fail_fast: true,
    }
}

#[test]
fn the_commands_of_a_task_come_in_order_with_what_is_known_of_them() {
    let (_t, fp) = plan();
    let d = plan_task_demand(&fp, &id_of("plain")).unwrap();
    assert_eq!(d.task_id, id_of("plain"));
    assert_eq!(d.schema_version, local_ci::wire::WIRE_SCHEMA_VERSION);
    let ids: Vec<&str> = d.nodes.iter().map(|n| n.step_id.as_str()).collect();
    // a `uses:` step and a push are not commands of the task
    assert_eq!(
        ids,
        ["plain#0", "plain#3", "plain#4", "plain#5", "plain#6", "plain#7"]
    );
    let n = &d.nodes;
    assert!(n[0].depends_on.is_empty());
    for i in 1..n.len() {
        assert_eq!(n[i].depends_on, [n[i - 1].step_id.clone()]);
    }
    // a label without an expression gives the key a run hands its executor
    assert_eq!(n[0].label, "First");
    assert_eq!(n[0].key.as_deref(), Some("local-ci ci.yml plain First"));
    assert_eq!(n[0].cwd, "");
    assert_eq!(
        n[0].argv,
        ["bash", "--noprofile", "--norc", "-eo", "pipefail", SCRIPT]
    );
    assert!(!n[0].conditional);
    // an expression in the label is read when the step runs
    assert_eq!(n[1].label, "Second ${{ github.sha }}");
    assert_eq!(n[1].key, None);
    assert_eq!(n[1].cwd, "sub");
    assert_eq!(n[1].argv, ["sh", "-e", SCRIPT]);
    assert_eq!(n[1].weight.timeout_minutes, Some(3.0));
    assert!(!n[1].conditional);
    assert_eq!(n[2].argv, ["python3", SCRIPT]);
    // an `if:` may keep a step from running
    assert!(n[3].conditional);
    // an install that is not done here may be skipped
    assert!(n[4].conditional);
    assert_eq!(n[5].argv, ["bash", SCRIPT]);
    assert!(!n[5].conditional);
    // the same text twice
    assert_eq!(plan_task_demand(&fp, &id_of("plain")).unwrap(), d);
    assert_eq!(d.digest().unwrap(), d.digest().unwrap());
    d.verify_digest(&d.digest().unwrap()).unwrap();
}

#[test]
fn what_is_settled_only_when_the_task_runs_is_marked() {
    let (_t, fp) = plan();
    // a job with an `if:`
    assert!(nodes(&fp, "gated").iter().all(|n| n.conditional));
    // a matrix that is an expression: the task expands and runs it all
    assert!(nodes(&fp, "dynmx").iter().all(|n| n.conditional));
    // the steps a receipt may have done
    let inst = nodes(&fp, "inst");
    assert!(inst[0].conditional, "{inst:?}");
    assert!(!inst[1].conditional);
    // plain steps are not
    assert!(nodes(&fp, "chain").iter().all(|n| !n.conditional));
    // a job with services is a heavier one
    let w = &nodes(&fp, "svc")[0].weight;
    assert_eq!((w.services, w.tests), (1, None));
    assert_eq!(nodes(&fp, "twice")[0].weight.services, 0);
}

#[test]
fn a_shard_task_lists_its_step_as_the_shell_on_a_script() {
    let (_t, fp) = plan();
    let a = CargoTest::from_command("1", "cargo test", Safety::default()).unwrap();
    let r = request("test", &a, &["a", "b", "c", "d"], "");
    let sp = fp.with_shards(&[r]).unwrap();
    let shards: Vec<_> = sp.tasks.iter().filter(|t| t.shard.is_some()).collect();
    assert_eq!(shards.len(), 2);
    // the form of an ordinary step of the same shell
    let plain = nodes(&sp, "chain")[0].argv.clone();
    assert_eq!(plain.last().unwrap(), SCRIPT);
    for t in &shards {
        let d = plan_task_demand(&sp, &t.id).unwrap();
        let ts = t.shard.as_ref().unwrap();
        let mut seven = d.clone();
        seven.schema_version = 7;
        assert_eq!(Digested::schema_version(&seven), 7);
        assert_eq!(d.nodes.len(), 1);
        let n = &d.nodes[0];
        assert_eq!(n.argv, plain);
        assert_eq!(n.weight.tests, Some(ts.tests.len()));
    }
    // a working directory below the workspace is the step's, and what the
    // command is built from is not in the listing
    let v = Vitest::from_command("2", "/p", "npx vitest run", Safety::default()).unwrap();
    let r = ShardRequest {
        dir: "web".into(),
        ..request("vt", &v, &["a.test.js", "b.test.js"], "web")
    };
    let sp = fp.with_shards(&[r]).unwrap();
    let t = sp.tasks.iter().find(|t| t.shard.is_some()).unwrap();
    let n = &plan_task_demand(&sp, &t.id).unwrap().nodes[0];
    assert_eq!(n.cwd, "web");
    assert_eq!(n.argv, plain);
}

#[test]
fn a_working_directory_that_leaves_the_workspace_is_not_listed() {
    let (_t, fp) = plan();
    let e = |fp: &FrozenPlan, id: &str| plan_task_demand(fp, id).unwrap_err().to_string();
    for dir in ["/etc", "../x", "a/../../x", "a/../b/..", "C:/x"] {
        let mut bad = fp.clone();
        bad.workflows[0]
            .jobs
            .iter_mut()
            .find(|j| j.id == "chain")
            .unwrap()
            .working_directory = Some(dir.into());
        let m = e(&bad, &id_of("chain"));
        assert!(
            m.contains("not a directory below the workspace"),
            "{dir}: {m}"
        );
    }
    // `./a//b` is read as `a/b`
    let mut ok = fp.clone();
    ok.workflows[0]
        .jobs
        .iter_mut()
        .find(|j| j.id == "chain")
        .unwrap()
        .working_directory = Some("./a//b".into());
    assert_eq!(nodes(&ok, "chain")[0].cwd, "a/b");

    // a shard task: its directory is the step's and below the workspace
    let a = CargoTest::from_command("1", "cargo test", Safety::default()).unwrap();
    let sp = fp
        .with_shards(&[request("test", &a, &["a", "b", "c", "d"], "")])
        .unwrap();
    let i = sp.tasks.iter().position(|t| t.shard.is_some()).unwrap();
    let id = sp.tasks[i].id.clone();
    for dir in ["/etc", "../x", "a\\b"] {
        let mut bad = sp.clone();
        bad.tasks[i].shard.as_mut().unwrap().dir = dir.into();
        let m = e(&bad, &id);
        assert!(
            m.contains("not a directory below the workspace"),
            "{dir}: {m}"
        );
    }
    let mut bad = sp.clone();
    bad.tasks[i].shard.as_mut().unwrap().dir = "web".into();
    let m = e(&bad, &id);
    assert!(m.contains("where its tests were listed"), "{m}");
}

#[test]
fn a_task_that_could_not_run_is_not_listed() {
    let (_t, fp) = plan();
    let e = |fp: &FrozenPlan, id: &str| plan_task_demand(fp, id).unwrap_err().to_string();
    assert!(e(&fp, "nope").contains("no task `nope`"));

    let mut bad = fp.clone();
    let i = bad
        .tasks
        .iter()
        .position(|t| t.id == id_of("chain"))
        .unwrap();
    bad.tasks[i].workflow = "other.yml".into();
    assert!(e(&bad, &id_of("chain")).contains("no workflow other.yml"));

    let mut bad = fp.clone();
    bad.tasks[i].jobs.push("ghost".into());
    assert!(e(&bad, &id_of("chain")).contains("job `ghost`"));

    // a shell that cannot be run, or read
    for (shell, want) in [
        ("zsh", "unsupported shell `zsh`"),
        ("sh -c '{0}", "bad shell"),
    ] {
        let mut bad = fp.clone();
        let job = bad.workflows[0]
            .jobs
            .iter_mut()
            .find(|j| j.id == "chain")
            .unwrap();
        job.steps[0].shell = Some(shell.into());
        assert!(e(&bad, &id_of("chain")).contains(want), "{shell}");
    }

    // a shard that has no command
    let a = CargoTest::from_command("1", "cargo test", Safety::default()).unwrap();
    let sp = fp
        .with_shards(&[request("test", &a, &["a", "b", "c", "d"], "")])
        .unwrap();
    let id = sp
        .tasks
        .iter()
        .find(|t| t.shard.is_some())
        .unwrap()
        .id
        .clone();
    let at = |sp: &FrozenPlan| sp.tasks.iter().position(|t| t.id == id).unwrap();
    let mut bad = sp.clone();
    let k = at(&bad);
    bad.tasks[k].shard.as_mut().unwrap().step = 9;
    assert!(e(&bad, &id).contains("runs no command"));
    let mut bad = sp.clone();
    let k = at(&bad);
    bad.tasks[k].shard.as_mut().unwrap().runner.runner = "mocha".into();
    assert!(e(&bad, &id).contains("no adapter for runner `mocha`"));
    let mut bad = sp.clone();
    let k = at(&bad);
    let ts = bad.tasks[k].shard.as_mut().unwrap();
    ts.mode = local_ci::shard::SplitMode::Listed;
    ts.tests = vec!["a\0b".into()];
    assert!(e(&bad, &id).contains("cannot be put in a command"));
}
