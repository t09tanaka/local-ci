//! The plan as data: what was decided, fixed so it can be stored, sent to
//! another host and run there without deciding again.
//!
//! A [`FrozenPlan`] holds no path of the machine that made it: files are
//! relative to the repository root and the repository is named by its slug.
//! Its `digest` is the SHA-256 of the plan in a canonical form (object keys
//! sorted, no whitespace), so the same decisions always give the same digest,
//! wherever the repository was cloned.

use crate::dag::{self, ExecutionTask, JobGraph, ShardGroup, ShardRequest};
use crate::plan::Plan;
use crate::shard::{Basis, Discovered, Shard, ShardPlan};
use crate::wire::{JobSpec, UnitSpec, WorkflowSpec};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Bumped when the shape of a [`FrozenPlan`] changes incompatibly.
pub const SCHEMA_VERSION: u32 = 5;

/// What the plan was made from: a [`crate::source::SourceManifest`] by its
/// id, the commit the jobs run against, and the ref the changed files were
/// taken against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    /// `SourceManifest::source_id` of the captured input. A run of a plain
    /// commit (nothing captured) names itself `commit:<sha>`.
    pub source_id: String,
    /// The full commit sha the jobs run against: the commit itself, or the
    /// synthetic commit made from a captured worktree.
    pub execution_commit: String,
    /// The base the changed files were taken against (a ref or a sha).
    pub base: String,
}

/// `.local-ci.toml` as it applied to the plan, plus the workflow directories
/// that were read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenConfig {
    /// Every directory the workflows were read from, relative to the root.
    pub workflow_dirs: Vec<String>,
    /// The directory output leaves off workflow paths.
    pub default_workflows_dir: String,
    /// Workflows or jobs whose results are never reused.
    pub no_cache: Vec<String>,
    /// Copies of the worktree link to setup's `node_modules`.
    pub link_node_modules: bool,
    /// `env:` names left out of the keys of setup steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_env: Vec<String>,
}

/// The selection the caller asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filters {
    /// Everything runs, whatever the changes.
    pub all: bool,
    pub include_disabled: bool,
    /// Extra workflow directories (normalized, relative to the root).
    pub workflows_dirs: Vec<String>,
    /// Only these workflows (file name suffix or name); empty for all.
    pub workflows: Vec<String>,
    /// Only these jobs and what they need; empty for all.
    pub jobs: Vec<String>,
}

/// A workflow that was not planned, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedWorkflow {
    pub workflow: String,
    pub reason: String,
    /// The workflow was selected but cannot be run (a dependency cycle, a
    /// `needs` of a job that does not exist, a matrix with keys that print
    /// alike), as opposed to one the changes do not trigger. A run that left
    /// one out is not a complete run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unrunnable: bool,
}

/// A workflow GitHub does not run, as the plan was told.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DisabledWorkflow {
    /// Path relative to the repository root.
    pub path: String,
    /// `disabled_manually`, `disabled_inactivity`, ...
    pub state: String,
}

/// Where the list of disabled workflows came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisabledSource {
    /// Not asked: every workflow was taken into account (`--all` or
    /// `--include-disabled`).
    NotChecked,
    /// The caller gave the list.
    Provided,
    /// `gh api` answered.
    Github,
    /// `gh` or the network was not there: nothing counts as disabled.
    Unavailable,
}

/// The list of disabled workflows the plan was made with, and its source.
/// Planning with the same record gives the same plan; the record is how a
/// plan made with the network can be made again without it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisabledRecord {
    pub source: DisabledSource,
    /// Sorted by path.
    pub workflows: Vec<DisabledWorkflow>,
}

impl DisabledRecord {
    pub fn not_checked() -> Self {
        DisabledRecord {
            source: DisabledSource::NotChecked,
            workflows: vec![],
        }
    }
}

/// A plan, fixed. Build one with [`crate::build_frozen_plan`]; run it with
/// [`crate::execute_plan`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenPlan {
    pub schema_version: u32,
    /// `sha256:<hex>` of the rest of the plan in canonical form.
    pub digest: String,
    /// `owner/name` of the `origin` remote, when it has one.
    pub repository: Option<String>,
    pub source: SourceRef,
    /// The branch the commit is the head of (`HEAD` runs only).
    pub branch: Option<String>,
    /// Files changed between base and commit, relative to the root.
    pub changed_files: Vec<String>,
    /// `sha256:<hex>` of `config`.
    pub config_digest: String,
    pub config: FrozenConfig,
    pub filters: Filters,
    /// The workflows that were selected, with the event they run as.
    pub workflows: Vec<WorkflowSpec>,
    pub skipped: Vec<SkippedWorkflow>,
    pub disabled: DisabledRecord,
    /// How a local run groups, schedules and reports jobs.
    pub units: Vec<UnitSpec>,
    /// The jobs and what they wait for.
    pub graph: JobGraph,
    /// What can be placed on a host, in an order each task can run after the
    /// ones before it.
    pub tasks: Vec<ExecutionTask>,
    pub shard_groups: Vec<ShardGroup>,
}

