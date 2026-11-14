//! Planning from captured bytes without Git, commands, secrets or mutable caller views.
//!
//! This entry point owns a private, short-lived materialization. It validates
//! the manifest and every object before parsing, never restores secret files,
//! and returns an ordinary frozen plan whose digest binds its captured source.

use crate::frozen::{
    DisabledRecord, DisabledSource, DisabledWorkflow, Filters, FrozenConfig, FrozenPlan, PlanParts,
    SourceRef,
};
use crate::source::{ObjectStore, RelPath, SourceKind, SourceManifest};
use crate::{config, filter, plan, workflow};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

/// Captured-source plans use a distinct format; legacy Git plans remain schema 6.
pub const PINNED_SCHEMA_VERSION: u32 = 7;

/// Captured provenance retained in the plan itself, including when serialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedIdentity {
    pub kind: SourceKind,
    pub manifest_digest: String,
    pub repository_id: Option<String>,
    pub cwd_relative: RelPath,
    /// False means changes were unknown, rather than a known empty change set.
    pub changed_files_known: bool,
    /// The frozen PR target branch name, distinct from the base commit/ref.
    pub base_branch: Option<String>,
}

/// Selection and event metadata captured by the caller. Nothing is inferred
/// from the process environment, current directory, Git, or the network.
#[derive(Debug, Clone)]
pub struct PinnedPlanOptions {
    pub filters: Filters,
    /// Authoritative resolved workflow directories, replacing the captured
    /// config value. None uses captured config/defaults; Some(empty) is invalid.
    /// Extra `filters.workflows_dirs` are still appended afterwards.
    pub workflow_dirs_override: Option<Vec<String>>,
    /// Explicit, possibly empty list. No GitHub lookup is performed.
    pub disabled: Vec<DisabledWorkflow>,
    pub base_branch: Option<String>,
    /// None is unknown. Some(empty) means a known empty change set.
    pub changed_files: Option<Vec<String>>,
}

/// Static diagnostics safe for a summary to display. Parser errors, scripts,
/// environment values and private filesystem paths are never embedded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedPlanError {
    InvalidSource,
    InvalidOptions,
    InvalidConfig,
    InvalidWorkflow,
    UnavailablePlanningInput,
    MissingBaseBranch,
    MissingHeadBranch,
    MissingChangedFiles,
    UnsupportedSource,
    InvalidPlan,
    UnsupportedExecution,
}
impl std::fmt::Display for PinnedPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidSource => "invalid captured source or objects",
            Self::InvalidOptions => "invalid pinned planning options",
            Self::InvalidConfig => "invalid captured planning configuration",
            Self::InvalidWorkflow => "invalid captured workflow",
            Self::UnavailablePlanningInput => "planning input is unavailable or secret",
            Self::MissingBaseBranch => "pinned base branch is required by a workflow filter",
            Self::MissingHeadBranch => "pinned head branch is required by a workflow filter",
            Self::MissingChangedFiles => "pinned changed files are required by a workflow filter",
            Self::UnsupportedSource => "captured source is unsupported for planning",
            Self::InvalidPlan => "captured workflows cannot form a frozen plan",
            Self::UnsupportedExecution => "pinned source is unsupported by the Git executor",
        })
    }
}
impl std::error::Error for PinnedPlanError {}

type Result<T> = std::result::Result<T, PinnedPlanError>;

