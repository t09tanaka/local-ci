//! Runner adapters for [`crate::shard`]: `cargo test`, Vitest, Jest, Playwright
//! and Cypress.
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

impl Safety {
    /// What an end-to-end suite is until the caller says otherwise: its tests
    /// reach a server, a database or a browser profile the workflow starts, and
    /// nothing says each shard gets its own. Such a job is not split
    /// ([`NotSplit::NoIsolation`]); a caller that has isolated them uses
    /// [`Safety::default`] or its own value.
    pub fn e2e() -> Self {
        Safety {
            shared_state: false,
            isolated_fixtures: false,
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
    /// `cargo-test`, `vitest`, `jest`, `playwright` or `cypress`.
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

const PLAYWRIGHT: Known = Known {
    flags: &["--headed", "--forbid-only", "--fully-parallel", "--no-deps"],
    valued: &[
        "--config",
        "-c",
        "--project",
        "--workers",
        "-j",
        "--retries",
        "--timeout",
        "--trace",
    ],
    filters: &["--last-failed", "--only-changed"],
    filters_valued: &[
        "--grep",
        "-g",
        "--grep-invert",
        "--gi",
        "--shard",
        "--tag",
        "--test-list",
        "--test-list-invert",
    ],
};

const CYPRESS: Known = Known {
    flags: &["--headless", "--headed", "--e2e"],
    valued: &[
        "--browser",
        "-b",
        "--config",
        "-c",
        "--config-file",
        "--project",
        "-P",
        "--env",
        "-e",
    ],
    filters: &[],
    filters_valued: &["--spec", "-s", "--tag"],
};

/// Words of an argument that choose which spec files Cypress runs.
const CYPRESS_SPEC_WORDS: &[&str] = &["specpattern", "testfiles", "folder"];

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
        [n, p, t, rest @ ..] if n == "npx" && p == "playwright" && t == "test" => {
            ("playwright", &PLAYWRIGHT, rest)
        }
        [p, t, rest @ ..] if p == "playwright" && t == "test" => ("playwright", &PLAYWRIGHT, rest),
        [n, c, r, rest @ ..] if n == "npx" && c == "cypress" && r == "run" => {
            ("cypress", &CYPRESS, rest)
        }
        [c, r, rest @ ..] if c == "cypress" && r == "run" => ("cypress", &CYPRESS, rest),
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
        // a job that collects coverage has reports that only a merge of its
        // shards' could give; nothing here collects them from the shards
        if a.starts_with("--coverage") || a.starts_with("--collectCoverage") {
            block(NotSplit::CoverageNotMerged);
            if takes_value && args.get(i + 1).is_some_and(|v| !v.starts_with('-')) {
                i += 1;
            }
        } else if known.flags.contains(&name) && !inline {
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
    if runner == "cypress"
        && keep.iter().any(|a| {
            let a = a.to_lowercase();
            CYPRESS_SPEC_WORDS.iter().any(|w| a.contains(w))
        })
    {
        block(NotSplit::JobFilter);
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
        "playwright" => Box::new(Playwright {
            info: info.clone(),
            safety,
            command: argv(&["npx", "playwright", "test"]),
            blocked: vec![],
        }),
        "cypress" => Box::new(Cypress {
            info: info.clone(),
            safety,
            root: root.into(),
            command: argv(&["npx", "cypress", "run"]),
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

/// `text` without the colour and cursor sequences (`ESC [ ... letter`) a
/// terminal would turn into style.
fn without_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
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
        // Plain, chatty output whatever the user's cargo configuration says:
        // the `Running` lines tell how many targets there are.
        Invocation {
            argv: self.command(&["--color", "never", "--", "--list"]),
            env: vec![
                ("CARGO_TERM_QUIET".into(), "false".into()),
                ("CARGO_TERM_VERBOSE".into(), "false".into()),
            ],
        }
    }

    fn discover(&self, out: &CommandOutput) -> Result<Discovered, String> {
        if !out.success {
            return Err(failed_listing(out));
        }
        let (stdout, stderr) = (without_ansi(&out.stdout), without_ansi(&out.stderr));
        let mut tests = Vec::new();
        let mut complete = true;
        let mut seen = BTreeSet::new();
        for line in stdout.lines() {
            match line.strip_suffix(": test") {
                Some(name) => {
                    if is_doctest(name) || !seen.insert(name) {
                        complete = false;
                    }
                    tests.push(name.to_string());
                }
                // what libtest prints around the tests; anything else is a
                // target that prints what it likes
                None if line.trim().is_empty()
                    || is_list_summary(line)
                    || line.ends_with(": benchmark") => {}
                None => complete = false,
            }
        }
        // Every target cargo ran ends its listing with a summary. A target
        // with a harness of its own (`harness = false`) prints what it likes,
        // so its tests are not in the list. The two streams cannot be read in
        // order, so the targets and the summaries are counted: a run that
        // shows no target (a quiet cargo), or one with a summary too few or too
        // many, is not known to be the whole.
        let targets = stderr
            .lines()
            .filter(|l| {
                let l = l.trim_start();
                l.starts_with("Running ") || l.starts_with("Doc-tests ")
            })
            .count();
        let summaries = stdout.lines().filter(|l| is_list_summary(l)).count();
        if targets == 0 || targets != summaries {
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
                let status = match result.split(',').next().unwrap_or_default() {
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

/// The JSON report of Vitest or Jest among other output.
fn json_of(stdout: &str) -> Option<Value> {
    json_with(stdout, "testResults")
}

/// The JSON report a reporter printed among other output: the first value
/// that starts a line and has a list under `key`.
fn json_with(stdout: &str, key: &str) -> Option<Value> {
    let mut start = 0;
    for line in stdout.split_inclusive('\n') {
        if line.starts_with('{') {
            let mut values =
                serde_json::Deserializer::from_str(&stdout[start..]).into_iter::<Value>();
            if let Some(Ok(v)) = values.next() {
                if v.get(key).is_some_and(Value::is_array) {
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

// ---- Playwright ----

/// The separator of the parts of a test's name: `[project] › file › title`,
/// which is also how `--test-list` reads a line.
const PW_SEP: &str = " › ";

/// A test of a Playwright JSON report.
struct PwTest {
    /// The project, the file (relative to the test directory), the titles of
    /// the groups around the test and its own.
    parts: Vec<String>,
    status: String,
    expected: String,
    ran: bool,
}

impl PwTest {
    fn identity(&self) -> String {
        format!(
            "[{}]{PW_SEP}{}",
            self.parts[0],
            self.parts[1..].join(PW_SEP)
        )
    }

    fn result(&self) -> TestResult {
        let status = match self.status.as_str() {
            "expected" | "flaky" => TestStatus::Passed,
            // a test that was meant to run and has no result was cut short
            // (a failure limit, a timeout of the run)
            "skipped" if !self.ran && self.expected != "skipped" => TestStatus::Failed,
            "skipped" => TestStatus::Skipped,
            _ => TestStatus::Failed,
        };
        TestResult {
            identity: self.identity(),
            status,
        }
    }
}

fn pw_walk(suite: &Value, file: &str, titles: &mut Vec<String>, out: &mut Vec<PwTest>) {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let list = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    for spec in list(suite, "specs") {
        for test in list(&spec, "tests") {
            let mut parts = vec![text(&test, "projectName"), file.to_string()];
            parts.extend(titles.iter().cloned());
            parts.push(text(&spec, "title"));
            out.push(PwTest {
                parts,
                status: text(&test, "status"),
                expected: text(&test, "expectedStatus"),
                ran: !list(&test, "results").is_empty(),
            });
        }
    }
    for child in list(suite, "suites") {
        titles.push(text(&child, "title"));
        pw_walk(&child, file, titles, out);
        titles.pop();
    }
}

/// The tests of a Playwright JSON report, in the order it holds them.
fn pw_tests(report: &Value) -> Vec<PwTest> {
    let mut out = Vec::new();
    for file in report
        .get("suites")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let name = file.get("file").and_then(Value::as_str).unwrap_or("");
        pw_walk(file, name, &mut Vec::new(), &mut out);
    }
    out
}

/// Something in the configuration that a shard run on its own cannot be given
/// a copy of, or that changes what a run reports.
fn pw_config_reason(report: &Value) -> Option<String> {
    let set = |v: Option<&Value>| match v {
        None | Some(Value::Null) => false,
        Some(Value::Array(a)) => !a.is_empty(),
        Some(_) => true,
    };
    let config = report.get("config");
    let has = |k: &str| set(config.and_then(|c| c.get(k)));
    if set(report.get("errors")) {
        return Some("the listing has errors".into());
    }
    if has("webServer") {
        return Some(
            "the configuration starts a `webServer`, which every shard would start on the same port"
                .into(),
        );
    }
    if has("globalSetup") || has("globalTeardown") {
        return Some(
            "the configuration has a global setup or teardown, which every shard would run".into(),
        );
    }
    if has("shard") {
        return Some("the configuration shards by itself".into());
    }
    let repeats = config
        .and_then(|c| c.get("projects"))
        .and_then(Value::as_array)
        .is_some_and(|ps| {
            ps.iter()
                .any(|p| p.get("repeatEach").and_then(Value::as_u64).unwrap_or(1) != 1)
        });
    repeats.then(|| "a project repeats its tests".into())
}

/// The names of the tests of a listing, or why they cannot be told apart by a
/// test list: a part that is empty, has a blank at its ends, holds the
/// separator or a line break; two tests with one name; a test named like a
/// group that holds others (the test list would select both).
fn pw_identities(tests: &[PwTest]) -> Result<Vec<String>, String> {
    if tests.is_empty() {
        return Err("the listing holds no test".into());
    }
    for t in tests {
        if let Some(bad) = t.parts.iter().enumerate().find(|(i, p)| {
            (*i > 0 && p.is_empty())
                || p.trim() != p.as_str()
                || p.contains('›')
                || p.contains('\n')
        }) {
            return Err(format!(
                "`{}` cannot be named in a test list (its part `{}` is empty, padded, or has a `›` or a line break)",
                t.identity(),
                bad.1
            ));
        }
    }
    let mut sorted: Vec<&Vec<String>> = tests.iter().map(|t| &t.parts).collect();
    sorted.sort();
    for w in sorted.windows(2) {
        if w[0] == w[1] {
            return Err(format!("`{}` is listed twice", w[0][1..].join(PW_SEP)));
        }
        if w[1].starts_with(w[0]) {
            return Err(format!(
                "`{}` is the name of a test and of a group that holds others",
                w[0][1..].join(PW_SEP)
            ));
        }
    }
    let mut ids: Vec<String> = tests.iter().map(PwTest::identity).collect();
    ids.sort();
    Ok(ids)
}

/// A shell script that writes the tests it is given (one per line) to a file
/// and runs `command` with `--test-list` on it.
fn pw_script(command: &[String]) -> String {
    let quoted: Vec<String> = command
        .iter()
        .map(|w| format!("'{}'", w.replace('\'', r"'\''")))
        .collect();
    format!(
        "list=$(mktemp \"${{TMPDIR:-/tmp}}/local-ci-tests.XXXXXX\") || exit 1; \
         printf '%s\\n' \"$@\" > \"$list\" || {{ rm -f \"$list\"; exit 1; }}; \
         {} --test-list \"$list\"; status=$?; rm -f \"$list\"; exit $status",
        quoted.join(" ")
    )
}

/// Playwright, split by test: a shard names its tests in a test list
/// (`--test-list`), each as `[project] › file › title`, so a test of each
/// project is a test of its own.
///
/// The runner's `--shard` is not used: it divides whole test groups, so
/// a shard can be left with no test, and a run with no test succeeds. A project
/// with `dependencies` (a setup project) is not visible in the listing; the
/// tests of the setup run in every shard that needs them, which the merge
/// reports as a test no shard was given.
#[derive(Debug, Clone)]
pub struct Playwright {
    info: RunnerInfo,
    safety: Safety,
    command: Vec<String>,
    blocked: Vec<NotSplit>,
}

impl Playwright {
    /// `config` are the arguments of the job's own command that leave the tests
    /// alone (`--config x`, `--project y`). An end-to-end suite starts with
    /// [`Safety::e2e`] unless the caller knows each shard has what it uses.
    pub fn new(version: &str, config: &[&str], safety: Safety) -> Self {
        Playwright {
            info: RunnerInfo {
                runner: "playwright".into(),
                version: version.into(),
                config: config.iter().map(|s| s.to_string()).collect(),
                filters: vec![],
            },
            safety,
            command: argv(&["npx", "playwright", "test"]),
            blocked: vec![],
        }
    }

    /// The runner of a job's `run` text; `None` when it is not `playwright test`.
    pub fn from_command(version: &str, run: &str, safety: Safety) -> Option<Self> {
        let p = parse_command(run).filter(|p| p.runner == "playwright")?;
        let args: Vec<&str> = p.args.iter().map(String::as_str).collect();
        Some(Playwright {
            blocked: p.blocked,
            ..Playwright::new(version, &args, safety)
        })
    }

    fn base(&self, extra: &[&str]) -> Vec<String> {
        let mut v = self.command.clone();
        v.extend(extra.iter().map(|s| s.to_string()));
        v.extend(self.info.config.iter().cloned());
        v
    }
}

impl RunnerAdapter for Playwright {
    fn info(&self) -> &RunnerInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        caps(false, self.safety, &self.blocked, false)
    }

    fn discover_command(&self) -> Invocation {
        invocation(self.base(&["--list", "--reporter=json"]))
    }

    fn discover(&self, out: &CommandOutput) -> Result<Discovered, String> {
        if !out.success {
            return Err(failed_listing(out));
        }
        let report = json_with(&out.stdout, "suites")
            .ok_or_else(|| "the listing holds no JSON report".to_string())?;
        if let Some(why) = pw_config_reason(&report) {
            return Err(why);
        }
        Ok(Discovered {
            tests: pw_identities(&pw_tests(&report))?,
            complete: true,
        })
    }

    fn invoke(&self, shard: &Shard, count: usize, mode: SplitMode) -> Invocation {
        let run = self.base(&["--reporter=json"]);
        match mode {
            SplitMode::Single => invocation(run),
            SplitMode::Native => {
                let mut v = run;
                v.push(format!("--shard={}/{count}", shard.index + 1));
                invocation(v)
            }
            SplitMode::Listed => {
                let mut v = argv(&["sh", "-c"]);
                v.push(pw_script(&run));
                v.push("sh".into());
                v.extend(shard.tests.iter().cloned());
                invocation(v)
            }
        }
    }

    fn collect(&self, job_id: &str, shard_id: &str, out: &CommandOutput) -> ShardOutcome {
        let report = json_with(&out.stdout, "suites");
        ShardOutcome {
            job_id: job_id.into(),
            shard_id: shard_id.into(),
            tests: report
                .as_ref()
                .map(|r| pw_tests(r).iter().map(PwTest::result).collect())
                .unwrap_or_default(),
            exit_ok: out.success,
            parsed: report.is_some(),
        }
    }
}

// ---- Cypress ----

/// What Cypress cannot be given as a spec: it reads the argument as a glob
/// list, so a comma, a glob character or a blank would select other files.
fn cypress_name_reason(name: &str) -> Option<char> {
    name.chars()
        .find(|c| c.is_whitespace() || ",*?[]{}()!".contains(*c))
}

/// The script that lists the spec files of Cypress's default layout, and stops
/// where the configuration says which files are specs (this adapter does not
/// evaluate the configuration, so it would list the wrong files).
const CYPRESS_LIST: &str = "for c in cypress.config.js cypress.config.cjs cypress.config.mjs cypress.config.ts cypress.json; do \
if [ -f \"$c\" ] && grep -Eq 'specPattern|excludeSpecPattern|integrationFolder|testFiles|ignoreTestFiles' \"$c\"; then \
echo \"$c chooses which files are specs; list them in a plan of your own\" >&2; exit 3; fi; done; \
find cypress/e2e -type f \\( -name '*.cy.js' -o -name '*.cy.jsx' -o -name '*.cy.ts' -o -name '*.cy.tsx' \\) | LC_ALL=C sort";

/// A line of the table Cypress prints when a run ends:
/// `✔  path  00:02  tests passing failing pending skipped`.
fn cypress_row(root: &str, line: &str) -> Option<TestResult> {
    let t: Vec<&str> = line
        .trim_matches(|c: char| c == '│' || c.is_whitespace())
        .split_whitespace()
        .collect();
    let [mark, path, time, counts @ ..] = t.as_slice() else {
        return None;
    };
    let clock = time.len() == 5
        && time
            .char_indices()
            .all(|(i, c)| if i == 2 { c == ':' } else { c.is_ascii_digit() });
    let n = |s: &str| {
        if s == "-" {
            Some(0)
        } else {
            s.parse::<u64>().ok()
        }
    };
    let counts: Option<Vec<u64>> = counts.iter().map(|c| n(c)).collect();
    let (true, true, Some([_, passing, failing, ..])) = (
        matches!(*mark, "✔" | "✖" | "-"),
        clock && counts.as_ref().is_some_and(|c| c.len() == 5),
        counts.as_deref(),
    ) else {
        return None;
    };
    let status = if *mark == "✖" || *failing > 0 {
        TestStatus::Failed
    } else if *passing > 0 {
        TestStatus::Passed
    } else {
        TestStatus::Skipped
    };
    Some(TestResult {
        identity: relative(root, path),
        status,
    })
}

/// Cypress, split by spec file.
///
/// Cypress has no command that lists its specs. The listing is the files of
/// the default layout (`cypress/e2e/**/*.cy.{js,jsx,ts,tsx}`); a configuration
/// that says which files are specs makes the listing fail rather than be
/// guessed. A shard runs its files with `--spec`, and what it reports is the
/// table Cypress prints at the end of a run, one row per spec file.
#[derive(Debug, Clone)]
pub struct Cypress {
    info: RunnerInfo,
    safety: Safety,
    root: String,
    command: Vec<String>,
    blocked: Vec<NotSplit>,
}

impl Cypress {
    /// `config` are the arguments of the job's own command that leave the specs
    /// alone. See [`Safety::e2e`] for `safety`.
    pub fn new(version: &str, root: &str, config: &[&str], safety: Safety) -> Self {
        Cypress {
            info: RunnerInfo {
                runner: "cypress".into(),
                version: version.into(),
                config: config.iter().map(|s| s.to_string()).collect(),
                filters: vec![],
            },
            safety,
            root: root.into(),
            command: argv(&["npx", "cypress", "run"]),
            blocked: vec![],
        }
    }

    /// The runner of a job's `run` text; `None` when it is not `cypress run`.
    pub fn from_command(version: &str, root: &str, run: &str, safety: Safety) -> Option<Self> {
        let p = parse_command(run).filter(|p| p.runner == "cypress")?;
        let args: Vec<&str> = p.args.iter().map(String::as_str).collect();
        Some(Cypress {
            blocked: p.blocked,
            ..Cypress::new(version, root, &args, safety)
        })
    }
}

impl RunnerAdapter for Cypress {
    fn info(&self) -> &RunnerInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        caps(false, self.safety, &self.blocked, false)
    }

    fn discover_command(&self) -> Invocation {
        invocation(argv(&["sh", "-c", CYPRESS_LIST]))
    }

    fn discover(&self, out: &CommandOutput) -> Result<Discovered, String> {
        if !out.success {
            return Err(failed_listing(out));
        }
        let mut tests: Vec<String> = out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        tests.sort();
        tests.dedup();
        if tests.is_empty() {
            return Err("the listing holds no spec".into());
        }
        if let Some((t, c)) = tests
            .iter()
            .find_map(|t| cypress_name_reason(t).map(|c| (t, c)))
        {
            return Err(format!(
                "`{t}` has a `{c}`, which `--spec` would read as more than the file"
            ));
        }
        Ok(Discovered {
            tests,
            complete: true,
        })
    }

    fn invoke(&self, shard: &Shard, _count: usize, mode: SplitMode) -> Invocation {
        let mut v = self.command.clone();
        v.extend(self.info.config.iter().cloned());
        if mode == SplitMode::Listed {
            v.push("--spec".into());
            v.push(shard.tests.join(","));
        }
        invocation(v)
    }

    fn collect(&self, job_id: &str, shard_id: &str, out: &CommandOutput) -> ShardOutcome {
        let text = without_ansi(&out.stdout);
        let tests: Vec<TestResult> = text
            .lines()
            .filter_map(|l| cypress_row(&self.root, l))
            .collect();
        ShardOutcome {
            job_id: job_id.into(),
            shard_id: shard_id.into(),
            parsed: !tests.is_empty(),
            tests,
            exit_ok: out.success,
        }
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
            [
                "cargo",
                "test",
                "--workspace",
                "--color",
                "never",
                "--",
                "--list"
            ]
        );
        // whatever the user's cargo is set to, the listing is read as it is
        assert_eq!(
            c.discover_command().env,
            [
                ("CARGO_TERM_QUIET".to_string(), "false".to_string()),
                ("CARGO_TERM_VERBOSE".to_string(), "false".to_string())
            ]
        );
        let listing = "b::two: test\na::one: test\n\n2 tests, 0 benchmarks\nbench_x: benchmark\n";
        let one = CommandOutput {
            stdout: listing.into(),
            stderr: "   Running unittests src/lib.rs (target/debug/deps/a-1)\n".into(),
            success: true,
        };
        let d = c.discover(&one).unwrap();
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
        assert_eq!(absolute("/p", "/q/a.js"), "/q/a.js");
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

    fn parsed(run: &str) -> Parsed {
        parse_command(run).unwrap()
    }

    #[test]
    fn a_command_is_read_into_what_it_keeps_and_why_it_cannot_be_split() {
        // not a runner
        for run in [
            "",
            "echo cargo test",
            "cargo build",
            "npm test",
            "'unclosed",
            "cargo",
        ] {
            assert_eq!(parse_command(run), None, "{run}");
        }
        let p = parsed("cargo test --workspace --features a,b -p x --no-fail-fast --release");
        assert_eq!(p.runner, "cargo-test");
        assert_eq!(
            p.args,
            ["--workspace", "--features", "a,b", "-p", "x", "--release"]
        );
        assert!(p.blocked.is_empty());
        let p = parsed("cargo test --features=a --manifest-path m/Cargo.toml");
        assert_eq!(p.args, ["--features=a", "--manifest-path", "m/Cargo.toml"]);
        assert!(p.blocked.is_empty());

        let filter = vec![NotSplit::JobFilter];
        let unread = vec![NotSplit::UnreadableArgs];
        for (run, why) in [
            // a name filter of cargo, before and after the `--`
            ("cargo test adds", &filter),
            ("cargo test -- adds", &filter),
            ("cargo test -- --skip slow", &filter),
            ("cargo test -- --skip=slow", &filter),
            ("cargo test -- --ignored", &filter),
            ("cargo test -- --include-ignored", &filter),
            ("cargo test -- --exact a", &filter),
            // arguments to the test binary that are not known to keep the tests
            ("cargo test -- --nocapture", &unread),
            ("cargo test --doc", &unread),
            ("cargo test --quiet", &unread),
            ("cargo test +nightly", &filter),
            ("cargo test -p", &unread),
            ("cargo test --workspace=1", &unread),
            // shell syntax
            ("cargo test && echo", &unread),
            ("cargo test | tee x", &unread),
            ("cargo test $ARGS", &unread),
            ("cargo test --features \"a\"; true", &unread),
            // Vitest
            ("npx vitest run src/a.test.js", &filter),
            ("vitest -t slow", &filter),
            ("vitest --testNamePattern=slow", &filter),
            ("vitest --exclude x", &filter),
            ("vitest --changed", &filter),
            ("vitest --shard=1/2", &filter),
            ("vitest --coverage", &vec![NotSplit::CoverageNotMerged]),
            ("vitest --config", &unread),
            // Jest
            ("npx jest src", &filter),
            ("jest -t slow", &filter),
            ("jest --testPathIgnorePatterns slow", &filter),
            ("jest --testPathIgnorePatterns=slow", &filter),
            ("jest --onlyChanged", &filter),
            ("jest -o", &filter),
            ("jest --coverage", &vec![NotSplit::CoverageNotMerged]),
        ] {
            assert_eq!(&parsed(run).blocked, why, "{run}");
        }
        // two reasons are both given, once each
        assert_eq!(
            parsed("jest a b --bogus --bogus2").blocked,
            [NotSplit::JobFilter, NotSplit::UnreadableArgs]
        );
        // what is kept
        let p = parsed("npx vitest run --config v.js --project a --silent");
        assert_eq!(p.runner, "vitest");
        assert_eq!(p.args, ["--config", "v.js", "--project", "a", "--silent"]);
        assert!(p.blocked.is_empty());
        let p = parsed("jest -c j.js --ci -i --runInBand --silent");
        assert_eq!(p.runner, "jest");
        assert_eq!(
            p.args,
            ["-c", "j.js", "--ci", "-i", "--runInBand", "--silent"]
        );
        assert!(p.blocked.is_empty());
        assert!(parsed("vitest").args.is_empty());
        assert!(parsed("npx vitest run").blocked.is_empty());
        // nothing after the `--` is nothing
        assert!(parsed("cargo test --workspace --").blocked.is_empty());
    }

    #[test]
    fn an_adapter_made_from_a_command_does_not_split_what_it_filters() {
        let s = Safety::default();
        let c = CargoTest::from_command("1", "cargo test --workspace", s).unwrap();
        assert_eq!(c.info().config, ["--workspace"]);
        assert!(!c.capabilities().job_filtered && !c.capabilities().unreadable_args);
        let c = CargoTest::from_command("1", "cargo test slow", s).unwrap();
        assert!(c.capabilities().job_filtered && !c.capabilities().unreadable_args);
        let c = CargoTest::from_command("1", "cargo test --bogus", s).unwrap();
        assert!(!c.capabilities().job_filtered && c.capabilities().unreadable_args);
        assert!(CargoTest::from_command("1", "npx jest", s).is_none());
        let v = Vitest::from_command("1", "/p", "vitest run -t x", s).unwrap();
        assert!(v.capabilities().job_filtered && v.capabilities().substring_targets);
        assert!(Vitest::from_command("1", "/p", "jest", s).is_none());
        let j = Jest::from_command("1", "/p", "jest --testPathIgnorePatterns x", s).unwrap();
        assert!(j.capabilities().job_filtered && !j.capabilities().substring_targets);
        assert!(Jest::from_command("1", "/p", "vitest", s).is_none());
        // the reasons come out of the partition
        let d = Discovered {
            tests: vec!["a".into(), "b".into()],
            complete: true,
        };
        let p = c.partition(
            &d,
            &super::super::History::default(),
            2,
            &Default::default(),
        );
        assert_eq!(p.not_split, vec![NotSplit::UnreadableArgs]);
    }

    #[test]
    fn the_adapter_of_a_plan_is_made_again_where_the_tests_are() {
        let info = |runner: &str, config: &[&str]| RunnerInfo {
            runner: runner.into(),
            version: "1".into(),
            config: config.iter().map(|s| s.to_string()).collect(),
            filters: vec![],
        };
        let s = shard(&["a"]);
        let c = from_info(&info("cargo-test", &["-p", "x"]), "/w").unwrap();
        assert_eq!(
            c.invoke(&s, 2, SplitMode::Listed).argv,
            [
                "cargo",
                "test",
                "-p",
                "x",
                "--no-fail-fast",
                "--",
                "--exact",
                "a"
            ]
        );
        let v = from_info(&info("vitest", &["--config=v.js"]), "/w/node").unwrap();
        assert_eq!(
            v.invoke(&s, 2, SplitMode::Listed).argv.last().unwrap(),
            "/w/node/a"
        );
        let j = from_info(&info("jest", &[]), "/w/node").unwrap();
        assert_eq!(
            j.invoke(&s, 2, SplitMode::Listed).argv.last().unwrap(),
            "/w/node/a"
        );
        assert!(from_info(&info("mocha", &[]), "/w").is_none());
    }

    #[test]
    fn a_command_is_a_script_with_each_word_quoted() {
        let inv = Invocation {
            argv: argv(&["cargo", "test", "--", "it's", "a b", "$x"]),
            env: vec![],
        };
        assert_eq!(
            script_of(&inv).unwrap(),
            "cargo test -- \"it's\" 'a b' '$x'"
        );
        let inv = Invocation {
            argv: argv(&["npx", "jest"]),
            env: vec![("A".into(), "1 2".into())],
        };
        assert_eq!(script_of(&inv).unwrap(), "env 'A=1 2' npx jest");
        // a word with a NUL cannot be quoted
        let bad = Invocation {
            argv: vec!["a\0b".into()],
            env: vec![],
        };
        assert_eq!(script_of(&bad), None);
        let bad_env = Invocation {
            argv: argv(&["x"]),
            env: vec![("A".into(), "\0".into())],
        };
        assert_eq!(script_of(&bad_env), None);
    }

    #[test]
    fn a_target_with_a_harness_of_its_own_makes_the_listing_incomplete() {
        let c = CargoTest::new("1", &[], Safety::default());
        let listing = |stderr: &str, stdout: &str| {
            c.discover(&CommandOutput {
                stdout: stdout.into(),
                stderr: stderr.into(),
                success: true,
            })
            .unwrap()
        };
        let two = "     Running unittests src/lib.rs (target/debug/deps/a-1)\n     Running tests/b.rs (target/debug/deps/b-2)\n";
        let ok = "a: test\n\n1 test, 0 benchmarks\nb: test\n\n1 tests, 0 benchmarks\n";
        assert!(listing(two, ok).complete);
        // the second target printed no summary: it is not libtest
        let custom = "a: test\n\n1 test, 0 benchmarks\nwhatever it prints\n";
        let d = listing(two, custom);
        assert!(!d.complete);
        assert_eq!(d.tests, ["a"]);
        // a target that prints nothing has no summary either
        assert!(!listing(two, "a: test\n\n1 test, 0 benchmarks\n").complete);
        // a summary more than there are targets: something else printed one
        let more = format!("{ok}\n1 test, 0 benchmarks\n");
        assert!(!listing(two, &more).complete);
        // text of its own among the tests is not libtest's either
        assert!(!listing(two, &format!("noise\n{ok}")).complete);
        // doc-tests are a target of their own, with a summary of their own
        let doc = format!("{two}   Doc-tests a\n");
        assert!(listing(&doc, &format!("{ok}\n0 tests, 0 benchmarks\n")).complete);
        assert!(!listing(&doc, ok).complete);
        // lines that only look like a summary are not one
        for line in [
            "1 test, 0",
            "x tests, 0 benchmarks",
            "1 cases, 0 benchmarks",
            "1 tests, y benchmarks",
            "1 tests, 0 numbers",
        ] {
            assert!(!is_list_summary(line), "{line}");
        }
        assert!(is_list_summary("12 tests, 3 benchmarks"));
    }

    #[test]
    fn colour_and_a_quiet_cargo_do_not_make_a_listing_look_complete_or_empty() {
        let c = CargoTest::new("1", &[], Safety::default());
        let listing = |stderr: &str, stdout: &str| {
            c.discover(&CommandOutput {
                stdout: stdout.into(),
                stderr: stderr.into(),
                success: true,
            })
            .unwrap()
        };
        let ok = "a: test\n\n1 test, 0 benchmarks\n";
        // `CARGO_TERM_COLOR=always`: the target lines come with escape codes
        let coloured = "\u{1b}[1m\u{1b}[92m     Running\u{1b}[0m unittests src/lib.rs (target/debug/deps/a-1)\n";
        let d = listing(coloured, ok);
        assert!(d.complete, "{d:?}");
        assert_eq!(d.tests, ["a"]);
        // a colour code on a test line does not hide the test
        let d = listing(coloured, "\u{1b}[0ma: test\n\n1 test, 0 benchmarks\n");
        assert_eq!(d.tests, ["a"]);
        // a bare escape that opens no sequence is kept
        assert_eq!(without_ansi("a\u{1b}b"), "a\u{1b}b");
        assert_eq!(without_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        // `CARGO_TERM_QUIET`: no target is shown, so nothing says the listing is
        // the whole one
        let d = listing("", ok);
        assert!(!d.complete);
        assert_eq!(d.tests, ["a"]);
    }

    #[test]
    fn a_cargo_run_without_a_result_line_is_not_a_report() {
        let c = CargoTest::new("1", &[], Safety::default());
        let o = c.collect("j", "s", &out("error: could not compile\n", false));
        assert!(!o.parsed && o.tests.is_empty());
        let o = c.collect(
            "j",
            "s",
            &out(
                "running 0 tests\n\ntest result: ok. 0 passed; 0 failed\n",
                true,
            ),
        );
        assert!(o.parsed && o.tests.is_empty());
    }

    #[test]
    fn jest_keeps_the_arguments_it_is_given() {
        let j = Jest::new("29", "/p", &["--config=j.js"], Safety::default());
        assert_eq!(j.info().config, ["--config=j.js"]);
        assert_eq!(
            j.discover_command().argv,
            ["npx", "jest", "--listTests", "--config=j.js"]
        );
        assert_eq!(
            j.invoke(&shard(&["a.js"]), 1, SplitMode::Single).argv,
            ["npx", "jest", "--json", "--config=j.js"]
        );
    }

    #[test]
    fn a_report_is_found_among_other_output() {
        let v = Vitest::new("1", "/p", &[], Safety::default());
        let report = r#"{"testResults":[{"name":"/p/a.test.js","status":"passed"}]}"#;
        // a line of the tests' own output that opens an object, then the report, then more
        let text = format!("{{\"not\": \"a report\"}}\n{{ broken\n{report}\ntrailing line\n");
        let o = v.collect("j", "s", &out(&text, true));
        assert!(o.parsed);
        assert_eq!(o.tests.len(), 1);
        // an object without results is not the report
        let o = v.collect("j", "s", &out("{\"testResults\": 1}\n", true));
        assert!(!o.parsed && o.tests.is_empty());
        // a file that failed is failed, whatever its tests say
        assert_eq!(file_status("failed", &["passed"]), TestStatus::Failed);
        assert_eq!(
            file_status("passed", &["passed", "failed"]),
            TestStatus::Failed
        );
        assert_eq!(file_status("passed", &[]), TestStatus::Passed);
    }

    #[test]
    fn paths_are_compared_as_the_system_spells_them() {
        let tmp = std::env::temp_dir().join(format!("l2-paths-{}", std::process::id()));
        let real = tmp.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = tmp.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (link, real_s) = (
            link.to_str().unwrap().to_string(),
            std::fs::canonicalize(&real)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string(),
        );
        // a reporter prints the real path of a file under a root given as a link
        assert_eq!(relative(&link, &format!("{real_s}/a/b.js")), "a/b.js");
        assert_eq!(relative(&link, &format!("{link}/a.js")), "a.js");
        // a command is given the real one
        assert_eq!(absolute(&link, "a/b.js"), format!("{real_s}/a/b.js"));
        // an absolute file is used as it is
        assert_eq!(absolute(&link, "/elsewhere/x.js"), "/elsewhere/x.js");
        assert_eq!(relative(&link, "/elsewhere/x.js"), "/elsewhere/x.js");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    // ---- Playwright and Cypress ----

    /// A Playwright JSON report: `(file, [(group, [(title, project, status, expected, ran)])])`
    /// shaped like the real one (a file suite, groups under it, specs with one test per project).
    fn pw_report(files: serde_json::Value, config: serde_json::Value) -> String {
        serde_json::json!({ "config": config, "suites": files, "errors": [] }).to_string()
    }

    fn pw_config() -> serde_json::Value {
        serde_json::json!({
            "webServer": null,
            "globalSetup": null,
            "globalTeardown": null,
            "shard": null,
            "projects": [{"name": "desktop", "repeatEach": 1}, {"name": "mobile", "repeatEach": 1}],
        })
    }

    fn pw_spec(title: &str, projects: &[(&str, &str, &str, bool)]) -> serde_json::Value {
        let tests: Vec<serde_json::Value> = projects
            .iter()
            .map(|(p, status, expected, ran)| {
                serde_json::json!({
                    "projectName": p,
                    "status": status,
                    "expectedStatus": expected,
                    "results": if *ran { vec![serde_json::json!({})] } else { vec![] },
                })
            })
            .collect();
        serde_json::json!({ "title": title, "tests": tests })
    }

    fn pw_listing() -> String {
        pw_report(
            serde_json::json!([
                {
                    "title": "a.spec.js", "file": "a.spec.js",
                    "specs": [pw_spec("one", &[("desktop", "skipped", "passed", false), ("mobile", "skipped", "passed", false)])],
                    "suites": [{
                        "title": "group", "file": "a.spec.js",
                        "specs": [pw_spec("two", &[("desktop", "skipped", "passed", false)])],
                        "suites": [],
                    }],
                },
                { "title": "b.spec.js", "file": "b.spec.js", "specs": [pw_spec("three", &[("desktop", "skipped", "passed", false)])] },
            ]),
            pw_config(),
        )
    }

    #[test]
    fn playwright_lists_a_test_per_project_and_names_it_as_a_test_list_does() {
        let a = Playwright::new("1.63.0", &["--config", "pw.js"], Safety::default());
        let inv = a.discover_command();
        assert_eq!(
            inv.argv,
            [
                "npx",
                "playwright",
                "test",
                "--list",
                "--reporter=json",
                "--config",
                "pw.js"
            ]
        );
        let d = a.discover(&out(&pw_listing(), true)).unwrap();
        assert!(d.complete);
        assert_eq!(
            d.tests,
            [
                "[desktop] › a.spec.js › group › two",
                "[desktop] › a.spec.js › one",
                "[desktop] › b.spec.js › three",
                "[mobile] › a.spec.js › one",
            ]
        );
        // other output around the report does not hide it
        let noisy = format!("starting\n{}\ndone\n", pw_listing());
        assert_eq!(a.discover(&out(&noisy, true)).unwrap(), d);
        assert!(a
            .discover(&out("", false))
            .unwrap_err()
            .contains("the listing failed: boom"));
        assert!(a
            .discover(&out("no json", true))
            .unwrap_err()
            .contains("no JSON report"));
        // a report that is not Playwright's holds no test
        assert!(a
            .discover(&out(r#"{"suites": []}"#, true))
            .unwrap_err()
            .contains("holds no test"));
    }

    #[test]
    fn a_playwright_listing_that_shows_what_shards_would_share_is_not_split() {
        let a = Playwright::new("1", &[], Safety::default());
        let files = serde_json::json!([{
            "title": "a.spec.js", "file": "a.spec.js",
            "specs": [pw_spec("one", &[("desktop", "skipped", "passed", false)])],
        }]);
        let with = |k: &str, v: serde_json::Value| {
            let mut c = pw_config();
            c[k] = v;
            pw_report(files.clone(), c)
        };
        for (listing, why) in [
            (
                with("webServer", serde_json::json!({"command": "x", "port": 3000})),
                "webServer",
            ),
            (with("webServer", serde_json::json!([{"port": 1}])), "webServer"),
            (with("globalSetup", serde_json::json!("setup.js")), "global setup"),
            (with("globalTeardown", serde_json::json!("t.js")), "global setup"),
            (with("shard", serde_json::json!({"current": 1, "total": 2})), "shards by itself"),
            (
                with("projects", serde_json::json!([{"name": "p", "repeatEach": 3}])),
                "repeats",
            ),
            (
                serde_json::json!({"config": pw_config(), "suites": files, "errors": [{"message": "x"}]})
                    .to_string(),
                "has errors",
            ),
        ] {
            let e = a.discover(&out(&listing, true)).unwrap_err();
            assert!(e.contains(why), "{why}: {e}");
        }
        // empty lists and a missing config are not a reason
        let ok = with("webServer", serde_json::json!([]));
        assert!(a.discover(&out(&ok, true)).is_ok());
        let bare = serde_json::json!({"suites": files}).to_string();
        assert!(a.discover(&out(&bare, true)).is_ok());
    }

    #[test]
    fn playwright_tests_that_a_test_list_cannot_tell_apart_are_not_listed() {
        let a = Playwright::new("1", &[], Safety::default());
        let listing = |specs: Vec<serde_json::Value>, group: &str| {
            pw_report(
                serde_json::json!([{
                    "title": "a.spec.js", "file": "a.spec.js",
                    "specs": specs,
                    "suites": [{ "title": group, "file": "a.spec.js",
                        "specs": [pw_spec("x", &[("desktop", "skipped", "passed", false)])] }],
                }]),
                pw_config(),
            )
        };
        let one = |t: &str| pw_spec(t, &[("desktop", "skipped", "passed", false)]);
        for (specs, group, why) in [
            (vec![one("a › b")], "g", "cannot be named"),
            (vec![one("a\nb")], "g", "cannot be named"),
            (vec![one(" padded")], "g", "cannot be named"),
            (vec![one("")], "g", "cannot be named"),
            (vec![one("one"), one("one")], "g", "is listed twice"),
            // a test named like the group that holds `x`
            (vec![one("g")], "g", "name of a test and of a group"),
        ] {
            let e = a.discover(&out(&listing(specs, group), true)).unwrap_err();
            assert!(e.contains(why), "{why}: {e}");
        }
        // a test and a group that merely start alike are two names
        assert!(a
            .discover(&out(&listing(vec![one("g2")], "g"), true))
            .is_ok());
        // a run with no project has an empty project name, which is a name
        let none = pw_report(
            serde_json::json!([{
                "title": "a.spec.js", "file": "a.spec.js",
                "specs": [pw_spec("one", &[("", "skipped", "passed", false)])],
            }]),
            pw_config(),
        );
        assert_eq!(
            a.discover(&out(&none, true)).unwrap().tests,
            ["[] › a.spec.js › one"]
        );
    }

    #[test]
    fn a_playwright_shard_names_its_tests_in_a_test_list_file() {
        let a = Playwright::new(
            "1",
            &["--config", "my pw.js", "--project", "it's"],
            Safety::default(),
        );
        let s = Shard {
            index: 1,
            tests: vec![
                "[desktop] › a.spec.js › one".into(),
                "[mobile] › b.spec.js › it's".into(),
            ],
            load: 0,
        };
        let inv = a.invoke(&s, 2, SplitMode::Listed);
        assert_eq!(&inv.argv[..2], ["sh", "-c"]);
        assert_eq!(inv.argv[3], "sh");
        assert_eq!(&inv.argv[4..], s.tests.as_slice());
        let script = &inv.argv[2];
        assert!(script.contains(
            "'npx' 'playwright' 'test' '--reporter=json' '--config' 'my pw.js' '--project' 'it'\\''s' --test-list \"$list\""
        ), "{script}");
        assert!(script.contains("mktemp") && script.contains("rm -f \"$list\"; exit $status"));
        // the script is a command a shell can be given
        assert!(script_of(&inv).is_some());
        // a whole run, and the runner's own shard feature
        assert_eq!(
            a.invoke(&s, 2, SplitMode::Single).argv,
            [
                "npx",
                "playwright",
                "test",
                "--reporter=json",
                "--config",
                "my pw.js",
                "--project",
                "it's"
            ]
        );
        assert_eq!(
            a.invoke(&s, 3, SplitMode::Native).argv.last().unwrap(),
            "--shard=2/3"
        );
    }

    #[test]
    fn a_playwright_report_gives_one_result_per_test_and_project() {
        let a = Playwright::new("1", &[], Safety::default());
        let report = pw_report(
            serde_json::json!([{
                "title": "a.spec.js", "file": "a.spec.js",
                "specs": [
                    pw_spec("ok", &[("desktop", "expected", "passed", true)]),
                    pw_spec("flaky", &[("desktop", "flaky", "passed", true)]),
                    pw_spec("bad", &[("desktop", "unexpected", "passed", true), ("mobile", "expected", "passed", true)]),
                    pw_spec("skipped", &[("desktop", "skipped", "skipped", true)]),
                    pw_spec("fixme", &[("desktop", "skipped", "skipped", false)]),
                    pw_spec("cut short", &[("desktop", "skipped", "passed", false)]),
                    pw_spec("unknown", &[("desktop", "interrupted", "passed", true)]),
                ],
            }]),
            pw_config(),
        );
        let o = a.collect("e2e", "e2e@0", &out(&report, false));
        assert!(o.parsed && !o.exit_ok);
        assert_eq!((o.job_id.as_str(), o.shard_id.as_str()), ("e2e", "e2e@0"));
        let got: Vec<(String, TestStatus)> = o
            .tests
            .iter()
            .map(|t| (t.identity.clone(), t.status))
            .collect();
        use TestStatus::*;
        assert_eq!(
            got,
            [
                ("[desktop] › a.spec.js › ok".to_string(), Passed),
                ("[desktop] › a.spec.js › flaky".to_string(), Passed),
                ("[desktop] › a.spec.js › bad".to_string(), Failed),
                ("[mobile] › a.spec.js › bad".to_string(), Passed),
                ("[desktop] › a.spec.js › skipped".to_string(), Skipped),
                ("[desktop] › a.spec.js › fixme".to_string(), Skipped),
                ("[desktop] › a.spec.js › cut short".to_string(), Failed),
                ("[desktop] › a.spec.js › unknown".to_string(), Failed),
            ]
        );
        let none = a.collect("e2e", "e2e@0", &out("crashed", false));
        assert!(!none.parsed && none.tests.is_empty());
        // a report with no suites list, and a suite with no fields, hold nothing
        let o = a.collect("e2e", "e2e@0", &out(r#"{"suites": [{}]}"#, true));
        assert!(o.parsed && o.tests.is_empty());
    }

    #[test]
    fn playwright_and_cypress_commands_are_read() {
        let p = parsed(
            "npx playwright test --config pw.js --project=chromium -j 2 --retries 1 --headed",
        );
        assert_eq!(p.runner, "playwright");
        assert_eq!(
            p.args,
            [
                "--config",
                "pw.js",
                "--project=chromium",
                "-j",
                "2",
                "--retries",
                "1",
                "--headed"
            ]
        );
        assert!(p.blocked.is_empty());
        assert_eq!(parsed("playwright test").runner, "playwright");
        let filter = vec![NotSplit::JobFilter];
        let unread = vec![NotSplit::UnreadableArgs];
        for (run, why) in [
            ("npx playwright test tests/a.spec.js", &filter),
            ("playwright test --grep slow", &filter),
            ("playwright test -g slow", &filter),
            ("playwright test -g=slow", &unread),
            ("playwright test --shard=1/2", &filter),
            ("playwright test --last-failed", &filter),
            ("playwright test --only-changed", &filter),
            ("playwright test --test-list list.txt", &filter),
            ("playwright test --reporter=html", &unread),
            ("playwright test --ui", &unread),
            ("playwright test --update-snapshots", &unread),
            ("playwright test --workers", &unread),
        ] {
            assert_eq!(parsed(run).blocked, *why, "{run}");
        }
        let p = parsed(
            "npx cypress run --browser chrome --headless --env a=b --config baseUrl=http://x",
        );
        assert_eq!(p.runner, "cypress");
        assert!(p.blocked.is_empty(), "{p:?}");
        assert_eq!(parsed("cypress run --e2e").runner, "cypress");
        for (run, why) in [
            ("cypress run --spec cypress/e2e/a.cy.js", &filter),
            ("npx cypress run -s a", &filter),
            ("cypress run --tag smoke", &filter),
            ("cypress run cypress/e2e/a.cy.js", &filter),
            ("cypress run --config specPattern=x/**", &filter),
            ("cypress run --config=excludeSpecPattern=x", &filter),
            ("cypress run -c integrationFolder=x", &filter),
            ("cypress run --component", &unread),
            ("cypress run --record", &unread),
            ("cypress run --quiet", &unread),
        ] {
            assert_eq!(parsed(run).blocked, *why, "{run}");
        }
        assert_eq!(parse_command("npx playwright install"), None);
        assert_eq!(parse_command("cypress open"), None);
    }

    #[test]
    fn a_job_that_collects_coverage_is_not_split() {
        let cov = vec![NotSplit::CoverageNotMerged];
        for run in [
            "npx vitest run --coverage",
            "vitest run --coverage.enabled --coverage.reporter lcov",
            "vitest run --coverage.enabled=true",
            "jest --coverage",
            "jest --coverage --coverageDirectory out",
            "jest --coverageDirectory=out",
            "jest --collectCoverage",
            "jest --collectCoverageFrom=src/**",
        ] {
            assert_eq!(parsed(run).blocked, cov, "{run}");
        }
        // the value of a coverage option is not a filter, a flag after it is read as usual
        assert_eq!(
            parsed("jest --coverageDirectory out -t x").blocked,
            [NotSplit::CoverageNotMerged, NotSplit::JobFilter]
        );
        // a flag without a value keeps the next flag as it is
        assert_eq!(parsed("jest --coverage --ci").blocked, cov);
        assert_eq!(parsed("jest --coverage").args, Vec::<String>::new());
    }

    fn cypress_table(rows: &[&str]) -> String {
        let mut t = String::from(
            "\n  (Run Finished)\n\n\n       Spec                                              Tests  Passing  Failing  Pending  Skipped  \n  ┌────────────────────────────────────────────────────────────────────────────────────────────────┐\n",
        );
        for r in rows {
            t.push_str(&format!("  │ {r} │\n"));
        }
        t.push_str("  └────────────────────────────────────────────────────────────────────────────────────────────────┘\n    ✖  1 of 3 failed (33%)                     00:06        5        4        1        -        -  \n");
        t
    }

    #[test]
    fn cypress_lists_the_files_of_its_default_layout_and_reads_the_summary_table() {
        let a = Cypress::new(
            "13.0.0",
            "/w/app",
            &["--browser", "chrome"],
            Safety::default(),
        );
        let inv = a.discover_command();
        assert_eq!(&inv.argv[..2], ["sh", "-c"]);
        assert!(inv.argv[2].contains("cypress/e2e") && inv.argv[2].contains("exit 3"));
        let d = a
            .discover(&out(
                "cypress/e2e/b.cy.js\ncypress/e2e/a.cy.ts\n\ncypress/e2e/a.cy.ts\n",
                true,
            ))
            .unwrap();
        assert_eq!(d.tests, ["cypress/e2e/a.cy.ts", "cypress/e2e/b.cy.js"]);
        assert!(d.complete);
        assert!(a
            .discover(&out("", false))
            .unwrap_err()
            .contains("the listing failed: boom"));
        assert!(a
            .discover(&out("\n", true))
            .unwrap_err()
            .contains("no spec"));
        for name in [
            "cypress/e2e/a b.cy.js",
            "cypress/e2e/a,b.cy.js",
            "cypress/e2e/a*.cy.js",
            "cypress/e2e/(a).cy.js",
        ] {
            let e = a.discover(&out(name, true)).unwrap_err();
            assert!(e.contains("`--spec` would read"), "{name}: {e}");
        }

        let s = shard(&["cypress/e2e/a.cy.ts", "cypress/e2e/b.cy.js"]);
        assert_eq!(
            a.invoke(&s, 2, SplitMode::Listed).argv,
            [
                "npx",
                "cypress",
                "run",
                "--browser",
                "chrome",
                "--spec",
                "cypress/e2e/a.cy.ts,cypress/e2e/b.cy.js"
            ]
        );
        assert_eq!(
            a.invoke(&s, 2, SplitMode::Single).argv,
            ["npx", "cypress", "run", "--browser", "chrome"]
        );

        let table = cypress_table(&[
            "✔  cypress/e2e/a.cy.ts                      00:01        2        2        -        -        -",
            "✖  /w/app/cypress/e2e/b.cy.js               00:04        2        1        1        -        -",
            "-  cypress/e2e/c.cy.js                      00:00        1        -        -        1        -",
            "✔  cypress/e2e/d.cy.js                      00:00        -        -        -        -        -",
        ]);
        let colour = table.replace("(Run Finished)", "\u{1b}[90m(Run Finished)\u{1b}[0m");
        let o = a.collect("e2e", "e2e@0", &out(&colour, false));
        use TestStatus::*;
        assert!(o.parsed && !o.exit_ok);
        let got: Vec<(&str, TestStatus)> = o
            .tests
            .iter()
            .map(|t| (t.identity.as_str(), t.status))
            .collect();
        assert_eq!(
            got,
            [
                ("cypress/e2e/a.cy.ts", Passed),
                ("cypress/e2e/b.cy.js", Failed),
                ("cypress/e2e/c.cy.js", Skipped),
                ("cypress/e2e/d.cy.js", Skipped),
            ]
        );
        // no table, no report
        let none = a.collect("e2e", "e2e@0", &out("Cypress could not start", false));
        assert!(!none.parsed && none.tests.is_empty());
    }

    #[test]
    fn only_a_row_of_the_table_is_a_spec() {
        let row = |l: &str| cypress_row("", l);
        let good = "│ ✔  cypress/e2e/a.cy.js   00:01   2   2   -   -   - │";
        assert_eq!(row(good).unwrap().identity, "cypress/e2e/a.cy.js");
        for l in [
            "",
            "Spec  Tests  Passing  Failing  Pending  Skipped",
            "│ ✔  All specs passed!   00:06   5   5   -   -   - │",
            "│ ✓  cypress/e2e/a.cy.js   00:01   2   2   -   -   - │",
            "│ ✔  cypress/e2e/a.cy.js   0:01   2   2   -   -   - │",
            "│ ✔  cypress/e2e/a.cy.js   00;01   2   2   -   -   - │",
            "│ ✔  cypress/e2e/a.cy.js   00:01   2   2   -   - │",
            "│ ✔  cypress/e2e/a.cy.js   00:01   2   x   -   -   - │",
            "✔  1 of 1 failed (33%)   00:06   5   4   1   -   -",
        ] {
            assert_eq!(row(l), None, "{l}");
        }
    }

    #[test]
    fn end_to_end_adapters_are_not_split_until_the_caller_says_each_shard_has_its_own() {
        use crate::shard::{partition_tests, History, PartitionOpts};
        let d = Discovered {
            tests: vec![
                "[p] › a.spec.js › one".into(),
                "[p] › b.spec.js › one".into(),
            ],
            complete: true,
        };
        let split = |safety: Safety| {
            let a = Playwright::new("1", &[], safety);
            partition_tests(&a, &d, &History::default(), 2, &PartitionOpts::default())
        };
        assert_eq!(split(Safety::e2e()).not_split, [NotSplit::NoIsolation]);
        let p = split(Safety::default());
        assert_eq!(p.mode, SplitMode::Listed);
        assert_eq!(p.shards.len(), 2);
        let files = Discovered {
            tests: vec!["cypress/e2e/a.cy.js".into(), "cypress/e2e/b.cy.js".into()],
            complete: true,
        };
        let c = Cypress::new("13", "/w", &[], Safety::e2e());
        let p = partition_tests(
            &c,
            &files,
            &History::default(),
            2,
            &PartitionOpts::default(),
        );
        assert_eq!(p.not_split, [NotSplit::NoIsolation]);
        // the job's own filters and coverage show through the adapters
        let p =
            Playwright::from_command("1", "playwright test --grep x", Safety::default()).unwrap();
        assert!(p.capabilities().job_filtered);
        assert!(Playwright::from_command("1", "cypress run", Safety::default()).is_none());
        let c =
            Cypress::from_command("1", "/w", "cypress run --spec a", Safety::default()).unwrap();
        assert!(c.capabilities().job_filtered);
        assert!(Cypress::from_command("1", "/w", "playwright test", Safety::default()).is_none());
    }

    #[test]
    fn the_playwright_and_cypress_adapters_of_a_plan_are_made_again() {
        let info = |runner: &str| RunnerInfo {
            runner: runner.into(),
            version: "1".into(),
            config: vec!["--config".into(), "c.js".into()],
            filters: vec![],
        };
        let s = shard(&["[p] › a.spec.js › one"]);
        let p = from_info(&info("playwright"), "/w").unwrap();
        assert!(p.capabilities().targets && !p.capabilities().native_shard);
        assert!(p.invoke(&s, 2, SplitMode::Listed).argv[2].contains("'--config' 'c.js'"));
        let c = from_info(&info("cypress"), "/w").unwrap();
        assert_eq!(
            c.invoke(&shard(&["cypress/e2e/a.cy.js"]), 2, SplitMode::Listed)
                .argv,
            [
                "npx",
                "cypress",
                "run",
                "--config",
                "c.js",
                "--spec",
                "cypress/e2e/a.cy.js"
            ]
        );
        let o = c.collect(
            "j",
            "j@0",
            &out(
                "│ ✔  /w/cypress/e2e/a.cy.js   00:01   1   1   -   -   - │",
                true,
            ),
        );
        assert_eq!(o.tests[0].identity, "cypress/e2e/a.cy.js");
    }
}
