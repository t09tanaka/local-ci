//! The job graph of a plan and the tasks it is run as.
//!
//! [`JobGraph`] is the workflow's jobs with their `needs` edges, a matrix
//! expanded into one node per variant plus the node later jobs `needs`
//! (the aggregation point). [`ExecutionTask`] is what can be placed on a host:
//! by default one task per job and one per matrix variant, following the
//! independent-job semantics of GitHub Actions (no files shared between jobs).
//! Jobs that the engine's unit keeps together because they write the same
//! files stay one task, and the task says why ([`GroupedReason`]).
//!
//! Nothing here runs anything; see [`crate::execute_task`].

use crate::artifact;
use crate::expr::NeedsRef;
use crate::plan::{self, Plan};
use crate::shard::{Discovered, RunnerInfo, ShardPlan, SplitMode};
use crate::wire::{JobSpec, MatrixValue, UnitSpec, WorkflowSpec};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// `<workflow file>::<job id>`; a matrix variant adds `#<index>`.
pub fn node_id(workflow: &str, job: &str) -> String {
    format!("{workflow}::{job}")
}

fn variant_id(workflow: &str, job: &str, index: usize) -> String {
    format!("{workflow}::{job}#{index}")
}

/// How the outputs of a matrix job's variants become the outputs of the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputAggregation {
    /// In the order of the matrix, whatever the order the variants finished
    /// in; for a name set by several variants the last one in matrix order
    /// wins. (GitHub takes the variant that finished last, which no one can
    /// plan for.)
    MatrixOrder,
    /// The shards of a split test job: each test once, whatever the shard it
    /// ran in; a test that was not reported, or twice, is a problem
    /// ([`crate::complete::aggregate_shard_group`]).
    TestIdentity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeKind {
    /// A job without a matrix.
    Job,
    /// A job whose matrix is an expression: its variants are known only when
    /// the jobs it needs have finished, so the node is one task that expands
    /// them when it runs.
    DynamicMatrix { expression: String },
    /// One variant of a matrix that is known at planning.
    MatrixVariant {
        /// The node later jobs depend on.
        group: String,
        index: usize,
        values: Map<String, Value>,
        label: String,
    },
    /// The point where a matrix job's variants meet: later jobs depend on
    /// this node, and it is done when every variant is.
    MatrixGroup {
        variants: Vec<String>,
        fail_fast: bool,
        max_parallel: Option<usize>,
        aggregation: OutputAggregation,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobNode {
    pub id: String,
    /// The workflow file.
    pub workflow: String,
    pub job: String,
    #[serde(flatten)]
    pub kind: NodeKind,
    /// Nodes that must be done first.
    pub depends_on: Vec<String>,
}

/// The jobs of the plan as a DAG. `nodes` are in dependency order (a node
/// comes after everything it depends on), otherwise in plan order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobGraph {
    pub nodes: Vec<JobNode>,
}

impl JobGraph {
    pub fn node(&self, id: &str) -> Option<&JobNode> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

/// What a job's matrix is at planning.
pub(crate) enum MatrixPlan {
    None,
    Dynamic(String),
    Static(Vec<Map<String, Value>>),
}

pub(crate) fn matrix_plan(job: &JobSpec) -> MatrixPlan {
    let Some(m) = &job.matrix else {
        return MatrixPlan::None;
    };
    match m {
        MatrixValue::Map(_) if !m.has_expression() => match crate::matrix::expand(&m.to_json()) {
            // `expand` never returns no variant: it fails instead
            Ok(c) => MatrixPlan::Static(c),
            Err(_) => MatrixPlan::Dynamic("matrix".into()),
        },
        MatrixValue::String(s) => MatrixPlan::Dynamic(s.clone()),
        _ => MatrixPlan::Dynamic("matrix".into()),
    }
}

/// The graph of the jobs `units` hold. Fails on a `needs` that names no job of
/// the plan and on a cycle; [`unrunnable_workflows`] tells which workflows
/// those are, so a plan can leave them out.
pub fn build_graph(workflows: &[WorkflowSpec], units: &[UnitSpec]) -> Result<JobGraph> {
    let mut nodes: Vec<JobNode> = Vec::new();
    for wi in 0..workflows.len() {
        nodes.extend(workflow_nodes(workflows, units, wi)?);
    }
    order(nodes)
}

/// The workflows (by index) that cannot be a job graph, each with why: a
/// `needs` of a job that is not in the plan, a cycle, or a matrix with two
/// keys that print alike. GitHub does not start such a workflow; the others
/// are not affected.
pub fn unrunnable_workflows(
    workflows: &[WorkflowSpec],
    units: &[UnitSpec],
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (wi, w) in workflows.iter().enumerate() {
        let why = w
            .jobs
            .iter()
            .find_map(|j| {
                let key = j.matrix.as_ref()?.duplicate_key()?;
                Some(format!(
                    "the matrix of job `{}` has two keys that print as `{key}`",
                    j.id
                ))
            })
            .or_else(|| {
                workflow_nodes(workflows, units, wi)
                    .and_then(order)
                    .err()
                    .map(|e| format!("{e:#}"))
            });
        if let Some(why) = why {
            out.push((wi, why));
        }
    }
    out
}

/// The nodes of workflow `wi`, in the order of its jobs.
fn workflow_nodes(
    workflows: &[WorkflowSpec],
    units: &[UnitSpec],
    wi: usize,
) -> Result<Vec<JobNode>> {
    let w = &workflows[wi];
    let mut nodes: Vec<JobNode> = Vec::new();
    let planned: BTreeSet<&str> = units
        .iter()
        .filter(|u| u.workflow == wi)
        .flat_map(|u| u.jobs.iter().map(String::as_str))
        .collect();
    for job in w.jobs.iter().filter(|j| planned.contains(j.id.as_str())) {
        let mut deps = Vec::new();
        for n in &job.needs {
            if !planned.contains(n.as_str()) {
                bail!(
                    "job `{}` of {} needs `{n}`, which is not a job of the plan",
                    job.id,
                    w.file
                );
            }
            deps.push(node_id(&w.file, n));
        }
        let id = node_id(&w.file, &job.id);
        let mk = |id: String, kind: NodeKind, depends_on: Vec<String>| JobNode {
            id,
            workflow: w.file.clone(),
            job: job.id.clone(),
            kind,
            depends_on,
        };
        match matrix_plan(job) {
            MatrixPlan::None => nodes.push(mk(id, NodeKind::Job, deps)),
            MatrixPlan::Dynamic(expression) => {
                nodes.push(mk(id, NodeKind::DynamicMatrix { expression }, deps))
            }
            MatrixPlan::Static(combos) => {
                let mut ids = Vec::new();
                for (i, c) in combos.into_iter().enumerate() {
                    let vid = variant_id(&w.file, &job.id, i);
                    ids.push(vid.clone());
                    let label = crate::matrix::describe(&c);
                    nodes.push(mk(
                        vid,
                        NodeKind::MatrixVariant {
                            group: id.clone(),
                            index: i,
                            values: c,
                            label,
                        },
                        deps.clone(),
                    ));
                }
                nodes.push(mk(
                    id,
                    NodeKind::MatrixGroup {
                        variants: ids.clone(),
                        fail_fast: job.fail_fast,
                        max_parallel: job.max_parallel,
                        aggregation: OutputAggregation::MatrixOrder,
                    },
                    ids,
                ));
            }
        }
    }
    Ok(nodes)
}

/// Dependency order, keeping plan order where it allows; a cycle is an error.
fn order(nodes: Vec<JobNode>) -> Result<JobGraph> {
    let mut done: Vec<JobNode> = Vec::with_capacity(nodes.len());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut rest = nodes;
    while !rest.is_empty() {
        let at = rest
            .iter()
            .position(|n| n.depends_on.iter().all(|d| seen.contains(d)));
        match at {
            Some(i) => {
                let n = rest.remove(i);
                seen.insert(n.id.clone());
                done.push(n);
            }
            None => {
                let names: Vec<String> = rest
                    .iter()
                    .filter(|n| !matches!(n.kind, NodeKind::MatrixVariant { .. }))
                    .map(|n| n.id.clone())
                    .collect();
                bail!(
                    "the jobs depend on each other in a cycle: {}",
                    names.join(", ")
                );
            }
        }
    }
    Ok(JobGraph { nodes: done })
}

// ---- tasks ----

/// Why jobs that could run apart are one task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum GroupedReason {
    /// They write the same files or build output, so they must not run at
    /// the same time in one checkout, and splitting them would change what
    /// they read.
    SharedOutputs { files: Vec<String> },
    /// Their shared files would put a job between two of them that depends on
    /// the first and is needed by the second: the whole group stays together.
    OrderAroundSharedOutputs,
}

/// Where a task's files live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspacePolicy {
    /// A checkout of the planned commit of its own; nothing is shared with
    /// another task (files cross jobs as artifacts, within one workflow run:
    /// see [`crate::artifact_io`]).
    pub own_checkout: bool,
    /// The jobs of the task share that checkout.
    pub shared_by_jobs: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskService {
    pub job: String,
    pub id: String,
    pub image: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Times the task may be started for one run. A local run starts it once.
    pub max_attempts: u32,
}

/// What the result of the task has to contain to be accepted: what its jobs
/// declare (checked by [`crate::verify_task_result`] and
/// [`crate::validate_run_result`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputVerification {
    /// `<job>.<output>` for each output the jobs declare.
    pub job_outputs: Vec<String>,
    /// The artifacts the task must publish: a literal name uploaded by a step
    /// that always runs and fails without files
    /// ([`crate::artifact::required_upload`]). Each must be among the artifacts
    /// uploaded by the task's own jobs.
    pub artifacts: Vec<String>,
}

