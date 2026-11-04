//! The result of a run: its state, the short text report, and the JSON file.

use crate::exec::{tail, JobResult, Outcome, StepResult, UnitResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub workflow: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct RunResult {
    pub version: String,
    pub sha: String,
    pub base: String,
    pub changed_files: usize,
    /// `changes` (what the changes trigger) or `all` (paths filters ignored).
    pub mode: String,
    pub state: String,
    pub description: String,
    pub secs: f64,
    pub run_dir: PathBuf,
    pub setup: Vec<StepResult>,
    pub units: Vec<UnitResult>,
    pub skipped_workflows: Vec<Skipped>,
    pub not_covered: Vec<String>,
    pub warnings: Vec<String>,
}

/// What `local-ci status` needs back from a saved result.
#[derive(Debug, Deserialize)]
pub struct Saved {
    pub sha: String,
    pub state: String,
    pub description: String,
}

static DEFAULT_WORKFLOWS_DIR: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// The directory `short` leaves off: the first `workflows-dirs` entry of
/// `.local-ci.toml`. Set by `resolve`, before anything is printed.
pub fn set_default_workflows_dir(d: &str) {
    *DEFAULT_WORKFLOWS_DIR
        .write()
        .unwrap_or_else(|e| e.into_inner()) = Some(d.to_string());
}

/// How a workflow is named in output: the file name for the default directory
/// (`.github/workflows/`, or the first one `.local-ci.toml` names), the path
/// for any other, so equal file names stay apart.
pub fn short(f: &str) -> &str {
    let dir = DEFAULT_WORKFLOWS_DIR
        .read()
        .unwrap_or_else(|e| e.into_inner());
    short_in(
        dir.as_deref()
            .unwrap_or(crate::config::DEFAULT_WORKFLOWS_DIR),
        f,
    )
}

fn short_in<'a>(dir: &str, f: &'a str) -> &'a str {
    f.strip_prefix(dir)
        .and_then(|r| r.strip_prefix('/'))
        .unwrap_or(f)
}

