//! The results of a run or of a task as they cross between hosts.
//!
//! [`RunResult`] and [`TaskResult`] are what local-ci writes for people and
//! for the engine: a step's `log` is a path on the host that ran it, and a run
//! has its `run_dir`. Neither means anything on another host, and a path that
//! comes from another host must not be believed. The types here are the same
//! results with the logs as paths relative to the directory the run (or the
//! task) kept them in, and no `run_dir`; the receiver puts its own directory
//! back with `into_*`, which refuses a path that leaves it.
//!
//! The JSON `local-ci run` and `local-ci status` write is that of
//! [`RunResult`] and does not change.
//!
//! What a receiver reads is bounded: the text ([`MAX_WIRE_BYTES`]), the number
//! of units, jobs and steps ([`MAX_WIRE_ITEMS`]) and every string
//! ([`MAX_WIRE_STRING`], [`MAX_WIRE_NOTE`]). `schema_version` is required in
//! the wire results (a result without it is refused), unlike the types the
//! engine reads from its own disk, which read a missing one as the first.
//!
//! A key the types do not know is refused, except in two of them:
//! [`WireStepResult`] and [`WireJobResult`] hold an `#[serde(flatten)]` field
//! ([`Outcome`]), and serde cannot refuse an unknown key there
//! (`deny_unknown_fields` and `flatten` do not go together). A step or job
//! result with a key of a newer writer is read without it. Read through
//! [`crate::wire::Digested::from_json_checked`] to notice: when the sender's
//! digest covers the key, the digest of what was read differs.

use crate::complete::{validate_run_result, Completeness};
use crate::exec::{JobResult, JobTime, Outcome, StepResult, UnitResult};
use crate::frozen::FrozenPlan;
use crate::report::{RunResult, Skipped};
use crate::shard::ShardOutcome;
use crate::task::{JobOutcome, TaskResult};
use crate::wire::{check_version, wire_version, Digested};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// The most JSON text a result is read from.
pub const MAX_WIRE_BYTES: usize = 64 * 1024 * 1024;
/// The longest relative path a log may have.
pub const MAX_LOG_PATH: usize = 1024;
/// The most units, jobs and steps (together, counted over the whole result)
/// and the most outcomes or artifacts a result read from another host may
/// hold. A plan of this tool is far smaller; the bound keeps a hostile result
/// from costing memory and a long check.
pub const MAX_WIRE_ITEMS: usize = 100_000;
/// The longest label, id, name or path a result read from another host may
/// hold, in bytes.
pub const MAX_WIRE_STRING: usize = 4 * 1024;
/// The longest `note` (an error message, a skip reason) of a step, in bytes.
pub const MAX_WIRE_NOTE: usize = 64 * 1024;

fn check_string(what: &str, s: &str, max: usize) -> Result<()> {
    if s.len() > max {
        bail!("{what} is {} bytes; at most {max} are read", s.len());
    }
    Ok(())
}

fn check_count(what: &str, n: usize) -> Result<()> {
    if n > MAX_WIRE_ITEMS {
        bail!("{n} {what}; at most {MAX_WIRE_ITEMS} are read");
    }
    Ok(())
}

/// `p` relative to `base`, as a wire path: UTF-8, `/` separated, nothing
/// that leaves `base`.
fn relative(base: &Path, p: &Path) -> Result<String> {
    let rel = p
        .strip_prefix(base)
        .map_err(|_| anyhow::anyhow!("a log is outside the directory of its run"))?;
    let text = rel.to_str().context("a log path is not UTF-8")?.to_string();
    check_relative(&text)?;
    Ok(text)
}

/// A path that is relative and stays below the directory it is joined to,
/// with no control character and no backslash.
fn check_relative(text: &str) -> Result<()> {
    if text.is_empty()
        || text.len() > MAX_LOG_PATH
        || text.chars().any(|c| c.is_control() || c == '\\')
    {
        bail!("not a log path: empty, too long, or with a control character or a backslash");
    }
    if !Path::new(text)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        bail!("a log path must be relative and stay below its directory: {text:?}");
    }
    Ok(())
}

fn resolve(base: &Path, text: &str) -> Result<PathBuf> {
    check_relative(text)?;
    Ok(base.join(text))
}

fn read_json<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T> {
    if text.len() > MAX_WIRE_BYTES {
        bail!(
            "{what} is {} bytes; at most {MAX_WIRE_BYTES} are read",
            text.len()
        );
    }
    serde_json::from_str(text).with_context(|| format!("cannot read the {what}"))
}

