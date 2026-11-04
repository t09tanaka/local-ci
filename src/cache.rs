//! Reuse the result of a unit that passed when nothing it depends on has
//! changed since: the files its workflow's `paths` (or all but its
//! `paths-ignore`) cover, the workflows and local actions themselves,
//! `.local-ci.toml`, the base when a step looks at it, and the versions of
//! Node and local-ci. `paths` is what GitHub goes by to decide whether the
//! workflow runs at all, so a reused result stands on the same ground as
//! GitHub not running it. Results are kept for a day: tests that depend on
//! the date, and checks against advisory databases, change without the code.

use crate::exec::{JobResult, Outcome, UnitResult};
use crate::filter::last_match;
use crate::plan::Unit;
use crate::workflow::{Job, Workflow};
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

pub const TTL_SECS: u64 = 24 * 3600;

/// What the fingerprints of one run share.
pub struct Inputs {
    root: PathBuf,
    /// `(path, object)` of every file at the commit, in git's order.
    files: Vec<(String, String)>,
    /// Directories whose files always count: workflows and local actions.
    always: Vec<String>,
    merge_base: Option<String>,
    node: String,
    no_cache: Vec<String>,
}

impl Inputs {
    pub fn read(
        root: &Path,
        sha: &str,
        base: &str,
        workflow_dirs: &[String],
        no_cache: &[String],
    ) -> Result<Inputs> {
        let listed = crate::git::git(root, &["ls-tree", "-r", "-z", "--full-tree", sha])?;
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
        let merge_base = crate::git::git(root, &["merge-base", base, sha]).ok();
        let node = Command::new("node")
            .arg("--version")
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        Ok(Inputs {
            root: root.to_path_buf(),
            files,
            always,
            merge_base,
            node,
            no_cache: no_cache.to_vec(),
        })
    }

    /// The fingerprint of `u` (in workflow `w`, run as `event`), or `None`
    /// when its result is never reused.
    pub fn fingerprint(&self, w: &Workflow, event: &str, u: &Unit) -> Option<String> {
        let jobs: Vec<&Job> = u
            .jobs
            .iter()
            .filter_map(|id| w.jobs.iter().find(|j| &j.id == id))
            .collect();
        if self.excluded(w, &u.jobs) {
            return None;
        }
        let scripts: Vec<&str> = jobs
            .iter()
            .flat_map(|j| j.steps.iter())
            .filter_map(|s| s.run.as_deref())
            .collect();
        if scripts.iter().any(|s| volatile(s)) {
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
            "local-ci {}\nnode {}\nevent {event}\nworkflow {}\njobs {}\n",
            env!("CARGO_PKG_VERSION"),
            self.node,
            w.file,
            u.jobs.join(",")
        );
        if scripts.iter().any(|s| looks_at_base(s)) {
            m.push_str(&format!(
                "base {}\n",
                self.merge_base.as_deref().unwrap_or("none")
            ));
        }
        for (path, object) in &self.files {
            if path == crate::config::FILE
                || self.always.iter().any(|d| path.starts_with(d.as_str()))
                || covered(path)
            {
                m.push_str(&format!("{object} {path}\n"));
            }
        }
        hash(&self.root, &m)
    }

    fn excluded(&self, w: &Workflow, jobs: &[String]) -> bool {
        let is_wf = |f: &str| w.file == f || w.file.ends_with(&format!("/{f}"));
        self.no_cache.iter().any(|e| match e.split_once(':') {
            Some((f, j)) => is_wf(f) && jobs.iter().any(|x| x == j),
            None => is_wf(e),
        })
    }
}

/// Checks whose result changes without the code: advisory databases.
fn volatile(script: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\baudit\b|advisor|osv-scanner").unwrap())
        .is_match(script)
}

/// A step whose result depends on the base it is compared with.
fn looks_at_base(script: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"base_ref|BASE_REF|merge-base|--affected|git (diff|log)|origin/|github\.event\.pull_request")
            .unwrap()
    })
    .is_match(script)
}

fn hash(root: &Path, text: &str) -> Option<String> {
    let mut child = Command::new("git")
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
                steps: vec![],
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
        let saved = Saved {
            at: now,
            sha: sha.to_string(),
            jobs: u
                .jobs
                .iter()
                .map(|j| SavedJob {
                    job: j.job.clone(),
                    display: j.display.clone(),
                    skip: match &j.outcome {
                        Outcome::Skip(r) => Some(r.clone()),
                        _ => None,
                    },
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
        Inputs {
            root: std::env::temp_dir(),
            files: files
                .iter()
                .map(|(p, o)| (p.to_string(), o.to_string()))
                .collect(),
            always: vec![".github/".into()],
            merge_base: Some("b".repeat(40)),
            node: "v22".into(),
            no_cache: vec!["ci.yml:audit".into()],
        }
    }

    const WF: &str = r#"
on:
  pull_request:
    paths: ["api/**", "!api/docs/**"]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
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
    fn only_what_the_paths_cover_counts() {
        let w = parse(".github/workflows/ci.yml", WF).unwrap();
        let base = [
            (".github/workflows/ci.yml", "w1"),
            ("api/src/a.ts", "a1"),
            ("api/docs/x.md", "d1"),
            ("web/b.ts", "b1"),
        ];
        let fp =
            |files: &[(&str, &str)]| inputs(files).fingerprint(&w, "pull_request", &unit("test"));
        let a = fp(&base).unwrap();
        // outside `paths`, or excluded by `!`: the same fingerprint
        let mut other = base;
        other[2].1 = "d2";
        other[3].1 = "b2";
        assert_eq!(fp(&other).unwrap(), a);
        // a covered file or the workflow itself: a different one
        let mut changed = base;
        changed[1].1 = "a2";
        assert_ne!(fp(&changed).unwrap(), a);
        let mut wf = base;
        wf[0].1 = "w2";
        assert_ne!(fp(&wf).unwrap(), a);
    }

    #[test]
    fn some_results_are_never_reused() {
        let w = parse(".github/workflows/ci.yml", WF).unwrap();
        let i = inputs(&[("api/a.ts", "a")]);
        assert!(i.fingerprint(&w, "pull_request", &unit("audit")).is_none());
        assert!(volatile("npm audit --omit=dev"));
        assert!(volatile("node scripts/audit-dependencies.mjs"));
        assert!(!volatile("npm test"));
        assert!(looks_at_base("npx test-gates --affected"));
        assert!(!looks_at_base("npm test"));
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
            steps: vec![],
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
        assert!(store.get("f2", "ci.yml", 100 + TTL_SECS + 1).is_none());
    }
}
