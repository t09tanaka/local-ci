//! Turn the triggered workflows into execution units, and work out which
//! leading installs of their jobs count as setup.
//!
//! A unit is a set of jobs that runs in order on one thread; units run in
//! parallel. Jobs land in one unit when they are linked by `needs:`, or when
//! they would write the same build output. Units that start databases or
//! containers (or look like integration / e2e tests) are "exclusive": they take
//! a per-repository lock, so they never overlap each other, in this run or in
//! another local-ci run of the same repository.
//!
//! Every job of a workflow starts with the same install steps (`npm ci` ...),
//! because each CI job gets a fresh VM. Those leading steps are the setup of a
//! task (see [`crate::setup`]): a local run does each of them once and the
//! tasks skip what the setup did, and a task placed on its own host does its
//! own.

use crate::filter::{decide, Decision, Event};
use crate::workflow::{Job, Step, Workflow};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Selected {
    pub wf: Workflow,
    pub event: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Unit {
    pub wf: usize,
    /// Job ids in dependency order.
    pub jobs: Vec<String>,
    pub exclusive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StepKey {
    pub wd: String,
    pub run: String,
    /// The shell the step runs with (step, else job, else workflow); `None`
    /// is the default, bash (also when it is written `bash`).
    pub shell: Option<String>,
    /// The workflow, job and step `env:` put together, sorted by name, without
    /// the values that are expressions and the names in `ignore-env`.
    pub env: Vec<(String, String)>,
    /// The `env:` entries whose value is an expression evaluated at run time.
    pub open_env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub selected: Vec<Selected>,
    pub skipped: Vec<(String, String)>,
    /// The files of those in `skipped` that were selected but cannot be run
    /// (a dependency cycle, a `needs` of nothing, a matrix with keys that
    /// print alike), as opposed to the ones the changes do not trigger.
    pub unrunnable: Vec<String>,
    pub units: Vec<Unit>,
    /// Names of `env:` entries that do not decide what an install does
    /// (`ignore-env` of `.local-ci.toml`); they are left out of step keys.
    pub ignore_env: Vec<String>,
}

impl Plan {
    pub fn job(&self, wf: usize, id: &str) -> &Job {
        self.selected[wf]
            .wf
            .jobs
            .iter()
            .find(|j| j.id == id)
            .expect("job in plan")
    }
}

/// Strip `./` and trailing `/`; the repository root is "".
pub fn norm_dir(d: &str) -> String {
    let mut s = d.trim();
    while let Some(r) = s.strip_prefix("./") {
        s = r;
    }
    let s = s.trim_end_matches('/');
    if s == "." {
        String::new()
    } else {
        s.to_string()
    }
}

pub fn step_dir(w: &Workflow, j: &Job, s: &Step) -> String {
    let d = s
        .working_directory
        .as_ref()
        .or(j.working_directory.as_ref())
        .or(w.working_directory.as_ref());
    norm_dir(d.map(String::as_str).unwrap_or(""))
}

/// The shell `s` runs with: its own, else the job's, else the workflow's.
pub fn resolved_shell(w: &Workflow, j: &Job, s: &Step) -> Option<String> {
    s.shell
        .clone()
        .or_else(|| j.shell.clone())
        .or_else(|| w.shell.clone())
}

/// Whether [`crate::exec`] can run a script with `shell` (`None` is bash).
pub(crate) fn shell_supported(shell: Option<&str>) -> bool {
    match shell.map(str::trim) {
        None | Some("bash") | Some("sh") | Some("python") => true,
        Some(custom) => custom.contains("{0}"),
    }
}

type EnvPairs = Vec<(String, String)>;

/// The `env:` of workflow, job and step put together (the step wins), as
/// (values known now, values that are expressions), each sorted by name.
pub fn effective_env(w: &Workflow, j: &Job, s: &Step) -> (EnvPairs, EnvPairs) {
    let mut all: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for (k, v) in w.env.iter().chain(j.env.iter()).chain(s.env.iter()) {
        all.insert(k, v);
    }
    let (mut env, mut open) = (vec![], vec![]);
    for (k, v) in all {
        if v.contains("${{") {
            open.push((k.to_string(), v.to_string()));
        } else {
            env.push((k.to_string(), v.to_string()));
        }
    }
    (env, open)
}

/// The key of a `run:` step. `ignore` names `env:` entries that are not part of
/// it. A shell written as `bash` is the default shell, so it is `None` here:
/// a job that says `shell: bash` and one that says nothing run the same way.
pub fn step_key(w: &Workflow, j: &Job, s: &Step, ignore: &[String]) -> Option<StepKey> {
    let (mut env, mut open_env) = effective_env(w, j, s);
    env.retain(|(k, _)| !ignore.contains(k));
    open_env.retain(|(k, _)| !ignore.contains(k));
    Some(StepKey {
        wd: step_dir(w, j, s),
        run: s.run.as_ref()?.trim().to_string(),
        shell: resolved_shell(w, j, s).filter(|sh| sh.trim() != "bash"),
        env,
        open_env,
    })
}

/// `uses:` steps that need nothing done locally (or only a version check).
pub fn is_plumbing(uses: &str) -> bool {
    let name = uses.split('@').next().unwrap_or(uses).to_ascii_lowercase();
    name == "actions/checkout"
        || name.starts_with("actions/cache")
        || name.ends_with("/rust-cache")
        || name.contains("/setup-")
        || name.ends_with("flutter-action")
        || name.starts_with("dtolnay/rust-toolchain")
}

fn installer() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^([A-Za-z_][A-Za-z0-9_]*=\S*\s+)*(npm (ci|install|i)|yarn( install)?|pnpm (install|i)|pip3? install|python3? -m pip install|bundle install|flutter pub get|dart pub get|go mod download|cargo fetch|composer install|npx prisma generate)(\s|$)",
        )
        .unwrap()
    })
}

