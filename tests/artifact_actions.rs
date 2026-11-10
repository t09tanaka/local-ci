//! `actions/upload-artifact@v4` and `actions/download-artifact@v4` within one
//! run, on fixture workflows of this file: upload then download across jobs
//! that `needs` each other, several files and globs, a missing file under each
//! policy, a second upload of a name, overwrite, matrices, paths that are
//! refused, receipts that are checked, and options that fail their workflow
//! before the run.

#![allow(clippy::disallowed_methods)] // the test builds the binary's environment

mod common;
use local_ci::artifact_io::{self, Area, Receipt, Who};
use local_ci::complete::{validate_run_result, Verdict};
use local_ci::exec::{JobResult, Opts, Outcome, Workspace};
use local_ci::report::RunResult;
use local_ci::{ExecContext, FrozenPlan, Quiet, Target};
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

fn ctx(home: &Path) -> ExecContext {
    ExecContext::new(
        [
            ("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin"),
            ("HOME", &home.to_string_lossy()),
            ("LOCAL_CI_HOME", &home.join("cache").to_string_lossy()),
        ]
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

const W1: &str = ".github/workflows/one.yml";

/// A repository whose workflow `one.yml` has these jobs; `feature` changes
/// `x/f`, which is what triggers it.
fn lab(jobs: &str) -> (common::Tmp, PathBuf) {
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::create_dir_all(dir.join("x")).unwrap();
    std::fs::write(dir.join("x/f"), "0\n").unwrap();
    std::fs::write(
        dir.join(W1),
        format!("name: w\non:\n  pull_request:\n    paths: ['x/**']\njobs:\n{jobs}"),
    )
    .unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "feature"]);
    std::fs::write(dir.join("x/f"), "1\n").unwrap();
    git(&dir, &["commit", "-qam", "change"]);
    (tmp, dir)
}

fn plan(tmp: &common::Tmp, dir: &Path) -> Result<FrozenPlan, String> {
    local_ci::build_frozen_plan(&ctx(tmp.path()), &target(dir), &mut Quiet)
        .map_err(|e| format!("{e:#}"))
}

fn run(jobs: &str) -> (common::Tmp, FrozenPlan, RunResult) {
    let (tmp, dir) = lab(jobs);
    let fp = plan(&tmp, &dir).unwrap();
    let cache = tmp.path().join("lab");
    std::fs::create_dir_all(cache.join("locks")).unwrap();
    let wt = cache.join("wt/w");
    local_ci::git::add_worktree(&ctx(tmp.path()), &dir, &wt, &fp.source.execution_commit).unwrap();
    let ws = Workspace {
        root: wt,
        repo_root: dir.clone(),
        run_dir: cache.join("runs/r1"),
        tools: cache.join("wt/w.tools"),
        lock_file: cache.join("locks/lab.lock"),
    };
    std::fs::create_dir_all(&ws.run_dir).unwrap();
    let opts = Opts {
        parallel: 2,
        canirun: None,
        portool: None,
        containers: 1,
        job_timeout_min: 5.0,
        keep: true,
        trash: cache.join("trash"),
    };
    let r = local_ci::execute_plan(&ctx(tmp.path()), &fp, &ws, opts).unwrap();
    (tmp, fp, r)
}

fn job<'a>(r: &'a RunResult, id: &str) -> &'a JobResult {
    r.units
        .iter()
        .flat_map(|u| &u.jobs)
        .find(|j| j.job == id)
        .unwrap_or_else(|| panic!("no job {id}"))
}

fn why(r: &RunResult, id: &str) -> String {
    match &job(r, id).outcome {
        Outcome::Error(e) => e.clone(),
        o => format!("{o:?}"),
    }
}

