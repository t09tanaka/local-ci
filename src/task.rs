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
use crate::setup::{PreparedReceipt, PreparedSetup, SetupRecipe, TemplateContext, TemplateKey};
use crate::shard::ShardOutcome;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

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
    /// counts as a failure, a job the run was cancelled under (before it
    /// started or while it ran) as cancelled.
    pub(crate) fn of(r: &JobResult) -> Self {
        if r.cancelled {
            return JobStatus::Cancelled;
        }
        match r.outcome {
            Outcome::Pass => JobStatus::Success,
            Outcome::Fail | Outcome::Error(_) => JobStatus::Failure,
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
    /// A job this one needs, directly or through the jobs it needs, failed
    /// (this job's own failure is `status`). This is what `failure()` of a job
    /// asks, together with the status of the jobs it needs directly; carrying it
    /// in each outcome lets a coordinator hand a task only the direct needs.
    #[serde(default)]
    pub ancestor_failed: bool,
}

impl JobOutcome {
    /// This job, or a job it needs, failed.
    pub fn failed_or_ancestor_failed(&self) -> bool {
        self.status == JobStatus::Failure || self.ancestor_failed
    }
}

/// What a task is given about the jobs it needs, by job id. Jobs of the same
/// task are not listed: they are run in order and see each other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskInputs {
    /// See [`crate::wire::WIRE_SCHEMA_VERSION`].
    #[serde(default = "crate::wire::wire_version")]
    pub schema_version: u32,
    pub needs: BTreeMap<String, JobOutcome>,
    /// The run was cancelled: what `cancelled()` answers in a job, whether or
    /// not a job it needs was cancelled. A coordinator sets it for the tasks it
    /// starts after the run was cancelled; [`TaskInputs::new`] sets it from the
    /// outcomes it is given.
    #[serde(default)]
    pub run_cancelled: bool,
}

impl crate::wire::Digested for TaskInputs {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}
impl crate::wire::Digested for TaskResult {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl Default for TaskInputs {
    fn default() -> Self {
        TaskInputs {
            schema_version: crate::wire::wire_version(),
            needs: BTreeMap::new(),
            run_cancelled: false,
        }
    }
}

impl TaskInputs {
    /// The inputs for a task that needs `needs`, from every job finished so far
    /// (the task takes the jobs it needs by id and ignores the rest).
    /// `cancel_requested`: the run was cancelled. The run also counts as
    /// cancelled when any finished job was.
    pub fn new<'a>(
        needs: impl IntoIterator<Item = &'a JobOutcome>,
        finished: impl IntoIterator<Item = &'a JobOutcome>,
        cancel_requested: bool,
    ) -> Self {
        TaskInputs {
            schema_version: crate::wire::wire_version(),
            needs: needs
                .into_iter()
                .map(|o| (o.job.clone(), o.clone()))
                .collect(),
            run_cancelled: cancel_requested
                || finished
                    .into_iter()
                    .any(|o| o.status == JobStatus::Cancelled),
        }
    }

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    /// See [`crate::wire::WIRE_SCHEMA_VERSION`].
    #[serde(default = "crate::wire::wire_version")]
    pub schema_version: u32,
    pub task: String,
    /// Each job (each matrix entry) with its steps, as a run reports them.
    pub jobs: Vec<JobResult>,
    /// Each job of the task as a whole, in the order they ran. For a task that
    /// is one variant of a matrix: that variant.
    pub outcomes: Vec<JobOutcome>,
    /// Why the receipt that came with the task was not believed (another
    /// commit, another workspace, another template key); the task then did all
    /// its setup itself. `None` when there was no receipt or it was used.
    pub receipt_ignored: Option<String>,
    /// Names of the artifacts uploaded by the task's own jobs, in the area of
    /// its workflow (the `artifacts` a task's
    /// [`crate::dag::OutputVerification`] asks for are checked against this).
    pub artifacts: Vec<String>,
    /// For the task of one shard of a split job: what the shard's tests
    /// reported, read from the log of the step that ran them. `None` for any
    /// other task.
    #[serde(default)]
    pub shard: Option<ShardOutcome>,
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
        status: JobStatus::of(r),
        outputs: outputs
            .iter()
            .map(|(k, v)| (k.clone(), crate::expr::to_string(v)))
            .collect(),
        skipped: match &r.outcome {
            Outcome::Skip(why) => Some(why.clone()),
            _ => None,
        },
        ancestor_failed: false,
    }
}

