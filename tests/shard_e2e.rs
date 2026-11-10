//! A41: the jobs that are not split, read from the workflow (containers,
//! servers, services, coverage, a job's own filters), and the end-to-end
//! suites that are (`tests/shard_browsers.rs` runs those for real).

#![allow(clippy::disallowed_methods)] // the tests start git themselves

mod common;
use local_ci::dag::{NotSplit, ShardRequest};
use local_ci::shard::adapters::{Cypress, Isolation, Playwright, Safety, Vitest};
use local_ci::shard::{
    partition_tests, Discovered, History, PartitionOpts, RunnerAdapter, SplitMode,
};
use local_ci::{ExecContext, FrozenPlan, Quiet, Target};
use std::path::Path;
use std::process::Command;

const F: &str = ".github/workflows/ci.yml";

fn declared() -> Isolation {
    Isolation::Declared {
        evidence: "the plan test starts nothing".into(),
    }
}

// ---- the plan ----

const JOBS: &str = "  e2e:
    runs-on: x
    steps:
      - run: npx playwright test
  cy:
    runs-on: x
    steps:
      - run: npx cypress run
  compose:
    runs-on: x
    steps:
      - run: docker compose up -d
      - run: npx playwright test
  server:
    runs-on: x
    steps:
      - run: npm start &
      - run: npx playwright test
  svc:
    runs-on: x
    services:
      pg:
        image: postgres:16
    steps:
      - run: npx playwright test
  soft:
    runs-on: x
    steps:
      - run: npx playwright test
        continue-on-error: true
  cov:
    runs-on: x
    steps:
      - run: npx vitest run --coverage
  filt:
    runs-on: x
    steps:
      - run: npx playwright test --grep smoke
  covenv:
    runs-on: x
    env:
      COVERAGE: '1'
    steps:
      - run: npx playwright test
  pnpmdev:
    runs-on: x
    steps:
      - run: pnpm dev
      - run: npx playwright test
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

/// The plan of the jobs above; `top` is added to the workflow before its jobs.
fn plan(top: &str) -> (common::Tmp, FrozenPlan) {
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(
        dir.join(F),
        format!("name: ci\non: pull_request\n{top}jobs:\n{JOBS}"),
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

fn request<A: RunnerAdapter>(job: &str, step: usize, a: &A, tests: &[&str]) -> ShardRequest {
    let d = Discovered {
        tests: tests.iter().map(|s| s.to_string()).collect(),
        complete: true,
        ..Default::default()
    };
    let p = partition_tests(a, &d, &History::default(), 2, &PartitionOpts::default());
    assert_eq!(p.mode, SplitMode::Listed, "{p:?}");
    ShardRequest {
        workflow: F.into(),
        job: job.into(),
        plan: p,
        step,
        dir: String::new(),
        discovered: d,
        fail_fast: true,
    }
}

#[test]
fn a41_a_job_that_shares_a_container_a_server_or_a_service_is_one_job_with_the_reason() {
    let (_t, fp) = plan("");
    let pw = Playwright::new("1.63.0", &[], declared());
    let tests = ["[p] › a.spec.js › one", "[p] › b.spec.js › one"];
    let cy = Cypress::new("1", &[], declared());
    let vi = Vitest::new("1", "/w", &[], Safety::default());
    let requests = [
        request("e2e", 0, &pw, &tests),
        request(
            "cy",
            0,
            &cy,
            &["cypress/e2e/a.cy.js", "cypress/e2e/b.cy.js"],
        ),
        request("compose", 1, &pw, &tests),
        request("server", 1, &pw, &tests),
        request("svc", 0, &pw, &tests),
        request("soft", 0, &pw, &tests),
        request("cov", 0, &vi, &["a.test.js", "b.test.js"]),
        request("filt", 0, &pw, &tests),
        request("covenv", 0, &pw, &tests),
        request("pnpmdev", 1, &pw, &tests),
    ];
    let sp = fp.with_shards(&requests).unwrap();
    sp.verify().unwrap();
    let group = |job: &str| {
        sp.shard_groups
            .iter()
            .find(|g| g.source_jobs == [job])
            .unwrap()
    };
    assert!(group("e2e").split && group("e2e").not_split.is_empty());
    assert!(group("cy").split);
    for (job, why) in [
        ("compose", NotSplit::NoIsolation),
        ("server", NotSplit::NoIsolation),
        ("svc", NotSplit::HasServices),
        ("soft", NotSplit::NotConnectable),
        ("cov", NotSplit::CoverageNotMerged),
        ("covenv", NotSplit::CoverageNotMerged),
        ("pnpmdev", NotSplit::NoIsolation),
        ("filt", NotSplit::JobFilter),
    ] {
        let g = group(job);
        assert!(!g.split, "{job}");
        assert!(g.not_split.contains(&why), "{job}: {:?}", g.not_split);
        // one task, the whole job
        assert_eq!(g.shards.len(), 1, "{job}");
    }
}

#[test]
fn a41_a_port_in_the_environment_of_the_whole_workflow_is_one_job_with_the_reason() {
    let (_t, fp) = plan("env:\n  API_PORT: '3000'\n");
    let pw = Playwright::new("1.63.0", &[], declared());
    let tests = ["[p] › a.spec.js › one", "[p] › b.spec.js › one"];
    let sp = fp.with_shards(&[request("e2e", 0, &pw, &tests)]).unwrap();
    let g = sp
        .shard_groups
        .iter()
        .find(|g| g.source_jobs == ["e2e"])
        .unwrap();
    assert!(!g.split);
    assert!(
        g.not_split.contains(&NotSplit::NoIsolation),
        "{:?}",
        g.not_split
    );
}
