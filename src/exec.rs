//! Running the plan: setup once, then every unit on its own thread.

use crate::artifact::{self};
use crate::artifact_io;
use crate::context::ExecContext;
use crate::dag::{self, ExecutionTask};
use crate::expr::{self, Ctx, Status};
use crate::frozen::FrozenPlan;
use crate::matrix;
use crate::plan::{self, Plan, Unit};
use crate::setup::{
    self, PreparedHow, PreparedReceipt, PreparedSetup, PreparedStep, SetupRecipe, TemplateContext,
};
use crate::task::{self, combine, outcome_of, JobOutcome, JobStatus, TaskInputs, TaskResult};
use crate::workflow::{Job, Service, Step, Workflow};
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Remove the service containers of `local-ci` runs that are gone: a run
/// killed before it cleaned up left its database behind on the service's
/// declared port, and later runs could not bind it. A container is named
/// `local-ci-<pid>-<n>` by the run that started it.
pub(crate) fn remove_orphan_containers(ctx: &ExecContext) {
    let Ok(out) = ctx
        .command("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=local-ci=1",
            "--format",
            "{{.Names}}",
        ])
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|n| orphan_owner(n).is_some_and(|pid| !alive(pid)))
        .map(String::from)
        .collect();
    if !names.is_empty() {
        let _ = ctx
            .command("docker")
            .args(["rm", "-f"])
            .args(&names)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The pid in a `local-ci-<pid>-<n>` container name.
fn orphan_owner(name: &str) -> Option<i32> {
    let rest = name.strip_prefix("local-ci-")?;
    let (pid, n) = rest.split_once('-')?;
    n.parse::<u64>().ok()?;
    pid.parse().ok()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let sent = unsafe { libc::kill(pid, 0) } == 0;
    sent || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", content = "reason", rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    Fail,
    Skip(String),
    /// Could not be checked here (tool missing, docker down, unsupported construct, timeout).
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub label: String,
    #[serde(flatten)]
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    pub secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub workflow: String,
    pub job: String,
    pub display: String,
    #[serde(flatten)]
    pub outcome: Outcome,
    pub secs: f64,
    pub steps: Vec<StepResult>,
    /// The run was cancelled while the job ran (its `outcome` is an error) or
    /// before it started (a skip). `needs.<id>.result` is `cancelled` then.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    /// Where the time went. Absent for jobs that did not run and for reused results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<JobTime>,
}

/// What a job waited for, and how long it really ran, in seconds.
///
/// `secs` of the job counts from the start of its steps and so holds the wait
/// for task-traffic-controller (`wait_queue_secs`); the other waits come before it. The waits
/// of a whole unit (`wait_lock_secs` for the repository lock, `wait_parallel_secs`
/// for a `--parallel` place) are charged to its first job, so that the sum over
/// jobs is not counted twice.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobTime {
    /// The job's own running, setup of its services and environment included:
    /// `secs` without `wait_queue_secs`.
    pub work_secs: f64,
    /// For the per-repository lock.
    pub wait_lock_secs: f64,
    /// For one of the `--containers` slots, shared by all runs on the machine.
    pub wait_slot_secs: f64,
    /// For task-traffic-controller to let a step start, summed over the steps.
    pub wait_queue_secs: f64,
    /// For one of the `--parallel` places of this run.
    pub wait_parallel_secs: f64,
}

/// The waits of a unit before any of its jobs started.
#[derive(Debug, Clone, Copy, Default)]
struct UnitWait {
    lock: f64,
    parallel: f64,
}

impl UnitWait {
    fn total(self) -> f64 {
        self.lock + self.parallel
    }
}

/// Charge the waits of a unit to its first job that ran (else its first).
fn charge(jobs: &mut [JobResult], w: UnitWait) {
    if w.total() <= 0.0 {
        return;
    }
    let at = jobs.iter().position(|j| j.time.is_some()).unwrap_or(0);
    if let Some(j) = jobs.get_mut(at) {
        let secs = j.secs;
        let t = j.time.get_or_insert_with(|| JobTime {
            work_secs: secs,
            ..JobTime::default()
        });
        t.wait_lock_secs += w.lock;
        t.wait_parallel_secs += w.parallel;
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UnitResult {
    /// The unit of the plan this is the result of (index into its `units`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<usize>,
    pub workflow: String,
    pub jobs: Vec<JobResult>,
    pub waited_secs: f64,
    /// Time spent copying the worktree for this unit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_secs: Option<f64>,
    /// Where the reused result comes from (`abc1234, 2h ago`), when the unit did not run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<String>,
    /// Each job of the unit as the jobs that need it saw it, in the order they
    /// ran. Empty for a reused result (which keeps no outputs).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<JobOutcome>,
}

pub struct Opts {
    pub parallel: usize,
    pub traffic_controller: Option<PathBuf>,
    /// Gives jobs that bind fixed ports a block of their own (`--no-portool` turns it off).
    pub portool: Option<PathBuf>,
    /// Exclusive jobs that may run at once, across all runs on the machine.
    pub containers: usize,
    pub job_timeout_min: f64,
    /// Leave the worktrees in place after the run (`--keep`).
    pub keep: bool,
    /// Where removed worktrees go before they are deleted in the background.
    pub trash: PathBuf,
}

/// Where one attempt runs: the host paths the plan is materialized at. The
/// plan itself holds no path of the host.
#[derive(Clone, Debug)]
pub struct Workspace {
    /// The checkout of the planned commit the jobs run in.
    pub root: PathBuf,
    /// The repository that checkout belongs to; copies for parallel units are
    /// made from it.
    pub repo_root: PathBuf,
    /// Where logs and the run's artifacts go. It must be new and unique to
    /// this run: [`crate::execute_plan`] refuses one that already has
    /// artifacts, and two runs in one directory would share (and destroy) one
    /// artifact area. [`crate::claim_run_dir`] makes one.
    pub run_dir: PathBuf,
    /// The attempt's own tool directory (shims, scripts, installed tools).
    pub tools: PathBuf,
    /// Held while exclusive units run.
    pub lock_file: PathBuf,
}

pub struct Runner<'a> {
    pub ctx: &'a ExecContext,
    pub fp: &'a FrozenPlan,
    /// The engine's view of the plan, worked out once from `fp`.
    pub plan: Plan,
    pub repo_root: PathBuf,
    pub wt: PathBuf,
    pub run_dir: PathBuf,
    pub tools: PathBuf,
    pub lock_file: PathBuf,
    pub slug: String,
    pub opts: Opts,
    pub base_path: String,
    /// The keys of the steps that count as setup, from the plan alone.
    hoisted: plan::Hoist,
    /// What each task was told is prepared, and the context its template keys
    /// are made under.
    receipts: Option<(HashMap<String, PreparedReceipt>, TemplateContext)>,
    seq: AtomicUsize,
    /// Copies that took a port block of their own.
    synced: Mutex<HashSet<PathBuf>>,
    docker_ok: OnceLock<Result<(), String>>,
    /// Why the units of a workflow (by index in the plan) are not run: an
    /// artifact step of it that cannot be run.
    refused: HashMap<usize, String>,
}

/// The descriptor a script holding the token is read from, and how a shell is
/// told to read it.
const PIPE_FD: i32 = 3;
const PIPE_SCRIPT: &str = "/dev/fd/3";

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

/// Replace `secret` in the file at `path` with `***`.
fn mask_in_file(path: &Path, secret: &str) {
    if secret.len() < 4 {
        return;
    }
    if let Ok(text) = fs::read_to_string(path) {
        if text.contains(secret) {
            let _ = fs::write(path, text.replace(secret, "***"));
        }
    }
}

fn slug_of(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(40).collect()
}

pub fn strip_ansi(s: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\r")
        .replace_all(s, "")
        .into_owned()
}

/// The last `n` non-empty lines of a log, without colour codes.
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(256 * 1024);
    let _ = f.seek(SeekFrom::Start(start));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = strip_ansi(&String::from_utf8_lossy(&buf));
    let lines: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !is_controller_line(l))
        .map(String::from)
        .collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

enum Exit {
    Code(i32),
    Timeout,
    Cancelled,
}

struct Semaphore {
    free: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn acquire(&self) {
        let mut f = self.free.lock().unwrap();
        while *f == 0 {
            f = self.cv.wait(f).unwrap();
        }
        *f -= 1;
    }
    fn release(&self) {
        *self.free.lock().unwrap() += 1;
        self.cv.notify_one();
    }
}

/// Compose projects a script brings up: `-p name` / `--project-name name` on a
/// `docker compose` / `docker-compose` command line, or `COMPOSE_PROJECT_NAME=name`.
pub fn compose_projects(script: &str) -> Vec<String> {
    static FLAG: OnceLock<Regex> = OnceLock::new();
    static ENV: OnceLock<Regex> = OnceLock::new();
    let flag = re(
        &FLAG,
        r"(?:^|\s)(?:-p|--project-name)(?:\s+|=)[\x22']?([A-Za-z0-9][A-Za-z0-9_-]*)",
    );
    let env = re(
        &ENV,
        r"\bCOMPOSE_PROJECT_NAME=[\x22']?([A-Za-z0-9][A-Za-z0-9_-]*)",
    );
    let mut out: Vec<String> = env
        .captures_iter(&script.replace("\\\n", " "))
        .map(|c| c[1].to_string())
        .collect();
    for cmd in compose_commands(script) {
        out.extend(
            flag.captures_iter(compose_globals(&cmd))
                .map(|c| c[1].to_string()),
        );
    }
    let mut seen = HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// The prefix task-traffic-controller puts on its own output lines. The one
/// place that knows it.
const CONTROLLER_PREFIX: &str = "task-traffic-controller: ";

/// A line of task-traffic-controller's own output, not the command's.
fn is_controller_line(line: &str) -> bool {
    line.starts_with(CONTROLLER_PREFIX)
}

/// Its ledger lock timing out, so the command never ran.
fn is_ledger_timeout_line(line: &str) -> bool {
    line.strip_prefix(CONTROLLER_PREFIX)
        .is_some_and(|r| r.starts_with("ledger: timed out"))
}

/// The log written since `from` holds only task-traffic-controller's own lines, ending in its
/// ledger lock timing out: the command itself never ran.
fn ledger_timed_out(log: &Path, from: u64) -> bool {
    let Ok(mut f) = File::open(log) else {
        return false;
    };
    let mut b = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.read_to_end(&mut b).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&b);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.iter().all(|l| is_controller_line(l)) && lines.iter().any(|l| is_ledger_timeout_line(l))
}

/// Each `docker compose` command line of a script, cut at the next command.
fn compose_commands(script: &str) -> Vec<String> {
    let joined = script.replace("\\\n", " ");
    let mut out = Vec::new();
    for line in joined.lines() {
        for marker in ["docker compose", "docker-compose"] {
            for (at, _) in line.match_indices(marker) {
                let rest = &line[at + marker.len()..];
                out.push(
                    rest.split(['|', ';', '&'])
                        .next()
                        .unwrap_or(rest)
                        .to_string(),
                );
            }
        }
    }
    out
}

/// The Compose files the script's `docker compose` commands read: their `-f`
/// files, or the default names. None for a file named by a variable.
fn compose_files(script: &str) -> Vec<Option<String>> {
    static WORD: OnceLock<Regex> = OnceLock::new();
    let mut out = Vec::new();
    for cmd in compose_commands(script) {
        let words: Vec<&str> = re(&WORD, r"\S+")
            .find_iter(compose_globals(&cmd))
            .map(|m| m.as_str())
            .collect();
        let mut named = false;
        for (i, w) in words.iter().enumerate() {
            let file = match *w {
                "-f" | "--file" => words.get(i + 1).copied(),
                w => w.strip_prefix("--file="),
            };
            if let Some(f) = file {
                named = true;
                let f = f.trim_matches(['"', '\'']);
                out.push((!f.contains('$')).then(|| f.to_string()));
            }
        }
        if !named {
            for f in [
                "compose.yaml",
                "compose.yml",
                "docker-compose.yaml",
                "docker-compose.yml",
            ] {
                out.push(Some(f.to_string()));
            }
        }
    }
    out
}

/// A Compose file that pins a container name or a host port.
fn compose_fixed(text: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r#"(?m)^\s*container_name\s*:|^\s*-\s*["']?(\d{1,3}(\.\d{1,3}){3}:)?\d{2,5}:\d|^\s*published\s*:\s*["']?\d"#,
    )
    .is_match(text)
}

/// `docker run` with a fixed name or host port.
fn docker_run_fixed(script: &str) -> bool {
    static RUN: OnceLock<Regex> = OnceLock::new();
    static FIXED: OnceLock<Regex> = OnceLock::new();
    let fixed = re(
        &FIXED,
        r#"\s--name[=\s]|\s(-p|--publish)[=\s]+["']?(\d{1,3}(\.\d{1,3}){3}:)?\d{2,5}:"#,
    );
    re(&RUN, r"docker\s+(container\s+)?run\b[^|;&\n]*")
        .find_iter(&script.replace("\\\n", " "))
        .any(|m| fixed.is_match(m.as_str()))
}

/// The part of a `docker compose` command line before its subcommand (`up`,
/// `exec`, ...). Only there is `-p` the project: `exec db mkdir -p tmp` is not.
fn compose_globals(args: &str) -> &str {
    // options of `docker compose` itself that take a value
    const VALUED: [&str; 10] = [
        "-f",
        "--file",
        "-p",
        "--project-name",
        "--env-file",
        "--profile",
        "--project-directory",
        "--ansi",
        "--parallel",
        "--progress",
    ];
    static WORD: OnceLock<Regex> = OnceLock::new();
    let mut value_next = false;
    for m in re(&WORD, r"\S+").find_iter(args) {
        let t = m.as_str();
        if t == "\\" {
            continue;
        }
        if value_next {
            value_next = false;
        } else if t.starts_with('-') {
            value_next = VALUED.contains(&t);
        } else {
            return &args[..m.start()];
        }
    }
    args
}

/// `docker compose -p <name> down` for each project that still has containers.
pub(crate) fn compose_down(ctx: &ExecContext, projects: &[String]) {
    for p in projects {
        let label = format!("label=com.docker.compose.project={p}");
        let has = ctx
            .command("docker")
            .args(["ps", "-aq", "--filter", &label])
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);
        if has {
            let _ = ctx
                .command("docker")
                .args(["compose", "-p", p, "down", "--remove-orphans"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Put in front of PATH for the run. Every job shares one checkout, and `npm ci`
/// deletes `node_modules` before installing, so a job installing a directory
/// (directly or from a script) would break another job running there. A
/// directory is installed once per run and lockfile; later installs are no-ops.
/// In a unit's copy `node_modules` holds links into setup's, which the other
/// units read: a command that writes into it first makes the copy's its own.
const NPM_SHIM: &str = r#"#!/bin/sh
# Written by local-ci for one run.
real="__REAL__"
own() {
  [ -f node_modules/.local-ci-shared ] || return 0
  t=$(cat node_modules/.local-ci-shared)
  rm -rf .node_modules.local-ci
  cp -cR "$t" .node_modules.local-ci 2>/dev/null || cp -a --reflink=auto "$t" .node_modules.local-ci || exit 1
  rm -rf node_modules && mv .node_modules.local-ci node_modules
}
sub="$1"
case "$sub" in
  ci|install|i) ;;
  rebuild|rb|uninstall|un|remove|r|update|up|link|ln|prune|dedupe|ddp) own; exec "$real" "$@" ;;
  *) exec "$real" "$@" ;;
esac
shift
for a in "$@"; do
  case "$a" in
    -*) ;;
    *) own; exec "$real" "$sub" "$@" ;;
  esac
done
dir=$(pwd -P)
ws=$(cd "${GITHUB_WORKSPACE:-/}" 2>/dev/null && pwd -P)
sum=$(cksum package-lock.json 2>/dev/null | cut -d' ' -f1)
# relative to the workspace, so a unit's copy of the worktree keeps setup's marks
key=$(printf '%s' "${dir#"$ws"}$LOCAL_CI_INSTALL_SCOPE" | cksum | cut -d' ' -f1)
marks="$LOCAL_CI_MARKS"
mkdir -p "$marks"
until mkdir "$marks/$key.lock" 2>/dev/null; do sleep 1; done
trap 'rmdir "$marks/$key.lock" 2>/dev/null' EXIT INT TERM
if [ -f "$marks/$key-$sum" ] && [ -d node_modules ]; then
  echo "local-ci: node_modules in $dir was installed earlier in this run; skipping npm $sub" >&2
  exit 0
