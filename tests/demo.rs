//! End-to-end runs against `examples/demo`, copied into a fresh git repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args([
            "-c",
            "user.email=demo@example.com",
            "-c",
            "user.name=demo",
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

struct Demo {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    home: PathBuf,
}

/// The demo on `main`, plus a `feature` branch that changes `app/`.
fn demo(change: &str) -> Demo {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("demo");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo"),
        &repo,
    );
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["checkout", "-qb", "feature"]);
    let f = repo.join(change);
    let mut text = std::fs::read_to_string(&f).unwrap();
    text.push_str("# changed\n");
    std::fs::write(&f, text).unwrap();
    git(&repo, &["commit", "-qam", "change"]);
    let home = tmp.path().join("home");
    Demo {
        _tmp: tmp,
        repo,
        home,
    }
}

fn local_ci(d: &Demo, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_local-ci"))
        .args(args)
        .current_dir(&d.repo)
        .env("LOCAL_CI_HOME", &d.home)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn plan_lists_units_and_setup() {
    let d = demo("app/lib.sh");
    let o = local_ci(&d, &["plan"]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("(1 file changed, triggered by the changes)"),
        "{out}"
    );
    assert!(out.contains("setup (once): app$ ./install.sh"), "{out}");
    assert!(
        out.contains("unit 1  ci.yml: lint → discover → test → deploy"),
        "{out}"
    );
    assert!(
        out.contains("integration (services: cache)  [exclusive]"),
        "{out}"
    );
}

#[test]
fn nothing_runs_when_paths_do_not_match() {
    let d = demo("README.md");
    let o = local_ci(&d, &["run", "--no-status", "--no-canirun"]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("not triggered (paths not matched: ci.yml)"),
        "{out}"
    );
    assert!(out.contains("result: success · pass 0"), "{out}");
    // --all runs it anyway
    let o = local_ci(&d, &["plan", "--all"]);
    let out = stdout(&o);
    assert!(out.contains("all checks)"), "{out}");
    assert!(out.contains("unit 1  ci.yml: lint"), "{out}");
}

#[test]
fn runs_jobs_with_dynamic_matrix() {
    let d = demo("app/lib.sh");
    let o = local_ci(
        &d,
        &[
            "run",
            "--no-status",
            "--no-canirun",
            "-j",
            "test",
            "-j",
            "deploy",
        ],
    );
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    for line in [
        "PASS   ci.yml › lint",
        "PASS   ci.yml › discover",
        "PASS   ci.yml › test (1)",
        "PASS   ci.yml › test (2)",
        "SKIP   deploy (if: false)",
    ] {
        assert!(out.contains(line), "missing `{line}` in\n{out}");
    }
    // the run left nothing behind in the repository
    let wt = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&d.repo)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&wt.stdout).lines().count(), 1);
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&d.repo)
        .output()
        .unwrap();
    assert!(status.stdout.is_empty());
}

#[test]
fn reports_the_failing_step_with_its_log() {
    let d = demo("app/lib.sh");
    std::fs::write(d.repo.join("app/lib.sh"), "add() { echo 0; }\n").unwrap();
    git(&d.repo, &["commit", "-qam", "break add"]);
    let o = local_ci(&d, &["run", "--no-status", "--no-canirun", "-j", "test"]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(
        out.contains("FAIL   ci.yml › test (1) › Unit tests (shard 1) (exit 1)"),
        "{out}"
    );
    assert!(
        out.contains("── ci.yml › test (1) › Unit tests (shard 1) (exit 1)"),
        "{out}"
    );
    assert!(out.contains("log: "), "{out}");
    assert!(out.contains("result: failure"), "{out}");
    // fail-fast: false keeps the other shard running
    assert!(out.contains("ci.yml › test (2)"), "{out}");
}

#[test]
fn parallel_units_do_not_see_each_others_writes() {
    let d = demo("app/lib.sh");
    // `tamper` edits a tracked file for a while and puts it back; `observe`
    // reads that file meanwhile. On GitHub each job has its own checkout.
    std::fs::write(
        d.repo.join(".github/workflows/isolation.yml"),
        r#"name: Isolation
on: pull_request
jobs:
  tamper:
    runs-on: ubuntu-latest
    steps:
      - run: |
          echo 'echo tampered' >> app/lib.sh
          sleep 4
          git checkout -- app/lib.sh
  observe:
    runs-on: ubuntu-latest
    steps:
      - run: |
          sleep 2
          git diff --exit-code -- app/lib.sh
          ! grep -q tampered app/lib.sh
"#,
    )
    .unwrap();
    git(&d.repo, &["add", "."]);
    git(&d.repo, &["commit", "-qm", "isolation"]);
    let o = local_ci(&d, &["plan", "-j", "tamper", "-j", "observe"]);
    let out = stdout(&o);
    assert!(out.contains("unit 1  isolation.yml: tamper"), "{out}");
    assert!(out.contains("unit 2  isolation.yml: observe"), "{out}");

    let o = local_ci(
        &d,
        &[
            "run",
            "--no-status",
            "--no-canirun",
            "--parallel",
            "2",
            "-j",
            "tamper",
            "-j",
            "observe",
        ],
    );
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("PASS   isolation.yml › observe"), "{out}");
    let wt = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&d.repo)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&wt.stdout).lines().count(), 1);
}