/// The entries of a matrix job as the job, as `needs.<id>.result` is for a
/// matrix on GitHub: failed if any failed, otherwise cancelled if any was,
/// skipped if all were, otherwise success. Outputs are merged in matrix order
/// (by `variant`, whatever order `parts` is in), a name set by several entries
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
    } else if any(JobStatus::Cancelled) {
        JobStatus::Cancelled
    } else if !parts.is_empty() && parts.iter().all(|p| p.status == JobStatus::Skipped) {
        JobStatus::Skipped
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
        ancestor_failed: parts.iter().any(|p| p.ancestor_failed),
    }
}

/// The outputs of `parts` that two of them give different values, as
/// sentences. The shards of one job run the same job: an output they disagree
/// on has no one value to publish.
pub(crate) fn conflicting_outputs(parts: &[JobOutcome]) -> Vec<String> {
    let mut first: BTreeMap<&str, &str> = BTreeMap::new();
    let mut out = BTreeSet::new();
    for (k, v) in parts.iter().flat_map(|p| &p.outputs) {
        if *first.entry(k).or_insert(v) != v {
            out.insert(k.as_str());
        }
    }
    out.into_iter()
        .map(|k| format!("the shards gave different values for the output `{k}`"))
        .collect()
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

/// Do `recipe`, the setup of a task, in `ws` and return what was prepared.
/// `key` is the template key the result is for. Steps that were run are in
/// `steps` as a run reports them; the receipt names only those that passed (or
/// that an equal install in the same workspace had already done).
pub fn execute_setup(
    ctx: &ExecContext,
    fp: &FrozenPlan,
    recipe: &SetupRecipe,
    key: &TemplateKey,
    ws: &Workspace,
    opts: Opts,
) -> Result<PreparedSetup> {
    fp.require_git_source()?;
    crate::wire::check_version("setup recipe", recipe.schema_version)?;
    let runner = Runner::new(ctx, fp, ws, opts)?;
    runner.prepare_tools()?;
    let r = runner.run_recipe(recipe, key.digest())?;
    ctx.cancel.remove_containers(ctx);
    Ok(r)
}

/// Run task `task_id` of `fp` in `ws`, with `inputs` as the results of the jobs
/// it needs, and return what it produced. Jobs it does not hold are not
/// started, and `-j`/units are not looked at again.
///
/// The workspace is a checkout of the planned commit. `prepared` says which
/// install steps of the task were done in it already (see [`execute_setup`]);
/// those are skipped and every other one is run by the task. The receipt is
/// believed only when it was made on this commit, in this very workspace
/// (its id is in the workspace) and under the template key the task's recipe
/// has with the given [`TemplateContext`]; otherwise it is ignored, the task
/// does all its setup itself, and `receipt_ignored` of the result says why.
/// With `None` the task prepares everything itself. The workspace is left for the caller to
/// remove.
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
    prepared: Option<(&PreparedReceipt, &TemplateContext)>,
    ws: &Workspace,
    opts: Opts,
) -> Result<TaskResult> {
    fp.require_git_source()?;
    let task = fp
        .task(task_id)
        .ok_or_else(|| anyhow!("the plan has no task `{task_id}`"))?;
    crate::wire::check_version("task inputs", inputs.schema_version)?;
    if let Some((receipt, _)) = prepared {
        crate::wire::check_version("receipt", receipt.schema_version)?;
    }
    let keep = opts.keep;
    let runner = Runner::new(ctx, fp, ws, opts)?;
    // as in a run: an artifact step local-ci cannot run fails the task's jobs
    // before anything is started
    if let Some(r) = runner.refused_task(task) {
        return Ok(r);
    }
    let result = (|| {
        runner.prepare_tools()?;
        let lock = if runner.task_locked(task) {
            runner.lock().ok()
        } else {
            None
        };
        let r = runner.run_task(
            &ws.root,
            task,
            inputs,
            prepared,
            true,
            lock.is_some(),
            false,
        );
        drop(lock);
        r
    })();
    ctx.cancel.remove_containers(ctx);
    if !keep {
        runner.release_ports(&ws.root)?;
    }
    result
}