/// An output of a job this task needs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NeedsOutput {
    pub job: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskVariant {
    /// The node of the matrix job.
    pub group: String,
    pub index: usize,
    pub values: Map<String, Value>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionTask {
    pub id: String,
    /// The workflow file.
    pub workflow: String,
    /// The job ids, in the order they run.
    pub jobs: Vec<String>,
    /// The graph nodes the task runs.
    pub nodes: Vec<String>,
    /// The engine unit this came from (index into the plan's units).
    pub unit: usize,
    pub variant: Option<TaskVariant>,
    /// The job's matrix is an expression: the task expands and runs it all.
    pub dynamic_matrix: bool,
    /// Tasks whose results (and outputs) are needed before this one starts.
    pub depends_on: Vec<String>,
    /// Outputs of jobs outside the task that its expressions read.
    pub needs_outputs: Vec<NeedsOutput>,
    /// The artifacts the task's jobs download by name
    /// ([`crate::artifact::needed_download`]), of the same workflow run.
    pub artifacts: Vec<String>,
    pub workspace: WorkspacePolicy,
    pub services: Vec<TaskService>,
    pub exclusive: bool,
    pub retry: RetryPolicy,
    pub verification: OutputVerification,
    pub grouped_reason: Option<GroupedReason>,
    /// The part of a split job this task runs.
    #[serde(default)]
    pub shard: Option<TaskShard>,
}

/// Which part of a split job a task runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskShard {
    /// The shard group (the id of the job's task before the split).
    pub group: String,
    pub index: usize,
    pub count: usize,
    pub mode: SplitMode,
    /// The tests of this shard (empty for the runner's own shard feature).
    pub tests: Vec<String>,
}

