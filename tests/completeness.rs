//! Is a run's result all of what the plan expected: missing, duplicate and
//! unexpected results, runs with nothing to do, results reused from the cache,
//! runs that left out what cannot be reproduced, a task that cannot be run,
//! the tree digest in a receipt and `ignore-env` values that differ.

#![allow(clippy::disallowed_methods)] // the test builds the binary's environment

use local_ci::complete::{validate_run_result, verify_task_result, Verdict};
use local_ci::exec::{Opts, Outcome, Workspace};
use local_ci::report::{self, RunResult};
use local_ci::setup::{derive_setup_recipe, SetupRecipe, TemplateContext};
use local_ci::{ExecContext, FrozenPlan, Quiet, RunOptions, Target, TaskInputs};
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
const W2: &str = ".github/workflows/two.yml";

fn wf(paths: &str, jobs: &str) -> String {
    format!("name: w\non:\n  pull_request:\n    paths: ['{paths}']\njobs:\n{jobs}")
}

const PLAIN: &str = "  a:\n    runs-on: x\n    steps:\n      - run: echo a\n      - run: echo again\n  m:\n    runs-on: x\n    strategy:\n      matrix:\n        n: [1, 2]\n    steps:\n      - run: echo ${{ matrix.n }}\n";

/// A repository with these files; `main` has the first version, `feature` one
/// change under `x/` and `y/`.
fn lab(files: &[(&str, String)]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    for d in ["x", "y"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
        std::fs::write(dir.join(d).join("f"), "0\n").unwrap();
    }
    std::fs::write(
        dir.join("install.sh"),
        "echo installed > installed.marker\n",
    )
    .unwrap();
    std::fs::write(dir.join("node.sh"), "mkdir -p node_modules/p\nprintf one > node_modules/p/a.js\nprintf two > node_modules/p/b.js\n").unwrap();
    for (path, text) in files {
        std::fs::write(dir.join(path), text).unwrap();
    }
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "feature"]);
    for d in ["x", "y"] {
        std::fs::write(dir.join(d).join("f"), "1\n").unwrap();
    }
    git(&dir, &["commit", "-qam", "change"]);
    (tmp, dir)
}

fn two_workflows() -> Vec<(&'static str, String)> {
    vec![(W1, wf("x/**", PLAIN)), (W2, wf("y/**", PLAIN))]
}

fn plan(tmp: &tempfile::TempDir, dir: &Path) -> FrozenPlan {
    local_ci::build_frozen_plan(&ctx(tmp.path()), &target(dir), &mut Quiet).unwrap()
}

fn opts(cache: &Path) -> Opts {
    Opts {
        parallel: 2,
        canirun: None,
        portool: None,
        containers: 1,
        job_timeout_min: 5.0,
        keep: true,
        trash: cache.join("trash"),
    }
}

fn workspace(tmp: &tempfile::TempDir, dir: &Path, fp: &FrozenPlan, name: &str) -> Workspace {
    let cache = tmp.path().join("lab");
    std::fs::create_dir_all(cache.join("locks")).unwrap();
    let wt = cache.join("wt").join(name);
    local_ci::git::add_worktree(&ctx(tmp.path()), dir, &wt, &fp.source.sha).unwrap();
    let ws = Workspace {
        root: wt,
        repo_root: dir.to_path_buf(),
        run_dir: cache.join("runs").join(name),
        tools: cache.join("wt").join(format!("{name}.tools")),
        lock_file: cache.join("locks/lab.lock"),
    };
    std::fs::create_dir_all(&ws.run_dir).unwrap();
    ws
}

fn execute(tmp: &tempfile::TempDir, dir: &Path, fp: &FrozenPlan, name: &str) -> RunResult {
    let ws = workspace(tmp, dir, fp, name);
    local_ci::execute_plan(&ctx(tmp.path()), fp, &ws, opts(&tmp.path().join("lab"))).unwrap()
}

fn reassemble(fp: &FrozenPlan, r: &RunResult) -> RunResult {
    report::assemble(fp, r.run_dir.clone(), r.setup.clone(), r.units.clone(), 0.0)
}

// ---- the result is all of the plan ----

#[test]
fn a_run_that_did_everything_is_complete_and_a_success() {
    let (tmp, dir) = lab(&two_workflows());
    let fp = plan(&tmp, &dir);
    let r = execute(&tmp, &dir, &fp, "full");
    let c = &r.completeness;
    assert_eq!(c.verdict, Verdict::Complete, "{c:?}");
    assert_eq!(r.state, "success", "{}", r.description);
    assert_eq!(c.expected_units, fp.units.len());
    assert!(fp.units.len() >= 2);
    // a, and m twice, in each of two workflows
    assert_eq!(c.expected_jobs, 6);
    assert_eq!(report::github_state(&r.state), "success");
}

