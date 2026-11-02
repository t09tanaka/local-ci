mod exec;
mod expr;
mod filter;
mod git;
mod matrix;
mod plan;
mod report;
mod workflow;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Run the GitHub Actions jobs a branch would trigger, locally, and print a short report"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
pub struct Target {
    /// Compare against this ref (default: origin's default branch)
    #[arg(long)]
    base: Option<String>,
    /// Commit to test (default: HEAD)
    #[arg(long, default_value = "HEAD")]
    sha: String,
    /// Run every workflow a pull request can trigger, ignoring `paths` / `paths-ignore`
    /// (`branches` and `types` still apply). Default: only what these changes trigger
    #[arg(long)]
    all: bool,
    /// Also run workflows that are disabled on GitHub (`paths` still apply; `--all` implies this)
    #[arg(long)]
    include_disabled: bool,
    /// Only these workflows (file name or `name:`), repeatable
    #[arg(long = "workflow", short = 'w')]
    workflows: Vec<String>,
    /// Only these jobs (id), plus the jobs they need; repeatable
    #[arg(long = "job", short = 'j')]
    jobs: Vec<String>,
    /// Repository directory (default: current directory)
    #[arg(long, short = 'C')]
    dir: Option<PathBuf>,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    target: Target,
    /// Units to run at once (default: all of them with canirun, else 4)
    #[arg(long)]
    parallel: Option<usize>,
    /// Do not wrap steps in `canirun exec`
    #[arg(long)]
    no_canirun: bool,
    /// Do not record the result as the `local-ci` commit status
    #[arg(long)]
    no_status: bool,
    /// Keep the worktrees afterwards (their paths are printed)
    #[arg(long)]
    keep: bool,
    /// Lines of a failing step's log to print
    #[arg(long, default_value_t = 30)]
    tail: usize,
    /// Job timeout in minutes when the workflow sets none
    #[arg(long, default_value_t = 60.0)]
    job_timeout: f64,
    /// Print the result as JSON instead of the text report
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show which workflows and jobs would run, without running anything
    Plan {
        #[command(flatten)]
        target: Target,
    },
    /// Run them in a throwaway worktree of the commit and report
    Run(RunArgs),
    /// Record the last run's result as the `local-ci` commit status (after pushing)
    Status {
        /// Repository directory (default: current directory)
        #[arg(long, short = 'C')]
        dir: Option<PathBuf>,
        /// Only post if the last run tested this commit (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        sha: String,
    },
}

pub struct Repo {
    pub root: PathBuf,
    pub sha: String,
    pub base: String,
    pub head: Option<String>,
    pub changed: Vec<String>,
    pub dirty: bool,
    pub all: bool,
}

