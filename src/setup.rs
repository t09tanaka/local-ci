//! What a task has to prepare before its own steps, and the record of what was
//! prepared.
//!
//! A [`SetupRecipe`] is worked out for one task from the plan alone
//! ([`derive_setup_recipe`]): the leading install steps of its jobs, in order,
//! each with the directory, the shell and the environment it runs with. The
//! plan itself keeps the original steps and says nothing about who does the
//! setup, so a task never depends on another host having done it.
//!
//! [`crate::execute_setup`] does a recipe in a workspace and returns a
//! [`PreparedReceipt`] of the steps that were prepared. [`crate::execute_task`]
//! skips the steps the receipt names and runs every other one itself; without a
//! receipt it runs them all.
//!
//! Whether a prepared tree may be reused is decided by a [`TemplateKey`]: the
//! same command with another shell, directory or environment is another key.

use crate::dag::ExecutionTask;
use crate::frozen::{canonical_json, sha256_of, FrozenPlan};
use crate::plan::{self, Hoist, Plan, StayReason, StepKey};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Bump when what a template holds, or how its key is made, changes.
pub const CACHE_SCHEMA: u32 = 1;

/// Where a recipe step sits in a job of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRef {
    /// The workflow file.
    pub workflow: String,
    pub job: String,
    /// Index in the steps of the job.
    pub index: usize,
}

/// One install of a recipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeStep {
    /// Names what the step does: a digest of directory, script, resolved shell
    /// and effective env. Two steps with the same id prepare the same thing.
    pub id: String,
    /// Working directory relative to the repository root ("" for the root).
    pub dir: String,
    pub run: String,
    /// The shell the step runs with (step, else job, else workflow); `None` is
    /// the default, bash.
    pub shell: Option<String>,
    /// The workflow, job and step `env:` put together, sorted by name. Values
    /// that are expressions are not here but in `unresolved_env`.
    pub env: Vec<(String, String)>,
    /// `env:` entries whose value is an expression evaluated at run time; they
    /// are not set for setup, and are part of the id.
    pub unresolved_env: Vec<(String, String)>,
    /// `env:` entries named in `ignore-env`, with the values of the first job
    /// that has the step. The step runs with them, but they are not part of
    /// the id or of any key.
    #[serde(default)]
    pub ignored_env: Vec<(String, String)>,
    pub label: String,
    /// The workflow and job the step is run for, when it is run.
    pub workflow: String,
    pub job: String,
    /// Every place in the task's jobs where this step is (a second job of the
    /// task that starts with the same install holds another place).
    pub at: Vec<StepRef>,
}

/// A step that looks like preparation but stays in its job, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeftInPlace {
    pub workflow: String,
    pub job: String,
    pub index: usize,
    pub label: String,
    pub reason: StayReason,
}

/// A tool version the task asks for (`node-version: 20` of a `setup-node`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ToolchainRequirement {
    pub tool: String,
    pub version: String,
    /// The `uses:` it was read from.
    pub from: String,
}

/// What a task prepares before its own steps.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SetupRecipe {
    pub task: String,
    /// In the order they run.
    pub steps: Vec<RecipeStep>,
    /// Preparation that stays in its job, with the reason.
    pub left_in_place: Vec<LeftInPlace>,
    pub toolchain: Vec<ToolchainRequirement>,
}

impl SetupRecipe {
    /// The steps of `recipes` as one recipe: each id once, at its first place,
    /// with all the places it has. (A local run does the setup of all its tasks
    /// in one workspace.)
    pub fn union(recipes: &[&SetupRecipe]) -> SetupRecipe {
        let mut out = SetupRecipe {
            task: "*".into(),
            ..Default::default()
        };
        for r in recipes {
            for s in &r.steps {
                match out.steps.iter_mut().find(|x| x.id == s.id) {
                    Some(x) => {
                        for a in &s.at {
                            if !x.at.contains(a) {
                                x.at.push(a.clone());
                            }
                        }
                    }
                    None => out.steps.push(s.clone()),
                }
            }
            for l in &r.left_in_place {
                if !out.left_in_place.contains(l) {
                    out.left_in_place.push(l.clone());
                }
            }
            for t in &r.toolchain {
                if !out.toolchain.contains(t) {
                    out.toolchain.push(t.clone());
                }
            }
        }
        out.toolchain.sort();
        out
    }

