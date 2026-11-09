//! Command-line adapter of `local-ci`: parses arguments, prints, picks the exit code.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use local_ci::{plan, report, workflow, Observer, Repo, RunOptions, Target};
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
struct TargetArgs {
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
    /// Also read workflows from this directory (relative to the repository root), repeatable,
    /// besides `.github/workflows` (or the `workflows-dirs` of `.local-ci.toml`).
    /// Their triggers are applied as for the others
    #[arg(long = "workflows-dir", value_name = "DIR")]
    workflows_dirs: Vec<String>,
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

impl TargetArgs {
    fn into_target(self) -> Result<Target> {
        let dir = match self.dir {
            Some(d) => d,
            None => std::env::current_dir()?,
        };
        Ok(Target {
            base: self.base,
            sha: self.sha,
            all: self.all,
            include_disabled: self.include_disabled,
            workflows_dirs: self.workflows_dirs,
            workflows: self.workflows,
            jobs: self.jobs,
            dir,
        })
    }
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Units to run at once (default: all of them with canirun, else 4)
    #[arg(long)]
    parallel: Option<usize>,
    /// Do not wrap steps in `canirun exec`
    #[arg(long)]
    no_canirun: bool,
    /// Do not give jobs that start containers or test servers a port block from portool
    #[arg(long)]
    no_portool: bool,
    /// Jobs with containers or test servers that may run at once, across all local-ci runs
    #[arg(long, default_value_t = 2)]
    containers: usize,
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
    /// Run every unit even when a result from the last day covers it
    /// (results of this run are still kept)
    #[arg(long)]
    no_cache: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show which workflows and jobs would run, without running anything
    Plan {
        #[command(flatten)]
        target: TargetArgs,
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

/// Prints notices the way `local-ci` always has, and installs the Ctrl-C handler.
struct Stderr;
impl Observer for Stderr {
    fn notice(&mut self, msg: &str) {
        eprintln!("local-ci: {msg}");
    }
    fn executing(&mut self) {
        ctrlc::set_handler(local_ci::cancel).ok();
    }
}

fn run(a: RunArgs) -> Result<i32> {
    let (json, tail, no_status) = (a.json, a.tail, a.no_status);
    let opts = RunOptions {
        target: a.target.into_target()?,
        parallel: a.parallel,
        no_canirun: a.no_canirun,
        no_portool: a.no_portool,
        containers: a.containers,
        keep: a.keep,
        job_timeout: a.job_timeout,
        no_cache: a.no_cache,
        cache_dir: local_ci::cache_dir(),
    };
    let rep = local_ci::run(opts, &mut Stderr)?;
    let result = &rep.result;
    if json {
        println!("{}", serde_json::to_string_pretty(result)?);
    } else {
        report::print(result, tail);
    }
    let cancelled = rep.cancelled;
    if !json {
        if no_status || cancelled {
            println!(
                "status: not posted ({})",
                if cancelled {
                    "cancelled"
                } else {
                    "--no-status"
                }
            );
        } else {
            match local_ci::post_status(
                &rep.repo_root,
                &result.sha,
                &result.state,
                &result.description,
            ) {
                Ok(()) => println!(
                    "status: local-ci={} posted to {}",
                    result.state,
                    &result.sha[..7]
                ),
                Err(e) => println!("status: not posted ({e:#})"),
            }
        }
    }
    Ok(if cancelled {
        130
    } else {
        match result.state.as_str() {
            "success" => 0,
            "failure" => 1,
            _ => 2,
        }
    })
}

fn status(dir: Option<PathBuf>, sha: &str) -> Result<i32> {
    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let (state, short) = local_ci::status(&dir, sha, &local_ci::cache_dir())?;
    println!("status: local-ci={state} posted to {short}");
    Ok(0)
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
            report::short(&w.file),
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
        Cmd::Plan { target } => target
            .into_target()
            .and_then(|t| local_ci::resolve(&t, &mut Stderr))
            .map(|(r, p)| {
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