fn relative(path: &str) -> bool {
    !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

fn text(root: &Path, m: &SourceManifest, path: &str) -> Result<Option<String>> {
    let rel = RelPath::new(path);
    if m.secret_files.iter().any(|s| s.path == rel) {
        return Err(PinnedPlanError::UnavailablePlanningInput);
    }
    let Some(entry) = m.object_manifest.entries.iter().find(|e| e.path == rel) else {
        return Ok(None);
    };
    if !matches!(entry.mode, 0o100644 | 0o100755) {
        return Err(PinnedPlanError::UnavailablePlanningInput);
    }
    std::fs::read_to_string(root.join(path))
        .map(Some)
        .map_err(|_| PinnedPlanError::UnavailablePlanningInput)
}

/// Plan an already captured source. No recapture, Git, command runner, demand,
/// queue, ledger, tool lookup, or secret restoration is involved.
///
/// The manifest is not trusted merely because it is a public DTO: its schema,
/// digest, paths, objects and complete materialized tree are checked first.
/// Directory sources retain `None` for commit and base; the legacy Git
/// execution entry points reject them before side effects.
pub fn build_frozen_plan_from_source(
    store: &ObjectStore,
    manifest: &SourceManifest,
    options: &PinnedPlanOptions,
) -> Result<FrozenPlan> {
    if !manifest.submodules.is_empty() {
        return Err(PinnedPlanError::UnsupportedSource);
    }
    if manifest.source_kind != SourceKind::Directory
        && manifest
            .execution_commit
            .as_ref()
            .is_none_or(String::is_empty)
    {
        return Err(PinnedPlanError::InvalidSource);
    }
    if manifest.source_kind == SourceKind::Directory
        && (manifest.execution_commit.is_some() || manifest.base.is_some())
    {
        return Err(PinnedPlanError::InvalidSource);
    }
    let cwd = manifest.cwd_relative.to_path();
    if cwd.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(PinnedPlanError::InvalidSource);
    }
    if options.base_branch.as_ref().is_some_and(String::is_empty)
        || options
            .changed_files
            .as_ref()
            .is_some_and(|v| v.iter().any(|p| !relative(p)))
        || options.filters.workflows_dirs.iter().any(|d| {
            let n = plan::norm_dir(d);
            !n.is_empty() && !relative(&n)
        })
    {
        return Err(PinnedPlanError::InvalidOptions);
    }
    if manifest.secret_files.iter().any(|s| {
        manifest
            .object_manifest
            .entries
            .iter()
            .any(|e| e.path == s.path)
    }) {
        return Err(PinnedPlanError::InvalidSource);
    }
    let lease = tempfile::tempdir().map_err(|_| PinnedPlanError::InvalidSource)?;
    crate::source::materialize_from(store, manifest, lease.path())
        .map_err(|_| PinnedPlanError::InvalidSource)?;
    let mut cfg = match text(lease.path(), manifest, config::FILE)? {
        Some(s) => config::parse(&s).map_err(|_| PinnedPlanError::InvalidConfig)?,
        None => config::Config::default(),
    };
    if let Some(dirs) = &options.workflow_dirs_override {
        if dirs.is_empty() {
            return Err(PinnedPlanError::InvalidOptions);
        }
        let mut normalized = vec![];
        for dir in dirs {
            let dir = plan::norm_dir(dir);
            if dir.is_empty() || !relative(&dir) {
                return Err(PinnedPlanError::InvalidOptions);
            }
            if !normalized.contains(&dir) {
                normalized.push(dir);
            }
        }
        cfg.workflows_dirs = Some(normalized);
    }
    let mut dirs: Vec<(String, bool)> = match &cfg.workflows_dirs {
        Some(d) => d.iter().map(|d| (d.clone(), true)).collect(),
        None => vec![(config::DEFAULT_WORKFLOWS_DIR.into(), false)],
    };
    for d in &options.filters.workflows_dirs {
        let d = plan::norm_dir(d);
        if !dirs.iter().any(|(old, _)| old == &d) {
            dirs.push((d, true));
        }
    }
    let mut workflows = vec![];
    for (dir, required) in &dirs {
        if !dir.is_empty() && !relative(dir) {
            return Err(PinnedPlanError::InvalidConfig);
        }
        let directory = RelPath::new(dir);
        if manifest.object_manifest.entries.iter().any(|e| {
            e.mode == 0o120000 && (directory == e.path || directory.starts_with_dir(&e.path))
        }) || manifest
            .secret_files
            .iter()
            .any(|s| directory == s.path || directory.starts_with_dir(&s.path))
        {
            return Err(PinnedPlanError::UnavailablePlanningInput);
        }
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let in_dir = |p: &[u8]| {
            p.strip_prefix(prefix.as_bytes()).is_some_and(|p| {
                !p.contains(&b'/') && (p.ends_with(b".yml") || p.ends_with(b".yaml"))
            })
        };
        if manifest.secret_files.iter().any(|s| in_dir(&s.path.0)) {
            return Err(PinnedPlanError::UnavailablePlanningInput);
        }
        let mut files = vec![];
        for e in &manifest.object_manifest.entries {
            if in_dir(&e.path.0) {
                files.push(
                    std::str::from_utf8(&e.path.0)
                        .map_err(|_| PinnedPlanError::UnavailablePlanningInput)?
                        .to_string(),
                );
            }
        }
        files.sort();
        if files.is_empty() && *required {
            return Err(PinnedPlanError::UnavailablePlanningInput);
        }
        for file in files {
            let content = text(lease.path(), manifest, &file)?
                .ok_or(PinnedPlanError::UnavailablePlanningInput)?;
            workflows.push(
                workflow::parse(&file, &content).map_err(|_| PinnedPlanError::InvalidWorkflow)?,
            );
        }
    }
    let mut disabled = options.disabled.clone();
    disabled.sort();
    disabled.dedup();
    let check_disabled = !options.filters.all && !options.filters.include_disabled;
    let mut disabled_skips = vec![];
    if check_disabled {
        workflows.retain(|w| {
            if let Some(d) = disabled.iter().find(|d| d.path == w.file) {
                if options.filters.workflows.is_empty()
                    || options
                        .filters
                        .workflows
                        .iter()
                        .any(|o| w.file.ends_with(o) || &w.name == o)
                {
                    disabled_skips.push((
                        w.file.clone(),
                        format!("{} on GitHub", d.state.replace('_', " ")),
                    ));
                }
                false
            } else {
                true
            }
        });
    }
    let event = filter::Event {
        // Availability is checked with the same ordered trigger evaluator
        // before these placeholders can influence the resulting plan.
        base: options.base_branch.clone().unwrap_or_default(),
        head: manifest.branch.clone(),
        changed: options.changed_files.clone().unwrap_or_default(),
        all: options.filters.all,
    };
    let mut decided = vec![];
    for w in workflows {
        if !options.filters.workflows.is_empty()
            && !options
                .filters
                .workflows
                .iter()
                .any(|o| w.file.ends_with(o) || &w.name == o)
        {
            continue;
        }
        let decision = filter::decide_with_metadata(
            &w,
            &event,
            options.base_branch.is_some(),
            manifest.source_kind != SourceKind::Directory || manifest.branch.is_some(),
            options.changed_files.is_some(),
        )
        .map_err(|missing| match missing {
            filter::MissingMetadata::BaseBranch => PinnedPlanError::MissingBaseBranch,
            filter::MissingMetadata::HeadBranch => PinnedPlanError::MissingHeadBranch,
            filter::MissingMetadata::ChangedFiles => PinnedPlanError::MissingChangedFiles,
        })?;
        decided.push((w, decision));
    }
    let mut p = plan::build_decided(decided);
    p.skipped.extend(disabled_skips);
    if !options.filters.jobs.is_empty() {
        crate::run::keep_jobs(&mut p, &options.filters.jobs);
    }
    let default_dir = cfg.default_workflows_dir().to_string();
    let frozen = FrozenPlan::new(PlanParts {
        repository: manifest
            .repository_id
            .as_ref()
            .and_then(|r| r.strip_prefix("github:"))
            .map(str::to_string),
        source: SourceRef {
            source_id: manifest.source_id.clone(),
            execution_commit: manifest.execution_commit.clone(),
            base: manifest.base.clone(),
            pinned: Some(PinnedIdentity {
                kind: manifest.source_kind,
                manifest_digest: manifest.manifest_digest.clone(),
                repository_id: manifest.repository_id.clone(),
                cwd_relative: manifest.cwd_relative.clone(),
                changed_files_known: options.changed_files.is_some(),
                base_branch: options.base_branch.clone(),
            }),
        },
        branch: manifest.branch.clone(),
        changed_files: options.changed_files.clone().unwrap_or_default(),
        config: FrozenConfig {
            workflow_dirs: dirs.into_iter().map(|(d, _)| d).collect(),
            default_workflows_dir: default_dir,
            no_cache: cfg.no_cache,
            link_node_modules: cfg.link_node_modules,
            ignore_env: cfg.ignore_env,
        },
        filters: Filters {
            workflows_dirs: options
                .filters
                .workflows_dirs
                .iter()
                .map(|d| plan::norm_dir(d))
                .collect(),
            ..options.filters.clone()
        },
        disabled: if check_disabled {
            DisabledRecord {
                source: DisabledSource::Provided,
                workflows: disabled,
            }
        } else {
            DisabledRecord::not_checked()
        },
        plan: p,
    })
    .map_err(|_| PinnedPlanError::InvalidPlan)?;
    frozen
        .verify()
        .map_err(|_| PinnedPlanError::InvalidSource)?;
    Ok(frozen)
}

