//! Splitting a job into shard tasks in the plan, and putting the results of the
//! shards together.

#![allow(clippy::disallowed_methods)] // the tests build their own commands

mod common;
use local_ci::complete::aggregate_shard_group;
use local_ci::dag::{shard_task_id, NotSplit, OutputAggregation, ShardRequest};
use local_ci::shard::adapters::{CargoTest, Safety};
use local_ci::shard::{
    partition_tests, Discovered, History, PartitionOpts, RunnerAdapter, ShardOutcome, ShardPlan,
    SplitMode, TestResult, TestStatus,
};
use local_ci::{ExecContext, FrozenPlan, Quiet, Target};
use std::path::{Path, PathBuf};
use std::process::Command;

const F: &str = ".github/workflows/ci.yml";

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

const JOBS: &str = "  test:
    runs-on: x
    steps:
      - run: cargo test
  after:
    needs: test
    runs-on: x
    steps:
      - run: echo after
  db:
    runs-on: x
    services:
      pg:
        image: postgres:16
    steps:
      - run: cargo test
  mx:
    runs-on: x
    strategy:
      matrix:
        n: [1, 2]
    steps:
      - run: cargo test
  dynmx:
    runs-on: x
    strategy:
      matrix: ${{ fromJSON('[1, 2]') }}
    steps:
      - run: cargo test
";

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

fn listing(names: &[&str]) -> Discovered {
    Discovered {
        tests: names.iter().map(|s| s.to_string()).collect(),
        complete: true,
    }
}

fn request(job: &str, plan: ShardPlan, d: Discovered) -> ShardRequest {
    ShardRequest {
        workflow: F.into(),
        job: job.into(),
        plan,
        discovered: d,
        fail_fast: true,
    }
}

fn split_request(job: &str, shards: usize) -> ShardRequest {
    let d = listing(&["a", "b", "c", "d"]);
    let a = CargoTest::new("1", &[], Safety::default());
    let p = partition_tests(
        &a,
        &d,
        &History::default(),
        shards,
        &PartitionOpts::default(),
    );
    request(job, p, d)
}

fn group<'a>(fp: &'a FrozenPlan, job: &str) -> &'a local_ci::dag::ShardGroup {
    fp.shard_groups
        .iter()
        .find(|g| g.source_jobs == [job])
        .unwrap()
}

#[test]
fn nothing_is_split_unless_asked() {
    let (_t, fp) = plan();
    let g = group(&fp, "test");
    assert!(!g.split);
    assert_eq!(g.not_split, vec![NotSplit::NoRunnerAdapter]);
    assert!(g.runner.is_none() && g.manifest.is_empty());
    assert!(fp.tasks.iter().all(|t| t.shard.is_none()));
    // the same plan again, without a request
    assert_eq!(fp.with_shards(&[]).unwrap(), fp);
}

#[test]
fn a_request_makes_one_task_per_shard() {
    let (_t, fp) = plan();
    let r = split_request("test", 2);
    let sp = fp.with_shards(std::slice::from_ref(&r)).unwrap();
    sp.verify().unwrap();
    let tasks: Vec<_> = sp.tasks.iter().filter(|t| t.jobs == ["test"]).collect();
    assert_eq!(tasks.len(), 2);
    let ids: Vec<String> = (0..2).map(|i| shard_task_id(F, "test", i)).collect();
    assert_eq!(tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), ids);
    let mut covered: Vec<String> = vec![];
    for (i, t) in tasks.iter().enumerate() {
        let s = t.shard.as_ref().unwrap();
        assert_eq!((s.index, s.count, s.mode), (i, 2, SplitMode::Listed));
        assert_eq!(s.group, format!("{F}::test"));
        assert_eq!(t.nodes, vec![format!("{F}::test")]);
        covered.extend(s.tests.clone());
    }
    covered.sort();
    assert_eq!(covered, ["a", "b", "c", "d"]);
    // what needs the job waits for every shard
    let after = sp.tasks.iter().find(|t| t.jobs == ["after"]).unwrap();
    assert_eq!(after.depends_on, ids);
    // the group says how they are merged
    let g = group(&sp, "test");
    assert!(g.split && g.not_split.is_empty());
    assert_eq!(g.shards, ids);
    assert_eq!(g.aggregation, Some(OutputAggregation::TestIdentity));
    assert_eq!(g.manifest, ["a", "b", "c", "d"]);
    assert!(g.fail_fast);
    assert_eq!(g.runner.as_ref().unwrap().runner, "cargo-test");
    // the other jobs stay as they were
    assert!(!group(&sp, "after").split);
    assert_eq!(
        group(&sp, "after").not_split,
        vec![NotSplit::NoRunnerAdapter]
    );
    // the plan survives JSON, and re-sealing it with another selection keeps the split
    let back = FrozenPlan::from_json(&sp.to_canonical_json().unwrap()).unwrap();
    assert_eq!(back, sp);
    let again = sp.with_plan(sp.to_plan()).unwrap();
    assert_eq!(again, sp);
    assert_eq!(sp.shard_requests().len(), 1);
    assert!(sp.shard_requests()[0].discovered.complete);
}

