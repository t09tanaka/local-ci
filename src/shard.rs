//! Splitting one test job into shards: the runner adapter contract, the
//! partition of the tests and the check that the shards cover them exactly once.
//!
//! Everything here is pure: an adapter builds argv and reads output; the caller
//! starts the process (see [`adapters`]). A job is split only when its runner
//! can list its tests, target them by name, shares no state between shards,
//! gets its fixtures per shard and merges to the verdict of the whole job;
//! otherwise [`partition_tests`] returns one shard with the reasons
//! ([`NotSplit`]).

pub mod adapters;

use crate::dag::NotSplit;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

/// The runner a plan was made for: what its behaviour depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerInfo {
    /// `cargo-test`, `vitest`, `jest`.
    pub runner: String,
    pub version: String,
    /// Configuration that changes what runs (config file, features).
    pub config: Vec<String>,
    /// Name filters, tags and skip conditions of the job's own command.
    pub filters: Vec<String>,
}

/// What a runner can do for a split (the conditions of a safe split).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// It can list its tests.
    pub enumerates: bool,
    /// A run can be told which tests to run.
    pub targets: bool,
    /// It has a shard feature of its own (`--shard i/n`) that needs no list.
    pub native_shard: bool,
    /// The tests of different shards update state they share (a database, a
    /// port, a container, files outside the checkout).
    pub shared_state: bool,
    /// Each shard gets its own fixtures.
    pub isolated_fixtures: bool,
    /// The results of the shards merge to the verdict of the whole job.
    pub aggregates_exactly: bool,
    /// The job's own command has a name filter, a skip or a tag.
    pub job_filtered: bool,
    /// The job's own command has an argument that is not known to leave the
    /// set of tests alone.
    pub unreadable_args: bool,
    /// A test is targeted by a substring of its name (a path given to Vitest
    /// is a filter), so a name that is part of another selects both.
    pub substring_targets: bool,
}

/// The tests a runner lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovered {
    pub tests: Vec<String>,
    /// The list is all there is. `false` when tests are generated at run time.
    pub complete: bool,
}

/// How long tests took before: identity to milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    pub millis: BTreeMap<String, u64>,
}

/// What a partition takes besides the tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartitionOpts {
    /// Setup and transfer a shard costs, in milliseconds. A shard that would
    /// carry less than this is merged into the others.
    pub fixed_cost_ms: u64,
    /// Speed of the worker of each shard, 1000 for the nominal one; a shard
    /// without an entry gets 1000.
    pub speed_permille: Vec<u32>,
}

/// How the shards divide the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitMode {
    /// One shard.
    Single,
    /// Each shard names its tests.
    Listed,
    /// The runner's own shard feature divides the tests.
    Native,
}

/// What the loads are estimated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Past durations.
    History,
    /// Every test weighs the same; the order is a stable hash of the name.
    Fallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shard {
    pub index: usize,
    /// The tests, by name; empty for a native shard.
    pub tests: Vec<String>,
    /// Estimated load after the correction for the speed of its worker.
    pub load: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardPlan {
    pub runner: RunnerInfo,
    pub mode: SplitMode,
    pub basis: Basis,
    pub shards: Vec<Shard>,
    /// Why the job is one shard (empty when it is split).
    pub not_split: Vec<NotSplit>,
}

/// A way a plan does not cover the tests exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestProblem {
    /// In more than one shard (or twice in one).
    Duplicate(String),
    /// Listed, in no shard.
    Missing(String),
    /// In a shard, not listed.
    Unexpected(String),
    /// Listed twice.
    DuplicateListed(String),
    /// A native shard that names tests, or a plan without shards.
    Malformed(String),
}

impl std::fmt::Display for ManifestProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestProblem::Duplicate(t) => write!(f, "`{t}` is in more than one shard"),
            ManifestProblem::Missing(t) => write!(f, "`{t}` is in no shard"),
            ManifestProblem::Unexpected(t) => write!(f, "`{t}` is in a shard but was not listed"),
            ManifestProblem::DuplicateListed(t) => write!(f, "`{t}` is listed twice"),
            ManifestProblem::Malformed(w) => f.write_str(w),
        }
    }
}

/// What to run for one shard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invocation {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// What a command printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResult {
    /// The name the runner lists the test by.
    pub identity: String,
    pub status: TestStatus,
}

/// What one shard reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardOutcome {
    pub job_id: String,
    pub shard_id: String,
    pub tests: Vec<TestResult>,
    /// The runner's command exited with success.
    pub exit_ok: bool,
    /// The report of the runner was read. `false` when its output holds no
    /// report (no JSON, no test line): then `tests` says nothing.
    #[serde(default = "yes")]
    pub parsed: bool,
}

fn yes() -> bool {
    true
}

/// The shards of one job together.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Merged {
    /// Every test once, by identity.
    pub tests: Vec<TestResult>,
    pub passed: usize,
    pub failed: Vec<String>,
    pub skipped: Vec<String>,
    /// Duplicated, missing or unexpected tests, shards that failed without a
    /// failed test.
    pub problems: Vec<String>,
}

impl Merged {
    /// Nothing failed and nothing is wrong with the shape of the results.
    pub fn ok(&self) -> bool {
        self.failed.is_empty() && self.problems.is_empty()
    }
}