fn docker_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bdocker(-compose)?\s").unwrap())
}

fn heavy_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)(integration|e2e|test:int|int-test|test-int|playwright|cypress)").unwrap()
    })
}

fn build_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(next build|vite build|tsc (-b|--build)|webpack|cargo build|flutter build|npm run build\b|nest build)").unwrap())
}

fn mentions_service_port(j: &Job, text: &str) -> bool {
    j.services
        .iter()
        .flat_map(|s| s.ports.iter())
        .any(|(h, _)| h.is_some_and(|h| text.contains(&format!(":{h}"))))
}

/// Uses databases / containers, or looks like an integration or e2e test.
pub fn is_exclusive(j: &Job) -> bool {
    !j.services.is_empty()
        || heavy_re().is_match(&j.id)
        || j.name.as_deref().is_some_and(|n| heavy_re().is_match(n))
        || j.steps
            .iter()
            .filter_map(|s| s.run.as_deref())
            .any(|r| docker_re().is_match(r) || server_re().is_match(r))
}

/// Scripts that start a browser test server or an e2e / integration suite,
/// which bind ports even when the job's name does not say so.
fn server_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)\b(playwright|cypress)\b|\btest:(e2e|int|integration)\b").unwrap()
    })
}

/// `run` followed by the `package.json` scripts it starts (`npm test`,
/// `npm run x`, `yarn x`, `pnpm x`, with their `pre` and `post` scripts) and
/// the ones those start in turn: what a step runs, as far as the text shows.
/// `pkg` is the `package.json` of the step's directory.
pub fn with_scripts(run: &str, pkg: Option<&str>) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    let re = R.get_or_init(|| {
        Regex::new(r"\b(npm|pnpm|yarn)\s+(run(?:-script)?\s+)?([A-Za-z0-9:_.\-]+)").unwrap()
    });
    let scripts: serde_json::Map<String, serde_json::Value> = pkg
        .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
        .and_then(|v| v.get("scripts").and_then(|s| s.as_object()).cloned())
        .unwrap_or_default();
    let mut out = run.to_string();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut todo = vec![run.to_string()];
    while let Some(text) = todo.pop() {
        for c in re.captures_iter(&text) {
            let (tool, run_kw, name) = (&c[1], c.get(2).is_some(), &c[3]);
            let name = match (tool, run_kw, name) {
                ("npm", false, "t" | "tst") => "test",
                ("npm", false, n @ ("test" | "start" | "stop" | "restart")) => n,
                ("npm", false, _) => continue,
                (_, _, n) => n,
            };
            for n in [
                format!("pre{name}"),
                name.to_string(),
                format!("post{name}"),
            ] {
                if let Some(body) = scripts.get(&n).and_then(|b| b.as_str()) {
                    if seen.insert(n) {
                        out.push('\n');
                        out.push_str(body);
                        todo.push(body.to_string());
                    }
                }
            }
        }
    }
    out
}

