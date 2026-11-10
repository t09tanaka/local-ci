//! Reuse the result of a unit that passed when nothing it depends on has
//! changed since: the files its workflow's `paths` (or all but its
//! `paths-ignore`) cover, every file at the root of the repository (lockfiles
//! and shared configuration), the workflows and local actions themselves,
//! the merge base and branch, the versions of the tools its steps use, and
//! local-ci itself. This trusts `paths` to name what the workflow depends on,
//! as GitHub does when a pull request changes nothing it covers; a change
//! outside it, say in a shared directory the workflow does not list, does
//! not run the unit again. Results are kept for a day: tests that depend on
//! the date, and checks against advisory databases, change without the code.

use crate::artifact;
use crate::artifact_io;
use crate::context::ExecContext;
use crate::exec::{JobResult, Outcome, StepResult, UnitResult};
use crate::filter::last_match;
use crate::plan::Unit;
use crate::workflow::Workflow;
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};

pub const TTL_SECS: u64 = 24 * 3600;

/// Tools whose version goes into the fingerprint of a unit that names them:
/// the word to look for and the command that prints the version.
const TOOLS: &[(&str, &[&str])] = &[
    ("npm", &["npm", "--version"]),
    ("npx", &["npm", "--version"]),
    ("yarn", &["yarn", "--version"]),
    ("pnpm", &["pnpm", "--version"]),
    ("dart", &["dart", "--version"]),
    ("flutter", &["flutter", "--version"]),
    ("cargo", &["rustc", "--version"]),
    ("python3", &["python3", "--version"]),
    ("pip", &["python3", "--version"]),
    ("go", &["go", "version"]),
];

/// What the fingerprints of one run share.
pub struct Inputs {
    /// Starts the version commands and git with the attempt's environment.
    ctx: ExecContext,
    root: PathBuf,
    /// `(path, object)` of every file at the commit, in git's order.
    files: Vec<(String, String)>,
    /// Directories whose files always count: workflows and local actions.
    always: Vec<String>,
    merge_base: Option<String>,
    branch: Option<String>,
    /// local-ci's version and, for builds between releases, its binary.
    binary: String,
    no_cache: Vec<String>,
    /// Output of version commands and contents of blobs, read once.
    seen: Mutex<HashMap<String, Option<String>>>,
}

impl Inputs {
    pub fn read(
        ctx: &ExecContext,
        root: &Path,
        sha: &str,
        base: &str,
        branch: Option<&str>,
        workflow_dirs: &[String],
        no_cache: &[String],
    ) -> Result<Inputs> {
        let listed = crate::git::git(ctx, root, &["ls-tree", "-r", "-z", "--full-tree", sha])?;
        let mut files = Vec::new();
        for entry in listed.split('\0').filter(|e| !e.is_empty()) {
            // `<mode> <type> <object>\t<path>`
            let (meta, path) = entry
                .split_once('\t')
                .context("unexpected ls-tree output")?;
            let object = meta.rsplit(' ').next().unwrap_or_default();
            files.push((path.to_string(), object.to_string()));
        }
        let mut always = vec![".github/".to_string()];
        for d in workflow_dirs {
            always.push(format!("{}/", d.trim_end_matches('/')));
        }
        let merge_base = crate::git::git(ctx, root, &["merge-base", base, sha]).ok();
        let exe = std::env::current_exe()
            .and_then(|p| p.metadata())
            .map(|m| {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                format!("{} {t}", m.len())
            })
            .unwrap_or_default();
        Ok(Inputs {
            ctx: ctx.fresh_attempt(),
            root: root.to_path_buf(),
            files,
            always,
            merge_base,
            branch: branch.map(String::from),
            binary: format!("{} {exe}", env!("CARGO_PKG_VERSION")),
            no_cache: no_cache.to_vec(),
            seen: Mutex::new(HashMap::new()),
        })
    }

