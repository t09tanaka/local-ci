//! `actions/upload-artifact@v4` and `actions/download-artifact@v4` within one
//! run, on fixture workflows of this file: upload then download across jobs
//! that `needs` each other, several files and globs, a missing file under each
//! policy, a second upload of a name, overwrite, matrices, paths that are
//! refused, receipts that are checked, and options that are errors before the
//! run.

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
    let (tmp, dir) = lab(&same);
    let e = plan(&tmp, &dir).unwrap_err();
    assert!(e.contains("every variant of the matrix"), "{e}");
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
        let (tmp, dir) = lab(&format!("  a:\n    runs-on: x\n    steps:\n      - uses: actions/download-artifact@v4\n        with:\n          name: d\n          path: {bad}\n"));
        assert!(plan(&tmp, &dir).is_err(), "{bad}");
        let (tmp, dir) = lab(&format!("  a:\n    runs-on: x\n    steps:\n      - uses: actions/upload-artifact@v4\n        with:\n          path: {bad}\n"));
        assert!(plan(&tmp, &dir).is_err(), "{bad}");
    }
}

#[test]
fn an_option_local_ci_cannot_honor_is_an_error_before_the_run() {
    let step = |uses: &str, with: &str| {
        format!("  a:\n    runs-on: x\n    steps:\n      - run: echo before\n      - uses: {uses}\n        with:\n{with}")
    };
    for (uses, with, needle) in [
        ("actions/download-artifact@v4", "          name: d\n          run-id: 123\n", "run-id"),
        ("actions/download-artifact@v4", "          name: d\n          repository: o/r\n          github-token: ${{ github.token }}\n", "repository"),
        ("actions/download-artifact@v4", "          name: d\n          github-token: ${{ github.token }}\n", "github-token"),
        ("actions/download-artifact@v4", "          artifact-ids: 1\n", "artifact-ids"),
        ("actions/upload-artifact@v4", "          path: x\n          compression-level: 0\n", "compression-level"),
        ("actions/upload-artifact@v4", "          path: x\n          retention-days: 0\n", "retention-days"),
        ("actions/upload-artifact@v4", "          name: a/b\n          path: x\n", "not allowed"),
        ("actions/upload-artifact@v3", "          path: x\n", "@v4"),
        ("actions/upload-artifact/merge@v4", "          name: x\n", "sub-actions"),
    ] {
        let (tmp, dir) = lab(&step(uses, with));
        let e = plan(&tmp, &dir).unwrap_err();
        assert!(e.contains(needle), "{needle}: {e}");
        assert!(e.contains(W1), "{e}");
        // and `run` stops before it makes a worktree or a run directory
        let opts = local_ci::RunOptions {
            target: target(&dir),
            parallel: Some(1),
            no_canirun: true,
            no_portool: true,
            containers: 1,
            keep: false,
            job_timeout: 5.0,
            no_cache: true,
            cache_dir: tmp.path().join("cache2"),
            source: None,
        };
        let e = local_ci::run(&ctx(tmp.path()), opts, &mut Quiet).err().expect("an error");
        assert!(format!("{e:#}").contains(needle));
        assert!(!tmp.path().join("cache2/wt").exists());
        assert!(!tmp.path().join("cache2/runs").exists());
    }
    // a third party's action of that name is not ours: it is skipped as before
    let (_t, _f, r) =
        run("  a:\n    runs-on: x\n    steps:\n      - uses: someone/upload-artifact@v4\n");
    assert_eq!(r.state, "incomplete");
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