    /// The key under which what this recipe prepares can be reused.
    pub fn template_key(&self, ctx: &TemplateContext) -> TemplateKey {
        let inputs = json!({
            "cache_schema": ctx.cache_schema,
            "source": ctx.source_digest,
            "steps": self.steps.iter().map(|s| json!({
                "dir": s.dir,
                "run": s.run,
                "shell": s.shell.as_deref().unwrap_or("bash"),
                "env": s.env,
                "unresolved_env": s.unresolved_env,
            })).collect::<Vec<_>>(),
            "toolchain": self.toolchain,
            "os": ctx.os,
            "arch": ctx.arch,
            "profile": ctx.profile,
            "scope": ctx.scope,
            "freshness": ctx.freshness,
            "secrets": ctx.secrets,
        });
        TemplateKey {
            digest: sha256_of(&canonical_json(&inputs)),
            inputs,
        }
    }

    /// The places in the jobs of the task whose steps `receipt` says were
    /// prepared, by job, when `receipt` belongs to this task in this workspace:
    /// it was made on `source`, for the workspace whose id is `workspace`, and
    /// under the template key this recipe has now in `tctx`, and whose
    /// artifacts are still in `root`. Otherwise `Err` says why it is not
    /// believed, and nothing is skipped.
    pub(crate) fn prepared_places(
        &self,
        receipt: &PreparedReceipt,
        source: &str,
        workspace: Option<&str>,
        root: &Path,
        tctx: &TemplateContext,
    ) -> Result<HashMap<String, HashSet<usize>>, String> {
        let mut out: HashMap<String, HashSet<usize>> = HashMap::new();
        if receipt.steps.is_empty() {
            return Ok(out);
        }
        if receipt.source != source {
            return Err("it was made for another commit".into());
        }
        if workspace.is_none_or(|w| w != receipt.workspace) {
            return Err("it was made for another workspace".into());
        }
        if receipt.template_key != self.template_key(tctx).digest() {
            return Err("its template key is not the key of this task's recipe here".into());
        }
        // The id outlives what the workspace held (`git clean -fdx` leaves the
        // git dir alone), so look for what the install made before believing it.
        if let Some(gone) = receipt
            .artifacts
            .iter()
            .find(|a| root.join(&a.path).symlink_metadata().is_err())
        {
            return Err(format!(
                "`{}`, which the setup made, is no longer in the workspace",
                gone.path
            ));
        }
        for s in self.steps.iter().filter(|s| receipt.covers(&s.id)) {
            for a in &s.at {
                out.entry(a.job.clone()).or_default().insert(a.index);
            }
        }
        Ok(out)
    }
}

/// Whether a prepared tree may be used by another run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReuseScope {
    /// Only by the run that made it.
    Run { run_id: String },
    /// By later runs too, while the key holds.
    Persistent,
}

/// How old a prepared tree may be to be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Freshness {
    Any,
    MaxAgeSecs { secs: u64 },
}

/// A secret that decides what setup produces, by name and a digest of its
/// value (an HMAC or a version). The value itself never goes into a key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SecretDigest {
    pub name: String,
    pub digest: String,
}

/// What a template key holds besides the recipe.
///
/// [`TemplateContext::local`] is enough for a template used by the run that
/// made it. Before a template is reused in a persistent scope, whoever stores it
/// (canyourun) has to fill in what `local` cannot know: the `profile` (the
/// versions of the tools the host carries) and the `secrets` that decide what
/// setup produces.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateContext {
    /// The source the recipe is run on (the commit).
    pub source_digest: String,
    pub os: String,
    pub arch: String,
    /// The host's profile (tool versions it carries and the like).
    pub profile: String,
    pub cache_schema: u32,
    pub scope: ReuseScope,
    pub freshness: Freshness,
    pub secrets: Vec<SecretDigest>,
}

impl TemplateContext {
    /// This machine, for a run of `fp` with id `run_id`.
    ///
    /// The profile is the constant `"local"`: the versions of the tools on this
    /// host are not in the key, and `secrets` is empty (a secret digest is only
    /// what the caller puts in `secrets`). Both are fine for the run that made
    /// the template; fill them in before reusing one in a persistent scope.
    pub fn local(fp: &FrozenPlan, run_id: &str) -> Self {
        TemplateContext {
            source_digest: fp.source.sha.clone(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            profile: "local".into(),
            cache_schema: CACHE_SCHEMA,
            scope: ReuseScope::Run {
                run_id: run_id.into(),
            },
            freshness: Freshness::Any,
            secrets: vec![],
        }
    }
}

/// The identity of a prepared template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateKey {
    digest: String,
    inputs: Value,
}

impl TemplateKey {
    /// `sha256:<hex>` of the inputs.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// What the key was made from, for showing why two keys differ.
    pub fn inputs(&self) -> &Value {
        &self.inputs
    }
}