    /// The fingerprint of `u` (in workflow `w`, run as `event`), or `None`
    /// when its result is never reused.
    pub fn fingerprint(&self, w: &Workflow, event: &str, u: &Unit) -> Option<String> {
        if self.excluded(w, &u.jobs) {
            return None;
        }
        // what the steps run, with the package.json scripts they start
        let mut texts = Vec::new();
        for id in &u.jobs {
            let j = w.jobs.iter().find(|j| &j.id == id)?;
            for s in &j.steps {
                // what an upload leaves is kept with the result (see `Store`);
                // a download is reused only against the artifacts it took,
                // which a name that is not known now cannot be checked for
                if let Some(u) = s.uses.as_deref() {
                    if matches!(artifact::parse_uses(u), Some(Ok(artifact::Kind::Download)))
                        && artifact::needed_download(u, &s.with).is_none()
                    {
                        return None;
                    }
                }
                let Some(run) = &s.run else { continue };
                let dir = crate::plan::step_dir(w, j, s);
                let pkg = if dir.is_empty() {
                    "package.json".to_string()
                } else {
                    format!("{dir}/package.json")
                };
                texts.push(crate::plan::with_scripts(run, self.blob(&pkg).as_deref()));
            }
        }
        if texts.iter().any(|t| volatile(t)) {
            return None;
        }
        let trigger = w.triggers.iter().find(|t| {
            if event == "push" {
                t.event == "push"
            } else {
                t.event.starts_with("pull_request")
            }
        });
        let covered = |path: &str| match trigger {
            Some(t) if !t.paths.is_empty() => last_match(&t.paths, path) == Some(true),
            Some(t) if !t.paths_ignore.is_empty() => {
                last_match(&t.paths_ignore, path) != Some(true)
            }
            _ => true,
        };
        let mut m = format!(
            "local-ci {}\nnode {}\nevent {event}\nworkflow {}\njobs {}\nbranch {}\nbase {}\n",
            self.binary,
            self.version(&["node", "--version"]),
            w.file,
            u.jobs.join(","),
            self.branch.as_deref().unwrap_or(""),
            self.merge_base.as_deref().unwrap_or("none"),
        );
        for (word, cmd) in TOOLS {
            if texts.iter().any(|t| names(t, word)) {
                m.push_str(&format!("{word} {}\n", self.version(cmd)));
            }
        }
        for (path, object) in &self.files {
            if !path.contains('/')
                || self.always.iter().any(|d| path.starts_with(d.as_str()))
                || covered(path)
            {
                m.push_str(&format!("{object} {path}\n"));
            }
        }
        hash(&self.ctx, &self.root, &m)
    }

    fn excluded(&self, w: &Workflow, jobs: &[String]) -> bool {
        let is_wf = |f: &str| w.file == f || w.file.ends_with(&format!("/{f}"));
        self.no_cache.iter().any(|e| match e.split_once(':') {
            Some((f, j)) => is_wf(f) && jobs.iter().any(|x| x == j),
            None => is_wf(e),
        })
    }

    fn version(&self, cmd: &[&str]) -> String {
        let key = cmd.join(" ");
        self.remember(key, || {
            let o = self
                .ctx
                .command(cmd[0])
                .args(&cmd[1..])
                .stdin(Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())?;
            // `java -version`-style tools print to stderr
            let text = [o.stdout, o.stderr].concat();
            Some(String::from_utf8_lossy(&text).trim().to_string())
        })
        .unwrap_or_default()
    }

    /// The file at `path` in the commit.
    fn blob(&self, path: &str) -> Option<String> {
        let object = self.files.iter().find(|(p, _)| p == path)?.1.clone();
        let root = self.root.clone();
        let ctx = self.ctx.fresh_attempt();
        self.remember(format!("blob {object}"), move || {
            crate::git::git(&ctx, &root, &["cat-file", "-p", &object]).ok()
        })
    }

    fn remember(&self, key: String, f: impl FnOnce() -> Option<String>) -> Option<String> {
        if let Some(v) = self.seen.lock().unwrap().get(&key) {
            return v.clone();
        }
        let v = f();
        self.seen.lock().unwrap().insert(key, v.clone());
        v
    }
}

/// Checks whose result changes without the code: advisory databases.
fn volatile(text: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\baudit\b|advisor|osv-scanner").unwrap())
        .is_match(&text.replace("--no-audit", ""))
}

/// Whether `text` runs the command `word`.
fn names(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .any(|w| w == word)
}

