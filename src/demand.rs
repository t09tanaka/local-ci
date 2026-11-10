//! What a task is going to ask of a machine, listed before it runs.
//!
//! A scheduler that admits commands against a machine's resources wants to
//! know the commands a task will start before it places the task. local-ci
//! cannot know all of them: an `if:`, a matrix that is an expression, an
//! install that a receipt may have done already, or an expression in a label
//! are settled only when the task runs. [`plan_task_demand`] lists the steps
//! that run a command, and marks the ones that may not run
//! (`conditional: true`) instead of guessing.
//!
//! The types hold nothing of a scheduler and nothing of the host: paths are
//! relative to the workspace, and the one place where a workspace path would
//! be in a command is a placeholder ([`SCRIPT`]).

use crate::frozen::FrozenPlan;
use crate::plan;
use crate::wire::{wire_version, Digested};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// In an `argv`: the file the step's script is in, which the host that runs
/// the step makes. Every step, the one that runs the tests of a shard too, is
/// started as its shell run on that file (that is what the executor does: the
/// shard's command is the script).
pub const SCRIPT: &str = "{script}";

/// How much a step asks of a machine, as far as the plan says.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WeightHint {
    /// The job starts containers or test servers that must not share a
    /// machine's ports ([`crate::plan::is_exclusive`]).
    pub exclusive: bool,
    /// Services the job starts.
    pub services: usize,
    /// The step's own `timeout-minutes`.
    pub timeout_minutes: Option<f64>,
    /// The tests of the shard, for the step that runs them.
    pub tests: Option<usize>,
}

/// One command a task starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DemandNode {
    /// Names the step within the task: `<job>#<index of the step>`.
    pub step_id: String,
    pub job: String,
    /// Index in the steps of the job.
    pub step: usize,
    /// The step's label as written: it may hold an expression.
    pub label: String,
    /// The working directory relative to the workspace (`""` for its root):
    /// in its plain form, never absolute, never with `..`.
    pub cwd: String,
    /// The command line the executor starts: the step's shell with
    /// [`SCRIPT`] in place of the file of the script. The step that runs the
    /// tests of a shard is listed the same way (its script is the command of
    /// the runner's adapter, built when the task runs; `weight.tests` says how
    /// many tests it has).
    pub argv: Vec<String>,
    /// The key the step's history is kept under (what a run hands an
    /// executor as the `key` of the step). `None` when the label holds an
    /// expression, which is read only when the step runs.
    pub key: Option<String>,
    pub weight: WeightHint,
    /// The nodes that run before this one: the one before it in the task.
    pub depends_on: Vec<String>,
    /// The step may not run, or not as listed: an `if:` (of the step or the
    /// job), a matrix that is an expression, a step a receipt may have done,
    /// or an install that is skipped where its tool is missing.
    pub conditional: bool,
}

/// The commands of a task, in the order they run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDemand {
    /// See [`crate::wire::WIRE_SCHEMA_VERSION`].
    #[serde(default = "wire_version")]
    pub schema_version: u32,
    pub task_id: String,
    pub nodes: Vec<DemandNode>,
}