#[test]
fn a31_a_missing_empty_or_doubled_result_is_not_a_success() {
    let (tmp, dir) = lab(&two_workflows());
    let fp = plan(&tmp, &dir);
    let ok = execute(&tmp, &dir, &fp, "ok");
    assert_eq!(ok.state, "success");

    // a unit's result is gone
    let mut r = ok.clone();
    r.units.pop();
    let c = validate_run_result(&fp, &r);
    assert_eq!(c.verdict, Verdict::Invalid);
    assert_eq!(c.missing.len(), 1, "{c:?}");
    assert_eq!(reassemble(&fp, &r).state, "error");

    // no result at all
    let mut r = ok.clone();
    r.units.clear();
    let c = validate_run_result(&fp, &r);
    assert_eq!(c.verdict, Verdict::Invalid);
    assert_eq!(c.missing.len(), fp.units.len());
    let again = reassemble(&fp, &r);
    assert_eq!(again.state, "error");
    assert_ne!(report::github_state(&again.state), "success");

    // a unit twice
    let mut r = ok.clone();
    r.units.push(ok.units[0].clone());
    let c = validate_run_result(&fp, &r);
    assert_eq!(c.verdict, Verdict::Invalid);
    assert_eq!(c.duplicate.len(), 1, "{c:?}");

    // a matrix entry gone
    let mut r = ok.clone();
    let mu = r
        .units
        .iter()
        .position(|u| u.jobs.iter().any(|j| j.job == "m"))
        .expect("a unit with the matrix job");
    let at = r.units[mu].jobs.iter().position(|j| j.job == "m").unwrap();
    r.units[mu].jobs.remove(at);
    let c = validate_run_result(&fp, &r);
    assert_eq!(c.verdict, Verdict::Invalid);
    assert!(c.missing.iter().any(|m| m.contains("matrix")), "{c:?}");

    // a whole job gone
    let mut r = ok.clone();
    let au = r
        .units
        .iter()
        .position(|u| u.jobs.iter().any(|j| j.job == "a"))
        .unwrap();
    r.units[au].jobs.retain(|j| j.job != "a");
    let c = validate_run_result(&fp, &r);
    assert!(c.missing.iter().any(|m| m.contains("job `a`")), "{c:?}");

    // a job the plan does not have, and a unit it does not have
    let mut r = ok.clone();
    let mut extra = r.units[0].jobs[0].clone();
    extra.job = "ghost".into();
    r.units[0].jobs.push(extra);
    let mut stray = ok.units[0].clone();
    stray.unit = Some(99);
    r.units.push(stray);
    let c = validate_run_result(&fp, &r);
    assert_eq!(c.unexpected.len(), 2, "{c:?}");
    assert_eq!(c.verdict, Verdict::Invalid);

    // a result that says nothing: a skip without a reason, a job that passed
    // with a step missing
    let mut r = ok.clone();
    r.units[0].jobs[0].outcome = Outcome::Skip(String::new());
    let c = validate_run_result(&fp, &r);
    assert!(
        c.invalid.iter().any(|m| m.contains("without a reason")),
        "{c:?}"
    );
    let mut r = ok.clone();
    let a = r.units[0].jobs.iter_mut().find(|j| j.job == "a").unwrap();
    a.steps.pop();
    let c = validate_run_result(&fp, &r);
    assert!(
        c.invalid.iter().any(|m| m.contains("steps accounted")),
        "{c:?}"
    );

    // a failure stays a failure however much is missing
    let mut r = ok.clone();
    r.units[0].jobs[0].outcome = Outcome::Fail;
    r.units.pop();
    assert_eq!(reassemble(&fp, &r).state, "failure");
}

#[test]
fn a_plan_with_nothing_to_run_is_no_work_not_a_success() {
    let (tmp, dir) = lab(&[(W1, wf("nowhere/**", PLAIN))]);
    let fp = plan(&tmp, &dir);
    assert!(fp.units.is_empty());
    let r = execute(&tmp, &dir, &fp, "none");
    assert_eq!(r.completeness.verdict, Verdict::NoWork);
    assert_eq!(r.state, "no_work");
    assert!(r.description.starts_with("no_work"), "{}", r.description);
    // the status a required check needs, as before
    assert_eq!(report::github_state(&r.state), "success");
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["completeness"]["verdict"], "no_work");
    assert_eq!(json["state"], "no_work");
    // a result in a plan that has no unit is not no_work
    let mut stray = r.clone();
    let (tmp2, dir2) = lab(&two_workflows());
    let other = execute(&tmp2, &dir2, &plan(&tmp2, &dir2), "x");
    stray.units = other.units;
    assert_eq!(validate_run_result(&fp, &stray).verdict, Verdict::Invalid);
}