/// How a step came to be prepared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum PreparedHow {
    /// It was run and passed.
    Ran,
    /// An install of the same kind (`npm ci` for `npm ci --no-audit`) in the
    /// same directory, shell and env had been run; this one was not.
    SameAs { step: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedStep {
    /// The id of the [`RecipeStep`].
    pub id: String,
    #[serde(flatten)]
    pub how: PreparedHow,
}

/// A digest of something setup made, by path relative to the workspace.
///
/// For now this is a digest of the names and sizes in the first two levels of
/// each new top-level entry, not of the contents, so it cannot tell two trees
/// with the same names and sizes apart. The digest of the contents (a tree
/// digest) comes with the result checks of a later stage; until then whoever
/// stores a template should compute the digest of the tree it stores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDigest {
    pub path: String,
    pub digest: String,
}

/// The steps of a recipe that were really prepared in a workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreparedReceipt {
    /// Digest of the [`TemplateKey`] it was made for.
    pub template_key: String,
    /// The commit it was prepared on.
    pub source: String,
    /// The id of the workspace it was prepared in (see [`workspace_id`]). A
    /// task believes the receipt only in the workspace that has this id.
    #[serde(default)]
    pub workspace: String,
    /// In the order they were prepared. A step that failed or was not reached
    /// is not here.
    pub steps: Vec<PreparedStep>,
    pub artifacts: Vec<ArtifactDigest>,
}

impl PreparedReceipt {
    /// A receipt that prepared nothing.
    pub fn empty(template_key: &str, source: &str) -> Self {
        PreparedReceipt {
            template_key: template_key.into(),
            source: source.into(),
            workspace: String::new(),
            steps: vec![],
            artifacts: vec![],
        }
    }

    /// This receipt for the workspace `workspace`, which was made from the one
    /// it is for: a copy [`crate::execute_task`] callers derive from a prepared
    /// workspace (a unit's copy, a materialized template) has a new id and
    /// holds what the original held, so the receipt is theirs to say so with.
    pub fn rebound(&self, workspace: &str) -> PreparedReceipt {
        PreparedReceipt {
            workspace: workspace.into(),
            ..self.clone()
        }
    }

    pub fn covers(&self, step_id: &str) -> bool {
        self.steps.iter().any(|s| s.id == step_id)
    }

    /// What this receipt says about the steps of `recipe` only, under `key`.
    pub fn restricted_to(&self, recipe: &SetupRecipe, key: &TemplateKey) -> PreparedReceipt {
        PreparedReceipt {
            template_key: key.digest().into(),
            source: self.source.clone(),
            workspace: self.workspace.clone(),
            steps: self
                .steps
                .iter()
                .filter(|s| recipe.steps.iter().any(|r| r.id == s.id))
                .cloned()
                .collect(),
            // what is made directly under a directory the recipe installs in
            artifacts: self
                .artifacts
                .iter()
                .filter(|a| {
                    let parent = a.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                    recipe.steps.iter().any(|r| r.dir == parent)
                })
                .cloned()
                .collect(),
        }
    }
}

fn step_id(k: &StepKey) -> String {
    sha256_of(&canonical_json(&json!({
        "dir": k.wd,
        "run": k.run,
        "shell": k.shell.as_deref().unwrap_or("bash"),
        "env": k.env,
        "unresolved_env": k.open_env,
    })))
}

/// The recipe of task `task_id` of `fp`.
pub fn derive_setup_recipe(fp: &FrozenPlan, task_id: &str) -> Result<SetupRecipe> {
    let task = fp
        .task(task_id)
        .ok_or_else(|| anyhow!("the plan has no task `{task_id}`"))?;
    let plan = fp.to_plan();
    derive_in(&plan, &plan::hoist(&plan), task)
}