pub fn is_installer(run: &str) -> bool {
    !run.contains('\n') && installer().is_match(run.trim())
}

/// `npm ci --no-audit` and `npm ci` install the same thing; run only the first one per directory.
pub(crate) fn install_family(run: &str) -> Option<String> {
    if !is_installer(run) {
        return None;
    }
    let words: Vec<&str> = run
        .split_whitespace()
        .filter(|w| !w.contains('=') && !w.starts_with('-'))
        .take(2)
        .collect();
    Some(words.join(" "))
}

fn write_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?:[^0-9&>]>{1,2}|\btee(?:\s+-a)?|\bcp\s+\S+)\s*["']?([A-Za-z0-9_./][A-Za-z0-9_./-]*)"#).unwrap())
}

/// Files a run script writes (`> .env.test`, `>> x`, `tee x`, `cp a x`), relative to the repository.
pub fn written_files(wd: &str, run: &str) -> BTreeSet<String> {
    write_re()
        .captures_iter(&format!(" {run}"))
        .map(|c| c[1].to_string())
        .filter(|f| !f.starts_with("/dev/"))
        .map(|f| {
            if f.starts_with('/') || wd.is_empty() {
                norm_dir(&f)
            } else {
                format!("{wd}/{}", norm_dir(&f))
            }
        })
        .collect()
}

/// What a job writes that another job of the workflow might also write:
/// build output directories (as `build:<dir>`) and files its scripts create.
pub(crate) fn writes(w: &Workflow, j: &Job) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for s in &j.steps {
        let Some(r) = s.run.as_deref() else { continue };
        let wd = step_dir(w, j, s);
        if build_re().is_match(r) {
            out.insert(format!("build:{wd}"));
        }
        out.extend(written_files(&wd, r));
    }
    out
}

/// Why a step that looks like preparation stays where it is in its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StayReason {
    /// Its script or env holds an expression (`${{ ... }}`).
    Expression,
    /// The env of its job or workflow depends on `needs`, `matrix` or `steps`.
    RuntimeEnv,
    /// It has an `if:`.
    Conditional,
    /// It may need the job's service containers.
    ServiceDependent,
    /// It writes `GITHUB_ENV`, `GITHUB_PATH`, `GITHUB_OUTPUT` or `GITHUB_STATE`,
    /// which cannot be captured and replayed.
    EnvFileEffect,
    /// It makes an environment tied to an absolute path (a Python venv).
    AbsolutePath,
    /// It runs with a shell local-ci cannot start (`pwsh`, ...).
    UnsupportedShell,
    /// Another job installs the same thing in the same directory with another
    /// shell or env; one workspace cannot hold both, so each job installs for
    /// itself.
    ConflictingInstall,
}

impl StayReason {
    pub fn describe(self) -> &'static str {
        match self {
            StayReason::Expression => "uses an expression evaluated at run time",
            StayReason::RuntimeEnv => "its env depends on needs, matrix or step outputs",
            StayReason::Conditional => "has an `if:`",
            StayReason::ServiceDependent => "may need the job's services",
            StayReason::EnvFileEffect => "writes GITHUB_ENV, GITHUB_PATH or a step output",
            StayReason::AbsolutePath => "builds an environment tied to an absolute path",
            StayReason::UnsupportedShell => "runs with a shell local-ci does not support",
            StayReason::ConflictingInstall => {
                "another job installs the same thing here with another shell or env"
            }
        }
    }
}

fn env_file_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"GITHUB_(ENV|PATH|OUTPUT|STATE)\b").unwrap())
}

