//! Resolving what a commit triggers, running it, and recording the result.
//!
//! The functions here do not print, read the process's working directory or
//! exit; everything they need comes in as arguments and everything they have
//! to say goes out through the return value or a [`Observer`].

use crate::{cache, config, exec, filter, git, plan, report, workflow};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// What the caller wants told while a run goes on.
pub trait Observer {
    /// A warning or progress line (no `local-ci:` prefix).
    fn notice(&mut self, _msg: &str) {}
    /// The run is about to start processes; the caller may install its signal
    /// handler now (see [`crate::cancel`]).
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
    /// Where runs, results, worktrees and locks live (see [`cache_dir`]).
    pub cache_dir: PathBuf,
}

/// What [`run`] produced; the caller prints it and picks the exit code.
pub struct RunReport {
    pub result: report::RunResult,
    pub repo_root: PathBuf,
    pub cancelled: bool,
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
}

pub fn resolve(t: &Target, obs: &mut dyn Observer) -> Result<(Repo, plan::Plan)> {
    let root = git::toplevel(&t.dir)?;
    let sha = git::rev(&root, &t.sha)?;
    let base = match &t.base {
        Some(b) => b.clone(),
        None => git::default_base(&root)?,
    };
    let changed = git::changed_files(&root, &base, &sha)?;
    let head = if t.sha == "HEAD" {
        git::current_branch(&root)
    } else {
        None
    };
    let config = config::at(&root, &sha)?;
    report::set_default_workflows_dir(config.default_workflows_dir());
    let mut wfs = Vec::new();
    for (file, text) in git::workflows_at(
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
        base: git::base_branch_name(&root, &base),
        head: head.clone(),
        changed: changed.clone(),
        all: t.all,
    };
    // GitHub does not run workflows that are disabled there; neither does the default mode.
    let mut disabled_skips = Vec::new();
    if !t.all && !t.include_disabled {
        let disabled = disabled_workflows(&root);
        wfs.retain(
            |w| match disabled.iter().find(|(path, _)| path == &w.file) {
                Some((_, state)) => {
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
            },
        );
    }
    let mut p = plan::build(wfs, &ev, &t.workflows);
    p.skipped.extend(disabled_skips);
    if !t.jobs.is_empty() {
        keep_jobs(&mut p, &t.jobs);
    }
    let dirty = git::is_dirty(&root);
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
            no_cache: config.no_cache,
            link_node_modules: config.link_node_modules,
        },
        p,
    ))
}

/// Workflows that are not `active` on GitHub (`disabled_manually`, ...), as (path, state).
/// Empty when gh or the network is not available.
fn disabled_workflows(root: &std::path::Path) -> Vec<(String, String)> {
    let Some(slug) = git::repo_slug(root) else {
        return vec![];
    };
    if find_tool("gh").is_none() {
        return vec![];
    }
    let child = std::process::Command::new("gh")
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
    let Ok(mut child) = child else { return vec![] };
    let t0 = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if t0.elapsed() < std::time::Duration::from_secs(10) => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            _ => {
                let _ = child.kill();
                return vec![];
            }
        }
    }
    let Ok(out) = child.wait_with_output() else {
        return vec![];
    };
    if !out.status.success() {
        return vec![];
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            l.split_once('\t')
                .map(|(p, s)| (p.to_string(), s.to_string()))
        })
        .collect()
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
    // worked out again rather than filtered: an install kept from a job left
    // out may be the one that runs, its twin in a kept job marked not to
    plan::fill_setup(p);
}

