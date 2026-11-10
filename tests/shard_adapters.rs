//! The adapters against real runners: `examples/sharding` holds a small Rust
//! crate and Node test suites (Vitest, Jest). Each runner lists its tests, the
//! tests are partitioned, every shard is run and the results are merged. The
//! Node suites need `npm ci` in `examples/sharding/node` and Node on the PATH;
//! without them the tests fail rather than skip.

#![allow(clippy::disallowed_methods)] // the tests read the environment of the run

mod common;
use local_ci::shard::adapters::{run_invocation, CargoTest, Jest, Safety, Vitest};
use local_ci::shard::{
    partition_tests, Discovered, History, Merged, PartitionOpts, RunnerAdapter, ShardPlan,
    SplitMode,
};
use local_ci::ExecContext;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/sharding")
        .join(name)
}

fn ctx(extra: &[(&str, &str)]) -> ExecContext {
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
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    ExecContext::new(env)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}

/// List, partition into `n`, run every shard, merge, and check the plan.
fn drive<A: RunnerAdapter>(
    a: &A,
    ctx: &ExecContext,
    dir: &Path,
    n: usize,
) -> (Discovered, ShardPlan, Merged) {
    let listing = run_invocation(ctx, &a.discover_command(), dir).unwrap();
    let d = a.discover(&listing).unwrap();
    assert!(d.complete, "{d:?}");
    let plan = partition_tests(a, &d, &History::default(), n, &PartitionOpts::default());
    assert_eq!(plan.mode, SplitMode::Listed, "{plan:?}");
    assert_eq!(plan.shards.len(), n);
    plan.verify(&d).unwrap();
    let outcomes: Vec<_> = plan
        .shards
        .iter()
        .map(|s| {
            let inv = a.invoke(s, plan.shards.len(), plan.mode);
            let out = run_invocation(ctx, &inv, dir).unwrap();
            a.collect("test", &format!("test@{}", s.index), &out)
        })
        .collect();
    let merged = a.merge(Some(&d.tests), &outcomes);
    (d, plan, merged)
}

#[test]
fn cargo_test_shards_cover_every_test_once() {
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join("rust");
    copy_dir(&example("rust"), &dir);
    let target = tmp.path().join("target");
    let target = target.to_str().unwrap();
    let a = CargoTest::new("test", &[], Safety::default());

    let (d, _, m) = drive(&a, &ctx(&[("CARGO_TARGET_DIR", target)]), &dir, 2);
    assert_eq!(
        d.tests,
        [
            "adds",
            "fails_on_purpose_when_asked",
            "tests::adds",
            "tests::adds_negative",
            "tests::nested::zero",
            "tests::slow_one"
        ]
    );
    assert!(m.ok(), "{m:?}");
    assert_eq!(
        (m.passed, m.skipped.as_slice()),
        (5, ["tests::slow_one".to_string()].as_slice())
    );

    // a failing test fails the merge, and is named
    let env = ctx(&[("CARGO_TARGET_DIR", target), ("SHARD_EXAMPLE_FAIL", "1")]);
    let (_, _, m) = drive(&a, &env, &dir, 3);
    assert_eq!(m.failed, ["fails_on_purpose_when_asked"]);
    assert!(m.problems.is_empty(), "{m:?}");
}

// A cargo that is set to colour its output, or to print little, lists the same
// tests and is still known to have listed them all.
#[test]
fn cargo_lists_the_same_tests_whatever_its_colour_and_quiet_settings() {
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join("rust");
    copy_dir(&example("rust"), &dir);
    let target = tmp.path().join("target");
    let target = target.to_str().unwrap();
    let a = CargoTest::new("test", &[], Safety::default());
    let listed = |extra: &[(&str, &str)]| {
        let mut env = vec![("CARGO_TARGET_DIR", target)];
        env.extend_from_slice(extra);
        let out = run_invocation(&ctx(&env), &a.discover_command(), &dir).unwrap();
        a.discover(&out).unwrap()
    };
    let plain = listed(&[]);
    assert!(plain.complete, "{plain:?}");
    for extra in [
        [("CARGO_TERM_COLOR", "always")],
        [("CARGO_TERM_QUIET", "true")],
        [("CARGO_TERM_VERBOSE", "true")],
    ] {
        assert_eq!(listed(&extra), plain, "{extra:?}");
    }
}

#[test]
fn vitest_shards_cover_every_file_once() {
    let dir = example("node");
    let a = Vitest::new("2.1.9", dir.to_str().unwrap(), &[], Safety::default());
    let (d, _, m) = drive(&a, &ctx(&[]), &dir, 2);
    assert_eq!(
        d.tests,
        [
            "vitest-tests/list.test.js",
            "vitest-tests/math.test.js",
            "vitest-tests/text.test.js"
        ]
    );
    assert!(m.ok(), "{m:?}");
    assert_eq!(m.passed, 3);
    let (_, _, m) = drive(&a, &ctx(&[("SHARD_EXAMPLE_FAIL", "1")]), &dir, 2);
    assert_eq!(m.failed, ["vitest-tests/text.test.js"]);
}

#[test]
fn jest_shards_cover_every_file_once() {
    let dir = example("node");
    let a = Jest::new("29.7.0", dir.to_str().unwrap(), &[], Safety::default());
    let (d, _, m) = drive(&a, &ctx(&[]), &dir, 2);
    assert_eq!(
        d.tests,
        [
            "jest-tests/list.test.js",
            "jest-tests/math.test.js",
            "jest-tests/text.test.js"
        ]
    );
    assert!(m.ok(), "{m:?}");
    assert_eq!(m.passed, 3);
    let (_, _, m) = drive(&a, &ctx(&[("SHARD_EXAMPLE_FAIL", "1")]), &dir, 3);
    assert_eq!(m.failed, ["jest-tests/text.test.js"]);
}

#[test]
fn the_runners_own_shard_feature_covers_every_file_once() {
    let dir = example("node");
    let root = dir.to_str().unwrap();
    let ctx = ctx(&[]);
    for (name, a) in [
        (
            "vitest",
            Box::new(Vitest::new("2", root, &[], Safety::default())) as Box<dyn Run>,
        ),
        (
            "jest",
            Box::new(Jest::new("29", root, &[], Safety::default())) as Box<dyn Run>,
        ),
    ] {
        let found = a.native(&ctx, &dir, 2);
        let mut all: Vec<String> = found.concat();
        all.sort();
        let before = all.len();
        all.dedup();
        assert_eq!(before, all.len(), "{name}: a file ran in two shards");
        assert_eq!(all.len(), 3, "{name}: {found:?}");
    }
}

/// What the native test needs of an adapter without being generic over it.
trait Run {
    fn native(&self, ctx: &ExecContext, dir: &Path, n: usize) -> Vec<Vec<String>>;
}

impl<A: RunnerAdapter> Run for A {
    fn native(&self, ctx: &ExecContext, dir: &Path, n: usize) -> Vec<Vec<String>> {
        assert!(self.capabilities().native_shard);
        (0..n)
            .map(|i| {
                let s = local_ci::shard::Shard {
                    index: i,
                    tests: vec![],
                    load: 0,
                };
                let out = run_invocation(ctx, &self.invoke(&s, n, SplitMode::Native), dir).unwrap();
                let o = self.collect("test", &format!("test@{i}"), &out);
                assert!(o.exit_ok);
                o.tests.into_iter().map(|t| t.identity).collect()
            })
            .collect()
    }
}