impl Digested for TaskDemand {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

/// The key a step's history is kept under: the workflow file (as `local-ci`
/// names it in output), the job and the step's label.
pub(crate) fn history_key(workflows_dir: &str, workflow: &str, job: &str, label: &str) -> String {
    format!(
        "local-ci {} {job} {label}",
        crate::report::short(workflows_dir, workflow)
    )
}

fn shell_argv(shell: Option<&str>) -> Result<Vec<String>> {
    let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect();
    Ok(match shell.map(str::trim) {
        None | Some("bash") => words(&["bash", "--noprofile", "--norc", "-eo", "pipefail", SCRIPT]),
        Some("sh") => words(&["sh", "-e", SCRIPT]),
        Some("python") => words(&["python3", SCRIPT]),
        Some(custom) if custom.contains("{0}") => shlex::split(&custom.replace("{0}", SCRIPT))
            .ok_or_else(|| anyhow!("bad shell `{custom}`"))?,
        Some(other) => bail!("unsupported shell `{other}`"),
    })
}

/// A working directory of the plan as the workspace-relative path it is:
/// nothing absolute, nothing with `..` or a NUL or a backslash. `a//b` and
/// `./a` are read as `a/b` and `a`.
pub(crate) fn workspace_dir(what: &str, dir: &str) -> Result<String> {
    crate::artifact::normalize_rel(dir)
        .map_err(|e| anyhow!("{what} {dir:?} is not a directory below the workspace: {e}"))
}

/// The commands task `task_id` of `fp` starts, as far as the plan tells.
///
/// A step that runs nothing (a `uses:` step, a step that only pushes or
/// deploys) is not listed. Fails for a task the plan does not have, a shell
/// that cannot be run, and a shard that has no command (the task would not
/// run either).
pub fn plan_task_demand(fp: &FrozenPlan, task_id: &str) -> Result<TaskDemand> {
    let task = fp
        .task(task_id)
        .ok_or_else(|| anyhow!("the plan has no task `{task_id}`"))?;
    let plan = fp.to_plan();
    let wi = plan
        .selected
        .iter()
        .position(|s| s.wf.file == task.workflow)
        .ok_or_else(|| anyhow!("the plan has no workflow {}", task.workflow))?;
    let w = &plan.selected[wi].wf;
    // the steps a receipt may have done already
    let recipe = crate::setup::derive_in(&plan, &plan::hoist(&plan), task)?;
    let prepared: BTreeSet<(&str, usize)> = recipe
        .steps
        .iter()
        .flat_map(|s| &s.at)
        .filter(|a| a.workflow == task.workflow)
        .map(|a| (a.job.as_str(), a.index))
        .collect();
    if let Some(ts) = &task.shard {
        workspace_dir("the shard's working directory", &ts.dir)?;
    }
    let mut nodes: Vec<DemandNode> = vec![];
    let mut shard_step = false;
    for id in &task.jobs {
        let job = w
            .jobs
            .iter()
            .find(|j| &j.id == id)
            .ok_or_else(|| anyhow!("the task has job `{id}`, which its workflow has not"))?;
        let exclusive = plan::is_exclusive(job);
        for (n, s) in job.steps.iter().enumerate() {
            let Some(run) = s.run.as_ref().filter(|_| s.uses.is_none()) else {
                continue;
            };
            let shard = task.shard.as_ref().filter(|t| t.step == n);
            shard_step |= shard.is_some();
            if crate::script::remote_effect(run) {
                continue;
            }
            let label = s.label();
            if let Some(ts) = shard {
                // the step runs where the tests were listed
                if plan::step_dir(w, job, s) != ts.dir {
                    bail!(
                        "task {}: the working directory of step {n} is not {:?}, where its tests were listed",
                        task.id,
                        ts.dir
                    );
                }
                // the shard's command must be one the step can run
                crate::exec::shard_script(ts, std::path::Path::new(""))
                    .map_err(|e| anyhow!("task {}: {e}", task.id))?;
            }
            let argv = shell_argv(plan::resolved_shell(w, job, s).as_deref())?;
            let cwd = workspace_dir("the working directory", &plan::step_dir(w, job, s))?;
            let dynamic_label = label.contains("${{");
            let step_id = format!("{id}#{n}");
            nodes.push(DemandNode {
                depends_on: nodes
                    .last()
                    .map(|p| p.step_id.clone())
                    .into_iter()
                    .collect(),
                step_id,
                job: id.clone(),
                step: n,
                cwd,
                argv,
                key: (!dynamic_label)
                    .then(|| history_key(&fp.config.default_workflows_dir, &w.file, id, &label)),
                label,
                weight: WeightHint {
                    exclusive,
                    services: job.services.len(),
                    timeout_minutes: s.timeout_minutes,
                    tests: shard.map(|t| t.tests.len()),
                },
                conditional: s.cond.is_some()
                    || job.cond.is_some()
                    || task.dynamic_matrix
                    || prepared.contains(&(id.as_str(), n))
                    || crate::script::global_install(run, &|_| false)
                        == crate::script::Install::Needed,
            });
        }
    }
    if let Some(ts) = &task.shard {
        if !shard_step {
            bail!(
                "task {} is a shard of a split job, but step {} of its job runs no command",
                task.id,
                ts.step
            );
        }
    }
    Ok(TaskDemand {
        schema_version: wire_version(),
        task_id: task.id.clone(),
        nodes,
    })
}
