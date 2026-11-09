//! Resolving what a commit triggers, running it, and recording the result.
//!
//! The functions here do not print, read the process's working directory or
//! environment, or exit; everything they need comes in as arguments (the
//! environment in an [`ExecContext`]) and everything they have to say goes out
//! through the return value or a [`Observer`].
//!
//! Planning ([`build_frozen_plan`]) and executing ([`execute_plan`]) are
//! separate: the first reads the repository and decides, the second runs what
//! was decided in a workspace it is given. [`run`] puts them together with the
//! results cache, the worktree and the saved result.

use crate::context::ExecContext;
use crate::frozen::{
    DisabledRecord, DisabledSource, DisabledWorkflow, Filters, FrozenConfig, FrozenPlan, PlanParts,
    SourceRef,
};
use crate::source::{self, SourceInfo, SourceKind, SourceManifest};
use crate::{cache, config, exec, filter, git, plan, report, setup, workflow};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// What the caller wants told while a run goes on.
pub trait Observer {
    /// A warning or progress line (no `local-ci:` prefix).
    fn notice(&mut self, _msg: &str) {}
    /// The run is about to start processes; the caller may install its signal
    /// handler now (it cancels with the [`ExecContext::cancel`] handle).
    fn executing(&mut self) {}
}

/// Ignores everything.
pub struct Quiet;
impl Observer for Quiet {}

/// Which commit to look at and which workflows to take into account.
#[derive(Clone, Debug)]
pub struct Target {
    pub base: Option<String>,
    pub sha: String,
    pub all: bool,
    pub include_disabled: bool,
    pub workflows_dirs: Vec<String>,
    pub workflows: Vec<String>,
    pub jobs: Vec<String>,
    /// Repository directory.
    pub dir: PathBuf,
}

/// Options of [`run`].
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub target: Target,
    pub parallel: Option<usize>,
    pub no_canirun: bool,
    pub no_portool: bool,
    pub containers: usize,
    pub keep: bool,
    pub job_timeout: f64,
    pub no_cache: bool,
    /// Where runs, results, worktrees and locks live (see [`ExecContext::cache_dir`]).
    pub cache_dir: PathBuf,
    /// The captured source the run is over: `target.sha` is its execution
    /// commit (found in `target.dir`, e.g. after `source::import_objects`).
    /// `None` for a run of a plain commit.
    pub source: Option<SourceManifest>,
}

/// What [`run`] produced; the caller prints it and picks the exit code.
pub struct RunReport {
    pub result: report::RunResult,
    pub repo_root: PathBuf,
    pub cancelled: bool,
    /// How to print `result` (see [`report::print`]).
    pub style: report::Style,
    /// The captured source the run was over, if it was over one. Decides
    /// whether the result may be a commit status: see [`publish_target`].
    pub source: Option<SourceInfo>,
}

pub struct Repo {
    pub root: PathBuf,
    pub sha: String,
    pub base: String,
    pub head: Option<String>,
    pub changed: Vec<String>,
    pub dirty: bool,
    pub all: bool,
    /// Where the workflows were read from, relative to the root.
    pub workflow_dirs: Vec<String>,
    /// `no-cache` of `.local-ci.toml`.
    pub no_cache: Vec<String>,
    /// `link-node-modules` of `.local-ci.toml`.
    pub link_node_modules: bool,
    /// `[setup] ignore-env` of `.local-ci.toml`.
    pub ignore_env: Vec<String>,
    /// The directory output leaves off workflow paths (see [`report::Style`]).
    pub default_workflows_dir: String,
    /// The disabled workflows the plan was made with, and where the list came from.
    pub disabled: DisabledRecord,
}

/// Read the repository and work out what `t` triggers. Besides the plan it
/// returns what the caller of the command line wants to print.
pub fn resolve(
    ctx: &ExecContext,
    t: &Target,
    obs: &mut dyn Observer,
) -> Result<(Repo, plan::Plan)> {
    resolve_with(ctx, t, None, obs)
}

