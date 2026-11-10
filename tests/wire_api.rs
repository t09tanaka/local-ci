//! The types that cross between a coordinator and a worker: versions,
//! digests, and results without the paths of the host that made them. The JSON
//! `local-ci run` writes is not one of them and does not change.

#![allow(clippy::disallowed_methods)] // the tests build their own commands

mod common;
use local_ci::complete::{Completeness, Verdict};
use local_ci::dag::{ExecutionTask, JobGraph};
use local_ci::exec::{JobResult, Outcome, StepResult, UnitResult};
use local_ci::report::{RunResult, Skipped};
use local_ci::result_wire::{
    MAX_LOG_PATH, MAX_WIRE_BYTES, MAX_WIRE_ITEMS, MAX_WIRE_NOTE, MAX_WIRE_STRING,
};
use local_ci::setup::{PreparedReceipt, SetupRecipe};
use local_ci::wire::{Digested, WIRE_SCHEMA_VERSION};
use local_ci::{
    validate_run_result, validate_wire_run_result, ExecContext, FrozenPlan, Quiet, Target,
    TaskInputs, TaskResult, WireRunResult, WireTaskResult,
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
    let e = WireRunResult::from_json_checked(&" ".repeat(MAX_WIRE_BYTES), "sha256:00").unwrap_err();
    assert!(!e.to_string().contains("at most"), "{e}");
    // every document says the version it holds
    assert_eq!(Digested::schema_version(&graph), WIRE_SCHEMA_VERSION);
    assert_eq!(Digested::schema_version(&task), WIRE_SCHEMA_VERSION);
    assert_eq!(Digested::schema_version(&recipe), WIRE_SCHEMA_VERSION);
    assert_eq!(Digested::schema_version(&receipt), WIRE_SCHEMA_VERSION);
    assert_eq!(Digested::schema_version(&inputs), WIRE_SCHEMA_VERSION);
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
    let other = SetupRecipe {
        task: "x".into(),
        ..SetupRecipe::default()
    };
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

fn task_result(dir: &str) -> TaskResult {
    TaskResult {
        schema_version: WIRE_SCHEMA_VERSION,
        task: "t1".into(),
        jobs: vec![run_result(dir).units[0].jobs[0].clone()],
        outcomes: vec![],
        receipt_ignored: None,
        artifacts: vec!["coverage".into()],
        shard: None,
    }
}

const CLI_JSON_PRETTY: &str = r#"{
  "version": "0.0.0",
  "sha": "abc",
  "base": "main",
  "changed_files": 1,
  "mode": "changes",
  "state": "success",
  "description": "success: pass 1 / fail 0 / error 0 / skip 0",
  "secs": 2.0,
  "run_dir": "/runs/r1",
  "setup": [
    {
      "label": "setup",
      "result": "pass",
      "exit": 0,
      "secs": 1.5,
      "log": "/runs/r1/setup/01-s.log"
    }
  ],
  "units": [
    {
      "unit": 0,
      "workflow": ".github/workflows/ci.yml",
      "jobs": [
        {
          "workflow": ".github/workflows/ci.yml",
          "job": "a",
          "display": "a",
          "result": "pass",
          "secs": 1.5,
          "steps": [
            {
              "label": "echo a",
              "result": "pass",
              "exit": 0,
              "secs": 1.5,
              "log": "/runs/r1/ci-a/01-echo-a.log"
            },
            {
              "label": "reused",
              "result": "pass",
              "exit": 0,
              "secs": 1.5
            }
          ]
        }
      ],
      "waited_secs": 0.0
    }
  ],
  "skipped_workflows": [
    {
      "workflow": "x.yml",
      "reason": "r"
    }
  ],
  "not_covered": [],
  "warnings": [],
  "completeness": {
    "verdict": "complete",
    "expected_units": 1,
    "expected_jobs": 1,
    "from_cache": 0,
    "missing": [],
    "duplicate": [],
    "unexpected": [],
    "invalid": [],
    "incomplete": []
  }
}"#;
// The pretty JSON `local-ci run` saves as `result.json` and `local-ci status`
// reads back, and the line `local-ci status` prints, are what they were.
#[test]
fn the_file_status_reads_and_the_line_it_prints_are_what_they_were() {
    let r = run_result("/runs/r1");
    let tmp = common::tempdir().unwrap();
    let path = tmp.path().join("result.json");
    local_ci::exec::write_json(&path, &r).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, CLI_JSON_PRETTY);
    let saved: local_ci::report::Saved = serde_json::from_str(&text).unwrap();
    assert_eq!(
        (
            saved.sha.as_str(),
            saved.state.as_str(),
            saved.description.as_str()
        ),
        (
            "abc",
            "success",
            "success: pass 1 / fail 0 / error 0 / skip 0"
        )
    );
    // `status: local-ci={state} posted to {short}`
    for (state, github) in [
        ("success", "success"),
        ("no_work", "success"),
        ("failure", "failure"),
        ("error", "error"),
        ("incomplete", "error"),
    ] {
        assert_eq!(local_ci::report::github_state(state), github, "{state}");
    }
}