fn run_opts(dir: &Path, cache: &Path) -> RunOptions {
    RunOptions {
        target: target(dir),
        parallel: Some(2),
        no_canirun: true,
        no_portool: true,
        containers: 1,
        keep: false,
        job_timeout: 5.0,
        no_cache: false,
        cache_dir: cache.to_path_buf(),
    }
}

#[test]
fn a_run_with_units_from_the_cache_is_checked_against_the_whole_plan() {
    let (tmp, dir) = lab(&two_workflows());
    let c = ctx(tmp.path());
    let cache = tmp.path().join("c");
    let first = local_ci::run(&c, run_opts(&dir, &cache), &mut Quiet)
        .unwrap()
        .result;
    assert_eq!(first.state, "success", "{}", first.description);
    assert_eq!(first.completeness.from_cache, 0);

    // only y/ changes: the unit of one.yml is reused, the one of two.yml runs
    std::fs::write(dir.join("y/f"), "2\n").unwrap();
    git(&dir, &["commit", "-qam", "again"]);
    let second = local_ci::run(&c, run_opts(&dir, &cache), &mut Quiet)
        .unwrap()
        .result;
    let cm = &second.completeness;
    assert_eq!(second.state, "success", "{} {cm:?}", second.description);
    assert_eq!(cm.verdict, Verdict::Complete);
    // a and m of each workflow are units of their own
    assert_eq!(cm.expected_units, 4);
    assert_eq!(cm.from_cache, 2, "{cm:?}");
    assert_eq!(
        second.units.iter().filter(|u| u.cached.is_some()).count(),
        2
    );
    assert_eq!(second.units.len(), 4);
    let mut ran: Vec<_> = second.units.iter().filter_map(|u| u.unit).collect();
    ran.sort();
    assert_eq!(ran, vec![0, 1, 2, 3]);

    // the part that ran is complete against the plan of its own, which is not
    // the plan of the run: it lacks the unit that came from the cache
    let full = local_ci::build_frozen_plan(&c, &target(&dir), &mut Quiet).unwrap();
    let mut only_ran = second.clone();
    only_ran.units.retain(|u| u.cached.is_none());
    let against_whole = validate_run_result(&full, &only_ran);
    assert_eq!(against_whole.verdict, Verdict::Invalid);
    assert_eq!(against_whole.missing.len(), 2);

    // everything from the cache
    let third = local_ci::run(&c, run_opts(&dir, &cache), &mut Quiet)
        .unwrap()
        .result;
    assert_eq!(third.state, "success");
    assert_eq!(third.completeness.from_cache, 4);
}

#[test]
fn skipping_what_cannot_be_reproduced_is_incomplete_not_a_pass() {
    let jobs = "  a:\n    runs-on: x\n    steps:\n      - run: echo a\n      - uses: some-org/unknown-action@v1\n";
    let (tmp, dir) = lab(&[(W1, wf("x/**", jobs))]);
    let fp = plan(&tmp, &dir);
    let r = execute(&tmp, &dir, &fp, "inc");
    assert_eq!(r.completeness.verdict, Verdict::Incomplete);
    assert_eq!(r.state, "incomplete", "{}", r.description);
    assert_eq!(r.completeness.incomplete.len(), 1);
    assert!(r.completeness.incomplete[0].contains("unknown-action"));
    // never posted as a success
    assert_eq!(report::github_state(&r.state), "error");
    assert!(r.description.contains("incomplete:"), "{}", r.description);

    // a skipped artifact step and a step that needs a secret too; a checkout
    // or a cache is not a gap
    let jobs = "  a:\n    runs-on: x\n    steps:\n      - uses: actions/checkout@v4\n      - uses: actions/cache@v4\n      - run: echo ${{ secrets.TOKEN }}\n";
    let (tmp, dir) = lab(&[(W1, wf("x/**", jobs))]);
    let r = execute(&tmp, &dir, &plan(&tmp, &dir), "inc2");
    assert_eq!(r.state, "incomplete");
    assert_eq!(r.completeness.incomplete.len(), 1, "{:?}", r.completeness);
    let jobs = "  a:\n    runs-on: x\n    steps:\n      - uses: actions/checkout@v4\n      - uses: actions/upload-artifact@v4\n";
    let (tmp, dir) = lab(&[(W1, wf("x/**", jobs))]);
    let r = execute(&tmp, &dir, &plan(&tmp, &dir), "inc3");
    assert_eq!(r.state, "incomplete");
    // a failure still wins
    let mut f = r.clone();
    f.units[0].jobs[0].outcome = Outcome::Fail;
    assert_eq!(reassemble(&plan(&tmp, &dir), &f).state, "failure");
}