fi
own
"$real" "$sub" "$@"
rc=$?
if [ "$rc" -eq 0 ]; then : > "$marks/$key-$sum"; fi
exit "$rc"
"#;

/// A step that may write into `node_modules`, run outside the npm shim.
fn writes_node_modules(script: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\b(npm (ci|install|i|rebuild|uninstall|update|link|prune|dedupe)|prisma generate|patch-package|yarn|pnpm)\b").unwrap()
    })
    .is_match(script)
}

/// GitHub's file commands: `name=value` lines and `name<<DELIM` blocks.
pub fn parse_file_command(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if let Some((k, delim)) = line.split_once("<<") {
            if !k.contains('=') {
                let mut val = Vec::new();
                for l in lines.by_ref() {
                    if l == delim {
                        break;
                    }
                    val.push(l);
                }
                out.push((k.to_string(), val.join("\n")));
                continue;
            }
        }
        if let Some((k, v)) = line.split_once('=') {
            if !k.is_empty() {
                out.push((k.to_string(), v.to_string()));
            }
        }
    }
    out
}

struct PortMap {
    declared: Option<u16>,
    container: u16,
    actual: u16,
}

/// Point `localhost:<declared>` (and `*_PORT=<declared>`) at the port docker gave us.
fn rewrite_ports(text: &str, maps: &[PortMap]) -> String {
    let mut s = text.to_string();
    for m in maps {
        if let Some(d) = m.declared {
            let r = Regex::new(&format!(r"\b(localhost|127\.0\.0\.1|0\.0\.0\.0):{d}\b")).unwrap();
            s = r
                .replace_all(&s, format!("${{1}}:{}", m.actual).as_str())
                .into_owned();
            let a = Regex::new(&format!(
                r#"(-e\s+|--env[=\s]\s*)?\b([A-Za-z_][A-Za-z0-9_]*=["']?){d}\b"#
            ))
            .unwrap();
            s = a
                .replace_all(&s, |c: &regex::Captures| {
                    if c.get(1).is_none() && port_key(c[2].trim_end_matches(['=', '"', '\''])) {
                        format!("{}{}", &c[2], m.actual)
                    } else {
                        c[0].to_string()
                    }
                })
                .into_owned();
        }
    }
    s
}

/// `PORT`, `DB_PORT`, `PORT_E2E`, `PGPORT`; not `EXPORT` or `REPORT`.
fn port_key(k: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?i)^(([a-z0-9_]*_)?|pg)port(_[a-z0-9_]*)?$").is_match(k)
}

fn port_literal(v: &str) -> Option<u16> {
    v.trim().parse::<u16>().ok().filter(|p| *p >= 1024)
}

/// Ports a script fixes for itself: `TEST_DB_PORT=15432 docker compose …`.
/// `docker run -e X_PORT=…` is the container's own port and stays.
fn assigned_ports(script: &str) -> Vec<u16> {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r#"(-e\s+|--env[=\s]\s*)?\b([A-Za-z_][A-Za-z0-9_]*)=["']?(\d{4,5})\b"#,
    )
    .captures_iter(script)
    .filter(|c| c.get(1).is_none() && port_key(&c[2]))
    .filter_map(|c| port_literal(&c[3]))
    .collect()
}

/// Give each fixed port a job sets itself a free one, the same in all its steps.
/// Free ports come from the ephemeral range, which portool's blocks stay below.
fn map_literals(ports: impl IntoIterator<Item = u16>, maps: &mut Vec<PortMap>) -> Result<()> {
    // held until all are picked, so the OS hands out a different one each time
    let mut held = Vec::new();
    for p in ports {
        if maps.iter().any(|m| m.declared == Some(p)) {
            continue;
        }
        let actual = loop {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).context("no free port")?;
            let a = l.local_addr()?.port();
            held.push(l);
            if !maps.iter().any(|m| m.actual == a) {
                break a;
            }
        };
        maps.push(PortMap {
            declared: Some(p),
            container: p,
            actual,
        });
    }
    Ok(())
}

/// `-p name` / `COMPOSE_PROJECT_NAME=name` with `-<tag>` appended, so two runs
/// never share a project and our `down` never stops one started by hand.
fn rename_compose_projects(script: &str, tag: &str) -> String {
    static FLAG: OnceLock<Regex> = OnceLock::new();
    static ENV: OnceLock<Regex> = OnceLock::new();
    let suffix = format!("-{tag}");
    let rename = |c: &regex::Captures| {
        if c[2].ends_with(&suffix) {
            c[0].to_string()
        } else {
            format!("{}{}{suffix}", &c[1], &c[2])
        }
    };
    let s = re(
        &ENV,
        r#"(\bCOMPOSE_PROJECT_NAME=["']?)([A-Za-z0-9][A-Za-z0-9_-]*)"#,
    )
    .replace_all(script, rename)
    .into_owned();
    let flag = re(
        &FLAG,
        r#"((?:^|\s)(?:-p|--project-name)(?:\s+|=)["']?)([A-Za-z0-9][A-Za-z0-9_-]*)"#,
    );
    let mut out = String::new();
    let mut rest = s.as_str();
    loop {
        let at = ["docker compose", "docker-compose"]
            .iter()
            .filter_map(|m| rest.find(m).map(|i| i + m.len()))
            .min();
        let Some(at) = at else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..at]);
        let seg = &rest[at..];
        // the command ends at the next `|`, `;`, `&` or unescaped newline
        let b = seg.as_bytes();
        let end = (0..b.len())
            .find(|&i| {
                matches!(b[i], b'|' | b';' | b'&')
                    || (b[i] == b'\n' && (i == 0 || b[i - 1] != b'\\'))
            })
            .unwrap_or(b.len());
        let globals = compose_globals(&seg[..end]);
        out.push_str(&flag.replace_all(globals, rename));
        out.push_str(&seg[globals.len()..end]);
        rest = &seg[end..];
    }
    out
}