// What a task result is as JSON: `schema_version` first, nothing else new.
const TASK_JSON: &str = r#"{"schema_version":1,"task":"t1","jobs":[{"workflow":".github/workflows/ci.yml","job":"a","display":"a","result":"pass","secs":1.5,"steps":[{"label":"echo a","result":"pass","exit":0,"secs":1.5,"log":"/runs/r1/ci-a/01-echo-a.log"},{"label":"reused","result":"pass","exit":0,"secs":1.5}]}],"outcomes":[],"receipt_ignored":null,"artifacts":["coverage"],"shard":null}"#;

#[test]
fn the_json_of_a_task_result_is_pinned() {
    let r = task_result("/runs/r1");
    assert_eq!(serde_json::to_string(&r).unwrap(), TASK_JSON);
    let back: TaskResult = serde_json::from_str(TASK_JSON).unwrap();
    assert_eq!(serde_json::to_string(&back).unwrap(), TASK_JSON);
    // written before the version existed, it reads as the first
    let mut v: Value = serde_json::from_str(TASK_JSON).unwrap();
    v.as_object_mut().unwrap().remove("schema_version");
    let old: TaskResult = serde_json::from_value(v).unwrap();
    assert_eq!(old.schema_version, WIRE_SCHEMA_VERSION);
}

fn wire_task(dir: &str) -> WireTaskResult {
    WireTaskResult::from_task_result(&task_result(dir), Path::new(dir)).unwrap()
}

#[test]
fn the_limits_are_what_they_are_documented_to_be_and_a_limit_is_allowed() {
    assert_eq!(MAX_WIRE_ITEMS, 100_000);
    assert_eq!(MAX_WIRE_STRING, 4096);
    assert_eq!(MAX_WIRE_NOTE, 64 * 1024);
    assert_eq!(MAX_LOG_PATH, 1024);
    assert_eq!(MAX_WIRE_BYTES, 64 * 1024 * 1024);
    // a log path of exactly the longest length is a path
    let mut w = WireRunResult::from_run_result(&run_result("/runs/r1")).unwrap();
    w.setup[0].log = Some("a".repeat(MAX_LOG_PATH));
    w.into_run_result("/m".into()).unwrap();
    // text of exactly the longest length is read (here it does not parse)
    let e = WireRunResult::from_json(&" ".repeat(MAX_WIRE_BYTES)).unwrap_err();
    assert!(!e.to_string().contains("at most"), "{e}");
    // every document says the version it holds
    let mut r = WireRunResult::from_run_result(&run_result("/runs/r1")).unwrap();
    r.schema_version = 7;
    assert_eq!(Digested::schema_version(&r), 7);
    let mut t = wire_task("/runs/r1");
    t.schema_version = 7;
    assert_eq!(Digested::schema_version(&t), 7);
}

#[test]
fn a_result_without_a_schema_version_is_not_read() {
    let run =
        serde_json::to_value(WireRunResult::from_run_result(&run_result("/runs/r1")).unwrap())
            .unwrap();
    let task = serde_json::to_value(wire_task("/runs/r1")).unwrap();
    assert!(run.get("schema_version").is_some());
    for (v, what) in [(run, "run"), (task, "task")] {
        let mut v = v;
        v.as_object_mut().unwrap().remove("schema_version");
        let text = v.to_string();
        let e = if what == "run" {
            WireRunResult::from_json(&text).unwrap_err()
        } else {
            WireTaskResult::from_json(&text).unwrap_err()
        };
        assert!(format!("{e:#}").contains("schema_version"), "{what}: {e:#}");
    }
}