pub fn resolve(t: &Target) -> Result<(Repo, plan::Plan)> {
    let cwd = match &t.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };
    let root = git::toplevel(&cwd)?;
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
    let mut wfs = Vec::new();
    for (file, text) in git::workflows_at(&root, &sha)? {
        match workflow::parse(&file, &text) {
            Ok(w) => wfs.push(w),
            Err(e) => eprintln!("local-ci: skipping {file}: {e:#}"),
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
    Ok((
        Repo {
            root,
            sha,
            base,
            head,
            changed,
            dirty,
            all: t.all,
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
    let live: Vec<(usize, String)> = p
        .units
        .iter()
        .flat_map(|u| u.jobs.iter().map(move |j| (u.wf, j.clone())))
        .collect();
    p.setup.retain(|s| {
        live.contains(&(s.wf, s.job.clone()))
            || live.iter().any(|(wf, j)| {
                let w = &p.selected[*wf].wf;
                w.jobs.iter().find(|x| &x.id == j).is_some_and(|job| {
                    job.steps
                        .iter()
                        .any(|st| plan::step_key(w, job, st).as_ref() == Some(&s.key))
                })
            })
    });
}

pub fn cache_dir() -> PathBuf {
    if let Ok(d) = std::env::var("LOCAL_CI_HOME") {
        return PathBuf::from(d);
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
    if cfg!(target_os = "macos") {
        return home.join("Library/Caches/local-ci");
    }
    match std::env::var("XDG_CACHE_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x).join("local-ci"),
        _ => home.join(".cache/local-ci"),
    }
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

fn find_tool(name: &str) -> Option<PathBuf> {
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
fn post_status(root: &std::path::Path, sha: &str, state: &str, description: &str) -> Result<()> {
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

fn run(a: RunArgs) -> Result<i32> {
    let t_start = std::time::Instant::now();
    let (repo, p) = resolve(&a.target)?;
    let cache = cache_dir();
    let key = repo_key(&repo.root);
    let runs = cache.join("runs").join(&key);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let run_dir = runs.join(format!("{stamp}-{}", &repo.sha[..7]));
    std::fs::create_dir_all(&run_dir)?;
    std::fs::create_dir_all(cache.join("locks"))?;
    if repo.dirty {
        eprintln!(
            "local-ci: uncommitted changes are not tested; testing commit {}",
            &repo.sha[..7]
        );
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
        ctrlc::set_handler(exec::kill_all).ok();
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
            eprintln!("local-ci: kept worktree {}", report::tilde(&wt));
            if p.units.len() > 1 {
                eprintln!("local-ci: and each unit's copy of it beside it (`-u<n>-`)");
            }
        } else {
            git::remove_worktree(&repo.root, &wt, &cache.join("trash"));
            let _ = std::fs::remove_dir_all(&tools);
        }
        result?
    };
    let state = report::state(&setup, &units);
    let not_covered = report::not_covered(&units);
    let description = report::description(state, &units, p.units.is_empty(), not_covered.len());
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
    if a.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        report::print(&result, a.tail);
    }
    let cancelled = exec::CANCEL.load(std::sync::atomic::Ordering::SeqCst);
    if !a.json {
        if a.no_status || cancelled {
            println!(
                "status: not posted ({})",
                if cancelled {
                    "cancelled"
                } else {
                    "--no-status"
                }
            );
        } else {
            match post_status(&repo.root, &repo.sha, state, &description) {
                Ok(()) => println!("status: local-ci={state} posted to {}", &repo.sha[..7]),
                Err(e) => println!("status: not posted ({e:#})"),
            }
        }
    }
    Ok(if cancelled {
        130
    } else {
        match state {
            "success" => 0,
            "failure" => 1,
            _ => 2,
        }
    })
}

fn status(dir: Option<PathBuf>, sha: &str) -> Result<i32> {
    let cwd = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let root = git::toplevel(&cwd)?;
    let want = git::rev(&root, sha)?;
    let last = cache_dir().join("runs").join(repo_key(&root)).join("last");
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
    println!(
        "status: local-ci={} posted to {}",
        saved.state,
        &saved.sha[..7]
    );
    Ok(0)
}

fn short(f: &str) -> &str {
    f.rsplit('/').next().unwrap_or(f)
}

fn job_note(j: &workflow::Job) -> String {
    let mut n = Vec::new();
    if j.container {
        n.push("container: unsupported".to_string());
    }
    if let Some(u) = &j.reusable {
        n.push(format!("reusable workflow {u}: unsupported"));
    }
    match &j.matrix {
        Some(serde_yaml::Value::String(_)) => n.push("matrix from expression".into()),
        Some(m) => {
            let c = matrix_size(m);
            if c != 1 {
                n.push(format!("matrix ×{c}"));
            }
        }
        None => {}
    }
    if !j.services.is_empty() {
        n.push(format!(
            "services: {}",
            j.services
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if n.is_empty() {
        String::new()
    } else {
        format!(" ({})", n.join("; "))
    }
}

fn matrix_size(m: &serde_yaml::Value) -> usize {
    let Some(map) = m.as_mapping() else { return 1 };
    let mut n = 1usize;
    let mut base = false;
    for (k, v) in map {
        let key = k.as_str().unwrap_or("");
        if key == "include" || key == "exclude" {
            continue;
        }
        if let Some(seq) = v.as_sequence() {
            n *= seq.len().max(1);
            base = true;
        }
    }
    let inc = map
        .get(serde_yaml::Value::String("include".into()))
        .and_then(|v| v.as_sequence())
        .map(|s| s.len())
        .unwrap_or(0);
    if base {
        n
    } else {
        inc.max(1)
    }
}

fn print_plan(r: &Repo, p: &plan::Plan) {
    println!(
        "local-ci plan  {} vs {} ({} file{} changed, {}){}",
        &r.sha[..7],
        r.base,
        r.changed.len(),
        if r.changed.len() == 1 { "" } else { "s" },
        if r.all {
            "all checks"
        } else {
            "triggered by the changes"
        },
        if r.dirty {
            "  [uncommitted changes are not tested]"
        } else {
            ""
        }
    );
    if !p.setup.is_empty() {
        let s: Vec<String> = p
            .setup
            .iter()
            .filter(|s| s.execute)
            .map(|s| {
                format!(
                    "{}$ {}",
                    if s.key.wd.is_empty() { "." } else { &s.key.wd },
                    s.label
                )
            })
            .collect();
        println!("setup (once): {}", s.join(" | "));
    }
    for (i, u) in p.units.iter().enumerate() {
        let w = &p.selected[u.wf].wf;
        let jobs: Vec<String> = u
            .jobs
            .iter()
            .map(|id| format!("{id}{}", job_note(p.job(u.wf, id))))
            .collect();
        println!(
            "unit {:<2} {}: {}{}",
            i + 1,
            short(&w.file),
            jobs.join(" → "),
            if u.exclusive { "  [exclusive]" } else { "" }
        );
    }
    if !p.skipped.is_empty() {
        let skipped: Vec<report::Skipped> = p
            .skipped
            .iter()
            .map(|(w, r)| report::Skipped {
                workflow: w.clone(),
                reason: r.clone(),
            })
            .collect();
        println!("not triggered ({})", report::group_skipped(&skipped));
    }
}

fn main() {
    let cli = Cli::parse();
    let res = match cli.cmd {
        Cmd::Plan { target } => resolve(&target).map(|(r, p)| {
            print_plan(&r, &p);
            0
        }),
        Cmd::Run(a) => run(a),
        Cmd::Status { dir, sha } => status(dir, &sha),
    };
    let code = res.unwrap_or_else(|e| {
        eprintln!("local-ci: {e:#}");
        64
    });
    std::process::exit(code);
}
