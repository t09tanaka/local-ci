//! Running one task of a plan, given what the tasks before it produced.
//!
//! [`execute_task`] does not look at any other task and never starts a job it
//! was not asked for: the results and outputs of the jobs it `needs` come in
//! as [`TaskInputs`], and what it produced goes out as a [`TaskResult`]. The
//! caller (a local run, or a coordinator that placed the earlier tasks on other
//! hosts) decides the order and carries the inputs over.

use crate::context::ExecContext;
use crate::dag::{JobNode, NodeKind};
use crate::exec::{JobResult, Opts, Outcome, Runner, Workspace};
use crate::frozen::FrozenPlan;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// How a job ended, as `needs.<id>.result` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Success,
    Failure,
    Skipped,
    Cancelled,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Success => "success",
            JobStatus::Failure => "failure",
            JobStatus::Skipped => "skipped",
            JobStatus::Cancelled => "cancelled",
        }
    }

    /// The status of one job (or matrix entry) from how it ended: an error
    /// counts as a failure, a skip because the run was cancelled as cancelled.
    pub(crate) fn of(o: &Outcome) -> Self {
        match o {
            Outcome::Pass => JobStatus::Success,
            Outcome::Fail | Outcome::Error(_) => JobStatus::Failure,
            Outcome::Skip(r) if r == "cancelled" => JobStatus::Cancelled,
            Outcome::Skip(_) => JobStatus::Skipped,
        }
    }
}

/// A finished job as the jobs that need it see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobOutcome {
    pub job: String,
    /// The matrix entry (index in matrix order) when this is one entry of a
    /// matrix job, `None` for the job as a whole.
    pub variant: Option<usize>,
    pub status: JobStatus,
    /// `outputs:` of the job, evaluated.
    pub outputs: BTreeMap<String, String>,
    /// Why the job did not run (an `if:` that was false, `needs failed`,
    /// `fail-fast`, `cancelled`, ...).
    pub skipped: Option<String>,
}

/// What a task is given about the jobs it needs, by job id. Jobs of the same
/// task are not listed: they are run in order and see each other.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskInputs {
    pub needs: BTreeMap<String, JobOutcome>,
}

impl TaskInputs {
    /// The `needs` context the expressions of a job are evaluated against.
    pub(crate) fn context(&self) -> Map<String, Value> {
        self.needs
            .iter()
            .map(|(k, o)| (k.clone(), ctx_of(o)))
            .collect()
    }
}

pub(crate) fn ctx_of(o: &JobOutcome) -> Value {
    let outputs: Map<String, Value> = o
        .outputs
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    json!({"result": o.status.as_str(), "outputs": outputs})
}

/// What running a task produced.
#[derive(Debug, Clone, Serialize)]
pub struct TaskResult {
    pub task: String,
    /// Each job (each matrix entry) with its steps, as a run reports them.
    pub jobs: Vec<JobResult>,
    /// Each job of the task as a whole, in the order they ran. For a task that
    /// is one variant of a matrix: that variant.
    pub outcomes: Vec<JobOutcome>,
}

/// One job, or one matrix entry of it, from its result and outputs.
pub(crate) fn outcome_of(
    job: &str,
    variant: Option<usize>,
    r: &JobResult,
    outputs: &Map<String, Value>,
) -> JobOutcome {
    JobOutcome {
        job: job.to_string(),
        variant,
        status: JobStatus::of(&r.outcome),
        outputs: outputs
            .iter()
            .map(|(k, v)| (k.clone(), crate::expr::to_string(v)))
            .collect(),
        skipped: match &r.outcome {
            Outcome::Skip(why) => Some(why.clone()),
            _ => None,
        },
    }
}