/// What a test runner has to provide to be split.
pub trait RunnerAdapter {
    fn info(&self) -> &RunnerInfo;
    fn capabilities(&self) -> Capabilities;
    /// The command that lists the tests.
    fn discover_command(&self) -> Invocation;
    /// The tests a listing holds; an error when it cannot be read.
    fn discover(&self, out: &CommandOutput) -> Result<Discovered, String>;
    /// Divide `discovered` among `shard_count` shards.
    fn partition(
        &self,
        discovered: &Discovered,
        history: &History,
        shard_count: usize,
        opts: &PartitionOpts,
    ) -> ShardPlan
    where
        Self: Sized,
    {
        partition_tests(self, discovered, history, shard_count, opts)
    }
    /// The command that runs one shard of `count`.
    fn invoke(&self, shard: &Shard, count: usize, mode: SplitMode) -> Invocation;
    /// What a shard reported, from what its command printed.
    fn collect(&self, job_id: &str, shard_id: &str, out: &CommandOutput) -> ShardOutcome;
    /// The shards of a job together, checked against the listed tests (`None`
    /// for a native split, which lists none).
    fn merge(&self, manifest: Option<&[String]>, outcomes: &[ShardOutcome]) -> Merged {
        merge_outcomes(manifest, outcomes)
    }
}

/// A stable 64-bit hash of a name (the first bytes of its SHA-256).
fn name_hash(name: &str) -> u64 {
    let d = Sha256::digest(name.as_bytes());
    u64::from_be_bytes(d[..8].try_into().expect("a digest has 32 bytes"))
}

/// Why a job cannot be split into shards with these capabilities and this
/// listing; empty when it can.
pub fn not_split_reasons(caps: &Capabilities, discovered: &Discovered) -> Vec<NotSplit> {
    let mut out = Vec::new();
    if !caps.enumerates && !caps.native_shard {
        out.push(NotSplit::NoEnumeration);
    }
    if caps.enumerates && !discovered.complete && !caps.native_shard {
        out.push(NotSplit::IncompleteDiscovery);
    }
    if !caps.targets && !caps.native_shard {
        out.push(NotSplit::NoShardTarget);
    }
    if caps.shared_state {
        out.push(NotSplit::SharedState);
    }
    if !caps.isolated_fixtures {
        out.push(NotSplit::NoIsolation);
    }
    if !caps.aggregates_exactly {
        out.push(NotSplit::InexactAggregation);
    }
    if caps.job_filtered {
        out.push(NotSplit::JobFilter);
    }
    if caps.unreadable_args {
        out.push(NotSplit::UnreadableArgs);
    }
    if caps.substring_targets && has_overlap(&discovered.tests) {
        out.push(NotSplit::PathOverlap);
    }
    out
}

/// One of the names is part of another one.
fn has_overlap(tests: &[String]) -> bool {
    let names: BTreeSet<&String> = tests.iter().collect();
    names
        .iter()
        .any(|a| names.iter().any(|b| a != b && b.contains(a.as_str())))
}

/// The most a shard's command may take (the arguments of a process are
/// limited; this keeps well under the smallest limit in use).
pub const MAX_ARGV_BYTES: usize = 100 * 1024;

/// The length of a command, as the system counts it.
fn argv_bytes(inv: &Invocation) -> usize {
    inv.argv.iter().map(|a| a.len() + 1).sum()
}

fn single(
    runner: RunnerInfo,
    tests: Vec<String>,
    basis: Basis,
    load: u64,
    why: Vec<NotSplit>,
) -> ShardPlan {
    ShardPlan {
        runner,
        mode: SplitMode::Single,
        basis,
        shards: vec![Shard {
            index: 0,
            tests,
            load,
        }],
        not_split: why,
    }
}

/// Divide `discovered` among at most `shard_count` shards.
///
/// The longest tests go first, each to the shard whose load (corrected for the
/// speed of its worker) stays lowest, so the loads come out even. A test
/// without history weighs the mean of those that have one; with no history at
/// all every test weighs the same and the order is a stable hash of the name.
/// The same inputs always give the same shards. Shards that would carry less
/// than `opts.fixed_cost_ms` are merged away. A job that cannot be split
/// safely (see [`not_split_reasons`]) is one shard that says why.
pub fn partition_tests(
    adapter: &dyn RunnerAdapter,
    discovered: &Discovered,
    history: &History,
    shard_count: usize,
    opts: &PartitionOpts,
) -> ShardPlan {
    let runner = adapter.info().clone();
    let caps = adapter.capabilities();
    let names: Vec<String> = discovered
        .tests
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let why = not_split_reasons(&caps, discovered);
    if !why.is_empty() {
        return single(runner, names, Basis::Fallback, 0, why);
    }
    if shard_count <= 1 {
        return single(
            runner,
            names,
            Basis::Fallback,
            0,
            vec![NotSplit::NotRequested],
        );
    }
    let listed = caps.enumerates && caps.targets && discovered.complete;
    if !listed {
        // the runner's own shard feature: nothing to weigh. With a listing
        // that is all there is, a shard holds at least one of its tests, so
        // there are no more shards than tests (a partial listing is a lower
        // bound only).
        let shard_count = if caps.enumerates && discovered.complete {
            shard_count.min(names.len())
        } else {
            shard_count
        };
        if shard_count <= 1 {
            return single(runner, names, Basis::Fallback, 0, vec![NotSplit::TooSmall]);
        }
        return ShardPlan {
            runner,
            mode: SplitMode::Native,
            basis: Basis::Fallback,
            shards: (0..shard_count)
                .map(|index| Shard {
                    index,
                    tests: vec![],
                    load: 0,
                })
                .collect(),
            not_split: vec![],
        };
    }
    if names.is_empty() {
        return single(runner, names, Basis::Fallback, 0, vec![NotSplit::TooSmall]);
    }
    let known: Vec<u64> = names
        .iter()
        .filter_map(|n| history.millis.get(n).copied())
        .collect();
    let basis = if known.is_empty() {
        Basis::Fallback
    } else {
        Basis::History
    };
    let mean = match basis {
        Basis::History => (known.iter().sum::<u64>() / known.len() as u64).max(1),
        Basis::Fallback => 1,
    };
    let weight = |n: &String| match basis {
        Basis::History => history.millis.get(n).copied().unwrap_or(mean).max(1),
        Basis::Fallback => 1,
    };
    let total: u64 = names.iter().map(weight).sum();
    let mut count = shard_count.min(names.len());
    if basis == Basis::History && opts.fixed_cost_ms > 0 {
        count = count.min(((total / opts.fixed_cost_ms) as usize).max(1));
    }
    if count <= 1 {
        return single(runner, names, basis, total, vec![NotSplit::TooSmall]);
    }
    let speed = |i: usize| u64::from(opts.speed_permille.get(i).copied().unwrap_or(1000).max(1));
    let mut order: Vec<&String> = names.iter().collect();
    // stable: equal weights stay in name order
    order.sort_by_key(|n| {
        (
            Reverse(weight(n)),
            if basis == Basis::Fallback {
                name_hash(n)
            } else {
                0
            },
        )
    });
    let mut loads = vec![0u64; count];
    let mut tests: Vec<Vec<String>> = vec![vec![]; count];
    for n in order {
        let w = weight(n);
        let at = (0..count)
            .min_by_key(|&i| (loads[i] + w * 1000 / speed(i), i))
            .expect("at least two shards");
        loads[at] += w * 1000 / speed(at);
        tests[at].push(n.clone());
    }
    let shards = tests
        .into_iter()
        .zip(loads)
        .filter(|(t, _)| !t.is_empty())
        .enumerate()
        .map(|(index, (mut t, load))| {
            t.sort();
            Shard {
                index,
                tests: t,
                load,
            }
        })
        .collect();
    let plan = ShardPlan {
        runner,
        mode: SplitMode::Listed,
        basis,
        shards,
        not_split: vec![],
    };
    let longest = plan
        .shards
        .iter()
        .map(|s| argv_bytes(&adapter.invoke(s, plan.shards.len(), SplitMode::Listed)))
        .max()
        .unwrap_or(0);
    if longest > MAX_ARGV_BYTES {
        return single(
            plan.runner,
            names,
            basis,
            total,
            vec![NotSplit::ArgvTooLarge],
        );
    }
    plan
}

