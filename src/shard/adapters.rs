//! Runner adapters for [`crate::shard`]: `cargo test`, Vitest and Jest.
//!
//! An adapter builds the command that lists the tests or runs a shard and reads
//! what the command printed. It starts nothing; [`run_invocation`] does that
//! for a caller that has an [`ExecContext`].
//!
//! Test identities: `cargo test` lists tests by libtest name (a name that two
//! test targets share makes the listing incomplete, since a name filter cannot
//! tell them apart, and so does a doctest, which cannot be run by name, and a
//! target with its own harness, which does not list the way libtest does);
//! Vitest and Jest split by file, and a file is named relative to the project
//! root, which is the working directory of the step that runs the tests.
//!
//! An adapter is made from the job's own command ([`parse_command`]) and keeps
//! only the arguments that are known to leave the set of tests alone; a name
//! filter, a skip, a tag or an argument it does not know makes the job
//! unsplittable, since a shard's command would run more (or other) tests than
//! the job did. [`from_info`] makes the adapter again from what a plan holds,
//! where the tests are run.

use super::{
    Capabilities, CommandOutput, Discovered, Invocation, RunnerAdapter, RunnerInfo, Shard,
    ShardOutcome, SplitMode, TestResult, TestStatus,
};
use crate::context::ExecContext;
use crate::dag::NotSplit;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// What the caller knows about the job that the runner cannot tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Safety {
    /// The tests of different shards update state they share.
    pub shared_state: bool,
    /// Each shard gets its own database, port and container where the tests
    /// use them.
    pub isolated_fixtures: bool,
}

impl Default for Safety {
    /// Unit tests that touch nothing outside their checkout.
    fn default() -> Self {
        Safety {
            shared_state: false,
            isolated_fixtures: true,
        }
    }
}

fn caps(native_shard: bool, safety: Safety, blocked: &[NotSplit], substring: bool) -> Capabilities {
    Capabilities {
        enumerates: true,
        targets: true,
        native_shard,
        shared_state: safety.shared_state,
        isolated_fixtures: safety.isolated_fixtures,
        aggregates_exactly: true,
        job_filtered: blocked.contains(&NotSplit::JobFilter),
        unreadable_args: blocked.contains(&NotSplit::UnreadableArgs),
        substring_targets: substring,
    }
}

// ---- the job's own command ----

/// The command of a job, read: which runner, the arguments to keep, and why
/// the job cannot be split (empty when it can).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// `cargo-test`, `vitest` or `jest`.
    pub runner: &'static str,
    /// The arguments every command of the runner keeps (they say which
    /// project, config or features, not which tests).
    pub args: Vec<String>,
    pub blocked: Vec<NotSplit>,
}

/// Which arguments of a runner are known.
struct Known {
    /// Flags that change nothing about the set of tests.
    flags: &'static [&'static str],
    /// The same, with a value (`--config x` or `--config=x`).
    valued: &'static [&'static str],
    /// Flags that choose tests.
    filters: &'static [&'static str],
    /// The same, with a value.
    filters_valued: &'static [&'static str],
}

const CARGO: Known = Known {
    flags: &[
        "--workspace",
        "--all",
        "--lib",
        "--bins",
        "--tests",
        "--release",
        "-r",
        "--all-features",
        "--no-default-features",
        "--locked",
        "--offline",
        "--frozen",
        "--no-fail-fast",
    ],
    valued: &[
        "-p",
        "--package",
        "--features",
        "-F",
        "--target",
        "--manifest-path",
        "--profile",
        "--exclude",
        "-j",
        "--jobs",
        "--test",
        "--bin",
    ],
    filters: &[],
    filters_valued: &[],
};

const VITEST: Known = Known {
    flags: &["--silent"],
    valued: &["--config", "-c", "--project"],
    filters: &["--changed"],
    filters_valued: &["-t", "--testNamePattern", "--exclude", "--shard", "--dir"],
};