#[test]
fn a_task_result_from_another_host_cannot_name_files_outside_its_directory() {
    let ok = wire_task("/runs/r1");
    ok.clone().into_task_result(Path::new("/mine/t1")).unwrap();
    let log_of = |w: &mut WireTaskResult, p: &str| {
        w.jobs[0].steps[0].log = Some(p.into());
    };
    for bad in [
        "/etc/passwd",
        "../x.log",
        "a/../../x.log",
        "a\\b.log",
        "a\nb.log",
        "a\u{1b}[31m.log",
        "a\u{7f}.log",
        "a\u{85}.log",
        "",
    ] {
        let mut w = ok.clone();
        log_of(&mut w, bad);
        let e = w
            .into_task_result(Path::new("/mine/t1"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("log path"), "{bad:?}: {e}");
    }
    for bad in [
        "",
        ".",
        "..",
        "a/b",
        "a\\b",
        "a\0b",
        "a\nb",
        &"x".repeat(201),
    ] {
        let mut w = ok.clone();
        w.artifacts = vec![bad.into()];
        let e = w
            .into_task_result(Path::new("/mine/t1"))
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("an artifact:"), "{bad:?}: {e}");
    }
    let mut w = ok.clone();
    w.artifacts = vec!["x".repeat(200), "a..b".into(), "cov erage".into()];
    w.into_task_result(Path::new("/mine/t1")).unwrap();
}

#[test]
fn what_a_result_may_hold_is_bounded() {
    let base = WireRunResult::from_run_result(&run_result("/runs/r1")).unwrap();
    let here = || "/mine/r9".into();
    // too many steps, over the whole result
    let mut w = base.clone();
    let s = w.setup[0].clone();
    w.setup = vec![s; MAX_WIRE_ITEMS + 1];
    let e = w.into_run_result(here()).unwrap_err().to_string();
    assert!(e.contains("units, jobs and steps"), "{e}");
    let mut w = base.clone();
    let s = w.setup[0].clone();
    w.setup = vec![s; MAX_WIRE_ITEMS - 3];
    let e = w.into_run_result(here());
    assert!(e.is_err(), "the units and jobs count too");
    let mut w = base.clone();
    let s = w.setup[0].clone();
    w.setup = vec![s; MAX_WIRE_ITEMS - 4];
    w.into_run_result(here()).unwrap();
    // strings
    let long = "x".repeat(MAX_WIRE_STRING + 1);
    for edit in [
        (|w: &mut WireRunResult, l: &str| w.setup[0].label = l.into())
            as fn(&mut WireRunResult, &str),
        |w, l| w.units[0].workflow = l.into(),
        |w, l| w.units[0].jobs[0].workflow = l.into(),
        |w, l| w.units[0].jobs[0].job = l.into(),
        |w, l| w.units[0].jobs[0].display = l.into(),
        |w, l| w.units[0].jobs[0].steps[0].label = l.into(),
    ] {
        let mut w = base.clone();
        edit(&mut w, &long);
        let e = w.into_run_result(here()).unwrap_err().to_string();
        assert!(e.contains("at most"), "{e}");
        let mut w = base.clone();
        edit(&mut w, &"x".repeat(MAX_WIRE_STRING));
        w.into_run_result(here()).unwrap();
    }
    // a note may be longer than a name
    let mut w = base.clone();
    w.setup[0].note = Some("n".repeat(MAX_WIRE_NOTE));
    w.clone().into_run_result(here()).unwrap();
    w.setup[0].note = Some("n".repeat(MAX_WIRE_NOTE + 1));
    assert!(w.into_run_result(here()).is_err());
    // a task result: counts and strings
    let t = wire_task("/runs/r1");
    let mut w = t.clone();
    w.task = long.clone();
    assert!(w.into_task_result(Path::new("/m")).is_err());
    let mut w = t.clone();
    let j = w.jobs[0].clone();
    w.jobs = vec![j; 3];
    w.jobs[0].steps = vec![w.jobs[0].steps[0].clone(); MAX_WIRE_ITEMS];
    let e = w.into_task_result(Path::new("/m")).unwrap_err().to_string();
    assert!(e.contains("steps"), "{e}");
    let mut w = t.clone();
    w.artifacts = vec!["a".into(); MAX_WIRE_ITEMS + 1];
    let e = w.into_task_result(Path::new("/m")).unwrap_err().to_string();
    assert!(e.contains("artifacts"), "{e}");
    let mut w = t.clone();
    w.outcomes = vec![
        local_ci::task::JobOutcome {
            job: "a".into(),
            variant: None,
            status: local_ci::task::JobStatus::Success,
            outputs: Default::default(),
            skipped: None,
            ancestor_failed: false,
        };
        MAX_WIRE_ITEMS + 1
    ];
    let e = w.into_task_result(Path::new("/m")).unwrap_err().to_string();
    assert!(e.contains("outcomes"), "{e}");
    let mut w = t;
    let j = w.jobs[0].clone();
    w.jobs = vec![j; MAX_WIRE_ITEMS + 1];
    let e = w.into_task_result(Path::new("/m")).unwrap_err().to_string();
    assert!(e.contains("jobs"), "{e}");
}

