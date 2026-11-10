//! The types that cross between a coordinator and a worker: versions,
//! digests, and results without the paths of the host that made them. The JSON
//! `local-ci run` writes is not one of them and does not change.

#![allow(clippy::disallowed_methods)] // the tests build their own commands

mod common;
use local_ci::complete::{Completeness, Verdict};
use local_ci::dag::{ExecutionTask, JobGraph};
use local_ci::exec::{JobResult, Outcome, StepResult, UnitResult};
use local_ci::report::{RunResult, Skipped};
use local_ci::result_wire::{MAX_LOG_PATH, MAX_WIRE_BYTES};
use local_ci::setup::{PreparedReceipt, SetupRecipe};
use local_ci::wire::{Digested, WIRE_SCHEMA_VERSION};
use local_ci::{
    validate_run_result, validate_wire_run_result, ExecContext, FrozenPlan, Quiet, Target,
    TaskInputs, WireRunResult,
};
use serde_json::{json, Value};
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

fn plan() -> (common::Tmp, FrozenPlan) {
    let tmp = common::tempdir().unwrap();
    let dir: PathBuf = tmp.path().join("work");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(
        dir.join(".github/workflows/ci.yml"),
        "name: ci\non: pull_request\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo a\n  b:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo b\n",
    )
    .unwrap();
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "feature"]);
    std::fs::write(dir.join("a.txt"), "b\n").unwrap();
    git(&dir, &["commit", "-qam", "change"]);
    let ctx = ExecContext::new(
        [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", &tmp.path().to_string_lossy()),
            ("LOCAL_CI_HOME", &tmp.path().join("cache").to_string_lossy()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    );
    let t = Target {
        base: Some("main".into()),
        sha: "HEAD".into(),
        all: false,
        include_disabled: true,
        workflows_dirs: vec![],
        workflows: vec![],
        jobs: vec![],
        dir,
    };
    let fp = local_ci::build_frozen_plan(&ctx, &t, &mut Quiet).unwrap();
    (tmp, fp)
}

/// `v` without every `schema_version` key, at any depth: what an older build wrote.
fn without_versions(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.remove("schema_version");
            m.values_mut().for_each(without_versions);
        }
        Value::Array(a) => a.iter_mut().for_each(without_versions),
        _ => {}
    }
}

#[test]
fn a_document_without_a_version_reads_as_the_first_and_this_build_writes_it() {
    let (_t, fp) = plan();
    let task = fp.tasks[0].clone();
    let recipe = SetupRecipe {
        task: task.id.clone(),
        ..SetupRecipe::default()
    };
    let receipt = PreparedReceipt::empty("k", "s");
    let inputs = TaskInputs::default();
    let graph = fp.graph.clone();
    for (v, name) in [
        (serde_json::to_value(&task).unwrap(), "task"),
        (serde_json::to_value(&recipe).unwrap(), "recipe"),
        (serde_json::to_value(&receipt).unwrap(), "receipt"),
        (serde_json::to_value(&inputs).unwrap(), "inputs"),
        (serde_json::to_value(&graph).unwrap(), "graph"),
    ] {
        assert_eq!(v["schema_version"], json!(WIRE_SCHEMA_VERSION), "{name}");
    }
    let read = |mut v: Value| {
        without_versions(&mut v);
        v
    };
    let t: ExecutionTask =
        serde_json::from_value(read(serde_json::to_value(&task).unwrap())).unwrap();
    assert_eq!(t, task);
    let r: SetupRecipe =
        serde_json::from_value(read(serde_json::to_value(&recipe).unwrap())).unwrap();
    assert_eq!(r, recipe);
    let p: PreparedReceipt =
        serde_json::from_value(read(serde_json::to_value(&receipt).unwrap())).unwrap();
    assert_eq!(p, receipt);
    let i: TaskInputs =
        serde_json::from_value(read(serde_json::to_value(&inputs).unwrap())).unwrap();
    assert_eq!(i, inputs);
    let g: JobGraph = serde_json::from_value(read(serde_json::to_value(&graph).unwrap())).unwrap();
    assert_eq!(g, graph);
    assert_eq!(g.schema_version, WIRE_SCHEMA_VERSION);
    assert_eq!(JobGraph::default().schema_version, WIRE_SCHEMA_VERSION);
}

#[test]
fn a_plan_holds_only_versions_it_reads() {
    let (_t, fp) = plan();
    fp.verify().unwrap();
    // the plan's own version moved with the fields its tasks and graph got
    assert_eq!(fp.schema_version, 6);
    let mut g = fp.clone();
    g.graph.schema_version += 1;
    let e = g.verify().unwrap_err().to_string();
    assert!(e.contains("plan job graph: schema version"), "{e}");
    let mut t = fp.clone();
    t.tasks[0].schema_version = 0;
    let e = t.verify().unwrap_err().to_string();
    assert!(e.contains("plan task"), "{e}");
}

