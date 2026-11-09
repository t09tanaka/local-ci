//! The library entry points, called without the command-line adapter.

#![allow(clippy::disallowed_methods)] // the tests build their own commands

use local_ci::{ExecContext, Quiet, Target};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// A repository with one workflow, changed on a `feature` branch.
fn repo() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("r");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(
        dir.join(".github/workflows/ci.yml"),
        "name: ci\non: pull_request\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n",
    )
    .unwrap();
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "feature"]);
    std::fs::write(dir.join("a.txt"), "b\n").unwrap();
    git(&dir, &["commit", "-qam", "change"]);
    (tmp, dir)
}

/// A context with a fixed environment of its own.
fn ctx() -> ExecContext {
    ExecContext::new(
        [("PATH", "/usr/bin:/bin"), ("HOME", "/nonexistent")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

fn target(dir: &Path) -> Target {
    Target {
        base: Some("main".into()),
        sha: "HEAD".into(),
        all: false,
        include_disabled: true,
        workflows_dirs: vec![],
        workflows: vec![],
        jobs: vec![],
        dir: dir.to_path_buf(),
    }
}

#[test]
fn resolve_plans_the_triggered_job_without_touching_the_process_cwd() {
    let (_tmp, dir) = repo();
    let before = std::env::current_dir().unwrap();
    let (repo, plan) = local_ci::resolve(&ctx(), &target(&dir), &mut Quiet).unwrap();
    assert_eq!(std::env::current_dir().unwrap(), before);
    assert_eq!(repo.base, "main");
    assert_eq!(repo.changed, vec!["a.txt".to_string()]);
    assert_eq!(plan.units.len(), 1);
    assert_eq!(plan.units[0].jobs, vec!["lint".to_string()]);
}

#[test]
fn resolve_keeps_only_the_named_jobs() {
    let (_tmp, dir) = repo();
    let mut t = target(&dir);
    t.jobs = vec!["nope".into()];
    let (_, plan) = local_ci::resolve(&ctx(), &t, &mut Quiet).unwrap();
    assert!(plan.units.is_empty());
}

#[test]
fn resolve_fails_outside_a_repository() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(local_ci::resolve(&ctx(), &target(tmp.path()), &mut Quiet).is_err());
}

#[test]
fn status_without_a_recorded_run_is_an_error() {
    let (tmp, dir) = repo();
    let err = local_ci::status(&ctx(), &dir, "HEAD", &tmp.path().join("cache")).unwrap_err();
    assert!(
        format!("{err:#}").contains("no local-ci run recorded"),
        "{err:#}"
    );
}
