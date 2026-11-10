//! Command-line adapter of `local-ci`: parses arguments, prints, picks the exit code.
//! The one place that reads the process environment.

#![allow(clippy::disallowed_methods)]

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use local_ci::{
    plan, report, workflow, CancelHandle, ExecContext, Observer, Repo, RunOptions, Target,
};
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
    /// Units to run at once (default: all of them with task-traffic-controller, else 4)
    #[arg(long)]
    parallel: Option<usize>,
    /// Do not wrap steps in `task-traffic-controller exec`
    #[arg(long, alias = "no-canirun")]
    no_traffic_controller: bool,
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
struct Stderr {
    cancel: CancelHandle,
}
impl Observer for Stderr {
    fn notice(&mut self, msg: &str) {
        eprintln!("local-ci: {msg}");
    }
    fn executing(&mut self) {
        // Ctrl-C stops the run this process is making
        let cancel = self.cancel.clone();
        ctrlc::set_handler(move || cancel.cancel()).ok();
    }
}

/// The context of this process: its environment, and a cancel handle for the one run it makes.
fn context() -> ExecContext {
    ExecContext::new(
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect(),
    )
}

fn run(ctx: &ExecContext, a: RunArgs) -> Result<i32> {
    let (json, tail, no_status) = (a.json, a.tail, a.no_status);
    let opts = RunOptions {
        target: a.target.into_target()?,
        parallel: a.parallel,
        no_traffic_controller: a.no_traffic_controller,
        no_portool: a.no_portool,
        containers: a.containers,
        keep: a.keep,
        job_timeout: a.job_timeout,
        no_cache: a.no_cache,
        cache_dir: ctx.cache_dir(),
        source: None,
    };
    let rep = local_ci::run(
        ctx,
        opts,
        &mut Stderr {
            cancel: ctx.cancel.clone(),
        },
    )?;
    let result = &rep.result;
    if json {
        println!("{}", serde_json::to_string_pretty(result)?);
    } else {
        report::print(result, tail, &rep.style);
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
            // a run over a captured source posts only to a commit whose
            // content it tested
            let posted =
                local_ci::publish_target(ctx, &rep.repo_root, &result.sha, rep.source.as_ref())
                    .and_then(|target| {
                        local_ci::post_status(
                            ctx,
                            &rep.repo_root,
                            &target,
                            &result.state,
                            &result.description,
                        )
                        .map(|()| target)
                        .map_err(|e| format!("{e:#}"))
                    });
            match posted {
                Ok(target) => println!(
                    "status: local-ci={} posted to {}",
                    report::github_state(&result.state),
                    &target[..7]
                ),
                Err(e) => println!("status: not posted ({e})"),
            }
        }
    }
    Ok(if cancelled {
        130
    } else {
        match result.state.as_str() {
            // `no_work` (the changes trigger no workflow) keeps the exit code a
            // run with nothing to do always had
            "success" | "no_work" => 0,
            "failure" => 1,
            // checked what it could, and left out what local-ci cannot reproduce
            "incomplete" => 3,
            _ => 2,
        }
    })
}

fn status(ctx: &ExecContext, dir: Option<PathBuf>, sha: &str) -> Result<i32> {
    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let (state, short) = local_ci::status(ctx, &dir, sha, &ctx.cache_dir())?;
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

fn print_plan(r: &Repo, p: &plan::Plan, fp: &local_ci::FrozenPlan) {
    let dir = r.default_workflows_dir.as_str();
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
    let mut recipes: Vec<local_ci::setup::SetupRecipe> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for t in &fp.tasks {
        match local_ci::setup::derive_setup_recipe(fp, &t.id) {
            Ok(r) => recipes.push(r),
            Err(e) => failed.push((t.id.clone(), format!("{e:#}"))),
        }
    }
    for (task, why) in &failed {
        println!("setup of {task} could not be worked out: {why}");
    }
    let all = local_ci::setup::SetupRecipe::union(&recipes.iter().collect::<Vec<_>>());
    let covered = local_ci::setup::covered_by(&all);
    if !all.steps.is_empty() {
        let s: Vec<String> = all
            .steps
            .iter()
            .filter(|s| !covered.contains_key(&s.id))
            .map(|s| {
                format!(
                    "{}$ {}",
                    if s.dir.is_empty() { "." } else { &s.dir },
                    s.label
                )
            })
            .collect();
        println!("setup (once): {}", s.join(" | "));
    }
    for l in &all.left_in_place {
        println!(
            "setup stays in {}: {} ({})",
            l.job,
            l.label,
            l.reason.describe()
        );
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
            report::short(dir, &w.file),
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
        println!("not triggered ({})", report::group_skipped(&skipped, dir));
    }
}

fn main() {
    let cli = Cli::parse();
    let ctx = context();
    let res = match cli.cmd {
        Cmd::Plan { target } => target.into_target().and_then(|t| {
            let (r, p) = local_ci::resolve(
                &ctx,
                &t,
                &mut Stderr {
                    cancel: ctx.cancel.clone(),
                },
            )?;
            let fp = local_ci::freeze(&ctx, &r, p.clone(), &t)?;
            print_plan(&r, &p, &fp);
            Ok(0)
        }),
        Cmd::Run(a) => run(&ctx, a),
        Cmd::Status { dir, sha } => status(&ctx, dir, &sha),
    };
    let code = res.unwrap_or_else(|e| {
        eprintln!("local-ci: {e:#}");
        64
    });
    std::process::exit(code);
}