pub(crate) fn verify_identity(fp: &FrozenPlan, identity: &PinnedIdentity) -> Result<()> {
    let digest = identity
        .manifest_digest
        .strip_prefix("sha256:")
        .ok_or(PinnedPlanError::InvalidSource)?;
    if digest.len() != 64
        || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        || fp.source.source_id != format!("src-{}", &digest[..32])
        || identity
            .cwd_relative
            .to_path()
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || (!identity.changed_files_known && !fp.changed_files.is_empty())
        || identity.base_branch.as_ref().is_some_and(String::is_empty)
        || fp.source.base.as_ref().is_some_and(String::is_empty)
    {
        return Err(PinnedPlanError::InvalidSource);
    }
    match identity.kind {
        SourceKind::Directory
            if fp.source.execution_commit.is_none()
                && fp.source.base.is_none()
                && fp.branch.is_none() => {}
        SourceKind::Worktree | SourceKind::Commit
            if fp.source.execution_commit.as_ref().is_some_and(|c| {
                matches!(c.len(), 40 | 64) && c.bytes().all(|b| b.is_ascii_hexdigit())
            }) => {}
        _ => return Err(PinnedPlanError::InvalidSource),
    }
    let repository = identity
        .repository_id
        .as_ref()
        .and_then(|s| s.strip_prefix("github:"))
        .map(str::to_string);
    if fp.repository != repository {
        return Err(PinnedPlanError::InvalidSource);
    }
    Ok(())
}