/// Whether the workflow reaches a service's host port `h` somewhere the
/// rewriting changes: `localhost:<h>`, `KEY_PORT=<h>`, a `*_PORT` env value,
/// or `${{ job.services... }}`. When it never names the port, the code uses it
/// as its default, and the service has to be on it as on GitHub.
fn rewritable_mention(w: &Workflow, job: &Job, h: u16) -> bool {
    let hs = h.to_string();
    let host = Regex::new(&format!(r"\b(localhost|127\.0\.0\.1|0\.0\.0\.0):{h}\b")).unwrap();
    let assign = Regex::new(&format!(r#"\b([A-Za-z_][A-Za-z0-9_]*)=["']?{h}\b"#)).unwrap();
    let in_text = |t: &str| {
        t.contains("job.services.")
            || host.is_match(t)
            || assign.captures_iter(t).any(|c| port_key(&c[1]))
    };
    let in_env = |pairs: &[(String, String)]| {
        pairs
            .iter()
            .any(|(k, v)| (port_key(k) && v.trim() == hs) || in_text(v))
    };
    in_env(&w.env)
        || in_env(&job.env)
        || job
            .steps
            .iter()
            .any(|s| in_env(&s.env) || s.run.as_deref().is_some_and(in_text))
}

fn rewrite_env_value(key: &str, val: &str, maps: &[PortMap]) -> String {
    for m in maps {
        if let Some(d) = m.declared {
            if port_key(key) && val.trim() == d.to_string() {
                return m.actual.to_string();
            }
        }
    }
    rewrite_ports(val, maps)
}

impl<'a> Runner<'a> {
    pub fn new(ctx: &'a ExecContext, fp: &'a FrozenPlan, ws: &Workspace, opts: Opts) -> Self {
        let base_path = ctx.var("PATH").unwrap_or_default().to_string();
        let plan = fp.to_plan();
        let refused = (0..plan.selected.len())
            .filter_map(|wf| Some((wf, artifact_io::workflow_refusal(&plan, wf)?)))
            .collect();
        Runner {
            ctx,
            fp,
            refused,
            hoisted: plan::hoist(&plan),
            plan,
            receipts: None,
            repo_root: ws.repo_root.clone(),
            wt: ws.root.clone(),
            run_dir: ws.run_dir.clone(),
            tools: ws.tools.clone(),
            lock_file: ws.lock_file.clone(),
            slug: fp.repository.clone().unwrap_or_default(),
            opts,
            base_path,
            seq: AtomicUsize::new(0),
            synced: Mutex::new(HashSet::new()),
            docker_ok: OnceLock::new(),
        }
    }

    fn github_ctx(&self, ws: &Path, w: &Workflow, event: &str, job: &str) -> Value {
        let base = crate::git::branch_name(&self.fp.source.base);
        let head = self.fp.branch.clone().unwrap_or_default();
        let pr = event == "pull_request";
        json!({
            "event_name": event,
            "event": if pr { json!({"pull_request": {"draft": false, "number": 0, "head": {"ref": head, "sha": self.fp.source.execution_commit}, "base": {"ref": base}}, "action": "synchronize"}) } else { json!({"ref": format!("refs/heads/{head}")}) },
            "sha": self.fp.source.execution_commit,
            "ref": if pr { "refs/pull/0/merge".to_string() } else { format!("refs/heads/{head}") },
            "ref_name": if pr { "0/merge".to_string() } else { head.clone() },
            "head_ref": if pr { head.clone() } else { String::new() },
            "base_ref": if pr { base } else { String::new() },
            "repository": self.slug,
            "repository_owner": self.slug.split('/').next().unwrap_or(""),
            "workflow": w.name,
            "job": job,
            "run_id": "0",
            "run_number": "0",
            "run_attempt": "1",
            "actor": "local-ci",
            "server_url": "https://github.com",
            "api_url": "https://api.github.com",
            "workspace": ws.to_string_lossy(),
        })
    }

    fn runner_ctx(&self) -> Value {
        let os = if cfg!(target_os = "macos") {
            "macOS"
        } else {
            "Linux"
        };
        let arch = if cfg!(target_arch = "aarch64") {
            "ARM64"
        } else {
            "X64"
        };
        json!({"os": os, "arch": arch, "temp": self.tools.join("tmp").to_string_lossy(), "tool_cache": self.tools.join("tool-cache").to_string_lossy(), "name": "local-ci"})
    }

    fn base_env(&self, gh: &Value, extra_path: &[String]) -> Vec<(String, String)> {
        let s = |k: &str| gh.get(k).map(expr::to_string).unwrap_or_default();
        let mut path: Vec<String> = vec![self.tools.join("bin").to_string_lossy().into_owned()];
        path.extend(extra_path.iter().rev().cloned());
        path.push(self.tools.join("venv/bin").to_string_lossy().into_owned());
        path.push(
            self.tools
                .join("npm-global/bin")
                .to_string_lossy()
                .into_owned(),
        );
        path.push(self.base_path.clone());
        let ws = s("workspace");
        vec![
            ("CI".into(), "true".into()),
            ("GITHUB_ACTIONS".into(), "true".into()),
            ("LOCAL_CI".into(), "1".into()),
            (
                "LOCAL_CI_MARKS".into(),
                self.marks(Path::new(&ws)).to_string_lossy().into_owned(),
            ),
            ("GITHUB_WORKSPACE".into(), ws),
            ("GITHUB_SHA".into(), self.fp.source.execution_commit.clone()),
            ("GITHUB_REF".into(), s("ref")),
            ("GITHUB_REF_NAME".into(), s("ref_name")),
            ("GITHUB_HEAD_REF".into(), s("head_ref")),
            ("GITHUB_BASE_REF".into(), s("base_ref")),
            ("GITHUB_EVENT_NAME".into(), s("event_name")),
            ("GITHUB_REPOSITORY".into(), self.slug.clone()),
            ("GITHUB_WORKFLOW".into(), s("workflow")),
            ("GITHUB_JOB".into(), s("job")),
            ("GITHUB_RUN_ID".into(), "0".into()),
            ("GITHUB_RUN_NUMBER".into(), "0".into()),
            (
                "RUNNER_OS".into(),
                self.runner_ctx()["os"].as_str().unwrap_or("").into(),
            ),
            (
                "RUNNER_TEMP".into(),
                self.tools.join("tmp").to_string_lossy().into_owned(),
            ),
            (
                "NPM_CONFIG_PREFIX".into(),
                self.tools.join("npm-global").to_string_lossy().into_owned(),
            ),
            ("PIP_REQUIRE_VIRTUALENV".into(), "true".into()),
            (
                "LOCAL_CI_TOOLS".into(),
                self.tools.to_string_lossy().into_owned(),
            ),
            // a `git push` from inside a script goes nowhere
            ("GIT_CONFIG_COUNT".into(), "3".into()),
            (
                "GIT_CONFIG_KEY_0".into(),
                "url./dev/null/local-ci-blocks-push/.pushInsteadOf".into(),
            ),
            ("GIT_CONFIG_VALUE_0".into(), "https://".into()),
            (
                "GIT_CONFIG_KEY_1".into(),
                "url./dev/null/local-ci-blocks-push/.pushInsteadOf".into(),
            ),
            ("GIT_CONFIG_VALUE_1".into(), "git@".into()),
            (
                "GIT_CONFIG_KEY_2".into(),
                "url./dev/null/local-ci-blocks-push/.pushInsteadOf".into(),
            ),
            ("GIT_CONFIG_VALUE_2".into(), "ssh://".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("PATH".into(), path.join(":")),
        ]
    }

    /// `text` with this machine's GitHub token, if it was asked for, replaced
    /// by `***`: for what is recorded.
    fn hide_token(&self, text: String) -> String {
        match self.ctx.github_token_if_fetched() {
            Some(t) if t.len() >= 4 => text.replace(t, "***"),
            _ => text,
        }
    }

    /// Run one shell step. Output goes to `log`; returns how it ended and how
    /// long task-traffic-controller kept it waiting for its turn. When task-traffic-controller's ledger lock
    /// times out the command never started, so it is tried again, a few times.
    /// A `piped` script holds the GitHub token: it is not written to a file but
    /// handed to the shell over a pipe (see `shell_once`).
    #[allow(clippy::too_many_arguments)]
    fn shell(
        &self,
        cwd: &Path,
        script: &str,
        shell: Option<&str>,
        env: &[(String, String)],
        timeout: Option<Duration>,
        log: &Path,
        key: Option<String>,
        piped: bool,
    ) -> Result<(Exit, Duration)> {
        let mut attempt = 0;
        let mut queued = Duration::ZERO;
        loop {
            let from = fs::metadata(log).map(|m| m.len()).unwrap_or(0);
            let (r, q) =
                self.shell_once(cwd, script, shell, env, timeout, log, key.clone(), piped)?;
            queued += q;
            let failed = matches!(r, Exit::Code(c) if c != 0);
            if !(failed
                && attempt < 3
                && self.opts.traffic_controller.is_some()
                && ledger_timed_out(log, from))
            {
                return Ok((r, queued));
            }
            attempt += 1;
            std::thread::sleep(Duration::from_secs(attempt));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn shell_once(
        &self,
        cwd: &Path,
        script: &str,
        shell: Option<&str>,
        env: &[(String, String)],
        timeout: Option<Duration>,
        log: &Path,
        key: Option<String>,
        piped: bool,
    ) -> Result<(Exit, Duration)> {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let file = self.tools.join(format!("scripts/step-{n}.sh"));
        // With task-traffic-controller the script touches this file as its first act, on the
        // line the script starts on, so task-traffic-controller still sees the same command and
        // the script keeps its line numbers. The file's time tells how long the
        // step waited for its turn, even if it prints nothing. Scripts of other
        // shells are not changed (a Python script may begin with
        // `from __future__`); their wait is read from the log.
        let turn = self.tools.join(format!("scripts/step-{n}.turn"));
        let queued = self.opts.traffic_controller.is_some() && key.is_some();
        let marked = queued && matches!(shell.map(str::trim), None | Some("bash") | Some("sh"));
        let sh_family = matches!(shell.map(str::trim), None | Some("bash") | Some("sh"));
        // bash and sh read a script that is not a file one byte at a time, so
        // the lines they have not run yet are still in the pipe, and fd 3 is
        // inherited by everything the script starts. They read the script
        // through a descriptor of their own, so the first act of a script
        // that holds the token is to close fd 3, on the line it starts on.
        // (Python parses the whole script before it runs a line of it, so
        // nothing is left in the pipe by then.)
        let close = if piped && sh_family {
            format!("exec {PIPE_FD}<&-; ")
        } else {
            String::new()
        };
        let body = if marked {
            let mark = shlex::try_quote(&turn.to_string_lossy())
                .map(|q| format!(": > {q} 2>/dev/null || true; "))
                .unwrap_or_default();
            format!("{close}{mark}{script}")
        } else {
            format!("{close}{script}")
        };
        // A script that holds the token never reaches a file: the shell reads
        // it from file descriptor 3, the read end of a pipe (so the argument
        // list shows `/dev/fd/3` and nothing else), and task-traffic-controller passes the
        // descriptor on to its child.
        let f = if piped {
            PIPE_SCRIPT.to_string()
        } else {
            write_private(&file, &body)?;
            file.to_string_lossy().into_owned()
        };
        let mut argv: Vec<String> = match shell.map(str::trim) {
            None | Some("bash") => vec![
                "bash".into(),
                "--noprofile".into(),
                "--norc".into(),
                "-eo".into(),
                "pipefail".into(),
                f,
            ],
            Some("sh") => vec!["sh".into(), "-e".into(), f],
            Some("python") => vec!["python3".into(), f],
            // A shell of our own may open the script twice (`shellcheck {0} &&
            // bash {0}` reads nothing the second time and exits 0), and cannot
            // be told to close fd 3: a script that holds the token is not run.
            Some(custom) if custom.contains("{0}") && piped => bail!(
                "the step's script holds the GitHub token, which is handed over a pipe that `shell: {custom}` cannot read; use bash, sh or python for a step that uses github.token"
            ),
            Some(custom) if custom.contains("{0}") => shlex::split(&custom.replace("{0}", &f))
                .ok_or_else(|| anyhow!("bad shell `{custom}`"))?,
            // keep in step with `plan::shell_supported`
            Some(other) => bail!("unsupported shell `{other}`"),
        };
        if let (Some(c), Some(k)) = (&self.opts.traffic_controller, key) {
            let mut w = vec![
                c.to_string_lossy().into_owned(),
                "exec".into(),
                "--key".into(),
                k,
                "--".into(),
            ];
            w.append(&mut argv);
            argv = w;
        }
        if !cwd.is_dir() {
            fs::write(
                log,
                format!(
                    "local-ci: working-directory {} does not exist\n",
                    cwd.display()
                ),
            )?;
            return Ok((Exit::Code(1), Duration::ZERO));
        }
        let out = OpenOptions::new().create(true).append(true).open(log)?;
        let err = out.try_clone()?;
        let mut cmd = self.ctx.command(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let pipe = if piped {
            let (r, w) = script_pipe(&self.tools.join(format!("scripts/step-{n}.fifo")))?;
            let rfd = std::os::fd::AsRawFd::as_raw_fd(&r);
            // SAFETY: only dup2 and fcntl, which are async-signal-safe, run
            // between fork and exec. dup2 clears close-on-exec on the copy.
            unsafe {
                cmd.pre_exec(move || {
                    if rfd == PIPE_FD {
                        let fl = libc::fcntl(rfd, libc::F_GETFD);
                        if fl < 0 || libc::fcntl(rfd, libc::F_SETFD, fl & !libc::FD_CLOEXEC) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    } else if libc::dup2(rfd, PIPE_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Some((r, w))
        } else {
            None
        };
        let mut child = cmd
            .spawn()
            .with_context(|| format!("cannot start {}", argv[0]))?;
        let writer_stop = Arc::new(AtomicBool::new(false));
        let mut writer = None;
        if let Some((r, w)) = pipe {
            // the child has its own copy
            drop(r);
            // A thread writes, so that a script larger than the pipe does not
            // block here. It ends when the shell has read it all, when every
            // reader is gone, or when the step is over (`writer_stop`), even
            // if something that outlived the step still holds the read end.
            let body = body.clone();
            let stop = writer_stop.clone();
            writer = Some(std::thread::spawn(move || {
                write_until_stopped(w, body.as_bytes(), &stop)
            }));
        }
        let pid = child.id() as i32;
        let spawned = (Instant::now(), std::time::SystemTime::now());
        self.ctx.cancel.track_pgid(pid);
        // With task-traffic-controller the clock starts when the command gets its turn, not while it waits.
        let mut started: Option<Instant> = if self.opts.traffic_controller.is_some() {
            None
        } else {
            Some(Instant::now())
        };
        let mut seen = 0u64;
        let mut term_at: Option<Instant> = None;
        let mut timed_out = false;
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break st;
            }
            if started.is_none() && marked && turn.exists() {
                started = Some(Instant::now());
            }
            if started.is_none() {
                if let Ok(mut lf) = File::open(log) {
                    let len = lf.metadata().map(|m| m.len()).unwrap_or(0);
                    if len > seen {
                        let _ = lf.seek(SeekFrom::Start(seen));
                        let mut b = Vec::new();
                        let _ = lf.read_to_end(&mut b);
                        seen = len;
                        if String::from_utf8_lossy(&b)
                            .lines()
                            .any(|l| !is_controller_line(l))
                        {
                            started = Some(Instant::now());
                        }
                    }
                }
            }
            let over = matches!((timeout, started), (Some(t), Some(s)) if s.elapsed() > t);
            if (over || self.ctx.cancel.is_cancelled()) && term_at.is_none() {
                timed_out = over;
                unsafe {
                    libc::killpg(pid, libc::SIGTERM);
                }
                term_at = Some(Instant::now());
            }
            if term_at.is_some_and(|t| t.elapsed() > Duration::from_secs(10)) {
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        self.ctx.cancel.untrack_pgid(pid);
        // the step is over: nothing is owed to a reader that is still around
        writer_stop.store(true, Ordering::SeqCst);
        if let Some(h) = writer {
            let _ = h.join();
        }
        if !piped {
            let _ = fs::remove_file(&file);
        }
        // Waiting for its turn: until the script touched `turn`, or the first
        // line of output that is not task-traffic-controller's own, or, when neither came
        // (task-traffic-controller gave up, the step was stopped, a silent step of another
        // shell), until it was over.
        let waited = if marked {
            let got_turn = fs::metadata(&turn).and_then(|m| m.modified()).ok();
            let _ = fs::remove_file(&turn);
            match got_turn {
                // a time before the spawn is the file system's rounding
                Some(t) => t.duration_since(spawned.1).unwrap_or(Duration::ZERO),
                None => spawned.0.elapsed(),
            }
        } else if queued {
            started.map_or_else(
                || spawned.0.elapsed(),
                |s| s.saturating_duration_since(spawned.0),
            )
        } else {
            Duration::ZERO
        };
        if timed_out {
            return Ok((Exit::Timeout, waited));
        }
        if self.ctx.cancel.is_cancelled() {
            return Ok((Exit::Cancelled, waited));
        }
        Ok((
            Exit::Code(status.code().unwrap_or(
                128 + std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0),
            )),
            waited,
        ))
    }

    fn log_path(&self, wf: &str, job: &str, n: usize, label: &str) -> PathBuf {
        let dir = self.run_dir.join(format!(
            "{}-{}",
            slug_of(
                crate::report::short(&self.fp.config.default_workflows_dir, wf)
                    .trim_end_matches(".yml")
                    .trim_end_matches(".yaml")
            ),
            slug_of(job)
        ));
        let _ = fs::create_dir_all(&dir);
        dir.join(format!("{:02}-{}.log", n, slug_of(label)))
    }

    /// Create the Python venv that `pip install` steps install into, if any step needs one.
    pub fn prepare_tools(&self) -> Result<()> {
        for d in [
            "scripts",
            "tmp",
            "npm-global/bin",
            "files",
            "bin",
            "installed",
        ] {
            fs::create_dir_all(self.tools.join(d))?;
        }
        if let Some(npm) = self
            .base_path
            .split(':')
            .map(|d| Path::new(d).join("npm"))
            .find(|p| p.is_file())
        {
            let shim = self.tools.join("bin/npm");
            fs::write(&shim, NPM_SHIM.replace("__REAL__", &npm.to_string_lossy()))?;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))?;
        }
        let wants_pip = self.plan.selected.iter().any(|s| {
            s.wf.jobs.iter().any(|j| {
                j.steps.iter().any(|st| {
                    st.run
                        .as_deref()
                        .is_some_and(|r| r.contains("pip install") || r.contains("pip3 install"))
                })
            })
        });
        if wants_pip {
            let ok = self
                .ctx
                .command("python3")
                .arg("-m")
                .arg("venv")
                .arg(self.tools.join("venv"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                eprintln!(
                    "local-ci: could not create a Python venv; `pip install` steps will be refused"
                );
            }
        }
        Ok(())
    }

    /// What each task of the plan was told is prepared, by task id. Set before
    /// [`Runner::run_units`]; a task without an entry prepares everything itself.
    /// `tctx` is the context the receipts' template keys were made under.
    pub fn set_receipts(
        &mut self,
        receipts: HashMap<String, PreparedReceipt>,
        tctx: TemplateContext,
    ) {
        self.receipts = Some((receipts, tctx));
    }

    /// Do the steps of `recipe` in the workspace, in order, and stop at the first
    /// that does not pass. `key` is the digest of the template key the receipt
    /// is for.
    /// The recipe of each task of the plan, in the plan's order.
    pub fn task_recipes(&self) -> Result<Vec<(String, SetupRecipe)>> {
        self.fp
            .tasks
            .iter()
            .map(|t| {
                Ok((
                    t.id.clone(),
                    setup::derive_in(&self.plan, &self.hoisted, t)?,
                ))
            })
            .collect()
    }

    pub fn run_recipe(&self, recipe: &SetupRecipe, key: &str) -> PreparedSetup {
        if recipe.steps.is_empty() {
            return PreparedSetup {
                receipt: PreparedReceipt::empty(key, &self.fp.source.execution_commit),
                steps: vec![],
            };
        }
        let workspace = match setup::ensure_workspace_id(&self.wt) {
            Ok(id) => id,
            Err(e) => {
                return PreparedSetup {
                    receipt: PreparedReceipt::empty(key, &self.fp.source.execution_commit),
                    steps: vec![StepResult {
                        label: "setup".into(),
                        outcome: Outcome::Error(format!("cannot record the workspace id: {e:#}")),
                        exit: None,
                        secs: 0.0,
                        log: None,
                        note: None,
                    }],
                };
            }
        };
        let covered = setup::covered_by(recipe);
        let dirs: BTreeSet<String> = recipe.steps.iter().map(|s| s.dir.clone()).collect();
        let before = setup::top_level_entries(&self.wt, &dirs);
        let mut out = Vec::new();
        let mut prepared: Vec<PreparedStep> = Vec::new();
        let mut notes: Vec<setup::ReceiptNote> = Vec::new();
        for (i, s) in recipe.steps.iter().enumerate() {
            if let Some(by) = covered.get(&s.id) {
                if prepared.iter().any(|p| &p.id == by) {
                    prepared.push(PreparedStep {
                        id: s.id.clone(),
                        how: PreparedHow::SameAs { step: by.clone() },
                    });
                    continue;
                }
            }
            let label = format!(
                "{}$ {}",
                if s.dir.is_empty() { "." } else { &s.dir },
                s.label
            );
            let found = self
                .plan
                .selected
                .iter()
                .position(|x| x.wf.file == s.workflow)
                .filter(|wi| {
                    self.plan.selected[*wi]
                        .wf
                        .jobs
                        .iter()
                        .any(|j| j.id == s.job)
                });
            let Some(wi) = found else {
                out.push(StepResult {
                    label,
                    outcome: Outcome::Error(format!(
                        "the plan has no job {} in {}",
                        s.job, s.workflow
                    )),
                    exit: None,
                    secs: 0.0,
                    log: None,
                    note: None,
                });
                break;
            };
            let sel = &self.plan.selected[wi];
            let gh = self.github_ctx(&self.wt, &sel.wf, &sel.event, &s.job);
            let mut env = self.base_env(&gh, &[]);
            env.extend(s.env.iter().cloned());
            env.extend(s.ignored_env.iter().cloned());
            let log = self.log_path("setup", "", i + 1, &format!("{}-{}", s.dir, s.label));
            let t0 = Instant::now();
            let cwd = self.wt.join(&s.dir);
            let res = self
                .shell(
                    &cwd,
                    &s.run,
                    s.shell.as_deref(),
                    &env,
                    Some(Duration::from_secs(1800)),
                    &log,
                    Some(format!("local-ci setup {label}")),
                    false,
                )
                .map(|(exit, _)| exit);
            let (outcome, exit) = match res {
                Ok(Exit::Code(0)) => (Outcome::Pass, Some(0)),
                Ok(Exit::Code(c)) => (Outcome::Fail, Some(c)),
                Ok(Exit::Timeout) => (Outcome::Error("timed out".into()), None),
                Ok(Exit::Cancelled) => (Outcome::Error("cancelled".into()), None),
                Err(e) => (Outcome::Error(format!("{e:#}")), None),
            };
            let stop = outcome != Outcome::Pass;
            // an install shared by jobs whose `ignore-env` values differ ran
            // with one set of them: say whose
            let note = setup::ignored_env_note(s);
            if !stop {
                prepared.push(PreparedStep {
                    id: s.id.clone(),
                    how: PreparedHow::Ran,
                });
                if let Some(text) = &note {
                    notes.push(setup::ReceiptNote {
                        step: s.id.clone(),
                        text: text.clone(),
                    });
                }
            }
            out.push(StepResult {
                label,
                outcome,
                exit,
                secs: t0.elapsed().as_secs_f64(),
                log: Some(log),
                note,
            });
            if stop {
                break;
            }
        }
        let artifacts = if out.iter().all(|r| r.outcome == Outcome::Pass) {
            setup::artifact_digests(
                &self.wt,
                &before,
                &setup::top_level_entries(&self.wt, &dirs),
            )
        } else {
            vec![]
        };
        PreparedSetup {
            receipt: PreparedReceipt {
                template_key: key.into(),
                source: self.fp.source.execution_commit.clone(),
                workspace,
                steps: prepared,
                artifacts,
                notes,
            },
            steps: out,
        }
    }

    pub fn run_units(&self) -> Vec<UnitResult> {
        let sem = Semaphore {
            free: Mutex::new(self.opts.parallel.max(1)),
            cv: Condvar::new(),
        };
        let results: Mutex<Vec<(usize, UnitResult)>> = Mutex::new(Vec::new());
        let isolate = self.plan.units.len() > 1;
        std::thread::scope(|scope| {
            for (i, u) in self.plan.units.iter().enumerate() {
                let sem = &sem;
                let results = &results;
                scope.spawn(move || {
                    // an artifact step of the workflow that cannot be run
                    // fails its units here, and only them
                    if let Some(why) = self.refused.get(&u.wf) {
                        results
                            .lock()
                            .unwrap()
                            .push((i, self.refused_unit(i, u, why)));
                        return;
                    }
                    let t0 = Instant::now();
                    let lock = if self.unit_locked(u) {
                        self.lock().ok()
                    } else {
                        None
                    };
                    let locked_at = t0.elapsed().as_secs_f64();
                    sem.acquire();
                    let waited = UnitWait {
                        lock: locked_at,
                        parallel: t0.elapsed().as_secs_f64() - locked_at,
                    };
                    let r = if isolate && !self.ctx.cancel.is_cancelled() {
                        self.run_unit_in_copy(i, u, waited)
                    } else {
                        self.run_unit(i, u, waited, &self.wt)
                    };
                    sem.release();
                    drop(lock);
                    results.lock().unwrap().push((i, r));
                });
            }
        });
        let mut r = results.into_inner().unwrap();
        r.sort_by_key(|(i, _)| *i);
        r.into_iter().map(|(_, u)| u).collect()
    }

    /// The entries of a job that is not run, each an error that says why: one
    /// per matrix variant (`only`: that variant alone) when the matrix is
    /// written out, so the result has what the plan expects of the job.
    fn refused_entries(
        &self,
        wf: usize,
        id: &str,
        why: &str,
        only: Option<usize>,
    ) -> Vec<JobResult> {
        let w = &self.plan.selected[wf].wf;
        let base = self.plan.job(wf, id).name.clone().unwrap_or(id.to_string());
        let displays: Vec<String> = match dag::matrix_plan(self.fp.job(wf, id)) {
            dag::MatrixPlan::Static(v) => v
                .iter()
                .enumerate()
                .filter(|(i, _)| only.is_none_or(|o| o == *i))
                .map(|(_, c)| format!("{base} ({})", matrix::describe(c)))
                .collect(),
            _ => vec![base],
        };
        displays
            .into_iter()
            .map(|display| JobResult {
                workflow: w.file.clone(),
                job: id.to_string(),
                display,
                outcome: Outcome::Error(why.to_string()),
                secs: 0.0,
                steps: vec![],
                cancelled: false,
                time: None,
            })
            .collect()
    }

    /// The result of a unit that is not run: each of its jobs is an error that
    /// says why.
    fn refused_unit(&self, ui: usize, u: &Unit, why: &str) -> UnitResult {
        let w = &self.plan.selected[u.wf].wf;
        UnitResult {
            unit: Some(ui),
            workflow: w.file.clone(),
            jobs: u
                .jobs
                .iter()
                .flat_map(|id| self.refused_entries(u.wf, id, why, None))
                .collect(),
            waited_secs: 0.0,
            copy_secs: None,
            cached: None,
            outcomes: vec![],
        }
    }

    /// The result of `task` when its workflow has an artifact step that cannot
    /// be run, `None` when it has none: nothing is started, and each job is an
    /// error that says why.
    pub(crate) fn refused_task(&self, task: &ExecutionTask) -> Option<TaskResult> {
        let wf = self
            .plan
            .selected
            .iter()
            .position(|s| s.wf.file == task.workflow)?;
        let why = self.refused.get(&wf)?;
        let only = task.variant.as_ref().map(|v| v.index);
        Some(TaskResult {
            task: task.id.clone(),
            jobs: task
                .jobs
                .iter()
                .flat_map(|id| self.refused_entries(wf, id, why, only))
                .collect(),
            outcomes: task
                .jobs
                .iter()
                .map(|id| JobOutcome {
                    job: id.clone(),
                    variant: only,
                    status: JobStatus::Failure,
                    outputs: Default::default(),
                    skipped: Some(why.clone()),
                    ancestor_failed: false,
                })
                .collect(),
            receipt_ignored: None,
            artifacts: vec![],
        })
    }

    pub(crate) fn lock(&self) -> Result<File> {
        use fs2::FileExt;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock_file)?;
        f.lock_exclusive()?;
        Ok(f)
    }

    /// Exclusive jobs can overlap: their fixed ports and Compose projects are
    /// rewritten (see `run_combo`), and portool gives the ports their scripts
    /// pick themselves. Without portool those may still collide, so they keep
    /// the per-repository lock.
    fn isolated(&self) -> bool {
        self.opts.portool.is_some() && self.wt.join(".portool.toml").is_file()
    }

    /// Whether the unit holds the per-repository lock while it runs. Jobs with
    /// `services:` always do: they get no portool ports (see `run_combo`), so a
    /// port their code picks for itself, a test server's, is not their own.
    fn unit_locked(&self, u: &Unit) -> bool {
        u.exclusive
            && (!self.isolated()
                || u.jobs
                    .iter()
                    .any(|id| !self.plan.job(u.wf, id).services.is_empty()))
    }

    /// One of the `--containers` slots, shared by every `local-ci` run on the
    /// machine: each exclusive job holds one while it runs, so databases started
    /// early do not pile up while their tests wait for CPU. None when cancelled.
    fn container_slot(&self) -> Result<Option<File>> {
        use fs2::FileExt;
        let dir = self.lock_file.parent().context("no lock directory")?;
        fs::create_dir_all(dir)?;
        loop {
            for i in 0..self.opts.containers.max(1) {
                let f = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(dir.join(format!("containers-{i}.lock")))?;
                if f.try_lock_exclusive().is_ok() {
                    return Ok(Some(f));
                }
            }
            if self.ctx.cancel.is_cancelled() {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Whether the job binds something fixed that rewriting does not reach: a
    /// Compose file with `container_name:` or a literal host port, or
    /// `docker run --name` / `-p <host port>:`. A Compose file named by a
    /// variable cannot be read here and counts as fixed.
    fn fixed_resources(&self, ws: &Path, w: &Workflow, job: &Job, cm: &Map<String, Value>) -> bool {
        let ctx = Ctx::new(cm, Status::Success);
        job.steps.iter().any(|s| {
            let Some(run) = &s.run else { return false };
            let script = expr::interpolate(run, &ctx).unwrap_or_else(|_| run.clone());
            let dir = ws.join(plan::step_dir(w, job, s));
            docker_run_fixed(&script)
                || compose_files(&script).iter().any(|f| match f {
                    Some(f) => fs::read_to_string(dir.join(f)).is_ok_and(|t| compose_fixed(&t)),
                    None => true,
                })
        })
    }

    /// Where the npm shim records the installs done in the workspace `ws`.
    fn marks(&self, ws: &Path) -> PathBuf {
        self.tools
            .join("installed")
            .join(ws.file_name().unwrap_or_default())
    }

    /// A copy of `from` at `to`, with the installs the npm shim recorded for it.
    fn copy_of(&self, from: &Path, to: &Path) -> Result<()> {
        crate::git::clone_worktree(
            self.ctx,
            &self.repo_root,
            from,
            to,
            &self.fp.source.execution_commit,
            self.fp.config.link_node_modules,
        )?;
        let marks = self.marks(to);
        fs::create_dir_all(&marks)?;
        for e in fs::read_dir(self.marks(from))
            .into_iter()
            .flatten()
            .flatten()
        {
            if e.file_type().is_ok_and(|t| t.is_file()) {
                fs::copy(e.path(), marks.join(e.file_name()))?;
            }
        }
        Ok(())
    }

    fn remove_copy(&self, ws: &Path) {
        if !self.opts.keep {
            self.release_ports(ws);
            crate::git::remove_worktree(self.ctx, &self.repo_root, ws, &self.opts.trash);
        }
    }

    /// Run the unit in its own copy of the worktree, as each GitHub job gets its
    /// own checkout: a unit that edits tracked files for a while (a negative
    /// control, a formatter) must not change what the units beside it read.
    fn run_unit_in_copy(&self, ui: usize, u: &Unit, waited: UnitWait) -> UnitResult {
        let n = ui + 1;
        // the pid stays last in the name: cleanup scripts read it from there
        let ws = self.wt.with_file_name(format!(
            "{}-u{n}-{}",
            &self.fp.source.execution_commit[..7],
            std::process::id()
        ));
        let t0 = Instant::now();
        // installs done in setup are in the copy too
        let r = match self.copy_of(&self.wt, &ws) {
            Ok(()) => {
                let copy_secs = t0.elapsed().as_secs_f64();
                UnitResult {
                    copy_secs: Some(copy_secs),
                    ..self.run_unit(ui, u, waited, &ws)
                }
            }
            Err(e) => {
                let w = &self.plan.selected[u.wf].wf;
                let reason = format!("cannot copy the worktree: {e:#}");
                let mut jobs: Vec<JobResult> = u
                    .jobs
                    .iter()
                    .map(|id| JobResult {
                        workflow: w.file.clone(),
                        job: id.clone(),
                        display: self.plan.job(u.wf, id).name.clone().unwrap_or(id.clone()),
                        outcome: Outcome::Error(reason.clone()),
                        secs: 0.0,
                        steps: vec![],
                        cancelled: false,
                        time: None,
                    })
                    .collect();
                charge(&mut jobs, waited);
                UnitResult {
                    unit: Some(ui),
                    workflow: w.file.clone(),
                    jobs,
                    waited_secs: waited.total(),
                    copy_secs: None,
                    cached: None,
                    outcomes: vec![],
                }
            }
        };
        self.remove_copy(&ws);
        r
    }

    /// Give back the port block of `ws`. The worktree of the run itself may hold
    /// one even when no job asked: portool's post-checkout hook takes it.
    pub fn release_ports(&self, ws: &Path) {
        let Some(bin) = &self.opts.portool else {
            return;
        };
        if (ws == self.wt || self.synced.lock().unwrap().remove(ws))
            && ws.join(".portool.toml").is_file()
        {
            crate::portool::release(self.ctx, bin, ws);
        }
    }

    fn run_unit(&self, ui: usize, u: &Unit, waited: UnitWait, ws: &Path) -> UnitResult {
        let w = &self.plan.selected[u.wf].wf;
        let locked = self.unit_locked(u);
        // The unit is run as its tasks, in the order the plan gives them, in
        // the one workspace of the unit (a local run's shortcut: a task run
        // elsewhere gets a checkout of its own). The variants of a matrix job
        // are run together, as the job.
        let mut inputs = TaskInputs::default();
        let mut jobs = Vec::new();
        let mut outcomes: Vec<JobOutcome> = Vec::new();
        // A copy of the setup workspace is another workspace: it gets an id of
        // its own, and the receipts of the setup are bound to it, as the copy
        // holds what the setup made.
        let copy_id = (ws != self.wt)
            .then(|| setup::ensure_workspace_id(ws).ok())
            .flatten();
        let origin = setup::workspace_id(&self.wt);
        // The receipts are this run's own. In the setup workspace they are
        // believed as they are. A copy is checked once, here, before any task
        // wrote in it: what the setup made must be in it as the receipt says;
        // after that the tasks of the unit trust the receipt even when one of
        // them wrote next to what the install made. A copy that does not
        // match has its receipts checked again by each task, and ignored.
        let mut copy_checked: HashMap<String, bool> = HashMap::new();
        if copy_id.is_some() {
            if let Some((m, _)) = &self.receipts {
                for t in self.fp.tasks.iter().filter(|t| t.unit == ui) {
                    if let Some(r) = m.get(&t.id) {
                        copy_checked
                            .entry(t.id.clone())
                            .or_insert_with(|| setup::verify_artifacts(r, ws, true).is_ok());
                    }
                }
            }
        }
        for task in self.fp.tasks.iter().filter(|t| t.unit == ui) {
            if task.variant.as_ref().is_some_and(|v| v.index > 0) {
                continue;
            }
            let receipt = self.receipts.as_ref().and_then(|(m, c)| {
                m.get(&task.id).map(|r| match &copy_id {
                    Some(id) if origin.as_deref() == Some(r.workspace.as_str()) => {
                        (r.rebound(id), c)
                    }
                    _ => (r.clone(), c),
                })
            });
            let verify = copy_id.is_some() && !copy_checked.get(&task.id).copied().unwrap_or(false);
            match self.run_task(
                ws,
                task,
                &inputs,
                receipt.as_ref().map(|(r, c)| (r, *c)),
                verify,
                locked,
                true,
            ) {
                Ok(r) => {
                    for o in r.outcomes {
                        inputs.run_cancelled |= o.status == JobStatus::Cancelled;
                        outcomes.push(o.clone());
                        inputs.needs.insert(o.job.clone(), o);
                    }
                    jobs.extend(r.jobs);
                }
                Err(e) => {
                    let reason = format!("{e:#}");
                    // The jobs of a task that could not be run failed: the
                    // jobs that need them must see that, not a missing need.
                    for id in &task.jobs {
                        let o = JobOutcome {
                            job: id.clone(),
                            variant: None,
                            status: JobStatus::Failure,
                            outputs: Default::default(),
                            skipped: Some(reason.clone()),
                            ancestor_failed: false,
                        };
                        outcomes.push(o.clone());
                        inputs.needs.insert(id.clone(), o);
                    }
                    // every entry of a matrix job failed, not one in its place
                    for id in &task.jobs {
                        let job = self.plan.job(u.wf, id);
                        let base = job.name.clone().unwrap_or(id.clone());
                        let displays: Vec<String> =
                            match crate::dag::matrix_plan(self.fp.job(u.wf, id)) {
                                crate::dag::MatrixPlan::Static(v) => v
                                    .iter()
                                    .map(|c| format!("{base} ({})", matrix::describe(c)))
                                    .collect(),
                                _ => vec![base],
                            };
                        for display in displays {
                            jobs.push(JobResult {
                                workflow: w.file.clone(),
                                job: id.clone(),
                                display,
                                outcome: Outcome::Error(reason.clone()),
                                secs: 0.0,
                                steps: vec![],
                                cancelled: false,
                                time: None,
                            });
                        }
                    }
                }
            }
        }
        charge(&mut jobs, waited);
        UnitResult {
            unit: Some(ui),
            workflow: w.file.clone(),
            jobs,
            waited_secs: waited.total(),
            copy_secs: None,
            cached: None,
            outcomes,
        }
    }

    /// Whether a task holds the per-repository lock while it runs (see
    /// `unit_locked`).
    pub(crate) fn task_locked(&self, t: &ExecutionTask) -> bool {
        t.exclusive && (!self.isolated() || !t.services.is_empty())
    }

    /// Run the jobs of `task` in `ws`, in order, against the results `inputs`
    /// holds for the jobs it needs. `locked`: the caller holds the repository
    /// lock. `whole_matrix`: for a task that is one variant of a matrix job,
    /// run all the variants (as one job) instead.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_task(
        &self,
        ws: &Path,
        task: &ExecutionTask,
        inputs: &TaskInputs,
        prepared: Option<(&PreparedReceipt, &TemplateContext)>,
        // whether to hash what the receipt describes before believing it: for
        // a receipt that comes from outside, yes; for the run's own receipt
        // in the workspace it was made for (or a copy checked when it was
        // made), no
        verify_contents: bool,
        locked: bool,
        whole_matrix: bool,
    ) -> Result<TaskResult> {
        let wi = self
            .plan
            .selected
            .iter()
            .position(|s| s.wf.file == task.workflow)
            .with_context(|| format!("the plan has no workflow {}", task.workflow))?;
        let sel = &self.plan.selected[wi];
        let w = &sel.wf;
        for id in &task.jobs {
            let job = self.plan.job(wi, id);
            if let Some(n) = job
                .needs
                .iter()
                .find(|n| !task.jobs.contains(n) && !inputs.needs.contains_key(*n))
            {
                bail!(
                    "task {} needs the result of job `{n}`; none was given",
                    task.id
                );
            }
        }
        let only = match (&task.variant, whole_matrix) {
            (Some(v), false) => Some(v.index),
            _ => None,
        };
        // A receipt is believed only for this commit, this workspace and the
        // template key this task's recipe has here; otherwise the task does all
        // its setup itself and says so.
        let mut receipt_ignored = None;
        let places = match prepared {
            Some((r, tctx)) => match setup::derive_in(&self.plan, &self.hoisted, task)?
                .prepared_places(
                    r,
                    &self.fp.source.execution_commit,
                    setup::workspace_id(ws).as_deref(),
                    ws,
                    tctx,
                    verify_contents,
                ) {
                Ok(p) => p,
                Err(why) => {
                    receipt_ignored = Some(why);
                    HashMap::new()
                }
            },
            None => HashMap::new(),
        };
        let none = HashSet::new();
        let mut needs_ctx = inputs.context();
        let mut jobs = Vec::new();
        let mut outcomes = Vec::new();
        for id in &task.jobs {
            let job = self.plan.job(wi, id);
            // `failure()` of a job: a job it needs, or one of theirs, failed
            let ancestor_failed = job.needs.iter().any(|n| {
                outcomes
                    .iter()
                    .find(|o: &&JobOutcome| &o.job == n)
                    .or_else(|| inputs.needs.get(n))
                    .is_some_and(JobOutcome::failed_or_ancestor_failed)
            });
            let results = self.run_job(
                ws,
                w,
                &sel.event,
                job,
                &needs_ctx,
                ancestor_failed,
                inputs.run_cancelled || self.ctx.cancel.is_cancelled(),
                places.get(id).unwrap_or(&none),
                locked,
                only,
            );
            let parts: Vec<JobOutcome> = results
                .iter()
                .enumerate()
                .map(|(i, (r, o))| outcome_of(id, only.or(Some(i)), r, o))
                .collect();
            let outcome = match only {
                Some(_) => parts.into_iter().next().unwrap_or_else(|| JobOutcome {
                    job: id.clone(),
                    variant: only,
                    status: JobStatus::Success,
                    outputs: Default::default(),
                    skipped: None,
                    ancestor_failed: false,
                }),
                None => combine(id, parts),
            };
            let outcome = JobOutcome {
                ancestor_failed,
                ..outcome
            };
            needs_ctx.insert(id.clone(), task::ctx_of(&outcome));
            jobs.extend(results.into_iter().map(|r| r.0));
            outcomes.push(outcome);
        }
        Ok(TaskResult {
            task: task.id.clone(),
            jobs,
            outcomes,
            receipt_ignored,
            artifacts: artifact_io::published_by(
                &self.run_dir,
                &task.workflow,
                &task
                    .jobs
                    .iter()
                    .map(|j| dag::node_id(&task.workflow, j))
                    .collect::<Vec<_>>(),
            ),
        })
    }

    /// One job, all its matrix combinations. Returns each combination's result and outputs.
    #[allow(clippy::too_many_arguments)]
    fn run_job(
        &self,
        ws: &Path,
        w: &Workflow,
        event: &str,
        job: &Job,
        needs: &Map<String, Value>,
        // a job it needs, or one of theirs, failed
        ancestor_failed: bool,
        // the run was cancelled
        run_cancelled: bool,
        // steps (by index) the task's setup already did
        prepared: &HashSet<usize>,
        // the unit holds the per-repository lock
        locked: bool,
        // run only this entry of the matrix
        only: Option<usize>,
    ) -> Vec<(JobResult, Map<String, Value>)> {
        let gh = self.github_ctx(ws, w, event, &job.id);
        let base_name = job.name.clone().unwrap_or_else(|| job.id.clone());
        let skip = |display: String, reason: String| {
            (
                JobResult {
                    workflow: w.file.clone(),
                    job: job.id.clone(),
                    display,
                    outcome: Outcome::Skip(reason),
                    secs: 0.0,
                    steps: vec![],
                    cancelled: false,
                    time: None,
                },
                Map::new(),
            )
        };
        // the run was cancelled before this entry started
        let cancelled_skip = |display: String| {
            let mut r = skip(display, "cancelled".into());
            r.0.cancelled = true;
            r
        };
        let error = |display: String, reason: String| {
            (
                JobResult {
                    workflow: w.file.clone(),
                    job: job.id.clone(),
                    display,
                    outcome: Outcome::Error(reason),
                    secs: 0.0,
                    steps: vec![],
                    cancelled: false,
                    time: None,
                },
                Map::new(),
            )
        };

        let needs_of_job: Map<String, Value> = job
            .needs
            .iter()
            .filter_map(|n| needs.get(n).map(|v| (n.clone(), v.clone())))
            .collect();
        // The status functions of a job, as on GitHub: `success()` when every
        // job it needs succeeded (a skipped one does not count), `failure()`
        // when one failed, or any job further up did, `cancelled()` when the
        // run was cancelled.
        let results: Vec<String> = needs_of_job
            .values()
            .map(|v| v["result"].as_str().unwrap_or("success").to_string())
            .collect();
        let any_result = |r: &str| results.iter().any(|x| x == r);
        let needs_fns = expr::StatusFns {
            success: results.iter().all(|r| r == "success"),
            failure: ancestor_failed || any_result("failure"),
            cancelled: run_cancelled,
        };
        // why a job that is not run by its `if:` is not run
        let why_not = || {
            if needs_fns.success || !expr::success_gated(job.cond.as_deref()) {
                "if: false"
            } else if any_result("failure") {
                "needs failed"
            } else if any_result("cancelled") {
                "needs cancelled"
            } else if any_result("skipped") {
                "needs skipped"
            } else {
                "if: false"
            }
        };
        let mut ctx_map = Map::new();
        ctx_map.insert("github".into(), gh.clone());
        ctx_map.insert("needs".into(), Value::Object(needs_of_job.clone()));
        ctx_map.insert("runner".into(), self.runner_ctx());
        ctx_map.insert("inputs".into(), json!({}));
        ctx_map.insert("vars".into(), json!({}));
        ctx_map.insert("env".into(), json!({}));
        ctx_map.insert("matrix".into(), json!({}));
        ctx_map.insert("strategy".into(), json!({}));

        // The matrix may be an expression over `needs`.
        let combos = match &job.matrix {
            None => vec![Map::new()],
            Some(m) => {
                let ctx = Ctx::new(&ctx_map, Status::Success);
                let value = eval_strings(&matrix::yaml_to_json(m), &ctx);
                match value.and_then(|v| matrix::expand(&v)) {
                    Ok(c) => c,
                    Err(e) => return vec![error(base_name, format!("matrix: {e:#}"))],
                }
            }
        };
        if job.container {
            return vec![skip(base_name, "container: is not supported".into())];
        }
        if let Some(u) = &job.reusable {
            return vec![skip(
                base_name,
                format!("reusable workflow {u} is not supported"),
            )];
        }

        // Combinations that pass `if:` run after this loop.
        let mut out: Vec<Option<(JobResult, Map<String, Value>)>> = Vec::new();
        let mut todo: Vec<(usize, Map<String, Value>, String)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let total = combos.len();
        for (idx, combo) in combos.into_iter().enumerate() {
            let mut cm = ctx_map.clone();
            cm.insert("matrix".into(), Value::Object(combo.clone()));
            cm.insert(
                "strategy".into(),
                json!({"job-index": idx, "job-total": total}),
            );
            let mut display = {
                let ctx = Ctx::new(&cm, Status::Success);
                match &job.name {
                    Some(n) if n.contains("${{") => {
                        expr::interpolate(n, &ctx).unwrap_or_else(|_| n.clone())
                    }
                    Some(n) if !combo.is_empty() => format!("{n} ({})", matrix::describe(&combo)),
                    Some(n) => n.clone(),
                    None if !combo.is_empty() => {
                        format!("{} ({})", job.id, matrix::describe(&combo))
                    }
                    None => job.id.clone(),
                }
            };
            // a name over some of the matrix only: keep the entries (and their logs) apart
            if !seen.insert(display.clone()) {
                display = format!("{display} #{}", idx + 1);
                seen.insert(display.clone());
            }
            if only.is_some_and(|o| o != idx) {
                continue;
            }
            let run_it = {
                let ctx = Ctx::with_fns(&cm, needs_fns);
                expr::condition(job.cond.as_deref(), &ctx)
            };
            match run_it {
                Ok(true) => {
                    todo.push((out.len(), cm, display));
                    out.push(None);
                }
                Ok(false) => out.push(Some(skip(display, why_not().into()))),
                Err(e) => out.push(Some(error(display, format!("if: {e:#}")))),
            }
        }

        let exclusive = plan::is_exclusive(job);
        let failed = AtomicBool::new(false);
        let stop = job.fail_fast.then_some(&failed);
        let one = |ws: &Path, cm: Map<String, Value>, display: String| {
            // Something rewriting cannot reach (a `container_name:`) still needs
            // the lock. Taken before the slot, in the order units take them.
            let t_lock = Instant::now();
            let lock = if exclusive && !locked && self.fixed_resources(ws, w, job, &cm) {
                self.lock().ok()
            } else {
                None
            };
            let wait_lock = t_lock.elapsed().as_secs_f64();
            let t_slot = Instant::now();
            let slot = if exclusive {
                match self.container_slot() {
                    Ok(Some(s)) => Some(s),
                    Ok(None) => return cancelled_skip(display),
                    // no lock files: run without a slot rather than not at all
                    Err(_) => None,
                }
            } else {
                None
            };
            let wait_slot = t_slot.elapsed().as_secs_f64();
            if self.ctx.cancel.is_cancelled() {
                return cancelled_skip(display);
            }
            if stop.is_some_and(|s| s.load(Ordering::SeqCst)) {
                return skip(display, "fail-fast".into());
            }
            let mut r = self.run_combo(ws, w, job, prepared, cm, display, stop);
            drop(slot);
            drop(lock);
            let secs = r.0.secs;
            let t = r.0.time.get_or_insert_with(|| JobTime {
                work_secs: secs,
                ..JobTime::default()
            });
            t.wait_lock_secs += wait_lock;
            t.wait_slot_secs += wait_slot;
            if matches!(r.0.outcome, Outcome::Fail | Outcome::Error(_)) {
                failed.store(true, Ordering::SeqCst);
            }
            r
        };
        // Each combination at once, in its own copy of the workspace, as each
        // gets its own runner on GitHub. Only with task-traffic-controller, which keeps the CPU
        // in check, and for exclusive jobs only when they cannot collide.
        let at_once = job.max_parallel.unwrap_or(usize::MAX);
        if todo.len() > 1
            && at_once > 1
            && self.opts.traffic_controller.is_some()
            && (!exclusive || !locked)
        {
            let done = Mutex::new(Vec::new());
            let sem = Semaphore {
                free: Mutex::new(at_once),
                cv: Condvar::new(),
            };
            std::thread::scope(|scope| {
                for (i, cm, display) in todo {
                    let (one, done, sem) = (&one, &done, &sem);
                    scope.spawn(move || {
                        sem.acquire();
                        // the pid stays last in the name: cleanup scripts read it from there
                        let copy = self.wt.with_file_name(format!(
                            "{}-c{}-{}",
                            &self.fp.source.execution_commit[..7],
                            self.seq.fetch_add(1, Ordering::SeqCst),
                            std::process::id()
                        ));
                        let r = match self.copy_of(ws, &copy) {
                            Ok(()) => one(&copy, cm, display),
                            Err(e) => error(display, format!("cannot copy the worktree: {e:#}")),
                        };
                        self.remove_copy(&copy);
                        sem.release();
                        done.lock().unwrap().push((i, r));
                    });
                }
            });
            for (i, r) in done.into_inner().unwrap() {
                out[i] = Some(r);
            }
        } else {
            for (i, cm, display) in todo {
                out[i] = Some(one(ws, cm, display));
            }
        }
        out.into_iter().flatten().collect()
    }

    /// Evaluate `pairs` into `into`. Says whether a secret that cannot be
    /// supplied was read.
    fn eval_env(
        host: &ExecContext,
        pairs: &[(String, String)],
        cm: &Map<String, Value>,
        into: &mut Map<String, Value>,
        maps: &[PortMap],
    ) -> Result<bool> {
        let mut secrets = false;
        for (k, v) in pairs {
            let mut ctx_map = cm.clone();
            ctx_map.insert("env".into(), Value::Object(into.clone()));
            let ctx = Ctx::new(&ctx_map, Status::Success).with_token(host);
            let val = expr::interpolate(v, &ctx)?;
            secrets |= ctx.used_secrets.get();
            into.insert(k.clone(), Value::String(rewrite_env_value(k, &val, maps)));
        }
        Ok(secrets)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_combo(
        &self,
        ws: &Path,
        w: &Workflow,
        job: &Job,
        prepared: &HashSet<usize>,
        mut cm: Map<String, Value>,
        display: String,
        // set when another entry of a fail-fast matrix failed
        stop: Option<&AtomicBool>,
    ) -> (JobResult, Map<String, Value>) {
        let t0 = Instant::now();
        let mut stopped = false;
        let mut steps: Vec<StepResult> = Vec::new();
        let mut names = Vec::new();
        let finish = |outcome: Outcome, steps: Vec<StepResult>, t0: Instant| JobResult {
            workflow: w.file.clone(),
            job: job.id.clone(),
            display: display.clone(),
            outcome,
            secs: t0.elapsed().as_secs_f64(),
            steps,
            cancelled: false,
            time: None,
        };

        // services
        let mut maps = Vec::new();
        let mut services_ctx = Map::new();
        if !job.services.is_empty() {
            match self.start_services(w, job, &cm, &mut names) {
                Ok((m, sc)) => {
                    maps = m;
                    services_ctx = sc;
                }
                Err(e) => {
                    self.stop_services(&names);
                    let mut r = finish(Outcome::Error(format!("services: {e:#}")), steps, t0);
                    r.cancelled = self.ctx.cancel.is_cancelled();
                    return (r, Map::new());
                }
            }
        }
        cm.insert(
            "job".into(),
            json!({"status": "success", "services": services_ctx}),
        );

        // A job that starts containers or test servers must not bind what
        // another run, or a matrix entry beside it, binds.
        let exclusive = plan::is_exclusive(job);
        let port_env = match &self.opts.portool {
            // Not with `services:`: the job's database is the service, and a
            // script that honours portool's ports would look for its own instead.
            Some(bin)
                if exclusive && job.services.is_empty() && ws.join(".portool.toml").is_file() =>
            {
                // before the sync: it may take a block and still fail
                self.synced.lock().unwrap().insert(ws.to_path_buf());
                match crate::portool::sync(self.ctx, bin, ws) {
                    Ok(v) => v,
                    Err(e) => {
                        self.stop_services(&names);
                        return (
                            finish(Outcome::Error(format!("{e:#}")), steps, t0),
                            Map::new(),
                        );
                    }
                }
            }
            _ => vec![],
        };

        // workflow + job env
        let mut env_ctx = Map::new();
        if let Err(e) = Self::eval_env(self.ctx, &w.env, &cm, &mut env_ctx, &maps)
            .and_then(|_| Self::eval_env(self.ctx, &job.env, &cm, &mut env_ctx, &maps))
            .and_then(|_| {
                if exclusive {
                    let fixed: Vec<u16> = env_ctx
                        .iter()
                        .filter(|(k, _)| port_key(k))
                        .filter_map(|(_, v)| port_literal(&expr::to_string(v)))
                        .collect();
                    map_literals(fixed, &mut maps)?;
                    for (k, v) in env_ctx.iter_mut() {
                        let s = expr::to_string(v);
                        let r = rewrite_env_value(k, &s, &maps);
                        if r != s {
                            *v = Value::String(r);
                        }
                    }
                }
                Ok(())
            })
        {
            self.stop_services(&names);
            return (
                finish(Outcome::Error(format!("env: {e:#}")), steps, t0),
                Map::new(),
            );
        }
        let job_timeout = Duration::from_secs_f64(
            60.0 * job.timeout_minutes.unwrap_or(self.opts.job_timeout_min),
        );
        let mut steps_ctx = Map::new();
        let mut extra_env: Vec<(String, String)> = Vec::new();
        let mut extra_path: Vec<String> = Vec::new();
        let mut failed = false;
        // `docker compose` without `-p` would share the project of the main checkout's directory.
        let compose_default = format!(
            "local-ci-{}-{}",
            std::process::id(),
            self.seq.fetch_add(1, Ordering::SeqCst)
        );
        let tag = format!("lc{}", &compose_default["local-ci-".len()..]);
        let mut projects = vec![compose_default.clone()];
        self.ctx.cancel.track_compose(compose_default.clone());
        let mut errored: Option<String> = None;
        // the first error was the run being cancelled
        let mut cancelled = false;
        // task-traffic-controller's queue, summed over the steps
        let mut queue = Duration::ZERO;

        for (n, s) in job.steps.iter().enumerate() {
            let raw_label = s.label();
            let label = if raw_label.contains("${{") {
                let mut lcm = cm.clone();
                lcm.insert("steps".into(), Value::Object(steps_ctx.clone()));
                lcm.insert("env".into(), Value::Object(env_ctx.clone()));
                expr::interpolate(&raw_label, &Ctx::new(&lcm, Status::Success)).unwrap_or(raw_label)
            } else {
                raw_label
            };
            // a label comes to result.json: the token does not
            let label = self.hide_token(label);
            let st0 = Instant::now();
            let push = |steps: &mut Vec<StepResult>,
                        outcome: Outcome,
                        exit: Option<i32>,
                        log: Option<PathBuf>,
                        note: Option<String>| {
                steps.push(StepResult {
                    label: label.clone(),
                    outcome,
                    exit,
                    secs: st0.elapsed().as_secs_f64(),
                    log,
                    note,
                });
            };
            stopped |= stop.is_some_and(|s| s.load(Ordering::SeqCst));
            let status = if self.ctx.cancel.is_cancelled() || stopped {
                Status::Cancelled
            } else if failed || errored.is_some() {
                Status::Failure
            } else {
                Status::Success
            };
            let mut scm = cm.clone();
            scm.insert("steps".into(), Value::Object(steps_ctx.clone()));
            let mut step_env = env_ctx.clone();
            for (k, v) in &extra_env {
                step_env.insert(k.clone(), Value::String(v.clone()));
            }
            scm.insert("env".into(), Value::Object(step_env.clone()));
            let go = expr::condition(
                s.cond.as_deref(),
                &Ctx::new(&scm, status).with_token(self.ctx),
            );
            match go {
                Ok(true) => {}
                Ok(false) => {
                    push(
                        &mut steps,
                        Outcome::Skip(if status == Status::Success {
                            "if: false".into()
                        } else {
                            "earlier step failed".into()
                        }),
                        None,
                        None,
                        None,
                    );
                    record(&mut steps_ctx, &s.id, "skipped", "skipped", Map::new());
                    continue;
                }
                Err(e) => {
                    let e = self.hide_token(format!("{e:#}"));
                    push(
                        &mut steps,
                        Outcome::Error(format!("if: {e}")),
                        None,
                        None,
                        None,
                    );
                    errored.get_or_insert(format!("{label}: if: {e}"));
                    continue;
                }
            }

            if let Some(uses) = &s.uses {
                let (outcome, note) = match artifact::parse_uses(uses) {
                    Some(Ok(kind)) => self.artifact_step(ws, w, job, &display, s, &scm, kind),
                    Some(Err(e)) => (Outcome::Error(e), None),
                    None => self.check_action(uses, s, &scm),
                };
                // what a `with:` value expanded to comes to result.json
                let outcome = match outcome {
                    Outcome::Error(e) => Outcome::Error(self.hide_token(e)),
                    Outcome::Skip(e) => Outcome::Skip(self.hide_token(e)),
                    o => o,
                };
                let note = note.map(|n| self.hide_token(n));
                if let Outcome::Error(e) = &outcome {
                    errored.get_or_insert(format!("{label}: {e}"));
                }
                let o = if matches!(outcome, Outcome::Pass) {
                    "success"
                } else {
                    "skipped"
                };
                push(&mut steps, outcome, None, None, note);
                record(&mut steps_ctx, &s.id, o, o, Map::new());
                continue;
            }
            let Some(run) = &s.run else {
                push(
                    &mut steps,
                    Outcome::Skip("no run or uses".into()),
                    None,
                    None,
                    None,
                );
                continue;
            };
            if prepared.contains(&n) {
                push(&mut steps, Outcome::Skip("setup".into()), None, None, None);
                record(&mut steps_ctx, &s.id, "success", "success", Map::new());
                continue;
            }
            if crate::script::remote_effect(run) {
                push(
                    &mut steps,
                    Outcome::Skip("pushes, publishes or deploys".into()),
                    None,
                    None,
                    None,
                );
                record(&mut steps_ctx, &s.id, "skipped", "skipped", Map::new());
                continue;
            }
            if crate::script::global_install(run, &|c| self.ctx.find_tool(c).is_some())
                == crate::script::Install::Needed
            {
                push(
                    &mut steps,
                    Outcome::Skip("installs system packages".into()),
                    None,
                    None,
                    None,
                );
                record(&mut steps_ctx, &s.id, "skipped", "skipped", Map::new());
                continue;
            }
            // interpolate
            let ctx = Ctx::new(&scm, status).with_token(self.ctx);
            let script = expr::interpolate(run, &ctx);
            let mut env_now = step_env.clone();
            // An install that another job runs here with another shell or env
            // is this job's own: the npm shim must not take another job's mark
            // of it for its own.
            if plan::step_key(w, job, s, &self.plan.ignore_env)
                .is_some_and(|k| self.hoisted.conflicting.contains(&k))
            {
                env_now.insert(
                    "LOCAL_CI_INSTALL_SCOPE".into(),
                    Value::String(job.id.clone()),
                );
            }
            let env_res = Self::eval_env(self.ctx, &s.env, &scm, &mut env_now, &maps);
            let secrets = ctx.used_secrets.get() || matches!(env_res, Ok(true));
            let (script, mut env_now) = match (script, env_res) {
                (Ok(sc), Ok(_)) => (sc, env_now),
                (Err(e), _) | (_, Err(e)) => {
                    let e = self.hide_token(format!("{e:#}"));
                    push(&mut steps, Outcome::Error(e.clone()), None, None, None);
                    errored.get_or_insert(format!("{label}: {e}"));
                    continue;
                }
            };
            let script = if exclusive {
                let fixed: Vec<u16> = assigned_ports(&script)
                    .into_iter()
                    .chain(
                        env_now
                            .iter()
                            .filter(|(k, _)| port_key(k))
                            .filter_map(|(_, v)| port_literal(&expr::to_string(v))),
                    )
                    .collect();
                if let Err(e) = map_literals(fixed, &mut maps) {
                    push(
                        &mut steps,
                        Outcome::Error(format!("{e:#}")),
                        None,
                        None,
                        None,
                    );
                    errored.get_or_insert(format!("{label}: {e:#}"));
                    continue;
                }
                for (k, v) in env_now.iter_mut() {
                    let s = expr::to_string(v);
                    let mut r = rewrite_env_value(k, &s, &maps);
                    if k == "COMPOSE_PROJECT_NAME"
                        && !r.is_empty()
                        && !r.ends_with(&format!("-{tag}"))
                    {
                        r = format!("{r}-{tag}");
                    }
                    if r != s {
                        *v = Value::String(r);
                    }
                }
                rename_compose_projects(&rewrite_ports(&script, &maps), &tag)
            } else {
                rewrite_ports(&script, &maps)
            };
            // A script or env that holds the token is run with the script
            // handed over a pipe, never written to a file.
            let token = self.ctx.github_token_if_fetched().filter(|t| !t.is_empty());
            // (A custom shell cannot be given a pipe, see `shell_once`; it is
            // handed a file, so only a script that holds the token is refused,
            // not one that gets the token through its environment.)
            let step_shell = s
                .shell
                .as_deref()
                .or(job.shell.as_deref())
                .or(w.shell.as_deref());
            let custom_shell = !matches!(
                step_shell.map(str::trim),
                None | Some("bash") | Some("sh") | Some("python")
            );
            let piped = token.is_some_and(|t| {
                script.contains(t)
                    || (!custom_shell && env_now.values().any(|v| expr::to_string(v).contains(t)))
            });
            if secrets {
                push(
                    &mut steps,
                    Outcome::Skip("uses secrets".into()),
                    None,
                    None,
                    None,
                );
                record(&mut steps_ctx, &s.id, "skipped", "skipped", Map::new());
                continue;
            }
            let wd = plan::step_dir(w, job, s);
            let files_dir = self
                .tools
                .join(format!("files/{}", self.seq.fetch_add(1, Ordering::SeqCst)));
            let _ = fs::create_dir_all(&files_dir);
            {
                use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
                let _ = fs::set_permissions(&files_dir, fs::Permissions::from_mode(0o700));
                for f in ["output", "env", "path", "summary"] {
                    let _ = OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .mode(0o600)
                        .open(files_dir.join(f));
                }
            }
            let fc = |name: &str| files_dir.join(name);
            let gh = scm["github"].clone();
            let mut env: Vec<(String, String)> = self.base_env(&gh, &extra_path);
            env.extend(port_env.iter().cloned());
            env.push((
                "GITHUB_OUTPUT".into(),
                fc("output").to_string_lossy().into_owned(),
            ));
            env.push((
                "GITHUB_ENV".into(),
                fc("env").to_string_lossy().into_owned(),
            ));
            env.push((
                "GITHUB_PATH".into(),
                fc("path").to_string_lossy().into_owned(),
            ));
            env.push((
                "GITHUB_STEP_SUMMARY".into(),
                fc("summary").to_string_lossy().into_owned(),
            ));
            env.push(("COMPOSE_PROJECT_NAME".into(), compose_default.clone()));
            let from_env = env_now
                .get("COMPOSE_PROJECT_NAME")
                .map(expr::to_string)
                .filter(|p| !p.is_empty());
            for p in compose_projects(&script).into_iter().chain(from_env) {
                if !projects.contains(&p) {
                    self.ctx.cancel.track_compose(p.clone());
                    projects.push(p);
                }
            }
            for (k, v) in &env_now {
                env.push((k.clone(), expr::to_string(v)));
            }
            let log = self.log_path(&w.file, &display, n + 1, &label);
            let remaining = job_timeout.saturating_sub(t0.elapsed());
            let timeout = s
                .timeout_minutes
                .map(|m| Duration::from_secs_f64(m * 60.0))
                .map(|t| t.min(remaining))
                .unwrap_or(remaining);
            let key = format!(
                "local-ci {} {} {}",
                crate::report::short(&self.fp.config.default_workflows_dir, &w.file),
                job.id,
                label
            );
            let pkg = fs::read_to_string(ws.join(&wd).join("package.json")).ok();
            if writes_node_modules(&plan::with_scripts(&script, pkg.as_deref())) {
                if let Err(e) = crate::git::own_node_modules(self.ctx, ws) {
                    let e = format!("cannot copy node_modules: {e:#}");
                    push(&mut steps, Outcome::Error(e.clone()), None, None, None);
                    errored.get_or_insert(format!("{label}: {e}"));
                    continue;
                }
            }
            let res = self
                .shell(
                    &ws.join(&wd),
                    &script,
                    step_shell,
                    &env,
                    Some(timeout),
                    &log,
                    Some(key),
                    piped,
                )
                .map(|(exit, queued)| {
                    queue += queued;
                    exit
                });
            if let Some(t) = self.ctx.github_token_if_fetched() {
                mask_in_file(&log, t);
            }
            let read = |f: &str| fs::read_to_string(fc(f)).unwrap_or_default();
            let mut outputs = Map::new();
            // Later steps of the job get the value, as on GitHub; it is in
            // memory only (the files are gone below). What leaves the job
            // is checked at the job's outputs.
            for (k, v) in parse_file_command(&read("output")) {
                outputs.insert(k, Value::String(v));
            }
            extra_env.extend(parse_file_command(&read("env")));
            extra_path.extend(
                read("path")
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(String::from),
            );
            let _ = fs::remove_dir_all(&files_dir);
            match res {
                Ok(Exit::Code(0)) => {
                    push(&mut steps, Outcome::Pass, Some(0), Some(log), None);
                    record(&mut steps_ctx, &s.id, "success", "success", outputs);
                }
                Ok(Exit::Code(c)) => {
                    let ce = s.continue_on_error;
                    push(
                        &mut steps,
                        Outcome::Fail,
                        Some(c),
                        Some(log),
                        if ce {
                            Some("continue-on-error".into())
                        } else {
                            None
                        },
                    );
                    record(
                        &mut steps_ctx,
                        &s.id,
                        "failure",
                        if ce { "success" } else { "failure" },
                        outputs,
                    );
                    if !ce {
                        failed = true;
                    }
                }
                Ok(Exit::Timeout) => {
                    push(
                        &mut steps,
                        Outcome::Error(format!("timed out after {}s", timeout.as_secs())),
                        None,
                        Some(log),
                        None,
                    );
                    record(&mut steps_ctx, &s.id, "failure", "failure", outputs);
                    errored.get_or_insert(format!("{label}: timed out"));
                }
                Ok(Exit::Cancelled) => {
                    push(
                        &mut steps,
                        Outcome::Error("cancelled".into()),
                        None,
                        Some(log),
                        None,
                    );
                    if errored.is_none() {
                        errored = Some("cancelled".into());
                        cancelled = true;
                    }
                }
                Err(e) => {
                    push(
                        &mut steps,
                        Outcome::Error(format!("{e:#}")),
                        None,
                        Some(log),
                        None,
                    );
                    errored.get_or_insert(format!("{label}: {e:#}"));
                }
            }
        }
        self.stop_services(&names);
        compose_down(self.ctx, &projects);
        self.ctx.cancel.untrack_compose(&projects);

        // job outputs
        let mut outputs = Map::new();
        let mut ocm = cm.clone();
        ocm.insert("steps".into(), Value::Object(steps_ctx));
        let octx = Ctx::new(&ocm, Status::Success);
        let mut dropped = Vec::new();
        for (k, v) in &job.outputs {
            if let Ok(val) = expr::interpolate(v, &octx) {
                // GitHub does not pass a job output that holds a secret on
                // to the jobs that need it, and nor does this
                if self
                    .ctx
                    .github_token_if_fetched()
                    .is_some_and(|t| !t.is_empty() && val.contains(t))
                {
                    dropped.push(k.clone());
                    // empty, which is what a name nobody set expands to; the
                    // key stays, because a plan expects the job to have set it
                    outputs.insert(k.clone(), Value::String(String::new()));
                    continue;
                }
                outputs.insert(k.clone(), Value::String(val));
            }
        }
        if !dropped.is_empty() {
            let note = format!(
                "job output {} holds the GitHub token: not passed on",
                dropped.join(", ")
            );
            // on the last step: a result has as many steps as the plan has
            if let Some(last) = steps.last_mut() {
                last.note = Some(match last.note.take() {
                    Some(n) => format!("{n}; {note}"),
                    None => note,
                });
            }
        }
        let outcome = if failed {
            Outcome::Fail
        } else if let Some(e) = errored {
            Outcome::Error(e)
        } else if stopped {
            Outcome::Skip("fail-fast: another entry failed".into())
        } else {
            Outcome::Pass
        };
        let mut result = finish(outcome, steps, t0);
        result.cancelled = cancelled && matches!(result.outcome, Outcome::Error(_));
        result.time = Some(JobTime {
            work_secs: (result.secs - queue.as_secs_f64()).max(0.0),
            wait_queue_secs: queue.as_secs_f64(),
            ..JobTime::default()
        });
        (result, outputs)
    }

    /// `uses:` steps: version checks for setup actions, skips for the rest.
    fn check_action(
        &self,
        uses: &str,
        s: &Step,
        cm: &Map<String, Value>,
    ) -> (Outcome, Option<String>) {
        let name = uses.split('@').next().unwrap_or(uses).to_ascii_lowercase();
        let ctx = Ctx::new(cm, Status::Success).with_token(self.ctx);
        let with = |k: &str| {
            s.with
                .iter()
                .find(|(n, _)| n == k)
                .and_then(|(_, v)| expr::interpolate(v, &ctx).ok())
                .filter(|v| !v.is_empty())
        };
        let tool = |bin: &str, args: &[&str]| -> Option<String> {
            let out = self.ctx.command(bin).args(args).output().ok()?;
            let text = String::from_utf8_lossy(&out.stdout).to_string()
                + &String::from_utf8_lossy(&out.stderr);
            out.status
                .success()
                .then(|| text.lines().next().unwrap_or("").trim().to_string())
        };
        let check = |bin: &str, args: &[&str], want: Option<String>| -> (Outcome, Option<String>) {
            match tool(bin, args) {
                None => (Outcome::Error(format!("`{bin}` not found")), None),
                Some(have) => {
                    let have_v = have.trim_start_matches(|c: char| !c.is_ascii_digit());
                    let note = match want {
                        Some(w)
                            if !have_v
                                .starts_with(w.trim_start_matches('v').trim_end_matches(".x")) =>
                        {
                            Some(format!("wants {w}, have {have}"))
                        }
                        _ => Some(have),
                    };
                    (Outcome::Pass, note)
                }
            }
        };
        let version_file = |key: &str| {
            with(key)
                .and_then(|f| fs::read_to_string(self.wt.join(f)).ok())
                .map(|t| t.trim().to_string())
        };
        match name.as_str() {
            "actions/checkout" => (Outcome::Skip("checkout".into()), None),
            "actions/setup-node" => check(
                "node",
                &["--version"],
                with("node-version").or_else(|| version_file("node-version-file")),
            ),
            "actions/setup-python" => check(
                "python3",
                &["--version"],
                with("python-version").or_else(|| version_file("python-version-file")),
            ),
            "actions/setup-go" => check("go", &["version"], None),
            "actions/setup-java" => check("java", &["-version"], None),
            "ruby/setup-ruby" => check("ruby", &["--version"], with("ruby-version")),
            "subosito/flutter-action" => check("flutter", &["--version"], with("flutter-version")),
            "dart-lang/setup-dart" => check("dart", &["--version"], None),
            n if n.starts_with("dtolnay/rust-toolchain") => check(
                "cargo",
                &["--version"],
                with("toolchain").filter(|t| t.starts_with(|c: char| c.is_ascii_digit())),
            ),
            n if n.starts_with("actions/cache") || n.ends_with("/rust-cache") => {
                (Outcome::Skip("cache".into()), None)
            }
            n if n.contains("upload-artifact") || n.contains("download-artifact") => {
                (Outcome::Skip("artifact".into()), None)
            }
            _ => (Outcome::Skip(format!("action {uses}")), None),
        }
    }

    /// An `actions/upload-artifact@v4` or `actions/download-artifact@v4` step
    /// of this run (see [`crate::artifact_io`]).
    #[allow(clippy::too_many_arguments)]
    fn artifact_step(
        &self,
        ws: &Path,
        w: &Workflow,
        job: &Job,
        display: &str,
        s: &Step,
        cm: &Map<String, Value>,
        kind: artifact::Kind,
    ) -> (Outcome, Option<String>) {
        let ctx = Ctx::new(cm, Status::Success).with_token(self.ctx);
        let mut with = Vec::new();
        for (k, v) in &s.with {
            match expr::interpolate(v, &ctx) {
                Ok(x) => with.push((k.clone(), x)),
                Err(e) => return (Outcome::Error(format!("with.{k}: {e:#}")), None),
            }
        }
        let area = artifact_io::Area::of_run(&self.run_dir, &self.fp.digest, &w.file);
        let node = dag::node_id(&w.file, &job.id);
        let mut holding = self
            .fp
            .tasks
            .iter()
            .filter(|t| t.workflow == w.file && t.jobs.contains(&job.id));
        let task = match (holding.next(), holding.next()) {
            (Some(t), None) => Some(t.id.clone()),
            _ => None,
        };
        let who = artifact_io::Who {
            job: node,
            display: display.to_string(),
            task,
        };
        let done = match kind {
            artifact::Kind::Upload => artifact_io::upload(&area, &who, ws, &with),
            artifact::Kind::Download => artifact_io::download(&area, ws, &with),
        };
        match done {
            Ok(note) => {
                // a step pinned to a commit runs as v4, and says so
                let note = match s.uses.as_deref().and_then(artifact::pin_warning) {
                    Some(w) => format!("{w}; {note}"),
                    None => note,
                };
                (Outcome::Pass, Some(note))
            }
            Err(e) => (Outcome::Error(e), None),
        }
    }

    fn docker_ok(&self) -> Result<(), String> {
        self.docker_ok
            .get_or_init(|| {
                match self
                    .ctx
                    .command("docker")
                    .args(["info", "--format", "{{.ServerVersion}}"])
                    .output()
                {
                    Ok(o) if o.status.success() => Ok(()),
                    Ok(o) => Err(String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .next()
                        .unwrap_or("docker info failed")
                        .to_string()),
                    Err(e) => Err(format!("docker: {e}")),
                }
            })
            .clone()
    }

    fn docker(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let mut child = self
            .ctx
            .command("docker")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let t0 = Instant::now();
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if t0.elapsed() > timeout || self.ctx.cancel.is_cancelled() {
                let _ = child.kill();
                bail!(
                    "docker {} did not finish in {}s",
                    args.first().unwrap_or(&""),
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "docker {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn start_services(
        &self,
        w: &Workflow,
        job: &Job,
        cm: &Map<String, Value>,
        names: &mut Vec<String>,
    ) -> Result<(Vec<PortMap>, Map<String, Value>)> {
        self.docker_ok()
            .map_err(|e| anyhow!("docker is not available ({e})"))?;
        let ctx = Ctx::new(cm, Status::Success);
        let mut maps = Vec::new();
        let mut sctx = Map::new();
        for svc in &job.services {
            let image = expr::interpolate(&svc.image, &ctx)?;
            if self
                .docker(&["image", "inspect", &image], Duration::from_secs(30))
                .is_err()
            {
                self.docker(&["pull", "--quiet", &image], Duration::from_secs(600))
                    .with_context(|| format!("pull {image}"))?;
            }
            let name = format!(
                "local-ci-{}-{}",
                std::process::id(),
                self.seq.fetch_add(1, Ordering::SeqCst)
            );
            let mut args: Vec<String> = vec![
                "run".into(),
                "-d".into(),
                "--name".into(),
                name.clone(),
                "--label".into(),
                "local-ci=1".into(),
            ];
            for (k, v) in &svc.env {
                args.push("-e".into());
                args.push(format!("{k}={}", expr::interpolate(v, &ctx)?));
            }
            for (d, c) in &svc.ports {
                args.push("-p".into());
                args.push(match d {
                    // nothing to rewrite: the code finds the service by this port
                    Some(h) if !rewritable_mention(w, job, *h) => format!("127.0.0.1:{h}:{c}"),
                    _ => format!("127.0.0.1::{c}"),
                });
            }
            let opts = expr::interpolate(&svc.options, &ctx)?;
            args.extend(
                shlex::split(&opts).ok_or_else(|| anyhow!("cannot parse options `{opts}`"))?,
            );
            args.push(image.clone());
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            self.docker(&refs, Duration::from_secs(120))?;
            self.ctx.cancel.track_container(name.clone());
            names.push(name.clone());
            let mut ports = Map::new();
            for (d, c) in &svc.ports {
                let line = self.docker(
                    &["port", &name, &format!("{c}/tcp")],
                    Duration::from_secs(30),
                )?;
                let actual: u16 = line
                    .lines()
                    .next()
                    .and_then(|l| l.rsplit(':').next())
                    .and_then(|p| p.trim().parse().ok())
                    .ok_or_else(|| anyhow!("no host port for {c}"))?;
                ports.insert(c.to_string(), Value::String(actual.to_string()));
                maps.push(PortMap {
                    declared: *d,
                    container: *c,
                    actual,
                });
            }
            self.wait_healthy(&name, svc, &maps)?;
            sctx.insert(
                svc.id.clone(),
                json!({"id": name, "ports": ports, "network": ""}),
            );
        }
        Ok((maps, sctx))
    }

    fn wait_healthy(&self, name: &str, svc: &Service, maps: &[PortMap]) -> Result<()> {
        let has_health = svc.options.contains("--health-cmd");
        let t0 = Instant::now();
        let limit = Duration::from_secs(180);
        loop {
            if self.ctx.cancel.is_cancelled() {
                bail!("cancelled");
            }
            let state = self.docker(&["inspect", "-f", "{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}", name], Duration::from_secs(30))?;
            let mut parts = state.split_whitespace();
            let (run, health) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            if run == "exited" || run == "dead" {
                let logs = self
                    .ctx
                    .command("docker")
                    .args(["logs", "--tail", "15", name])
                    .output()
                    .map(|o| {
                        String::from_utf8_lossy(&o.stderr).to_string()
                            + &String::from_utf8_lossy(&o.stdout)
                    })
                    .unwrap_or_default();
                bail!("service {} exited:\n{}", svc.id, logs.trim());
            }
            if has_health {
                if health == "healthy" {
                    return Ok(());
                }
                if health == "unhealthy" {
                    bail!("service {} is unhealthy", svc.id);
                }
            } else {
                let mine: Vec<&PortMap> = maps
                    .iter()
                    .filter(|m| svc.ports.iter().any(|(_, c)| *c == m.container))
                    .collect();
                if run == "running"
                    && mine.iter().all(|m| {
                        std::net::TcpStream::connect_timeout(
                            &([127, 0, 0, 1], m.actual).into(),
                            Duration::from_secs(1),
                        )
                        .is_ok()
                    })
                {
                    return Ok(());
                }
            }
            if t0.elapsed() > limit {
                bail!("service {} not ready after {}s", svc.id, limit.as_secs());
            }
            std::thread::sleep(Duration::from_millis(1000));
        }
    }

    fn stop_services(&self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let _ = self
            .ctx
            .command("docker")
            .arg("rm")
            .arg("-f")
            .args(names)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let set: HashSet<&String> = names.iter().collect();
        self.ctx.cancel.untrack_containers(&set);
    }
}

/// A pipe for a script that holds the token, made so that neither end is ever
/// open without close-on-exec, so that no command started by another thread
/// can inherit an end (the write end, or the read end with the rest of the
/// script). The write end is non-blocking (see `write_until_stopped`); the
/// read end blocks, as the shell reads with plain blocking reads.
///
/// macOS: `std::io::pipe` is `pipe()` and then `fcntl(FD_CLOEXEC)`, two calls
/// with a window between them, so a FIFO is used: it is opened with
/// `O_CLOEXEC` in the call itself, and its name is removed as soon as both
/// ends are open. A FIFO is not used elsewhere: on Linux, opening
/// `/dev/fd/3` opens the FIFO again through `/proc/self/fd/3`, and a blocking
/// `O_RDONLY` open of a FIFO waits for a writer; a small script is written
/// and the write end closed before the shell starts, so the shell would wait
/// for ever.
#[cfg(target_os = "macos")]
fn script_pipe(path: &Path) -> Result<(File, File)> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let _ = fs::remove_file(path);
    // SAFETY: plain libc calls on a NUL-terminated path; the descriptors are
    // handed to `File`, which owns them from then on.
    unsafe {
        if libc::mkfifo(c.as_ptr(), 0o600) < 0 {
            return Err(std::io::Error::last_os_error()).context("cannot make the script pipe");
        }
        let r = libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        );
        let r_err = std::io::Error::last_os_error();
        let w = if r >= 0 {
            libc::open(
                c.as_ptr(),
                libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        } else {
            -1
        };
        let w_err = std::io::Error::last_os_error();
        let _ = libc::unlink(c.as_ptr());
        if r < 0 || w < 0 {
            if r >= 0 {
                libc::close(r);
            }
            if w >= 0 {
                libc::close(w);
            }
            return Err(if r < 0 { r_err } else { w_err }).context("cannot open the script pipe");
        }
        // the shell reads with plain blocking reads
        let fl = libc::fcntl(r, libc::F_GETFL);
        if fl < 0 || libc::fcntl(r, libc::F_SETFL, fl & !libc::O_NONBLOCK) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(r);
            libc::close(w);
            return Err(e).context("cannot set up the script pipe");
        }
        Ok((File::from_raw_fd(r), File::from_raw_fd(w)))
    }
}

/// Linux and the other unixes: an anonymous pipe. The standard library makes
/// it with `pipe2(O_CLOEXEC)` on Linux, the BSDs and the like (one call, so
/// there is no window; see `library/std/src/sys/pal/unix/pipe.rs` in the
/// Rust source, and `strace -e trace=pipe2` on a program that calls
/// `std::io::pipe`).
#[cfg(not(target_os = "macos"))]
fn script_pipe(_path: &Path) -> Result<(File, File)> {
    use std::os::fd::{AsRawFd, OwnedFd};
    let (r, w) = std::io::pipe().context("cannot make the script pipe")?;
    let w = File::from(OwnedFd::from(w));
    // SAFETY: fcntl on a descriptor `w` owns. The write end is its own open
    // file description, so this does not touch the read end.
    unsafe {
        let fl = libc::fcntl(w.as_raw_fd(), libc::F_GETFL);
        if fl < 0 || libc::fcntl(w.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error()).context("cannot set up the script pipe");
        }
    }
    Ok((File::from(OwnedFd::from(r)), w))
}

/// Write `data` to the non-blocking `w`, until it is all written, the other
/// end is closed, or `stop` is set (the step is over). A reader that outlives
/// its step (a background process that kept the descriptor) cannot keep this
/// thread blocked in `write`.
fn write_until_stopped(mut w: File, data: &[u8], stop: &AtomicBool) {
    let mut rest = data;
    while !rest.is_empty() && !stop.load(Ordering::SeqCst) {
        match w.write(rest) {
            Ok(0) => return,
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Write a file only its owner can read: step scripts may hold values that
/// must not be shared.
fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    f.write_all(content.as_bytes())
}

/// Evaluate every `${{ }}` string inside a JSON value, keeping the result's type.
fn eval_strings(v: &Value, ctx: &Ctx) -> Result<Value> {
    Ok(match v {
        Value::String(s) if s.contains("${{") => expr::interpolate_value(s, ctx)?,
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| eval_strings(x, ctx))
                .collect::<Result<_>>()?,
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| Ok((k.clone(), eval_strings(x, ctx)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

fn record(
    ctx: &mut Map<String, Value>,
    id: &Option<String>,
    outcome: &str,
    conclusion: &str,
    outputs: Map<String, Value>,
) {
    if let Some(id) = id {
        ctx.insert(
            id.clone(),
            json!({"outcome": outcome, "conclusion": conclusion, "outputs": outputs}),
        );
    }
}

pub fn write_json(path: &Path, v: &impl Serialize) -> Result<()> {
    let mut f = File::create(path)?;
    f.write_all(serde_json::to_string_pretty(v)?.as_bytes())?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests build their own commands
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    // What this checks: the ends of the pipe carry close-on-exec, the write end is
    // non-blocking and the read end blocks, no name is left behind, and what is
    // written can be read back. It does NOT check that no other thread's exec can
    // inherit an end: that window (between `pipe()` and `fcntl` on macOS) cannot
    // be hit from a test, so it is closed by construction (a FIFO opened with
    // `O_CLOEXEC` on macOS, `pipe2(O_CLOEXEC)` through std elsewhere) and reviewed
    // in `script_pipe`, not tested.
    fn the_script_pipe_has_cloexec_a_nonblocking_writer_and_no_name_left() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.fifo");
        let (r, w) = script_pipe(&path).unwrap();
        assert!(!path.exists());
        for f in [&r, &w] {
            let fl = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFD) };
            assert!(fl >= 0 && fl & libc::FD_CLOEXEC != 0, "{fl}");
        }
        // the read end blocks, the write end does not
        let (rl, wl) = unsafe {
            (
                libc::fcntl(r.as_raw_fd(), libc::F_GETFL),
                libc::fcntl(w.as_raw_fd(), libc::F_GETFL),
            )
        };
        assert_eq!(rl & libc::O_NONBLOCK, 0);
        assert_ne!(wl & libc::O_NONBLOCK, 0);
        let mut r = r;
        let stop = AtomicBool::new(false);
        write_until_stopped(w, b"abc", &stop);
        let mut got = String::new();
        r.read_to_string(&mut got).unwrap();
        assert_eq!(got, "abc");
    }

    #[test]
    fn the_writer_ends_with_the_step_even_if_a_reader_outlives_it() {
        let dir = tempfile::tempdir().unwrap();
        let (r, w) = script_pipe(&dir.path().join("p.fifo")).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let big = vec![b'x'; 4 << 20];
        let s2 = stop.clone();
        let h = std::thread::spawn(move || write_until_stopped(w, &big, &s2));
        // nobody reads: the pipe fills and the writer waits
        std::thread::sleep(Duration::from_millis(300));
        assert!(!h.is_finished());
        stop.store(true, Ordering::SeqCst);
        let t = Instant::now();
        h.join().unwrap();
        assert!(t.elapsed() < Duration::from_secs(2));
        // `r` was held the whole time, as a background process would
        drop(r);
    }

    #[test]
    fn the_writer_ends_when_every_reader_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (r, w) = script_pipe(&dir.path().join("p.fifo")).unwrap();
        drop(r);
        let stop = AtomicBool::new(false);
        write_until_stopped(w, &vec![b'x'; 1 << 20], &stop);
    }

    fn job_with_secs(secs: f64) -> JobResult {
        JobResult {
            workflow: "ci.yml".into(),
            job: "j".into(),
            display: "j".into(),
            outcome: Outcome::Pass,
            secs,
            steps: vec![],
            cancelled: false,
            time: None,
        }
    }

    #[test]
    fn a_units_waits_are_charged_to_its_first_job_once() {
        let mut jobs = vec![job_with_secs(5.0), job_with_secs(7.0)];
        charge(
            &mut jobs,
            UnitWait {
                lock: 30.0,
                parallel: 2.0,
            },
        );
        assert_eq!(
            jobs[0].time,
            Some(JobTime {
                work_secs: 5.0,
                wait_lock_secs: 30.0,
                wait_parallel_secs: 2.0,
                ..JobTime::default()
            })
        );
        assert_eq!(jobs[1].time, None);
        // nothing waited: nothing recorded
        let mut jobs = vec![job_with_secs(5.0)];
        charge(&mut jobs, UnitWait::default());
        assert_eq!(jobs[0].time, None);
        // a job that did not run is passed over
        let mut ran = job_with_secs(3.0);
        ran.time = Some(JobTime {
            work_secs: 3.0,
            ..JobTime::default()
        });
        let mut jobs = vec![job_with_secs(0.0), ran];
        charge(
            &mut jobs,
            UnitWait {
                lock: 4.0,
                parallel: 0.0,
            },
        );
        assert_eq!(jobs[0].time, None);
        assert_eq!(jobs[1].time.as_ref().unwrap().wait_lock_secs, 4.0);
    }

    #[test]
    fn job_time_is_added_to_the_json_and_left_out_when_unknown() {
        let mut j = job_with_secs(5.0);
        let v = serde_json::to_value(&j).unwrap();
        assert!(v.get("time").is_none(), "{v}");
        assert_eq!(v["secs"], 5.0);
        j.time = Some(JobTime {
            work_secs: 4.0,
            wait_queue_secs: 1.0,
            ..JobTime::default()
        });
        let v = serde_json::to_value(&j).unwrap();
        assert_eq!(v["time"]["work_secs"], 4.0);
        assert_eq!(v["time"]["wait_queue_secs"], 1.0);
        assert_eq!(v["time"]["wait_slot_secs"], 0.0);
    }

    #[test]
    fn file_commands() {
        let v = parse_file_command("a=1\nmatrix<<EOF\n{\"x\":\n[1]}\nEOF\nb=two=2\n");
        assert_eq!(
            v,
            vec![
                ("a".into(), "1".into()),
                ("matrix".into(), "{\"x\":\n[1]}".into()),
                ("b".into(), "two=2".into())
            ]
        );
    }

    #[test]
    fn port_rewrites() {
        let maps = vec![PortMap {
            declared: Some(5433),
            container: 5432,
            actual: 49152,
        }];
        assert_eq!(
            rewrite_ports("postgresql://u:p@localhost:5433/db?x=1", &maps),
            "postgresql://u:p@localhost:49152/db?x=1"
        );
        assert_eq!(rewrite_ports("127.0.0.1:54330", &maps), "127.0.0.1:54330");
        assert_eq!(rewrite_env_value("DB_PORT", "5433", &maps), "49152");
        assert_eq!(rewrite_env_value("COUNT", "5433", &maps), "5433");
        assert_eq!(
            rewrite_ports("TEST_DB_PORT=5433 docker compose up; N=5433", &maps),
            "TEST_DB_PORT=49152 docker compose up; N=5433"
        );
        assert_eq!(rewrite_ports("PORT='5433' x", &maps), "PORT='49152' x");
    }

    #[test]
    fn service_ports_the_workflow_never_names() {
        let wf = |env: &str, run: &str| {
            crate::workflow::parse(
                "ci.yml",
                &format!(
                    "on: push\njobs:\n  t:\n    runs-on: ubuntu-latest\n    env: {{{env}}}\n    services:\n      db:\n        image: mysql\n        ports: ['15306:3306']\n    steps:\n      - run: {run}\n"
                ),
            )
            .unwrap()
        };
        let named = |w: &Workflow| rewritable_mention(w, &w.jobs[0], 15306);
        assert!(!named(&wf("", "npm test")));
        assert!(!named(&wf("COUNT: '15306'", "mysql -P 15306")));
        assert!(named(&wf("DB_PORT: '15306'", "npm test")));
        assert!(named(&wf(
            "",
            "DATABASE_URL=mysql://localhost:15306/x npm test"
        )));
        assert!(named(&wf(
            "",
            "PORT=${{ job.services.db.ports[3306] }} npm test"
        )));
    }

    #[test]
    fn fixed_ports_a_job_sets() {
        assert!(port_key("PORT") && port_key("TEST_DB_PORT") && port_key("port_e2e"));
        assert!(!port_key("EXPORT") && !port_key("REPORT") && !port_key("PORTS"));
        assert_eq!(
            assigned_ports("TEST_DB_PORT=15432 docker compose up\nEXPORT=4000 PORT=80 X=5000"),
            vec![15432]
        );
        assert!(
            assigned_ports("docker run -e POSTGRES_PORT=15432 --env=X_PORT=15433 img").is_empty()
        );
        let mut maps = vec![];
        map_literals([15432, 15432, 15433], &mut maps).unwrap();
        assert_eq!(maps.len(), 2);
        assert_ne!(maps[0].actual, maps[1].actual);
        assert_eq!(
            rewrite_ports("docker run -e PGPORT=15432 img; PGPORT=15432", &maps[..1]),
            format!("docker run -e PGPORT=15432 img; PGPORT={}", maps[0].actual)
        );
        assert_eq!(rewrite_env_value("REPORT", "15432", &maps), "15432");
        assert!(maps
            .iter()
            .all(|m| m.actual != 0 && m.declared != Some(m.actual)));
    }

    #[test]
    fn what_rewriting_cannot_reach() {
        assert_eq!(
            compose_files(
                "docker compose -f a.yml --file=b.yml -p x up -d; docker compose -f \"$F\" up"
            ),
            vec![Some("a.yml".into()), Some("b.yml".into()), None]
        );
        assert_eq!(compose_files("docker compose up -f nope").len(), 4);
        assert!(compose_fixed("services:\n  db:\n    container_name: db\n"));
        assert!(compose_fixed("    ports:\n      - \"5432:5432\"\n"));
        assert!(compose_fixed("    ports:\n      - 127.0.0.1:5432:5432\n"));
        assert!(compose_fixed(
            "      - target: 80\n        published: 8080\n"
        ));
        assert!(!compose_fixed(
            "    ports:\n      - \"${DB_PORT:-5432}:5432\"\n      - \"5432\"\n"
        ));
        assert!(docker_run_fixed("docker run -d --name db postgres"));
        assert!(docker_run_fixed("docker run \\\n  -p 5432:5432 postgres"));
        assert!(!docker_run_fixed(
            "docker run -p 127.0.0.1::5432 -e X=1 postgres; docker compose run --name x web"
        ));
    }

    #[test]
    fn ledger_timeouts_are_retried() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("step.log");
        fs::write(&log, "earlier output\n").unwrap();
        let from = fs::metadata(&log).unwrap().len();
        let mut f = OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "task-traffic-controller: waiting 0:00 (queue)").unwrap();
        writeln!(
            f,
            "task-traffic-controller: ledger: timed out after 10s waiting for /x/ledger.lock"
        )
        .unwrap();
        assert!(ledger_timed_out(&log, from));
        writeln!(f, "npm ERR! something").unwrap();
        assert!(!ledger_timed_out(&log, from));
        assert!(!ledger_timed_out(&log, 0));
    }

    #[test]
    fn only_the_controllers_prefix_marks_its_own_lines() {
        let p = "task-traffic-controller";
        assert!(is_controller_line(&format!("{p}: waiting 0:00 (queue)")));
        assert!(is_ledger_timeout_line(&format!(
            "{p}: ledger: timed out after 10s"
        )));
        assert!(!is_ledger_timeout_line(&format!("{p}: waiting")));
        assert!(!is_controller_line("canirun: waiting"));
        assert!(!is_ledger_timeout_line("canirun: ledger: timed out"));
        assert!(!is_ledger_timeout_line("ledger: timed out"));
    }

    #[test]
    fn compose_projects_get_a_tag() {
        assert_eq!(
            rename_compose_projects(
                "TEST_DB_PORT=1 docker compose \\\n  -p app-int \\\n  -f a.yml up -d",
                "lc1-2"
            ),
            "TEST_DB_PORT=1 docker compose \\\n  -p app-int-lc1-2 \\\n  -f a.yml up -d"
        );
        assert_eq!(
            rename_compose_projects(
                "mkdir -p app && docker compose --project-name=app down",
                "t"
            ),
            "mkdir -p app && docker compose --project-name=app-t down"
        );
        assert_eq!(
            rename_compose_projects("COMPOSE_PROJECT_NAME=\"z\" docker compose up", "t"),
            "COMPOSE_PROJECT_NAME=\"z-t\" docker compose up"
        );
        assert_eq!(
            rename_compose_projects("docker compose -p a-t up", "t"),
            "docker compose -p a-t up"
        );
        assert_eq!(
            rename_compose_projects("docker compose -p a up\nnpm test -p b", "t"),
            "docker compose -p a-t up\nnpm test -p b"
        );
        // after the subcommand, `-p` belongs to it
        assert_eq!(
            rename_compose_projects(
                "docker compose -f x.yml -p a exec db mkdir -p tmp; docker compose run -p 8080:80 web",
                "t"
            ),
            "docker compose -f x.yml -p a-t exec db mkdir -p tmp; docker compose run -p 8080:80 web"
        );
        assert_eq!(
            compose_projects("docker compose exec db psql -p 5432"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn compose_project_names() {
        assert_eq!(
            compose_projects(
                "docker compose -p a up -d; docker compose -p b down; docker compose -p a ps"
            ),
            vec!["a", "b"]
        );
        assert_eq!(
            compose_projects("TEST_DB_PORT=1 docker compose \\\n  -p app-int \\\n  -f a.yml up -d"),
            vec!["app-int"]
        );
        assert_eq!(
            compose_projects("docker compose --project-name=y up"),
            vec!["y"]
        );
        assert!(compose_projects("npm run test -p foo").is_empty());
        assert!(compose_projects("mkdir -p api && docker compose up -d").is_empty());
        assert_eq!(
            compose_projects("COMPOSE_PROJECT_NAME=z docker compose up"),
            vec!["z"]
        );
    }

    #[test]
    fn npm_installs_once_per_directory() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls");
        let fake = tmp.path().join("fake-npm");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$*\" >> {}\nmkdir -p node_modules\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let shim = tmp.path().join("npm");
        fs::write(&shim, NPM_SHIM.replace("__REAL__", &fake.to_string_lossy())).unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        let ws = tmp.path().join("ws");
        let proj = ws.join("proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("package-lock.json"), "{}").unwrap();
        let npm_in = |ws: &Path, marks: &str, args: &[&str]| {
            let st = Command::new(&shim)
                .args(args)
                .current_dir(ws.join("proj"))
                .env("GITHUB_WORKSPACE", ws)
                .env("LOCAL_CI_MARKS", tmp.path().join(marks))
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(st.success());
        };
        let npm = |args: &[&str]| npm_in(&ws, "marks", args);
        npm(&["ci", "--no-audit"]);
        npm(&["ci"]);
        npm(&["install"]);
        npm(&["install", "left-pad"]);
        npm(&["run", "test"]);
        fs::write(proj.join("package-lock.json"), "{\"changed\":1}").unwrap();
        npm(&["ci"]);
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(calls, "ci --no-audit\ninstall left-pad\nrun test\nci\n");

        // A unit's copy of the workspace, with the marks copied, skips too;
        // a copy without them does not.
        let copy = tmp.path().join("copy");
        fs::create_dir_all(copy.join("proj/node_modules")).unwrap();
        fs::copy(
            proj.join("package-lock.json"),
            copy.join("proj/package-lock.json"),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("copied-marks")).unwrap();
        for e in fs::read_dir(tmp.path().join("marks")).unwrap().flatten() {
            if e.file_type().unwrap().is_file() {
                fs::copy(
                    e.path(),
                    tmp.path().join("copied-marks").join(e.file_name()),
                )
                .unwrap();
            }
        }
        npm_in(&copy, "copied-marks", &["ci"]);
        npm_in(&copy, "other-marks", &["ci"]);
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(calls, "ci --no-audit\ninstall left-pad\nrun test\nci\nci\n");

        // Where node_modules links into setup's, a read leaves it so and a
        // write gets a copy of its own first.
        let shared = tmp.path().join("setup/node_modules");
        fs::create_dir_all(shared.join("x")).unwrap();
        fs::write(shared.join("x/index.js"), "x").unwrap();
        let linked = tmp.path().join("linked");
        let nm = linked.join("proj/node_modules");
        fs::create_dir_all(&nm).unwrap();
        fs::write(linked.join("proj/package-lock.json"), "{}").unwrap();
        std::os::unix::fs::symlink(shared.join("x"), nm.join("x")).unwrap();
        fs::write(
            nm.join(crate::git::SHARED),
            shared.to_string_lossy().as_bytes(),
        )
        .unwrap();
        npm_in(&linked, "copied-marks", &["run", "test"]);
        assert!(nm.join("x").is_symlink());
        npm_in(&linked, "copied-marks", &["rebuild"]);
        assert!(!nm.join("x").is_symlink() && !nm.join(crate::git::SHARED).exists());
        assert_eq!(fs::read_to_string(nm.join("x/index.js")).unwrap(), "x");
        assert!(shared.join("x/index.js").is_file());
        assert!(!linked.join("proj/.node_modules.local-ci").exists());
    }

    #[test]
    fn steps_that_write_node_modules() {
        assert!(writes_node_modules("npx prisma generate"));
        assert!(writes_node_modules("cd api && npm ci"));
        assert!(writes_node_modules("npm i -D left-pad"));
        assert!(writes_node_modules("yarn install --frozen-lockfile"));
        assert!(!writes_node_modules("npm run test"));
        assert!(!writes_node_modules("npm init -y"));
        assert!(!writes_node_modules("npx prisma migrate deploy"));
    }

    #[test]
    fn orphan_containers_are_told_by_their_owner() {
        assert_eq!(orphan_owner("local-ci-28989-214"), Some(28989));
        assert_eq!(orphan_owner("local-ci-28989"), None);
        assert_eq!(orphan_owner("local-ci-lc1-x"), None);
        assert_eq!(orphan_owner("postgres"), None);
        assert!(alive(std::process::id() as i32));
    }

    #[test]
    fn ansi_and_slugs() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m\r"), "red");
        assert_eq!(
            slug_of("Run integration tests (1/2)"),
            "run-integration-tests-1-2"
        );
    }

    #[test]
    fn a_task_result_survives_a_round_trip_through_json() {
        let step = |o: Outcome, note: Option<&str>| StepResult {
            label: "step".into(),
            outcome: o,
            exit: Some(1),
            secs: 1.5,
            log: Some(PathBuf::from("/l/log")),
            note: note.map(String::from),
        };
        let job = |display: &str, o: Outcome, cancelled: bool, time: Option<JobTime>| JobResult {
            workflow: "ci.yml".into(),
            job: "j".into(),
            display: display.into(),
            outcome: o,
            secs: 2.0,
            steps: vec![
                step(Outcome::Pass, Some("a note")),
                step(Outcome::Fail, None),
            ],
            cancelled,
            time,
        };
        let time = JobTime {
            work_secs: 1.0,
            wait_lock_secs: 2.0,
            wait_slot_secs: 3.0,
            wait_queue_secs: 4.0,
            wait_parallel_secs: 5.0,
        };
        let r = crate::task::TaskResult {
            task: "t1".into(),
            jobs: vec![
                job("a", Outcome::Pass, false, Some(time.clone())),
                job("b", Outcome::Fail, true, None),
                job("c", Outcome::Skip("why".into()), false, None),
                job("d", Outcome::Error("boom".into()), false, None),
            ],
            outcomes: vec![crate::task::JobOutcome {
                job: "j".into(),
                variant: Some(1),
                status: crate::task::JobStatus::Success,
                outputs: [("k".to_string(), "v".to_string())].into(),
                skipped: None,
                ancestor_failed: true,
            }],
            receipt_ignored: Some("another commit".into()),
            artifacts: vec!["bundle".into()],
        };
        let text = serde_json::to_string(&r).unwrap();
        let back: crate::task::TaskResult = serde_json::from_str(&text).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), text);
        assert_eq!(back.jobs[0].time, Some(time));
        assert_eq!(back.jobs[1].outcome, Outcome::Fail);
        assert!(back.jobs[1].cancelled && !back.jobs[0].cancelled);
        assert_eq!(back.jobs[2].outcome, Outcome::Skip("why".into()));
        assert_eq!(back.jobs[3].outcome, Outcome::Error("boom".into()));
        assert_eq!(back.jobs[0].steps[0].note.as_deref(), Some("a note"));
        assert_eq!(back.jobs[0].steps[1].note, None);
        assert_eq!(back.artifacts, ["bundle"]);
    }
}