#[test]
fn a_native_split_lists_no_tests() {
    let (_t, fp) = plan();
    let d = Discovered {
        tests: vec![],
        complete: false,
    };
    let a = local_ci::shard::adapters::Vitest::new("2", "/p", &[], Safety::default());
    let p = partition_tests(&a, &d, &History::default(), 3, &PartitionOpts::default());
    assert_eq!(p.mode, SplitMode::Native);
    let sp = fp.with_shards(&[request("test", p, d)]).unwrap();
    sp.verify().unwrap();
    let g = group(&sp, "test");
    assert!(g.split && g.manifest.is_empty());
    assert_eq!(g.shards.len(), 3);
    assert_eq!(sp.with_plan(sp.to_plan()).unwrap(), sp);
    assert!(!sp.shard_requests()[0].discovered.complete);
}

#[test]
fn a_split_that_does_not_cover_the_tests_is_refused() {
    let (_t, fp) = plan();
    let mut r = split_request("test", 2);
    r.plan.shards[0].tests.pop();
    let sp = fp.with_shards(&[r]).unwrap();
    assert!(sp.tasks.iter().all(|t| t.shard.is_none()));
    let g = group(&sp, "test");
    assert!(!g.split);
    assert_eq!(g.not_split, vec![NotSplit::ManifestMismatch]);
}

#[test]
fn what_cannot_be_split_says_why() {
    let (_t, fp) = plan();
    // the runner is known and the split was not asked for
    let d = listing(&["a", "b"]);
    let a = CargoTest::new("1", &[], Safety::default());
    let p = partition_tests(&a, &d, &History::default(), 1, &PartitionOpts::default());
    let sp = fp.with_shards(&[request("test", p, d.clone())]).unwrap();
    let g = group(&sp, "test");
    assert_eq!(g.not_split, vec![NotSplit::NotRequested]);
    assert_eq!(g.runner.as_ref().unwrap().version, "1");
    // an end-to-end job whose services cannot be given to each shard
    let shared = Safety {
        shared_state: true,
        isolated_fixtures: false,
    };
    let a = CargoTest::new("1", &[], shared);
    let p = partition_tests(&a, &d, &History::default(), 2, &PartitionOpts::default());
    let sp = fp.with_shards(&[request("test", p, d.clone())]).unwrap();
    assert_eq!(
        group(&sp, "test").not_split,
        vec![NotSplit::SharedState, NotSplit::NoIsolation]
    );
    // a job with services is not split even when the runner could
    let sp = fp.with_shards(&[split_request("db", 2)]).unwrap();
    assert!(!group(&sp, "db").split);
    assert_eq!(group(&sp, "db").not_split, vec![NotSplit::HasServices]);
    // nor a matrix
    let sp = fp
        .with_shards(&[split_request("mx", 2), split_request("dynmx", 2)])
        .unwrap();
    assert!(sp.tasks.iter().all(|t| t.shard.is_none()));
    let dynamic = group(&sp, "dynmx");
    assert_eq!(dynamic.not_split, vec![NotSplit::MatrixVariant]);
    for g in sp.shard_groups.iter().filter(|g| g.source_jobs == ["mx"]) {
        assert_eq!(
            g.not_split,
            vec![NotSplit::NoRunnerAdapter, NotSplit::MatrixVariant]
        );
    }
    // a request for a job the plan does not have changes nothing
    let sp = fp.with_shards(&[split_request("nope", 2)]).unwrap();
    assert_eq!(sp, fp);
}

