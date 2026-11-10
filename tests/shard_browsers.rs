//! A41: end-to-end suites run for real. Playwright runs in a copy of
//! `examples/sharding/playwright` (it needs `npm ci` there and `npx playwright
//! install chromium`; without them the tests fail rather than skip). Cypress
//! runs in `examples/sharding/cypress` (`npm ci` and `npx cypress install`) on
//! Linux; its test is ignored elsewhere, since the Cypress binary and its
//! display need are only set up in CI's Linux job.
//!
//! This target needs the `browser-tests` feature, which the coverage gate does
//! not use: the gate measures the unit tests of the adapters, not a browser.

#![allow(clippy::disallowed_methods)] // the tests read the environment of the run

mod common;
use local_ci::dag::NotSplit;
use local_ci::shard::adapters::{run_invocation, Cypress, Isolation, Playwright};
use local_ci::shard::{
    merge_with, partition_tests, Discovered, History, Merged, PartitionOpts, Rules, RunnerAdapter,
    ShardOutcome, ShardPlan, SplitMode,
};
use local_ci::ExecContext;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/sharding")
        .join(name)
}

fn declared() -> Isolation {
    Isolation::Declared {
        evidence: "the examples start nothing the shards share".into(),
    }
}

fn ctx(extra: &[(&str, &str)]) -> ExecContext {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    env.remove("SHARD_EXAMPLE_FAIL");
    env.remove("CYPRESS_SHARD_EXAMPLE_FAIL");
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
    assert!(d.blocked.is_empty(), "{d:?}");
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
            a.collect_given("e2e", &format!("e2e@{}", s.index), &out, &s.tests)
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
fn a41_playwright_shards_run_each_file_of_each_project_once() {
    let (_t, dir) = copy_of("playwright");
    let a = Playwright::new("1.63.0", &[], declared());
    for n in [2, 5] {
        let (d, plan, outcomes, m) = drive(&a, &ctx(&[]), &dir, n);
        // the suite is not fully parallel: three files, two projects, six units
        assert_eq!(d.tests.len(), 6, "{d:?}");
        assert!(d.tests.contains(&"[mobile] › text.spec.js".to_string()));
        assert!(m.ok(), "{m:?}");
        assert_eq!(m.passed, 6);
        assert!(m.skipped.is_empty(), "{:?}", m.skipped);
        // what a shard ran is the files it was given, whole: the two tests of a
        // file are never in two shards
        let mut shard_of: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
        for (s, o) in plan.shards.iter().zip(&outcomes) {
            assert!(o.exit_ok, "{o:?}");
            let listing = run_invocation(&ctx(&[]), &a.invoke(s, n, plan.mode), &dir).unwrap();
            for t in a.collect("e2e", "x", &listing).tests {
                let file = t.identity.rsplitn(2, " › ").last().unwrap().to_string();
                let unit = s.tests.iter().find(|u| file.starts_with(u.as_str()));
                assert!(
                    unit.is_some(),
                    "{} ran in a shard not given its file",
                    t.identity
                );
                shard_of.entry(t.identity).or_default().insert(s.index);
            }
        }
        for (test, shards) in &shard_of {
            assert_eq!(shards.len(), 1, "{test}");
        }
        let by_file = |file: &str, project: &str| -> BTreeSet<usize> {
            shard_of
                .iter()
                .filter(|(t, _)| t.starts_with(&format!("[{project}] › {file} › ")))
                .flat_map(|(_, s)| s.iter().copied())
                .collect()
        };
        for file in ["math.spec.js", "text.spec.js", "viewport.spec.js"] {
            for project in ["desktop", "mobile"] {
                assert_eq!(by_file(file, project).len(), 1, "{project} {file}");
            }
        }
    }
    // a failing test fails its file, its shard and the job, in each project
    let (_, _, _, m) = drive(&a, &ctx(&[("SHARD_EXAMPLE_FAIL", "1")]), &dir, 3);
    assert_eq!(
        m.failed,
        ["[desktop] › text.spec.js", "[mobile] › text.spec.js"]
    );
    assert!(!m.ok());
}

#[test]
fn a41_a_fully_parallel_suite_is_divided_by_test_except_a_serial_file() {
    let (_t, dir) = copy_of("playwright");
    let a = Playwright::new(
        "1.63.0",
        &["--config", "playwright.parallel.config.js"],
        declared(),
    );
    let (d, plan, _, m) = drive(&a, &ctx(&[]), &dir, 3);
    assert_eq!(
        d.tests,
        [
            "[unit] › ordered.spec.js",
            "[unit] › plain.spec.js › adds",
            "[unit] › plain.spec.js › multiplies"
        ]
    );
    // three shards, one unit each: the serial file is run whole
    assert!(plan.shards.iter().all(|s| s.tests.len() == 1));
    assert!(m.ok(), "{m:?}");
    assert_eq!(m.passed, 3);
}

#[test]
fn a41_a_playwright_suite_with_a_server_or_a_global_setup_is_not_split() {
    let (_t, dir) = copy_of("playwright");
    for (config, why) in [
        ("playwright.server.config.js", "webServer"),
        ("playwright.setup.config.js", "global setup"),
    ] {
        let a = Playwright::new("1.63.0", &["--config", config], declared());
        let listing = run_invocation(&ctx(&[]), &a.discover_command(), &dir).unwrap();
        let e = a.discover(&listing).unwrap_err();
        assert!(e.contains(why), "{config}: {e}");
    }
    // and a suite whose isolation nobody declared is not split either
    let a = Playwright::new("1.63.0", &[], Isolation::None);
    let listing = run_invocation(&ctx(&[]), &a.discover_command(), &dir).unwrap();
    let d = a.discover(&listing).unwrap();
    let p = partition_tests(&a, &d, &History::default(), 2, &PartitionOpts::default());
    assert_eq!(p.not_split, [NotSplit::NoIsolation]);
}

#[test]
fn a41_a_project_with_a_dependency_or_a_teardown_is_not_split() {
    let (_t, dir) = copy_of("playwright");
    for config in ["playwright.deps.config.js", "playwright.teardown.config.js"] {
        let a = Playwright::new("1.63.0", &["--config", config], declared());
        let listing = run_invocation(&ctx(&[]), &a.discover_command(), &dir).unwrap();
        let d = a.discover(&listing).unwrap();
        // the listing holds the setup's test among the others, and a shard that
        // ran a test of `desktop` would run it again
        assert!(
            d.tests.contains(&"[setup] › auth.setup.js".to_string()),
            "{config}: {d:?}"
        );
        assert_eq!(d.blocked, [NotSplit::ProjectDependencies], "{config}");
        let p = partition_tests(&a, &d, &History::default(), 2, &PartitionOpts::default());
        assert_eq!(p.mode, SplitMode::Single, "{config}");
        assert!(p.not_split.contains(&NotSplit::ProjectDependencies));
    }
    // the same suite without the dependency: nothing blocks it
    let a = Playwright::new("1.63.0", &[], declared());
    let listing = run_invocation(&ctx(&[]), &a.discover_command(), &dir).unwrap();
    assert!(a.discover(&listing).unwrap().blocked.is_empty());
}

/// Cypress, for real: `npm ci` and `npx cypress install` in the example first.
/// Ignored where that is not set up (CI runs it on Linux).
#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "Cypress runs in CI's Linux job")]
fn a41_cypress_shards_run_their_spec_files_through_the_module_api_and_report_each_once() {
    let dir = example("cypress");
    let a = Cypress::new("14.5.4", &[], declared());
    let (d, plan, outcomes, m) = drive(&a, &ctx(&[]), &dir, 2);
    assert_eq!(
        d.tests,
        [
            "cypress/e2e/login.cy.js",
            "cypress/e2e/nested/profile.cy.js",
            "cypress/e2e/signup.cy.js"
        ]
    );
    assert_eq!(plan.shards.len(), 2);
    assert!(
        outcomes.iter().all(|o| o.parsed && o.exit_ok),
        "{outcomes:?}"
    );
    // two passed, one with a pending test only
    assert!(m.ok(), "{m:?}");
    assert_eq!(m.passed, 2);
    assert_eq!(m.skipped, ["cypress/e2e/nested/profile.cy.js"]);
    // a failing spec fails its shard through the exit code, and the job
    let (_, _, outcomes, m) = drive(&a, &ctx(&[("CYPRESS_SHARD_EXAMPLE_FAIL", "1")]), &dir, 2);
    assert_eq!(m.failed, ["cypress/e2e/signup.cy.js"]);
    assert!(!m.ok());
    assert_eq!(outcomes.iter().filter(|o| !o.exit_ok).count(), 1);
    // a configuration that says which files are specs is not guessed at
    let custom = example("cypress-custom");
    let listing = run_invocation(&ctx(&[]), &a.discover_command(), &custom).unwrap();
    assert!(!listing.success);
    let e = a.discover(&listing).unwrap_err();
    assert!(e.contains("chooses which files are specs"), "{e}");
}
