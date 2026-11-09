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

    /// The kept result for `fp`, as a unit that passed, if it is recent enough.
    pub fn get(&self, fp: &str, workflow: &str, now: u64) -> Option<UnitResult> {
        let text = std::fs::read_to_string(self.dir.join(format!("{fp}.json"))).ok()?;
        let saved: Saved = serde_json::from_str(&text).ok()?;
        if now.saturating_sub(saved.at) > TTL_SECS {
            return None;
        }
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
        Some(UnitResult {
            workflow: workflow.to_string(),
            jobs,
            waited_secs: 0.0,
            copy_secs: None,
            cached: Some(format!(
                "{}, {}",
                &saved.sha[..saved.sha.len().min(7)],
                ago(now.saturating_sub(saved.at))
            )),
        })
    }

    /// Keep `u`'s result under `fp` when every job passed or was skipped.
    pub fn put(&self, fp: &str, sha: &str, u: &UnitResult, now: u64) {
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
        let saved = Saved {
            at: now,
            sha: sha.to_string(),
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
        let _ = std::fs::create_dir_all(&self.dir);
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
                let _ = std::fs::remove_file(e.path());
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
            workflow: "ci.yml".into(),
            jobs: vec![job(o)],
            waited_secs: 0.0,
            copy_secs: None,
            cached: None,
        };
        let sha = "a".repeat(40);
        store.put("f1", &sha, &u(Outcome::Fail), 100);
        assert!(store.get("f1", "ci.yml", 100).is_none());
        store.put("f2", &sha, &u(Outcome::Pass), 100);
        let hit = store.get("f2", "ci.yml", 100 + 7200).unwrap();
        assert_eq!(hit.jobs[0].outcome, Outcome::Pass);
        assert_eq!(hit.cached.as_deref(), Some("aaaaaaa, 2h ago"));
        // what was not covered is still reported
        assert_eq!(
            crate::report::not_covered(&[hit], ".github/workflows"),
            vec!["ci.yml › test › deploy: uses secrets"]
        );
        assert!(store.get("f2", "ci.yml", 100 + TTL_SECS + 1).is_none());
    }
}