// ---- a task that cannot be run ----

#[test]
fn a_task_that_cannot_be_run_fails_its_jobs_and_the_jobs_that_need_them() {
    let job = |id: &str, needs: &str, cond: &str| {
        format!("  {id}:\n    runs-on: x\n{needs}{cond}    steps:\n      - run: echo {id}\n")
    };
    let jobs = [
        job("a", "", ""),
        job("b", "    needs: a\n", ""),
        job("d", "    needs: b\n", "    if: always()\n"),
        job("e", "    needs: b\n", ""),
    ]
    .concat();
    let (tmp, dir) = lab(&[(W1, wf("x/**", &jobs))]);
    let mut fp = plan(&tmp, &dir);
    // b is started before a, which it needs: the task cannot be run
    let at = |fp: &FrozenPlan, j: &str| fp.tasks.iter().position(|t| t.jobs == [j]).unwrap();
    let (ia, ib) = (at(&fp, "a"), at(&fp, "b"));
    fp.tasks.swap(ia, ib);
    let r = execute(&tmp, &dir, &fp, "err");
    let by = |id: &str| r.units[0].jobs.iter().find(|j| j.job == id).unwrap();
    assert!(
        matches!(by("b").outcome, Outcome::Error(_)),
        "{:?}",
        by("b").outcome
    );
    assert_eq!(by("a").outcome, Outcome::Pass);
    // e needed b: it does not run, and says why (it did not see a missing need)
    assert_eq!(by("e").outcome, Outcome::Skip("needs failed".into()));
    // d asked to run whatever happened: it runs, and sees a failure
    assert_eq!(by("d").outcome, Outcome::Pass);
    let o = r.units[0].outcomes.iter().find(|o| o.job == "b").unwrap();
    assert_eq!(o.status, local_ci::JobStatus::Failure);
    assert!(o.skipped.as_deref().unwrap().contains("none was given"));
    assert_eq!(r.state, "error");
}

// ---- what a task's result must hold ----

#[test]
fn a_task_result_is_checked_against_what_the_plan_requires_of_it() {
    let jobs = "  a:\n    runs-on: x\n    outputs:\n      v: ${{ steps.s.outputs.v }}\n    steps:\n      - id: s\n        run: echo v=1 >> $GITHUB_OUTPUT\n";
    let (tmp, dir) = lab(&[(W1, wf("x/**", jobs))]);
    let fp = plan(&tmp, &dir);
    let ws = workspace(&tmp, &dir, &fp, "t");
    let task = &fp.tasks[0];
    assert_eq!(task.verification.job_outputs, vec!["a.v".to_string()]);
    let r = local_ci::execute_task(
        &ctx(tmp.path()),
        &fp,
        &task.id,
        &TaskInputs::default(),
        None,
        &ws,
        opts(&tmp.path().join("lab")),
    )
    .unwrap();
    assert_eq!(verify_task_result(task, &r), Vec::<String>::new());

    let mut lost = r.clone();
    lost.outcomes[0].outputs.clear();
    let why = verify_task_result(task, &lost);
    assert!(why.iter().any(|w| w.contains("output `v`")), "{why:?}");

    let mut none = r.clone();
    none.outcomes.clear();
    assert!(verify_task_result(task, &none)
        .iter()
        .any(|w| w.contains("no outcome")));

    let mut twice = r.clone();
    twice.outcomes.push(twice.outcomes[0].clone());
    assert!(verify_task_result(task, &twice)
        .iter()
        .any(|w| w.contains("2 outcomes")));

    let mut other = r.clone();
    other.task = "elsewhere".into();
    assert!(!verify_task_result(task, &other).is_empty());

    // an artifact the task must publish
    let mut t2 = task.clone();
    t2.verification.artifacts = vec!["report".into()];
    assert!(verify_task_result(&t2, &r)
        .iter()
        .any(|w| w.contains("artifact `report`")));
    let mut with = r.clone();
    with.artifacts = vec!["report".into()];
    assert!(verify_task_result(&t2, &with).is_empty());
    // a job that failed is not asked for its outputs
    let mut failed = lost.clone();
    failed.outcomes[0].status = local_ci::JobStatus::Failure;
    assert!(verify_task_result(task, &failed).is_empty());
}

// ---- receipts hold the digest of the tree ----

const NODE_JOB: &str = "  a:\n    runs-on: x\n    steps:\n      - run: sh node.sh\n      - run: test -f node_modules/p/a.js\n  b:\n    runs-on: x\n    steps:\n      - run: sh node.sh\n      - run: test -f node_modules/p/a.js && echo b\n";