/// A [`StepResult`] with its log relative to the directory of the run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireStepResult {
    pub label: String,
    #[serde(flatten)]
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    pub secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl WireStepResult {
    pub fn from_step(s: &StepResult, base: &Path) -> Result<Self> {
        Ok(WireStepResult {
            label: s.label.clone(),
            outcome: s.outcome.clone(),
            exit: s.exit,
            secs: s.secs,
            log: s.log.as_deref().map(|l| relative(base, l)).transpose()?,
            note: s.note.clone(),
        })
    }

    pub fn into_step(self, base: &Path) -> Result<StepResult> {
        check_string("a step label", &self.label, MAX_WIRE_STRING)?;
        if let Some(n) = &self.note {
            check_string("a step note", n, MAX_WIRE_NOTE)?;
        }
        Ok(StepResult {
            label: self.label,
            outcome: self.outcome,
            exit: self.exit,
            secs: self.secs,
            log: self.log.as_deref().map(|l| resolve(base, l)).transpose()?,
            note: self.note,
        })
    }
}

fn steps_of(steps: &[StepResult], base: &Path) -> Result<Vec<WireStepResult>> {
    steps
        .iter()
        .map(|s| WireStepResult::from_step(s, base))
        .collect()
}

fn steps_back(steps: Vec<WireStepResult>, base: &Path) -> Result<Vec<StepResult>> {
    steps.into_iter().map(|s| s.into_step(base)).collect()
}

/// A [`JobResult`] with relative logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireJobResult {
    pub workflow: String,
    pub job: String,
    pub display: String,
    #[serde(flatten)]
    pub outcome: Outcome,
    pub secs: f64,
    pub steps: Vec<WireStepResult>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<JobTime>,
}

impl WireJobResult {
    pub fn from_job(j: &JobResult, base: &Path) -> Result<Self> {
        Ok(WireJobResult {
            workflow: j.workflow.clone(),
            job: j.job.clone(),
            display: j.display.clone(),
            outcome: j.outcome.clone(),
            secs: j.secs,
            steps: steps_of(&j.steps, base)?,
            cancelled: j.cancelled,
            time: j.time.clone(),
        })
    }

    pub fn into_job(self, base: &Path) -> Result<JobResult> {
        for (what, v) in [
            ("a workflow", &self.workflow),
            ("a job id", &self.job),
            ("a job name", &self.display),
        ] {
            check_string(what, v, MAX_WIRE_STRING)?;
        }
        check_count("steps in a job", self.steps.len())?;
        Ok(JobResult {
            workflow: self.workflow,
            job: self.job,
            display: self.display,
            outcome: self.outcome,
            secs: self.secs,
            steps: steps_back(self.steps, base)?,
            cancelled: self.cancelled,
            time: self.time,
        })
    }
}

/// A [`UnitResult`] with relative logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireUnitResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<usize>,
    pub workflow: String,
    pub jobs: Vec<WireJobResult>,
    pub waited_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<JobOutcome>,
}

impl WireUnitResult {
    pub fn from_unit(u: &UnitResult, base: &Path) -> Result<Self> {
        Ok(WireUnitResult {
            unit: u.unit,
            workflow: u.workflow.clone(),
            jobs: u
                .jobs
                .iter()
                .map(|j| WireJobResult::from_job(j, base))
                .collect::<Result<_>>()?,
            waited_secs: u.waited_secs,
            copy_secs: u.copy_secs,
            cached: u.cached.clone(),
            outcomes: u.outcomes.clone(),
        })
    }

    pub fn into_unit(self, base: &Path) -> Result<UnitResult> {
        check_string("a workflow", &self.workflow, MAX_WIRE_STRING)?;
        check_count("jobs in a unit", self.jobs.len())?;
        check_count("outcomes in a unit", self.outcomes.len())?;
        Ok(UnitResult {
            unit: self.unit,
            workflow: self.workflow,
            jobs: self
                .jobs
                .into_iter()
                .map(|j| j.into_job(base))
                .collect::<Result<_>>()?,
            waited_secs: self.waited_secs,
            copy_secs: self.copy_secs,
            cached: self.cached,
            outcomes: self.outcomes,
        })
    }
}

/// A [`RunResult`] without the host's `run_dir` and with relative logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRunResult {
    /// See [`crate::wire::WIRE_SCHEMA_VERSION`]. Required: a result without
    /// it is refused.
    pub schema_version: u32,
    pub version: String,
    pub sha: String,
    pub base: String,
    pub changed_files: usize,
    pub mode: String,
    pub state: String,
    pub description: String,
    pub secs: f64,
    pub setup: Vec<WireStepResult>,
    pub units: Vec<WireUnitResult>,
    pub skipped_workflows: Vec<Skipped>,
    pub not_covered: Vec<String>,
    pub warnings: Vec<String>,
    pub completeness: Completeness,
}

impl Digested for WireRunResult {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl WireRunResult {
    /// `r` with its logs relative to `r.run_dir`. Fails for a log outside it.
    pub fn from_run_result(r: &RunResult) -> Result<Self> {
        let base = r.run_dir.as_path();
        Ok(WireRunResult {
            schema_version: wire_version(),
            version: r.version.clone(),
            sha: r.sha.clone(),
            base: r.base.clone(),
            changed_files: r.changed_files,
            mode: r.mode.clone(),
            state: r.state.clone(),
            description: r.description.clone(),
            secs: r.secs,
            setup: steps_of(&r.setup, base)?,
            units: r
                .units
                .iter()
                .map(|u| WireUnitResult::from_unit(u, base))
                .collect::<Result<_>>()?,
            skipped_workflows: r.skipped_workflows.clone(),
            not_covered: r.not_covered.clone(),
            warnings: r.warnings.clone(),
            completeness: r.completeness.clone(),
        })
    }

