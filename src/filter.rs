//! Which workflows a pull request (or a push) of this branch would trigger.

use crate::workflow::{Trigger, Workflow};
use globset::{GlobBuilder, GlobMatcher};

/// The event local-ci pretends happened.
#[derive(Debug, Clone)]
pub struct Event {
    /// The base branch name the pull request targets (e.g. `main`).
    pub base: String,
    /// The branch being checked; `None` when detached.
    pub head: Option<String>,
    pub changed: Vec<String>,
    /// Ignore `paths` / `paths-ignore`.
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Run { event: String },
    Skip { reason: String },
}

fn matcher(pat: &str) -> Option<GlobMatcher> {
    // GitHub lets `**` glue onto a name (`**.md`, `docs/**.md`); globset wants it as its own component.
    let mut p = String::new();
    let mut rest = pat;
    while let Some(i) = rest.find("**") {
        p.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        if after.is_empty() || after.starts_with('/') {
            p.push_str("**");
        } else {
            p.push_str("**/*");
        }
        rest = after;
    }
    p.push_str(rest);
    GlobBuilder::new(&p)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

/// Last matching pattern wins; `!` negates. `None` if no pattern matched.
pub fn last_match(patterns: &[String], path: &str) -> Option<bool> {
    let mut result = None;
    for p in patterns {
        let (neg, body) = match p.strip_prefix('!') {
            Some(b) => (true, b),
            None => (false, p.as_str()),
        };
        if let Some(m) = matcher(body) {
            if m.is_match(path) {
                result = Some(!neg);
            }
        }
    }
    result
}

fn branch_ok(t: &Trigger, branch: Option<&str>) -> bool {
    if t.branches.is_empty() && t.branches_ignore.is_empty() {
        return true;
    }
    let Some(b) = branch else { return false };
    if !t.branches.is_empty() {
        return last_match(&t.branches, b) == Some(true);
    }
    last_match(&t.branches_ignore, b) != Some(true)
}

fn paths_ok(t: &Trigger, ev: &Event) -> Result<(), String> {
    if ev.all {
        return Ok(());
    }
    if !t.paths.is_empty() {
        if ev
            .changed
            .iter()
            .any(|f| last_match(&t.paths, f) == Some(true))
        {
            return Ok(());
        }
        return Err("paths not matched".into());
    }
    if !t.paths_ignore.is_empty()
        && !ev.changed.is_empty()
        && ev
            .changed
            .iter()
            .all(|f| last_match(&t.paths_ignore, f) == Some(true))
    {
        return Err("all changes in paths-ignore".into());
    }
    Ok(())
}

const PR_EVENTS: [&str; 2] = ["pull_request", "pull_request_target"];
const DEFAULT_PR_TYPES: [&str; 3] = ["opened", "synchronize", "reopened"];

pub fn decide(w: &Workflow, ev: &Event) -> Decision {
    let mut reasons = Vec::new();
    for t in &w.triggers {
        let pr = PR_EVENTS.contains(&t.event.as_str());
        if !pr && t.event != "push" {
            continue;
        }
        if pr
            && !t.types.is_empty()
            && !t
                .types
                .iter()
                .any(|ty| DEFAULT_PR_TYPES.contains(&ty.as_str()))
        {
            reasons.push(format!("{}: types {}", t.event, t.types.join(",")));
            continue;
        }
        // `push: tags: [...]` without `branches` runs for tag pushes only
        if !pr
            && (!t.tags.is_empty() || !t.tags_ignore.is_empty())
            && t.branches.is_empty()
            && t.branches_ignore.is_empty()
        {
            reasons.push("push: tags only".into());
            continue;
        }
        let branch = if pr {
            Some(ev.base.as_str())
        } else {
            ev.head.as_deref()
        };
        if !branch_ok(t, branch) {
            reasons.push(format!("{}: branch filter", t.event));
            continue;
        }
        match paths_ok(t, ev) {
            Ok(()) => {
                return Decision::Run {
                    event: if pr {
                        "pull_request".into()
                    } else {
                        "push".into()
                    },
                }
            }
            Err(r) => reasons.push(r),
        }
    }
    if w.triggers
        .iter()
        .all(|t| !PR_EVENTS.contains(&t.event.as_str()) && t.event != "push")
    {
        let evs: Vec<&str> = w.triggers.iter().map(|t| t.event.as_str()).collect();
        return Decision::Skip {
            reason: format!(
                "only {}",
                if evs.is_empty() {
                    "no triggers".to_string()
                } else {
                    evs.join(", ")
                }
            ),
        };
    }
    reasons.dedup();
    Decision::Skip {
        reason: reasons.join("; "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::parse;

    fn ev(changed: &[&str]) -> Event {
        Event {
            base: "main".into(),
            head: Some("feature".into()),
            changed: changed.iter().map(|s| s.to_string()).collect(),
            all: false,
        }
    }

    #[test]
    fn glob_semantics() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(last_match(&p(&["src/**"]), "src/a/b.ts"), Some(true));
        assert_eq!(last_match(&p(&["*.js"]), "a/b.js"), None);
        assert_eq!(last_match(&p(&["*.js"]), "b.js"), Some(true));
        assert_eq!(last_match(&p(&["**/*.ts"]), "x/y/z.ts"), Some(true));
        assert_eq!(last_match(&p(&["**/*.ts"]), "z.ts"), Some(true));
        assert_eq!(
            last_match(&p(&["app/**", "!app/docs/**"]), "app/docs/x.md"),
            Some(false)
        );
        assert_eq!(
            last_match(
                &p(&[".github/workflows/ci.yml"]),
                ".github/workflows/ci.yml"
            ),
            Some(true)
        );
    }

    #[test]
    fn paths_and_ignore() {
        let w = parse(
            "a.yml",
            "on:\n  pull_request:\n    paths: ['api/**']\njobs: {}\n",
        )
        .unwrap();
        assert!(matches!(
            decide(&w, &ev(&["api/x.ts"])),
            Decision::Run { .. }
        ));
        assert!(matches!(
            decide(&w, &ev(&["web/x.ts"])),
            Decision::Skip { .. }
        ));
        let mut all = ev(&["web/x.ts"]);
        all.all = true;
        assert!(matches!(decide(&w, &all), Decision::Run { .. }));
        let w = parse(
            "a.yml",
            "on:\n  pull_request:\n    paths-ignore: ['**.md']\njobs: {}\n",
        )
        .unwrap();
        assert!(matches!(
            decide(&w, &ev(&["README.md"])),
            Decision::Skip { .. }
        ));
        assert!(matches!(
            decide(&w, &ev(&["README.md", "a.ts"])),
            Decision::Run { .. }
        ));
    }

    #[test]
    fn paths_ignore_does_not_skip_a_run_that_changed_nothing() {
        let w = parse(
            "a.yml",
            "on:\n  pull_request:\n    paths-ignore: ['**.md']\njobs: {}\n",
        )
        .unwrap();
        assert!(matches!(decide(&w, &ev(&[])), Decision::Run { .. }));
    }

    #[test]
    fn events_types_and_branches() {
        let w = parse("a.yml", "on:\n  push:\n    branches: [main]\njobs: {}\n").unwrap();
        assert!(matches!(decide(&w, &ev(&["a"])), Decision::Skip { .. }));
        let w = parse(
            "a.yml",
            "on:\n  pull_request:\n    branches: [main]\njobs: {}\n",
        )
        .unwrap();
        assert_eq!(
            decide(&w, &ev(&["a"])),
            Decision::Run {
                event: "pull_request".into()
            }
        );
        let w = parse(
            "a.yml",
            "on:\n  pull_request:\n    types: [closed]\njobs: {}\n",
        )
        .unwrap();
        assert!(matches!(decide(&w, &ev(&["a"])), Decision::Skip { .. }));
        let w = parse(
            "a.yml",
            "on:\n  pull_request:\n    types: [opened, ready_for_review]\njobs: {}\n",
        )
        .unwrap();
        assert!(matches!(decide(&w, &ev(&["a"])), Decision::Run { .. }));
        let w = parse(
            "a.yml",
            "on:\n  workflow_dispatch:\n  schedule:\n    - cron: '0 0 * * *'\njobs: {}\n",
        )
        .unwrap();
        assert_eq!(
            decide(&w, &ev(&["a"])),
            Decision::Skip {
                reason: "only workflow_dispatch, schedule".into()
            }
        );
        let w = parse("a.yml", "on: push\njobs: {}\n").unwrap();
        assert_eq!(
            decide(&w, &ev(&["a"])),
            Decision::Run {
                event: "push".into()
            }
        );
        let w = parse(
            "a.yml",
            "on:\n  push:\n    tags: ['v*']\n  workflow_dispatch:\njobs: {}\n",
        )
        .unwrap();
        assert_eq!(
            decide(&w, &ev(&["a"])),
            Decision::Skip {
                reason: "push: tags only".into()
            }
        );
    }

    #[test]
    fn an_invalid_glob_matches_nothing() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(last_match(&p(&["[a"]), "a"), None);
        assert_eq!(last_match(&p(&["[a", "a"]), "a"), Some(true));
        assert_eq!(last_match(&p(&["a", "!b"]), "a"), Some(true));
        assert_eq!(last_match(&p(&["a", "!a"]), "a"), Some(false));
    }

    #[test]
    fn branches_and_branches_ignore() {
        let mut e = ev(&["a"]);
        let w = parse("a.yml", "on:\n  push:\n    branches: [main]\njobs: {}\n").unwrap();
        e.head = None;
        // a push with no branch cannot satisfy a branch filter
        assert!(matches!(decide(&w, &e), Decision::Skip { .. }));
        let w = parse(
            "a.yml",
            "on:\n  pull_request:\n    branches-ignore: [main]\njobs: {}\n",
        )
        .unwrap();
        assert!(matches!(decide(&w, &ev(&["a"])), Decision::Skip { .. }));
        let mut other = ev(&["a"]);
        other.base = "dev".into();
        assert!(matches!(decide(&w, &other), Decision::Run { .. }));
    }

    #[test]
    fn a_workflow_with_no_triggers_says_so() {
        let w = parse("a.yml", "jobs: {}\n").unwrap();
        assert_eq!(
            decide(&w, &ev(&["a"])),
            Decision::Skip {
                reason: "only no triggers".into()
            }
        );
    }
}
