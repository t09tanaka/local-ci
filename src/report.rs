//! The result of a run: its state, the short text report, and the JSON file.

use crate::exec::{tail, JobResult, JobTime, Outcome, StepResult, UnitResult};
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

/// What printing needs to know about the host: the directory output leaves
/// off workflow paths, and the home directory shown as `~`.
#[derive(Debug, Clone)]
pub struct Style {
    /// The first `workflows-dirs` entry of `.local-ci.toml`, else `.github/workflows`.
    pub workflows_dir: String,
    pub home: Option<String>,
}

/// How a workflow is named in output: the file name for the default directory
/// `dir` (`.github/workflows/`, or the first one `.local-ci.toml` names), the
/// path for any other, so equal file names stay apart.
pub fn short<'a>(dir: &str, f: &'a str) -> &'a str {
    short_in(dir, f)
}

fn short_in<'a>(dir: &str, f: &'a str) -> &'a str {
    f.strip_prefix(dir)
        .and_then(|r| r.strip_prefix('/'))
        .unwrap_or(f)
}

/// `p` with `home` at its start shown as `~`.
pub fn tilde(p: &Path, home: Option<&str>) -> String {
    let s = p.to_string_lossy().into_owned();
    match home {
        Some(h) if !h.is_empty() && s.starts_with(h) => format!("~{}", &s[h.len()..]),
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

/// ` (ran 2m20s · waited slot 5m00s, queue 40s)` for a job that waited a second
/// or more for something; empty otherwise.
fn waits(t: Option<&JobTime>) -> String {
    let Some(t) = t else { return String::new() };
    let list: Vec<String> = [
        ("lock", t.wait_lock_secs),
        ("slot", t.wait_slot_secs),
        ("queue", t.wait_queue_secs),
        ("parallel", t.wait_parallel_secs),
    ]
    .iter()
    .filter(|(_, s)| *s >= 1.0)
    .map(|(n, s)| format!("{n} {}", dur(*s)))
    .collect();
    if list.is_empty() {
        return String::new();
    }
    format!(" (ran {} · waited {})", dur(t.work_secs), list.join(", "))
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

/// Jobs whose result was reused instead of run.
pub fn cached_jobs(units: &[UnitResult]) -> usize {
    units
        .iter()
        .filter(|u| u.cached.is_some())
        .map(|u| u.jobs.len())
        .sum()
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
pub fn not_covered(units: &[UnitResult], dir: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for j in units.iter().flat_map(|u| &u.jobs) {
        if let Outcome::Skip(r) = &j.outcome {
            if r.contains("not supported") {
                out.push(format!("{} › {}: {r}", short(dir, &j.workflow), j.display));
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
                        short(dir, &j.workflow),
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

/// The result of a run over `units`, worked out from what ran. `nothing_triggered`
/// is a plan that had no unit to begin with.
pub fn assemble(
    fp: &crate::frozen::FrozenPlan,
    run_dir: PathBuf,
    setup: Vec<StepResult>,
    units: Vec<UnitResult>,
    nothing_triggered: bool,
    secs: f64,
) -> RunResult {
    let dir = fp.config.default_workflows_dir.as_str();
    let state = state(&setup, &units);
    let not_covered = not_covered(&units, dir);
    let description = description(state, &units, nothing_triggered, not_covered.len(), dir);
    RunResult {
        version: env!("CARGO_PKG_VERSION").into(),
        sha: fp.source.sha.clone(),
        base: fp.source.base.clone(),
        changed_files: fp.changed_files.len(),
        mode: if fp.filters.all { "all" } else { "changes" }.into(),
        state: state.into(),
        description,
        secs,
        run_dir,
        warnings: warnings(&units),
        setup,
        units,
        skipped_workflows: fp
            .skipped
            .iter()
            .map(|s| Skipped {
                workflow: s.workflow.clone(),
                reason: s.reason.clone(),
            })
            .collect(),
        not_covered,
    }
}

pub fn description(
    state: &str,
    units: &[UnitResult],
    skipped_only: bool,
    not_covered: usize,
    dir: &str,
) -> String {
    if skipped_only {
        return "no workflow triggered by these changes".into();
    }
    let c = counts(units);
    let mut wfs: Vec<&str> = units.iter().map(|u| short(dir, &u.workflow)).collect();
    wfs.dedup();
    let mut d = format!(
        "{state}: pass {} / fail {} / error {} / skip {}",
        c.pass, c.fail, c.error, c.skip
    );
    if not_covered > 0 {
        d.push_str(&format!(", {not_covered} not covered"));
    }
    let cached = cached_jobs(units);
    if cached > 0 {
        d.push_str(&format!(", {cached} cached"));
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
pub fn group_skipped(skipped: &[Skipped], dir: &str) -> String {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for sk in skipped {
        match groups.iter_mut().find(|(reason, _)| *reason == sk.reason) {
            Some((_, files)) => files.push(short(dir, &sk.workflow)),
            None => groups.push((&sk.reason, vec![short(dir, &sk.workflow)])),
        }
    }
    groups
        .iter()
        .map(|(reason, files)| format!("{reason}: {}", files.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn print(r: &RunResult, tail_lines: usize, style: &Style) {
    let dir = style.workflows_dir.as_str();
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
        tilde(&r.run_dir, style.home.as_deref())
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
    for (u, j) in r
        .units
        .iter()
        .flat_map(|u| u.jobs.iter().map(move |j| (u, j)))
    {
        let name = format!("{} › {}", short(dir, &j.workflow), j.display);
        match &j.outcome {
            Outcome::Pass => match &u.cached {
                Some(from) => println!("PASS   {name}  cached ({from})"),
                None => println!("PASS   {name}  {}{}", dur(j.secs), waits(j.time.as_ref())),
            },
            Outcome::Fail => {
                let s = failed_step(j);
                let what = s
                    .map(|s| format!(" › {} (exit {})", s.label, s.exit.unwrap_or(-1)))
                    .unwrap_or_default();
                println!(
                    "FAIL   {name}{what}  {}{}",
                    dur(j.secs),
                    waits(j.time.as_ref())
                );
            }
            Outcome::Error(e) => println!("ERROR  {name}  {}", e.lines().next().unwrap_or("")),
            Outcome::Skip(reason) => skipped_jobs.push(format!("{} ({reason})", j.display)),
        }
    }
    if !skipped_jobs.is_empty() {
        println!("SKIP   {}", skipped_jobs.join(", "));
    }
    if !r.skipped_workflows.is_empty() {
        println!(
            "not triggered ({})",
            group_skipped(&r.skipped_workflows, dir)
        );
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
                details.push((format!("{} › {}", short(dir, &j.workflow), j.display), s));
            } else if let Outcome::Error(e) = &j.outcome {
                if e.lines().count() > 1 {
                    println!(
                        "\n── ERROR {} › {}\n{}",
                        short(dir, &j.workflow),
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
            println!("   log: {}", tilde(log, style.home.as_deref()));
        }
    }
    let c = counts(&r.units);
    let cached = cached_jobs(&r.units);
    println!(
        "\nresult: {} · pass {} / fail {} / error {} / skip {}{} · {}",
        r.state,
        c.pass,
        c.fail,
        c.error,
        c.skip,
        if cached > 0 {
            format!(" ({cached} cached)")
        } else {
            String::new()
        },
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
            cancelled: false,
            time: None,
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
                cached: None,
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
        let d = description(
            "success",
            &u(vec![Outcome::Pass]),
            false,
            2,
            ".github/workflows",
        );
        assert_eq!(
            d,
            "ci.yml: success: pass 1 / fail 0 / error 0 / skip 0, 2 not covered"
        );
        assert!(d.len() <= 140);
    }

    #[test]
    fn waits_are_named_only_when_there_were_some() {
        let t = |lock, slot, queue, parallel| JobTime {
            work_secs: 140.0,
            wait_lock_secs: lock,
            wait_slot_secs: slot,
            wait_queue_secs: queue,
            wait_parallel_secs: parallel,
        };
        assert_eq!(waits(None), "");
        assert_eq!(waits(Some(&t(0.0, 0.4, 0.0, 0.2))), "");
        assert_eq!(
            waits(Some(&t(0.0, 300.0, 40.0, 0.0))),
            " (ran 2m20s · waited slot 5m00s, queue 40s)"
        );
        assert_eq!(
            waits(Some(&t(2.0, 0.0, 0.0, 3.0))),
            " (ran 2m20s · waited lock 2s, parallel 3s)"
        );
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