/// Everything a plan is made of except the two digests.
pub struct PlanParts {
    pub repository: Option<String>,
    pub source: SourceRef,
    pub branch: Option<String>,
    pub changed_files: Vec<String>,
    pub config: FrozenConfig,
    pub filters: Filters,
    pub disabled: DisabledRecord,
    pub plan: Plan,
}

/// Take the workflows `broken` (index, reason) out of `plan` and note them as
/// skipped. The units follow the workflows left, and the installs the plan
/// hoists are worked out again from them.
fn drop_workflows(plan: &mut Plan, broken: &[(usize, String)]) {
    let gone = |i: usize| broken.iter().any(|(b, _)| *b == i);
    let new_index: Vec<Option<usize>> = {
        let mut next = 0;
        (0..plan.selected.len())
            .map(|i| {
                (!gone(i)).then(|| {
                    next += 1;
                    next - 1
                })
            })
            .collect()
    };
    for (i, why) in broken {
        plan.unrunnable.push(plan.selected[*i].wf.file.clone());
        plan.skipped
            .push((plan.selected[*i].wf.file.clone(), why.clone()));
    }
    let selected = std::mem::take(&mut plan.selected);
    plan.selected = selected
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !gone(*i))
        .map(|(_, s)| s)
        .collect();
    plan.units.retain_mut(|u| match new_index[u.wf] {
        Some(n) => {
            u.wf = n;
            true
        }
        None => false,
    });
}