fn notes(r: &RunResult, id: &str) -> String {
    job(r, id)
        .steps
        .iter()
        .filter_map(|s| s.note.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn receipt(r: &RunResult, name: &str, v: u32) -> Receipt {
    let p = r
        .run_dir
        .join(format!("artifacts/{name}/v{v}/receipt.json"));
    serde_json::from_slice(&std::fs::read(&p).unwrap_or_else(|_| panic!("no {p:?}"))).unwrap()
}

fn paths(rc: &Receipt) -> Vec<&str> {
    rc.files.iter().map(|f| f.path.as_str()).collect()
}

const BUILD: &str = r#"
  build:
    runs-on: x
    steps:
      - run: |
          mkdir -p out/sub
          printf one > out/a.txt
          printf two > out/sub/b.txt
          printf hidden > out/.h
          printf skip > out/skip.log
      - uses: actions/upload-artifact@v4
        with:
          name: bundle
          path: out
          if-no-files-found: error
"#;

#[test]
fn an_upload_is_downloaded_by_a_job_that_needs_it() {
    let jobs = format!(
        "{BUILD}  use:\n    runs-on: x\n    needs: build\n    steps:\n      - uses: actions/download-artifact@v4\n        with:\n          name: bundle\n          path: got\n      - run: |\n          test \"$(cat got/a.txt)\" = one\n          test \"$(cat got/sub/b.txt)\" = two\n          test ! -e got/.h\n          ls out > /dev/null\n"
    );
    let (_tmp, fp, r) = run(&jobs);
    assert_eq!(r.state, "success", "{} {}", r.description, why(&r, "use"));
    assert_eq!(
        r.completeness.verdict,
        Verdict::Complete,
        "{:?}",
        r.completeness
    );
    assert!(notes(&r, "build").contains("uploaded `bundle` v1: 3 files"));
    assert!(notes(&r, "use").contains("downloaded 1 artifact (3 files) into `got`"));

    let rc = receipt(&r, "bundle", 1);
    assert_eq!(paths(&rc), ["a.txt", "skip.log", "sub/b.txt"]);
    let a = rc.files.iter().find(|f| f.path == "a.txt").unwrap();
    assert_eq!(a.size, 3);
    // sha256("one")
    assert_eq!(
        a.sha256,
        "7692c3ad3540bb803c020b3aee66cd8887123234ea0c6e7143c0add73ff431ed"
    );
    assert_eq!(rc.plan_digest, fp.digest);
    assert_eq!(rc.attempt_id, "r1");
    assert_eq!(rc.job, format!("{W1}::build"));
    assert_eq!(rc.task.as_deref(), Some(&*format!("{W1}::build")));
    // the files are fixed read-only in the run's area
    let stored = r.run_dir.join("artifacts/bundle/v1/files/a.txt");
    assert!(std::fs::metadata(stored).unwrap().permissions().readonly());
    // the plan knows what the task must publish and what the other one needs
    let t_build = fp.tasks.iter().find(|t| t.jobs == ["build"]).unwrap();
    let t_use = fp.tasks.iter().find(|t| t.jobs == ["use"]).unwrap();
    assert_eq!(t_build.verification.artifacts, ["bundle"]);
    assert_eq!(t_use.artifacts, ["bundle"]);
}

#[test]
fn several_lines_and_globs_select_files_and_name_them() {
    let jobs = r#"
  build:
    runs-on: x
    steps:
      - run: |
          mkdir -p a/sub b
          printf 1 > a/x.txt
          printf 2 > a/y.log
          printf 3 > a/sub/z.txt
          printf 4 > b/w.txt
      - uses: actions/upload-artifact@v4
        with:
          name: globbed
          path: |
            a/**/*.txt
            !a/sub
            b/w.txt
      - uses: actions/upload-artifact@v4
        with:
          name: one-dir
          path: a/*.txt
      - uses: actions/upload-artifact@v4
        with:
          name: single
          path: ./b/w.txt
"#;
    let (_tmp, _fp, r) = run(jobs);
    assert_eq!(r.state, "success", "{} {}", r.description, why(&r, "build"));
    // two lines from two directories keep their directories
    assert_eq!(paths(&receipt(&r, "globbed", 1)), ["a/x.txt", "b/w.txt"]);
    assert_eq!(paths(&receipt(&r, "one-dir", 1)), ["x.txt"]);
    assert_eq!(paths(&receipt(&r, "single", 1)), ["w.txt"]);
}

#[test]
fn a_missing_file_follows_if_no_files_found() {
    let step = |policy: &str| {
        format!("      - uses: actions/upload-artifact@v4\n        with:\n          name: nothing\n          path: nowhere/*\n{policy}")
    };
    let jobs = |policy: &str| {
        format!(
            "  a:\n    runs-on: x\n    steps:\n      - run: echo hi\n{}",
            step(policy)
        )
    };
    // warn is the default: the job passes and says so
    for p in ["", "          if-no-files-found: warn\n"] {
        let (_t, _f, r) = run(&jobs(p));
        assert_eq!(r.state, "success", "{}", r.description);
        assert!(
            notes(&r, "a").contains("warning: no files"),
            "{}",
            notes(&r, "a")
        );
        assert!(!r.run_dir.join("artifacts/nothing").exists());
    }
    let (_t, _f, r) = run(&jobs("          if-no-files-found: ignore\n"));
    assert_eq!(r.state, "success");
    assert!(notes(&r, "a").contains("nothing uploaded"));
    // error fails the step and so the run
    let (_t, fp, r) = run(&jobs("          if-no-files-found: error\n"));
    assert_ne!(r.state, "success");
    assert!(
        why(&r, "a").contains("no files were found"),
        "{}",
        why(&r, "a")
    );
    assert_eq!(fp.tasks[0].verification.artifacts, ["nothing"]);
}

#[test]
fn a_second_upload_of_a_name_is_a_conflict_unless_it_overwrites() {
    let up = |name: &str, body: &str, extra: &str| {
        format!("      - run: printf {body} > f.txt\n      - uses: actions/upload-artifact@v4\n        with:\n          name: {name}\n          path: f.txt\n{extra}")
    };
    let jobs = format!(
        "  a:\n    runs-on: x\n    steps:\n{}{}",
        up("r", "first", ""),
        up("r", "second", "")
    );
    let (_t, _f, r) = run(&jobs);
    assert_ne!(r.state, "success");
    let e = why(&r, "a");
    assert!(
        e.contains("`r`") && e.contains("already uploaded") && e.contains("overwrite"),
        "{e}"
    );
    assert_eq!(receipt(&r, "r", 1).files[0].size, 5);
    assert!(!r.run_dir.join("artifacts/r/v2").exists());

    // overwrite makes a new version and a download takes the newest
    let jobs = format!(
        "  a:\n    runs-on: x\n    steps:\n{}{}      - uses: actions/download-artifact@v4\n        with:\n          name: r\n          path: got\n      - run: test \"$(cat got/f.txt)\" = second\n",
        up("r", "first", ""),
        up("r", "second", "          overwrite: true\n")
    );
    let (_t, _f, r) = run(&jobs);
    assert_eq!(r.state, "success", "{} {}", r.description, why(&r, "a"));
    assert!(notes(&r, "a").contains("uploaded `r` v2"));
    // the older version stays
    assert_eq!(receipt(&r, "r", 1).files[0].size, 5);
    assert_eq!(receipt(&r, "r", 2).files[0].size, 6);
}

#[test]
fn a_matrix_uploads_one_artifact_per_variant_and_a_download_merges_them() {
    let jobs = r#"
  build:
    runs-on: x
    strategy:
      matrix:
        n: [1, 2]
    steps:
      - run: printf "v${{ matrix.n }}" > r${{ matrix.n }}.txt
      - uses: actions/upload-artifact@v4
        with:
          name: part-${{ matrix.n }}
          path: r${{ matrix.n }}.txt
  all:
    runs-on: x
    needs: build
    steps:
      - uses: actions/download-artifact@v4
        with:
          pattern: part-*
          merge-multiple: true
          path: merged
      - uses: actions/download-artifact@v4
        with:
          pattern: part-*
          path: apart
      - run: |
          test "$(cat merged/r1.txt)" = v1
          test "$(cat merged/r2.txt)" = v2
          test "$(cat apart/part-1/r1.txt)" = v1
          test "$(cat apart/part-2/r2.txt)" = v2
"#;
    let (_t, fp, r) = run(jobs);
    assert_eq!(r.state, "success", "{} {}", r.description, why(&r, "all"));
    assert_eq!(paths(&receipt(&r, "part-1", 1)), ["r1.txt"]);
    assert_eq!(paths(&receipt(&r, "part-2", 1)), ["r2.txt"]);
    // the name is not known before the run, so nothing is required of it
    assert!(fp.tasks.iter().all(|t| t.verification.artifacts.is_empty()));
    assert_eq!(
        r.completeness.verdict,
        Verdict::Complete,
        "{:?}",
        r.completeness
    );

    // two variants with one name are refused before the run
    let same = jobs.replace("part-${{ matrix.n }}", "part");
    let (_t, _f, r) = run(&same);
    assert_eq!(r.state, "error");
    let e = why(&r, "build");
    assert!(e.contains("does not tell the variants"), "{e}");
    assert!(e.contains(W1) && e.contains("job `build`"), "{e}");
}

#[test]
fn a_download_with_no_upload_is_an_error_naming_the_remedy() {
    let jobs = "  a:\n    runs-on: x\n    steps:\n      - uses: actions/download-artifact@v4\n        with:\n          name: nope\n";
    let (_t, _f, r) = run(jobs);
    assert_ne!(r.state, "success");
    let e = why(&r, "a");
    assert!(e.contains("`nope`") && e.contains("`needs`"), "{e}");
}

#[test]
fn a_download_does_not_leave_the_workspace() {
    // `link` points outside the workspace
    let tmp = common::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let outside = outside.canonicalize().unwrap();
    let jobs = format!(
        r#"
  a:
    runs-on: x
    env:
      OUT: {}
    steps:
      - run: printf data > f.txt
      - uses: actions/upload-artifact@v4
        with:
          name: d
          path: f.txt
      - uses: actions/download-artifact@v4
        with:
          name: d
          path: ${{{{ env.OUT }}}}
"#,
        outside.display()
    );
    // an absolute destination is refused
    let (_t, _f, r) = run(&jobs);
    assert_ne!(r.state, "success");
    assert!(why(&r, "a").contains("absolute"), "{}", why(&r, "a"));
    assert!(!outside.join("f.txt").exists());
    // so is `..`
    let (_t, _f, r) = run(&jobs.replace(&outside.display().to_string(), "../escape"));
    assert!(why(&r, "a").contains("`..`"), "{}", why(&r, "a"));
    // and a destination that goes through a symlink of the workspace
    let linked = jobs
        .replace(&outside.display().to_string(), "link/sub")
        .replace(
            "- run: printf data > f.txt",
            &format!(
                "- run: ln -s {} link && printf data > f.txt",
                outside.display()
            ),
        );
    let (_t, _f, r) = run(&linked);
    assert_ne!(r.state, "success");
    assert!(why(&r, "a").contains("symlink"), "{}", why(&r, "a"));
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    // a path that is plainly wrong is refused before the run
    for bad in ["/etc/x", "../x", "a/../../x"] {
        let (_t, _f, r) = run(&format!("  a:\n    runs-on: x\n    steps:\n      - uses: actions/download-artifact@v4\n        with:\n          name: d\n          path: {bad}\n"));
        assert_eq!(r.state, "error", "{bad}");
        assert!(r.units[0].jobs[0].steps.is_empty(), "{bad}");
        let (_t, _f, r) = run(&format!("  a:\n    runs-on: x\n    steps:\n      - uses: actions/upload-artifact@v4\n        with:\n          path: {bad}\n"));
        assert_eq!(r.state, "error", "{bad}");
        assert!(r.units[0].jobs[0].steps.is_empty(), "{bad}");
    }
}

#[test]
fn an_option_local_ci_cannot_honor_fails_its_job_before_the_run() {
    let step = |uses: &str, with: &str| {
        format!("  a:\n    runs-on: x\n    steps:\n      - run: echo before\n      - uses: {uses}\n        with:\n{with}")
    };
    for (uses, with, needle) in [
        ("actions/download-artifact@v4", "          name: d\n          run-id: 123\n", "run-id"),
        ("actions/download-artifact@v4", "          name: d\n          repository: o/r\n          github-token: ${{ github.token }}\n", "repository"),
        ("actions/download-artifact@v4", "          name: d\n          github-token: ${{ github.token }}\n", "github-token"),
        ("actions/download-artifact@v4", "          artifact-ids: 1\n", "artifact-ids"),
        ("actions/upload-artifact@v4", "          path: x\n          compression-level: 10\n", "compression-level"),
        ("actions/upload-artifact@v4", "          path: x\n          retention-days: 0\n", "retention-days"),
        ("actions/upload-artifact@v4", "          name: a/b\n          path: x\n", "not allowed"),
        ("actions/upload-artifact@v3", "          path: x\n", "@v4"),
        ("actions/upload-artifact@v5", "          path: x\n", "@v4"),
        ("actions/upload-artifact/merge@v4", "          name: x\n", "sub-actions"),
    ] {
        // the plan is made; the job fails, naming the option, workflow, job and step
        let (tmp, dir) = lab(&step(uses, with));
        plan(&tmp, &dir).unwrap();
        let (_t, _f, r) = run(&step(uses, with));
        assert_eq!(r.state, "error", "{needle}");
        let e = why(&r, "a");
        for part in [needle, W1, "job `a`", "step `"] {
            assert!(e.contains(part), "{part}: {e}");
        }
        assert!(r.units[0].jobs[0].steps.is_empty(), "nothing of the job ran");
        // and the JSON result carries it
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(needle) && json.contains("\"result\":\"error\""), "{json}");
    }
    // a third party's action of that name is not ours: it is skipped as before
    let (_t, _f, r) =
        run("  a:\n    runs-on: x\n    steps:\n      - uses: someone/upload-artifact@v4\n");
    assert_eq!(r.state, "incomplete");
}

/// Options for `local_ci::run` with the results cache on.
fn cached_opts(tmp: &common::Tmp, dir: &Path, workflows: &[&str]) -> local_ci::RunOptions {
    let mut t = target(dir);
    t.workflows = workflows.iter().map(|w| w.to_string()).collect();
    local_ci::RunOptions {
        target: t,
        parallel: Some(2),
        no_canirun: true,
        no_portool: true,
        containers: 1,
        keep: false,
        job_timeout: 5.0,
        no_cache: false,
        cache_dir: tmp.path().join("cache3"),
        source: None,
    }
}

fn run_cached(tmp: &common::Tmp, dir: &Path, workflows: &[&str]) -> RunResult {
    // two runs in one second have run directories of their own
    local_ci::run(
        &ctx(tmp.path()),
        cached_opts(tmp, dir, workflows),
        &mut Quiet,
    )
    .unwrap()
    .result
}

fn is_cached(r: &RunResult, id: &str) -> bool {
    r.units
        .iter()
        .find(|u| u.jobs.iter().any(|j| j.job == id))
        .unwrap_or_else(|| panic!("no unit of {id}"))
        .cached
        .is_some()
}

#[test]
fn a_workflow_that_cannot_be_run_fails_alone() {
    let bad = "  a:\n    runs-on: x\n    steps:\n      - uses: actions/upload-artifact@v4\n        with:\n          path: x\n          github-token: t\n  c:\n    runs-on: x\n    steps:\n      - run: echo fine\n";
    let (tmp, dir) = lab(bad);
    std::fs::write(
        dir.join(".github/workflows/two.yml"),
        "name: two\non:\n  pull_request:\n    paths: ['x/**']\njobs:\n  b:\n    runs-on: x\n    steps:\n      - run: echo ok\n",
    )
    .unwrap();
    git(&dir, &["add", ".github/workflows/two.yml"]);
    git(&dir, &["commit", "-qm", "two"]);
    // planning is not stopped
    let fp = plan(&tmp, &dir).unwrap();
    assert_eq!(fp.units.len(), 3);
    let mut o = cached_opts(&tmp, &dir, &[]);
    o.no_cache = true;
    let r = local_ci::run(&ctx(tmp.path()), o.clone(), &mut Quiet)
        .unwrap()
        .result;
    // the workflow with the option fails: every job of it, as an error
    assert_eq!(r.state, "error");
    for id in ["a", "c"] {
        let e = why(&r, id);
        assert!(e.contains("github-token") && e.contains(W1), "{id}: {e}");
    }
    // the other one runs as usual
    assert_eq!(job(&r, "b").outcome, Outcome::Pass);
    assert!(!job(&r, "b").steps.is_empty());
    // jobs a filter leaves out are not looked at
    o.target.jobs = vec!["c".into()];
    let r = local_ci::run(&ctx(tmp.path()), o.clone(), &mut Quiet)
        .unwrap()
        .result;
    assert_eq!(job(&r, "c").outcome, Outcome::Pass, "{r:?}");
    assert!(r
        .units
        .iter()
        .flat_map(|u| &u.jobs)
        .all(|j| !matches!(j.outcome, Outcome::Error(_))));
    // but one whose `if:` is decided when it runs is
    let (_t, _f, r) = run(&bad.replace(
        "  a:\n    runs-on: x\n",
        "  a:\n    if: github.event_name == 'nothing'\n    runs-on: x\n",
    ));
    assert!(why(&r, "a").contains("github-token"));
    assert_eq!(r.state, "error");
}

const SHA: &str = "ea165f8d65b6e75b540449e92b4886f43607fa02";

#[test]
fn an_action_pinned_to_a_commit_runs_as_v4_with_one_warning() {
    let jobs = format!(
        r#"
  build:
    runs-on: x
    steps:
      - run: mkdir -p out && printf one > out/a.txt
      - uses: actions/upload-artifact@{SHA} # v4.6.2
        with:
          name: bundle
          path: out
  test:
    runs-on: x
    needs: build
    steps:
      - uses: actions/download-artifact@{SHA}
        with:
          name: bundle
          path: got
      - run: test "$(cat got/a.txt)" = one
"#
    );
    let (_t, _f, r) = run(&jobs);
    assert_eq!(r.state, "success", "{r:?}");
    assert!(
        notes(&r, "build").contains("pinned to a commit"),
        "{}",
        notes(&r, "build")
    );
    assert_eq!(
        r.warnings
            .iter()
            .filter(|w| w.contains("upload-artifact"))
            .count(),
        1,
        "{:?}",
        r.warnings
    );
    // another major version is still not run, tag or not
    for v in [
        "v3",
        "v5",
        "ea165f8d65b6e75b540449e92b4886f43607fa0",
        "ea165f8d65b6e75b540449e92b4886f43607fa02a",
    ] {
        let (_t, _f, r) = run(&format!(
            "  a:\n    runs-on: x\n    steps:\n      - uses: actions/upload-artifact@{v}\n        with:\n          path: x\n"
        ));
        assert!(why(&r, "a").contains("only @v4"), "{v}: {}", why(&r, "a"));
    }
}

#[test]
fn compression_level_is_checked_and_recorded_and_changes_nothing() {
    let jobs = |level: &str| {
        format!(
            "  build:\n    runs-on: x\n    steps:\n      - run: mkdir -p out && printf one > out/a.txt\n      - uses: actions/upload-artifact@v4\n        with:\n          name: bundle\n          path: out\n          compression-level: {level}\n"
        )
    };
    let (_t, _f, plain) = run(&jobs("6").replace("          compression-level: 6\n", ""));
    let (_t, _f, r) = run(&jobs("6"));
    assert_eq!(r.state, "success", "{r:?}");
    let rc = receipt(&r, "bundle", 1);
    assert_eq!(rc.compression_level, Some(6));
    assert_eq!(receipt(&plain, "bundle", 1).compression_level, None);
    assert_eq!(rc.files, receipt(&plain, "bundle", 1).files);
    let (_t, _f, r) = run(&jobs("0"));
    assert_eq!(receipt(&r, "bundle", 1).compression_level, Some(0));
    for bad in ["10", "x", "-1"] {
        let (_t, _f, r) = run(&jobs(bad));
        let e = why(&r, "build");
        assert!(
            e.contains("compression-level") && e.contains("0 to 9"),
            "{bad}: {e}"
        );
    }
}

#[test]
fn an_always_true_condition_keeps_an_upload_required() {
    for cond in [
        "always()",
        "${{ always() }}",
        "success()",
        "!cancelled()",
        "true",
    ] {
        let jobs = BUILD.replace(
            "      - uses: actions/upload-artifact@v4\n",
            &format!("      - if: {cond}\n        uses: actions/upload-artifact@v4\n"),
        );
        let (_t, fp, r) = run(&jobs);
        assert_eq!(r.state, "success", "{cond}");
        assert_eq!(fp.tasks[0].verification.artifacts, ["bundle"], "{cond}");
        std::fs::remove_file(r.run_dir.join("artifacts/bundle/v1/receipt.json")).unwrap();
        let c = validate_run_result(&fp, &r);
        assert_eq!(c.verdict, Verdict::Invalid, "{cond}: {c:?}");
    }
    // a real condition, or `continue-on-error`, still leaves it optional
    for step in [
        "      - if: github.ref == 'refs/heads/main'\n        uses: actions/upload-artifact@v4\n",
        "      - if: always() && github.ref == 'x'\n        uses: actions/upload-artifact@v4\n",
        "      - continue-on-error: true\n        uses: actions/upload-artifact@v4\n",
    ] {
        let jobs = BUILD.replace("      - uses: actions/upload-artifact@v4\n", step);
        let (_t, fp, _r) = run(&jobs);
        assert!(fp.tasks[0].verification.artifacts.is_empty(), "{step}");
    }
}

#[test]
fn a_matrix_upload_name_must_tell_every_variant_apart() {
    let job = |name: &str, extra: &str| {
        format!(
            "  m:\n    runs-on: x\n    strategy:\n      matrix:\n        os: [a, b]\n        node: [1, 2]\n{extra}    steps:\n      - run: mkdir -p out && echo hi > out/f\n      - uses: actions/upload-artifact@v4\n        with:\n          name: {name}\n          path: out\n"
        )
    };
    // one key of two is not enough, with or without overwrite
    for (name, extra) in [
        ("r-${{ matrix.os }}", ""),
        ("r-${{ matrix.node }}", ""),
        ("r", ""),
        ("r-${{ matrix.os }}\n          overwrite: true", ""),
    ] {
        let (_t, _f, r) = run(&job(name, extra));
        assert_eq!(r.state, "error", "{name}");
        let e = why(&r, "m");
        assert!(e.contains("does not tell the variants"), "{name}: {e}");
    }
    let e = {
        let (_t, _f, r) = run(&job("r-${{ matrix.os }}", ""));
        why(&r, "m")
    };
    assert!(e.contains("matrix.node") && e.contains("m`"), "{e}");
    // every key or the index of the variant; a key `include` only adds to a
    // variant is not one to ask for
    for (name, extra) in [
        ("r-${{ matrix.os }}-${{ matrix.node }}", ""),
        ("r-${{ strategy.job-index }}", ""),
        (
            "r-${{ matrix.os }}-${{ matrix.node }}",
            "        include:\n          - os: a\n            node: 1\n            extra: z\n",
        ),
    ] {
        let (_t, _f, r) = run(&job(name, extra));
        assert_eq!(r.state, "success", "{name}: {r:?}");
    }
    // the whole matrix is JSON, which a name cannot hold: refused before the run
    for name in ["r-${{ matrix }}", "r-${{ toJSON(matrix) }}"] {
        let (_t, _f, r) = run(&job(name, ""));
        assert_eq!(r.state, "error", "{name}");
        assert!(why(&r, "m").contains("holds the whole matrix"), "{name}");
    }
}

#[test]
fn an_include_key_that_only_adds_to_a_variant_is_not_asked_of_the_name() {
    let jobs = "  m:\n    runs-on: x\n    strategy:\n      matrix:\n        include:\n          - os: a\n            experimental: true\n    steps:\n      - run: mkdir -p out && echo hi > out/f\n      - uses: actions/upload-artifact@v4\n        with:\n          name: x-${{ matrix.os }}\n          path: out\n";
    let (_t, _f, r) = run(jobs);
    assert_eq!(r.state, "success", "{r:?}");
    let jobs = "  m:\n    runs-on: x\n    strategy:\n      matrix:\n        os: [a, b]\n        include:\n          - os: a\n            experimental: true\n    steps:\n      - run: mkdir -p out && echo hi > out/f\n      - uses: actions/upload-artifact@v4\n        with:\n          name: x-${{ matrix.os }}\n          path: out\n";
    let (_t, _f, r) = run(jobs);
    assert_eq!(r.state, "success", "{r:?}");
}

fn kept_files(cache: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![cache.join("results")];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else if p.to_string_lossy().contains(".artifacts/") && p.ends_with("a.txt") {
                out.push(p);
            }
        }
    }
    out
}

const BUILD_ONLY: &str = r#"
  build:
    runs-on: x
    steps:
      - run: mkdir -p out && printf one > out/a.txt
      - uses: actions/upload-artifact@v4
        with:
          name: bundle
          path: out
          if-no-files-found: error
"#;

#[test]
fn a_unit_that_uploads_is_reused_with_its_artifacts() {
    let (tmp, dir) = lab(BUILD_ONLY);
    let first = run_cached(&tmp, &dir, &[]);
    assert_eq!(first.state, "success", "{first:?}");
    assert!(!is_cached(&first, "build"));
    let kept = kept_files(&tmp.path().join("cache3"));
    assert_eq!(kept.len(), 1, "{kept:?}");

    let second = run_cached(&tmp, &dir, &[]);
    assert_eq!(second.state, "success", "{second:?}");
    assert!(is_cached(&second, "build"));
    // the artifact is in this run's area, under this run's plan and attempt
    let (a, b) = (receipt(&first, "bundle", 1), receipt(&second, "bundle", 1));
    assert_eq!(a.restored_from, None);
    assert_eq!(b.restored_from.as_deref(), Some(a.attempt_id.as_str()));
    assert_ne!(b.attempt_id, a.attempt_id);
    assert_eq!(
        b.attempt_id,
        second.run_dir.file_name().unwrap().to_string_lossy()
    );
    assert_ne!(b.plan_digest, a.plan_digest);
    assert_eq!(b.files, a.files);
    assert_eq!(
        std::fs::read_to_string(second.run_dir.join("artifacts/bundle/v1/files/a.txt")).unwrap(),
        "one"
    );
    assert_eq!(artifact_io::published_names(&second.run_dir), ["bundle"]);

    // a file that is not what the receipt says: the unit runs again
    let f = &kept[0];
    std::fs::set_permissions(f, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    std::fs::write(f, "tampered").unwrap();
    let third = run_cached(&tmp, &dir, &[]);
    assert_eq!(third.state, "success", "{third:?}");
    assert!(!is_cached(&third, "build"));
    let c = receipt(&third, "bundle", 1);
    assert_eq!(c.restored_from, None);
    assert_eq!(
        std::fs::read_to_string(third.run_dir.join("artifacts/bundle/v1/files/a.txt")).unwrap(),
        "one"
    );
    // and what that run kept is whole again
    let fourth = run_cached(&tmp, &dir, &[]);
    assert!(is_cached(&fourth, "build"));
}

#[test]
fn a_unit_that_downloads_is_reused_only_against_the_same_artifact() {
    let (tmp, dir) = lab(BUILD_ONLY);
    std::fs::write(
        dir.join(".github/workflows/two.yml"),
        "name: two\non:\n  pull_request:\n    paths: ['x/**']\njobs:\n  check:\n    runs-on: x\n    steps:\n      - uses: actions/download-artifact@v4\n        with:\n          name: bundle\n          path: got\n      - run: test \"$(cat got/a.txt)\" = one\n",
    )
    .unwrap();
    git(&dir, &["add", ".github/workflows/two.yml"]);
    git(&dir, &["commit", "-qm", "two"]);
    // the uploader first, so the download has its artifact whatever the order
    let r1 = run_cached(&tmp, &dir, &["one.yml"]);
    assert_eq!(r1.state, "success", "{r1:?}");
    // the downloader runs, against the uploader's reused artifact
    let r2 = run_cached(&tmp, &dir, &[]);
    assert_eq!(r2.state, "success", "{r2:?}");
    assert!(is_cached(&r2, "build") && !is_cached(&r2, "check"));
    // both are reused now
    let r3 = run_cached(&tmp, &dir, &[]);
    assert!(is_cached(&r3, "build") && is_cached(&r3, "check"), "{r3:?}");
    assert_eq!(r3.state, "success", "{r3:?}");
    // the result of `check` was kept against another artifact: it runs again
    let cache = tmp.path().join("cache3/results");
    let mut changed = 0;
    for e in walk_json(&cache) {
        let text = std::fs::read_to_string(&e).unwrap();
        if text.contains("\"downloads\":[[\"bundle\"") {
            let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
            v["downloads"][0][1] = "0".repeat(64).into();
            std::fs::write(&e, serde_json::to_string(&v).unwrap()).unwrap();
            changed += 1;
        }
    }
    assert_eq!(changed, 1);
    let r4 = run_cached(&tmp, &dir, &[]);
    assert!(
        is_cached(&r4, "build") && !is_cached(&r4, "check"),
        "{r4:?}"
    );
    assert_eq!(r4.state, "success", "{r4:?}");
}

fn walk_json(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .flat_map(|p| {
            if p.is_dir() {
                walk_json(&p)
            } else if p.extension().is_some_and(|x| x == "json") && !p.ends_with("receipt.json") {
                vec![p]
            } else {
                vec![]
            }
        })
        .collect()
}

#[test]
fn a_run_with_a_required_artifact_missing_is_not_a_success() {
    let jobs = BUILD;
    let (_t, fp, r) = run(jobs);
    assert_eq!(r.state, "success");
    assert_eq!(validate_run_result(&fp, &r).verdict, Verdict::Complete);
    // the receipt is gone: the result is not the plan's
    std::fs::remove_file(r.run_dir.join("artifacts/bundle/v1/receipt.json")).unwrap();
    let c = validate_run_result(&fp, &r);
    assert_eq!(c.verdict, Verdict::Invalid);
    assert!(
        c.invalid.iter().any(|w| w.contains("artifact `bundle`")),
        "{c:?}"
    );
    let again = local_ci::report::assemble(
        &fp,
        r.run_dir.clone(),
        r.setup.clone(),
        r.units.clone(),
        0.0,
    );
    assert_eq!(again.state, "error");
    // a task result names what the task published
    let (_t, _f, ok) = run(jobs);
    assert_eq!(artifact_io::published_names(&ok.run_dir), ["bundle"]);
    assert_eq!(
        artifact_io::published_by(&ok.run_dir, &[format!("{W1}::build")]),
        ["bundle"]
    );
    assert!(artifact_io::published_by(&ok.run_dir, &[format!("{W1}::other")]).is_empty());
}

fn w(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn a_download_checks_every_file_against_the_receipt_before_placing_any() {
    let tmp = common::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("out")).unwrap();
    std::fs::write(ws.join("out/a"), "alpha").unwrap();
    std::fs::write(ws.join("out/b"), "beta").unwrap();
    let area = Area::of_run(&tmp.path().join("run"), "sha256:plan");
    let who = Who {
        job: "w::j".into(),
        display: "j".into(),
        task: None,
    };
    let note =
        artifact_io::upload(&area, &who, &ws, &w(&[("name", "n"), ("path", "out")])).unwrap();
    assert!(note.contains("2 files"));

    // an empty `with` of a download takes everything; into a fresh place
    let dest = tmp.path().join("ws2");
    std::fs::create_dir_all(&dest).unwrap();
    let got = artifact_io::download(&area, &dest, &w(&[("name", "n"), ("path", "d")])).unwrap();
    assert!(got.contains("2 files"), "{got}");
    assert_eq!(std::fs::read_to_string(dest.join("d/a")).unwrap(), "alpha");
    // placed files are writable
    assert!(!std::fs::metadata(dest.join("d/a"))
        .unwrap()
        .permissions()
        .readonly());
    // placing again replaces them
    artifact_io::download(&area, &dest, &w(&[("name", "n"), ("path", "d")])).unwrap();

    // a file that was changed afterwards is found; nothing is placed
    let stored = tmp.path().join("run/artifacts/n/v1/files/b");
    let mut perm = std::fs::metadata(&stored).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o644);
    std::fs::set_permissions(&stored, perm).unwrap();
    std::fs::write(&stored, "BETA").unwrap();
    let fresh = tmp.path().join("ws3");
    std::fs::create_dir_all(&fresh).unwrap();
    let e = artifact_io::download(&area, &fresh, &w(&[("name", "n")])).unwrap_err();
    assert!(e.contains("does not match its receipt"), "{e}");
    assert!(std::fs::read_dir(&fresh).unwrap().next().is_none());

    // a receipt that names a path outside is refused too
    std::fs::write(&stored, "beta").unwrap();
    let rp = tmp.path().join("run/artifacts/n/v1/receipt.json");
    let mut perm = std::fs::metadata(&rp).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o644);
    std::fs::set_permissions(&rp, perm).unwrap();
    let text = std::fs::read_to_string(&rp)
        .unwrap()
        .replace("\"path\": \"b\"", "\"path\": \"../../b\"");
    std::fs::write(&rp, text).unwrap();
    let e = artifact_io::download(&area, &fresh, &w(&[("name", "n")])).unwrap_err();
    assert!(e.contains("`..`"), "{e}");

    // two artifacts that would write one file are not merged silently
    let who2 = Who {
        job: "w::k".into(),
        display: "k".into(),
        task: None,
    };
    artifact_io::upload(&area, &who2, &ws, &w(&[("name", "m"), ("path", "out/a")])).unwrap();
    std::fs::write(
        &rp,
        std::fs::read_to_string(&rp)
            .unwrap()
            .replace("../../b", "b"),
    )
    .unwrap();
    let e = artifact_io::download(&area, &fresh, &w(&[("merge-multiple", "true")])).unwrap_err();
    assert!(e.contains("both write `a`"), "{e}");
    // a version without a receipt is not published
    std::fs::create_dir_all(tmp.path().join("run/artifacts/half/v1")).unwrap();
    assert_eq!(
        artifact_io::published_names(&tmp.path().join("run")),
        ["m", "n"]
    );
    // an artifact that is not there
    let e = artifact_io::download(&area, &fresh, &w(&[("name", "half")])).unwrap_err();
    assert!(e.contains("`half`"), "{e}");
    // a symlink in the way of a file is not followed
    let victim = tmp.path().join("victim");
    std::fs::write(&victim, "keep").unwrap();
    std::os::unix::fs::symlink(&victim, fresh.join("a")).unwrap();
    let e = artifact_io::download(&area, &fresh, &w(&[("name", "m")])).unwrap_err();
    assert!(e.contains("symlink"), "{e}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
}

// ---- review fixes (safety): file work on a bare workspace, no run needed ----

struct Lab {
    tmp: common::Tmp,
    ws: PathBuf,
    out: PathBuf,
    area: Area,
}

fn who() -> Who {
    Who {
        job: "w::j".into(),
        display: "j".into(),
        task: None,
    }
}

fn lab_io() -> Lab {
    let tmp = common::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let out = tmp.path().join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let area = Area::of_run(&tmp.path().join("run"), "sha256:plan");
    Lab { tmp, ws, out, area }
}

impl Lab {
    fn up(&self, name: &str, path: &str) -> Result<String, String> {
        artifact_io::upload(
            &self.area,
            &who(),
            &self.ws,
            &w(&[("name", name), ("path", path)]),
        )
    }
    fn down(&self, ws: &Path, with: &[(&str, &str)]) -> Result<String, String> {
        artifact_io::download(&self.area, ws, &w(with))
    }
    fn fresh(&self, n: &str) -> PathBuf {
        let d = self.tmp.path().join(n);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn receipt_path(&self, name: &str) -> PathBuf {
        self.tmp
            .path()
            .join(format!("run/artifacts/{name}/v1/receipt.json"))
    }
    fn edit_receipt(&self, name: &str, from: &str, to: &str) {
        let rp = self.receipt_path(name);
        let mut perm = std::fs::metadata(&rp).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o644);
        std::fs::set_permissions(&rp, perm).unwrap();
        let text = std::fs::read_to_string(&rp).unwrap();
        assert!(text.contains(from), "{text}");
        std::fs::write(&rp, text.replace(from, to)).unwrap();
    }
}

fn is_empty(d: &Path) -> bool {
    std::fs::read_dir(d).unwrap().next().is_none()
}

#[test]
fn an_upload_does_not_take_what_a_symlink_points_at() {
    let l = lab_io();
    std::fs::write(l.out.join("secret"), "s3cret").unwrap();
    std::fs::create_dir_all(l.ws.join("real")).unwrap();
    std::fs::write(l.ws.join("real/f"), "f").unwrap();
    // a symlink in the middle of the path
    std::os::unix::fs::symlink(&l.out, l.ws.join("link")).unwrap();
    let e = l.up("a", "link/secret").unwrap_err();
    assert!(e.contains("symlink"), "{e}");
    // a symlink inside a directory that is uploaded is left out
    std::fs::create_dir_all(l.ws.join("dir")).unwrap();
    std::fs::write(l.ws.join("dir/own"), "own").unwrap();
    std::os::unix::fs::symlink(l.out.join("secret"), l.ws.join("dir/leak")).unwrap();
    std::os::unix::fs::symlink(&l.out, l.ws.join("dir/leakdir")).unwrap();
    l.up("b", "dir").unwrap();
    let d = l.fresh("got");
    l.down(&d, &[("name", "b")]).unwrap();
    assert_eq!(std::fs::read_to_string(d.join("own")).unwrap(), "own");
    assert!(!d.join("leak").exists() && !d.join("leakdir").exists());
    // a symlink given as the file itself
    let e = l.up("c", "dir/leak").unwrap_err();
    assert!(e.contains("symlink") || e.contains("no files"), "{e}");
}

#[test]
fn a_name_with_path_characters_is_refused_when_it_is_expanded_too() {
    let l = lab_io();
    std::fs::write(l.ws.join("f"), "f").unwrap();
    for bad in ["../../x", "a/b", "..", "${{ none }}/../../x", "a\\b"] {
        let e = l.up(bad, "f").unwrap_err();
        assert!(
            e.contains("not allowed") || e.contains("usable"),
            "{bad}: {e}"
        );
        let d = l.fresh("d");
        let e = l.down(&d, &[("name", bad)]).unwrap_err();
        assert!(
            e.contains("not allowed") || e.contains("usable"),
            "{bad}: {e}"
        );
    }
    assert!(!l.tmp.path().join("x").exists());
    assert!(!l.tmp.path().join("run/artifacts").exists());
}

#[test]
fn a_receipt_must_be_of_the_artifact_version_and_run_that_was_asked_for() {
    let l = lab_io();
    std::fs::write(l.ws.join("f"), "f").unwrap();
    l.up("n", "f").unwrap();
    let d = l.fresh("d");
    l.down(&d, &[("name", "n")]).unwrap();
    // another run (a plan digest that is not this one's)
    let other = Area::of_run(&l.tmp.path().join("run"), "sha256:other");
    let e = artifact_io::download(&other, &d, &w(&[("name", "n")])).unwrap_err();
    assert!(e.contains("another run"), "{e}");
    // a receipt of another name, and of another version
    l.edit_receipt("n", "\"name\": \"n\"", "\"name\": \"m\"");
    let e = l.down(&d, &[("name", "n")]).unwrap_err();
    assert!(e.contains("is of the artifact `m`"), "{e}");
    l.edit_receipt("n", "\"name\": \"m\"", "\"name\": \"n\"");
    l.edit_receipt("n", "\"version\": 1", "\"version\": 7");
    let e = l.down(&d, &[("name", "n")]).unwrap_err();
    assert!(e.contains("version 7"), "{e}");
    l.edit_receipt("n", "\"version\": 7", "\"version\": 1");
    l.edit_receipt("n", "sha256:plan", "sha256:forged");
    let e = l.down(&d, &[("name", "n")]).unwrap_err();
    assert!(e.contains("another run"), "{e}");
}

#[test]
fn a_rejection_partway_leaves_nothing_placed() {
    let l = lab_io();
    std::fs::create_dir_all(l.ws.join("src/sub")).unwrap();
    std::fs::write(l.ws.join("src/a"), "a").unwrap();
    std::fs::write(l.ws.join("src/sub/b"), "b").unwrap();
    l.up("n", "src").unwrap();
    // `sub` of the destination is a symlink out of the workspace
    let d = l.fresh("d");
    std::os::unix::fs::symlink(&l.out, d.join("sub")).unwrap();
    let e = l.down(&d, &[("name", "n")]).unwrap_err();
    assert!(e.contains("symlink"), "{e}");
    assert!(
        !d.join("a").exists(),
        "`a` was placed before `sub/b` was refused"
    );
    assert!(is_empty(&l.out));
    // a directory where a file goes
    let d2 = l.fresh("d2");
    std::fs::create_dir_all(d2.join("sub/b")).unwrap();
    let e = l.down(&d2, &[("name", "n")]).unwrap_err();
    assert!(e.contains("not replaced"), "{e}");
    assert!(!d2.join("a").exists());
    // a file that does not match its receipt, after one that does
    let stored = l.tmp.path().join("run/artifacts/n/v1/files/sub/b");
    let mut perm = std::fs::metadata(&stored).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o644);
    std::fs::set_permissions(&stored, perm).unwrap();
    std::fs::write(&stored, "B").unwrap();
    let d3 = l.fresh("d3");
    assert!(l.down(&d3, &[("name", "n")]).is_err());
    assert!(is_empty(&d3));
}

#[test]
fn names_and_files_that_differ_only_in_case_are_a_conflict() {
    let l = lab_io();
    std::fs::write(l.ws.join("f"), "f").unwrap();
    l.up("Report", "f").unwrap();
    let e = l.up("report", "f").unwrap_err();
    assert!(e.contains("only in case"), "{e}");
    assert!(
        !l.tmp.path().join("run/artifacts/report/v1").exists()
            || std::fs::read_dir(l.tmp.path().join("run/artifacts"))
                .unwrap()
                .count()
                == 1
    );
    // two artifacts with files of one name in different case, merged
    std::fs::create_dir_all(l.ws.join("x")).unwrap();
    std::fs::create_dir_all(l.ws.join("y")).unwrap();
    std::fs::write(l.ws.join("x/Out.txt"), "1").unwrap();
    std::fs::write(l.ws.join("y/out.txt"), "2").unwrap();
    l.up("r1", "x/Out.txt").unwrap();
    l.up("r2", "y/out.txt").unwrap();
    let d = l.fresh("d");
    let e = l
        .down(&d, &[("merge-multiple", "true"), ("pattern", "r*")])
        .unwrap_err();
    assert!(e.contains("only in case"), "{e}");
    assert!(is_empty(&d));
}

#[test]
fn upload_and_download_read_a_workspace_prefixed_path_alike() {
    let l = lab_io();
    let ws = l.ws.to_string_lossy().into_owned();
    std::fs::create_dir_all(l.ws.join("out")).unwrap();
    std::fs::write(l.ws.join("out/a"), "a").unwrap();
    // upload by the absolute path inside the workspace, and by the root itself
    l.up("n", &format!("{ws}/out")).unwrap();
    l.up("all", &ws).unwrap();
    // download to `<workspace>/dest`, to the root, and out of the workspace
    let d = l.fresh("d");
    let dws = d.to_string_lossy().into_owned();
    l.down(&d, &[("name", "n"), ("path", &format!("{dws}/dest"))])
        .unwrap();
    assert!(d.join("dest/a").is_file());
    l.down(&d, &[("name", "n"), ("path", &dws)]).unwrap();
    assert!(d.join("a").is_file());
    let e = l
        .down(&d, &[("name", "n"), ("path", &format!("{dws}/../x"))])
        .unwrap_err();
    assert!(e.contains("`..`"), "{e}");
    let e = l.down(&d, &[("name", "n"), ("path", &ws)]).unwrap_err();
    assert!(e.contains("absolute"), "{e}");
}

#[test]
fn a_temp_file_swapped_for_a_symlink_is_not_written_through() {
    let l = lab_io();
    std::fs::write(l.ws.join("f"), "payload").unwrap();
    l.up("n", "f").unwrap();
    let d = l.fresh("d");
    std::fs::write(l.out.join("victim"), "keep").unwrap();
    std::os::unix::fs::symlink(l.out.join("victim"), d.join("f.lci-tmp")).unwrap();
    l.down(&d, &[("name", "n")]).unwrap();
    assert_eq!(std::fs::read_to_string(d.join("f")).unwrap(), "payload");
    assert_eq!(
        std::fs::read_to_string(l.out.join("victim")).unwrap(),
        "keep"
    );
}

const TWO_PACKAGE: &str = "name: two\non:\n  pull_request:\n    paths: ['x/**']\njobs:\n  package:\n    runs-on: x\n    steps:\n      - run: sleep 2\n      - uses: actions/download-artifact@v4\n        with:\n          name: bundle\n          path: got\n      - run: mkdir -p dist && cp got/a.txt dist/d.txt\n      - uses: actions/upload-artifact@v4\n        with:\n          name: dist\n          path: dist\n";

#[test]
fn a_restore_that_fails_leaves_no_artifact_of_the_units_that_run_again() {
    for overwrite in [false, true] {
        let (tmp, dir) = lab(BUILD_ONLY);
        let two = if overwrite {
            format!("{TWO_PACKAGE}          overwrite: true\n")
        } else {
            TWO_PACKAGE.to_string()
        };
        std::fs::write(dir.join(".github/workflows/two.yml"), two).unwrap();
        git(&dir, &["add", ".github/workflows/two.yml"]);
        git(&dir, &["commit", "-qm", "two"]);
        // the uploader first; `package` waits a little, so that when both run
        // together `bundle` is there when it downloads
        let first = run_cached(&tmp, &dir, &["one.yml"]);
        assert_eq!(first.state, "success", "{overwrite}: {first:?}");
        let second = run_cached(&tmp, &dir, &[]);
        assert_eq!(second.state, "success", "{overwrite}: {second:?}");
        let third = run_cached(&tmp, &dir, &[]);
        assert!(is_cached(&third, "build") && is_cached(&third, "package"));
        // the artifact `package` takes is not what its receipt says
        let kept = kept_files(&tmp.path().join("cache3"));
        assert_eq!(kept.len(), 1, "{kept:?}");
        std::fs::set_permissions(
            &kept[0],
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .unwrap();
        std::fs::write(&kept[0], "tampered").unwrap();
        // both run again, and `dist` of the restored result is not left over
        let fourth = run_cached(&tmp, &dir, &[]);
        assert_eq!(fourth.state, "success", "{overwrite}: {fourth:?}");
        assert!(!is_cached(&fourth, "build") && !is_cached(&fourth, "package"));
        assert!(!why_all(&fourth).contains("already uploaded"));
        assert!(fourth.run_dir.join("artifacts/dist/v1").exists());
        assert!(
            !fourth.run_dir.join("artifacts/dist/v2").exists(),
            "{overwrite}: a stale v1 of `dist` was left in the run's area"
        );
        assert_eq!(receipt(&fourth, "dist", 1).restored_from, None);
        let fifth = run_cached(&tmp, &dir, &[]);
        assert!(is_cached(&fifth, "build") && is_cached(&fifth, "package"));
    }
}

fn why_all(r: &RunResult) -> String {
    r.units
        .iter()
        .flat_map(|u| &u.jobs)
        .map(|j| format!("{:?}", j.outcome))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn a_regular_file_partway_along_the_download_path_places_nothing() {
    let l = lab_io();
    std::fs::create_dir_all(l.ws.join("src/sub")).unwrap();
    std::fs::write(l.ws.join("src/a"), "a").unwrap();
    std::fs::write(l.ws.join("src/sub/b"), "b").unwrap();
    l.up("n", "src").unwrap();
    let d = l.fresh("d");
    std::fs::write(d.join("sub"), "a file").unwrap();
    let e = l.down(&d, &[("name", "n")]).unwrap_err();
    assert!(e.contains("not a directory"), "{e}");
    assert!(
        !d.join("a").exists(),
        "`a` was placed before `sub/b` failed"
    );
    assert_eq!(std::fs::read_to_string(d.join("sub")).unwrap(), "a file");
}

#[test]
fn a_matrix_job_that_uploads_is_reused_with_its_artifacts() {
    let jobs = "  m:\n    runs-on: x\n    strategy:\n      matrix:\n        os: [a, b]\n    steps:\n      - run: mkdir -p out && printf one > out/a.txt\n      - uses: actions/upload-artifact@v4\n        with:\n          name: bundle-${{ matrix.os }}\n          path: out\n";
    let (tmp, dir) = lab(jobs);
    let first = run_cached(&tmp, &dir, &[]);
    assert_eq!(first.state, "success", "{first:?}");
    let second = run_cached(&tmp, &dir, &[]);
    assert!(is_cached(&second, "m"), "{second:?}");
    // both variants' uploads are in this run's area again
    assert_eq!(
        artifact_io::published_names(&second.run_dir),
        ["bundle-a", "bundle-b"]
    );
    // a matrix entry names the job by its id
    assert!(first.units[0].jobs.iter().all(|j| j.job == "m"));
}

#[test]
fn a_task_run_through_the_library_is_refused_like_a_run() {
    // an upload of a matrix whose name does not tell the variants apart
    let jobs = "  m:\n    runs-on: x\n    strategy:\n      matrix:\n        os: [a, b]\n    steps:\n      - run: mkdir -p out && echo hi > out/f\n      - uses: actions/upload-artifact@v4\n        with:\n          name: report\n          path: out\n          overwrite: true\n";
    let (tmp, dir) = lab(jobs);
    let fp = plan(&tmp, &dir).unwrap();
    let cache = tmp.path().join("lab");
    std::fs::create_dir_all(cache.join("locks")).unwrap();
    let wt = cache.join("wt/w");
    local_ci::git::add_worktree(&ctx(tmp.path()), &dir, &wt, &fp.source.execution_commit).unwrap();
    let ws = Workspace {
        root: wt.clone(),
        repo_root: dir.clone(),
        run_dir: cache.join("runs/r1"),
        tools: cache.join("wt/w.tools"),
        lock_file: cache.join("locks/lab.lock"),
    };
    std::fs::create_dir_all(&ws.run_dir).unwrap();
    assert!(!fp.tasks.is_empty());
    for task in &fp.tasks {
        let opts = Opts {
            parallel: 1,
            canirun: None,
            portool: None,
            containers: 1,
            job_timeout_min: 5.0,
            keep: true,
            trash: cache.join("trash"),
        };
        let r = local_ci::execute_task(
            &ctx(tmp.path()),
            &fp,
            &task.id,
            &local_ci::TaskInputs::default(),
            None,
            &ws,
            opts,
        )
        .unwrap();
        assert!(!r.jobs.is_empty());
        for j in &r.jobs {
            let Outcome::Error(e) = &j.outcome else {
                panic!("{:?}", j.outcome)
            };
            assert!(e.contains("does not tell the variants"), "{e}");
            assert_eq!(j.job, "m");
        }
        assert!(r
            .outcomes
            .iter()
            .all(|o| o.status == local_ci::JobStatus::Failure));
        // nothing was started: no output of the step
        assert!(!wt.join("out").exists());
    }
}

#[test]
fn a_refused_matrix_job_has_an_error_for_each_variant_in_a_complete_check() {
    let jobs = "  m:\n    runs-on: x\n    strategy:\n      matrix:\n        os: [a, b]\n    steps:\n      - run: echo hi\n      - uses: actions/upload-artifact@v4\n        with:\n          name: report\n          path: x\n";
    let (_t, fp, r) = run(jobs);
    assert_eq!(r.state, "error");
    let c = validate_run_result(&fp, &r);
    // an error the plan accounts for, not a job missing entries
    assert!(c.missing.is_empty(), "{c:?}");
    assert_eq!(c.verdict, Verdict::Complete, "{c:?}");
    let entries: Vec<_> = r.units[0].jobs.iter().collect();
    assert_eq!(entries.len(), 2);
    assert!(entries
        .iter()
        .all(|j| matches!(j.outcome, Outcome::Error(_))));
}