#[test]
fn what_crosses_has_a_digest_of_its_content() {
    let (_t, fp) = plan();
    let a = fp.tasks[0].clone();
    let mut b = a.clone();
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    assert!(a.digest().unwrap().starts_with("sha256:"));
    b.exclusive = !b.exclusive;
    assert_ne!(a.digest().unwrap(), b.digest().unwrap());
    let e = b
        .verify_digest(&a.digest().unwrap())
        .unwrap_err()
        .to_string();
    assert!(e.contains("does not match"), "{e}");

    let recipe = SetupRecipe::default();
    let mut other = SetupRecipe::default();
    other.task = "x".into();
    assert_ne!(recipe.digest().unwrap(), other.digest().unwrap());
    let r1 = PreparedReceipt::empty("k", "s");
    assert_ne!(
        r1.digest().unwrap(),
        PreparedReceipt::empty("k2", "s").digest().unwrap()
    );
    let mut i = TaskInputs::default();
    let d0 = i.digest().unwrap();
    i.run_cancelled = true;
    assert_ne!(d0, i.digest().unwrap());
    assert_eq!(
        fp.graph.digest().unwrap(),
        fp.graph.clone().digest().unwrap()
    );
}

fn step(label: &str, log: Option<&str>) -> StepResult {
    StepResult {
        label: label.into(),
        outcome: Outcome::Pass,
        exit: Some(0),
        secs: 1.5,
        log: log.map(PathBuf::from),
        note: None,
    }
}

fn run_result(dir: &str) -> RunResult {
    RunResult {
        version: "0.0.0".into(),
        sha: "abc".into(),
        base: "main".into(),
        changed_files: 1,
        mode: "changes".into(),
        state: "success".into(),
        description: "success: pass 1 / fail 0 / error 0 / skip 0".into(),
        secs: 2.0,
        run_dir: dir.into(),
        setup: vec![step("setup", Some(&format!("{dir}/setup/01-s.log")))],
        units: vec![UnitResult {
            unit: Some(0),
            workflow: ".github/workflows/ci.yml".into(),
            jobs: vec![JobResult {
                workflow: ".github/workflows/ci.yml".into(),
                job: "a".into(),
                display: "a".into(),
                outcome: Outcome::Pass,
                secs: 1.5,
                steps: vec![
                    step("echo a", Some(&format!("{dir}/ci-a/01-echo-a.log"))),
                    step("reused", None),
                ],
                cancelled: false,
                time: None,
            }],
            waited_secs: 0.0,
            copy_secs: None,
            cached: None,
            outcomes: vec![],
        }],
        skipped_workflows: vec![Skipped {
            workflow: "x.yml".into(),
            reason: "r".into(),
        }],
        not_covered: vec![],
        warnings: vec![],
        completeness: Completeness {
            verdict: Verdict::Complete,
            expected_units: 1,
            expected_jobs: 1,
            from_cache: 0,
            missing: vec![],
            duplicate: vec![],
            unexpected: vec![],
            invalid: vec![],
            incomplete: vec![],
        },
    }
}

// The bytes `local-ci run` and `local-ci status` write for a result. A change
// to this text is a change of the command's output.
const CLI_JSON: &str = r#"{"version":"0.0.0","sha":"abc","base":"main","changed_files":1,"mode":"changes","state":"success","description":"success: pass 1 / fail 0 / error 0 / skip 0","secs":2.0,"run_dir":"/runs/r1","setup":[{"label":"setup","result":"pass","exit":0,"secs":1.5,"log":"/runs/r1/setup/01-s.log"}],"units":[{"unit":0,"workflow":".github/workflows/ci.yml","jobs":[{"workflow":".github/workflows/ci.yml","job":"a","display":"a","result":"pass","secs":1.5,"steps":[{"label":"echo a","result":"pass","exit":0,"secs":1.5,"log":"/runs/r1/ci-a/01-echo-a.log"},{"label":"reused","result":"pass","exit":0,"secs":1.5}]}],"waited_secs":0.0}],"skipped_workflows":[{"workflow":"x.yml","reason":"r"}],"not_covered":[],"warnings":[],"completeness":{"verdict":"complete","expected_units":1,"expected_jobs":1,"from_cache":0,"missing":[],"duplicate":[],"unexpected":[],"invalid":[],"incomplete":[]}}"#;

#[test]
fn the_json_of_a_run_is_what_it_was() {
    let r = run_result("/runs/r1");
    assert_eq!(serde_json::to_string(&r).unwrap(), CLI_JSON);
    // `status` reads it back with what it needs
    let saved: local_ci::report::Saved = serde_json::from_str(CLI_JSON).unwrap();
    assert_eq!(
        (saved.sha.as_str(), saved.state.as_str()),
        ("abc", "success")
    );
}