fn setup_in(
    tmp: &tempfile::TempDir,
    dir: &Path,
    fp: &FrozenPlan,
    name: &str,
) -> (Workspace, SetupRecipe, local_ci::setup::PreparedSetup) {
    let ws = workspace(tmp, dir, fp, name);
    let recipe = derive_setup_recipe(fp, &fp.tasks[0].id).unwrap();
    let key = recipe.template_key(&TemplateContext::local(fp, "run-1"));
    let done = local_ci::execute_setup(
        &ctx(tmp.path()),
        fp,
        &recipe,
        &key,
        &ws,
        opts(&tmp.path().join("lab")),
    )
    .unwrap();
    (ws, recipe, done)
}

fn task_with(
    tmp: &tempfile::TempDir,
    fp: &FrozenPlan,
    ws: &Workspace,
    receipt: &local_ci::setup::PreparedReceipt,
) -> local_ci::TaskResult {
    local_ci::execute_task(
        &ctx(tmp.path()),
        fp,
        &fp.tasks[0].id,
        &TaskInputs::default(),
        Some((receipt, &TemplateContext::local(fp, "run-1"))),
        ws,
        opts(&tmp.path().join("lab")),
    )
    .unwrap()
}

#[test]
fn a_receipt_is_believed_only_for_the_tree_the_setup_made() {
    let (tmp, dir) = lab(&[(W1, wf("x/**", NODE_JOB))]);
    let fp = plan(&tmp, &dir);

    // the tree is what the setup made: the install is not run again
    let (ws, _, done) = setup_in(&tmp, &dir, &fp, "ok");
    assert!(done.ok(), "{:?}", done.steps);
    let art = done
        .receipt
        .artifacts
        .iter()
        .find(|a| a.path == "node_modules")
        .expect("node_modules is an artifact");
    assert!(art.digest.starts_with("sha256:"), "{}", art.digest);
    let r = task_with(&tmp, &fp, &ws, &done.receipt);
    assert_eq!(r.receipt_ignored, None);
    assert_eq!(r.jobs[0].steps[0].outcome, Outcome::Skip("setup".into()));

    // a file in node_modules is gone
    let (ws, _, done) = setup_in(&tmp, &dir, &fp, "gone");
    std::fs::remove_file(ws.root.join("node_modules/p/b.js")).unwrap();
    let r = task_with(&tmp, &fp, &ws, &done.receipt);
    assert!(
        r.receipt_ignored
            .as_deref()
            .unwrap()
            .contains("node_modules"),
        "{:?}",
        r.receipt_ignored
    );
    // the task did its setup itself, and the tree is whole again
    assert_eq!(r.jobs[0].steps[0].outcome, Outcome::Pass);
    assert!(ws.root.join("node_modules/p/b.js").is_file());

    // a file has other contents of the same size
    let (ws, _, done) = setup_in(&tmp, &dir, &fp, "same-size");
    std::fs::write(ws.root.join("node_modules/p/b.js"), "TWO").unwrap();
    let r = task_with(&tmp, &fp, &ws, &done.receipt);
    assert!(r.receipt_ignored.is_some());
    assert_eq!(r.jobs[0].steps[0].outcome, Outcome::Pass);
}

// ---- ignore-env ----

#[test]
fn ignore_env_values_that_differ_between_jobs_are_recorded() {
    let jobs = "  a:\n    runs-on: x\n    env:\n      DB_URL: one-secret\n    steps:\n      - run: sh install.sh\n      - run: test -f installed.marker\n  b:\n    runs-on: x\n    env:\n      DB_URL: two-secret\n    steps:\n      - run: sh install.sh\n      - run: test -f installed.marker\n";
    let (tmp, dir) = lab(&[
        (W1, wf("x/**", jobs)),
        (
            ".local-ci.toml",
            "[setup]\nignore-env = [\"DB_URL\"]\n".into(),
        ),
    ]);
    let fp = plan(&tmp, &dir);
    assert_eq!(fp.config.ignore_env, vec!["DB_URL".to_string()]);

    // one install is made for both jobs, with the values of the first
    let r = execute(&tmp, &dir, &fp, "env");
    // both steps of the shared start are installs for the jobs
    assert_eq!(r.setup.len(), 2, "{:?}", r.setup);
    let note = r.setup[0].note.as_deref().expect("a note on the install");
    assert!(note.contains("ignore-env"), "{note}");
    // the name and a fingerprint of the value, never the value
    assert!(note.contains("DB_URL=sha256:5939fc78"), "{note}");
    assert!(note.contains("one.yml › a"), "{note}");
    assert!(
        note.contains("one.yml › b: DB_URL=sha256:7997011b"),
        "{note}"
    );
    assert!(!note.contains("secret"), "{note}");
    assert!(!r.warnings.iter().any(|w| w.contains("secret")));
    assert!(r.warnings.iter().any(|w| w == note), "{:?}", r.warnings);

    // and in the receipt of the setup
    let recipes: Vec<SetupRecipe> = fp
        .tasks
        .iter()
        .map(|t| derive_setup_recipe(&fp, &t.id).unwrap())
        .collect();
    let all = SetupRecipe::union(&recipes.iter().collect::<Vec<_>>());
    let ws = workspace(&tmp, &dir, &fp, "env2");
    let key = all.template_key(&TemplateContext::local(&fp, "run-1"));
    let done = local_ci::execute_setup(
        &ctx(tmp.path()),
        &fp,
        &all,
        &key,
        &ws,
        opts(&tmp.path().join("lab")),
    )
    .unwrap();
    assert_eq!(done.receipt.notes.len(), 2, "{:?}", done.receipt.notes);
    assert!(done.receipt.notes[0]
        .text
        .contains("DB_URL=sha256:7997011b"));
    assert!(done
        .receipt
        .notes
        .iter()
        .all(|n| !n.text.contains("secret")));

    // jobs that agree leave no note
    let same = jobs.replace("two-secret", "one-secret");
    let (tmp, dir) = lab(&[
        (W1, wf("x/**", &same)),
        (
            ".local-ci.toml",
            "[setup]\nignore-env = [\"DB_URL\"]\n".into(),
        ),
    ]);
    let r = execute(&tmp, &dir, &plan(&tmp, &dir), "env3");
    assert_eq!(r.setup.len(), 2);
    assert!(r.setup.iter().all(|s| s.note.is_none()));
}

