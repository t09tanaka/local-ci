//! A41: end-to-end suites (Playwright, Cypress), and the jobs that are not
//! split. Playwright runs for real in a copy of `examples/sharding/playwright`
//! (it needs `npm ci` there and `npx playwright install chromium`; without them
//! the tests fail rather than skip). Cypress is not installed: its listing is
//! run for real, and what a shard reports is the table of a Cypress run written
//! out by the test.

#![allow(clippy::disallowed_methods)] // the tests read the environment of the run

mod common;
use local_ci::dag::{NotSplit, ShardRequest};
use local_ci::shard::adapters::{run_invocation, Cypress, Playwright, Safety, Vitest};
use local_ci::shard::{
    merge_with, partition_tests, Discovered, History, Merged, PartitionOpts, Rules, RunnerAdapter,
    ShardOutcome, ShardPlan, SplitMode,
};
use local_ci::{ExecContext, FrozenPlan, Quiet, Target};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

const F: &str = ".github/workflows/ci.yml";

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/sharding")
        .join(name)
}

fn ctx(extra: &[(&str, &str)]) -> ExecContext {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    env.remove("SHARD_EXAMPLE_FAIL");
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    ExecContext::new(env)
}

/// A copy of an example to run in: Playwright clears its output directory at
/// the start of a run, so tests that run at once need directories of their own.
fn copy_of(name: &str) -> (common::Tmp, PathBuf) {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            let skip = ["node_modules", "test-results", "playwright-report"];
            if skip.contains(&e.file_name().to_str().unwrap()) {
                continue;
            }
            let dest = to.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy(&e.path(), &dest);
            } else {
                std::fs::copy(e.path(), &dest).unwrap();
            }
        }
    }
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join(name);
    copy(&example(name), &dir);
    if example(name).join("node_modules").exists() {
        std::os::unix::fs::symlink(example(name).join("node_modules"), dir.join("node_modules"))
            .unwrap();
    }
    (tmp, dir)
}

/// List, split in `n`, run every shard, merge with the check that a shard
/// reports only what it was given.
fn drive<A: RunnerAdapter>(
    a: &A,
    ctx: &ExecContext,
    dir: &Path,
    n: usize,
) -> (Discovered, ShardPlan, Vec<ShardOutcome>, Merged) {
    let listing = run_invocation(ctx, &a.discover_command(), dir).unwrap();
    let d = a.discover(&listing).unwrap();
    let plan = partition_tests(a, &d, &History::default(), n, &PartitionOpts::default());
    assert_eq!(plan.mode, SplitMode::Listed, "{plan:?}");
    assert_eq!(plan.shards.len(), n);
    plan.verify(&d).unwrap();
    let outcomes: Vec<ShardOutcome> = plan
        .shards
        .iter()
        .map(|s| {
            let inv = a.invoke(s, n, plan.mode);
            let out = run_invocation(ctx, &inv, dir).unwrap();
            a.collect("e2e", &format!("e2e@{}", s.index), &out)
        })
        .collect();
    let ids: Vec<String> = plan
        .shards
        .iter()
        .map(|s| format!("e2e@{}", s.index))
        .collect();
    let given: Vec<BTreeSet<&str>> = plan
        .shards
        .iter()
        .map(|s| s.tests.iter().map(String::as_str).collect())
        .collect();
    let rules = Rules {
        allow_unlisted: false,
        assigned: ids.iter().map(String::as_str).zip(given).collect(),
    };
    let merged = merge_with(Some(&d.tests), &rules, &outcomes);
    (d, plan, outcomes, merged)
}

#[test]
fn a41_playwright_shards_run_each_test_of_each_project_once() {
    let (_t, dir) = copy_of("playwright");
    let a = Playwright::new("1.63.0", &[], Safety::default());
    for n in [2, 5] {
        let (d, plan, outcomes, m) = drive(&a, &ctx(&[]), &dir, n);
        // three files, six tests, two projects
        assert_eq!(d.tests.len(), 12, "{d:?}");
        assert!(d
            .tests
            .contains(&"[mobile] › text.spec.js › text › upper-cases".to_string()));
        assert!(m.ok(), "{m:?}");
        assert_eq!(m.passed, 10);
        assert_eq!(
            m.skipped,
            [
                "[desktop] › viewport.spec.js › is skipped",
                "[mobile] › viewport.spec.js › is skipped"
            ]
        );
        for (s, o) in plan.shards.iter().zip(&outcomes) {
            let mut ran: Vec<&str> = o.tests.iter().map(|t| t.identity.as_str()).collect();
            ran.sort();
            assert_eq!(ran, s.tests, "a shard ran other tests than it was given");
        }
    }
    // a failing test fails its shard and the job, in each project
    let (_, _, _, m) = drive(&a, &ctx(&[("SHARD_EXAMPLE_FAIL", "1")]), &dir, 3);
    assert_eq!(
        m.failed,
        [
            "[desktop] › text.spec.js › text › fails on purpose when asked",
            "[mobile] › text.spec.js › text › fails on purpose when asked"
        ]
    );
    assert!(!m.ok());
}