const JEST: Known = Known {
    flags: &["--ci", "--silent", "--runInBand", "-i"],
    valued: &["--config", "-c"],
    filters: &["--onlyChanged", "-o", "--onlyFailures"],
    filters_valued: &[
        "-t",
        "--testNamePattern",
        "--testPathPattern",
        "--testPathPatterns",
        "--testPathIgnorePatterns",
        "--testMatch",
        "--testRegex",
        "--changedSince",
        "--findRelatedTests",
        "--shard",
    ],
};

/// The command of `run` as one of the runners, `None` when it is none.
///
/// Anything but one plain command (a `&&`, a pipe, a variable, a quote that
/// does not close) leaves the arguments unread. So does every argument that is
/// not known to keep the tests as they are. The name filters of `cargo test`
/// are the words before and after its `--`.
pub fn parse_command(run: &str) -> Option<Parsed> {
    let words = shlex::split(run.trim())?;
    let (runner, known, args): (&'static str, &Known, &[String]) = match words.as_slice() {
        [c, t, rest @ ..] if c == "cargo" && t == "test" => ("cargo-test", &CARGO, rest),
        [n, v, rest @ ..] if n == "npx" && v == "vitest" => ("vitest", &VITEST, rest),
        [v, rest @ ..] if v == "vitest" => ("vitest", &VITEST, rest),
        [n, j, rest @ ..] if n == "npx" && j == "jest" => ("jest", &JEST, rest),
        [j, rest @ ..] if j == "jest" => ("jest", &JEST, rest),
        _ => return None,
    };
    let mut parsed = Parsed {
        runner,
        args: vec![],
        blocked: vec![],
    };
    if run.chars().any(|c| ";|&<>`$()\n".contains(c)) {
        parsed.blocked.push(NotSplit::UnreadableArgs);
        return Some(parsed);
    }
    let mut block = |why: NotSplit| {
        if !parsed.blocked.contains(&why) {
            parsed.blocked.push(why);
        }
    };
    let mut keep = Vec::new();
    let mut i = 0;
    // Vitest's `run` is its default mode, not a filter
    if runner == "vitest" && args.first().is_some_and(|a| a == "run") {
        i = 1;
    }
    while i < args.len() {
        let a = args[i].as_str();
        let name = if a.starts_with("--") {
            a.split('=').next().unwrap_or(a)
        } else {
            a
        };
        let inline = a.starts_with("--") && a.contains('=');
        // the words after `--` go to the test binary of `cargo test`
        if runner == "cargo-test" && a == "--" {
            let rest = &args[i + 1..];
            if !rest.is_empty() {
                let narrows = rest.iter().any(|t| {
                    !t.starts_with('-')
                        || ["--skip", "--ignored", "--include-ignored", "--exact"]
                            .iter()
                            .any(|f| t.split('=').next() == Some(f))
                });
                block(if narrows {
                    NotSplit::JobFilter
                } else {
                    NotSplit::UnreadableArgs
                });
            }
            break;
        }
        let takes_value = !inline;
        if known.flags.contains(&name) && !inline {
            if name != "--no-fail-fast" {
                keep.push(a.to_string());
            }
        } else if known.valued.contains(&name) {
            keep.push(a.to_string());
            if takes_value {
                match args.get(i + 1) {
                    Some(v) => keep.push(v.clone()),
                    None => block(NotSplit::UnreadableArgs),
                }
                i += 1;
            }
        } else if known.filters.contains(&name) && !inline {
            block(NotSplit::JobFilter);
        } else if known.filters_valued.contains(&name) {
            block(NotSplit::JobFilter);
            if takes_value {
                i += 1;
            }
        } else if !a.starts_with('-') {
            block(NotSplit::JobFilter);
        } else {
            block(NotSplit::UnreadableArgs);
        }
        i += 1;
    }
    parsed.args = keep;
    Some(parsed)
}