// ---- an install is believed in its own run, without hashing it again ----

const CACHE_JOBS: &str = "  a:\n    runs-on: x\n    steps:\n      - run: sh node.sh\n      - run: mkdir -p node_modules/.cache && echo x > node_modules/.cache/t\n  b:\n    runs-on: x\n    needs: a\n    steps:\n      - run: sh node.sh\n      - run: test -f node_modules/.cache/t\n";

#[test]
fn a_job_that_writes_in_node_modules_does_not_make_the_next_job_install_again() {
    let (tmp, dir) = lab(&[(W1, wf("x/**", CACHE_JOBS))]);
    let fp = plan(&tmp, &dir);
    let r = execute(&tmp, &dir, &fp, "cachewrite");
    assert_eq!(r.state, "success", "{}", r.description);
    let b = r.units[0].jobs.iter().find(|j| j.job == "b").unwrap();
    assert_eq!(b.steps[0].outcome, Outcome::Skip("setup".into()), "{b:?}");
    assert_eq!(b.steps[1].outcome, Outcome::Pass);
}

#[test]
fn a_receipt_from_outside_is_checked_again_before_it_is_used() {
    let (tmp, dir) = lab(&[(W1, wf("x/**", NODE_JOB))]);
    let fp = plan(&tmp, &dir);
    // something wrote next to what the install made after the receipt was made
    let (ws, _, done) = setup_in(&tmp, &dir, &fp, "late-write");
    std::fs::create_dir_all(ws.root.join("node_modules/.cache")).unwrap();
    std::fs::write(ws.root.join("node_modules/.cache/t"), "x").unwrap();
    let r = task_with(&tmp, &fp, &ws, &done.receipt);
    assert!(r.receipt_ignored.is_some(), "{r:?}");
    assert_eq!(r.jobs[0].steps[0].outcome, Outcome::Pass);
}

#[test]
fn a_copy_of_a_template_is_checked_when_it_is_made() {
    use local_ci::template::TemplateStore;
    use std::os::unix::fs::PermissionsExt;
    let (tmp, dir) = lab(&[(W1, wf("x/**", NODE_JOB))]);
    let fp = plan(&tmp, &dir);
    let (ws, recipe, done) = setup_in(&tmp, &dir, &fp, "builder");
    assert!(done.ok());
    let key = recipe.template_key(&TemplateContext::local(&fp, "run-1"));
    let c = ctx(tmp.path());
    let store = TemplateStore::new(tmp.path().join("templates"));
    let tpl = store.publish(&c, &key, &done.receipt, &ws.root).unwrap();
    // a copy right after it was made is what the receipt describes
    let ok = tmp.path().join("copy-ok");
    let receipt = tpl.materialize(&c, &ok).unwrap();
    assert_eq!(receipt.steps, done.receipt.steps);
    // a template that was changed after it was sealed is not copied
    let file = tpl.tree().join("node_modules/p/b.js");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&file, "TWO").unwrap();
    let bad = tmp.path().join("copy-bad");
    assert!(tpl.materialize(&c, &bad).is_err());
    assert!(!bad.exists());
}

// ---- a matrix job whose task cannot be run ----