impl ShardPlan {
    /// The plan splits the job.
    pub fn is_split(&self) -> bool {
        self.mode != SplitMode::Single
    }

    /// Check that every listed test is in exactly one shard and nothing else
    /// is. A native plan lists no tests, so only its shape is checked.
    pub fn verify(&self, discovered: &Discovered) -> Result<(), Vec<ManifestProblem>> {
        let mut out = Vec::new();
        if self.shards.is_empty() {
            out.push(ManifestProblem::Malformed("the plan has no shard".into()));
        }
        if self.mode == SplitMode::Native {
            if self.shards.iter().any(|s| !s.tests.is_empty()) {
                out.push(ManifestProblem::Malformed(
                    "a native shard names tests".into(),
                ));
            }
            return if out.is_empty() { Ok(()) } else { Err(out) };
        }
        let mut listed: BTreeMap<&str, usize> = BTreeMap::new();
        for t in &discovered.tests {
            *listed.entry(t).or_default() += 1;
        }
        let mut placed: BTreeMap<&str, usize> = BTreeMap::new();
        for t in self.shards.iter().flat_map(|s| &s.tests) {
            *placed.entry(t).or_default() += 1;
        }
        for (t, n) in &listed {
            if *n > 1 {
                out.push(ManifestProblem::DuplicateListed((*t).to_string()));
            }
            match placed.get(t) {
                None => out.push(ManifestProblem::Missing((*t).to_string())),
                Some(p) if *p > 1 => out.push(ManifestProblem::Duplicate((*t).to_string())),
                Some(_) => {}
            }
        }
        for t in placed.keys().filter(|t| !listed.contains_key(*t)) {
            out.push(ManifestProblem::Unexpected((*t).to_string()));
        }
        if out.is_empty() {
            Ok(())
        } else {
            Err(out)
        }
    }
}

/// The shards of one job together. With a `manifest`, every test in it must be
/// reported once and no other; each test is counted once whatever the shard.
pub fn merge_outcomes(manifest: Option<&[String]>, outcomes: &[ShardOutcome]) -> Merged {
    merge_with(manifest, &Rules::default(), outcomes)
}

/// What a merge checks besides the manifest.
#[derive(Debug, Clone, Default)]
pub struct Rules<'a> {
    /// A test reported that is not in the manifest is not a problem (the
    /// listing may be partial).
    pub allow_unlisted: bool,
    /// The tests each shard (by `shard_id`) was given; a test reported by
    /// another shard is a problem. A shard not named here is not checked.
    pub assigned: BTreeMap<&'a str, BTreeSet<&'a str>>,
}