/// The adapter a plan's runner was made for, to run and read one shard where
/// the tests are: `root` is the project root (the working directory of the
/// step). `None` for a runner this module does not know.
pub fn from_info(info: &RunnerInfo, root: &str) -> Option<Box<dyn RunnerAdapter>> {
    let safety = Safety::default();
    Some(match info.runner.as_str() {
        "cargo-test" => Box::new(CargoTest {
            info: info.clone(),
            safety,
            args: info.config.clone(),
            blocked: vec![],
        }),
        "vitest" => Box::new(Vitest {
            info: info.clone(),
            safety,
            root: root.into(),
            command: argv(&["npx", "vitest"]),
            blocked: vec![],
        }),
        "jest" => Box::new(Jest {
            info: info.clone(),
            safety,
            root: root.into(),
            command: argv(&["npx", "jest"]),
            blocked: vec![],
        }),
        _ => return None,
    })
}

/// A command as the text of a shell script (the words quoted), and `None` for
/// a word a shell cannot be given.
pub fn script_of(inv: &Invocation) -> Option<String> {
    let mut words = Vec::new();
    if !inv.env.is_empty() {
        words.push("env".to_string());
        for (k, v) in &inv.env {
            words.push(shlex::try_quote(&format!("{k}={v}")).ok()?.into_owned());
        }
    }
    for a in &inv.argv {
        words.push(shlex::try_quote(a).ok()?.into_owned());
    }
    Some(words.join(" "))
}

/// Run `inv` in `cwd` and capture what it prints.
pub fn run_invocation(ctx: &ExecContext, inv: &Invocation, cwd: &Path) -> Result<CommandOutput> {
    let (program, args) = inv.argv.split_first().context("an empty command")?;
    let out = ctx
        .command(program)
        .args(args)
        .envs(inv.env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .output()
        .with_context(|| format!("cannot start {program}"))?;
    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
    })
}

