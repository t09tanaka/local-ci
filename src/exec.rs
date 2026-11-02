//! Running the plan: setup once, then every unit on its own thread.

use crate::expr::{self, Ctx, Status};
use crate::matrix;
use crate::plan::{self, Plan, Unit};
use crate::workflow::{Job, Service, Step, Workflow};
use crate::Repo;
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub static CANCEL: AtomicBool = AtomicBool::new(false);
static PGIDS: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static CONTAINERS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static COMPOSE_PROJECTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Stop everything that is running (Ctrl-C).
pub fn kill_all() {
    CANCEL.store(true, Ordering::SeqCst);
    for &pg in PGIDS.lock().unwrap().iter() {
        unsafe {
            libc::killpg(pg, libc::SIGTERM);
        }
    }
}

pub fn remove_containers() {
    let projects: Vec<String> = COMPOSE_PROJECTS.lock().unwrap().drain(..).collect();
    compose_down(&projects);
    let names: Vec<String> = CONTAINERS.lock().unwrap().drain(..).collect();
    if !names.is_empty() {
        let _ = Command::new("docker")
            .arg("rm")
            .arg("-f")
            .args(&names)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "result", content = "reason", rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    Fail,
    Skip(String),
    /// Could not be checked here (tool missing, docker down, unsupported construct, timeout).
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct JobResult {
    pub workflow: String,
    pub job: String,
    pub display: String,
    #[serde(flatten)]
    pub outcome: Outcome,
    pub secs: f64,
    pub steps: Vec<StepResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnitResult {
    pub workflow: String,
    pub jobs: Vec<JobResult>,
    pub waited_secs: f64,
    /// Time spent copying the worktree for this unit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_secs: Option<f64>,
}

pub struct Opts {
    pub parallel: usize,
    pub canirun: Option<PathBuf>,
    /// Gives jobs that bind fixed ports a block of their own (`--no-portool` turns it off).
    pub portool: Option<PathBuf>,
    pub job_timeout_min: f64,
    /// Leave the worktrees in place after the run (`--keep`).
    pub keep: bool,
    /// Where removed worktrees go before they are deleted in the background.
    pub trash: PathBuf,
}

pub struct Runner<'a> {
    pub repo: &'a Repo,
    pub plan: &'a Plan,
    pub wt: PathBuf,
    pub run_dir: PathBuf,
    pub tools: PathBuf,
    pub lock_file: PathBuf,
    pub slug: String,
    pub opts: Opts,
    pub base_path: String,
    seq: AtomicUsize,
    /// Copies that took a port block of their own.
    synced: Mutex<HashSet<PathBuf>>,
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

fn global_install(run: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?m)^\s*(sudo\s|brew (install|upgrade|tap)\b|apt(-get)?\s|yum\s|dnf\s|apk add\b|choco\s|snap install\b)").is_match(run)
}

/// Steps that would change something outside this machine with your credentials.
fn remote_effect(run: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"(?m)(^|[;&|(]\s*|\s)(git\s+push|gh\s+(api|release|pr|issue|workflow|run|repo|secret|variable|label|gist)\b|npm\s+publish|cargo\s+publish|dart\s+pub\s+publish|docker\s+(push|login)|aws\s|gcloud\s|kubectl\s|helm\s+(install|upgrade)|terraform\s+(apply|destroy)|serverless\s+deploy|sls\s+deploy|firebase\s+deploy|vercel\b|netlify\s+deploy|wrangler\s+(deploy|publish))",
    )
    .is_match(run)
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
        .filter(|l| !l.trim().is_empty() && !l.starts_with("canirun: "))
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
    let joined = script.replace("\\\n", " ");
    let mut out = Vec::new();
    for line in joined.lines() {
        out.extend(env.captures_iter(line).map(|c| c[1].to_string()));
        for marker in ["docker compose", "docker-compose"] {
            if let Some(at) = line.find(marker) {
                let rest = &line[at + marker.len()..];
                // stop at the next command on the line
                let rest = rest.split(['|', ';', '&']).next().unwrap_or(rest);
                out.extend(flag.captures_iter(rest).map(|c| c[1].to_string()));
            }
        }
    }
    out.dedup();
    out
}

/// `docker compose -p <name> down` for each project that still has containers.
fn compose_down(projects: &[String]) {
    for p in projects {
        let label = format!("label=com.docker.compose.project={p}");
        let has = Command::new("docker")
            .args(["ps", "-aq", "--filter", &label])
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);
        if has {
            let _ = Command::new("docker")
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
const NPM_SHIM: &str = r#"#!/bin/sh
# Written by local-ci for one run.
real="__REAL__"
sub="$1"
case "$sub" in
  ci|install|i) ;;
  *) exec "$real" "$@" ;;
esac
shift
for a in "$@"; do
  case "$a" in
    -*) ;;
    *) exec "$real" "$sub" "$@" ;;
  esac
