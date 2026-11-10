//! Is a run's result all of what the plan expected?
//!
//! A result that lacks a unit, a job, a matrix entry or a step is not a result
//! of the plan, and an empty result is not a success: [`validate_run_result`]
//! lists what the plan expects and checks the result against it. A run with
//! nothing to do is [`Verdict::NoWork`], which is not the same as a run whose
//! checks passed. A run that left out what local-ci cannot reproduce (an
//! action it does not know, a step that needs a secret, ...) is
//! [`Verdict::Incomplete`]: it may still have passed everything it did run, but
//! it is not a strict pass.
//!
//! The plan to check against is the whole plan, not the part of it that was
//! run: a unit whose result came from the cache (`UnitResult::cached`) counts
//! as the unit's result.

use crate::dag::{self, ExecutionTask, MatrixPlan};
use crate::exec::{JobResult, Outcome, UnitResult};
use crate::frozen::FrozenPlan;
use crate::report::RunResult;
use crate::task::{JobOutcome, JobStatus, TaskResult};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Everything the plan expects is there, and nothing was left out.
    Complete,
    /// The plan has nothing to run (the filters left no unit). Not a success
    /// of any check.
    NoWork,
    /// Everything is there, but parts were skipped that cannot be reproduced
    /// here (see [`Completeness::incomplete`]).
    Incomplete,
    /// The result lacks something, has something twice or something the plan
    /// does not have, or does not hold what the plan requires of it.
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Completeness {
    pub verdict: Verdict,
    /// Units the plan expects.
    pub expected_units: usize,
    /// Job entries the plan expects (a matrix job counts once per variant; a
    /// job whose matrix is an expression counts once).
    pub expected_jobs: usize,
    /// Units whose result was reused from an earlier run instead of run now.
    pub from_cache: usize,
    /// Expected, not in the result.
    pub missing: Vec<String>,
    /// In the result more than once.
    pub duplicate: Vec<String>,
    /// In the result, not in the plan.
    pub unexpected: Vec<String>,
    /// In the result, but not acceptable: no reason for a skip, a step not
    /// accounted for, an output or an artifact the plan requires that is not
    /// there.
    pub invalid: Vec<String>,
    /// What was left out because it cannot be reproduced here.
    pub incomplete: Vec<String>,
}

impl Completeness {
    /// Anything wrong with the shape of the result.
    pub fn has_problems(&self) -> bool {
        !(self.missing.is_empty()
            && self.duplicate.is_empty()
            && self.unexpected.is_empty()
            && self.invalid.is_empty())
    }

    /// The problems as short sentences, for the report.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut add = |what: &str, v: &[String]| {
            out.extend(v.iter().map(|x| format!("{what}: {x}")));
        };
        add("missing", &self.missing);
        add("duplicate", &self.duplicate);
        add("unexpected", &self.unexpected);
        add("invalid", &self.invalid);
        out
    }
}

fn unit_label(file: &str, jobs: &[String]) -> String {
    format!("{file} ({})", jobs.join(", "))
}