fn failed_listing(out: &CommandOutput) -> String {
    let why = out
        .stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no output");
    format!("the listing failed: {why}")
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn invocation(argv: Vec<String>) -> Invocation {
    Invocation { argv, env: vec![] }
}

// ---- cargo test ----

/// `cargo test`.
#[derive(Debug, Clone)]
pub struct CargoTest {
    info: RunnerInfo,
    safety: Safety,
    /// Arguments of the job's own `cargo test` before the `--`
    /// (`--workspace`, `--features x`), without the filter.
    args: Vec<String>,
    /// Why the job's own command cannot be split.
    blocked: Vec<NotSplit>,
}

impl CargoTest {
    /// A runner for a command whose arguments `args` are known to leave the
    /// tests alone.
    pub fn new(version: &str, args: &[&str], safety: Safety) -> Self {
        CargoTest {
            info: RunnerInfo {
                runner: "cargo-test".into(),
                version: version.into(),
                config: args.iter().map(|s| s.to_string()).collect(),
                filters: vec![],
            },
            safety,
            args: args.iter().map(|s| s.to_string()).collect(),
            blocked: vec![],
        }
    }

    /// The runner of a job's `run` text; `None` when it is not `cargo test`.
    /// A filter or an argument it does not know is kept as a reason the job is
    /// not split.
    pub fn from_command(version: &str, run: &str, safety: Safety) -> Option<Self> {
        let p = parse_command(run).filter(|p| p.runner == "cargo-test")?;
        let args: Vec<&str> = p.args.iter().map(String::as_str).collect();
        Some(CargoTest {
            blocked: p.blocked,
            ..CargoTest::new(version, &args, safety)
        })
    }

    fn command(&self, extra: &[&str]) -> Vec<String> {
        let mut v = argv(&["cargo", "test"]);
        v.extend(self.args.iter().cloned());
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    }
}

fn is_doctest(name: &str) -> bool {
    name.contains(" - ") && name.ends_with(')') && name.contains("(line ")
}

/// A line `N tests, M benchmarks`: what libtest ends a listing with.
fn is_list_summary(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    matches!(words.as_slice(), [n, t, m, b]
        if n.parse::<u64>().is_ok()
            && matches!(*t, "test," | "tests,")
            && m.parse::<u64>().is_ok()
            && matches!(*b, "benchmark" | "benchmarks"))
}

impl RunnerAdapter for CargoTest {
    fn info(&self) -> &RunnerInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        caps(false, self.safety, &self.blocked, false)
    }

    fn discover_command(&self) -> Invocation {
        invocation(self.command(&["--", "--list"]))
    }

    fn discover(&self, out: &CommandOutput) -> Result<Discovered, String> {
        if !out.success {
            return Err(failed_listing(out));
        }
        let mut tests = Vec::new();
        let mut complete = true;
        let mut seen = BTreeSet::new();
        for name in out.stdout.lines().filter_map(|l| l.strip_suffix(": test")) {
            if is_doctest(name) || !seen.insert(name) {
                complete = false;
            }
            tests.push(name.to_string());
        }
        // Every target cargo ran ends its listing with a summary. A target
        // with a harness of its own (`harness = false`) prints what it likes,
        // so its tests are not in the list.
        let targets = out
            .stderr
            .lines()
            .filter(|l| l.trim_start().starts_with("Running "))
            .count();
        let summaries = out.stdout.lines().filter(|l| is_list_summary(l)).count();
        if targets > summaries {
            complete = false;
        }
        tests.sort();
        tests.dedup();
        Ok(Discovered { tests, complete })
    }

    fn invoke(&self, shard: &Shard, _count: usize, mode: SplitMode) -> Invocation {
        let mut v = self.command(&["--no-fail-fast"]);
        if mode == SplitMode::Listed {
            v.push("--".into());
            v.push("--exact".into());
            v.extend(shard.tests.iter().cloned());
        }
        invocation(v)
    }

    fn collect(&self, job_id: &str, shard_id: &str, out: &CommandOutput) -> ShardOutcome {
        let tests = out
            .stdout
            .lines()
            .filter_map(|l| {
                let rest = l.strip_prefix("test ")?;
                let (name, result) = rest.rsplit_once(" ... ")?;
                let status = match result.split(',').next()? {
                    "ok" => TestStatus::Passed,
                    "FAILED" => TestStatus::Failed,
                    "ignored" => TestStatus::Skipped,
                    _ => return None,
                };
                Some(TestResult {
                    identity: name.strip_suffix(" - should panic").unwrap_or(name).into(),
                    status,
                })
            })
            .collect();
        ShardOutcome {
            job_id: job_id.into(),
            shard_id: shard_id.into(),
            tests,
            exit_ok: out.success,
            // a run that got as far as running tests ends with a result line
            parsed: out.stdout.lines().any(|l| l.starts_with("test result:")),
        }
    }
}

// ---- Vitest and Jest ----

/// A file of a JS project, relative to `root` when it is under it. The root is
/// compared as the system spells it (links followed), which is how reporters
/// print the paths of files.
fn relative(root: &str, path: &str) -> String {
    let real = std::fs::canonicalize(root).ok();
    let real = real.as_deref().and_then(Path::to_str);
    for r in real.into_iter().chain([root]) {
        let r = r.trim_end_matches('/');
        if let Some(rest) = path.strip_prefix(r).and_then(|p| p.strip_prefix('/')) {
            return rest.to_string();
        }
    }
    path.to_string()
}

/// A file as a command is given it: a path outside the root as it is, one
/// under it from the root, spelled as the system does (a runner compares paths
/// with the real ones).
fn absolute(root: &str, file: &str) -> String {
    if Path::new(file).is_absolute() {
        return file.to_string();
    }
    let real = std::fs::canonicalize(root).ok();
    let root = real.as_deref().and_then(Path::to_str).unwrap_or(root);
    format!("{}/{file}", root.trim_end_matches('/'))
}

/// The JSON report a reporter printed among other output: the first value
/// that starts a line and has a list of `testResults`.
fn json_of(stdout: &str) -> Option<Value> {
    let mut start = 0;
    for line in stdout.split_inclusive('\n') {
        if line.starts_with('{') {
            let mut values =
                serde_json::Deserializer::from_str(&stdout[start..]).into_iter::<Value>();
            if let Some(Ok(v)) = values.next() {
                if v.get("testResults").is_some_and(Value::is_array) {
                    return Some(v);
                }
            }
        }
        start += line.len();
    }
    None
}