done
dir=$(pwd -P)
ws=$(cd "${GITHUB_WORKSPACE:-/}" 2>/dev/null && pwd -P)
sum=$(cksum package-lock.json 2>/dev/null | cut -d' ' -f1)
# relative to the workspace, so a unit's copy of the worktree keeps setup's marks
key=$(printf '%s' "${dir#"$ws"}" | cksum | cut -d' ' -f1)
marks="$LOCAL_CI_MARKS"
mkdir -p "$marks"
until mkdir "$marks/$key.lock" 2>/dev/null; do sleep 1; done
trap 'rmdir "$marks/$key.lock" 2>/dev/null' EXIT INT TERM
if [ -f "$marks/$key-$sum" ] && [ -d node_modules ]; then
  echo "local-ci: node_modules in $dir was installed earlier in this run; skipping npm $sub" >&2
  exit 0
fi
"$real" "$sub" "$@"
rc=$?
if [ "$rc" -eq 0 ]; then : > "$marks/$key-$sum"; fi
exit "$rc"
"#;

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
            let a = Regex::new(&format!(r#"\b([A-Za-z_][A-Za-z0-9_]*=["']?){d}\b"#)).unwrap();
            s = a
                .replace_all(&s, |c: &regex::Captures| {
                    if port_key(c[1].trim_end_matches(['=', '"', '\''])) {
                        format!("{}{}", &c[1], m.actual)
                    } else {
                        c[0].to_string()
                    }
                })
                .into_owned();
        }
    }
    s
}

/// `PORT`, `DB_PORT`, `PORT_E2E`; not `EXPORT` or `REPORT`.
fn port_key(k: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?i)^([a-z0-9_]*_)?port(_[a-z0-9_]*)?$").is_match(k)
}

fn port_literal(v: &str) -> Option<u16> {
    v.trim().parse::<u16>().ok().filter(|p| *p >= 1024)
}

/// Ports a script fixes for itself: `TEST_DB_PORT=33306 docker compose …`.
fn assigned_ports(script: &str) -> Vec<u16> {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#"\b([A-Za-z_][A-Za-z0-9_]*)=["']?(\d{4,5})\b"#)
        .captures_iter(script)
        .filter(|c| port_key(&c[1]))
        .filter_map(|c| port_literal(&c[2]))
        .collect()
}