/// `<workflow file>::<job id>@<index>`: the task of one shard of a split job.
pub fn shard_task_id(workflow: &str, job: &str, index: usize) -> String {
    format!("{workflow}::{job}@{index}")
}

/// A request to split one job, made by the caller from a listing and a history
/// (the plan cannot run a runner). Nothing is split without one.
#[derive(Debug, Clone, PartialEq)]
pub struct ShardRequest {
    /// The workflow file.
    pub workflow: String,
    pub job: String,
    pub plan: ShardPlan,
    /// What the runner listed; the shards are checked against it.
    pub discovered: Discovered,
    /// A failed shard cancels the shards that have not finished.
    pub fail_fast: bool,
}

/// Why a task is not split into shards of one test run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotSplit {
    /// No runner adapter can list and partition its tests yet.
    NoRunnerAdapter,
    /// Its jobs share a checkout.
    SharedWorkspace,
    /// It starts services that one shard cannot get a copy of.
    HasServices,
    /// It runs one variant of a matrix; the matrix is the split.
    MatrixVariant,
    /// A runner adapter knows the job and the split was not asked for.
    NotRequested,
    /// The runner cannot list its tests.
    NoEnumeration,
    /// The runner's listing failed.
    DiscoveryFailed,
    /// The listing is not all the tests (they are generated at run time) and
    /// the runner has no shard feature of its own.
    IncompleteDiscovery,
    /// A run cannot be told which tests to run.
    NoShardTarget,
    /// The shards would update state they share.
    SharedState,
    /// A database, port or container cannot be given to each shard.
    NoIsolation,
    /// The shards' results do not merge to the verdict of the whole job.
    InexactAggregation,
    /// Too few tests, or too little time, for a second shard to pay.
    TooSmall,
    /// The shards do not hold the listed tests exactly once.
    ManifestMismatch,
}