pub fn tilde(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

fn dur(secs: f64) -> String {
    let s = secs.round() as u64;
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

pub struct Counts {
    pub pass: usize,
    pub fail: usize,
    pub error: usize,
    pub skip: usize,
}

pub fn counts(units: &[UnitResult]) -> Counts {
    let mut c = Counts {
        pass: 0,
        fail: 0,
        error: 0,
        skip: 0,
    };
    for j in units.iter().flat_map(|u| &u.jobs) {
        match j.outcome {
            Outcome::Pass => c.pass += 1,
            Outcome::Fail => c.fail += 1,
            Outcome::Error(_) => c.error += 1,
            Outcome::Skip(_) => c.skip += 1,
        }
    }
    c
}

/// success / failure / error, the same table the commit status uses.
pub fn state(setup: &[StepResult], units: &[UnitResult]) -> &'static str {
    let c = counts(units);
    if setup.iter().any(|s| s.outcome == Outcome::Fail) || c.fail > 0 {
        "failure"
    } else if setup.iter().any(|s| matches!(s.outcome, Outcome::Error(_))) || c.error > 0 {
        "error"
    } else {
        "success"
    }
}

/// Steps and jobs that were skipped because they cannot be reproduced here.
pub fn not_covered(units: &[UnitResult]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for j in units.iter().flat_map(|u| &u.jobs) {
        if let Outcome::Skip(r) = &j.outcome {
            if r.contains("not supported") {
                out.push(format!("{} › {}: {r}", short(&j.workflow), j.display));
            }
        }
        for s in &j.steps {
            if let Outcome::Skip(r) = &s.outcome {
                if r == "uses secrets"
                    || r.starts_with("action ")
                    || r == "installs system packages"
                    || r == "pushes, publishes or deploys"
                {
                    out.push(format!(
                        "{} › {} › {}: {r}",
                        short(&j.workflow),
                        j.display,
                        s.label
                    ));
                }
            }
        }
    }
    out
}

pub fn warnings(units: &[UnitResult]) -> Vec<String> {
    let mut out: Vec<String> = units
        .iter()
        .flat_map(|u| &u.jobs)
        .flat_map(|j| &j.steps)
        .filter_map(|s| {
            s.note
                .as_ref()
                .filter(|n| n.starts_with("wants "))
                .map(|n| format!("{}: {n}", s.label))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn description(
    state: &str,
    units: &[UnitResult],
    skipped_only: bool,
    not_covered: usize,
) -> String {
    if skipped_only {
        return "no workflow triggered by these changes".into();
    }
    let c = counts(units);
    let mut wfs: Vec<&str> = units.iter().map(|u| short(&u.workflow)).collect();
    wfs.dedup();
    let mut d = format!(
        "{state}: pass {} / fail {} / error {} / skip {}",
        c.pass, c.fail, c.error, c.skip
    );
    if not_covered > 0 {
        d.push_str(&format!(", {not_covered} not covered"));
    }
    let list = wfs.join(",");
    if d.len() + list.len() + 3 <= 140 {
        d = format!("{list}: {d}");
    }
    d.chars().take(140).collect()
}

fn failed_step(j: &JobResult) -> Option<&StepResult> {
    j.steps
        .iter()
        .find(|s| s.outcome == Outcome::Fail && s.note.is_none())
        .or_else(|| {
            j.steps
                .iter()
                .find(|s| matches!(s.outcome, Outcome::Error(_)))
        })
}

/// "paths not matched: a.yml, b.yml; only schedule: c.yml"
pub fn group_skipped(skipped: &[Skipped]) -> String {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for sk in skipped {
        match groups.iter_mut().find(|(reason, _)| *reason == sk.reason) {
            Some((_, files)) => files.push(short(&sk.workflow)),
            None => groups.push((&sk.reason, vec![short(&sk.workflow)])),
        }
    }
    groups
        .iter()
        .map(|(reason, files)| format!("{reason}: {}", files.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn print(r: &RunResult, tail_lines: usize) {
    println!(
        "local-ci {} · {} vs {} · {} file{} changed · {} · logs {}",
        r.version,
        &r.sha[..7],
        r.base,
        r.changed_files,
        if r.changed_files == 1 { "" } else { "s" },
        if r.mode == "all" {
            "all checks"
        } else {
            "triggered by the changes"
        },
        tilde(&r.run_dir)
    );
    let setup_bad: Vec<&StepResult> = r
        .setup
        .iter()
        .filter(|s| s.outcome != Outcome::Pass)
        .collect();
    if !r.setup.is_empty() {
        let secs: f64 = r.setup.iter().map(|s| s.secs).sum();
        if setup_bad.is_empty() {
            println!(
                "setup  ok  {} step{} {}",
                r.setup.len(),
                if r.setup.len() == 1 { "" } else { "s" },
                dur(secs)
            );
        } else {
            println!("setup  FAILED at {}", setup_bad[0].label);
        }
    }
    let mut skipped_jobs = Vec::new();
    for j in r.units.iter().flat_map(|u| &u.jobs) {
        let name = format!("{} › {}", short(&j.workflow), j.display);
        match &j.outcome {
            Outcome::Pass => println!("PASS   {name}  {}", dur(j.secs)),
            Outcome::Fail => {
                let s = failed_step(j);
                let what = s
                    .map(|s| format!(" › {} (exit {})", s.label, s.exit.unwrap_or(-1)))
                    .unwrap_or_default();
                println!("FAIL   {name}{what}  {}", dur(j.secs));
            }
            Outcome::Error(e) => println!("ERROR  {name}  {}", e.lines().next().unwrap_or("")),
            Outcome::Skip(reason) => skipped_jobs.push(format!("{} ({reason})", j.display)),
        }
    }
    if !skipped_jobs.is_empty() {
        println!("SKIP   {}", skipped_jobs.join(", "));
    }
    if !r.skipped_workflows.is_empty() {
        println!("not triggered ({})", group_skipped(&r.skipped_workflows));
    }
    if !r.not_covered.is_empty() {
        println!("not covered: {}", r.not_covered.join("; "));
    }
    if !r.warnings.is_empty() {
        println!("warnings: {}", r.warnings.join("; "));
    }
    // details of what went wrong
    let mut details: Vec<(String, &StepResult)> = setup_bad
        .iter()
        .map(|s| ("setup".to_string(), *s))
        .collect();
    for j in r.units.iter().flat_map(|u| &u.jobs) {
        if matches!(j.outcome, Outcome::Fail | Outcome::Error(_)) {
            if let Some(s) = failed_step(j) {
                details.push((format!("{} › {}", short(&j.workflow), j.display), s));
            } else if let Outcome::Error(e) = &j.outcome {
                if e.lines().count() > 1 {
                    println!(
                        "\n── ERROR {} › {}\n{}",
                        short(&j.workflow),
                        j.display,
                        indent(e.lines().skip(1).map(String::from).collect())
                    );
                }
            }
        }
    }
    for (where_, s) in details {
        let what = match &s.outcome {
            Outcome::Error(e) => format!("error: {e}"),
            _ => format!("exit {}", s.exit.unwrap_or(-1)),
        };
        println!("\n── {where_} › {} ({what})", s.label);
        if let Some(log) = &s.log {
            let t = tail(log, tail_lines);
            if t.is_empty() {
                println!("   (no output)");
            } else {
                println!("{}", indent(t));
            }
            println!("   log: {}", tilde(log));
        }
    }
    let c = counts(&r.units);
    println!(
        "\nresult: {} · pass {} / fail {} / error {} / skip {} · {}",
        r.state,
        c.pass,
        c.fail,
        c.error,
        c.skip,
        dur(r.secs)
    );
}

fn indent(lines: Vec<String>) -> String {
    lines
        .iter()
        .map(|l| format!("   {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(outcome: Outcome) -> JobResult {
        JobResult {
            workflow: ".github/workflows/ci.yml".into(),
            job: "j".into(),
            display: "j".into(),
            outcome,
            secs: 1.0,
            steps: vec![],
        }
    }

    #[test]
    fn states() {
        let u = |o: Vec<Outcome>| {
            vec![UnitResult {
                workflow: "ci.yml".into(),
                jobs: o.into_iter().map(job).collect(),
                waited_secs: 0.0,
                copy_secs: None,
            }]
        };
        assert_eq!(
            state(
                &[],
                &u(vec![Outcome::Pass, Outcome::Skip("if: false".into())])
            ),
            "success"
        );
        assert_eq!(
            state(
                &[],
                &u(vec![Outcome::Pass, Outcome::Error("docker".into())])
            ),
            "error"
        );
        assert_eq!(
            state(
                &[],
                &u(vec![Outcome::Fail, Outcome::Error("docker".into())])
            ),
            "failure"
        );
        let d = description("success", &u(vec![Outcome::Pass]), false, 2);
        assert_eq!(
            d,
            "ci.yml: success: pass 1 / fail 0 / error 0 / skip 0, 2 not covered"
        );
        assert!(d.len() <= 140);
    }

    #[test]
    fn durations() {
        assert_eq!(dur(5.2), "5s");
        assert_eq!(dur(125.0), "2m05s");
    }

    #[test]
    fn workflow_names_leave_off_the_default_dir() {
        assert_eq!(
            short_in(".github/workflows", ".github/workflows/ci.yml"),
            "ci.yml"
        );
        assert_eq!(short_in(".ci/workflows", ".ci/workflows/ci.yml"), "ci.yml");
        assert_eq!(
            short_in(".ci/workflows", ".github/workflows/ci.yml"),
            ".github/workflows/ci.yml"
        );
        assert_eq!(
            short_in(".ci/workflows", "ci/local/m.yml"),
            "ci/local/m.yml"
        );
        // a sibling that only shares the prefix keeps its path
        assert_eq!(
            short_in(".ci/workflows", ".ci/workflows-old/ci.yml"),
            ".ci/workflows-old/ci.yml"
        );
    }
}