#[test]
fn a_matrix_job_whose_task_cannot_be_run_fails_every_entry_without_a_missing_note() {
    let jobs = "  a:\n    runs-on: x\n    steps:\n      - run: echo a\n  b:\n    runs-on: x\n    needs: a\n    strategy:\n      matrix:\n        n: [1, 2, 3]\n    steps:\n      - run: echo ${{ matrix.n }}\n";
    let (tmp, dir) = lab(&[(W1, wf("x/**", jobs))]);
    let mut fp = plan(&tmp, &dir);
    // b's variants are tried before a, which they need
    let first_b = fp
        .tasks
        .iter()
        .position(|t| t.jobs == ["b"])
        .expect("a task of b");
    let ia = fp.tasks.iter().position(|t| t.jobs == ["a"]).unwrap();
    fp.tasks.swap(ia, first_b);
    let r = execute(&tmp, &dir, &fp, "matrix-err");
    let b: Vec<_> = r.units[0].jobs.iter().filter(|j| j.job == "b").collect();
    assert_eq!(b.len(), 3, "{b:?}");
    assert!(b.iter().all(|j| matches!(j.outcome, Outcome::Error(_))));
    assert!(
        r.completeness.missing.is_empty(),
        "{:?}",
        r.completeness.missing
    );
    assert_eq!(r.state, "error");
}

// ---- a workflow that could not be run is not a pass ----

#[test]
fn a_workflow_that_cannot_be_run_makes_the_run_incomplete() {
    let cyc = "  a:\n    runs-on: x\n    needs: b\n    steps:\n      - run: echo a\n  b:\n    runs-on: x\n    needs: a\n    steps:\n      - run: echo b\n";
    let (tmp, dir) = lab(&[(W1, wf("x/**", cyc)), (W2, wf("y/**", PLAIN))]);
    let fp = plan(&tmp, &dir);
    assert!(fp.skipped.iter().any(|s| s.unrunnable), "{:?}", fp.skipped);
    let r = execute(&tmp, &dir, &fp, "cycle");
    assert_eq!(r.completeness.verdict, Verdict::Incomplete);
    assert_eq!(r.state, "incomplete", "{}", r.description);
    assert!(
        r.completeness
            .incomplete
            .iter()
            .any(|i| i.contains("one.yml (not run)")),
        "{:?}",
        r.completeness.incomplete
    );
    assert!(r.description.contains("one.yml"), "{}", r.description);

    // when that is the only workflow, nothing ran, and that is not `no_work`
    let (tmp, dir) = lab(&[(W1, wf("x/**", cyc))]);
    let r = execute(&tmp, &dir, &plan(&tmp, &dir), "cycle2");
    assert_eq!(r.state, "incomplete", "{}", r.description);

    // a workflow the changes do not trigger is not a gap
    let (tmp, dir) = lab(&[(W1, wf("nothing/**", PLAIN)), (W2, wf("y/**", PLAIN))]);
    let r = execute(&tmp, &dir, &plan(&tmp, &dir), "untriggered");
    assert_eq!(r.state, "success", "{}", r.description);
}

// ---- the machine's GitHub token ----