/// [`resolve`], with the workflows GitHub does not run given by the caller:
/// `Some(list)` is taken as is and nothing is asked of `gh` or the network;
/// `None` asks `gh` (unless the target is `all` or `include_disabled`, which
/// never ask).
pub fn resolve_with(
    ctx: &ExecContext,
    t: &Target,
    disabled_list: Option<Vec<DisabledWorkflow>>,
    obs: &mut dyn Observer,
) -> Result<(Repo, plan::Plan)> {
    let root = git::toplevel(ctx, &t.dir)?;
    let sha = git::rev(ctx, &root, &t.sha)?;
    let base = match &t.base {
        Some(b) => b.clone(),
        None => git::default_base(ctx, &root)?,
    };
    let changed = git::changed_files(ctx, &root, &base, &sha)?;
    let head = if t.sha == "HEAD" {
        git::current_branch(ctx, &root)
    } else {
        None
    };
    let config = config::at(ctx, &root, &sha)?;
    let mut wfs = Vec::new();
    for (file, text) in git::workflows_at(
        ctx,
        &root,
        &sha,
        config.workflows_dirs.as_deref(),
        &t.workflows_dirs,
    )? {
        match workflow::parse(&file, &text) {
            Ok(w) => wfs.push(w),
            Err(e) => obs.notice(&format!("skipping {file}: {e:#}")),
        }
    }
    let ev = filter::Event {
        base: git::base_branch_name(ctx, &root, &base),
        head: head.clone(),
        changed: changed.clone(),
        all: t.all,
    };
    // GitHub does not run workflows that are disabled there; neither does the default mode.
    let mut disabled_skips = Vec::new();
    let mut record = DisabledRecord::not_checked();
    if !t.all && !t.include_disabled {
        let (source, mut list) = match disabled_list {
            Some(l) => (DisabledSource::Provided, l),
            None => match disabled_workflows(ctx, &root) {
                Some(l) => (DisabledSource::Github, l),
                None => (DisabledSource::Unavailable, vec![]),
            },
        };
        list.sort();
        list.dedup();
        let disabled = list.clone();
        record = DisabledRecord {
            source,
            workflows: list,
        };
        wfs.retain(|w| match disabled.iter().find(|d| d.path == w.file) {
            Some(DisabledWorkflow { state, .. }) => {
                if t.workflows.is_empty()
                    || t.workflows
                        .iter()
                        .any(|o| w.file.ends_with(o.as_str()) || &w.name == o)
                {
                    disabled_skips.push((
                        w.file.clone(),
                        format!("{} on GitHub", state.replace('_', " ")),
                    ));
                }
                false
            }
            None => true,
        });
    }
    let mut p = plan::build(wfs, &ev, &t.workflows);
    p.skipped.extend(disabled_skips);
    if !t.jobs.is_empty() {
        keep_jobs(&mut p, &t.jobs);
    }
    let dirty = git::is_dirty(ctx, &root);
    let mut workflow_dirs = config
        .workflows_dirs
        .clone()
        .unwrap_or_else(|| vec![config::DEFAULT_WORKFLOWS_DIR.to_string()]);
    workflow_dirs.extend(t.workflows_dirs.iter().map(|d| plan::norm_dir(d)));
    Ok((
        Repo {
            root,
            sha,
            base,
            head,
            changed,
            dirty,
            all: t.all,
            workflow_dirs,
            default_workflows_dir: config.default_workflows_dir().to_string(),
            disabled: record,
            no_cache: config.no_cache,
            link_node_modules: config.link_node_modules,
            ignore_env: config.ignore_env,
        },
        p,
    ))
}

/// Workflows that are not `active` on GitHub (`disabled_manually`, ...).
/// `None` when gh or the network is not available.
fn disabled_workflows(ctx: &ExecContext, root: &std::path::Path) -> Option<Vec<DisabledWorkflow>> {
    let slug = git::repo_slug(ctx, root)?;
    let gh = ctx.find_tool("gh")?;
    let child = ctx
        .command(gh)
        .args([
            "api",
            &format!("repos/{slug}/actions/workflows?per_page=100"),
            "-q",
            r#".workflows[] | select(.state != "active") | "\(.path)\t\(.state)""#,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return None };
    let t0 = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if t0.elapsed() < std::time::Duration::from_secs(10) => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let Ok(out) = child.wait_with_output() else {
        return None;
    };
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                l.split_once('\t').map(|(p, s)| DisabledWorkflow {
                    path: p.to_string(),
                    state: s.to_string(),
                })
            })
            .collect(),
    )
}