/// Give each fixed port a job sets itself a free one, the same in all its steps.
/// Free ports come from the ephemeral range, which portool's blocks stay below.
fn map_literals(ports: impl IntoIterator<Item = u16>, maps: &mut Vec<PortMap>) -> Result<()> {
    for p in ports {
        if maps.iter().any(|m| m.declared == Some(p)) {
            continue;
        }
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).context("no free port")?;
        maps.push(PortMap {
            declared: Some(p),
            container: p,
            actual: l.local_addr()?.port(),
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
        out.push_str(&flag.replace_all(&seg[..end], rename));
        rest = &seg[end..];
    }
    out
}

fn rewrite_env_value(key: &str, val: &str, maps: &[PortMap]) -> String {
    for m in maps {
        if let Some(d) = m.declared {
            if key.to_ascii_uppercase().contains("PORT") && val.trim() == d.to_string() {
                return m.actual.to_string();
            }
        }
    }
    rewrite_ports(val, maps)
}

impl<'a> Runner<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: &'a Repo,
        plan: &'a Plan,
        wt: PathBuf,
        run_dir: PathBuf,
        tools: PathBuf,
        lock_file: PathBuf,
        slug: String,
        opts: Opts,
    ) -> Self {
        let base_path = std::env::var("PATH").unwrap_or_default();
        Runner {
            repo,
            plan,
            wt,
            run_dir,
            tools,
            lock_file,
            slug,
            opts,
            base_path,
            seq: AtomicUsize::new(0),
            synced: Mutex::new(HashSet::new()),
        }
    }

    fn github_ctx(&self, ws: &Path, w: &Workflow, event: &str, job: &str) -> Value {
        let base = crate::git::branch_name(&self.repo.base);
        let head = self.repo.head.clone().unwrap_or_default();
        let pr = event == "pull_request";
        json!({
            "event_name": event,
            "event": if pr { json!({"pull_request": {"draft": false, "number": 0, "head": {"ref": head, "sha": self.repo.sha}, "base": {"ref": base}}, "action": "synchronize"}) } else { json!({"ref": format!("refs/heads/{head}")}) },
            "sha": self.repo.sha,
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
            ("GITHUB_SHA".into(), self.repo.sha.clone()),
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

    /// Run one shell step. Output goes to `log`; returns how it ended.
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
    ) -> Result<Exit> {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let file = self.tools.join(format!("scripts/step-{n}.sh"));
        fs::write(&file, script)?;
        let f = file.to_string_lossy().into_owned();
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
            Some(custom) if custom.contains("{0}") => shlex::split(&custom.replace("{0}", &f))
                .ok_or_else(|| anyhow!("bad shell `{custom}`"))?,
            Some(other) => bail!("unsupported shell `{other}`"),
        };
        if let (Some(c), Some(k)) = (&self.opts.canirun, key) {
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
            return Ok(Exit::Code(1));
        }
        let out = OpenOptions::new().create(true).append(true).open(log)?;
        let err = out.try_clone()?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("cannot start {}", argv[0]))?;
        let pid = child.id() as i32;
        PGIDS.lock().unwrap().push(pid);
        // With canirun the clock starts when the command gets its turn, not while it waits.
        let mut started: Option<Instant> = if self.opts.canirun.is_some() {
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
                            .any(|l| !l.starts_with("canirun: "))
                        {
                            started = Some(Instant::now());
                        }
                    }
                }
            }
            let over = matches!((timeout, started), (Some(t), Some(s)) if s.elapsed() > t);
            if (over || CANCEL.load(Ordering::SeqCst)) && term_at.is_none() {
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
        PGIDS.lock().unwrap().retain(|&p| p != pid);
        let _ = fs::remove_file(&file);
        if timed_out {
            return Ok(Exit::Timeout);
        }
        if CANCEL.load(Ordering::SeqCst) {
            return Ok(Exit::Cancelled);
        }
        Ok(Exit::Code(status.code().unwrap_or(
            128 + std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0),
        )))
    }

    fn log_path(&self, wf: &str, job: &str, n: usize, label: &str) -> PathBuf {
        let dir = self.run_dir.join(format!(
            "{}-{}",
            slug_of(
                crate::report::short(wf)
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
            let ok = Command::new("python3")
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

    pub fn run_setup(&self) -> Vec<StepResult> {
        let mut out = Vec::new();
        for (i, s) in self.plan.setup.iter().enumerate() {
            if !s.execute {
                continue;
            }
            let sel = &self.plan.selected[s.wf];
            let job = self.plan.job(s.wf, &s.job);
            let gh = self.github_ctx(&self.wt, &sel.wf, &sel.event, &job.id);
            let mut env = self.base_env(&gh, &[]);
            for (k, v) in sel
                .wf
                .env
                .iter()
                .chain(job.env.iter())
                .chain(s.key.env.iter())
            {
                if !v.contains("${{") {
                    env.push((k.clone(), v.clone()));
                }
            }
            let log = self.log_path("setup", "", i + 1, &format!("{}-{}", s.key.wd, s.label));
            let t0 = Instant::now();
            let cwd = self.wt.join(&s.key.wd);
            let res = self.shell(
                &cwd,
                &s.key.run,
                s.shell.as_deref(),
                &env,
                Some(Duration::from_secs(1800)),
                &log,
                None,
            );
            let label = format!(
                "{}$ {}",
                if s.key.wd.is_empty() { "." } else { &s.key.wd },
                s.label
            );
            let (outcome, exit) = match res {
                Ok(Exit::Code(0)) => (Outcome::Pass, Some(0)),
                Ok(Exit::Code(c)) => (Outcome::Fail, Some(c)),
                Ok(Exit::Timeout) => (Outcome::Error("timed out".into()), None),
                Ok(Exit::Cancelled) => (Outcome::Error("cancelled".into()), None),
                Err(e) => (Outcome::Error(format!("{e:#}")), None),
            };
            let stop = outcome != Outcome::Pass;
            out.push(StepResult {
                label,
                outcome,
                exit,
                secs: t0.elapsed().as_secs_f64(),
                log: Some(log),
                note: None,
            });
            if stop {
                break;
            }
        }
        out
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
                    let t0 = Instant::now();
                    let lock = if u.exclusive { self.lock().ok() } else { None };
                    sem.acquire();
                    let waited = t0.elapsed().as_secs_f64();
                    let r = if isolate && !CANCEL.load(Ordering::SeqCst) {
                        self.run_unit_in_copy(i + 1, u, waited)
                    } else {
                        self.run_unit(u, waited, &self.wt)
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

    fn lock(&self) -> Result<File> {
        use fs2::FileExt;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock_file)?;
        f.lock_exclusive()?;
        Ok(f)
    }

    /// Where the npm shim records the installs done in the workspace `ws`.
    fn marks(&self, ws: &Path) -> PathBuf {
        self.tools
            .join("installed")
            .join(ws.file_name().unwrap_or_default())
    }

    /// Run the unit in its own copy of the worktree, as each GitHub job gets its
    /// own checkout: a unit that edits tracked files for a while (a negative
    /// control, a formatter) must not change what the units beside it read.
    fn run_unit_in_copy(&self, n: usize, u: &Unit, waited: f64) -> UnitResult {
        // the pid stays last in the name: cleanup scripts read it from there
        let ws = self.wt.with_file_name(format!(
            "{}-u{n}-{}",
            &self.repo.sha[..7],
            std::process::id()
        ));
        let t0 = Instant::now();
        let copied = crate::git::clone_worktree(&self.repo.root, &self.wt, &ws, &self.repo.sha)
            .and_then(|_| {
                // installs done in setup are in the copy too
                let to = self.marks(&ws);
                fs::create_dir_all(&to)?;
                let from = fs::read_dir(self.marks(&self.wt));
                for e in from.into_iter().flatten().flatten() {
                    if e.file_type().is_ok_and(|t| t.is_file()) {
                        fs::copy(e.path(), to.join(e.file_name()))?;
                    }
                }
                Ok(())
            });
        let r = match copied {
            Ok(()) => {
                let copy_secs = t0.elapsed().as_secs_f64();
                UnitResult {
                    copy_secs: Some(copy_secs),
                    ..self.run_unit(u, waited, &ws)
                }
            }
            Err(e) => {
                let w = &self.plan.selected[u.wf].wf;
                let reason = format!("cannot copy the worktree: {e:#}");
                let jobs = u
                    .jobs
                    .iter()
                    .map(|id| JobResult {
                        workflow: w.file.clone(),
                        job: id.clone(),
                        display: self.plan.job(u.wf, id).name.clone().unwrap_or(id.clone()),
                        outcome: Outcome::Error(reason.clone()),
                        secs: 0.0,
                        steps: vec![],
                    })
                    .collect();
                UnitResult {
                    workflow: w.file.clone(),
                    jobs,
                    waited_secs: waited,
                    copy_secs: None,
                }
            }
        };
        if !self.opts.keep {
            self.release_ports(&ws);
            crate::git::remove_worktree(&self.repo.root, &ws, &self.opts.trash);
        }
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
            crate::portool::release(bin, ws);
        }
    }

    fn run_unit(&self, u: &Unit, waited: f64, ws: &Path) -> UnitResult {
        let sel = &self.plan.selected[u.wf];
        let w = &sel.wf;
        let mut needs_ctx = Map::new();
        let mut jobs = Vec::new();
        for id in &u.jobs {
            let job = self.plan.job(u.wf, id);
            let mut job_results = self.run_job(ws, w, &sel.event, job, &needs_ctx);
            let failed = job_results
                .iter()
                .any(|r| r.0.outcome == Outcome::Fail || matches!(r.0.outcome, Outcome::Error(_)));
            let all_skipped = !job_results.is_empty()
                && job_results
                    .iter()
                    .all(|r| matches!(r.0.outcome, Outcome::Skip(_)));
            let result = if failed {
                "failure"
            } else if all_skipped {
                "skipped"
            } else {
                "success"
            };
            let mut outputs = Map::new();
            for (_, o) in &job_results {
                for (k, v) in o {
                    outputs.insert(k.clone(), v.clone());
                }
            }
            needs_ctx.insert(id.clone(), json!({"result": result, "outputs": outputs}));
            jobs.extend(job_results.drain(..).map(|r| r.0));
        }
        UnitResult {
            workflow: w.file.clone(),
            jobs,
            waited_secs: waited,
            copy_secs: None,
        }
    }

    /// One job, all its matrix combinations. Returns each combination's result and outputs.
    fn run_job(
        &self,
        ws: &Path,
        w: &Workflow,
        event: &str,
        job: &Job,
        needs: &Map<String, Value>,
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
                },
                Map::new(),
            )
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
                },
                Map::new(),
            )
        };

        let needs_of_job: Map<String, Value> = job
            .needs
            .iter()
            .filter_map(|n| needs.get(n).map(|v| (n.clone(), v.clone())))
            .collect();
        let needs_status = if needs_of_job.values().any(|v| v["result"] != "success") {
            Status::Failure
        } else {
            Status::Success
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

        let mut out = Vec::new();
        let mut combo_failed = false;
        for combo in combos {
            let mut cm = ctx_map.clone();
            cm.insert("matrix".into(), Value::Object(combo.clone()));
            let display = {
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
            if CANCEL.load(Ordering::SeqCst) {
                out.push(skip(display, "cancelled".into()));
                continue;
            }
            if combo_failed && job.fail_fast {
                out.push(skip(display, "fail-fast".into()));
                continue;
            }
            let run_it = {
                let ctx = Ctx::new(&cm, needs_status);
                expr::condition(job.cond.as_deref(), &ctx)
            };
            match run_it {
                Ok(true) => {}
                Ok(false) => {
                    out.push(skip(
                        display,
                        if needs_status == Status::Failure {
                            "needs failed".into()
                        } else {
                            "if: false".into()
                        },
                    ));
                    continue;
                }
                Err(e) => {
                    out.push(error(display, format!("if: {e:#}")));
                    continue;
                }
            }
            let r = self.run_combo(ws, w, job, cm, display);
            if r.0.outcome == Outcome::Fail {
                combo_failed = true;
            }
            out.push(r);
        }
        out
    }

    fn eval_env(
        pairs: &[(String, String)],
        cm: &Map<String, Value>,
        into: &mut Map<String, Value>,
        maps: &[PortMap],
    ) -> Result<bool> {
        let mut secrets = false;
        for (k, v) in pairs {
            let mut ctx_map = cm.clone();
            ctx_map.insert("env".into(), Value::Object(into.clone()));
            let ctx = Ctx::new(&ctx_map, Status::Success);
            let val = expr::interpolate(v, &ctx)?;
            secrets |= ctx.used_secrets.get();
            into.insert(k.clone(), Value::String(rewrite_env_value(k, &val, maps)));
        }
        Ok(secrets)
    }

    fn run_combo(
        &self,
        ws: &Path,
        w: &Workflow,
        job: &Job,
        mut cm: Map<String, Value>,
        display: String,
    ) -> (JobResult, Map<String, Value>) {
        let t0 = Instant::now();
        let mut steps: Vec<StepResult> = Vec::new();
        let mut names = Vec::new();
        let finish = |outcome: Outcome, steps: Vec<StepResult>, t0: Instant| JobResult {
            workflow: w.file.clone(),
            job: job.id.clone(),
            display: display.clone(),
            outcome,
            secs: t0.elapsed().as_secs_f64(),
            steps,
        };

        // services
        let mut maps = Vec::new();
        let mut services_ctx = Map::new();
        if !job.services.is_empty() {
            match self.start_services(job, &cm, &mut names) {
                Ok((m, sc)) => {
                    maps = m;
                    services_ctx = sc;
                }
                Err(e) => {
                    self.stop_services(&names);
                    return (
                        finish(Outcome::Error(format!("services: {e:#}")), steps, t0),
                        Map::new(),
                    );
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
            Some(bin) if exclusive && ws.join(".portool.toml").is_file() => {
                match crate::portool::sync(bin, ws) {
                    Ok(v) => {
                        self.synced.lock().unwrap().insert(ws.to_path_buf());
                        v
                    }
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
        if let Err(e) = Self::eval_env(&w.env, &cm, &mut env_ctx, &maps)
            .and_then(|_| Self::eval_env(&job.env, &cm, &mut env_ctx, &maps))
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
        COMPOSE_PROJECTS
            .lock()
            .unwrap()
            .push(compose_default.clone());
        let mut errored: Option<String> = None;

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
            let status = if CANCEL.load(Ordering::SeqCst) {
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
            let go = expr::condition(s.cond.as_deref(), &Ctx::new(&scm, status));
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
                    push(
                        &mut steps,
                        Outcome::Error(format!("if: {e:#}")),
                        None,
                        None,
                        None,
                    );
                    errored.get_or_insert(format!("{label}: if: {e:#}"));
                    continue;
                }
            }

            if let Some(uses) = &s.uses {
                let (outcome, note) = self.check_action(uses, s, &scm);
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
            if let Some(k) = plan::step_key(w, job, s) {
                if self.plan.is_setup(&k) {
                    push(&mut steps, Outcome::Skip("setup".into()), None, None, None);
                    record(&mut steps_ctx, &s.id, "success", "success", Map::new());
                    continue;
                }
            }
            if remote_effect(run) {
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
            if global_install(run) {
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
            let ctx = Ctx::new(&scm, status);
            let script = expr::interpolate(run, &ctx);
            let mut env_now = step_env.clone();
            let env_res = Self::eval_env(&s.env, &scm, &mut env_now, &maps);
            let secrets = ctx.used_secrets.get() || matches!(env_res, Ok(true));
            let (script, mut env_now) = match (script, env_res) {
                (Ok(sc), Ok(_)) => (sc, env_now),
                (Err(e), _) | (_, Err(e)) => {
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
                    let r = rewrite_env_value(k, &s, &maps);
                    if r != s {
                        *v = Value::String(r);
                    }
                }
                rename_compose_projects(&rewrite_ports(&script, &maps), &tag)
            } else {
                rewrite_ports(&script, &maps)
            };
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
            let fc = |name: &str| files_dir.join(name);
            for f in ["output", "env", "path", "summary"] {
                let _ = File::create(fc(f));
            }
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
            for p in compose_projects(&script) {
                if !projects.contains(&p) {
                    COMPOSE_PROJECTS.lock().unwrap().push(p.clone());
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
                crate::report::short(&w.file),
                job.id,
                label
            );
            let res = self.shell(
                &ws.join(&wd),
                &script,
                s.shell
                    .as_deref()
                    .or(job.shell.as_deref())
                    .or(w.shell.as_deref()),
                &env,
                Some(timeout),
                &log,
                Some(key),
            );
            let read = |f: &str| fs::read_to_string(fc(f)).unwrap_or_default();
            let mut outputs = Map::new();
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
                    errored.get_or_insert("cancelled".into());
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
        compose_down(&projects);
        COMPOSE_PROJECTS
            .lock()
            .unwrap()
            .retain(|p| !projects.contains(p));

        // job outputs
        let mut outputs = Map::new();
        let mut ocm = cm.clone();
        ocm.insert("steps".into(), Value::Object(steps_ctx));
        let octx = Ctx::new(&ocm, Status::Success);
        for (k, v) in &job.outputs {
            if let Ok(val) = expr::interpolate(v, &octx) {
                outputs.insert(k.clone(), Value::String(val));
            }
        }
        let outcome = if failed {
            Outcome::Fail
        } else if let Some(e) = errored {
            Outcome::Error(e)
        } else {
            Outcome::Pass
        };
        (finish(outcome, steps, t0), outputs)
    }

    /// `uses:` steps: version checks for setup actions, skips for the rest.
    fn check_action(
        &self,
        uses: &str,
        s: &Step,
        cm: &Map<String, Value>,
    ) -> (Outcome, Option<String>) {
        let name = uses.split('@').next().unwrap_or(uses).to_ascii_lowercase();
        let ctx = Ctx::new(cm, Status::Success);
        let with = |k: &str| {
            s.with
                .iter()
                .find(|(n, _)| n == k)
                .and_then(|(_, v)| expr::interpolate(v, &ctx).ok())
                .filter(|v| !v.is_empty())
        };
        let tool = |bin: &str, args: &[&str]| -> Option<String> {
            let out = Command::new(bin).args(args).output().ok()?;
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

    fn docker_ok() -> Result<(), String> {
        static OK: OnceLock<Result<(), String>> = OnceLock::new();
        OK.get_or_init(|| {
            match Command::new("docker")
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

    fn docker(args: &[&str], timeout: Duration) -> Result<String> {
        let mut child = Command::new("docker")
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
            if t0.elapsed() > timeout || CANCEL.load(Ordering::SeqCst) {
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
        job: &Job,
        cm: &Map<String, Value>,
        names: &mut Vec<String>,
    ) -> Result<(Vec<PortMap>, Map<String, Value>)> {
        Self::docker_ok().map_err(|e| anyhow!("docker is not available ({e})"))?;
        let ctx = Ctx::new(cm, Status::Success);
        let mut maps = Vec::new();
        let mut sctx = Map::new();
        for svc in &job.services {
            let image = expr::interpolate(&svc.image, &ctx)?;
            if Self::docker(&["image", "inspect", &image], Duration::from_secs(30)).is_err() {
                Self::docker(&["pull", "--quiet", &image], Duration::from_secs(600))
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
            for (_, c) in &svc.ports {
                args.push("-p".into());
                args.push(format!("127.0.0.1::{c}"));
            }
            let opts = expr::interpolate(&svc.options, &ctx)?;
            args.extend(
                shlex::split(&opts).ok_or_else(|| anyhow!("cannot parse options `{opts}`"))?,
            );
            args.push(image.clone());
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            Self::docker(&refs, Duration::from_secs(120))?;
            CONTAINERS.lock().unwrap().push(name.clone());
            names.push(name.clone());
            let mut ports = Map::new();
            for (d, c) in &svc.ports {
                let line = Self::docker(
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
            if CANCEL.load(Ordering::SeqCst) {
                bail!("cancelled");
            }
            let state = Self::docker(&["inspect", "-f", "{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}", name], Duration::from_secs(30))?;
            let mut parts = state.split_whitespace();
            let (run, health) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            if run == "exited" || run == "dead" {
                let logs = Command::new("docker")
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
        let _ = Command::new("docker")
            .arg("rm")
            .arg("-f")
            .args(names)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let set: HashSet<&String> = names.iter().collect();
        CONTAINERS.lock().unwrap().retain(|n| !set.contains(n));
    }
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
mod tests {
    use super::*;

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
    fn fixed_ports_a_job_sets() {
        assert!(port_key("PORT") && port_key("TEST_DB_PORT") && port_key("port_e2e"));
        assert!(!port_key("EXPORT") && !port_key("REPORT") && !port_key("PORTS"));
        assert_eq!(
            assigned_ports("TEST_DB_PORT=15432 docker compose up\nEXPORT=4000 PORT=80 X=5000"),
            vec![15432]
        );
        let mut maps = vec![];
        map_literals([15432, 15432, 15433], &mut maps).unwrap();
        assert_eq!(maps.len(), 2);
        assert!(maps
            .iter()
            .all(|m| m.actual != 0 && m.declared != Some(m.actual)));
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
    }

    #[test]
    fn compose_project_names() {
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
    }

    #[test]
    fn remote_effects() {
        assert!(remote_effect("git push origin HEAD"));
        assert!(remote_effect(
            "npm run build && npm publish --access public"
        ));
        assert!(remote_effect("gh release upload v1 a.tgz"));
        assert!(remote_effect("aws s3 sync dist s3://bucket"));
        assert!(!remote_effect("git diff --exit-code"));
        assert!(!remote_effect("npm run lint"));
        assert!(!remote_effect("echo pushing"));
    }

    #[test]
    fn system_installs() {
        assert!(global_install("sudo apt-get install -y jq"));
        assert!(global_install("echo hi\n  brew install x"));
        assert!(!global_install("npm ci"));
        assert!(!global_install("echo sudo"));
    }

    #[test]
    fn ansi_and_slugs() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m\r"), "red");
        assert_eq!(
            slug_of("Run integration tests (1/2)"),
            "run-integration-tests-1-2"
        );
    }
}