/// [`derive_setup_recipe`] over a plan whose `hoist` is known.
pub(crate) fn derive_in(plan: &Plan, hoist: &Hoist, task: &ExecutionTask) -> Result<SetupRecipe> {
    let sel = plan
        .selected
        .iter()
        .find(|s| s.wf.file == task.workflow)
        .ok_or_else(|| anyhow!("the plan has no workflow {}", task.workflow))?;
    let w = &sel.wf;
    let mut out = SetupRecipe {
        task: task.id.clone(),
        ..Default::default()
    };
    let mut tools: BTreeSet<ToolchainRequirement> = BTreeSet::new();
    for id in &task.jobs {
        let Some(j) = w.jobs.iter().find(|j| &j.id == id) else {
            continue;
        };
        if j.container || j.reusable.is_some() {
            continue;
        }
        for s in &j.steps {
            let Some(uses) = &s.uses else { continue };
            for (k, v) in &s.with {
                if let Some(tool) = k.strip_suffix("-version") {
                    if !v.contains("${{") && !v.is_empty() {
                        tools.insert(ToolchainRequirement {
                            tool: tool.to_string(),
                            version: v.clone(),
                            from: uses.split('@').next().unwrap_or(uses).to_string(),
                        });
                    }
                }
            }
        }
        let prefix = plan::prefix_of(w, j, &plan.ignore_env);
        let mut seen: HashSet<String> = HashSet::new();
        // set when a step comes up a second time, which ends the prefix
        let mut ended = false;
        for p in &prefix.steps {
            if !hoist.keys.contains(&p.key) {
                if hoist.conflicting.contains(&p.key) {
                    out.left_in_place.push(LeftInPlace {
                        workflow: w.file.clone(),
                        job: j.id.clone(),
                        index: p.index,
                        label: p.step.label(),
                        reason: StayReason::ConflictingInstall,
                    });
                }
                break;
            }
            let id = step_id(&p.key);
            // The same step again after others is not the same preparation: it
            // runs where the job has it, so the job's prefix ends before it.
            if !seen.insert(id.clone()) {
                ended = true;
                break;
            }
            let at = StepRef {
                workflow: w.file.clone(),
                job: j.id.clone(),
                index: p.index,
            };
            match out.steps.iter_mut().find(|s| s.id == id) {
                Some(s) => s.at.push(at),
                None => out.steps.push(RecipeStep {
                    id,
                    dir: p.key.wd.clone(),
                    run: p.key.run.clone(),
                    shell: p.key.shell.clone(),
                    env: p.key.env.clone(),
                    unresolved_env: p.key.open_env.clone(),
                    ignored_env: plan::effective_env(w, j, p.step)
                        .0
                        .into_iter()
                        .filter(|(k, _)| plan.ignore_env.contains(k))
                        .collect(),
                    label: p.step.label(),
                    workflow: w.file.clone(),
                    job: j.id.clone(),
                    at: vec![at],
                }),
            }
        }
        if let (false, Some((index, reason))) = (ended, prefix.stopped_by) {
            let s = &j.steps[index];
            let run = s.run.as_deref().unwrap_or("");
            if plan::is_installer(run)
                || matches!(
                    reason,
                    StayReason::EnvFileEffect
                        | StayReason::AbsolutePath
                        | StayReason::UnsupportedShell
                )
            {
                out.left_in_place.push(LeftInPlace {
                    workflow: w.file.clone(),
                    job: j.id.clone(),
                    index,
                    label: s.label(),
                    reason,
                });
            }
        }
    }
    out.toolchain = tools.into_iter().collect();
    Ok(out)
}

/// Installs of one kind in one place (`npm ci` and `npm install` in `app`)
/// prepare the same thing; of those, only the first is run. Returns, for each
/// step of `recipe` that is not the first of its kind, the id of the first.
pub fn covered_by(recipe: &SetupRecipe) -> HashMap<String, String> {
    let mut seen: Vec<(String, &RecipeStep)> = Vec::new();
    let mut out = HashMap::new();
    for s in &recipe.steps {
        let Some(family) = plan::install_family(&s.run) else {
            continue;
        };
        let first = seen.iter().find(|(f, x)| {
            *f == family
                && x.dir == s.dir
                && x.shell == s.shell
                && x.env == s.env
                && x.unresolved_env == s.unresolved_env
        });
        match first {
            Some((_, x)) => {
                out.insert(s.id.clone(), x.id.clone());
            }
            None => seen.push((family, s)),
        }
    }
    out
}

/// The result of doing a recipe in a workspace.
#[derive(Debug, Clone)]
pub struct PreparedSetup {
    pub receipt: PreparedReceipt,
    /// Each step that was run, as a run reports it.
    pub steps: Vec<crate::exec::StepResult>,
}

impl PreparedSetup {
    /// Whether every step that was run passed.
    pub fn ok(&self) -> bool {
        self.steps
            .iter()
            .all(|s| s.outcome == crate::exec::Outcome::Pass)
    }
}

/// The entries directly under each of `dirs` (relative to `root`), as paths
/// relative to `root`.
pub(crate) fn top_level_entries(root: &Path, dirs: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(root.join(d)) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            out.insert(if d.is_empty() {
                name
            } else {
                format!("{d}/{name}")
            });
        }
    }
    out
}

const MANIFEST_CAP: usize = 20_000;

