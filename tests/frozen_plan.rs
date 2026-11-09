//! Planning and executing apart: the frozen plan, its digest, and attempts that
//! do not share state.

use local_ci::{ExecContext, FrozenPlan, Observer, Quiet, RunOptions, Target};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

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

/// A repository with one workflow whose job runs `script`, changed on a branch.
fn repo(script: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(
        dir.join(".github/workflows/ci.yml"),
        format!(
            "name: ci\non: pull_request\njobs:\n  check:\n    runs-on: ubuntu-latest\n    env:\n      MODE: fast\n    steps:\n      - run: {script}\n  other:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo other\n"
        ),
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

fn target(dir: &Path, base: &str) -> Target {
    Target {
        base: Some(base.into()),
        sha: "HEAD".into(),
        all: false,
        include_disabled: true,
        workflows_dirs: vec![],
        workflows: vec![],
        jobs: vec![],
        dir: dir.to_path_buf(),
    }
}

fn plan_of(dir: &Path, base: &str, home: &Path) -> FrozenPlan {
    local_ci::build_frozen_plan(&ctx(home), &target(dir, base), &mut Quiet).unwrap()
}

#[test]
fn a_frozen_plan_survives_json_and_keeps_its_digest() {
    let (tmp, dir) = repo("echo hi");
    let fp = plan_of(&dir, "main", tmp.path());
    assert_eq!(fp.schema_version, local_ci::frozen::SCHEMA_VERSION);
    assert!(fp.digest.starts_with("sha256:"));
    assert_eq!(fp.units.len(), 2);
    assert_eq!(fp.changed_files, vec!["a.txt".to_string()]);
    assert_eq!(fp.source.base, "main");
    fp.verify().unwrap();

    let text = fp.to_canonical_json().unwrap();
    let back = FrozenPlan::from_json(&text).unwrap();
    assert_eq!(back, fp);
    assert_eq!(back.to_canonical_json().unwrap(), text);

    // a changed plan no longer matches its digest
    let tampered = text.replace("echo hi", "echo bye");
    assert!(FrozenPlan::from_json(&tampered).is_err());
    let newer = text.replace("\"schema_version\":3", "\"schema_version\":4");
    assert!(FrozenPlan::from_json(&newer).is_err());
}

#[test]
fn the_digest_follows_the_input_and_nothing_else() {
    let (tmp, dir) = repo("echo hi");
    let a = plan_of(&dir, "main", tmp.path());
    let b = plan_of(&dir, "main", tmp.path());
    assert_eq!(a.digest, b.digest);

    // a different selection is a different plan
    let mut t = target(&dir, "main");
    t.jobs = vec!["check".into()];
    let only = local_ci::build_frozen_plan(&ctx(tmp.path()), &t, &mut Quiet).unwrap();
    assert_ne!(a.digest, only.digest);
    assert_eq!(only.units.len(), 1);

    // so is a different commit
    std::fs::write(dir.join("a.txt"), "c\n").unwrap();
    git(&dir, &["commit", "-qam", "again"]);
    let c = plan_of(&dir, "main", tmp.path());
    assert_ne!(a.digest, c.digest);
    assert_ne!(a.source.sha, c.source.sha);
}

#[test]
fn clones_of_one_repository_in_different_directories_give_one_digest() {
    let (tmp, dir) = repo("echo hi");
    let one = tmp.path().join("one/clone");
    let two = tmp.path().join("deeper/two/clone");
    for to in [&one, &two] {
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        git(
            tmp.path(),
            &["clone", "-q", &dir.to_string_lossy(), &to.to_string_lossy()],
        );
    }
    let a = plan_of(&one, "origin/main", &tmp.path().join("h1"));
    let b = plan_of(&two, "origin/main", &tmp.path().join("h2"));
    assert_eq!(a.digest, b.digest);
    assert_eq!(
        a.to_canonical_json().unwrap(),
        b.to_canonical_json().unwrap()
    );
    // nothing of the host is in the plan
    let text = a.to_canonical_json().unwrap();
    for host_path in [tmp.path(), one.as_path(), two.as_path(), dir.as_path()] {
        let s = host_path.to_string_lossy();
        assert!(!text.contains(s.as_ref()), "{s} is in the plan");
    }
}

/// What a result says apart from times and paths.
fn essence(r: &local_ci::report::RunResult) -> serde_json::Value {
    let mut v = serde_json::to_value(r).unwrap();
    let o = v.as_object_mut().unwrap();
    o.remove("secs");
    o.remove("run_dir");
    let strip = |x: &mut serde_json::Value| {
        let mut stack = vec![x];
        while let Some(x) = stack.pop() {
            match x {
                serde_json::Value::Object(m) => {
                    for k in ["secs", "log", "time", "waited_secs", "copy_secs"] {
                        m.remove(k);
                    }
                    stack.extend(m.values_mut());
                }
                serde_json::Value::Array(a) => stack.extend(a.iter_mut()),
                _ => {}
            }
        }
    };
    strip(&mut v);
    v
}

fn run_options(dir: &Path, cache: &Path) -> RunOptions {
    RunOptions {
        target: target(dir, "main"),
        parallel: Some(2),
        no_canirun: true,
        no_portool: true,
        containers: 1,
        keep: false,
        job_timeout: 5.0,
        no_cache: true,
        cache_dir: cache.to_path_buf(),
    }
}

#[test]
fn executing_a_built_plan_gives_what_run_gives() {
    let (tmp, dir) = repo("echo ran");
    let c = ctx(tmp.path());
    let via_run = local_ci::run(&c, run_options(&dir, &tmp.path().join("c1")), &mut Quiet)
        .unwrap()
        .result;
    assert_eq!(via_run.state, "success", "{}", via_run.description);

    let fp = local_ci::build_frozen_plan(&c, &target(&dir, "main"), &mut Quiet).unwrap();
    let cache = tmp.path().join("c2");
    let wt = cache.join("wt/manual");
    local_ci::git::add_worktree(&c, &dir, &wt, &fp.source.sha).unwrap();
    let ws = local_ci::exec::Workspace {
        root: wt,
        repo_root: dir.clone(),
        run_dir: cache.join("runs/manual"),
        tools: cache.join("wt/manual.tools"),
        lock_file: cache.join("locks/manual.lock"),
    };
    std::fs::create_dir_all(&ws.run_dir).unwrap();
    std::fs::create_dir_all(cache.join("locks")).unwrap();
    let opts = local_ci::exec::Opts {
        parallel: 2,
        canirun: None,
        portool: None,
        containers: 1,
        job_timeout_min: 5.0,
        keep: false,
        trash: cache.join("trash"),
    };
    let via_execute = local_ci::execute_plan(&c, &fp, &ws, opts).unwrap();
    assert_eq!(essence(&via_run), essence(&via_execute));
}

struct Signal(mpsc::Sender<()>);
impl Observer for Signal {
    fn executing(&mut self) {
        let _ = self.0.send(());
    }
}

#[test]
fn cancelling_one_attempt_leaves_a_concurrent_one_running() {
    let (tmp_a, dir_a) = repo("sleep 60");
    let (tmp_b, dir_b) = repo("sleep 3; echo done");
    let a = ctx(tmp_a.path());
    let b = ctx(tmp_b.path());
    let (tx, rx) = mpsc::channel();
    let mut opts_a = run_options(&dir_a, &tmp_a.path().join("cache"));
    opts_a.target.jobs = vec!["check".into()];
    let mut opts_b = run_options(&dir_b, &tmp_b.path().join("cache"));
    opts_b.target.jobs = vec!["check".into()];
    let t0 = std::time::Instant::now();
    let (ra, rb) = std::thread::scope(|s| {
        let ha = s.spawn(|| local_ci::run(&a, opts_a, &mut Signal(tx)).unwrap());
        let hb = s.spawn(|| local_ci::run(&b, opts_b, &mut Quiet).unwrap());
        rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        a.cancel.cancel();
        (ha.join().unwrap(), hb.join().unwrap())
    });
    assert!(ra.cancelled);
    assert!(
        t0.elapsed().as_secs() < 45,
        "the cancelled run did not stop"
    );
    assert!(!rb.cancelled);
    assert!(!b.cancel.is_cancelled());
    assert_eq!(rb.result.state, "success", "{}", rb.result.description);
}

#[test]
fn the_library_does_not_read_the_process_environment() {
    // clippy's disallowed-methods (clippy.toml) says the same; this holds when
    // only `cargo test` runs.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for e in std::fs::read_dir(src).unwrap() {
        let p = e.unwrap().path();
        if p.file_name().unwrap() == "main.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap();
        for needle in ["env::var(", "env::var_os(", "env::vars(", "env::vars_os("] {
            assert!(!text.contains(needle), "{} uses {needle}", p.display());
        }
    }
}