/// Check `r` against everything `fp` expects of it. `fp` is the whole plan.
pub fn validate_run_result(fp: &FrozenPlan, r: &RunResult) -> Completeness {
    let mut c = Completeness {
        verdict: Verdict::Complete,
        expected_units: fp.units.len(),
        expected_jobs: 0,
        from_cache: 0,
        missing: vec![],
        duplicate: vec![],
        unexpected: vec![],
        invalid: vec![],
        incomplete: r.not_covered.clone(),
    };
    // a workflow that could not be run at all was not checked
    c.incomplete
        .extend(fp.skipped.iter().filter(|s| s.unrunnable).map(|s| {
            let name = crate::report::short(&fp.config.default_workflows_dir, &s.workflow);
            format!("{name} (not run): {}", s.reason)
        }));
    // which result stands for which unit of the plan
    let mut got: Vec<Vec<&UnitResult>> = vec![vec![]; fp.units.len()];
    for (i, ru) in r.units.iter().enumerate() {
        match ru.unit.filter(|u| *u < fp.units.len()) {
            Some(ui) => {
                let spec = &fp.units[ui];
                if fp.workflows[spec.workflow].file != ru.workflow {
                    c.unexpected.push(format!(
                        "a result for {} stands for unit {ui}, which is of {}",
                        ru.workflow, fp.workflows[spec.workflow].file
                    ));
                } else {
                    got[ui].push(ru);
                }
            }
            None => c.unexpected.push(format!(
                "result {i} ({}) is of no unit of the plan",
                ru.workflow
            )),
        }
    }
    // the artifacts the run has a receipt for
    let published = crate::artifact_io::published_names(&r.run_dir);
    for (ui, spec) in fp.units.iter().enumerate() {
        let file = &fp.workflows[spec.workflow].file;
        let label = unit_label(file, &spec.jobs);
        for job in &spec.jobs {
            c.expected_jobs += expected_entries(fp, spec.workflow, job).0.max(1);
        }
        match got[ui].as_slice() {
            [] => c.missing.push(format!("the result of {label}")),
            [one] => check_unit(fp, ui, &label, one, &published, &mut c),
            many => c.duplicate.push(format!("{label}: {} results", many.len())),
        }
    }
    c.verdict = if c.has_problems() {
        Verdict::Invalid
    } else if !c.incomplete.is_empty() {
        Verdict::Incomplete
    } else if fp.units.is_empty() {
        Verdict::NoWork
    } else {
        Verdict::Complete
    };
    c
}

/// How many entries a job's result has: `(n, exact)`. A matrix known at
/// planning gives one per variant, exactly; a matrix computed at run time at
/// least one; a job that is not run for what it is (`container:`, a reusable
/// workflow) one skip.
fn expected_entries(fp: &FrozenPlan, wf: usize, job: &str) -> (usize, bool) {
    let spec = fp.job(wf, job);
    if spec.container || spec.reusable.is_some() {
        return (1, true);
    }
    match dag::matrix_plan(spec) {
        MatrixPlan::None => (1, true),
        MatrixPlan::Static(v) => (v.len(), true),
        MatrixPlan::Dynamic(_) => (1, false),
    }
}

