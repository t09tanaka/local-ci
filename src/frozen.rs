//! The plan as data: what was decided, fixed so it can be stored, sent to
//! another host and run there without deciding again.
//!
//! A [`FrozenPlan`] holds no path of the machine that made it: files are
//! relative to the repository root and the repository is named by its slug.
//! Its `digest` is the SHA-256 of the plan in a canonical form (object keys
//! sorted, no whitespace), so the same decisions always give the same digest,
//! wherever the repository was cloned.

use crate::plan::{self, Plan, Selected, SetupStep, Unit};
use crate::workflow::Job;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Bumped when the shape of a [`FrozenPlan`] changes incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

/// What the plan was made from: a commit and the ref it was compared with.
/// (Snapshots of uncommitted work are added later.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    /// The full commit sha the jobs run against.
    pub sha: String,
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
    pub workflows: Vec<Selected>,
    pub skipped: Vec<SkippedWorkflow>,
    pub units: Vec<Unit>,
    pub setup: Vec<SetupStep>,
}

/// Everything a plan is made of except the two digests.
pub struct PlanParts {
    pub repository: Option<String>,
    pub source: SourceRef,
    pub branch: Option<String>,
    pub changed_files: Vec<String>,
    pub config: FrozenConfig,
    pub filters: Filters,
    pub plan: Plan,
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

fn sha256_of(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

impl FrozenPlan {
    /// Fix `parts` and compute the digests.
    pub fn new(parts: PlanParts) -> Result<Self> {
        let config_digest = sha256_of(&canonical_json(&serde_json::to_value(&parts.config)?));
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
            workflows: parts.plan.selected,
            skipped: parts
                .plan
                .skipped
                .into_iter()
                .map(|(workflow, reason)| SkippedWorkflow { workflow, reason })
                .collect(),
            units: parts.plan.units,
            setup: parts.plan.setup,
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
            selected: self.workflows.clone(),
            skipped: self
                .skipped
                .iter()
                .map(|s| (s.workflow.clone(), s.reason.clone()))
                .collect(),
            units: self.units.clone(),
            setup: self.setup.clone(),
        }
    }

    /// This plan with `plan` as its workflows, units and setup (for running a
    /// part of it), sealed again under a new digest.
    pub fn with_plan(&self, plan: Plan) -> Result<Self> {
        FrozenPlan::new(PlanParts {
            repository: self.repository.clone(),
            source: self.source.clone(),
            branch: self.branch.clone(),
            changed_files: self.changed_files.clone(),
            config: self.config.clone(),
            filters: self.filters.clone(),
            plan,
        })
    }

    pub fn job(&self, wf: usize, id: &str) -> &Job {
        self.workflows[wf]
            .wf
            .jobs
            .iter()
            .find(|j| j.id == id)
            .expect("job in plan")
    }

    pub fn is_setup(&self, k: &plan::StepKey) -> bool {
        self.setup.iter().any(|s| &s.key == k)
    }
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
