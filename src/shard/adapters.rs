//! Runner adapters for [`crate::shard`]: `cargo test`, Vitest and Jest.
//!
//! An adapter builds the command that lists the tests or runs a shard and reads
//! what the command printed. It starts nothing; [`run_invocation`] does that
//! for a caller that has an [`ExecContext`].
//!
//! Test identities: `cargo test` lists tests by libtest name (a name that two
//! test targets share makes the listing incomplete, since a name filter cannot
//! tell them apart, and so does a doctest, which cannot be run by name);
//! Vitest and Jest split by file, and a file is named relative to the project
//! root.

use super::{
    Capabilities, CommandOutput, Discovered, Invocation, RunnerAdapter, RunnerInfo, Shard,
    ShardOutcome, SplitMode, TestResult, TestStatus,
};
use crate::context::ExecContext;
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

fn caps(native_shard: bool, safety: Safety) -> Capabilities {
    Capabilities {
        enumerates: true,
        targets: true,
        native_shard,
        shared_state: safety.shared_state,
        isolated_fixtures: safety.isolated_fixtures,
        aggregates_exactly: true,
    }
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
}

impl CargoTest {
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
        }
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

impl RunnerAdapter for CargoTest {
    fn info(&self) -> &RunnerInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        caps(false, self.safety)
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
        }
    }
}

// ---- Vitest and Jest ----

/// A file of a JS project, relative to `root` when it is under it.
fn relative(root: &str, path: &str) -> String {
    let root = root.trim_end_matches('/');
    path.strip_prefix(root)
        .and_then(|p| p.strip_prefix('/'))
        .unwrap_or(path)
        .to_string()
}

fn absolute(root: &str, file: &str) -> String {
    format!("{}/{file}", root.trim_end_matches('/'))
}

/// The JSON a reporter printed: the whole of stdout, or what starts at the
/// first line that opens an object.
fn json_of(stdout: &str) -> Option<Value> {
    serde_json::from_str(stdout.trim()).ok().or_else(|| {
        let at = stdout.find("\n{")? + 1;
        serde_json::from_str(stdout[at..].trim()).ok()
    })
}

/// How a test file went, from its own status and that of its tests.
fn file_status(file: &str, tests: &[&str]) -> TestStatus {
    if tests.contains(&"failed") || (tests.is_empty() && file == "failed") {
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
    let report = json_of(&out.stdout).unwrap_or_default();
    let tests = report
        .get("testResults")
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
        }
    }
}

impl RunnerAdapter for Vitest {
    fn info(&self) -> &RunnerInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        caps(true, self.safety)
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
        }
    }
}

impl RunnerAdapter for Jest {
    fn info(&self) -> &RunnerInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        caps(true, self.safety)
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