fn check_unit(
    fp: &FrozenPlan,
    ui: usize,
    label: &str,
    ru: &UnitResult,
    published: &[String],
    c: &mut Completeness,
) {
    let spec = &fp.units[ui];
    let cached = ru.cached.is_some();
    if cached {
        c.from_cache += 1;
    }
    let mut by_job: BTreeMap<&str, Vec<&JobResult>> = BTreeMap::new();
    for j in &ru.jobs {
        by_job.entry(j.job.as_str()).or_default().push(j);
    }
    for id in by_job.keys() {
        if !spec.jobs.iter().any(|j| j == id) {
            c.unexpected.push(format!(
                "{label}: a result of job `{id}`, which is not in the unit"
            ));
        }
    }
    let mut passed: Vec<&str> = Vec::new();
    for id in &spec.jobs {
        let (n, exact) = expected_entries(fp, spec.workflow, id);
        let entries = by_job.get(id.as_str()).map(Vec::as_slice).unwrap_or(&[]);
        if entries.is_empty() {
            c.missing.push(format!("{label}: job `{id}` has no result"));
            continue;
        }
        if entries.len() < n {
            c.missing.push(format!(
                "{label}: job `{id}` has {} of its {n} matrix entries",
                entries.len()
            ));
        } else if entries.len() > n && exact {
            c.unexpected.push(format!(
                "{label}: job `{id}` has {} results, the plan has {n}",
                entries.len()
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        for e in entries {
            if seen.contains(&e.display.as_str()) {
                c.duplicate
                    .push(format!("{label}: job `{id}` › `{}` twice", e.display));
            }
            seen.push(&e.display);
            check_entry(fp, spec.workflow, id, label, e, cached, c);
        }
        if succeeded(entries) {
            passed.push(id);
        }
    }
    // What the plan requires of the tasks (outputs, artifacts); a reused
    // result keeps neither.
    if cached {
        return;
    }
    for t in fp
        .tasks
        .iter()
        .filter(|t| t.unit == ui && t.variant.as_ref().is_none_or(|v| v.index == 0))
    {
        let ok: Vec<&str> = t
            .jobs
            .iter()
            .map(String::as_str)
            .filter(|j| passed.contains(j))
            .collect();
        let every = ok.len() == t.jobs.len();
        let mut why = check_outputs(t, &ok, &ru.outcomes, published, every);
        why.retain(|w| !w.is_empty());
        c.invalid
            .extend(why.into_iter().map(|w| format!("{label}: {w}")));
    }
}

/// A job (all its entries) that passed: no entry failed, at least one passed.
fn succeeded(entries: &[&JobResult]) -> bool {
    entries
        .iter()
        .all(|e| matches!(e.outcome, Outcome::Pass | Outcome::Skip(_)) && !e.cancelled)
        && entries.iter().any(|e| e.outcome == Outcome::Pass)
}

fn check_entry(
    fp: &FrozenPlan,
    wf: usize,
    id: &str,
    label: &str,
    e: &JobResult,
    cached: bool,
    c: &mut Completeness,
) {
    let name = &e.display;
    match &e.outcome {
        Outcome::Skip(why) if why.trim().is_empty() => c
            .invalid
            .push(format!("{label}: `{name}` was skipped without a reason")),
        Outcome::Error(why) if why.trim().is_empty() => c
            .invalid
            .push(format!("{label}: `{name}` is an error without a reason")),
        Outcome::Pass if !cached => {
            let want = fp.job(wf, id).steps.len();
            if e.steps.len() != want {
                c.invalid.push(format!(
                    "{label}: `{name}` passed with {} of its {want} steps accounted for",
                    e.steps.len()
                ));
            }
        }
        _ => {}
    }
    for s in &e.steps {
        if matches!(&s.outcome, Outcome::Skip(w) if w.trim().is_empty()) {
            c.invalid.push(format!(
                "{label}: step `{}` of `{name}` was skipped without a reason",
                s.label
            ));
        }
    }
}

/// The part of the checks of a task's result that look at what it produced:
/// every job of `jobs` has exactly one outcome, every output the task's jobs
/// declare (`verification.job_outputs`) is there for a job that succeeded, and
/// when `all_succeeded` every artifact it must publish
/// (`verification.artifacts`) is in `artifacts`.
fn check_outputs(
    task: &ExecutionTask,
    jobs: &[&str],
    outcomes: &[JobOutcome],
    artifacts: &[String],
    all_succeeded: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    for j in jobs {
        let mine: Vec<&JobOutcome> = outcomes.iter().filter(|o| o.job == *j).collect();
        let [o] = mine.as_slice() else {
            out.push(if mine.is_empty() {
                format!("task {} has no outcome of job `{j}`", task.id)
            } else {
                format!("task {} has {} outcomes of job `{j}`", task.id, mine.len())
            });
            continue;
        };
        if o.status != JobStatus::Success {
            continue;
        }
        for decl in &task.verification.job_outputs {
            if let Some((job, name)) = decl.split_once('.') {
                if job == *j && !o.outputs.contains_key(name) {
                    out.push(format!(
                        "task {}: job `{j}` did not produce its output `{name}`",
                        task.id
                    ));
                }
            }
        }
    }
    if all_succeeded {
        for a in &task.verification.artifacts {
            if !artifacts.contains(a) {
                out.push(format!(
                    "task {}: the artifact `{a}` it must publish is missing",
                    task.id
                ));
            }
        }
    }
    out
}

/// What is wrong with the result of one task against what the plan requires of
/// it: the jobs it was to run (each with one outcome), the outputs they
/// declare and the artifacts it must publish. Empty when the result is
/// acceptable. A coordinator calls this on a result it received before it
/// accepts it.
pub fn verify_task_result(task: &ExecutionTask, r: &TaskResult) -> Vec<String> {
    let mut out = Vec::new();
    if r.task != task.id {
        out.push(format!(
            "the result is of task `{}`, not of `{}`",
            r.task, task.id
        ));
    }
    let jobs: Vec<&str> = task.jobs.iter().map(String::as_str).collect();
    for o in &r.outcomes {
        if !jobs.contains(&o.job.as_str()) {
            out.push(format!(
                "task {} has an outcome of job `{}`, which it does not hold",
                task.id, o.job
            ));
        }
    }
    let every = r.outcomes.iter().all(|o| o.status == JobStatus::Success);
    out.extend(check_outputs(task, &jobs, &r.outcomes, &r.artifacts, every));
    out
}