/// A job (or the jobs of a task) that could become several shards, and the
/// reasons it did not. A task is split only on a [`ShardRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShardGroup {
    pub id: String,
    pub workflow: String,
    pub source_jobs: Vec<String>,
    /// The tasks the source is run as (one, when nothing is split).
    pub shards: Vec<String>,
    pub split: bool,
    pub not_split: Vec<NotSplit>,
    /// How shard results are merged (none when not split).
    pub aggregation: Option<OutputAggregation>,
    /// The runner of a split job.
    #[serde(default)]
    pub runner: Option<RunnerInfo>,
    /// The tests the shards hold together, once each (empty when the runner's
    /// own shard feature divides them).
    #[serde(default)]
    pub manifest: Vec<String>,
    /// A failed shard cancels the others.
    #[serde(default)]
    pub fail_fast: bool,
}

/// The outputs of other jobs that `job` reads, found by reading its
/// expressions. An output named by `needs.x.outputs.y`, `needs['x'].outputs['y']`
/// and the like is that one; `needs.x.outputs` taken whole or `toJSON(needs)`
/// count as every output the job needed declares, since the plan cannot say
/// which are read.
fn needs_outputs_of(job: &JobSpec, wf: &WorkflowSpec) -> Vec<NeedsOutput> {
    fn strings(v: &Value, bare: bool, out: &mut Vec<(String, bool)>) {
        match v {
            Value::String(s) => out.push((s.clone(), bare)),
            Value::Array(a) => a.iter().for_each(|e| strings(e, false, out)),
            Value::Object(m) => {
                for (k, e) in m {
                    strings(e, k == "cond", out);
                }
            }
            _ => {}
        }
    }
    let mut texts = Vec::new();
    strings(
        &serde_json::to_value(job).unwrap_or_default(),
        false,
        &mut texts,
    );
    let declared = |id: &str| -> Vec<NeedsOutput> {
        wf.job(id)
            .map(|j| {
                j.outputs
                    .iter()
                    .map(|(name, _)| NeedsOutput {
                        job: id.to_string(),
                        name: name.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = BTreeSet::new();
    for (text, bare) in texts {
        for r in crate::expr::needs_refs(&text, bare) {
            match r {
                NeedsRef::Output { job, name } => {
                    out.insert(NeedsOutput { job, name });
                }
                NeedsRef::JobOutputs(id) => out.extend(declared(&id)),
                NeedsRef::All => job.needs.iter().for_each(|n| out.extend(declared(n))),
            }
        }
    }
    out.into_iter().collect()
}

/// Cut the plan's units into tasks. `plan` is the engine's view (the file
/// overlap is worked out on it); `specs` and `units` are the wire view of the
/// same plan.
pub(crate) fn derive_tasks(
    plan: &Plan,
    specs: &[WorkflowSpec],
    units: &[UnitSpec],
    requests: &[ShardRequest],
) -> (Vec<ExecutionTask>, Vec<ShardGroup>) {
    let mut tasks: Vec<ExecutionTask> = Vec::new();
    for (ui, u) in units.iter().enumerate() {
        let wf = &specs[u.workflow];
        let w = &plan.selected[u.workflow].wf;
        let pos: HashMap<&str, usize> = u
            .jobs
            .iter()
            .enumerate()
            .map(|(i, j)| (j.as_str(), i))
            .collect();
        let writes: Vec<BTreeSet<String>> = u
            .jobs
            .iter()
            .map(|id| {
                let j = w.jobs.iter().find(|j| &j.id == id).expect("job in plan");
                plan::writes(w, j)
            })
            .collect();
        // components of jobs that share written files
        let n = u.jobs.len();
        let mut comp: Vec<usize> = (0..n).collect();
        fn root(c: &[usize], mut i: usize) -> usize {
            while c[i] != i {
                i = c[i];
            }
            i
        }
        for a in 0..n {
            for b in (a + 1)..n {
                if !writes[a].is_disjoint(&writes[b]) {
                    let (x, y) = (root(&comp, a), root(&comp, b));
                    comp[x] = y;
                }
            }
        }
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut seen_root: Vec<usize> = Vec::new();
        for i in 0..n {
            let r = root(&comp, i);
            match seen_root.iter().position(|x| *x == r) {
                Some(g) => groups[g].push(i),
                None => {
                    seen_root.push(r);
                    groups.push(vec![i]);
                }
            }
        }
        // Tasks run in the order of the unit (the workflow's order, with each
        // job after the jobs it needs). When cutting the unit into groups would
        // run a job earlier or later than that (a group around a job that is not
        // part of it, or groups that need each other in a cycle), the unit stays
        // one task in its own order.
        let mut merged = false;
        let in_unit_order = |groups: &[Vec<usize>], order: &[usize]| {
            order
                .iter()
                .flat_map(|&g| groups[g].iter().copied())
                .eq(0..n)
        };
        let mut ordered = order_groups(wf, u, &groups, &pos).filter(|o| in_unit_order(&groups, o));
        if ordered.is_none() {
            groups = vec![(0..n).collect()];
            merged = true;
            ordered = Some(vec![0]);
        }
        let ordered = ordered.unwrap();
        for gi in ordered {
            let g = &groups[gi];
            let ids: Vec<String> = g.iter().map(|&i| u.jobs[i].clone()).collect();
            let specs_of: Vec<&JobSpec> = ids
                .iter()
                .map(|id| wf.job(id).expect("job in plan"))
                .collect();
            let grouped_reason = (g.len() > 1).then(|| {
                if merged {
                    GroupedReason::OrderAroundSharedOutputs
                } else {
                    let mut count: BTreeMap<&String, usize> = BTreeMap::new();
                    for &i in g {
                        for f in &writes[i] {
                            *count.entry(f).or_default() += 1;
                        }
                    }
                    GroupedReason::SharedOutputs {
                        files: count
                            .into_iter()
                            .filter(|(_, c)| *c > 1)
                            .map(|(f, _)| f.clone())
                            .collect(),
                    }
                }
            });
            let base = ExecutionTask {
                id: String::new(),
                workflow: wf.file.clone(),
                jobs: ids.clone(),
                nodes: vec![],
                unit: ui,
                variant: None,
                dynamic_matrix: false,
                depends_on: vec![],
                needs_outputs: vec![],
                artifacts: specs_of
                    .iter()
                    .flat_map(|j| &j.steps)
                    .filter_map(|s| artifact::needed_download(s.uses.as_deref()?, &s.with))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                workspace: WorkspacePolicy {
                    own_checkout: true,
                    shared_by_jobs: g.len() > 1,
                },
                services: specs_of
                    .iter()
                    .flat_map(|j| {
                        j.services.iter().map(|s| TaskService {
                            job: j.id.clone(),
                            id: s.id.clone(),
                            image: s.image.clone(),
                        })
                    })
                    .collect(),
                exclusive: g.iter().any(|&i| {
                    let j = w.jobs.iter().find(|j| j.id == u.jobs[i]).unwrap();
                    plan::is_exclusive(j)
                }),
                retry: RetryPolicy { max_attempts: 1 },
                verification: OutputVerification {
                    job_outputs: specs_of
                        .iter()
                        .flat_map(|j| j.outputs.iter().map(|(k, _)| format!("{}.{k}", j.id)))
                        .collect(),
                    artifacts: specs_of
                        .iter()
                        .filter(|j| j.matrix.is_none())
                        .flat_map(|j| &j.steps)
                        .filter_map(|s| {
                            artifact::required_upload(
                                s.uses.as_deref()?,
                                &s.with,
                                s.cond.as_deref().is_some_and(|c| !artifact::always_true(c))
                                    || s.continue_on_error,
                            )
                        })
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                },
                grouped_reason,
                shard: None,
            };
            let external: BTreeSet<&str> = specs_of
                .iter()
                .flat_map(|j| j.needs.iter().map(String::as_str))
                .filter(|n| !ids.iter().any(|i| i == n))
                .collect();
            let needs_outputs: Vec<NeedsOutput> = specs_of
                .iter()
                .flat_map(|j| needs_outputs_of(j, wf))
                .filter(|o| external.contains(o.job.as_str()))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let single = (g.len() == 1).then(|| specs_of[0]);
            match single.map(|j| (j, matrix_plan(j))) {
                Some((j, MatrixPlan::Static(combos))) => {
                    for (i, c) in combos.into_iter().enumerate() {
                        let id = variant_id(&wf.file, &j.id, i);
                        tasks.push(ExecutionTask {
                            id: id.clone(),
                            nodes: vec![id],
                            variant: Some(TaskVariant {
                                group: node_id(&wf.file, &j.id),
                                index: i,
                                label: crate::matrix::describe(&c),
                                values: c,
                            }),
                            needs_outputs: needs_outputs.clone(),
                            ..base.clone()
                        });
                    }
                }
                other => {
                    let dynamic = matches!(other, Some((_, MatrixPlan::Dynamic(_))));
                    let id = format!("{}::{}", wf.file, ids.join("+"));
                    let split = other
                        .as_ref()
                        .filter(|(_, m)| matches!(m, MatrixPlan::None))
                        .and_then(|(j, _)| request_for(requests, &wf.file, &j.id))
                        .filter(|r| structural_reasons(&base).is_empty() && manifest_ok(r));
                    if let Some(r) = split.filter(|r| r.plan.is_split()) {
                        let count = r.plan.shards.len();
                        for s in &r.plan.shards {
                            tasks.push(ExecutionTask {
                                id: shard_task_id(&wf.file, &ids[0], s.index),
                                nodes: ids.iter().map(|j| node_id(&wf.file, j)).collect(),
                                needs_outputs: needs_outputs.clone(),
                                shard: Some(TaskShard {
                                    group: id.clone(),
                                    index: s.index,
                                    count,
                                    mode: r.plan.mode,
                                    tests: s.tests.clone(),
                                }),
                                ..base.clone()
                            });
                        }
                        continue;
                    }
                    tasks.push(ExecutionTask {
                        id,
                        nodes: ids.iter().map(|j| node_id(&wf.file, j)).collect(),
                        dynamic_matrix: dynamic,
                        needs_outputs,
                        ..base
                    });
                }
            }
        }
    }
    // dependencies between tasks
    let mut by_job: HashMap<(String, String), Vec<String>> = HashMap::new();
    for t in &tasks {
        for j in &t.jobs {
            by_job
                .entry((t.workflow.clone(), j.clone()))
                .or_default()
                .push(t.id.clone());
        }
    }
    for t in &mut tasks {
        let wf = specs.iter().find(|w| w.file == t.workflow).unwrap();
        let mut deps = BTreeSet::new();
        for j in &t.jobs {
            for n in &wf.job(j).unwrap().needs {
                if t.jobs.contains(n) {
                    continue;
                }
                if let Some(ids) = by_job.get(&(t.workflow.clone(), n.clone())) {
                    deps.extend(ids.iter().cloned());
                }
            }
        }
        t.depends_on = deps.into_iter().collect();
    }
    let mut shard_groups = Vec::new();
    for t in &tasks {
        let request = |t: &ExecutionTask| {
            (t.jobs.len() == 1)
                .then(|| request_for(requests, &t.workflow, &t.jobs[0]))
                .flatten()
        };
        if let Some(s) = &t.shard {
            if s.index == 0 {
                let r = request(t).expect("a shard comes from a request");
                shard_groups.push(ShardGroup {
                    id: s.group.clone(),
                    workflow: t.workflow.clone(),
                    source_jobs: t.jobs.clone(),
                    shards: tasks
                        .iter()
                        .filter(|o| o.shard.as_ref().is_some_and(|x| x.group == s.group))
                        .map(|o| o.id.clone())
                        .collect(),
                    split: true,
                    not_split: vec![],
                    aggregation: Some(OutputAggregation::TestIdentity),
                    runner: Some(r.plan.runner.clone()),
                    manifest: manifest_of(r),
                    fail_fast: r.fail_fast,
                });
            }
            continue;
        }
        let mut not_split = structural_reasons(t);
        let asked = request(t).filter(|_| t.variant.is_none());
        match asked {
            None => not_split.insert(0, NotSplit::NoRunnerAdapter),
            Some(r) => {
                if t.dynamic_matrix {
                    not_split.push(NotSplit::MatrixVariant);
                }
                not_split.extend(r.plan.not_split.iter().cloned());
                if !manifest_ok(r) {
                    not_split.push(NotSplit::ManifestMismatch);
                }
            }
        }
        shard_groups.push(ShardGroup {
            id: t.id.clone(),
            workflow: t.workflow.clone(),
            source_jobs: t.jobs.clone(),
            shards: vec![t.id.clone()],
            split: false,
            not_split,
            aggregation: None,
            runner: asked.map(|r| r.plan.runner.clone()),
            manifest: vec![],
            fail_fast: false,
        });
    }
    (tasks, shard_groups)
}

fn request_for<'a>(
    requests: &'a [ShardRequest],
    workflow: &str,
    job: &str,
) -> Option<&'a ShardRequest> {
    requests
        .iter()
        .find(|r| r.workflow == workflow && r.job == job)
}

/// What about the task itself keeps it from being split.
fn structural_reasons(t: &ExecutionTask) -> Vec<NotSplit> {
    let mut out = Vec::new();
    if t.workspace.shared_by_jobs {
        out.push(NotSplit::SharedWorkspace);
    }
    if !t.services.is_empty() {
        out.push(NotSplit::HasServices);
    }
    if t.variant.is_some() {
        out.push(NotSplit::MatrixVariant);
    }
    out
}

/// The shards of the request hold the tests it lists, each once.
fn manifest_ok(r: &ShardRequest) -> bool {
    r.plan.verify(&r.discovered).is_ok()
}

fn manifest_of(r: &ShardRequest) -> Vec<String> {
    r.plan
        .shards
        .iter()
        .flat_map(|s| s.tests.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The groups in an order where each comes after the groups it needs, the
/// lowest first job first; `None` when they need each other in a cycle.
fn order_groups(
    wf: &WorkflowSpec,
    u: &UnitSpec,
    groups: &[Vec<usize>],
    pos: &HashMap<&str, usize>,
) -> Option<Vec<usize>> {
    let group_of = |job: &str| {
        groups
            .iter()
            .position(|g| g.iter().any(|&i| u.jobs[i] == job))
    };
    let deps: Vec<BTreeSet<usize>> = groups
        .iter()
        .enumerate()
        .map(|(gi, g)| {
            g.iter()
                .flat_map(|&i| wf.job(&u.jobs[i]).unwrap().needs.iter())
                .filter(|n| pos.contains_key(n.as_str()))
                .filter_map(|n| group_of(n))
                .filter(|d| *d != gi)
                .collect()
        })
        .collect();
    let mut done: Vec<usize> = Vec::new();
    while done.len() < groups.len() {
        let next = (0..groups.len())
            .filter(|g| !done.contains(g) && deps[*g].iter().all(|d| done.contains(d)))
            .min_by_key(|g| groups[*g][0])?;
        done.push(next);
    }
    Some(done)
}