#[test]
fn a_run_result_crosses_without_a_path_of_the_host_and_comes_back_in_the_receivers() {
    let r = run_result("/runs/r1");
    let wire = WireRunResult::from_run_result(&r).unwrap();
    let text = serde_json::to_string(&wire).unwrap();
    assert!(!text.contains("/runs/r1"), "{text}");
    assert!(!text.contains("run_dir"), "{text}");
    assert!(text.contains(r#""log":"ci-a/01-echo-a.log""#), "{text}");
    assert!(text.contains(r#""schema_version":1"#));
    let back = WireRunResult::from_json(&text)
        .unwrap()
        .into_run_result("/mine/r9".into())
        .unwrap();
    assert_eq!(back.run_dir, PathBuf::from("/mine/r9"));
    assert_eq!(
        back.units[0].jobs[0].steps[0].log,
        Some(PathBuf::from("/mine/r9/ci-a/01-echo-a.log"))
    );
    assert_eq!(back.units[0].jobs[0].steps[1].log, None);
    // all else is the same
    let again = serde_json::to_string(&run_result("/mine/r9")).unwrap();
    assert_eq!(serde_json::to_string(&back).unwrap(), again);
    assert_eq!(wire.digest().unwrap(), wire.clone().digest().unwrap());

    // a result of another version is not taken
    let mut v = wire.clone();
    v.schema_version = 9;
    assert!(v.into_run_result("/mine".into()).is_err());
    // nor one with a log outside its directory, here or there
    let mut outside = r.clone();
    outside.setup[0].log = Some("/elsewhere/s.log".into());
    assert!(WireRunResult::from_run_result(&outside).is_err());
    let mut same = r.clone();
    same.setup[0].log = Some("/runs/r1".into());
    assert!(WireRunResult::from_run_result(&same).is_err());
    let mut odd = r.clone();
    odd.units[0].jobs[0].steps[0].log = Some(PathBuf::from(format!(
        "/runs/r1/{}",
        "a".repeat(MAX_LOG_PATH + 1)
    )));
    assert!(WireRunResult::from_run_result(&odd).is_err());
}

#[test]
fn a_log_path_that_leaves_the_directory_is_refused() {
    let wire = WireRunResult::from_run_result(&run_result("/runs/r1")).unwrap();
    for bad in [
        "/etc/passwd",
        "../x.log",
        "a/../../x.log",
        "./x.log",
        "",
        "a\0b.log",
    ] {
        let mut w = wire.clone();
        w.setup[0].log = Some(bad.into());
        let e = w
            .into_run_result("/mine/r9".into())
            .unwrap_err()
            .to_string();
        assert!(e.contains("log path"), "{bad:?}: {e}");
    }
    let mut long = wire.clone();
    long.units[0].jobs[0].steps[0].log = Some("a".repeat(MAX_LOG_PATH + 1));
    assert!(long.into_run_result("/mine/r9".into()).is_err());
    // a name that holds dots is a name
    let mut ok = wire.clone();
    ok.setup[0].log = Some("a..b/c...log".into());
    ok.into_run_result("/mine/r9".into()).unwrap();
}

#[test]
fn a_result_is_read_up_to_a_size_and_only_if_it_parses() {
    let e = WireRunResult::from_json(&" ".repeat(MAX_WIRE_BYTES + 1))
        .unwrap_err()
        .to_string();
    assert!(e.contains("at most"), "{e}");
    let e = local_ci::WireTaskResult::from_json(&" ".repeat(MAX_WIRE_BYTES + 1))
        .unwrap_err()
        .to_string();
    assert!(e.contains("at most"), "{e}");
    for text in ["", "{}", "[]", "{\"schema_version\":1}"] {
        assert!(WireRunResult::from_json(text).is_err(), "{text}");
        assert!(local_ci::WireTaskResult::from_json(text).is_err(), "{text}");
    }
}

#[test]
fn a_wire_result_is_checked_against_the_plan_like_a_local_one() {
    let (tmp, fp) = plan();
    let dir = tmp.path().join("run");
    std::fs::create_dir_all(&dir).unwrap();
    // a result with nothing in it is not a result of the plan
    let mut r = run_result(dir.to_str().unwrap());
    r.units.clear();
    let wire = WireRunResult::from_run_result(&r).unwrap();
    let c = validate_wire_run_result(&fp, wire.clone(), &dir).unwrap();
    assert_eq!(c, validate_run_result(&fp, &r));
    assert_eq!(c.verdict, Verdict::Invalid);
    assert!(!c.missing.is_empty());
    // a version this build does not read is an error, not a verdict
    let mut v = wire;
    v.schema_version = 2;
    assert!(validate_wire_run_result(&fp, v, &dir).is_err());
}