fn absolute_env_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // `venv` and friends only as the command being run (or `python -m
        // venv`), so `npm run lint:venv` is not taken for one.
        Regex::new(
            r"(?m)(?:^|[;&|(])\s*(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*(?:virtualenv|pipenv|conda)(?:\s|$)|\bpython3?\s+-m\s+(?:venv|virtualenv)\b|\bpip3?\s+install\b|\bpython3?\s+-m\s+pip\b",
        )
        .unwrap()
    })
}

fn runtime_expr(v: &str) -> bool {
    v.contains("${{")
        && ["needs.", "matrix.", "steps.", "strategy."]
            .iter()
            .any(|p| v.contains(p))
}

/// What keeps a `run:` step from being setup, if anything.
pub(crate) fn stay_reason(w: &Workflow, j: &Job, s: &Step) -> Option<StayReason> {
    let run = s.run.as_deref()?;
    if run.contains("${{") || s.env.iter().any(|(_, v)| v.contains("${{")) {
        return Some(StayReason::Expression);
    }
    if w.env
        .iter()
        .chain(j.env.iter())
        .any(|(_, v)| runtime_expr(v))
    {
        return Some(StayReason::RuntimeEnv);
    }
    if s.cond.is_some() {
        return Some(StayReason::Conditional);
    }
    if env_file_re().is_match(run) {
        return Some(StayReason::EnvFileEffect);
    }
    if absolute_env_re().is_match(run) {
        return Some(StayReason::AbsolutePath);
    }
    if !shell_supported(resolved_shell(w, j, s).as_deref()) {
        return Some(StayReason::UnsupportedShell);
    }
    if !j.services.is_empty() && (!is_installer(run) || mentions_service_port(j, run)) {
        // anything but an install may need the job's database
        return Some(StayReason::ServiceDependent);
    }
    None
}

/// A leading step of a job that could be setup.
pub(crate) struct PrefixStep<'a> {
    /// Its place in the steps of the job.
    pub index: usize,
    pub key: StepKey,
    pub step: &'a Step,
}

/// The leading steps of a job that could be setup, and the step that ended
/// them with the reason, when there is one. Plumbing `uses:` are passed over;
/// the first step that is neither plumbing nor possible setup ends it.
pub(crate) struct Prefix<'a> {
    pub steps: Vec<PrefixStep<'a>>,
    pub stopped_by: Option<(usize, StayReason)>,
}

pub(crate) fn prefix_of<'a>(w: &Workflow, j: &'a Job, ignore: &[String]) -> Prefix<'a> {
    let mut out = Prefix {
        steps: Vec::new(),
        stopped_by: None,
    };
    for (index, s) in j.steps.iter().enumerate() {
        if let Some(u) = &s.uses {
            if is_plumbing(u) {
                continue;
            }
            break;
        }
        if s.run.is_none() {
            break;
        }
        if let Some(r) = stay_reason(w, j, s) {
            out.stopped_by = Some((index, r));
            break;
        }
        out.steps.push(PrefixStep {
            index,
            key: step_key(w, j, s, ignore).unwrap(),
            step: s,
        });
    }
    out
}

fn topo(jobs: &[&Job]) -> Vec<String> {
    let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
    let mut done: Vec<String> = Vec::new();
    while done.len() < jobs.len() {
        let before = done.len();
        for j in jobs {
            if done.contains(&j.id) {
                continue;
            }
            if j.needs
                .iter()
                .all(|n| done.contains(n) || !ids.contains(&n.as_str()))
            {
                done.push(j.id.clone());
            }
        }
        if done.len() == before {
            // a cycle: keep file order for the rest
            for j in jobs {
                if !done.contains(&j.id) {
                    done.push(j.id.clone());
                }
            }
        }
    }
    done
}