/// The entries of a matrix job as the job: failed if any failed, skipped if
/// all were, otherwise success. Outputs are merged in matrix order (by
/// `variant`, whatever order `parts` is in), a name set by several entries
/// keeping the value of the last in matrix order.
///
/// GitHub takes, for a name set by several entries, the value of the entry that
/// finished last; that depends on timing, so a plan cannot be run twice with the
/// same outputs. The order of the matrix is the rule here instead.
pub(crate) fn combine(job: &str, mut parts: Vec<JobOutcome>) -> JobOutcome {
    parts.sort_by_key(|p| p.variant.unwrap_or(0));
    let mut outputs = BTreeMap::new();
    for p in &parts {
        for (k, v) in &p.outputs {
            outputs.insert(k.clone(), v.clone());
        }
    }
    let any = |s: JobStatus| parts.iter().any(|p| p.status == s);
    let status = if any(JobStatus::Failure) {
        JobStatus::Failure
    } else if !parts.is_empty()
        && parts
            .iter()
            .all(|p| !matches!(p.status, JobStatus::Success))
    {
        if any(JobStatus::Cancelled) {
            JobStatus::Cancelled
        } else {
            JobStatus::Skipped
        }
    } else {
        JobStatus::Success
    };
    let skipped = parts.iter().find_map(|p| p.skipped.clone());
    JobOutcome {
        job: job.to_string(),
        variant: None,
        status,
        outputs,
        skipped: if status == JobStatus::Success {
            None
        } else {
            skipped
        },
    }
}

/// The job that a matrix is, from the outcomes of its variants, which may
/// arrive in any order. Every variant of the node must be there exactly once.
pub fn aggregate_matrix(group: &JobNode, variants: &[JobOutcome]) -> Result<JobOutcome> {
    let NodeKind::MatrixGroup { variants: ids, .. } = &group.kind else {
        bail!("{} is not a matrix", group.id);
    };
    let mut seen = vec![false; ids.len()];
    for v in variants {
        let i = v
            .variant
            .filter(|i| *i < ids.len())
            .ok_or_else(|| anyhow!("an outcome of {} names no variant of it", group.id))?;
        if std::mem::replace(&mut seen[i], true) {
            bail!("variant {i} of {} is there twice", group.id);
        }
    }
    if let Some(i) = seen.iter().position(|s| !s) {
        bail!("variant {i} of {} has no outcome", group.id);
    }
    Ok(combine(&group.job, variants.to_vec()))
}

/// Run the installs the plan hoisted, in `ws`. A task assumes they are done in
/// the workspace it is given (see [`crate::dag::WorkspacePolicy`]); this is how
/// they get done. Returns whether all passed.
pub fn execute_setup(
    ctx: &ExecContext,
    fp: &FrozenPlan,
    ws: &Workspace,
    opts: Opts,
) -> Result<Vec<crate::exec::StepResult>> {
    let runner = Runner::new(ctx, fp, ws, opts);
    runner.prepare_tools()?;
    let r = runner.run_setup();
    ctx.cancel.remove_containers(ctx);
    Ok(r)
}

/// Run task `task_id` of `fp` in `ws`, with `inputs` as the results of the jobs
/// it needs, and return what it produced. Jobs it does not hold are not
/// started, and `-j`/units are not looked at again.
///
/// The workspace is a checkout of the planned commit with the plan's hoisted
/// setup done (see [`execute_setup`]); it is left for the caller to remove.
/// Containers the task started are gone when this returns.
///
/// Conditions (`needs.*.result`, `success()`, `failure()`, `always()`,
/// `cancelled()`) and `needs.<id>.outputs.*` are evaluated against `inputs`.
/// A job whose condition is false is not run and the result says why.
pub fn execute_task(
    ctx: &ExecContext,
    fp: &FrozenPlan,
    task_id: &str,
    inputs: &TaskInputs,
    ws: &Workspace,
    opts: Opts,
) -> Result<TaskResult> {
    let task = fp
        .task(task_id)
        .ok_or_else(|| anyhow!("the plan has no task `{task_id}`"))?;
    let keep = opts.keep;
    let runner = Runner::new(ctx, fp, ws, opts);
    let result = (|| {
        runner.prepare_tools()?;
        let lock = if runner.task_locked(task) {
            runner.lock().ok()
        } else {
            None
        };
        let r = runner.run_task(&ws.root, task, inputs, lock.is_some(), false);
        drop(lock);
        r
    })();
    ctx.cancel.remove_containers(ctx);
    if !keep {
        runner.release_ports(&ws.root);
    }
    result
}