#[test]
fn a_plan_of_an_older_schema_is_refused_by_name() {
    let (_t, fp) = plan();
    assert_eq!(fp.schema_version, 5);
    let text = fp.to_canonical_json().unwrap();
    let old = text
        .replace("\"schema_version\":5", "\"schema_version\":4")
        .replace(",\"shard\":null", "");
    assert!(!old.contains("\"shard\""));
    let err = FrozenPlan::from_json(&old).unwrap_err().to_string();
    assert!(err.contains("schema version 4"), "{err}");
}

fn all(t: &[String]) -> Vec<(&str, TestStatus)> {
    t.iter().map(|n| (n.as_str(), TestStatus::Passed)).collect()
}

fn outcome(job: &str, shard: &str, ok: bool, tests: &[(&str, TestStatus)]) -> ShardOutcome {
    ShardOutcome {
        job_id: job.into(),
        shard_id: shard.into(),
        tests: tests
            .iter()
            .map(|(i, s)| TestResult {
                identity: i.to_string(),
                status: *s,
            })
            .collect(),
        exit_ok: ok,
    }
}

#[test]
fn shard_results_are_put_together_by_job_shard_and_test() {
    use TestStatus::*;
    let (_t, fp) = plan();
    let r = split_request("test", 2);
    let sp = fp.with_shards(std::slice::from_ref(&r)).unwrap();
    let g = group(&sp, "test").clone();
    let (s0, s1) = (g.shards[0].clone(), g.shards[1].clone());
    let (t0, t1) = (&r.plan.shards[0].tests, &r.plan.shards[1].tests);

    let ok = aggregate_shard_group(
        &g,
        &[
            outcome("test", &s0, true, &all(t0)),
            outcome("test", &s1, true, &all(t1)),
        ],
    );
    assert!(ok.ok(), "{:?}", ok.problems());
    assert_eq!(ok.merged.passed, 4);
    assert!(ok.cancel.is_empty());

    // a shard that has not reported: the group is not complete
    let part = aggregate_shard_group(&g, &[outcome("test", &s0, true, &all(t0))]);
    assert!(!part.ok());
    assert_eq!(part.missing, vec![s1.clone()]);
    assert!(part.cancel.is_empty());
    assert!(part.problems().iter().any(|p| p.starts_with("missing")));

    // a failed shard under fail-fast cancels the one still running
    let mut failed_tests = all(t0);
    failed_tests[0].1 = Failed;
    let ff = aggregate_shard_group(&g, &[outcome("test", &s0, false, &failed_tests)]);
    assert_eq!(ff.cancel, vec![s1.clone()]);
    assert_eq!(ff.merged.failed.len(), 1);
    assert!(ff.problems().iter().any(|p| p.starts_with("failed")));
    let mut slow = g.clone();
    slow.fail_fast = false;
    assert!(
        aggregate_shard_group(&slow, &[outcome("test", &s0, false, &failed_tests)])
            .cancel
            .is_empty()
    );

    // twice, of another job, of a shard the group does not have
    let bad = aggregate_shard_group(
        &g,
        &[
            outcome("test", &s0, true, &all(t0)),
            outcome("test", &s0, true, &all(t0)),
            outcome("other", &s1, true, &[]),
            outcome("test", "nope", true, &[]),
        ],
    );
    assert_eq!(bad.duplicate, vec![s0.clone()]);
    assert_eq!(bad.unexpected.len(), 2);
    assert!(bad.problems().iter().any(|p| p.starts_with("duplicate")));
    assert!(bad.problems().iter().any(|p| p.starts_with("unexpected")));
    assert!(bad.problems().iter().any(|p| p.starts_with("invalid")));

    // a test that no shard reported, and one the group never listed
    let m = aggregate_shard_group(
        &g,
        &[
            outcome("test", &s0, true, &all(t0)),
            outcome("test", &s1, true, &[("zzz", Passed)]),
        ],
    );
    assert!(!m.ok());
    assert!(m.merged.problems.iter().any(|p| p.contains("zzz")));
}

#[test]
fn the_adapter_merge_is_the_same_check() {
    let a = CargoTest::new("1", &[], Safety::default());
    let m = a.merge(Some(&["x".to_string()]), &[outcome("j", "s", true, &[])]);
    assert_eq!(m.problems, vec!["`x` was not reported"]);
}