#[test]
fn json_result_is_saved() {
    let d = demo("app/lib.sh");
    let o = local_ci(
        &d,
        &["run", "--no-status", "--no-canirun", "--json", "-j", "lint"],
    );
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["state"], "success");
    assert_eq!(v["units"][0]["jobs"][0]["result"], "pass");
    let saved = Path::new(v["run_dir"].as_str().unwrap()).join("result.json");
    assert!(saved.is_file());
}

#[test]
fn services_get_their_own_port() {
    if !docker_available() {
        eprintln!("docker is not available; skipping");
        return;
    }
    let d = demo("app/lib.sh");
    let o = local_ci(
        &d,
        &["run", "--no-status", "--no-canirun", "-j", "integration"],
    );
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("PASS   ci.yml › integration"), "{out}");
}

/// `gh` that reports ci.yml as disabled on GitHub.
fn fake_gh_disabled(d: &Demo) -> String {
    let bin = d.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\nprintf '.github/workflows/ci.yml\\tdisabled_manually\\n'\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &d.repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/demo/demo.git",
        ],
    );
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}

fn local_ci_path(d: &Demo, path: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_local-ci"))
        .args(args)
        .current_dir(&d.repo)
        .env("LOCAL_CI_HOME", &d.home)
        .env("PATH", path)
        .output()
        .unwrap()
}

#[test]
fn include_disabled_runs_disabled_workflows_with_paths() {
    let d = demo("app/lib.sh");
    let path = fake_gh_disabled(&d);
    let base = ["plan", "--base", "main"];
    let o = local_ci_path(&d, &path, &base);
    let out = stdout(&o);
    assert!(
        out.contains("not triggered (disabled manually on GitHub: ci.yml)"),
        "{out}"
    );
    assert!(!out.contains("unit 1"), "{out}");
    let o = local_ci_path(&d, &path, &[&base[..], &["--include-disabled"]].concat());
    let out = stdout(&o);
    assert!(out.contains("triggered by the changes)"), "{out}");
    assert!(out.contains("unit 1  ci.yml: lint"), "{out}");

    // paths still apply
    let d = demo("README.md");
    let path = fake_gh_disabled(&d);
    let o = local_ci_path(&d, &path, &[&base[..], &["--include-disabled"]].concat());
    let out = stdout(&o);
    assert!(
        out.contains("not triggered (paths not matched: ci.yml)"),
        "{out}"
    );
    assert!(!out.contains("unit 1"), "{out}");
}

#[test]
fn workflows_dir_adds_workflows_github_does_not_run() {
    let d = demo("app/lib.sh");
    // Same file name as .github/workflows/ci.yml: the output names it by path.
    std::fs::create_dir_all(d.repo.join("ci/local")).unwrap();
    std::fs::write(
        d.repo.join("ci/local/ci.yml"),
        r#"on: pull_request
jobs:
  extra:
    runs-on: ubuntu-latest
    steps:
      - run: echo extra
"#,
    )
    .unwrap();
    git(&d.repo, &["add", "."]);
    git(&d.repo, &["commit", "-qm", "local-only workflow"]);

    let out = stdout(&local_ci(&d, &["plan"]));
    assert!(!out.contains("ci/local"), "{out}");

    let o = local_ci(&d, &["plan", "--workflows-dir", "./ci/local/"]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("  ci.yml: lint"), "{out}");
    assert!(out.contains("  ci/local/ci.yml: extra"), "{out}");

    let o = local_ci(
        &d,
        &[
            "run",
            "--no-status",
            "--no-canirun",
            "--workflows-dir",
            "ci/local",
            "-w",
            "ci/local/ci.yml",
        ],
    );
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("PASS   ci/local/ci.yml › extra"), "{out}");
}