fn ctx_with_gh(home: &Path, token: Option<&str>) -> ExecContext {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = match token {
        Some(t) => format!("#!/bin/sh\n[ \"$1 $2\" = \"auth token\" ] && echo {t}\n"),
        None => "#!/bin/sh\nexit 1\n".to_string(),
    };
    std::fs::write(bin.join("gh"), script).unwrap();
    std::fs::set_permissions(bin.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    ExecContext::new(
        [
            (
                "PATH",
                format!("{}:/usr/bin:/bin:/usr/local/bin", bin.display()),
            ),
            ("HOME", home.to_string_lossy().into_owned()),
            (
                "LOCAL_CI_HOME",
                home.join("cache").to_string_lossy().into_owned(),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    )
}

const TOKEN_JOB: &str = "  a:\n    runs-on: x\n    env:\n      GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n    steps:\n      - run: test \"$GH_TOKEN\" = ghs_faketoken123 && echo \"token ${{ github.token }}\"\n";

#[test]
fn the_machines_github_token_is_supplied_and_masked_in_the_log() {
    let (tmp, dir) = lab(&[(W1, wf("x/**", TOKEN_JOB))]);
    let c = ctx_with_gh(tmp.path(), Some("ghs_faketoken123"));
    let fp = local_ci::build_frozen_plan(&c, &target(&dir), &mut Quiet).unwrap();
    let ws = workspace(&tmp, &dir, &fp, "token");
    let r = local_ci::execute_plan(&c, &fp, &ws, opts(&tmp.path().join("lab"))).unwrap();
    assert_eq!(r.state, "success", "{}", r.description);
    let step = &r.units[0].jobs[0].steps[0];
    assert_eq!(step.outcome, Outcome::Pass);
    let log = std::fs::read_to_string(step.log.as_ref().unwrap()).unwrap();
    assert!(log.contains("token ***"), "{log}");
    assert!(!log.contains("ghs_faketoken123"), "{log}");

    // no token on this machine: skipped as before, and the run is incomplete
    let (tmp, dir) = lab(&[(W1, wf("x/**", TOKEN_JOB))]);
    let c = ctx_with_gh(tmp.path(), None);
    let fp = local_ci::build_frozen_plan(&c, &target(&dir), &mut Quiet).unwrap();
    let ws = workspace(&tmp, &dir, &fp, "notoken");
    let r = local_ci::execute_plan(&c, &fp, &ws, opts(&tmp.path().join("lab"))).unwrap();
    assert_eq!(r.state, "incomplete", "{}", r.description);

    // other secrets are never supplied
    let jobs = "  a:\n    runs-on: x\n    steps:\n      - run: echo ${{ secrets.OTHER }}\n";
    let (tmp, dir) = lab(&[(W1, wf("x/**", jobs))]);
    let c = ctx_with_gh(tmp.path(), Some("ghs_faketoken123"));
    let fp = local_ci::build_frozen_plan(&c, &target(&dir), &mut Quiet).unwrap();
    let ws = workspace(&tmp, &dir, &fp, "other");
    let r = local_ci::execute_plan(&c, &fp, &ws, opts(&tmp.path().join("lab"))).unwrap();
    assert_eq!(r.state, "incomplete");
}

fn tree_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            tree_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

#[test]
fn the_token_is_never_written_to_a_file_of_the_run() {
    // the script itself checks, while it runs, that it holds no token and
    // that only its owner can read it
    let jobs = "  a:\n    runs-on: x\n    env:\n      GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n    steps:\n      - run: |\n          ! grep -q ghs_faketoken123 \"$0\"\n          test \"$(ls -l \"$0\" | cut -c1-10)\" = -rw-------\n          test \"$GH_TOKEN\" = ghs_faketoken123\n          echo \"viewer ${{ github.token }}\"\n      - env:\n          T: ${{ format('x-{0}', secrets.GITHUB_TOKEN) }}\n        run: ! grep -q ghs_faketoken123 \"$0\" && test \"$T\" = x-ghs_faketoken123\n";
    let (tmp, dir) = lab(&[(W1, wf("x/**", jobs))]);
    let c = ctx_with_gh(tmp.path(), Some("ghs_faketoken123"));
    let fp = local_ci::build_frozen_plan(&c, &target(&dir), &mut Quiet).unwrap();
    let ws = workspace(&tmp, &dir, &fp, "nodisk");
    let r = local_ci::execute_plan(&c, &fp, &ws, opts(&tmp.path().join("lab"))).unwrap();
    assert_eq!(
        r.state, "success",
        "{} {:?}",
        r.description, r.units[0].jobs[0].steps
    );
    let mut files = Vec::new();
    tree_files(tmp.path(), &mut files);
    let mut seen = 0;
    for f in files {
        // the fake `gh` and the workflow under test name the value themselves
        if f.starts_with(tmp.path().join("fakebin"))
            || f.to_string_lossy().contains("/wt/")
            || f.to_string_lossy().contains("/.github/workflows/")
        {
            continue;
        }
        let b = std::fs::read(&f).unwrap();
        seen += 1;
        assert!(
            !String::from_utf8_lossy(&b).contains("ghs_faketoken123"),
            "token in {}",
            f.display()
        );
    }
    assert!(seen > 3);
}

// ---- what reads and what writes outside this machine ----

#[test]
fn reading_commands_run_and_writing_ones_are_left_out() {
    let step = |run: &str| format!("      - run: {run}\n");
    let jobs = format!(
        "  a:\n    runs-on: x\n    steps:\n{}{}{}{}",
        step("git push --dry-run origin HEAD || true"),
        step("echo using aws sdk"),
        step("grep -q installed install.sh"),
        step("sudo -n true || true"),
    );
    let (tmp, dir) = lab(&[(W1, wf("x/**", &jobs))]);
    let r = execute(&tmp, &dir, &plan(&tmp, &dir), "reads");
    assert_eq!(
        r.state, "success",
        "{} {:?}",
        r.description, r.units[0].jobs[0].steps
    );

    let jobs = format!(
        "  a:\n    runs-on: x\n    steps:\n{}",
        step("git push origin main"),
    );
    let (tmp, dir) = lab(&[(W1, wf("x/**", &jobs))]);
    let r = execute(&tmp, &dir, &plan(&tmp, &dir), "writes");
    assert_eq!(r.state, "incomplete");
    assert!(r.description.contains("not covered"), "{}", r.description);
}