/// One key per repository, shared by all of its worktrees.
fn repo_key(root: &std::path::Path) -> String {
    use std::hash::{Hash, Hasher};
    let common = git::git(
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

pub fn find_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    for d in path.split(':').filter(|d| !d.is_empty()) {
        let p = PathBuf::from(d).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    let p = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".cargo/bin")
        .join(name);
    p.is_file().then_some(p)
}

/// Post the `local-ci` commit status with gh. Err explains why it was not posted.
pub fn post_status(
    root: &std::path::Path,
    sha: &str,
    state: &str,
    description: &str,
) -> Result<()> {
    let slug = git::repo_slug(root).context("no origin remote")?;
    find_tool("gh").context("gh is not installed")?;
    let out = std::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/{slug}/statuses/{sha}"),
            "-f",
            &format!("state={state}"),
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

pub fn run(a: RunOptions, obs: &mut dyn Observer) -> Result<RunReport> {
    let t_start = std::time::Instant::now();
    let (repo, mut p) = resolve(&a.target, obs)?;
    let cache = a.cache_dir.clone();
    let key = repo_key(&repo.root);
    let runs = cache.join("runs").join(&key);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let nothing_triggered = p.units.is_empty();
    // Units whose result from the last day still holds are not run.
    let store = cache::Store::new(cache.join("results").join(&key));
    store.prune(stamp);
    let fingerprints: Vec<Option<String>> = if p.units.is_empty() {
        vec![]
    } else {
        match cache::Inputs::read(
            &repo.root,
            &repo.sha,
            &repo.base,
            repo.head.as_deref(),
            &repo.workflow_dirs,
            &repo.no_cache,
        ) {
            Ok(inputs) => p
                .units
                .iter()
                .map(|u| {
                    let s = &p.selected[u.wf];
                    inputs.fingerprint(&s.wf, &s.event, u)
                })
                .collect(),
            Err(e) => {
                obs.notice(&format!("not reusing results: {e:#}"));
                vec![None; p.units.len()]
            }
        }
    };
    let mut reused = Vec::new();
    let mut to_run = Vec::new();
    let mut run_fps = Vec::new();
    for (u, fp) in std::mem::take(&mut p.units).into_iter().zip(fingerprints) {
        let hit = fp
            .as_deref()
            .filter(|_| !a.no_cache)
            .and_then(|f| store.get(f, &p.selected[u.wf].wf.file, stamp));
        match hit {
            Some(r) => reused.push(r),
            None => {
                to_run.push(u);
                run_fps.push(fp);
            }
        }
    }
    p.units = to_run;
    if !reused.is_empty() {
        plan::fill_setup(&mut p);
    }
    let run_dir = runs.join(format!("{stamp}-{}", &repo.sha[..7]));
    std::fs::create_dir_all(&run_dir)?;
    std::fs::create_dir_all(cache.join("locks"))?;
    if repo.dirty {
        obs.notice(&format!(
            "uncommitted changes are not tested; testing commit {}",
            &repo.sha[..7]
        ));
    }
    let skipped: Vec<report::Skipped> = p
        .skipped
        .iter()
        .map(|(w, r)| report::Skipped {
            workflow: w.clone(),
            reason: r.clone(),
        })
        .collect();

    let (setup, units) = if p.units.is_empty() {
        (vec![], vec![])
    } else {
        let name = format!("{}-{}", &repo.sha[..7], std::process::id());
        let wt = cache.join("wt").join(&name);
        let tools = cache.join("wt").join(format!("{name}.tools"));
        std::fs::create_dir_all(cache.join("wt"))?;
        git::add_worktree(&repo.root, &wt, &repo.sha)?;
        obs.executing();
        exec::remove_orphan_containers();
        let canirun = if a.no_canirun {
            None
        } else {
            find_tool("canirun")
        };
        let parallel = a.parallel.unwrap_or(if canirun.is_some() {
            p.units.len().clamp(1, 16)
        } else {
            4
        });
        let opts = exec::Opts {
            parallel,
            canirun,
            portool: if a.no_portool {
                None
            } else {
                find_tool("portool")
            },
            containers: a.containers,
            job_timeout_min: a.job_timeout,
            keep: a.keep,
            trash: cache.join("trash"),
        };
        let slug = git::repo_slug(&repo.root).unwrap_or_default();
        let runner = exec::Runner::new(
            &repo,
            &p,
            wt.clone(),
            run_dir.clone(),
            tools.clone(),
            cache.join("locks").join(format!("{key}.lock")),
            slug,
            opts,
        );
        let result = (|| -> Result<_> {
            runner.prepare_tools()?;
            let setup = runner.run_setup();
            let ok = setup.iter().all(|s| s.outcome == exec::Outcome::Pass);
            let units = if ok && !exec::CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
                runner.run_units()
            } else {
                vec![]
            };
            Ok((setup, units))
        })();
        exec::remove_containers();
        if a.keep {
            obs.notice(&format!("kept worktree {}", report::tilde(&wt)));
            if p.units.len() > 1 {
                obs.notice(
                    "and the copies of units and matrix entries beside it (`-u<n>-`, `-c<n>-`)",
                );
            }
        } else {
            runner.release_ports(&wt);
            git::remove_worktree(&repo.root, &wt, &cache.join("trash"));
            let _ = std::fs::remove_dir_all(&tools);
        }
        result?
    };
    if !exec::CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        for (u, fp) in units.iter().zip(&run_fps) {
            if let Some(fp) = fp {
                store.put(fp, &repo.sha, u, now);
            }
        }
    }
    let units: Vec<exec::UnitResult> = reused.into_iter().chain(units).collect();
    let state = report::state(&setup, &units);
    let not_covered = report::not_covered(&units);
    let description = report::description(state, &units, nothing_triggered, not_covered.len());
    let result = report::RunResult {
        version: env!("CARGO_PKG_VERSION").into(),
        sha: repo.sha.clone(),
        base: repo.base.clone(),
        changed_files: repo.changed.len(),
        mode: if repo.all { "all" } else { "changes" }.into(),
        state: state.into(),
        description: description.clone(),
        secs: t_start.elapsed().as_secs_f64(),
        run_dir: run_dir.clone(),
        warnings: report::warnings(&units),
        setup,
        units,
        skipped_workflows: skipped,
        not_covered,
    };
    exec::write_json(&run_dir.join("result.json"), &result)?;
    std::fs::write(runs.join("last"), run_dir.to_string_lossy().as_bytes())?;
    prune_runs(&runs, 20);
    let cancelled = exec::CANCEL.load(std::sync::atomic::Ordering::SeqCst);
    Ok(RunReport {
        result,
        repo_root: repo.root,
        cancelled,
    })
}

/// Record the last run's result as the `local-ci` commit status.
/// Returns the state and the short sha it was posted for.
pub fn status(
    dir: &std::path::Path,
    sha: &str,
    cache_dir: &std::path::Path,
) -> Result<(String, String)> {
    let root = git::toplevel(dir)?;
    let want = git::rev(&root, sha)?;
    let last = cache_dir.join("runs").join(repo_key(&root)).join("last");
    let run_dir =
        std::fs::read_to_string(&last).context("no local-ci run recorded for this repository")?;
    let saved: report::Saved = serde_json::from_str(&std::fs::read_to_string(
        PathBuf::from(run_dir.trim()).join("result.json"),
    )?)?;
    if saved.sha != want {
        bail!(
            "the last run tested {}, not {}; run `local-ci run` again",
            &saved.sha[..7],
            &want[..7]
        );
    }
    post_status(&root, &saved.sha, &saved.state, &saved.description)?;
    Ok((saved.state.clone(), saved.sha[..7].to_string()))
}