#[test]
fn workflows_dir_must_be_committed() {
    let d = demo("app/lib.sh");
    std::fs::create_dir_all(d.repo.join("ci/local")).unwrap();
    std::fs::write(d.repo.join("ci/local/ci.yml"), "on: pull_request\n").unwrap();
    let o = local_ci(&d, &["plan", "--workflows-dir", "ci/local"]);
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("commit them first"), "{err}");
}

/// The demo with its workflow moved from `.github/workflows` to `.ci/workflows`
/// and a `.local-ci.toml` pointing there, committed on `feature`.
fn moved_to_ci_workflows(config: &str) -> Demo {
    let d = demo("app/lib.sh");
    std::fs::create_dir_all(d.repo.join(".ci")).unwrap();
    git(&d.repo, &["mv", ".github/workflows", ".ci/workflows"]);
    std::fs::write(d.repo.join(".local-ci.toml"), config).unwrap();
    git(&d.repo, &["add", ".local-ci.toml"]);
    git(&d.repo, &["commit", "-qm", "move the workflows"]);
    d
}

#[test]
fn config_replaces_the_workflows_dir() {
    let d = moved_to_ci_workflows("workflows-dirs = [\".ci/workflows\"]\n");
    assert!(!d.repo.join(".github/workflows").exists());
    let o = local_ci(&d, &["plan"]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}");
    // named without the configured directory, as `.github/workflows/` was
    assert!(
        out.contains("unit 1  ci.yml: lint → discover → test → deploy"),
        "{out}"
    );
    assert!(!out.contains(".ci/workflows"), "{out}");

    let o = local_ci(&d, &["run", "--no-status", "--no-canirun", "-j", "lint"]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("PASS   ci.yml › lint"), "{out}");
}

#[test]
fn config_replaces_github_workflows_instead_of_adding_to_it() {
    let d = demo("app/lib.sh");
    std::fs::create_dir_all(d.repo.join(".ci/workflows")).unwrap();
    std::fs::write(
        d.repo.join(".ci/workflows/other.yml"),
        "on: pull_request\njobs:\n  other:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo other\n",
    )
    .unwrap();
    std::fs::write(
        d.repo.join(".local-ci.toml"),
        "workflows-dirs = [\".ci/workflows\"]\n",
    )
    .unwrap();
    git(&d.repo, &["add", "."]);
    git(&d.repo, &["commit", "-qm", "configure"]);
    let o = local_ci(&d, &["plan"]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("unit 1  other.yml: other"), "{out}");
    assert!(!out.contains("ci.yml"), "{out}");
}

#[test]
fn config_is_read_from_the_commit() {
    // Written but not committed: the commit has no `.local-ci.toml`, so the
    // workflows still come from `.github/workflows`.
    let d = demo("app/lib.sh");
    std::fs::write(
        d.repo.join(".local-ci.toml"),
        "workflows-dirs = [\".ci/workflows\"]\n",
    )
    .unwrap();
    let o = local_ci(&d, &["plan"]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("unit 1  ci.yml: lint"), "{out}");
}

#[test]
fn config_and_workflows_dir_add_up() {
    let d = moved_to_ci_workflows("workflows-dirs = [\".ci/workflows\"]\n");
    std::fs::create_dir_all(d.repo.join("ci/local")).unwrap();
    std::fs::write(
        d.repo.join("ci/local/ci.yml"),
        "on: pull_request\njobs:\n  extra:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo extra\n",
    )
    .unwrap();
    git(&d.repo, &["add", "."]);
    git(&d.repo, &["commit", "-qm", "local-only workflow"]);
    let o = local_ci(&d, &["plan", "--workflows-dir", "ci/local"]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("  ci.yml: lint"), "{out}");
    assert!(out.contains("  ci/local/ci.yml: extra"), "{out}");
    // naming a configured directory again reads it once
    let o = local_ci(&d, &["plan", "--workflows-dir", ".ci/workflows/"]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}");
    assert_eq!(out.matches("ci.yml: lint").count(), 1, "{out}");
}

#[test]
fn bad_config_is_an_error() {
    for (config, msg) in [
        (
            "workflows-dirs = [\"/etc/workflows\"]\n",
            "relative to the repository root",
        ),
        ("workflows-dir = [\".ci/workflows\"]\n", "unknown field"),
        (
            "workflows-dirs = [\".ci/workflows\", \"ci/none\"]\n",
            ".local-ci.toml: workflows-dirs ci/none: no .yml / .yaml files there",
        ),
    ] {
        let d = moved_to_ci_workflows(config);
        let o = local_ci(&d, &["plan"]);
        assert_eq!(o.status.code(), Some(64), "{config}");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains(".local-ci.toml"), "{config}: {err}");
        assert!(err.contains(msg), "{config}: {err}");
    }
}