pub fn build(workflows: Vec<Workflow>, ev: &Event, only: &[String]) -> Plan {
    let mut plan = Plan::default();
    for w in workflows {
        if !only.is_empty()
            && !only
                .iter()
                .any(|o| w.file.ends_with(o.as_str()) || &w.name == o)
        {
            continue;
        }
        match decide(&w, ev) {
            Decision::Run { event } => plan.selected.push(Selected { wf: w, event }),
            Decision::Skip { reason } => plan.skipped.push((w.file.clone(), reason)),
        }
    }

    for (wi, sel) in plan.selected.iter().enumerate() {
        let w = &sel.wf;
        // union-find over job indexes
        let n = w.jobs.len();
        let mut parent: Vec<usize> = (0..n).collect();
        fn find(p: &mut [usize], i: usize) -> usize {
            let mut r = i;
            while p[r] != r {
                r = p[r];
            }
            p[i] = r;
            r
        }
        let idx: HashMap<&str, usize> = w
            .jobs
            .iter()
            .enumerate()
            .map(|(i, j)| (j.id.as_str(), i))
            .collect();
        for (i, j) in w.jobs.iter().enumerate() {
            for need in &j.needs {
                if let Some(&k) = idx.get(need.as_str()) {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, k));
                    parent[a] = b;
                }
            }
        }
        // jobs that write the same file or build output run one after another
        let outs: Vec<BTreeSet<String>> = w.jobs.iter().map(|j| writes(w, j)).collect();
        for a in 0..n {
            for b in (a + 1)..n {
                if !outs[a].is_disjoint(&outs[b]) {
                    let (x, y) = (find(&mut parent, a), find(&mut parent, b));
                    parent[x] = y;
                }
            }
        }
        let mut groups: Vec<(usize, Vec<&Job>)> = Vec::new();
        for (i, job) in w.jobs.iter().enumerate() {
            let r = find(&mut parent, i);
            match groups.iter_mut().find(|(root, _)| *root == r) {
                Some((_, g)) => g.push(job),
                None => groups.push((r, vec![job])),
            }
        }
        for (_, g) in groups {
            let exclusive = g.iter().any(|j| is_exclusive(j));
            plan.units.push(Unit {
                wf: wi,
                jobs: topo(&g),
                exclusive,
            });
        }
    }

    plan
}

/// Which leading steps of the jobs of a plan count as setup.
#[derive(Debug, Clone, Default)]
pub(crate) struct Hoist {
    pub keys: BTreeSet<StepKey>,
    /// Installs that would have been setup but that jobs run with different
    /// shells or env in one directory; they stay in their jobs.
    pub conflicting: BTreeSet<StepKey>,
}

#[cfg(test)]
pub(crate) fn hoisted_keys(plan: &Plan) -> BTreeSet<StepKey> {
    hoist(plan).keys
}