#[test]
fn a41_a_playwright_suite_with_a_server_or_a_global_setup_is_not_split() {
    let (_t, dir) = copy_of("playwright");
    for (config, why) in [
        ("playwright.server.config.js", "webServer"),
        ("playwright.setup.config.js", "global setup"),
    ] {
        let a = Playwright::new("1.63.0", &["--config", config], Safety::default());
        let listing = run_invocation(&ctx(&[]), &a.discover_command(), &dir).unwrap();
        let e = a.discover(&listing).unwrap_err();
        assert!(e.contains(why), "{config}: {e}");
    }
    // and a suite whose isolation nobody declared is not split either
    let a = Playwright::new("1.63.0", &[], Safety::e2e());
    let listing = run_invocation(&ctx(&[]), &a.discover_command(), &dir).unwrap();
    let d = a.discover(&listing).unwrap();
    let p = partition_tests(&a, &d, &History::default(), 2, &PartitionOpts::default());
    assert_eq!(p.not_split, [NotSplit::NoIsolation]);
}

#[test]
fn a41_a_setup_project_runs_in_every_shard_and_the_merge_says_so() {
    let (_t, dir) = copy_of("playwright");
    let a = Playwright::new(
        "1.63.0",
        &["--config", "playwright.deps.config.js"],
        Safety::default(),
    );
    let (d, _, _, m) = drive(&a, &ctx(&[]), &dir, 2);
    assert!(d
        .tests
        .contains(&"[setup] › auth.setup.js › signs in".to_string()));
    // the dependency is not in the listing, so the job was split; the setup then
    // ran in both shards, and that does not pass as a clean run
    assert!(!m.ok(), "{m:?}");
    assert!(
        m.problems
            .iter()
            .any(|p| p.contains("auth.setup.js") && p.contains("not given it")),
        "{:?}",
        m.problems
    );
}

/// The table a Cypress run ends with, for the specs a shard was given.
fn cypress_run(specs: &str, failing: &[&str]) -> String {
    let mut t = String::from("\n  (Run Finished)\n\n  ┌──────────┐\n");
    for s in specs.split(',') {
        let (mark, pass, fail) = if failing.contains(&s) {
            ("✖", "-", "1")
        } else {
            ("✔", "1", "-")
        };
        t.push_str(&format!(
            "  │ {mark}  {s}   00:01   1   {pass}   {fail}   -   - │\n"
        ));
    }
    t.push_str("  └──────────┘\n");
    t
}

fn spec_of(argv: &[String]) -> &str {
    let at = argv.iter().position(|a| a == "--spec").unwrap();
    &argv[at + 1]
}

#[test]
fn a41_cypress_shards_run_their_spec_files_and_report_each_once() {
    let dir = example("cypress");
    let a = Cypress::new("13.0.0", dir.to_str().unwrap(), &[], Safety::default());
    let listing = run_invocation(&ctx(&[]), &a.discover_command(), &dir).unwrap();
    let d = a.discover(&listing).unwrap();
    assert_eq!(
        d.tests,
        [
            "cypress/e2e/login.cy.js",
            "cypress/e2e/nested/profile.cy.js",
            "cypress/e2e/signup.cy.js"
        ]
    );
    let p = partition_tests(&a, &d, &History::default(), 2, &PartitionOpts::default());
    assert_eq!(p.mode, SplitMode::Listed);
    p.verify(&d).unwrap();
    for failing in [None, Some("cypress/e2e/signup.cy.js")] {
        let failing: Vec<&str> = failing.into_iter().collect();
        let outcomes: Vec<ShardOutcome> = p
            .shards
            .iter()
            .map(|s| {
                let inv = a.invoke(s, 2, p.mode);
                let specs = spec_of(&inv.argv).to_string();
                assert_eq!(specs, s.tests.join(","));
                let out = local_ci::shard::CommandOutput {
                    stdout: cypress_run(&specs, &failing),
                    stderr: String::new(),
                    success: failing.is_empty()
                        || !s.tests.iter().any(|t| failing.contains(&t.as_str())),
                };
                a.collect("e2e", &format!("e2e@{}", s.index), &out)
            })
            .collect();
        let m = a.merge(Some(&d.tests), &outcomes);
        assert_eq!(m.ok(), failing.is_empty(), "{m:?}");
        assert_eq!(m.failed, failing);
        assert_eq!(m.tests.len(), 3);
    }
    // a configuration that says which files are specs is not guessed at
    let custom = example("cypress-custom");
    let listing = run_invocation(&ctx(&[]), &a.discover_command(), &custom).unwrap();
    assert!(!listing.success);
    let e = a.discover(&listing).unwrap_err();
    assert!(e.contains("chooses which files are specs"), "{e}");
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
    let dir = tmp.path().join("work");
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

fn request<A: RunnerAdapter>(job: &str, step: usize, a: &A, tests: &[&str]) -> ShardRequest {
    let d = Discovered {
        tests: tests.iter().map(|s| s.to_string()).collect(),
        complete: true,
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
    let (_t, fp) = plan();
    let pw = Playwright::new("1", &[], Safety::default());
    let tests = ["[p] › a.spec.js › one", "[p] › b.spec.js › one"];
    let cy = Cypress::new("1", "/w", &[], Safety::default());
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
        ("filt", NotSplit::JobFilter),
    ] {
        let g = group(job);
        assert!(!g.split, "{job}");
        assert!(g.not_split.contains(&why), "{job}: {:?}", g.not_split);
        // one task, the whole job
        assert_eq!(g.shards.len(), 1, "{job}");
    }
}