fn hash(ctx: &ExecContext, root: &Path, text: &str) -> Option<String> {
    let mut child = ctx
        .command("git")
        .args(["hash-object", "--stdin"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(text.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|h| h.len() >= 40)
}

#[derive(Serialize, Deserialize)]
struct Saved {
    at: u64,
    sha: String,
    jobs: Vec<SavedJob>,
    /// The artifacts the unit downloaded and what they held (name, digest):
    /// the result is reused only against the same ones.
    #[serde(default)]
    downloads: Vec<(String, String)>,
    /// The artifacts the unit uploaded, kept beside this file in
    /// `<fp>.artifacts/` (name, version, digest).
    #[serde(default)]
    uploads: Vec<(String, u32, String)>,
}

/// A kept result that is recent enough, with what it needs from and gives to
/// the artifacts of the run it is reused in.
pub struct Hit {
    pub result: UnitResult,
    pub downloads: Vec<(String, String)>,
    pub uploads: Vec<(String, u32, String)>,
}

/// Which of the kept results (`None`: there is none) are reused. A result
/// that downloaded artifacts is reused only when every one of them is what
/// another reused result uploads (its newest version), with the digest it was
/// kept against: an artifact that will be made again by a unit that runs, or
/// that changed, is not. Results that depend on each other are settled
/// together, whatever their order. An artifact is of the workflow that
/// uploaded it: another workflow's result of the same name is not it.
pub fn settle(hits: &[Option<Hit>]) -> Vec<bool> {
    let mut taken = vec![false; hits.len()];
    // artifacts belong to a workflow: (workflow, name) -> digest
    let mut available: HashMap<(&str, &str), &str> = HashMap::new();
    loop {
        let mut progress = false;
        for (i, h) in hits.iter().enumerate() {
            let Some(h) = h.as_ref().filter(|_| !taken[i]) else {
                continue;
            };
            let wf = h.result.workflow.as_str();
            if !h
                .downloads
                .iter()
                .all(|(n, d)| available.get(&(wf, n.as_str())) == Some(&d.as_str()))
            {
                continue;
            }
            taken[i] = true;
            progress = true;
            let mut ups: Vec<&(String, u32, String)> = h.uploads.iter().collect();
            ups.sort_by_key(|u| u.1);
            for (n, _, d) in ups {
                available.insert((wf, n), d);
            }
        }
        if !progress {
            return taken;
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SavedJob {
    job: String,
    display: String,
    /// Why the job was skipped, when it was.
    skip: Option<String>,
    /// Steps that were skipped or noted, which the report lists as not
    /// covered or as warnings.
    #[serde(default)]
    steps: Vec<SavedStep>,
}

#[derive(Serialize, Deserialize)]
struct SavedStep {
    label: String,
    skip: Option<String>,
    note: Option<String>,
}

/// The results kept for one repository.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store { dir }
    }

    fn artifacts_dir(&self, fp: &str) -> PathBuf {
        self.dir.join(format!("{fp}.artifacts"))
    }

    /// The kept result for `fp`, as a unit that passed, if it is recent enough.
    pub fn get(&self, fp: &str, workflow: &str, now: u64) -> Option<Hit> {
        let text = std::fs::read_to_string(self.dir.join(format!("{fp}.json"))).ok()?;
        let saved: Saved = serde_json::from_str(&text).ok()?;
        if now.saturating_sub(saved.at) > TTL_SECS {
            return None;
        }
        let (downloads, uploads) = (saved.downloads, saved.uploads);
        let jobs = saved
            .jobs
            .into_iter()
            .map(|j| JobResult {
                workflow: workflow.to_string(),
                job: j.job,
                display: j.display,
                outcome: match j.skip {
                    Some(r) => Outcome::Skip(r),
                    None => Outcome::Pass,
                },
                secs: 0.0,
                cancelled: false,
                time: None,
                steps: j
                    .steps
                    .into_iter()
                    .map(|s| StepResult {
                        label: s.label,
                        outcome: match s.skip {
                            Some(r) => Outcome::Skip(r),
                            None => Outcome::Pass,
                        },
                        exit: None,
                        secs: 0.0,
                        log: None,
                        note: s.note,
                    })
                    .collect(),
            })
            .collect();
        Some(Hit {
            result: UnitResult {
                unit: None,
                workflow: workflow.to_string(),
                jobs,
                waited_secs: 0.0,
                copy_secs: None,
                outcomes: vec![],
                cached: Some(format!(
                    "{}, {}",
                    &saved.sha[..saved.sha.len().min(7)],
                    ago(now.saturating_sub(saved.at))
                )),
            },
            downloads,
            uploads,
        })
    }

    /// Put the artifacts the result under `fp` kept into `run_dir`, as this
    /// run's own (see [`artifact_io::restore_artifacts`]). An error means the
    /// result is not reused: a file that does not match its receipt, or
    /// something missing.
    pub fn restore(&self, fp: &str, hit: &Hit, run_dir: &Path, plan_digest: &str) -> Result<()> {
        if hit.uploads.is_empty() {
            return Ok(());
        }
        artifact_io::restore_artifacts(
            &self.artifacts_dir(fp),
            run_dir,
            plan_digest,
            &hit.result.workflow,
            &hit.uploads,
        )
    }

    /// Keep `u`'s result under `fp` when every job passed or was skipped, with
    /// the artifacts it uploaded in `run_dir` and the digests of the `downloads`
    /// (artifact names) it took. A result whose artifacts cannot be kept whole
    /// is not kept.
    pub fn put(
        &self,
        fp: &str,
        sha: &str,
        u: &UnitResult,
        now: u64,
        run_dir: &Path,
        downloads: &[String],
    ) {
        if u.jobs.is_empty()
            || u.jobs
                .iter()
                .any(|j| matches!(j.outcome, Outcome::Fail | Outcome::Error(_)))
        {
            return;
        }
        let skip = |o: &Outcome| match o {
            Outcome::Skip(r) => Some(r.clone()),
            _ => None,
        };
        let _ = std::fs::create_dir_all(&self.dir);
        let nodes: Vec<String> = u
            .jobs
            .iter()
            .map(|j| crate::dag::node_id(&j.workflow, &j.job))
            .collect();
        let tmp_art = self
            .dir
            .join(format!(".{fp}.artifacts.{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_art);
        let Ok(uploads) = artifact_io::save_unit_artifacts(run_dir, &u.workflow, &nodes, &tmp_art)
        else {
            let _ = std::fs::remove_dir_all(&tmp_art);
            return;
        };
        let mut seen = Vec::new();
        // what the unit uploaded itself is not another unit's to change
        for n in downloads
            .iter()
            .filter(|n| !uploads.iter().any(|(u, _, _)| u == *n))
        {
            let Some(d) = artifact_io::latest_digest(run_dir, &u.workflow, n) else {
                let _ = std::fs::remove_dir_all(&tmp_art);
                return;
            };
            seen.push((n.clone(), d));
        }
        let final_art = self.artifacts_dir(fp);
        let _ = std::fs::remove_dir_all(&final_art);
        if !uploads.is_empty() && std::fs::rename(&tmp_art, &final_art).is_err() {
            let _ = std::fs::remove_dir_all(&tmp_art);
            return;
        }
        let saved = Saved {
            at: now,
            sha: sha.to_string(),
            downloads: seen,
            uploads,
            jobs: u
                .jobs
                .iter()
                .map(|j| SavedJob {
                    job: j.job.clone(),
                    display: j.display.clone(),
                    skip: skip(&j.outcome),
                    steps: j
                        .steps
                        .iter()
                        .filter(|s| matches!(s.outcome, Outcome::Skip(_)) || s.note.is_some())
                        .map(|s| SavedStep {
                            label: s.label.clone(),
                            skip: skip(&s.outcome),
                            note: s.note.clone(),
                        })
                        .collect(),
                })
                .collect(),
        };
        let tmp = self.dir.join(format!(".{fp}.{}", std::process::id()));
        if let Ok(text) = serde_json::to_string(&saved) {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, self.dir.join(format!("{fp}.json")));
            }
        }
    }

    /// Remove what is too old to be used.
    pub fn prune(&self, now: u64) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for e in rd.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|t| now.saturating_sub(t.as_secs()) > TTL_SECS);
            if old {
                let p = e.path();
                let _ = if p.is_dir() {
                    std::fs::remove_dir_all(&p)
                } else {
                    std::fs::remove_file(&p)
                };
            }
        }
    }
}