/// The steps that count as setup: from the common head of the jobs, those
/// that are installs, or that several jobs repeat word for word, as long as
/// everything before them in each of those jobs is also setup. An install that
/// two jobs run in one directory under different shells or env is not setup
/// (one workspace cannot hold both results). Worked out from the plan alone, so
/// it is the same wherever it is asked.
pub(crate) fn hoist(plan: &Plan) -> Hoist {
    let mut prefixes: Vec<Vec<StepKey>> = Vec::new();
    for u in &plan.units {
        let w = &plan.selected[u.wf].wf;
        for id in &u.jobs {
            let j = w.jobs.iter().find(|j| &j.id == id).unwrap();
            if j.container || j.reusable.is_some() {
                continue;
            }
            prefixes.push(
                prefix_of(w, j, &plan.ignore_env)
                    .steps
                    .into_iter()
                    .map(|p| p.key)
                    .collect(),
            );
        }
    }
    let mut count: HashMap<&StepKey, usize> = HashMap::new();
    for p in &prefixes {
        let uniq: BTreeSet<&StepKey> = p.iter().collect();
        for k in uniq {
            *count.entry(k).or_default() += 1;
        }
    }
    let mut hoisted: BTreeSet<StepKey> = count
        .iter()
        .filter(|(k, c)| **c >= 2 || is_installer(&k.run))
        .map(|(k, _)| (*k).clone())
        .collect();
    // one install of a kind per directory: two ways of doing it conflict
    type Way<'a> = (&'a Option<String>, &'a EnvPairs, &'a EnvPairs);
    let mut ways: HashMap<(&str, String), BTreeSet<Way>> = HashMap::new();
    for k in &hoisted {
        if let Some(family) = install_family(&k.run) {
            ways.entry((k.wd.as_str(), family)).or_default().insert((
                &k.shell,
                &k.env,
                &k.open_env,
            ));
        }
    }
    let conflicting: BTreeSet<StepKey> = hoisted
        .iter()
        .filter(|k| {
            install_family(&k.run)
                .and_then(|f| ways.get(&(k.wd.as_str(), f)))
                .is_some_and(|w| w.len() > 1)
        })
        .cloned()
        .collect();
    for k in &conflicting {
        hoisted.remove(k);
    }
    loop {
        let mut drop = BTreeSet::new();
        for p in &prefixes {
            let mut clean = true;
            for k in p {
                if !hoisted.contains(k) {
                    clean = false;
                } else if !clean {
                    drop.insert(k.clone());
                }
            }
        }
        if drop.is_empty() {
            break;
        }
        for k in drop {
            hoisted.remove(&k);
        }
    }
    Hoist {
        keys: hoisted,
        conflicting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::parse;

    fn ev() -> Event {
        Event {
            base: "main".into(),
            head: Some("f".into()),
            changed: vec!["app/a.ts".into()],
            all: false,
        }
    }

    const WF: &str = r#"
on: pull_request
defaults:
  run:
    working-directory: ./app
jobs:
  lint:
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
      - run: npm ci
      - run: npx prisma generate
      - run: npm run lint
  build:
    steps:
      - run: npm ci
      - run: npx prisma generate
      - run: npm run build
  verify-build:
    steps:
      - run: npm ci
      - run: npm run build && node check.js
  int:
    services:
      db:
        image: postgres
        ports: ["5432:5432"]
    steps:
      - run: npm ci
      - run: echo "DATABASE_URL=postgres://x@localhost:5432/t" > .env.test
      - run: npx prisma generate
      - run: npm run test:int
  deploy:
    needs: [build]
    steps:
      - run: echo ${{ secrets.X }}
"#;

    #[test]
    fn units_and_setup() {
        let w = parse(".github/workflows/ci.yml", WF).unwrap();
        let p = build(vec![w], &ev(), &[]);
        let units: Vec<(Vec<String>, bool)> = p
            .units
            .iter()
            .map(|u| (u.jobs.clone(), u.exclusive))
            .collect();
        assert_eq!(
            units,
            vec![
                (vec!["lint".to_string()], false),
                (
                    vec!["build".into(), "verify-build".into(), "deploy".into()],
                    false
                ),
                (vec!["int".to_string()], true),
            ]
        );
        let setup = hoisted_keys(&p);
        let runs: Vec<&str> = setup.iter().map(|k| k.run.as_str()).collect();
        assert_eq!(runs, vec!["npm ci", "npx prisma generate"]);
        assert!(setup.iter().all(|k| k.wd == "app"));
    }

    #[test]
    fn scripts_a_step_starts() {
        let pkg = r#"{"scripts": {
            "pretest": "prisma generate",
            "test": "jest && npm run lint",
            "lint": "eslint .",
            "build": "tsc",
            "audit": "npm audit"
        }}"#;
        let all = with_scripts("npm test", Some(pkg));
        assert!(
            all.contains("prisma generate") && all.contains("jest") && all.contains("eslint .")
        );
        assert!(!all.contains("tsc") && !all.contains("npm audit"));
        assert!(with_scripts("yarn build", Some(pkg)).ends_with("\ntsc"));
        assert_eq!(with_scripts("npm ci", Some(pkg)), "npm ci");
        assert_eq!(with_scripts("npm test", None), "npm test");
    }

    #[test]
    fn hoisting_stops_at_job_specific_steps() {
        let wf = r#"
on: pull_request
jobs:
  a:
    steps:
      - run: cp .env.example .env
      - run: npm run migrate
      - run: npm test
  b:
    steps:
      - run: npm run migrate
      - run: npm run lint
"#;
        let p = build(vec![parse("x.yml", wf).unwrap()], &ev(), &[]);
        // `npm run migrate` repeats, but in `a` it follows a step that is not hoisted
        assert!(hoisted_keys(&p).is_empty());
    }

    #[test]
    fn skipped_and_filtered() {
        let a = parse(
            ".github/workflows/a.yml",
            "on:\n  pull_request:\n    paths: ['web/**']\njobs: {}\n",
        )
        .unwrap();
        let b = parse(
            ".github/workflows/b.yml",
            "on: pull_request\njobs:\n  x:\n    steps: [{run: 'true'}]\n",
        )
        .unwrap();
        let p = build(vec![a.clone(), b.clone()], &ev(), &[]);
        assert_eq!(
            p.skipped,
            vec![(
                ".github/workflows/a.yml".to_string(),
                "paths not matched".to_string()
            )]
        );
        assert_eq!(p.units.len(), 1);
        let p = build(vec![a, b], &ev(), &["a.yml".to_string()]);
        assert!(p.units.is_empty() && p.skipped.len() == 1);
    }

    #[test]
    fn written() {
        let f = |wd: &str, r: &str| written_files(wd, r).into_iter().collect::<Vec<_>>();
        assert_eq!(
            f("app", "echo \"A=1\" > .env.test\necho B >> .env.test"),
            vec!["app/.env.test"]
        );
        assert_eq!(f("", "cp .env.example .env"), vec![".env"]);
        assert_eq!(
            f(
                "",
                "echo \"x=1\" >> \"$GITHUB_OUTPUT\"\nnpm test 2>&1 | tee out.log\nfoo > /dev/null"
            ),
            vec!["out.log"]
        );
        assert!(f("", "a 2>&1").is_empty());
    }

    #[test]
    fn jobs_writing_the_same_file_share_a_unit() {
        let wf = r#"
on: pull_request
jobs:
  unit:
    steps:
      - run: echo "DATABASE_URL=postgres://localhost:5432/t" > .env.test
      - run: npm test
  int:
    services:
      db:
        image: postgres
        ports: ["5432:5432"]
    steps:
      - run: echo "DATABASE_URL=postgres://localhost:5432/t" > .env.test
      - run: npm run test:int
  lint:
    steps:
      - run: npm run lint
"#;
        let p = build(vec![parse("x.yml", wf).unwrap()], &ev(), &[]);
        let units: Vec<(Vec<String>, bool)> = p
            .units
            .iter()
            .map(|u| (u.jobs.clone(), u.exclusive))
            .collect();
        assert_eq!(
            units,
            vec![
                (vec!["unit".to_string(), "int".into()], true),
                (vec!["lint".to_string()], false)
            ]
        );
    }

    #[test]
    fn a_venv_is_told_by_the_command_not_by_a_name() {
        let tied = |r: &str| absolute_env_re().is_match(r);
        for r in [
            "python3 -m venv .venv",
            "python -m virtualenv env",
            "virtualenv env",
            "pipenv install",
            "conda create -n x",
            "cd api && conda install y",
            "FOO=1 pipenv sync",
            "pip install -r requirements.txt",
            "python3 -m pip install x",
        ] {
            assert!(tied(r), "{r}");
        }
        for r in [
            "npm run lint:venv",
            "my-venv-tool --check",
            "./scripts/venv-report.sh",
            "echo conda-forge",
            "npm ci",
        ] {
            assert!(!tied(r), "{r}");
        }
    }

    #[test]
    fn a_shell_local_ci_cannot_start_keeps_a_step_out_of_setup() {
        let wf = "on: pull_request\ndefaults:\n  run:\n    shell: pwsh\njobs:\n  a:\n    steps:\n      - run: npm ci\n";
        let w = parse("x.yml", wf).unwrap();
        let j = &w.jobs[0];
        assert_eq!(
            stay_reason(&w, j, &j.steps[0]),
            Some(StayReason::UnsupportedShell)
        );
        for ok in [
            None,
            Some("bash"),
            Some("sh"),
            Some("python"),
            Some("perl {0}"),
        ] {
            assert!(shell_supported(ok), "{ok:?}");
        }
        assert!(!shell_supported(Some("pwsh")));
    }

    #[test]
    fn dirs() {
        assert_eq!(norm_dir("./api/"), "api");
        assert_eq!(norm_dir("."), "");
        assert_eq!(norm_dir(""), "");
    }
}