    /// The result with `run_dir` as the directory its logs are in here. Fails
    /// for a version this build does not read and for a log path that is not
    /// relative or leaves the directory.
    pub fn into_run_result(self, run_dir: PathBuf) -> Result<RunResult> {
        check_version("run result", self.schema_version)?;
        self.check_size()?;
        Ok(RunResult {
            version: self.version,
            sha: self.sha,
            base: self.base,
            changed_files: self.changed_files,
            mode: self.mode,
            state: self.state,
            description: self.description,
            secs: self.secs,
            setup: steps_back(self.setup, &run_dir)?,
            units: self
                .units
                .into_iter()
                .map(|u| u.into_unit(&run_dir))
                .collect::<Result<_>>()?,
            skipped_workflows: self.skipped_workflows,
            not_covered: self.not_covered,
            warnings: self.warnings,
            completeness: self.completeness,
            run_dir,
        })
    }

    /// The result's size is within the bounds a receiver reads.
    fn check_size(&self) -> Result<()> {
        let mut items = self.setup.len() + self.units.len();
        for u in &self.units {
            items += u.jobs.len();
            items += u.jobs.iter().map(|j| j.steps.len()).sum::<usize>();
        }
        check_count("units, jobs and steps", items)?;
        check_count("warnings", self.warnings.len())?;
        check_count("workflows not covered", self.not_covered.len())?;
        check_count("skipped workflows", self.skipped_workflows.len())
    }

    /// Read a result from JSON (at most [`MAX_WIRE_BYTES`] of it). Prefer
    /// [`crate::wire::Digested::from_json_checked`], which also checks the
    /// digest.
    pub fn from_json(text: &str) -> Result<Self> {
        read_json(text, "run result")
    }
}

/// Is a result that came from another host all of what `fp` expects of it?
/// `run_dir` is where this host keeps the run's files (its artifacts).
pub fn validate_wire_run_result(
    fp: &FrozenPlan,
    r: WireRunResult,
    run_dir: &Path,
) -> Result<Completeness> {
    Ok(validate_run_result(fp, &r.into_run_result(run_dir.into())?))
}

/// A [`TaskResult`] with relative logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireTaskResult {
    /// See [`crate::wire::WIRE_SCHEMA_VERSION`]. Required: a result without
    /// it is refused.
    pub schema_version: u32,
    pub task: String,
    pub jobs: Vec<WireJobResult>,
    pub outcomes: Vec<JobOutcome>,
    pub receipt_ignored: Option<String>,
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub shard: Option<ShardOutcome>,
}

impl Digested for WireTaskResult {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl WireTaskResult {
    /// `r` with its logs relative to `base`, the directory the task kept them
    /// in. Fails for a log outside it.
    pub fn from_task_result(r: &TaskResult, base: &Path) -> Result<Self> {
        Ok(WireTaskResult {
            schema_version: r.schema_version,
            task: r.task.clone(),
            jobs: r
                .jobs
                .iter()
                .map(|j| WireJobResult::from_job(j, base))
                .collect::<Result<_>>()?,
            outcomes: r.outcomes.clone(),
            receipt_ignored: r.receipt_ignored.clone(),
            artifacts: r.artifacts.clone(),
            shard: r.shard.clone(),
        })
    }

    /// The result with `base` as the directory its logs are in here.
    pub fn into_task_result(self, base: &Path) -> Result<TaskResult> {
        check_version("task result", self.schema_version)?;
        check_string("a task id", &self.task, MAX_WIRE_STRING)?;
        check_count("jobs", self.jobs.len())?;
        check_count(
            "steps",
            self.jobs.iter().map(|j| j.steps.len()).sum::<usize>(),
        )?;
        check_count("outcomes", self.outcomes.len())?;
        check_count("artifacts", self.artifacts.len())?;
        for a in &self.artifacts {
            crate::artifact::validate_name(a).map_err(|e| anyhow::anyhow!("an artifact: {e}"))?;
        }
        Ok(TaskResult {
            schema_version: self.schema_version,
            task: self.task,
            jobs: self
                .jobs
                .into_iter()
                .map(|j| j.into_job(base))
                .collect::<Result<_>>()?,
            outcomes: self.outcomes,
            receipt_ignored: self.receipt_ignored,
            artifacts: self.artifacts,
            shard: self.shard,
        })
    }

    /// Read a result from JSON (at most [`MAX_WIRE_BYTES`] of it). Prefer
    /// [`crate::wire::Digested::from_json_checked`], which also checks the
    /// digest.
    pub fn from_json(text: &str) -> Result<Self> {
        read_json(text, "task result")
    }
}