/// How a test file went, from its own status and that of its tests.
fn file_status(file: &str, tests: &[&str]) -> TestStatus {
    if file == "failed" || tests.contains(&"failed") {
        TestStatus::Failed
    } else if !tests.is_empty()
        && tests
            .iter()
            .all(|t| matches!(*t, "skipped" | "pending" | "todo" | "disabled"))
    {
        TestStatus::Skipped
    } else {
        TestStatus::Passed
    }
}

/// The files of a Vitest or Jest JSON report, by the name they are listed by.
fn collect_js(root: &str, job_id: &str, shard_id: &str, out: &CommandOutput) -> ShardOutcome {
    let report = json_of(&out.stdout);
    let tests = report
        .as_ref()
        .and_then(|r| r.get("testResults"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            let name = r.get("name")?.as_str()?;
            let own = r.get("status").and_then(Value::as_str).unwrap_or("");
            let tests: Vec<&str> = r
                .get("assertionResults")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|t| t.get("status").and_then(Value::as_str))
                .collect();
            Some(TestResult {
                identity: relative(root, name),
                status: file_status(own, &tests),
            })
        })
        .collect();
    ShardOutcome {
        job_id: job_id.into(),
        shard_id: shard_id.into(),
        tests,
        exit_ok: out.success,
        parsed: report.is_some(),
    }
}

/// The files a listing prints, one per line, by the name they are listed by.
fn listed_files(root: &str, out: &CommandOutput) -> Result<Discovered, String> {
    if !out.success {
        return Err(failed_listing(out));
    }
    let mut tests: Vec<String> = out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| relative(root, l))
        .collect();
    tests.sort();
    tests.dedup();
    Ok(Discovered {
        tests,
        complete: true,
    })
}

/// Vitest, split by test file.
#[derive(Debug, Clone)]
pub struct Vitest {
    info: RunnerInfo,
    safety: Safety,
    /// The project root, absolute.
    root: String,
    /// The command that starts Vitest.
    command: Vec<String>,
    blocked: Vec<NotSplit>,
}

impl Vitest {
    pub fn new(version: &str, root: &str, config: &[&str], safety: Safety) -> Self {
        Vitest {
            info: RunnerInfo {
                runner: "vitest".into(),
                version: version.into(),
                config: config.iter().map(|s| s.to_string()).collect(),
                filters: vec![],
            },
            safety,
            root: root.into(),
            command: argv(&["npx", "vitest"]),
            blocked: vec![],
        }
    }

    /// The runner of a job's `run` text; `None` when it is not Vitest.
    pub fn from_command(version: &str, root: &str, run: &str, safety: Safety) -> Option<Self> {
        let p = parse_command(run).filter(|p| p.runner == "vitest")?;
        let args: Vec<&str> = p.args.iter().map(String::as_str).collect();
        Some(Vitest {
            blocked: p.blocked,
            ..Vitest::new(version, root, &args, safety)
        })
    }
}

impl RunnerAdapter for Vitest {
    fn info(&self) -> &RunnerInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        // a path given to Vitest is a filter on the paths of files
        caps(true, self.safety, &self.blocked, true)
    }

    fn discover_command(&self) -> Invocation {
        let mut v = self.command.clone();
        v.extend(argv(&["list", "--filesOnly"]));
        v.extend(self.info.config.iter().cloned());
        invocation(v)
    }

    fn discover(&self, out: &CommandOutput) -> Result<Discovered, String> {
        listed_files(&self.root, out)
    }

    fn invoke(&self, shard: &Shard, count: usize, mode: SplitMode) -> Invocation {
        let mut v = self.command.clone();
        v.extend(argv(&["run", "--reporter=json"]));
        v.extend(self.info.config.iter().cloned());
        match mode {
            SplitMode::Single => {}
            SplitMode::Native => v.push(format!("--shard={}/{count}", shard.index + 1)),
            SplitMode::Listed => v.extend(shard.tests.iter().map(|f| absolute(&self.root, f))),
        }
        invocation(v)
    }

    fn collect(&self, job_id: &str, shard_id: &str, out: &CommandOutput) -> ShardOutcome {
        collect_js(&self.root, job_id, shard_id, out)
    }
}