/// [`merge_outcomes`] with `rules`.
pub fn merge_with(
    manifest: Option<&[String]>,
    rules: &Rules<'_>,
    outcomes: &[ShardOutcome],
) -> Merged {
    let listed: Option<BTreeSet<&str>> = manifest.map(|m| m.iter().map(String::as_str).collect());
    let mut seen: BTreeMap<&str, Vec<&TestResult>> = BTreeMap::new();
    for t in outcomes.iter().flat_map(|o| &o.tests) {
        seen.entry(&t.identity).or_default().push(t);
    }
    let mut m = Merged::default();
    for o in outcomes {
        if let Some(own) = rules.assigned.get(o.shard_id.as_str()) {
            for t in o
                .tests
                .iter()
                .filter(|t| !own.contains(t.identity.as_str()))
            {
                m.problems.push(format!(
                    "`{}` was reported by shard {} of {}, which was not given it",
                    t.identity, o.shard_id, o.job_id
                ));
            }
        }
    }
    for (id, rs) in &seen {
        if rs.len() > 1 {
            m.problems
                .push(format!("`{id}` was reported more than once"));
        }
        if let Some(man) = &listed {
            if !rules.allow_unlisted && !man.contains(id) {
                m.problems
                    .push(format!("`{id}` was reported but was not listed"));
            }
        }
        let r = rs[0];
        match r.status {
            TestStatus::Passed => m.passed += 1,
            TestStatus::Failed => m.failed.push((*id).to_string()),
            TestStatus::Skipped => m.skipped.push((*id).to_string()),
        }
        m.tests.push(r.clone());
    }
    for t in listed.iter().flatten() {
        if !seen.contains_key(t) {
            m.problems.push(format!("`{t}` was not reported"));
        }
    }
    for o in outcomes {
        if !o.parsed {
            m.problems.push(format!(
                "the report of shard {} of {} could not be read",
                o.shard_id, o.job_id
            ));
        } else if o.exit_ok && o.tests.is_empty() {
            m.problems.push(format!(
                "shard {} of {} succeeded but reported no test",
                o.shard_id, o.job_id
            ));
        }
        if !o.exit_ok && !o.tests.iter().any(|t| t.status == TestStatus::Failed) {
            m.problems.push(format!(
                "shard {} of {} failed without a failed test",
                o.shard_id, o.job_id
            ));
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runner with the capabilities a test sets.
    struct Fake {
        info: RunnerInfo,
        caps: Capabilities,
    }

    const OK: Capabilities = Capabilities {
        enumerates: true,
        targets: true,
        native_shard: false,
        shared_state: false,
        isolated_fixtures: true,
        aggregates_exactly: true,
        job_filtered: false,
        unreadable_args: false,
        substring_targets: false,
    };

    fn fake(caps: Capabilities) -> Fake {
        Fake {
            info: RunnerInfo {
                runner: "fake".into(),
                version: "1".into(),
                ..Default::default()
            },
            caps,
        }
    }

    impl RunnerAdapter for Fake {
        fn info(&self) -> &RunnerInfo {
            &self.info
        }
        fn capabilities(&self) -> Capabilities {
            self.caps
        }
        fn discover_command(&self) -> Invocation {
            Invocation {
                argv: vec![],
                env: vec![],
            }
        }
        fn discover(&self, _: &CommandOutput) -> Result<Discovered, String> {
            Err("no".into())
        }
        fn invoke(&self, _: &Shard, _: usize, _: SplitMode) -> Invocation {
            self.discover_command()
        }
        fn collect(&self, job_id: &str, shard_id: &str, _: &CommandOutput) -> ShardOutcome {
            ShardOutcome {
                job_id: job_id.into(),
                shard_id: shard_id.into(),
                tests: vec![],
                exit_ok: true,
                parsed: true,
            }
        }
    }

    fn found(names: &[&str]) -> Discovered {
        Discovered {
            tests: names.iter().map(|s| s.to_string()).collect(),
            complete: true,
        }
    }

    fn hist(items: &[(&str, u64)]) -> History {
        History {
            millis: items.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    fn names(p: &ShardPlan) -> Vec<Vec<&str>> {
        p.shards
            .iter()
            .map(|s| s.tests.iter().map(String::as_str).collect())
            .collect()
    }

    #[test]
    fn longest_first_evens_the_loads() {
        // 8 | 7 | 6 | 5 | 4 over two shards: LPT gives 8+5+...
        let d = found(&["a", "b", "c", "d", "e"]);
        let h = hist(&[("a", 8), ("b", 7), ("c", 6), ("d", 5), ("e", 4)]);
        let p = Fake::partition(&fake(OK), &d, &h, 2, &PartitionOpts::default());
        assert_eq!(p.mode, SplitMode::Listed);
        assert_eq!(p.basis, Basis::History);
        // a->0 (8), b->1 (7), c->1 (13), d->0 (13), e->0 (17) vs 1 (13)? e goes to the lighter: 13 tie -> 0
        assert_eq!(names(&p), vec![vec!["a", "d", "e"], vec!["b", "c"]]);
        assert_eq!(p.shards[0].load, 17);
        assert_eq!(p.shards[1].load, 13);
        assert!(p.verify(&d).is_ok());
        assert!(p.is_split());
        assert!(p.not_split.is_empty());
    }

    #[test]
    fn the_same_input_gives_the_same_shards() {
        let d = found(&["t3", "t1", "t2", "t4", "t5", "t6"]);
        let h = hist(&[("t1", 5), ("t2", 5), ("t3", 5)]);
        let a = partition_tests(&fake(OK), &d, &h, 3, &PartitionOpts::default());
        let mut shuffled = d.clone();
        shuffled.tests.reverse();
        let b = partition_tests(&fake(OK), &shuffled, &h, 3, &PartitionOpts::default());
        assert_eq!(a, b);
    }

    #[test]
    fn a_test_without_history_weighs_the_mean() {
        // mean of 10 and 30 is 20: x weighs 20 and goes after a (30), before b (10)
        let d = found(&["a", "b", "x"]);
        let h = hist(&[("a", 30), ("b", 10)]);
        let p = partition_tests(&fake(OK), &d, &h, 2, &PartitionOpts::default());
        assert_eq!(names(&p), vec![vec!["a"], vec!["b", "x"]]);
        assert_eq!(p.shards[1].load, 30);
    }

    #[test]
    fn equal_weights_stay_in_name_order() {
        let d = found(&["c", "a", "b", "d"]);
        let h = hist(&[("a", 5), ("b", 5), ("c", 5), ("d", 5)]);
        let p = partition_tests(&fake(OK), &d, &h, 2, &PartitionOpts::default());
        assert_eq!(names(&p), vec![vec!["a", "c"], vec!["b", "d"]]);
    }

    #[test]
    fn without_history_the_order_is_a_stable_hash() {
        let d = found(&["a", "b", "c", "d", "e", "f"]);
        let p = partition_tests(
            &fake(OK),
            &d,
            &History::default(),
            3,
            &PartitionOpts::default(),
        );
        assert_eq!(p.basis, Basis::Fallback);
        assert!(p.shards.iter().all(|s| s.tests.len() == 2));
        // pinned: the hash must not change between versions
        assert_eq!(name_hash("a"), 0xca978112ca1bbdca);
        assert_eq!(
            names(&p),
            vec![vec!["b", "d"], vec!["e", "f"], vec!["a", "c"]]
        );
        // history for names that are not listed is no history
        let p2 = partition_tests(
            &fake(OK),
            &d,
            &hist(&[("zz", 3)]),
            3,
            &PartitionOpts::default(),
        );
        assert_eq!(p2, p);
        assert_eq!(p.shards.iter().map(|s| s.load).sum::<u64>(), 6);
    }

    #[test]
    fn a_faster_worker_takes_more() {
        let d = found(&["a", "b", "c", "d"]);
        let h = hist(&[("a", 10), ("b", 10), ("c", 10), ("d", 10)]);
        let o = PartitionOpts {
            fixed_cost_ms: 0,
            speed_permille: vec![2000, 1000],
        };
        let p = partition_tests(&fake(OK), &d, &h, 2, &o);
        assert_eq!(names(&p), vec![vec!["a", "b", "d"], vec!["c"]]);
        assert_eq!(p.shards[0].load, 15);
        assert_eq!(p.shards[1].load, 10);
        // a speed of 0 is taken as the slowest there is
        let o = PartitionOpts {
            fixed_cost_ms: 0,
            speed_permille: vec![0, 1000],
        };
        let p = partition_tests(&fake(OK), &d, &h, 2, &o);
        assert_eq!(names(&p), vec![vec!["a", "b", "c", "d"]]);
    }

    #[test]
    fn a_shard_left_without_tests_is_dropped() {
        let d = found(&["a", "b"]);
        let h = hist(&[("a", 1), ("b", 1)]);
        let o = PartitionOpts {
            fixed_cost_ms: 0,
            speed_permille: vec![1000, 1],
        };
        let p = partition_tests(&fake(OK), &d, &h, 2, &o);
        // b costs 1000000 on the slow worker and goes to the fast one too
        assert_eq!(names(&p), vec![vec!["a", "b"]]);
        assert_eq!(p.shards[0].index, 0);
        assert!(p.verify(&d).is_ok());
    }

    #[test]
    fn small_shards_are_merged_by_their_fixed_cost() {
        let d = found(&["a", "b", "c", "d"]);
        let h = hist(&[("a", 10), ("b", 10), ("c", 10), ("d", 10)]);
        let o = |fixed| PartitionOpts {
            fixed_cost_ms: fixed,
            speed_permille: vec![],
        };
        // 40 ms in all: a cost of 15 leaves room for two shards
        assert_eq!(
            partition_tests(&fake(OK), &d, &h, 4, &o(15)).shards.len(),
            2
        );
        // exactly one cost per shard
        assert_eq!(
            partition_tests(&fake(OK), &d, &h, 4, &o(10)).shards.len(),
            4
        );
        assert_eq!(partition_tests(&fake(OK), &d, &h, 4, &o(0)).shards.len(), 4);
        // too small for two
        let p = partition_tests(&fake(OK), &d, &h, 4, &o(30));
        assert_eq!(p.mode, SplitMode::Single);
        assert_eq!(p.not_split, vec![NotSplit::TooSmall]);
        assert_eq!(p.shards[0].load, 40);
        assert_eq!(p.shards[0].tests.len(), 4);
        // without history there are no milliseconds to compare with
        let p = partition_tests(&fake(OK), &d, &History::default(), 4, &o(1_000_000));
        assert_eq!(p.shards.len(), 4);
    }

    #[test]
    fn never_more_shards_than_tests() {
        let d = found(&["a", "b"]);
        let p = partition_tests(
            &fake(OK),
            &d,
            &History::default(),
            5,
            &PartitionOpts::default(),
        );
        assert_eq!(p.shards.len(), 2);
        let p = partition_tests(
            &fake(OK),
            &found(&["a"]),
            &History::default(),
            5,
            &PartitionOpts::default(),
        );
        assert_eq!(p.not_split, vec![NotSplit::TooSmall]);
        let p = partition_tests(
            &fake(OK),
            &found(&[]),
            &History::default(),
            5,
            &PartitionOpts::default(),
        );
        assert_eq!(p.not_split, vec![NotSplit::TooSmall]);
        assert!(!p.is_split());
        // duplicates in the listing count once
        let p = partition_tests(
            &fake(OK),
            &found(&["a", "a"]),
            &History::default(),
            2,
            &PartitionOpts::default(),
        );
        assert_eq!(names(&p), vec![vec!["a"]]);
    }

    #[test]
    fn one_shard_requested_is_not_a_split() {
        let p = partition_tests(
            &fake(OK),
            &found(&["a", "b"]),
            &History::default(),
            1,
            &PartitionOpts::default(),
        );
        assert_eq!(p.not_split, vec![NotSplit::NotRequested]);
        assert_eq!(names(&p), vec![vec!["a", "b"]]);
        let p = partition_tests(
            &fake(OK),
            &found(&["a", "b"]),
            &History::default(),
            0,
            &PartitionOpts::default(),
        );
        assert_eq!(p.not_split, vec![NotSplit::NotRequested]);
    }

    #[test]
    fn each_condition_of_a_safe_split_is_a_reason() {
        let d = found(&["a", "b"]);
        let bad = |c: Capabilities| not_split_reasons(&c, &d);
        assert_eq!(bad(OK), vec![]);
        assert_eq!(
            bad(Capabilities {
                enumerates: false,
                ..OK
            }),
            vec![NotSplit::NoEnumeration]
        );
        assert_eq!(
            bad(Capabilities {
                targets: false,
                ..OK
            }),
            vec![NotSplit::NoShardTarget]
        );
        assert_eq!(
            bad(Capabilities {
                shared_state: true,
                ..OK
            }),
            vec![NotSplit::SharedState]
        );
        assert_eq!(
            bad(Capabilities {
                isolated_fixtures: false,
                ..OK
            }),
            vec![NotSplit::NoIsolation]
        );
        assert_eq!(
            bad(Capabilities {
                aggregates_exactly: false,
                ..OK
            }),
            vec![NotSplit::InexactAggregation]
        );
        let partial = Discovered {
            tests: vec![],
            complete: false,
        };
        assert_eq!(
            not_split_reasons(&OK, &partial),
            vec![NotSplit::IncompleteDiscovery]
        );
        // a runner that lists nothing is not blamed for an incomplete list
        assert_eq!(
            not_split_reasons(
                &Capabilities {
                    enumerates: false,
                    ..OK
                },
                &partial
            ),
            vec![NotSplit::NoEnumeration]
        );
        // its own shard feature makes up for what is missing
        let native = Capabilities {
            native_shard: true,
            enumerates: false,
            targets: false,
            ..OK
        };
        assert_eq!(not_split_reasons(&native, &partial), vec![]);
        let native = Capabilities {
            native_shard: true,
            ..OK
        };
        assert_eq!(not_split_reasons(&native, &partial), vec![]);
    }

    #[test]
    fn an_unsafe_job_is_one_shard_with_its_reasons() {
        let d = found(&["b", "a"]);
        let c = Capabilities {
            shared_state: true,
            isolated_fixtures: false,
            ..OK
        };
        let p = partition_tests(
            &fake(c),
            &d,
            &History::default(),
            4,
            &PartitionOpts::default(),
        );
        assert_eq!(p.mode, SplitMode::Single);
        assert_eq!(
            p.not_split,
            vec![NotSplit::SharedState, NotSplit::NoIsolation]
        );
        assert_eq!(names(&p), vec![vec!["a", "b"]]);
        assert!(p.verify(&d).is_ok());
    }

    #[test]
    fn an_incomplete_listing_uses_the_native_shard_feature() {
        let partial = Discovered {
            tests: vec!["a".into()],
            complete: false,
        };
        let c = Capabilities {
            native_shard: true,
            ..OK
        };
        let p = partition_tests(
            &fake(c),
            &partial,
            &History::default(),
            3,
            &PartitionOpts::default(),
        );
        assert_eq!(p.mode, SplitMode::Native);
        assert_eq!(p.shards.len(), 3);
        assert_eq!(p.shards[2].index, 2);
        assert!(p.shards.iter().all(|s| s.tests.is_empty()));
        assert!(p.verify(&partial).is_ok());
        // without the feature it is not split
        let p = partition_tests(
            &fake(OK),
            &partial,
            &History::default(),
            3,
            &PartitionOpts::default(),
        );
        assert_eq!(p.not_split, vec![NotSplit::IncompleteDiscovery]);
        // a runner that cannot list uses it as well, when the list is complete
        let c = Capabilities {
            native_shard: true,
            enumerates: false,
            ..OK
        };
        let p = partition_tests(
            &fake(c),
            &found(&["a"]),
            &History::default(),
            2,
            &PartitionOpts::default(),
        );
        assert_eq!(p.mode, SplitMode::Native);
    }

    fn plan_of(mode: SplitMode, shards: &[&[&str]]) -> ShardPlan {
        ShardPlan {
            runner: RunnerInfo::default(),
            mode,
            basis: Basis::Fallback,
            shards: shards
                .iter()
                .enumerate()
                .map(|(index, t)| Shard {
                    index,
                    tests: t.iter().map(|s| s.to_string()).collect(),
                    load: 0,
                })
                .collect(),
            not_split: vec![],
        }
    }

    #[test]
    fn the_manifest_check_finds_every_way_to_miss() {
        let d = found(&["a", "b", "c"]);
        let ok = plan_of(SplitMode::Listed, &[&["a", "b"], &["c"]]);
        assert_eq!(ok.verify(&d), Ok(()));
        let bad = |p: &ShardPlan, d: &Discovered| p.verify(d).unwrap_err();
        assert_eq!(
            bad(&plan_of(SplitMode::Listed, &[&["a"], &["c"]]), &d),
            vec![ManifestProblem::Missing("b".into())]
        );
        assert_eq!(
            bad(&plan_of(SplitMode::Listed, &[&["a", "b"], &["b", "c"]]), &d),
            vec![ManifestProblem::Duplicate("b".into())]
        );
        assert_eq!(
            bad(&plan_of(SplitMode::Listed, &[&["a", "a", "b", "c"]]), &d),
            vec![ManifestProblem::Duplicate("a".into())]
        );
        assert_eq!(
            bad(&plan_of(SplitMode::Listed, &[&["a", "b"], &["c", "z"]]), &d),
            vec![ManifestProblem::Unexpected("z".into())]
        );
        let twice = found(&["a", "a"]);
        assert_eq!(
            bad(&plan_of(SplitMode::Listed, &[&["a"]]), &twice),
            vec![ManifestProblem::DuplicateListed("a".into())]
        );
        assert_eq!(
            bad(&plan_of(SplitMode::Listed, &[]), &found(&[])),
            vec![ManifestProblem::Malformed("the plan has no shard".into())]
        );
        assert_eq!(
            bad(&plan_of(SplitMode::Native, &[&["a"], &[]]), &d),
            vec![ManifestProblem::Malformed(
                "a native shard names tests".into()
            )]
        );
        assert_eq!(
            bad(&plan_of(SplitMode::Native, &[]), &d),
            vec![ManifestProblem::Malformed("the plan has no shard".into())]
        );
    }

    #[test]
    fn problems_read_as_sentences() {
        let all = [
            ManifestProblem::Duplicate("a".into()),
            ManifestProblem::Missing("b".into()),
            ManifestProblem::Unexpected("c".into()),
            ManifestProblem::DuplicateListed("d".into()),
            ManifestProblem::Malformed("e".into()),
        ];
        let text: Vec<String> = all.iter().map(|p| p.to_string()).collect();
        assert_eq!(
            text,
            vec![
                "`a` is in more than one shard",
                "`b` is in no shard",
                "`c` is in a shard but was not listed",
                "`d` is listed twice",
                "e"
            ]
        );
    }

    fn outcome(shard: &str, ok: bool, tests: &[(&str, TestStatus)]) -> ShardOutcome {
        ShardOutcome {
            job_id: "j".into(),
            shard_id: shard.into(),
            tests: tests
                .iter()
                .map(|(i, s)| TestResult {
                    identity: i.to_string(),
                    status: *s,
                })
                .collect(),
            exit_ok: ok,
            parsed: true,
        }
    }

    #[test]
    fn merging_counts_each_test_once_and_checks_the_set() {
        use TestStatus::*;
        let man: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let good = [
            outcome("s0", true, &[("a", Passed), ("c", Skipped)]),
            outcome("s1", false, &[("b", Failed)]),
        ];
        let m = merge_outcomes(Some(&man), &good);
        assert_eq!(m.passed, 1);
        assert_eq!(m.failed, vec!["b"]);
        assert_eq!(m.skipped, vec!["c"]);
        assert_eq!(m.tests.len(), 3);
        assert!(m.problems.is_empty());
        assert!(!m.ok());
        let all = [outcome(
            "s0",
            true,
            &[("a", Passed), ("b", Passed), ("c", Passed)],
        )];
        assert!(merge_outcomes(Some(&man), &all).ok());

        // missing, repeated, unexpected
        let m = merge_outcomes(
            Some(&man),
            &[outcome(
                "s0",
                true,
                &[("a", Passed), ("a", Passed), ("z", Passed)],
            )],
        );
        assert_eq!(
            m.problems,
            vec![
                "`a` was reported more than once",
                "`z` was reported but was not listed",
                "`b` was not reported",
                "`c` was not reported",
            ]
        );
        assert_eq!(m.passed, 2);
        // the same test in two shards counts once
        let m = merge_outcomes(
            None,
            &[
                outcome("s0", true, &[("a", Passed)]),
                outcome("s1", true, &[("a", Passed)]),
            ],
        );
        assert_eq!(m.passed, 1);
        assert_eq!(m.problems.len(), 1);
        // a native split lists nothing to check against
        let m = merge_outcomes(None, &[outcome("s0", true, &[("q", Passed)])]);
        assert!(m.ok());
        // a failing shard with no failed test is a problem; a failed test explains it
        let m = merge_outcomes(None, &[outcome("s0", false, &[("q", Passed)])]);
        assert_eq!(
            m.problems,
            vec!["shard s0 of j failed without a failed test"]
        );
        let m = merge_outcomes(None, &[outcome("s0", true, &[("q", Failed)])]);
        assert!(m.problems.is_empty());
        assert!(!m.ok());
        // the trait method merges the same way
        let f = fake(OK);
        let m = f.merge(
            Some(&["x".to_string()]),
            &[outcome("s0", true, &[("x", TestStatus::Failed)])],
        );
        assert_eq!(m.failed, vec!["x"]);
    }

    #[test]
    fn the_default_adapter_methods_are_the_free_functions() {
        let f = fake(OK);
        assert_eq!(f.info().runner, "fake");
        assert!(f.discover_command().argv.is_empty());
        assert!(f
            .discover(&CommandOutput {
                stdout: String::new(),
                stderr: String::new(),
                success: true
            })
            .is_err());
        let shard = Shard {
            index: 0,
            tests: vec![],
            load: 0,
        };
        assert!(f.invoke(&shard, 1, SplitMode::Single).env.is_empty());
        assert_eq!(
            f.collect(
                "j",
                "s",
                &CommandOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    success: true
                }
            )
            .shard_id,
            "s"
        );
    }

    fn makespan(shards: &[Vec<&str>], millis: &BTreeMap<String, u64>) -> u64 {
        shards
            .iter()
            .map(|s| s.iter().map(|n| millis[*n]).sum::<u64>())
            .max()
            .unwrap()
    }

    #[test]
    fn longest_first_beats_a_split_by_name_order() {
        // the slow tests come first by name: halves by name put them together
        let weights = [
            ("t0", 90),
            ("t1", 80),
            ("t2", 70),
            ("t3", 60),
            ("t4", 10),
            ("t5", 20),
            ("t6", 30),
            ("t7", 40),
        ];
        let h = hist(&weights);
        let all: Vec<&str> = weights.iter().map(|(n, _)| *n).collect();
        let d = found(&all);
        let p = partition_tests(&fake(OK), &d, &h, 2, &PartitionOpts::default());
        let lpt = makespan(&names(&p), &h.millis);
        let naive: Vec<Vec<&str>> = all.chunks(4).map(<[&str]>::to_vec).collect();
        let by_name = makespan(&naive, &h.millis);
        assert_eq!((lpt, by_name), (200, 300));
        assert!(lpt < by_name);
    }

    #[test]
    fn a_native_split_has_no_more_shards_than_listed_tests() {
        let native = Capabilities {
            targets: false,
            native_shard: true,
            ..OK
        };
        let d = found(&["a", "b", "c"]);
        let p = partition_tests(
            &fake(native),
            &d,
            &History::default(),
            8,
            &PartitionOpts::default(),
        );
        assert_eq!((p.mode, p.shards.len()), (SplitMode::Native, 3));
        // one test is nothing to split
        let one = found(&["a"]);
        let p = partition_tests(
            &fake(native),
            &one,
            &History::default(),
            8,
            &PartitionOpts::default(),
        );
        assert_eq!(p.not_split, vec![NotSplit::TooSmall]);
        assert!(!p.is_split());
    }

    #[test]
    fn what_the_job_filters_is_a_reason_not_to_split() {
        let d = found(&["a", "b"]);
        let bad = |c: Capabilities| not_split_reasons(&c, &d);
        assert_eq!(
            bad(Capabilities {
                job_filtered: true,
                ..OK
            }),
            vec![NotSplit::JobFilter]
        );
        assert_eq!(
            bad(Capabilities {
                unreadable_args: true,
                ..OK
            }),
            vec![NotSplit::UnreadableArgs]
        );
        // a path that is part of another selects both when given as a filter
        let sub = Capabilities {
            substring_targets: true,
            ..OK
        };
        assert_eq!(bad(sub), vec![]);
        let overlapping = found(&["src/a.test.js", "lib/src/a.test.js", "b.test.js"]);
        assert_eq!(
            not_split_reasons(&sub, &overlapping),
            vec![NotSplit::PathOverlap]
        );
        assert_eq!(not_split_reasons(&OK, &overlapping), vec![]);
        assert!(!has_overlap(&["a".into(), "a".into(), "b".into()]));
    }

    #[test]
    fn a_command_too_long_to_start_is_not_split() {
        use super::adapters::{CargoTest, Safety};
        let c = CargoTest::new("1", &[], Safety::default());
        let long = "x".repeat(1100);
        let names: Vec<String> = (0..200).map(|i| format!("{long}{i:04}")).collect();
        let d = Discovered {
            tests: names,
            complete: true,
        };
        let p = partition_tests(&c, &d, &History::default(), 2, &PartitionOpts::default());
        assert_eq!(p.not_split, vec![NotSplit::ArgvTooLarge]);
        assert!(!p.is_split());
        // the same names in shards that fit are split
        let p = partition_tests(&c, &d, &History::default(), 8, &PartitionOpts::default());
        assert!(p.is_split());
        let longest = p
            .shards
            .iter()
            .map(|s| argv_bytes(&c.invoke(s, 8, SplitMode::Listed)))
            .max()
            .unwrap();
        assert!(longest <= MAX_ARGV_BYTES);
    }

    #[test]
    fn the_length_of_a_command_counts_each_word_and_its_separator() {
        let inv = |a: &[&str]| Invocation {
            argv: a.iter().map(|s| s.to_string()).collect(),
            env: vec![],
        };
        assert_eq!(argv_bytes(&inv(&["ab", "c"])), 5);
        assert_eq!(argv_bytes(&inv(&[])), 0);
        // the most a command may take is allowed, one byte more is not
        use super::adapters::{CargoTest, Safety};
        let c = CargoTest::new("1", &[], Safety::default());
        // the words without a test; a name and its separator come on top
        let fixed = argv_bytes(&c.invoke(
            &Shard {
                index: 0,
                tests: vec![],
                load: 0,
            },
            2,
            SplitMode::Listed,
        ));
        for (len, split) in [
            (MAX_ARGV_BYTES - fixed - 1, true),
            (MAX_ARGV_BYTES - fixed, false),
        ] {
            let names: Vec<String> = ["a", "b"].iter().map(|c| c.repeat(len)).collect();
            let d = Discovered {
                tests: names,
                complete: true,
            };
            let p = partition_tests(&c, &d, &History::default(), 2, &PartitionOpts::default());
            assert_eq!(p.is_split(), split, "{len}");
            if split {
                let worst = p
                    .shards
                    .iter()
                    .map(|s| argv_bytes(&c.invoke(s, 2, SplitMode::Listed)))
                    .max()
                    .unwrap();
                assert_eq!(worst, MAX_ARGV_BYTES);
            }
        }
    }

    #[test]
    fn an_outcome_without_the_flag_was_read() {
        let o: ShardOutcome =
            serde_json::from_str(r#"{"job_id":"j","shard_id":"s","tests":[],"exit_ok":true}"#)
                .unwrap();
        assert!(o.parsed);
    }

    #[test]
    fn a_shard_that_reports_nothing_readable_is_a_problem() {
        use TestStatus::*;
        let mut unread = outcome("s0", true, &[]);
        unread.parsed = false;
        let m = merge_outcomes(None, &[unread]);
        assert_eq!(
            m.problems,
            vec!["the report of shard s0 of j could not be read"]
        );
        // it succeeded and ran nothing
        let m = merge_outcomes(None, &[outcome("s0", true, &[])]);
        assert_eq!(
            m.problems,
            vec!["shard s0 of j succeeded but reported no test"]
        );
        // a failure with no report is the failure, once
        let m = merge_outcomes(None, &[outcome("s0", false, &[])]);
        assert_eq!(
            m.problems,
            vec!["shard s0 of j failed without a failed test"]
        );
        assert!(merge_outcomes(None, &[outcome("s0", true, &[("a", Passed)])]).ok());
    }

    #[test]
    fn a_shard_reports_only_what_it_was_given() {
        use TestStatus::*;
        let man: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let mut rules = Rules::default();
        rules.assigned.insert("s0", ["a"].into());
        rules.assigned.insert("s1", ["b"].into());
        let both = [
            outcome("s0", true, &[("a", Passed)]),
            outcome("s1", true, &[("b", Passed)]),
        ];
        assert!(merge_with(Some(&man), &rules, &both).ok());
        // s1 ran a test of s0: the set is complete, but not as it was divided
        let swapped = [
            outcome("s0", true, &[("a", Passed), ("b", Passed)]),
            outcome("s1", true, &[("b", Passed)]),
        ];
        let m = merge_with(Some(&man), &rules, &swapped);
        assert_eq!(
            m.problems,
            vec![
                "`b` was reported by shard s0 of j, which was not given it",
                "`b` was reported more than once"
            ]
        );
        // a shard the rules do not name is not checked
        let m = merge_with(Some(&man), &Rules::default(), &swapped);
        assert_eq!(m.problems, vec!["`b` was reported more than once"]);
        // a partial listing: more than listed is fine, less is not
        let partial = Rules {
            allow_unlisted: true,
            ..Rules::default()
        };
        let m = merge_with(
            Some(&man),
            &partial,
            &[outcome(
                "s0",
                true,
                &[("a", Passed), ("z", Passed), ("b", Passed)],
            )],
        );
        assert!(m.ok(), "{:?}", m.problems);
        let m = merge_with(
            Some(&man),
            &partial,
            &[outcome("s0", true, &[("z", Passed)])],
        );
        assert_eq!(m.problems.len(), 2);
    }
}