fn ago(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s => format!("{}h ago", s / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::parse;

    fn inputs(files: &[(&str, &str)]) -> Inputs {
        let i = Inputs {
            ctx: ExecContext::new(
                [
                    ("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin"),
                    ("HOME", "/tmp"),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ),
            root: std::env::temp_dir(),
            files: files
                .iter()
                .map(|(p, o)| (p.to_string(), o.to_string()))
                .collect(),
            always: vec![".github/".into()],
            merge_base: Some("b".repeat(40)),
            branch: Some("feature".into()),
            binary: "0.0.0".into(),
            no_cache: vec!["ci.yml:audit".into()],
            seen: Mutex::new(HashMap::new()),
        };
        // no commands or git objects behind these tests
        for cmd in ["node --version", "npm --version"] {
            i.seen.lock().unwrap().insert(cmd.into(), Some("v".into()));
        }
        i
    }

    const WF: &str = r#"
on:
  pull_request:
    paths: ["api/**", "!api/docs/**"]
jobs:
  test:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: api
    steps:
      - run: npm ci --no-audit
      - run: npm test
  audit:
    runs-on: ubuntu-latest
    steps:
      - run: npm test
"#;

    fn unit(job: &str) -> Unit {
        Unit {
            wf: 0,
            jobs: vec![job.into()],
            exclusive: false,
        }
    }

    #[test]
    fn what_the_fingerprint_covers() {
        let w = parse(".github/workflows/ci.yml", WF).unwrap();
        let base = [
            (".github/workflows/ci.yml", "w1"),
            ("package-lock.json", "l1"),
            ("api/src/a.ts", "a1"),
            ("api/docs/x.md", "d1"),
            ("web/b.ts", "b1"),
        ];
        let fp = |files: &[(&str, &str)]| {
            inputs(files)
                .fingerprint(&w, "pull_request", &unit("test"))
                .unwrap()
        };
        let a = fp(&base);
        // outside `paths`, or excluded by `!`: the same fingerprint
        let mut other = base;
        other[3].1 = "d2";
        other[4].1 = "b2";
        assert_eq!(fp(&other), a);
        // a covered file, a file at the root or the workflow: a different one
        for i in [0, 1, 2] {
            let mut changed = base;
            changed[i].1 = "x";
            assert_ne!(fp(&changed), a, "{}", base[i].0);
        }
        // so do the base and the branch
        let mut moved = inputs(&base);
        moved.merge_base = Some("c".repeat(40));
        assert_ne!(
            moved
                .fingerprint(&w, "pull_request", &unit("test"))
                .unwrap(),
            a
        );
        let mut other_branch = inputs(&base);
        other_branch.branch = Some("other".into());
        assert_ne!(
            other_branch
                .fingerprint(&w, "pull_request", &unit("test"))
                .unwrap(),
            a
        );
    }

    #[test]
    fn some_results_are_never_reused() {
        let w = parse(".github/workflows/ci.yml", WF).unwrap();
        let i = inputs(&[("api/a.ts", "a"), ("api/package.json", "p")]);
        // listed in no-cache
        assert!(i.fingerprint(&w, "pull_request", &unit("audit")).is_none());
        // `npm ci --no-audit` alone is not an audit...
        assert!(i.fingerprint(&w, "pull_request", &unit("test")).is_some());
        // ...but a script `npm test` starts that runs one is
        let i = inputs(&[("api/a.ts", "a"), ("api/package.json", "p")]);
        i.seen.lock().unwrap().insert(
            "blob p".into(),
            Some(r#"{"scripts":{"test":"npm run check","check":"npm audit"}}"#.into()),
        );
        assert!(i.fingerprint(&w, "pull_request", &unit("test")).is_none());
        assert!(volatile("node scripts/audit-dependencies.mjs"));
        assert!(!volatile("npm ci --no-audit"));
        assert!(names("cd api && npm test", "npm") && !names("pnpm i", "npm"));
    }

    #[test]
    fn kept_results_pass_until_they_are_old() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().to_path_buf());
        let job = |o: Outcome| JobResult {
            workflow: "ci.yml".into(),
            job: "test".into(),
            display: "test".into(),
            outcome: o,
            secs: 3.0,
            steps: vec![
                StepResult {
                    label: "deploy".into(),
                    outcome: Outcome::Skip("uses secrets".into()),
                    exit: None,
                    secs: 0.0,
                    log: None,
                    note: None,
                },
                StepResult {
                    label: "test".into(),
                    outcome: Outcome::Pass,
                    exit: Some(0),
                    secs: 3.0,
                    log: None,
                    note: None,
                },
            ],
            cancelled: false,
            time: None,
        };
        let u = |o: Outcome| UnitResult {
            unit: None,
            workflow: "ci.yml".into(),
            jobs: vec![job(o)],
            waited_secs: 0.0,
            copy_secs: None,
            cached: None,
            outcomes: vec![],
        };
        let sha = "a".repeat(40);
        let run = tmp.path().join("run");
        store.put("f1", &sha, &u(Outcome::Fail), 100, &run, &[]);
        assert!(store.get("f1", "ci.yml", 100).is_none());
        store.put("f2", &sha, &u(Outcome::Pass), 100, &run, &[]);
        let hit = store.get("f2", "ci.yml", 100 + 7200).unwrap().result;
        assert_eq!(hit.jobs[0].outcome, Outcome::Pass);
        assert_eq!(hit.cached.as_deref(), Some("aaaaaaa, 2h ago"));
        // what was not covered is still reported
        assert_eq!(
            crate::report::not_covered(&[hit], ".github/workflows"),
            vec!["ci.yml › test › deploy: uses secrets"]
        );
        assert!(store.get("f2", "ci.yml", 100 + TTL_SECS + 1).is_none());
    }

    fn hit(downloads: &[(&str, &str)], uploads: &[(&str, u32, &str)]) -> Option<Hit> {
        hit_in("ci.yml", downloads, uploads)
    }

    fn hit_in(
        workflow: &str,
        downloads: &[(&str, &str)],
        uploads: &[(&str, u32, &str)],
    ) -> Option<Hit> {
        Some(Hit {
            result: UnitResult {
                unit: None,
                workflow: workflow.into(),
                jobs: vec![],
                waited_secs: 0.0,
                copy_secs: None,
                outcomes: vec![],
                cached: None,
            },
            downloads: downloads
                .iter()
                .map(|(n, d)| (n.to_string(), d.to_string()))
                .collect(),
            uploads: uploads
                .iter()
                .map(|(n, v, d)| (n.to_string(), *v, d.to_string()))
                .collect(),
        })
    }

    #[test]
    fn a_download_is_reused_only_against_what_it_took() {
        // no downloads: reused; nothing kept: not
        assert_eq!(settle(&[hit(&[], &[]), None]), [true, false]);
        // the uploader is reused with the digest the download was kept
        // against, whichever comes first
        let up = hit(&[], &[("b", 1, "d1")]);
        let down = hit(&[("b", "d1")], &[]);
        assert_eq!(settle(&[down, up]), [true, true]);
        // another digest, an uploader that runs again, or none at all
        assert_eq!(
            settle(&[hit(&[("b", "d1")], &[]), hit(&[], &[("b", 1, "d2")])]),
            [false, true]
        );
        assert_eq!(settle(&[hit(&[("b", "d1")], &[]), None]), [false, false]);
        assert_eq!(settle(&[hit(&[("b", "d1")], &[])]), [false]);
        // a chain settles whatever its order
        assert_eq!(
            settle(&[
                hit(&[("c", "x")], &[]),
                hit(&[("b", "y")], &[("c", 1, "x")]),
                hit(&[], &[("b", 1, "y")]),
            ]),
            [true, true, true]
        );
        // a newer version is the one a download takes
        assert_eq!(
            settle(&[
                hit(&[("b", "new")], &[]),
                hit(&[], &[("b", 2, "new"), ("b", 1, "old")]),
            ]),
            [true, true]
        );
        // every artifact it took must match
        assert_eq!(
            settle(&[
                hit(&[("a", "1"), ("b", "2")], &[]),
                hit(&[], &[("a", 1, "1")]),
            ]),
            [false, true]
        );
    }

    #[test]
    fn an_artifact_is_only_of_the_workflow_that_uploaded_it() {
        // the same name and digest, uploaded by another workflow: not it
        assert_eq!(
            settle(&[
                hit_in("e2e.yml", &[("b", "d")], &[]),
                hit_in("ci.yml", &[], &[("b", 1, "d")]),
            ]),
            [false, true]
        );
        // the same workflow: it is
        assert_eq!(
            settle(&[
                hit_in("ci.yml", &[("b", "d")], &[]),
                hit_in("ci.yml", &[], &[("b", 1, "d")]),
            ]),
            [true, true]
        );
        // two workflows that upload one name each have their own
        assert_eq!(
            settle(&[
                hit_in("ci.yml", &[("b", "x")], &[]),
                hit_in("e2e.yml", &[("b", "y")], &[]),
                hit_in("ci.yml", &[], &[("b", 1, "x")]),
                hit_in("e2e.yml", &[], &[("b", 1, "y")]),
            ]),
            [true, true, true, true]
        );
        // a chain inside one workflow settles whatever its order
        assert_eq!(
            settle(&[
                hit_in("ci.yml", &[("c", "x")], &[]),
                hit_in("ci.yml", &[("b", "y")], &[("c", 1, "x")]),
                hit_in("ci.yml", &[], &[("b", 1, "y")]),
            ]),
            [true, true, true]
        );
    }
}