/// Keep only the named jobs and the jobs they need.
fn keep_jobs(p: &mut plan::Plan, wanted: &[String]) {
    for u in &mut p.units {
        let w = &p.selected[u.wf].wf;
        let mut keep: Vec<String> = u
            .jobs
            .iter()
            .filter(|j| wanted.contains(j))
            .cloned()
            .collect();
        let mut i = 0;
        while i < keep.len() {
            if let Some(j) = w.jobs.iter().find(|j| j.id == keep[i]) {
                for n in &j.needs {
                    if !keep.contains(n) {
                        keep.push(n.clone());
                    }
                }
            }
            i += 1;
        }
        u.jobs.retain(|j| keep.contains(j));
    }
    p.units.retain(|u| !u.jobs.is_empty());
}

/// One key per repository, shared by all of its worktrees.
fn repo_key(ctx: &ExecContext, root: &std::path::Path) -> String {
    use std::hash::{Hash, Hasher};
    let common = git::git(
        ctx,
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .unwrap_or_else(|_| root.to_string_lossy().into_owned());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    common.hash(&mut h);
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{name}-{:08x}", h.finish() as u32)
}

/// Post the `local-ci` commit status with gh. Err explains why it was not posted.
pub fn post_status(
    ctx: &ExecContext,
    root: &std::path::Path,
    sha: &str,
    state: &str,
    description: &str,
) -> Result<()> {
    let slug = git::repo_slug(ctx, root).context("no origin remote")?;
    let gh = ctx.find_tool("gh").context("gh is not installed")?;
    let out = ctx
        .command(gh)
        .args([
            "api",
            &format!("repos/{slug}/statuses/{sha}"),
            "-f",
            &format!("state={}", report::github_state(state)),
            "-f",
            "context=local-ci",
            "-f",
            &format!("description={description}"),
        ])
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    let err =
        String::from_utf8_lossy(&out.stderr).to_string() + &String::from_utf8_lossy(&out.stdout);
    if err.contains("No commit found") || err.contains("422") {
        bail!(
            "{} is not on GitHub yet; run `local-ci status` after pushing",
            &sha[..7]
        );
    }
    bail!("gh api failed: {}", err.lines().next().unwrap_or("").trim())
}

fn prune_runs(dir: &std::path::Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut runs: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    runs.sort();
    if runs.len() > keep {
        for p in &runs[..runs.len() - keep] {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

/// Plan what `t` triggers and fix it. Reads the repository (and asks `gh`
/// which workflows are disabled, unless the target says otherwise) but starts
/// no job, touches no results cache and creates no worktree.
pub fn build_frozen_plan(
    ctx: &ExecContext,
    t: &Target,
    obs: &mut dyn Observer,
) -> Result<FrozenPlan> {
    build_frozen_plan_with(ctx, t, None, obs)
}

/// [`build_frozen_plan`] with the disabled workflows given: `Some(list)` plans
/// without asking `gh` or the network, so the plan depends on its inputs only;
/// the list and its source are recorded in the plan.
pub fn build_frozen_plan_with(
    ctx: &ExecContext,
    t: &Target,
    disabled: Option<Vec<DisabledWorkflow>>,
    obs: &mut dyn Observer,
) -> Result<FrozenPlan> {
    let (repo, p) = resolve_with(ctx, t, disabled, obs)?;
    freeze(ctx, &repo, p, t)
}

/// Fix an already resolved plan (what [`build_frozen_plan`] does after
/// [`resolve`]).
pub fn freeze(ctx: &ExecContext, repo: &Repo, p: plan::Plan, t: &Target) -> Result<FrozenPlan> {
    FrozenPlan::new(PlanParts {
        repository: git::repo_slug(ctx, &repo.root),
        source: SourceRef {
            source_id: format!("commit:{}", repo.sha),
            execution_commit: repo.sha.clone(),
            base: repo.base.clone(),
        },
        branch: repo.head.clone(),
        changed_files: repo.changed.clone(),
        config: FrozenConfig {
            workflow_dirs: repo.workflow_dirs.clone(),
            default_workflows_dir: repo.default_workflows_dir.clone(),
            no_cache: repo.no_cache.clone(),
            link_node_modules: repo.link_node_modules,
            ignore_env: repo.ignore_env.clone(),
        },
        filters: Filters {
            all: t.all,
            include_disabled: t.include_disabled,
            workflows_dirs: t.workflows_dirs.iter().map(|d| plan::norm_dir(d)).collect(),
            workflows: t.workflows.clone(),
            jobs: t.jobs.clone(),
        },
        disabled: repo.disabled.clone(),
        plan: p,
    })
}

/// Run what `fp` says in `ws` and report. Nothing is decided again: the units,
/// their order and the setup are the plan's. The attempt's processes and
/// containers are tracked in `ctx.cancel`; the containers are gone when this
/// returns, the workspace is left for the caller to remove.
pub fn execute_plan(
    ctx: &ExecContext,
    fp: &FrozenPlan,
    ws: &exec::Workspace,
    opts: exec::Opts,
) -> Result<report::RunResult> {
    let t0 = std::time::Instant::now();
    let nothing = fp.units.is_empty();
    if nothing {
        return Ok(report::assemble(
            fp,
            ws.run_dir.clone(),
            vec![],
            vec![],
            t0.elapsed().as_secs_f64(),
        ));
    }
    let keep = opts.keep;
    let mut runner = exec::Runner::new(ctx, fp, ws, opts);
    let result = (|| -> Result<_> {
        runner.prepare_tools()?;
        // Each task has a recipe of its own. This run does them in the one
        // workspace, each step once, and tells every task which of its steps
        // that covered; a template key shared by several tasks is prepared
        // (and receipted) once.
        let recipes = runner.task_recipes()?;
        let run_id = ws
            .run_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tctx = setup::TemplateContext::local(fp, &run_id);
        let all = setup::SetupRecipe::union(&recipes.iter().map(|(_, r)| r).collect::<Vec<_>>());
        let done = runner.run_recipe(&all, all.template_key(&tctx).digest());
        let ok = done.ok();
        let mut by_key: std::collections::HashMap<String, setup::PreparedReceipt> =
            Default::default();
        let mut receipts = std::collections::HashMap::new();
        for (task, recipe) in &recipes {
            let key = recipe.template_key(&tctx);
            let r = by_key
                .entry(key.digest().to_string())
                .or_insert_with(|| done.receipt.restricted_to(recipe, &key));
            receipts.insert(task.clone(), r.clone());
        }
        runner.set_receipts(receipts, tctx);
        let setup = done.steps;
        let units = if ok && !ctx.cancel.is_cancelled() {
            runner.run_units()
        } else {
            vec![]
        };
        Ok((setup, units))
    })();
    ctx.cancel.remove_containers(ctx);
    if !keep {
        runner.release_ports(&ws.root);
    }
    let (setup, units) = result?;
    Ok(report::assemble(
        fp,
        ws.run_dir.clone(),
        setup,
        units,
        t0.elapsed().as_secs_f64(),
    ))
}

pub fn run(ctx: &ExecContext, a: RunOptions, obs: &mut dyn Observer) -> Result<RunReport> {
    let t_start = std::time::Instant::now();
    let mut full = build_frozen_plan(ctx, &a.target, obs)?;
    if let Some(m) = &a.source {
        if m.execution_commit.as_deref() != Some(full.source.execution_commit.as_str()) {
            bail!(
                "the target is {}, not the execution commit of source {}",
                &full.source.execution_commit[..7],
                m.source_id
            );
        }
        full = full.with_source_id(&m.source_id)?;
    }
    let root = git::toplevel(ctx, &a.target.dir)?;
    let cache = a.cache_dir.clone();
    let key = repo_key(ctx, &root);
    let runs = cache.join("runs").join(&key);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let full_plan = full.to_plan();
    // Units whose result from the last day still holds are not run.
    let store = cache::Store::new(cache.join("results").join(&key));
    store.prune(stamp);
    let sha = full.source.execution_commit.as_str();
    let fingerprints: Vec<Option<String>> = if full.units.is_empty() {
        vec![]
    } else {
        match cache::Inputs::read(
            ctx,
            &root,
            sha,
            &full.source.base,
            full.branch.as_deref(),
            &full.config.workflow_dirs,
            &full.config.no_cache,
        ) {
            Ok(inputs) => full_plan
                .units
                .iter()
                .map(|u| {
                    let s = &full_plan.selected[u.wf];
                    inputs.fingerprint(&s.wf, &s.event, u)
                })
                .collect(),
            Err(e) => {
                obs.notice(&format!("not reusing results: {e:#}"));
                vec![None; full.units.len()]
            }
        }
    };
    let mut p = full_plan;
    let mut reused = Vec::new();
    let mut to_run = Vec::new();
    // where each unit still to run stands in the whole plan
    let mut run_idx = Vec::new();
    let mut run_fps = Vec::new();
    for (i, (u, fp)) in std::mem::take(&mut p.units)
        .into_iter()
        .zip(fingerprints)
        .enumerate()
    {
        let hit = fp
            .as_deref()
            .filter(|_| !a.no_cache)
            .and_then(|f| store.get(f, &p.selected[u.wf].wf.file, stamp));
        match hit {
            Some(r) => reused.push(exec::UnitResult { unit: Some(i), ..r }),
            None => {
                run_idx.push(i);
                to_run.push(u);
                run_fps.push(fp);
            }
        }
    }
    p.units = to_run;
    // what is left to run, fixed again
    let fp = full.with_plan(p)?;
    let run_dir = runs.join(format!("{stamp}-{}", &sha[..7]));
    std::fs::create_dir_all(&run_dir)?;
    let source_info = a.source.as_ref().map(SourceInfo::of);
    if let Some(info) = &source_info {
        exec::write_json(&run_dir.join("source.json"), info)?;
    }
    std::fs::create_dir_all(cache.join("locks"))?;
    if git::is_dirty(ctx, &root) {
        obs.notice(&format!(
            "uncommitted changes are not tested; testing commit {}",
            &sha[..7]
        ));
    }

    let (setup, units) = if fp.units.is_empty() {
        (vec![], vec![])
    } else {
        let name = format!("{}-{}", &sha[..7], std::process::id());
        let wt = cache.join("wt").join(&name);
        let tools = cache.join("wt").join(format!("{name}.tools"));
        std::fs::create_dir_all(cache.join("wt"))?;
        match &a.source {
            // the captured bytes, not a checkout of the commit
            Some(m) => source::add_worktree(
                ctx,
                &source::ObjectStore::open(cache.join("sources"))?,
                &root,
                &wt,
                m,
            )?,
            None => git::add_worktree(ctx, &root, &wt, sha)?,
        }
        obs.executing();
        exec::remove_orphan_containers(ctx);
        let canirun = if a.no_canirun {
            None
        } else {
            ctx.find_tool("canirun")
        };
        let parallel = a.parallel.unwrap_or(if canirun.is_some() {
            fp.units.len().clamp(1, 16)
        } else {
            4
        });
        let opts = exec::Opts {
            parallel,
            canirun,
            portool: if a.no_portool {
                None
            } else {
                ctx.find_tool("portool")
            },
            containers: a.containers,
            job_timeout_min: a.job_timeout,
            keep: a.keep,
            trash: cache.join("trash"),
        };
        let ws = exec::Workspace {
            root: wt.clone(),
            repo_root: root.clone(),
            run_dir: run_dir.clone(),
            tools: tools.clone(),
            lock_file: cache.join("locks").join(format!("{key}.lock")),
        };
        let result = execute_plan(ctx, &fp, &ws, opts);
        if a.keep {
            obs.notice(&format!("kept worktree {}", report::tilde(&wt, ctx.home())));
            if fp.units.len() > 1 {
                obs.notice(
                    "and the copies of units and matrix entries beside it (`-u<n>-`, `-c<n>-`)",
                );
            }
        } else {
            git::remove_worktree(ctx, &root, &wt, &cache.join("trash"));
            let _ = std::fs::remove_dir_all(&tools);
        }
        let r = result?;
        (r.setup, r.units)
    };
    if !ctx.cancel.is_cancelled() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        for (u, fp_) in units.iter().zip(&run_fps) {
            if let Some(f) = fp_ {
                store.put(f, sha, u, now);
            }
        }
    }
    // The units that ran were numbered in the part of the plan that was run;
    // the result is checked against the whole plan.
    let units: Vec<exec::UnitResult> = reused
        .into_iter()
        .chain(units.into_iter().map(|u| exec::UnitResult {
            unit: u.unit.and_then(|i| run_idx.get(i).copied()),
            ..u
        }))
        .collect();
    let result = report::assemble(
        &full,
        run_dir.clone(),
        setup,
        units,
        t_start.elapsed().as_secs_f64(),
    );
    exec::write_json(&run_dir.join("result.json"), &result)?;
    std::fs::write(runs.join("last"), run_dir.to_string_lossy().as_bytes())?;
    prune_runs(&runs, 20);
    let cancelled = ctx.cancel.is_cancelled();
    Ok(RunReport {
        result,
        repo_root: root,
        cancelled,
        style: report::Style {
            workflows_dir: full.config.default_workflows_dir.clone(),
            home: ctx.home().map(String::from),
        },
        source: source_info,
    })
}

/// The commit a result may be posted to as its `local-ci` status.
///
/// A run of a plain commit (`info` is `None`) posts to the commit it tested.
/// A run of a captured source posts to the commit it was captured from, and
/// only when what was tested is what that commit holds; otherwise `Err` says
/// why not. Success of a dirty worktree, a synthetic commit or a directory
/// is never success of the original `HEAD`.
pub fn publish_target(
    ctx: &ExecContext,
    root: &std::path::Path,
    tested_sha: &str,
    info: Option<&SourceInfo>,
) -> std::result::Result<String, String> {
    let Some(info) = info else {
        return Ok(tested_sha.to_string());
    };
    let target = match info.source_kind {
        SourceKind::Worktree => info.original_head.clone(),
        SourceKind::Commit => info.execution_commit.clone(),
        SourceKind::Directory => None,
    };
    let Some(target) = target else {
        return Err("the run tested a directory snapshot, not a commit".into());
    };
    source::publishable(ctx, root, info, &target)?;
    Ok(target)
}

/// Record the last run's result as the `local-ci` commit status.
/// Returns the state and the short sha it was posted for.
pub fn status(
    ctx: &ExecContext,
    dir: &std::path::Path,
    sha: &str,
    cache_dir: &std::path::Path,
) -> Result<(String, String)> {
    let root = git::toplevel(ctx, dir)?;
    let want = git::rev(ctx, &root, sha)?;
    let last = cache_dir
        .join("runs")
        .join(repo_key(ctx, &root))
        .join("last");
    let run_dir =
        std::fs::read_to_string(&last).context("no local-ci run recorded for this repository")?;
    let saved: report::Saved = serde_json::from_str(&std::fs::read_to_string(
        PathBuf::from(run_dir.trim()).join("result.json"),
    )?)?;
    let run_dir = PathBuf::from(run_dir.trim());
    let info: Option<SourceInfo> = match std::fs::read_to_string(run_dir.join("source.json")) {
        Ok(t) => Some(serde_json::from_str(&t).context("source.json is not readable")?),
        Err(_) => None,
    };
    match &info {
        None if saved.sha != want => bail!(
            "the last run tested {}, not {}; run `local-ci run` again",
            &saved.sha[..7],
            &want[..7]
        ),
        None => {}
        Some(i) => {
            // the run was over a captured source: it speaks for `want` only
            // when it tested the same content
            if let Err(why) = source::publishable(ctx, &root, i, &want) {
                bail!("not posting: {why}");
            }
        }
    }
    post_status(ctx, &root, &want, &saved.state, &saved.description)?;
    Ok((
        report::github_state(&saved.state).to_string(),
        want[..7].to_string(),
    ))
}