/// A digest of the names and sizes of the first two levels under each entry of
/// `after` that was not in `before`.
pub(crate) fn artifact_digests(
    root: &Path,
    before: &BTreeSet<String>,
    after: &BTreeSet<String>,
) -> Vec<ArtifactDigest> {
    after
        .difference(before)
        .map(|p| {
            let mut lines = Vec::new();
            manifest(&root.join(p), "", 0, &mut lines);
            lines.sort();
            ArtifactDigest {
                path: p.clone(),
                digest: sha256_of(&lines.join("\n")),
            }
        })
        .collect()
}

fn manifest(path: &Path, rel: &str, depth: usize, out: &mut Vec<String>) {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return;
    };
    let kind = if md.file_type().is_symlink() {
        "l"
    } else if md.is_dir() {
        "d"
    } else {
        "f"
    };
    out.push(format!(
        "{rel}\t{kind}\t{}",
        if kind == "f" { md.len() } else { 0 }
    ));
    if kind != "d" || depth >= 2 || out.len() >= MANIFEST_CAP {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            manifest(&e.path(), &format!("{rel}/{name}"), depth + 1, out);
        }
    }
}

/// The file in a workspace's git dir that holds its id.
const WORKSPACE_ID: &str = "local-ci-workspace-id";

/// The git dir of the workspace at `root`: `.git` itself, or the directory a
/// linked worktree's `.git` file points to (`<repo>/.git/worktrees/<name>`).
/// It is outside the working tree, so no step, `git status` or tool that walks
/// the tree can see what is kept there, and each worktree has its own.
fn git_dir_of(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let dir = Path::new(text.trim().strip_prefix("gitdir:")?.trim());
    let dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        root.join(dir)
    };
    dir.is_dir().then_some(dir)
}

/// Where the workspace at `root` stands, as `(file that keeps its id, the root
/// it is recorded for)`. The recorded root is what keeps a copy from being
/// taken for the original: a copied tree whose `.git` still points at the
/// same git dir, or a copied `.git` directory, carries the file but sits at
/// another path, and so has no id of its own until it is prepared itself.
fn id_place(root: &Path) -> Option<(PathBuf, String)> {
    let canon = std::fs::canonicalize(root).ok()?;
    let git_dir = git_dir_of(root)?;
    // A `.git` file that points at the git dir of another worktree (a copy of
    // a worktree) is not this workspace's own: nothing may be recorded there,
    // it would replace the id of the original.
    if root.join(".git").is_file() {
        let owner = std::fs::read_to_string(git_dir.join("gitdir")).ok()?;
        let owner = std::fs::canonicalize(owner.trim()).ok()?;
        if owner != std::fs::canonicalize(root.join(".git")).ok()? {
            return None;
        }
    }
    Some((
        git_dir.join(WORKSPACE_ID),
        canon.to_string_lossy().into_owned(),
    ))
}

/// The id of the workspace at `root`, if it has one.
///
/// A workspace is a git worktree, and its id lives in its own git dir, never in
/// the working tree: an untracked file there would show up in `git status`,
/// `git diff`, formatters and packaging, which CI never sees. The id is
/// recorded together with the path of the workspace and is believed only at
/// that path, so a copy of the workspace (a materialized template, a unit's
/// copy) is another workspace, not the original.
///
/// A directory that is not a git worktree has nothing to keep an id in. Its id
/// is derived instead, from its path and (on unix) its device and inode, and
/// nothing is written; a copy has another inode and so another id.
pub fn workspace_id(root: &Path) -> Option<String> {
    let Some((file, recorded)) = id_place(root) else {
        return derived_id(root);
    };
    let text = std::fs::read_to_string(file).ok()?;
    let (id, at) = text.split_once('\n')?;
    (at.trim_end() == recorded && !id.is_empty()).then(|| id.to_string())
}

fn derived_id(root: &Path) -> Option<String> {
    let canon = std::fs::canonicalize(root).ok()?;
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::metadata(&canon).ok()?;
        format!("{}:{}", md.dev(), md.ino())
    };
    #[cfg(not(unix))]
    let ino = String::new();
    Some(sha256_of(&format!("dir:{}:{ino}", canon.display())))
}

/// The id of the workspace at `root`; made and written (into its git dir)
/// when it has none.
pub(crate) fn ensure_workspace_id(root: &Path) -> Result<String> {
    if let Some(id) = workspace_id(root) {
        return Ok(id);
    }
    let Some((file, recorded)) = id_place(root) else {
        return derived_id(root).context("cannot derive an id for the workspace");
    };
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let id = sha256_of(&format!(
        "{}:{nanos}:{}:{recorded}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
    ));
    std::fs::write(&file, format!("{id}\n{recorded}"))?;
    Ok(id)
}