/// `value` as compact JSON with the keys of every object sorted.
pub fn canonical_json(value: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    out.insert(k.clone(), sorted(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

pub(crate) fn sha256_of(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

impl FrozenPlan {
    /// Fix `parts` and compute the digests.
    ///
    /// A workflow whose jobs cannot be run in an order (a cycle, a `needs` of a
    /// job that does not exist) is left out and listed in `skipped` with the
    /// reason, as GitHub does not start that workflow; the others are planned.
    pub fn new(parts: PlanParts) -> Result<Self> {
        Self::new_with_shards(parts, &[])
    }

    /// Like [`FrozenPlan::new`], splitting the jobs `shards` ask to split into
    /// one task per shard (those whose split is safe and whose shards hold the
    /// listed tests exactly once; the others stay one task with the reasons in
    /// their shard group).
    pub fn new_with_shards(mut parts: PlanParts, shards: &[ShardRequest]) -> Result<Self> {
        let config_digest = sha256_of(&canonical_json(&serde_json::to_value(&parts.config)?));
        let specs = |plan: &Plan| {
            (
                plan.selected
                    .iter()
                    .map(WorkflowSpec::from_selected)
                    .collect::<Vec<_>>(),
                plan.units
                    .iter()
                    .map(UnitSpec::from_unit)
                    .collect::<Vec<_>>(),
            )
        };
        let (mut workflows, mut units) = specs(&parts.plan);
        let broken = dag::unrunnable_workflows(&workflows, &units);
        if !broken.is_empty() {
            drop_workflows(&mut parts.plan, &broken);
            (workflows, units) = specs(&parts.plan);
        }
        let graph = dag::build_graph(&workflows, &units)?;
        let (tasks, shard_groups) = dag::derive_tasks(&parts.plan, &workflows, &units, shards);
        let unrunnable = std::mem::take(&mut parts.plan.unrunnable);
        let mut fp = FrozenPlan {
            schema_version: SCHEMA_VERSION,
            digest: String::new(),
            repository: parts.repository,
            source: parts.source,
            branch: parts.branch,
            changed_files: parts.changed_files,
            config_digest,
            config: parts.config,
            filters: parts.filters,
            workflows,
            skipped: parts
                .plan
                .skipped
                .into_iter()
                .map(|(workflow, reason)| SkippedWorkflow {
                    unrunnable: unrunnable.contains(&workflow),
                    workflow,
                    reason,
                })
                .collect(),
            disabled: parts.disabled,
            units,
            graph,
            tasks,
            shard_groups,
        };
        fp.digest = fp.compute_digest()?;
        Ok(fp)
    }

    /// The digest the plan should carry: the SHA-256 of its canonical JSON with
    /// the `digest` field empty.
    pub fn compute_digest(&self) -> Result<String> {
        let mut v = serde_json::to_value(self).context("cannot serialize the plan")?;
        v["digest"] = Value::String(String::new());
        Ok(sha256_of(&canonical_json(&v)))
    }

    /// Fail unless the schema is known and the digests match the content.
    pub fn verify(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            bail!(
                "plan schema version {} is not supported (this is {SCHEMA_VERSION})",
                self.schema_version
            );
        }
        let want = sha256_of(&canonical_json(&serde_json::to_value(&self.config)?));
        if want != self.config_digest {
            bail!("plan config digest does not match its config");
        }
        if dag::build_graph(&self.workflows, &self.units)? != self.graph {
            bail!("plan job graph does not match its jobs");
        }
        let want = self.compute_digest()?;
        if want != self.digest {
            bail!(
                "plan digest {} does not match its content ({want})",
                self.digest
            );
        }
        Ok(())
    }

    /// The plan in canonical JSON (what the digest covers, with the digest).
    pub fn to_canonical_json(&self) -> Result<String> {
        Ok(canonical_json(&serde_json::to_value(self)?))
    }

    /// Read a plan and verify it.
    pub fn from_json(text: &str) -> Result<Self> {
        let fp: FrozenPlan = serde_json::from_str(text).context("cannot read the plan")?;
        fp.verify()?;
        Ok(fp)
    }

    /// The selection part as a [`Plan`].
    pub fn to_plan(&self) -> Plan {
        Plan {
            selected: self
                .workflows
                .iter()
                .map(WorkflowSpec::to_selected)
                .collect(),
            skipped: self
                .skipped
                .iter()
                .map(|s| (s.workflow.clone(), s.reason.clone()))
                .collect(),
            unrunnable: self
                .skipped
                .iter()
                .filter(|s| s.unrunnable)
                .map(|s| s.workflow.clone())
                .collect(),
            units: self.units.iter().map(UnitSpec::to_unit).collect(),
            ignore_env: self.config.ignore_env.clone(),
        }
    }

    /// This plan with `plan` as its workflows and units (for running a
    /// part of it), sealed again under a new digest.
    pub fn with_plan(&self, plan: Plan) -> Result<Self> {
        FrozenPlan::new_with_shards(
            PlanParts {
                repository: self.repository.clone(),
                source: self.source.clone(),
                branch: self.branch.clone(),
                changed_files: self.changed_files.clone(),
                config: self.config.clone(),
                filters: self.filters.clone(),
                disabled: self.disabled.clone(),
                plan,
            },
            &self.shard_requests(),
        )
    }

    /// This plan with `shards` split, sealed again.
    pub fn with_shards(&self, shards: &[ShardRequest]) -> Result<Self> {
        let parts = PlanParts {
            repository: self.repository.clone(),
            source: self.source.clone(),
            branch: self.branch.clone(),
            changed_files: self.changed_files.clone(),
            config: self.config.clone(),
            filters: self.filters.clone(),
            disabled: self.disabled.clone(),
            plan: self.to_plan(),
        };
        FrozenPlan::new_with_shards(parts, shards)
    }

    /// The splits the plan holds, as the requests that made them.
    pub fn shard_requests(&self) -> Vec<ShardRequest> {
        self.shard_groups
            .iter()
            .filter(|g| g.split)
            .filter_map(|g| {
                let first = self.task(g.shards.first()?)?;
                let ts = first.shard.as_ref()?;
                Some(ShardRequest {
                    workflow: g.workflow.clone(),
                    job: g.source_jobs.first()?.clone(),
                    plan: ShardPlan {
                        runner: g.runner.clone().unwrap_or_default(),
                        mode: ts.mode,
                        basis: Basis::Fallback,
                        shards: g
                            .shards
                            .iter()
                            .filter_map(|id| self.task(id)?.shard.as_ref())
                            .map(|s| Shard {
                                index: s.index,
                                tests: s.tests.clone(),
                                load: 0,
                            })
                            .collect(),
                        not_split: vec![],
                    },
                    step: ts.step,
                    dir: ts.dir.clone(),
                    discovered: Discovered {
                        tests: g.manifest.clone(),
                        complete: g.manifest_exact,
                    },
                    fail_fast: g.fail_fast,
                })
            })
            .collect()
    }

    /// This plan naming `source_id` as its input, sealed again. For a run of
    /// a captured source: the commit the plan was made from is the
    /// manifest's execution commit.
    pub fn with_source_id(mut self, source_id: &str) -> Result<Self> {
        self.source.source_id = source_id.to_string();
        self.digest = self.compute_digest()?;
        Ok(self)
    }

    pub fn job(&self, wf: usize, id: &str) -> &JobSpec {
        self.workflows[wf].job(id).expect("job in plan")
    }

    pub fn task(&self, id: &str) -> Option<&ExecutionTask> {
        self.tasks.iter().find(|t| t.id == id)
    }
}

/// The graph of the jobs the plan holds: `needs` as edges, a matrix as one
/// node per variant plus the node later jobs depend on. Fails on a cycle or a
/// `needs` that names no job of the plan.
pub fn derive_job_dag(fp: &FrozenPlan) -> Result<JobGraph> {
    dag::build_graph(&fp.workflows, &fp.units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        let v: Value =
            serde_json::from_str(r#"{"b":1,"a":{"d":[{"z":1,"y":2}],"c":null}}"#).unwrap();
        assert_eq!(
            canonical_json(&v),
            r#"{"a":{"c":null,"d":[{"y":2,"z":1}]},"b":1}"#
        );
    }

    #[test]
    fn digests_are_sha256_hex() {
        assert_eq!(
            sha256_of(""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