#[test]
fn a_document_is_read_with_its_digest_and_its_version_checked() {
    let w = WireRunResult::from_run_result(&run_result("/runs/r1")).unwrap();
    let text = serde_json::to_string(&w).unwrap();
    let d = w.digest().unwrap();
    let back = WireRunResult::from_json_checked(&text, &d).unwrap();
    assert_eq!(back.digest().unwrap(), d);
    // another digest, another version, not JSON, too long
    let e = WireRunResult::from_json_checked(&text, "sha256:00").unwrap_err();
    assert!(e.to_string().contains("does not match"), "{e}");
    let mut other = w.clone();
    other.schema_version = 7;
    let e = WireRunResult::from_json_checked(
        &serde_json::to_string(&other).unwrap(),
        &other.digest().unwrap(),
    )
    .unwrap_err();
    assert!(e.to_string().contains("schema version 7"), "{e}");
    let e = WireRunResult::from_json_checked("{", &d).unwrap_err();
    assert!(format!("{e:#}").contains("cannot read"), "{e:#}");
    let e = WireRunResult::from_json_checked(&" ".repeat(MAX_WIRE_BYTES + 1), &d).unwrap_err();
    assert!(e.to_string().contains("at most"), "{e}");
    // the other types that cross have it too
    let t = TaskInputs::default();
    let text = serde_json::to_string(&t).unwrap();
    assert_eq!(
        TaskInputs::from_json_checked(&text, &t.digest().unwrap()).unwrap(),
        t
    );
    let k = wire_task("/runs/r1");
    let text = serde_json::to_string(&k).unwrap();
    WireTaskResult::from_json_checked(&text, &k.digest().unwrap()).unwrap();
}

// serde refuses an unknown key except in the types that hold a flattened
// field, which read it away (see the module docs of `result_wire`).
#[test]
fn an_unknown_key_is_refused_except_where_serde_cannot() {
    let w = WireRunResult::from_run_result(&run_result("/runs/r1")).unwrap();
    let t = wire_task("/runs/r1");
    for (mut v, at) in [
        (serde_json::to_value(&w).unwrap(), ""),
        (serde_json::to_value(&w).unwrap(), "/units/0"),
        (serde_json::to_value(&t).unwrap(), ""),
    ] {
        v.pointer_mut(at).unwrap()["new_field"] = json!(1);
        let text = v.to_string();
        let e = if v.get("units").is_some() {
            WireRunResult::from_json(&text).map(|_| ())
        } else {
            WireTaskResult::from_json(&text).map(|_| ())
        };
        assert!(
            format!("{:#}", e.unwrap_err()).contains("new_field"),
            "{at}"
        );
    }
    let mut v = serde_json::to_value(&w).unwrap();
    v["setup"][0]["new_field"] = json!(1);
    v["units"][0]["jobs"][0]["new_field"] = json!(1);
    let read = WireRunResult::from_json(&v.to_string()).unwrap();
    assert_eq!(read.digest().unwrap(), w.digest().unwrap());
}