/// Jest, split by test file.
#[derive(Debug, Clone)]
pub struct Jest {
    info: RunnerInfo,
    safety: Safety,
    root: String,
    command: Vec<String>,
    blocked: Vec<NotSplit>,
}

impl Jest {
    pub fn new(version: &str, root: &str, config: &[&str], safety: Safety) -> Self {
        Jest {
            info: RunnerInfo {
                runner: "jest".into(),
                version: version.into(),
                config: config.iter().map(|s| s.to_string()).collect(),
                filters: vec![],
            },
            safety,
            root: root.into(),
            command: argv(&["npx", "jest"]),
            blocked: vec![],
        }
    }

    /// The runner of a job's `run` text; `None` when it is not Jest.
    pub fn from_command(version: &str, root: &str, run: &str, safety: Safety) -> Option<Self> {
        let p = parse_command(run).filter(|p| p.runner == "jest")?;
        let args: Vec<&str> = p.args.iter().map(String::as_str).collect();
        Some(Jest {
            blocked: p.blocked,
            ..Jest::new(version, root, &args, safety)
        })
    }
}

impl RunnerAdapter for Jest {
    fn info(&self) -> &RunnerInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        caps(true, self.safety, &self.blocked, false)
    }

    fn discover_command(&self) -> Invocation {
        let mut v = self.command.clone();
        v.push("--listTests".into());
        v.extend(self.info.config.iter().cloned());
        invocation(v)
    }

    fn discover(&self, out: &CommandOutput) -> Result<Discovered, String> {
        listed_files(&self.root, out)
    }

    fn invoke(&self, shard: &Shard, count: usize, mode: SplitMode) -> Invocation {
        let mut v = self.command.clone();
        v.push("--json".into());
        v.extend(self.info.config.iter().cloned());
        match mode {
            SplitMode::Single => {}
            SplitMode::Native => v.push(format!("--shard={}/{count}", shard.index + 1)),
            SplitMode::Listed => {
                v.push("--runTestsByPath".into());
                v.extend(shard.tests.iter().map(|f| absolute(&self.root, f)));
            }
        }
        invocation(v)
    }

    fn collect(&self, job_id: &str, shard_id: &str, out: &CommandOutput) -> ShardOutcome {
        collect_js(&self.root, job_id, shard_id, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(stdout: &str, success: bool) -> CommandOutput {
        CommandOutput {
            stdout: stdout.into(),
            stderr: "warn\nboom\n".into(),
            success,
        }
    }

    fn shard(tests: &[&str]) -> Shard {
        Shard {
            index: 1,
            tests: tests.iter().map(|s| s.to_string()).collect(),
            load: 0,
        }
    }

    #[test]
    fn cargo_lists_and_filters_by_name() {
        let c = CargoTest::new("1.92", &["--workspace"], Safety::default());
        assert_eq!(c.info().runner, "cargo-test");
        assert_eq!(c.info().config, vec!["--workspace"]);
        assert_eq!(
            c.discover_command().argv,
            ["cargo", "test", "--workspace", "--", "--list"]
        );
        let listing = "b::two: test\na::one: test\n\n2 tests, 0 benchmarks\nbench_x: benchmark\n";
        let d = c.discover(&out(listing, true)).unwrap();
        assert_eq!(d.tests, ["a::one", "b::two"]);
        assert!(d.complete);
        // the same name in two targets, and a doctest, make it incomplete
        let d = c.discover(&out("x: test\nx: test\n", true)).unwrap();
        assert_eq!(d.tests, ["x"]);
        assert!(!d.complete);
        let d = c
            .discover(&out("src/lib.rs - add (line 1): test\n", true))
            .unwrap();
        assert!(!d.complete);
        assert!(c.discover(&out("", false)).unwrap_err().contains("boom"));
        let silent = CommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            success: false,
        };
        assert!(c.discover(&silent).unwrap_err().contains("no output"));

        let s = shard(&["a::one", "b::two"]);
        assert_eq!(
            c.invoke(&s, 2, SplitMode::Listed).argv,
            [
                "cargo",
                "test",
                "--workspace",
                "--no-fail-fast",
                "--",
                "--exact",
                "a::one",
                "b::two"
            ]
        );
        assert_eq!(
            c.invoke(&s, 2, SplitMode::Single).argv,
            ["cargo", "test", "--workspace", "--no-fail-fast"]
        );
        let caps = c.capabilities();
        assert!(caps.enumerates && caps.targets && !caps.native_shard);
    }

    #[test]
    fn cargo_collects_each_line_of_the_report() {
        let c = CargoTest::new("1", &[], Safety::default());
        let text = "running 5 tests\ntest a ... ok\ntest b ... FAILED\ntest c ... ignored, slow\n\
test d - should panic ... ok\ntest e ... bench\nnot a test line\ntest f without result\n";
        let o = c.collect("j", "s", &out(text, false));
        let got: Vec<(&str, TestStatus)> = o
            .tests
            .iter()
            .map(|t| (t.identity.as_str(), t.status))
            .collect();
        assert_eq!(
            got,
            [
                ("a", TestStatus::Passed),
                ("b", TestStatus::Failed),
                ("c", TestStatus::Skipped),
                ("d", TestStatus::Passed)
            ]
        );
        assert!(!o.exit_ok);
        assert_eq!((o.job_id.as_str(), o.shard_id.as_str()), ("j", "s"));
    }

    #[test]
    fn js_paths_are_relative_to_the_root() {
        assert_eq!(relative("/p", "/p/a/b.js"), "a/b.js");
        assert_eq!(relative("/p/", "/p/a.js"), "a.js");
        assert_eq!(relative("/p", "/pp/a.js"), "/pp/a.js");
        assert_eq!(relative("/p", "a.js"), "a.js");
        assert_eq!(absolute("/p/", "a/b.js"), "/p/a/b.js");
    }

    #[test]
    fn vitest_lists_files_and_runs_them_by_path() {
        let v = Vitest::new("2.1.9", "/p", &["--config=v.js"], Safety::default());
        assert_eq!(v.info().runner, "vitest");
        assert_eq!(
            v.discover_command().argv,
            ["npx", "vitest", "list", "--filesOnly", "--config=v.js"]
        );
        let d = v
            .discover(&out("b.test.js\n a.test.js \n\nb.test.js\n", true))
            .unwrap();
        assert_eq!(d.tests, ["a.test.js", "b.test.js"]);
        assert!(d.complete);
        let d = v.discover(&out("/p/x.test.js\n", true)).unwrap();
        assert_eq!(d.tests, ["x.test.js"]);
        assert!(v.discover(&out("", false)).is_err());
        let s = shard(&["a.test.js"]);
        assert_eq!(
            v.invoke(&s, 3, SplitMode::Listed).argv,
            [
                "npx",
                "vitest",
                "run",
                "--reporter=json",
                "--config=v.js",
                "/p/a.test.js"
            ]
        );
        assert_eq!(
            v.invoke(&s, 3, SplitMode::Native).argv.last().unwrap(),
            "--shard=2/3"
        );
        assert_eq!(
            v.invoke(&s, 3, SplitMode::Single).argv,
            ["npx", "vitest", "run", "--reporter=json", "--config=v.js"]
        );
        assert!(v.capabilities().native_shard);
    }

    #[test]
    fn jest_lists_files_and_runs_them_by_path() {
        let j = Jest::new("29", "/p", &[], Safety::default());
        assert_eq!(j.info().runner, "jest");
        assert_eq!(j.discover_command().argv, ["npx", "jest", "--listTests"]);
        let d = j
            .discover(&out("/p/z.test.js\n/p/a.test.js\n", true))
            .unwrap();
        assert_eq!(d.tests, ["a.test.js", "z.test.js"]);
        assert!(j.discover(&out("", false)).is_err());
        let s = shard(&["a.test.js", "z.test.js"]);
        assert_eq!(
            j.invoke(&s, 2, SplitMode::Listed).argv,
            [
                "npx",
                "jest",
                "--json",
                "--runTestsByPath",
                "/p/a.test.js",
                "/p/z.test.js"
            ]
        );
        assert_eq!(
            j.invoke(&s, 2, SplitMode::Native).argv,
            ["npx", "jest", "--json", "--shard=2/2"]
        );
        assert_eq!(
            j.invoke(&s, 2, SplitMode::Single).argv,
            ["npx", "jest", "--json"]
        );
        assert!(j.capabilities().native_shard);
    }

    #[test]
    fn a_js_report_gives_one_result_per_file() {
        let report = r#"{"testResults":[
          {"name":"/p/a.test.js","status":"passed","assertionResults":[{"status":"passed"},{"status":"pending"}]},
          {"name":"/p/b.test.js","status":"failed","assertionResults":[{"status":"failed"}]},
          {"name":"/p/c.test.js","status":"passed","assertionResults":[{"status":"skipped"},{"status":"todo"}]},
          {"name":"/p/d.test.js","status":"failed","assertionResults":[]},
          {"name":"/p/e.test.js","status":"passed"},
          {"status":"passed"}
        ]}"#;
        let v = Vitest::new("1", "/p", &[], Safety::default());
        for text in [report.to_string(), format!("noise\n{report}")] {
            let o = v.collect("j", "s", &out(&text, false));
            let got: Vec<(&str, TestStatus)> = o
                .tests
                .iter()
                .map(|t| (t.identity.as_str(), t.status))
                .collect();
            assert_eq!(
                got,
                [
                    ("a.test.js", TestStatus::Passed),
                    ("b.test.js", TestStatus::Failed),
                    ("c.test.js", TestStatus::Skipped),
                    ("d.test.js", TestStatus::Failed),
                    ("e.test.js", TestStatus::Passed),
                ]
            );
            assert!(!o.exit_ok);
        }
        // not a report: no result at all, and the shard did not succeed
        let j = Jest::new("1", "/p", &[], Safety::default());
        for text in ["", "{broken", "log\n{broken", "[]", "{\"testResults\":3}"] {
            let o = j.collect("j", "s", &out(text, false));
            assert!(o.tests.is_empty(), "{text}");
        }
        assert_eq!(j.collect("j", "s", &out(report, true)).tests.len(), 5);
    }

    #[test]
    fn safety_decides_what_the_runner_may_claim() {
        let s = Safety {
            shared_state: true,
            isolated_fixtures: false,
        };
        let c = CargoTest::new("1", &[], s).capabilities();
        assert!(c.shared_state && !c.isolated_fixtures);
        let d = Safety::default();
        assert!(!d.shared_state && d.isolated_fixtures);
    }

    #[test]
    fn an_invocation_runs_in_the_directory_it_is_given() {
        let ctx = ExecContext::new(std::collections::BTreeMap::from([(
            "PATH".to_string(),
            "/usr/bin:/bin".to_string(),
        )]));
        let dir = std::env::temp_dir();
        let inv = Invocation {
            argv: argv(&["sh", "-c", "echo $X; pwd; exit 3"]),
            env: vec![("X".into(), "v".into())],
        };
        let o = run_invocation(&ctx, &inv, &dir).unwrap();
        assert!(!o.success);
        assert!(o.stdout.starts_with("v\n"));
        assert!(run_invocation(&ctx, &invocation(vec![]), &dir).is_err());
        let missing = invocation(argv(&["no-such-program-l2"]));
        assert!(run_invocation(&ctx, &missing, &dir).is_err());
    }
}
