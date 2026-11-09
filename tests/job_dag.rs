//! The job graph, the tasks a plan is cut into, and running a task from the
//! results of the ones before it.

#![allow(clippy::disallowed_methods)] // the tests build their own commands

use local_ci::dag::{GroupedReason, NodeKind};
use local_ci::exec::Outcome;
use local_ci::frozen::{DisabledSource, DisabledWorkflow};
use local_ci::{
    ExecContext, FrozenPlan, JobOutcome, JobStatus, Quiet, Target, TaskInputs, TaskResult,
};
use std::collections::BTreeMap;
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

/// A repository whose one workflow is `jobs` (YAML under `jobs:`), changed on a branch.
fn repo(jobs: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(
        dir.join(".github/workflows/ci.yml"),
        format!("name: ci\non: pull_request\njobs:\n{jobs}"),
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

/// Like [`repo`], with the workflows `files` (name, full YAML) in place of one.
fn repo_of(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let (tmp, dir) = repo("  x:\n    runs-on: x\n    steps:\n      - run: echo x\n");
    std::fs::remove_file(dir.join(F)).unwrap();
    for (name, text) in files {
        std::fs::write(dir.join(".github/workflows").join(name), text).unwrap();
    }
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "workflows"]);
    (tmp, dir)
}

fn ctx_with(home: &Path, path: &str) -> ExecContext {
    ExecContext::new(
        [
            ("PATH", path),
            ("HOME", &home.to_string_lossy()),
            ("LOCAL_CI_HOME", &home.join("cache").to_string_lossy()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    )
}

fn ctx(home: &Path) -> ExecContext {
    ctx_with(home, "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
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

fn plan_of(jobs: &str) -> (tempfile::TempDir, PathBuf, FrozenPlan) {
    let (tmp, dir) = repo(jobs);
    let fp = local_ci::build_frozen_plan(&ctx(tmp.path()), &target(&dir), &mut Quiet).unwrap();
    (tmp, dir, fp)
}

const F: &str = ".github/workflows/ci.yml";

fn id(job: &str) -> String {
    format!("{F}::{job}")
}

const CHAIN: &str = "  a:\n    runs-on: x\n    steps:\n      - run: echo a\n  b:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo b\n  c:\n    needs: b\n    runs-on: x\n    steps:\n      - run: echo c\n";

const DIAMOND: &str = "  a:\n    runs-on: x\n    steps:\n      - run: echo a\n  b:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo b\n  c:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo c\n  d:\n    needs: [b, c]\n    runs-on: x\n    steps:\n      - run: echo d\n";

#[test]
fn a_needs_chain_is_a_chain_of_edges_and_of_tasks() {
    let (_t, _d, fp) = plan_of(CHAIN);
    // one engine unit, three tasks: the jobs do not share a checkout
    assert_eq!(fp.units.len(), 1);
    let g = local_ci::derive_job_dag(&fp).unwrap();
    assert_eq!(g, fp.graph);
    let ids: Vec<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, [id("a"), id("b"), id("c")]);
    assert_eq!(g.node(&id("c")).unwrap().depends_on, [id("b")]);
    assert_eq!(fp.tasks.len(), 3);
    assert_eq!(fp.tasks[0].depends_on, Vec::<String>::new());
    assert_eq!(fp.tasks[1].depends_on, [id("a")]);
    assert_eq!(fp.tasks[2].depends_on, [id("b")]);
    assert!(fp.tasks.iter().all(|t| t.grouped_reason.is_none()));
    assert!(fp.tasks.iter().all(|t| t.workspace.own_checkout));
    assert!(fp.tasks.iter().all(|t| t.retry.max_attempts == 1));
    // a shard group per task says why nothing is split
    assert_eq!(fp.shard_groups.len(), 3);
    assert!(fp
        .shard_groups
        .iter()
        .all(|s| !s.split && !s.not_split.is_empty()));
}

#[test]
fn a_diamond_joins_on_both_sides() {
    let (_t, _d, fp) = plan_of(DIAMOND);
    let g = &fp.graph;
    assert_eq!(g.nodes.len(), 4);
    assert_eq!(g.node(&id("d")).unwrap().depends_on, [id("b"), id("c")]);
    let pos = |j: &str| g.nodes.iter().position(|n| n.id == id(j)).unwrap();
    assert!(pos("a") < pos("b") && pos("a") < pos("c"));
    assert!(pos("b") < pos("d") && pos("c") < pos("d"));
    let d = fp.task(&id("d")).unwrap();
    assert_eq!(d.depends_on, [id("b"), id("c")]);
    // the tasks come in an order each can run after the ones before it
    let order: Vec<&str> = fp.tasks.iter().map(|t| t.id.as_str()).collect();
    for t in &fp.tasks {
        let at = order.iter().position(|o| *o == t.id).unwrap();
        for dep in &t.depends_on {
            assert!(order.iter().position(|o| o == dep).unwrap() < at);
        }
    }
}

const MATRIX: &str = "  m:\n    runs-on: x\n    strategy:\n      matrix:\n        n: [1, 2, 3]\n    outputs:\n      o: ${{ steps.s.outputs.o }}\n    steps:\n      - id: s\n        run: echo \"o=${{ matrix.n }}\" >> \"$GITHUB_OUTPUT\"\n  gather:\n    needs: m\n    runs-on: x\n    steps:\n      - run: test \"${{ needs.m.outputs.o }}\" = 3\n";

#[test]
fn a_matrix_is_variants_joined_at_the_node_later_jobs_need() {
    let (_t, _d, fp) = plan_of(MATRIX);
    let g = &fp.graph;
    let variants: Vec<String> = (0..3).map(|i| format!("{}#{i}", id("m"))).collect();
    for (i, v) in variants.iter().enumerate() {
        let n = g.node(v).unwrap();
        assert!(matches!(&n.kind, NodeKind::MatrixVariant { index, .. } if *index == i));
        assert!(n.depends_on.is_empty());
    }
    let group = g.node(&id("m")).unwrap();
    assert!(matches!(&group.kind, NodeKind::MatrixGroup { variants: v, .. } if *v == variants));
    assert_eq!(group.depends_on, variants);
    assert_eq!(g.node(&id("gather")).unwrap().depends_on, [id("m")]);
    // each variant is a task; the later job waits for all of them
    assert_eq!(fp.tasks.len(), 4);
    let gather = fp.task(&id("gather")).unwrap();
    assert_eq!(gather.depends_on, variants);
    assert_eq!(
        gather.needs_outputs,
        [local_ci::dag::NeedsOutput {
            job: "m".into(),
            name: "o".into()
        }]
    );
    let v1 = fp.task(&variants[1]).unwrap();
    assert_eq!(v1.variant.as_ref().unwrap().label, "2");
    // a matrix that is an expression is expanded when it runs
    let (_t, _d, dynamic) = plan_of(
        "  p:\n    runs-on: x\n    outputs:\n      m: ${{ steps.s.outputs.m }}\n    steps:\n      - id: s\n        run: echo 'm=[1,2]' >> \"$GITHUB_OUTPUT\"\n  q:\n    needs: p\n    runs-on: x\n    strategy:\n      matrix:\n        v: ${{ fromJSON(needs.p.outputs.m) }}\n    steps:\n      - run: echo ${{ matrix.v }}\n",
    );
    assert!(matches!(
        dynamic.graph.node(&id("q")).unwrap().kind,
        NodeKind::DynamicMatrix { .. }
    ));
    assert!(dynamic.task(&id("q")).unwrap().dynamic_matrix);
}

#[test]
fn a_workflow_with_a_cycle_or_a_needs_of_nothing_is_skipped_and_the_others_run() {
    let good = "name: good\non: pull_request\njobs:\n  g:\n    runs-on: x\n    steps:\n      - run: echo good\n";
    let cycle = "name: cycle\non: pull_request\njobs:\n  a:\n    needs: b\n    runs-on: x\n    steps:\n      - run: echo a\n  b:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo b\n";
    let nothing = "name: nothing\non: pull_request\njobs:\n  a:\n    needs: nope\n    runs-on: x\n    steps:\n      - run: echo a\n";
    let (tmp, dir) = repo_of(&[
        ("a-cycle.yml", cycle),
        ("good.yml", good),
        ("nothing.yml", nothing),
    ]);
    let fp = local_ci::build_frozen_plan(&ctx(tmp.path()), &target(&dir), &mut Quiet).unwrap();
    // only the workflow that can run is planned, with its tasks
    let files: Vec<&str> = fp.workflows.iter().map(|w| w.file.as_str()).collect();
    assert_eq!(files, [".github/workflows/good.yml"]);
    assert_eq!(fp.units.len(), 1);
    let ids: Vec<&str> = fp.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, [".github/workflows/good.yml::g"]);
    // the others are skipped, each with the job and the needs that broke it
    let why = |f: &str| {
        fp.skipped
            .iter()
            .find(|s| s.workflow == format!(".github/workflows/{f}"))
            .unwrap_or_else(|| panic!("{f} is not in {:?}", fp.skipped))
            .reason
            .clone()
    };
    let r = why("a-cycle.yml");
    assert!(
        r.contains("cycle") && r.contains("::a") && r.contains("::b"),
        "{r}"
    );
    let r = why("nothing.yml");
    assert!(
        r.contains("job `a`") && r.contains("needs `nope`") && r.contains("nothing.yml"),
        "{r}"
    );
    // it is a plan like any other: it verifies, and survives the engine's view
    fp.verify().unwrap();
    assert_eq!(fp.with_plan(fp.to_plan()).unwrap(), fp);
    // and the workflow that remains runs
    let mut lab = Lab::from_plan(tmp, dir, fp);
    let r = lab
        .run(".github/workflows/good.yml::g", &TaskInputs::default())
        .unwrap();
    assert_eq!(r.outcomes[0].status, JobStatus::Success);
}

#[test]
fn jobs_that_write_the_same_file_stay_one_task_and_say_why() {
    let (_t, _d, fp) = plan_of(
        "  lint:\n    runs-on: x\n    steps:\n      - run: echo lint\n  a:\n    runs-on: x\n    steps:\n      - run: echo a > shared.txt\n  b:\n    runs-on: x\n    steps:\n      - run: echo b > shared.txt\n  c:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo c\n",
    );
    // engine units: {lint} and {a, b, c}
    assert_eq!(fp.units.len(), 2);
    let ids: Vec<&str> = fp.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, [id("lint"), format!("{F}::a+b"), id("c")]);
    let ab = fp.task(&format!("{F}::a+b")).unwrap();
    assert_eq!(ab.jobs, ["a", "b"]);
    assert!(ab.workspace.shared_by_jobs);
    assert_eq!(
        ab.grouped_reason,
        Some(GroupedReason::SharedOutputs {
            files: vec!["shared.txt".into()]
        })
    );
    assert_eq!(
        fp.task(&id("c")).unwrap().depends_on,
        std::slice::from_ref(&ab.id)
    );
    assert!(fp.task(&id("lint")).unwrap().grouped_reason.is_none());
    // the shard group records that the shared workspace is a reason not to split
    let sg = fp.shard_groups.iter().find(|s| s.id == ab.id).unwrap();
    assert!(sg
        .not_split
        .contains(&local_ci::dag::NotSplit::SharedWorkspace));
}

#[test]
fn a_job_between_two_that_share_a_file_keeps_the_whole_group_together() {
    let (_t, _d, fp) = plan_of(
        "  a:\n    runs-on: x\n    steps:\n      - run: echo a > s.txt\n  mid:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo m\n  b:\n    needs: mid\n    runs-on: x\n    steps:\n      - run: echo b > s.txt\n",
    );
    assert_eq!(fp.tasks.len(), 1);
    assert_eq!(fp.tasks[0].jobs, ["a", "mid", "b"]);
    assert_eq!(
        fp.tasks[0].grouped_reason,
        Some(GroupedReason::OrderAroundSharedOutputs)
    );
}

#[test]
fn the_plan_survives_its_wire_form_and_a_round_trip_through_the_engine() {
    let (_t, _d, fp) = plan_of(MATRIX);
    let back = FrozenPlan::from_json(&fp.to_canonical_json().unwrap()).unwrap();
    assert_eq!(back, fp);
    // wire -> engine -> wire changes nothing, digest included
    let again = fp.with_plan(fp.to_plan()).unwrap();
    assert_eq!(again.digest, fp.digest);
    assert_eq!(again, fp);
    // and a matrix with keys YAML reads as numbers or null plans and round-trips
    let (_t, _d, odd) = plan_of(
        "  m:\n    runs-on: x\n    strategy:\n      matrix:\n        1.5: [a]\n        null: [b]\n        os: [x]\n    steps:\n      - run: echo hi\n",
    );
    let text = odd.to_canonical_json().unwrap();
    assert_eq!(FrozenPlan::from_json(&text).unwrap(), odd);
    assert_eq!(odd.with_plan(odd.to_plan()).unwrap(), odd);
}

// ---- running tasks ----

const WORKFLOW: &str = "  prep:\n    runs-on: x\n    outputs:\n      v: ${{ steps.s.outputs.v }}\n    steps:\n      - id: s\n        run: echo \"v=hello\" >> \"$GITHUB_OUTPUT\"\n  use:\n    needs: prep\n    runs-on: x\n    steps:\n      - run: test \"${{ needs.prep.outputs.v }}\" = hello\n  broken:\n    runs-on: x\n    steps:\n      - run: exit 3\n  after_broken:\n    needs: broken\n    runs-on: x\n    steps:\n      - run: echo not reached\n  cleanup:\n    needs: broken\n    if: always()\n    runs-on: x\n    steps:\n      - run: echo cleanup\n  only_on_failure:\n    needs: broken\n    if: failure()\n    runs-on: x\n    steps:\n      - run: echo saw failure\n  only_on_success:\n    needs: prep\n    if: always() && needs.prep.result == 'skipped'\n    runs-on: x\n    steps:\n      - run: echo never\n  m:\n    runs-on: x\n    strategy:\n      matrix:\n        n: [1, 2, 3]\n    outputs:\n      o: ${{ steps.s.outputs.o }}\n    steps:\n      - id: s\n        run: echo \"o=${{ matrix.n }}\" >> \"$GITHUB_OUTPUT\"\n  gather:\n    needs: m\n    runs-on: x\n    steps:\n      - run: test \"${{ needs.m.outputs.o }}\" = 3\n";

struct Lab {
    tmp: tempfile::TempDir,
    dir: PathBuf,
    fp: FrozenPlan,
    cache: PathBuf,
    n: usize,
}

impl Lab {
    fn new(jobs: &str) -> Self {
        let (tmp, dir, fp) = plan_of(jobs);
        Self::from_plan(tmp, dir, fp)
    }

    fn from_plan(tmp: tempfile::TempDir, dir: PathBuf, fp: FrozenPlan) -> Self {
        let cache = tmp.path().join("lab");
        std::fs::create_dir_all(cache.join("locks")).unwrap();
        Lab {
            tmp,
            dir,
            fp,
            cache,
            n: 0,
        }
    }

    /// Run `task` in a checkout of its own.
    fn run(&mut self, task: &str, inputs: &TaskInputs) -> anyhow::Result<TaskResult> {
        let c = ctx(self.tmp.path());
        self.run_in(&c, task, inputs)
    }

    /// Run `task` in a checkout of its own, under `c`.
    fn run_in(
        &mut self,
        c: &ExecContext,
        task: &str,
        inputs: &TaskInputs,
    ) -> anyhow::Result<TaskResult> {
        self.n += 1;
        let name = format!("t{}", self.n);
        let wt = self.cache.join("wt").join(&name);
        local_ci::git::add_worktree(c, &self.dir, &wt, &self.fp.source.sha).unwrap();
        let ws = local_ci::exec::Workspace {
            root: wt.clone(),
            repo_root: self.dir.clone(),
            run_dir: self.cache.join("runs").join(&name),
            tools: self.cache.join("wt").join(format!("{name}.tools")),
            lock_file: self.cache.join("locks/lab.lock"),
        };
        std::fs::create_dir_all(&ws.run_dir).unwrap();
        let opts = local_ci::exec::Opts {
            parallel: 1,
            canirun: None,
            portool: None,
            containers: 1,
            job_timeout_min: 5.0,
            keep: false,
            trash: self.cache.join("trash"),
        };
        let r = local_ci::execute_task(c, &self.fp, task, inputs, None, &ws, opts);
        local_ci::git::remove_worktree(c, &self.dir, &wt, &self.cache.join("trash"));
        r
    }

    /// All the tasks, each in a checkout of its own, passing results on as
    /// inputs. `reverse_variants`: matrix variants run last to first.
    fn run_all(&mut self, reverse_variants: bool) -> (BTreeMap<String, Expected>, TaskInputs) {
        let mut inputs = TaskInputs::default();
        let mut results = BTreeMap::new();
        let mut pending: BTreeMap<String, Vec<JobOutcome>> = BTreeMap::new();
        let tasks = self.fp.tasks.clone();
        let mut i = 0;
        while i < tasks.len() {
            // a matrix's variants are consecutive
            let mut batch = vec![&tasks[i]];
            while i + batch.len() < tasks.len()
                && tasks[i].variant.is_some()
                && tasks[i + batch.len()].variant.as_ref().map(|v| &v.group)
                    == tasks[i].variant.as_ref().map(|v| &v.group)
            {
                batch.push(&tasks[i + batch.len()]);
            }
            i += batch.len();
            if reverse_variants {
                batch.reverse();
            }
            for t in batch {
                let r = self.run(&t.id, &inputs).unwrap();
                for j in &r.jobs {
                    results.insert(
                        j.display.clone(),
                        (
                            j.outcome.clone(),
                            j.steps
                                .iter()
                                .map(|s| (s.label.clone(), s.outcome.clone()))
                                .collect(),
                        ),
                    );
                }
                for o in r.outcomes {
                    match &t.variant {
                        Some(v) => pending.entry(v.group.clone()).or_default().push(o),
                        None => {
                            inputs.needs.insert(o.job.clone(), o);
                        }
                    }
                }
            }
            for (group, parts) in std::mem::take(&mut pending) {
                let node = self.fp.graph.node(&group).unwrap();
                let whole = local_ci::aggregate_matrix(node, &parts).unwrap();
                inputs.needs.insert(whole.job.clone(), whole);
            }
        }
        (results, inputs)
    }
}

/// A job as it ends up, written out by hand from the fixture: how it ended, and
/// the steps it ran as (label, how each ended).
type Expected = (Outcome, Vec<(String, Outcome)>);

#[test]
fn tasks_run_one_by_one_in_checkouts_of_their_own_give_what_the_fixture_says() {
    let mut lab = Lab::new(WORKFLOW);
    let (got, inputs) = lab.run_all(false);

    let passed =
        |label: &str| -> Expected { (Outcome::Pass, vec![(label.to_string(), Outcome::Pass)]) };
    let skipped = |why: &str| -> Expected { (Outcome::Skip(why.into()), vec![]) };
    let out_step = |n: u32| format!("echo \"o={n}\" >> \"$GITHUB_OUTPUT\"");
    let want: BTreeMap<String, Expected> = [
        ("prep", passed("echo \"v=hello\" >> \"$GITHUB_OUTPUT\"")),
        ("use", passed("test \"hello\" = hello")),
        (
            "broken",
            (Outcome::Fail, vec![("exit 3".to_string(), Outcome::Fail)]),
        ),
        ("after_broken", skipped("needs failed")),
        ("cleanup", passed("echo cleanup")),
        ("only_on_failure", passed("echo saw failure")),
        ("only_on_success", skipped("if: false")),
        ("m (1)", passed(&out_step(1))),
        ("m (2)", passed(&out_step(2))),
        ("m (3)", passed(&out_step(3))),
        ("gather", passed("test \"3\" = 3")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(got, want);

    // what each job came to, as the jobs after it saw it: needs.<id>.result,
    // its outputs, and why it did not run
    let out_up = |job: &str, status, outputs: &[(&str, &str)], skipped: Option<&str>, up: bool| {
        (
            job.to_string(),
            JobOutcome {
                job: job.to_string(),
                variant: None,
                status,
                outputs: outputs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                skipped: skipped.map(String::from),
                ancestor_failed: up,
            },
        )
    };
    let out = |job: &str, status, outputs: &[(&str, &str)], skipped: Option<&str>| {
        out_up(job, status, outputs, skipped, false)
    };
    // `up`: a job it needs, directly or not, failed
    use JobStatus::*;
    let want_needs: BTreeMap<String, JobOutcome> = [
        out("prep", Success, &[("v", "hello")], None),
        out("use", Success, &[], None),
        out("broken", Failure, &[], None),
        out_up("after_broken", Skipped, &[], Some("needs failed"), true),
        out_up("cleanup", Success, &[], None, true),
        out_up("only_on_failure", Success, &[], None, true),
        out("only_on_success", Skipped, &[], Some("if: false")),
        out("m", Success, &[("o", "3")], None),
        out("gather", Success, &[], None),
    ]
    .into();
    assert_eq!(inputs.needs, want_needs);
}

#[test]
fn matrix_outputs_follow_the_matrix_whatever_order_the_variants_finish_in() {
    let mut lab = Lab::new(WORKFLOW);
    let (_, forward) = lab.run_all(false);
    let (_, backward) = lab.run_all(true);
    assert_eq!(forward.needs["m"].outputs["o"], "3");
    assert_eq!(forward.needs["m"], backward.needs["m"]);
    assert_eq!(forward, backward);

    // shuffled arrival at the aggregation point gives the same job
    let node = lab.fp.graph.node(&id("m")).unwrap().clone();
    let part = |i: usize| JobOutcome {
        job: "m".into(),
        variant: Some(i),
        status: JobStatus::Success,
        outputs: [("o".to_string(), format!("v{i}"))].into(),
        skipped: None,
        ancestor_failed: false,
    };
    let a = local_ci::aggregate_matrix(&node, &[part(0), part(1), part(2)]).unwrap();
    let b = local_ci::aggregate_matrix(&node, &[part(2), part(0), part(1)]).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.outputs["o"], "v2");
    // a missing or doubled variant is refused
    assert!(local_ci::aggregate_matrix(&node, &[part(0), part(1)]).is_err());
    assert!(local_ci::aggregate_matrix(&node, &[part(0), part(1), part(1)]).is_err());
}

#[test]
fn a_task_reads_needs_outputs_and_results_from_its_inputs() {
    let mut lab = Lab::new(WORKFLOW);
    let given = |status, v: &str| TaskInputs {
        needs: [(
            "prep".to_string(),
            JobOutcome {
                job: "prep".into(),
                variant: None,
                status,
                outputs: [("v".to_string(), v.to_string())].into(),
                skipped: None,
                ancestor_failed: false,
            },
        )]
        .into(),
        ..Default::default()
    };
    // the output the caller gives is the one the step sees
    let ok = lab
        .run(&id("use"), &given(JobStatus::Success, "hello"))
        .unwrap();
    assert_eq!(ok.outcomes[0].status, JobStatus::Success);
    let wrong = lab
        .run(&id("use"), &given(JobStatus::Success, "bye"))
        .unwrap();
    assert_eq!(wrong.outcomes[0].status, JobStatus::Failure);
    // a needed job that did not succeed stops the job, and the result says so
    let skipped = lab
        .run(&id("use"), &given(JobStatus::Failure, "hello"))
        .unwrap();
    assert_eq!(skipped.outcomes[0].status, JobStatus::Skipped);
    assert_eq!(skipped.outcomes[0].skipped.as_deref(), Some("needs failed"));
    assert!(skipped.jobs[0].steps.is_empty());
    // a task is not run without the results it needs, and no other job starts
    let e = lab.run(&id("use"), &TaskInputs::default()).unwrap_err();
    assert!(format!("{e:#}").contains("prep"), "{e:#}");
    assert!(lab.run("no-such-task", &TaskInputs::default()).is_err());
}

const RESULTS: &str = "  prep:\n    runs-on: x\n    steps:\n      - run: echo prep\n  on_cancelled:\n    needs: prep\n    if: cancelled()\n    runs-on: x\n    steps:\n      - run: echo c\n  on_failure:\n    needs: prep\n    if: failure()\n    runs-on: x\n    steps:\n      - run: echo f\n  on_result:\n    needs: prep\n    if: always() && needs.prep.result == 'cancelled'\n    runs-on: x\n    steps:\n      - run: echo r\n  on_default:\n    needs: prep\n    runs-on: x\n    steps:\n      - run: echo d\n";

/// The inputs of a task that needs `prep`, which ended as `status`.
fn prep_as(status: JobStatus) -> TaskInputs {
    let prep = JobOutcome {
        job: "prep".into(),
        variant: None,
        status,
        outputs: BTreeMap::new(),
        skipped: None,
        ancestor_failed: false,
    };
    TaskInputs::new([&prep], [&prep], false)
}

#[test]
fn cancelled_failure_and_the_result_of_a_needed_job_are_read_from_the_inputs() {
    let mut lab = Lab::new(RESULTS);
    // (status of prep, job) -> did the job run?
    let ran = |lab: &mut Lab, status, job: &str| {
        let r = lab.run(&id(job), &prep_as(status)).unwrap();
        match r.outcomes[0].status {
            JobStatus::Success => true,
            JobStatus::Skipped => false,
            other => panic!("{job} after {status:?}: {other:?}"),
        }
    };
    use JobStatus::*;
    for (status, on_cancelled, on_failure, on_result, on_default) in [
        (Success, false, false, false, true),
        (Failure, false, true, false, false),
        (Cancelled, true, false, true, false),
    ] {
        assert_eq!(
            ran(&mut lab, status, "on_cancelled"),
            on_cancelled,
            "{status:?}"
        );
        assert_eq!(
            ran(&mut lab, status, "on_failure"),
            on_failure,
            "{status:?}"
        );
        assert_eq!(ran(&mut lab, status, "on_result"), on_result, "{status:?}");
        assert_eq!(
            ran(&mut lab, status, "on_default"),
            on_default,
            "{status:?}"
        );
    }
    // a skipped job is not a cancelled one
    assert!(!ran(&mut lab, Skipped, "on_cancelled"));
    assert!(!ran(&mut lab, Skipped, "on_result"));
}

#[test]
fn a_job_cancelled_while_it_runs_is_cancelled_for_the_jobs_after_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mark = tmp.path().join("started");
    let mut lab = Lab::new(&format!(
        "  slow:\n    runs-on: x\n    steps:\n      - run: touch '{}' && sleep 60\n  on_failure:\n    needs: slow\n    if: failure()\n    runs-on: x\n    steps:\n      - run: echo f\n  on_cancelled:\n    needs: slow\n    if: cancelled()\n    runs-on: x\n    steps:\n      - run: echo c\n",
        mark.display()
    ));
    let c = ctx(lab.tmp.path());
    let cancel = c.cancel.clone();
    let m = mark.clone();
    let canceller = std::thread::spawn(move || {
        for _ in 0..600 {
            if m.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        cancel.cancel();
    });
    let slow = lab.run_in(&c, &id("slow"), &TaskInputs::default()).unwrap();
    canceller.join().unwrap();
    assert!(mark.exists(), "the step never started");
    // the step was stopped; a local run still reports it as it did
    assert!(matches!(slow.jobs[0].outcome, Outcome::Error(_)));
    assert!(slow.jobs[0].cancelled);
    assert_eq!(slow.outcomes[0].status, JobStatus::Cancelled);

    // the jobs after it: the run was cancelled, so cancelled() holds
    let inputs = TaskInputs::new(&slow.outcomes, &slow.outcomes, false);
    assert!(inputs.run_cancelled);
    let f = lab.run(&id("on_failure"), &inputs).unwrap();
    assert_eq!(f.outcomes[0].status, JobStatus::Skipped);
    assert_eq!(f.outcomes[0].skipped.as_deref(), Some("if: false"));
    let c = lab.run(&id("on_cancelled"), &inputs).unwrap();
    assert_eq!(c.outcomes[0].status, JobStatus::Success);
}

#[test]
fn a_matrix_is_as_failed_as_its_worst_entry_then_as_cancelled() {
    let lab = Lab::new(MATRIX);
    let node = lab.fp.graph.node(&id("m")).unwrap().clone();
    let part = |i: usize, status| JobOutcome {
        job: "m".into(),
        variant: Some(i),
        status,
        outputs: BTreeMap::new(),
        skipped: None,
        ancestor_failed: false,
    };
    use JobStatus::*;
    let of = |a, b, c| {
        local_ci::aggregate_matrix(&node, &[part(0, a), part(1, b), part(2, c)])
            .unwrap()
            .status
    };
    assert_eq!(of(Success, Success, Success), Success);
    assert_eq!(of(Success, Cancelled, Success), Cancelled);
    assert_eq!(of(Cancelled, Skipped, Success), Cancelled);
    assert_eq!(of(Skipped, Skipped, Cancelled), Cancelled);
    assert_eq!(of(Cancelled, Failure, Success), Failure);
    assert_eq!(of(Skipped, Skipped, Skipped), Skipped);
    assert_eq!(of(Success, Skipped, Skipped), Success);
    assert_eq!(of(Success, Failure, Skipped), Failure);
}

#[test]
fn jobs_keep_the_order_of_the_workflow_when_a_job_sits_between_two_that_share_a_file() {
    // a and c write the same file, b needs a: cutting {a, c} and {b} apart
    // would run c before b
    let (_t, _d, fp) = plan_of(
        "  a:\n    runs-on: x\n    steps:\n      - run: echo a > s.txt\n  b:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo b\n  c:\n    runs-on: x\n    steps:\n      - run: echo c > s.txt\n",
    );
    let order: Vec<&str> = fp
        .tasks
        .iter()
        .flat_map(|t| t.jobs.iter().map(String::as_str))
        .collect();
    assert_eq!(order, ["a", "b", "c"]);
    assert_eq!(fp.units[0].jobs, ["a", "b", "c"]);
    assert_eq!(
        fp.tasks[0].grouped_reason,
        Some(GroupedReason::OrderAroundSharedOutputs)
    );
    // independent jobs that do not share a file are still apart, in order
    let (_t, _d, fp) = plan_of(
        "  a:\n    runs-on: x\n    steps:\n      - run: echo a\n  b:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo b\n  c:\n    runs-on: x\n    steps:\n      - run: echo c\n",
    );
    let order: Vec<&str> = fp.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(order, [id("a"), id("b"), id("c")]);
}

#[test]
fn needs_outputs_are_found_however_they_are_spelled() {
    let (_t, _d, fp) = plan_of(
        r#"  p:
    runs-on: x
    outputs:
      x: ${{ steps.s.outputs.x }}
      y: ${{ steps.s.outputs.y }}
      z: ${{ steps.s.outputs.z }}
    steps:
      - id: s
        run: echo "x=1" >> "$GITHUB_OUTPUT"
  dot:
    needs: p
    runs-on: x
    steps:
      - run: echo ${{ needs.p.outputs.x }}
  bracket:
    needs: p
    runs-on: x
    steps:
      - run: echo ${{ needs['p'].outputs['y'] }}
  mixed:
    needs: p
    runs-on: x
    steps:
      - run: echo ${{ needs.p.outputs['z'] }}
  in_if:
    needs: p
    if: needs.p.outputs.x == '1'
    runs-on: x
    steps:
      - run: echo hi
  whole_outputs:
    needs: p
    runs-on: x
    steps:
      - run: echo ${{ toJSON(needs.p.outputs) }}
  whole_needs:
    needs: p
    runs-on: x
    steps:
      - run: echo '${{ toJSON(needs) }}'
  computed:
    needs: p
    runs-on: x
    steps:
      - run: echo ${{ needs.p.outputs[format('{0}', 'y')] }}
  result_only:
    needs: p
    runs-on: x
    steps:
      - run: echo ${{ needs.p.result }}
"#,
    );
    let of = |job: &str| -> Vec<String> {
        fp.task(&id(job))
            .unwrap()
            .needs_outputs
            .iter()
            .map(|o| format!("{}.{}", o.job, o.name))
            .collect()
    };
    assert_eq!(of("dot"), ["p.x"]);
    assert_eq!(of("bracket"), ["p.y"]);
    assert_eq!(of("mixed"), ["p.z"]);
    assert_eq!(of("in_if"), ["p.x"]);
    // not named: every output the job declares
    assert_eq!(of("whole_outputs"), ["p.x", "p.y", "p.z"]);
    assert_eq!(of("whole_needs"), ["p.x", "p.y", "p.z"]);
    assert_eq!(of("computed"), ["p.x", "p.y", "p.z"]);
    assert!(of("result_only").is_empty());
}

#[test]
fn a_matrix_with_two_keys_that_print_alike_is_not_planned() {
    let good = "name: good\non: pull_request\njobs:\n  g:\n    runs-on: x\n    steps:\n      - run: echo good\n";
    let twins = "name: twins\non: pull_request\njobs:\n  m:\n    runs-on: x\n    strategy:\n      matrix:\n        1: [a]\n        \"1\": [b]\n    steps:\n      - run: echo hi\n";
    let (tmp, dir) = repo_of(&[("good.yml", good), ("twins.yml", twins)]);
    let fp = local_ci::build_frozen_plan(&ctx(tmp.path()), &target(&dir), &mut Quiet).unwrap();
    let files: Vec<&str> = fp.workflows.iter().map(|w| w.file.as_str()).collect();
    assert_eq!(files, [".github/workflows/good.yml"]);
    let sk = &fp.skipped[0];
    assert_eq!(sk.workflow, ".github/workflows/twins.yml");
    assert!(
        sk.reason.contains("job `m`")
            && sk.reason.contains("two keys")
            && sk.reason.contains("`1`"),
        "{}",
        sk.reason
    );
    // what is planned goes through the engine's view and back unchanged
    assert_eq!(fp.with_plan(fp.to_plan()).unwrap(), fp);
}

// ---- planning without the network ----

fn fake_gh(dir: &Path, marker: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let gh = dir.join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho called >> '{}'\nprintf '{F}\\tdisabled_manually\\n'\n",
            marker.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_given_list_of_disabled_workflows_is_used_and_the_network_is_not() {
    let (tmp, dir) = repo(CHAIN);
    git(
        &dir,
        &[
            "remote",
            "add",
            "origin",
            "git@example.com:acme/widgets.git",
        ],
    );
    let bin = tmp.path().join("bin");
    let marker = tmp.path().join("gh-called");
    fake_gh(&bin, &marker);
    let c = ctx_with(tmp.path(), &format!("{}:/usr/bin:/bin", bin.display()));
    let mut t = target(&dir);
    t.include_disabled = false;

    // a list that does not name the workflow: planned, gh never asked
    let other = vec![DisabledWorkflow {
        path: ".github/workflows/other.yml".into(),
        state: "disabled_manually".into(),
    }];
    let planned =
        local_ci::build_frozen_plan_with(&c, &t, Some(other.clone()), &mut Quiet).unwrap();
    assert!(!marker.exists(), "the network was asked");
    assert_eq!(planned.units.len(), 1);
    assert_eq!(planned.disabled.source, DisabledSource::Provided);
    assert_eq!(planned.disabled.workflows, other);

    // a list that names it: skipped, with the reason, gh still not asked
    let named = vec![DisabledWorkflow {
        path: F.into(),
        state: "disabled_manually".into(),
    }];
    let skipped = local_ci::build_frozen_plan_with(&c, &t, Some(named), &mut Quiet).unwrap();
    assert!(!marker.exists());
    assert!(skipped.units.is_empty());
    assert_eq!(skipped.skipped[0].reason, "disabled manually on GitHub");
    assert_ne!(skipped.digest, planned.digest);

    // the same inputs give the same plan
    let again = local_ci::build_frozen_plan_with(&c, &t, Some(other), &mut Quiet).unwrap();
    assert_eq!(again.digest, planned.digest);

    // without a list, gh is asked and the answer is recorded
    let asked = local_ci::build_frozen_plan(&c, &t, &mut Quiet).unwrap();
    assert!(marker.exists());
    assert_eq!(asked.disabled.source, DisabledSource::Github);
    assert!(asked.units.is_empty());
    // and replaying what it said, offline, gives that plan
    let replay = local_ci::build_frozen_plan_with(
        &c,
        &t,
        Some(asked.disabled.workflows.clone()),
        &mut Quiet,
    )
    .unwrap();
    assert_eq!(replay.skipped, asked.skipped);
    assert_eq!(replay.units, asked.units);
}

// ---- the context of an attempt ----

#[test]
fn a_fresh_attempt_is_not_cancelled_with_the_one_it_came_from() {
    let a = ctx(Path::new("/tmp"));
    let b = a.fresh_attempt();
    a.cancel.cancel();
    assert!(a.cancel.is_cancelled());
    assert!(!b.cancel.is_cancelled());
    assert_eq!(a.env(), b.env());
}

// ---- status functions and `needs` as GitHub reads them ----

/// What a job came to: its status and why it did not run.
type Came = (JobStatus, Option<String>);

fn came(status: JobStatus, why: Option<&str>) -> Came {
    (status, why.map(String::from))
}

impl Lab {
    /// The tasks one by one, each given only the outcomes of the jobs it needs
    /// directly (what a coordinator that placed them on other hosts would hand
    /// over).
    fn run_distributed(&mut self) -> BTreeMap<String, Came> {
        let tasks = self.fp.tasks.clone();
        let mut done: BTreeMap<String, JobOutcome> = BTreeMap::new();
        let mut pending: BTreeMap<String, Vec<JobOutcome>> = BTreeMap::new();
        let mut i = 0;
        while i < tasks.len() {
            let mut batch = vec![&tasks[i]];
            while i + batch.len() < tasks.len()
                && tasks[i].variant.is_some()
                && tasks[i + batch.len()].variant.as_ref().map(|v| &v.group)
                    == tasks[i].variant.as_ref().map(|v| &v.group)
            {
                batch.push(&tasks[i + batch.len()]);
            }
            i += batch.len();
            for t in batch {
                let direct: Vec<&JobOutcome> = tasks
                    .iter()
                    .filter(|d| t.depends_on.contains(&d.id))
                    .flat_map(|d| d.jobs.iter())
                    .filter(|j| !t.jobs.contains(j))
                    .filter_map(|j| done.get(j))
                    .collect();
                let inputs = TaskInputs::new(direct, done.values(), false);
                let r = self.run(&t.id, &inputs).unwrap();
                for o in r.outcomes {
                    match &t.variant {
                        Some(v) => pending.entry(v.group.clone()).or_default().push(o),
                        None => {
                            done.insert(o.job.clone(), o);
                        }
                    }
                }
            }
            for (group, parts) in std::mem::take(&mut pending) {
                let node = self.fp.graph.node(&group).unwrap();
                let whole = local_ci::aggregate_matrix(node, &parts).unwrap();
                done.insert(whole.job.clone(), whole);
            }
        }
        done.into_iter()
            .map(|(k, o)| (k, (o.status, o.skipped)))
            .collect()
    }

    /// The whole plan in one local run: how each job (a matrix as a whole,
    /// from its entries) ended.
    fn run_local(&self) -> BTreeMap<String, Came> {
        let c = ctx(self.tmp.path());
        let cache = self.tmp.path().join("local");
        let wt = cache.join("wt/all");
        local_ci::git::add_worktree(&c, &self.dir, &wt, &self.fp.source.sha).unwrap();
        let ws = local_ci::exec::Workspace {
            root: wt,
            repo_root: self.dir.clone(),
            run_dir: cache.join("runs/all"),
            tools: cache.join("wt/all.tools"),
            lock_file: cache.join("locks/all.lock"),
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
        let r = local_ci::execute_plan(&c, &self.fp, &ws, opts).unwrap();
        let mut parts: BTreeMap<String, Vec<JobOutcome>> = BTreeMap::new();
        for j in r.units.iter().flat_map(|u| u.jobs.iter()) {
            let (status, skipped) = match &j.outcome {
                Outcome::Pass => (JobStatus::Success, None),
                Outcome::Fail | Outcome::Error(_) => (JobStatus::Failure, None),
                Outcome::Skip(why) => (JobStatus::Skipped, Some(why.clone())),
            };
            let variant = Some(parts.get(&j.job).map_or(0, Vec::len));
            parts.entry(j.job.clone()).or_default().push(JobOutcome {
                job: j.job.clone(),
                variant,
                status,
                outputs: BTreeMap::new(),
                skipped,
                ancestor_failed: false,
            });
        }
        parts
            .into_iter()
            .map(|(k, p)| {
                let whole = if p.len() == 1 {
                    p[0].clone()
                } else {
                    let node = self.fp.graph.node(&id(&k)).unwrap();
                    local_ci::aggregate_matrix(node, &p).unwrap()
                };
                (k, (whole.status, whole.skipped))
            })
            .collect()
    }
}

/// The jobs of `jobs` come to `want`, in a local run and task by task alike.
fn check_both(jobs: &str, want: &[(&str, JobStatus, Option<&str>)]) {
    let mut lab = Lab::new(jobs);
    let want: BTreeMap<String, Came> = want
        .iter()
        .map(|(j, s, w)| (j.to_string(), came(*s, *w)))
        .collect();
    let pick = |got: BTreeMap<String, Came>| -> BTreeMap<String, Came> {
        got.into_iter()
            .filter(|(k, _)| want.contains_key(k))
            .collect()
    };
    assert_eq!(pick(lab.run_distributed()), want, "task by task");
    assert_eq!(pick(lab.run_local()), want, "local run");
}

fn step(job: &str, needs: &str, cond: &str, run: &str) -> String {
    let needs = if needs.is_empty() {
        String::new()
    } else {
        format!("    needs: [{needs}]\n")
    };
    let cond = if cond.is_empty() {
        String::new()
    } else {
        format!("    if: {cond}\n")
    };
    format!("  {job}:\n{needs}{cond}    runs-on: x\n    steps:\n      - run: {run}\n")
}

#[test]
fn a_failure_function_is_not_true_after_a_job_that_was_only_skipped() {
    // p is skipped on a pull request; n needs it and asks failure(): nothing failed
    let jobs = [
        step("p", "", "github.event_name == 'push'", "echo p"),
        step("n", "p", "failure()", "echo notify"),
        step("d", "p", "", "echo d"),
        step("al", "p", "always()", "echo al"),
        step("ok", "p", "needs.p.result == 'skipped'", "echo ok"),
    ]
    .concat();
    use JobStatus::*;
    check_both(
        &jobs,
        &[
            ("p", Skipped, Some("if: false")),
            ("n", Skipped, Some("if: false")),
            ("d", Skipped, Some("needs skipped")),
            ("al", Success, None),
            // an explicit condition without a status function is still gated by success()
            ("ok", Skipped, Some("needs skipped")),
        ],
    );
}

#[test]
fn failure_looks_through_a_skipped_job_to_the_failure_above_it() {
    // a fails; b is skipped for it; c asks failure(): GitHub says true
    let jobs = [
        step("a", "", "", "exit 1"),
        step("b", "a", "", "echo b"),
        step("c", "b", "failure()", "echo c"),
        step("d", "b", "", "echo d"),
        step("e", "b", "success()", "echo e"),
        step("f", "b", "always()", "echo f"),
        step("g", "b", "cancelled()", "echo g"),
        step("h", "a", "failure()", "echo h"),
    ]
    .concat();
    use JobStatus::*;
    check_both(
        &jobs,
        &[
            ("a", Failure, None),
            ("b", Skipped, Some("needs failed")),
            ("c", Success, None),
            ("d", Skipped, Some("needs skipped")),
            ("e", Skipped, Some("needs skipped")),
            ("f", Success, None),
            ("g", Skipped, Some("if: false")),
            ("h", Success, None),
        ],
    );
}

#[test]
fn a_partly_failed_matrix_is_failed_for_the_jobs_after_it() {
    let m = "  m:\n    runs-on: x\n    strategy:\n      fail-fast: false\n      matrix:\n        n: [1, 2, 3]\n    steps:\n      - run: test ${{ matrix.n }} != 2\n";
    let jobs = [
        m.to_string(),
        step("after", "m", "", "echo after"),
        step("on_fail", "m", "failure()", "echo f"),
        step("on_ok", "m", "success()", "echo ok"),
        step("then", "on_fail", "", "echo then"),
        step("late", "after", "failure()", "echo late"),
    ]
    .concat();
    use JobStatus::*;
    check_both(
        &jobs,
        &[
            ("m", Failure, None),
            ("after", Skipped, Some("needs failed")),
            ("on_fail", Success, None),
            ("on_ok", Skipped, Some("needs failed")),
            // on_fail ran, so it is a success for `then`, whatever failed above it
            ("then", Success, None),
            ("late", Success, None),
        ],
    );
}

#[test]
fn cancelled_is_the_run_being_cancelled_and_failure_is_not_cancelled() {
    let mut lab = Lab::new(
        &[
            step("a", "", "", "echo a"),
            step("on_failure", "a", "failure()", "echo f"),
            step("on_cancelled", "a", "cancelled()", "echo c"),
            step("on_default", "a", "", "echo d"),
            step("on_always", "a", "always()", "echo al"),
        ]
        .concat(),
    );
    let a = |status| JobOutcome {
        job: "a".into(),
        variant: None,
        status,
        outputs: BTreeMap::new(),
        skipped: None,
        ancestor_failed: false,
    };
    let run = |lab: &mut Lab, inputs: &TaskInputs, job: &str| {
        let o = lab.run(&id(job), inputs).unwrap().outcomes.remove(0);
        (o.status, o.skipped)
    };
    use JobStatus::*;
    // a was cancelled and the run is cancelled
    let cancelled = TaskInputs::new([&a(Cancelled)], [&a(Cancelled)], false);
    assert!(cancelled.run_cancelled);
    assert_eq!(
        run(&mut lab, &cancelled, "on_failure"),
        came(Skipped, Some("if: false"))
    );
    assert_eq!(
        run(&mut lab, &cancelled, "on_cancelled"),
        came(Success, None)
    );
    assert_eq!(
        run(&mut lab, &cancelled, "on_default"),
        came(Skipped, Some("needs cancelled"))
    );
    assert_eq!(run(&mut lab, &cancelled, "on_always"), came(Success, None));
    // the run was cancelled while a had finished: cancelled() holds all the same
    let late = TaskInputs::new([&a(Success)], [&a(Success)], true);
    assert_eq!(run(&mut lab, &late, "on_cancelled"), came(Success, None));
    assert_eq!(
        run(&mut lab, &late, "on_failure"),
        came(Skipped, Some("if: false"))
    );
    // a cancelled job alone, with the run not marked cancelled, is not the run being cancelled
    let alone = TaskInputs {
        needs: [("a".to_string(), a(Cancelled))].into(),
        run_cancelled: false,
    };
    assert_eq!(
        run(&mut lab, &alone, "on_cancelled"),
        came(Skipped, Some("if: false"))
    );
    // an ancestor's failure arrives in the outcome of the job between
    let mut b = a(Skipped);
    b.ancestor_failed = true;
    let through = TaskInputs::new([&b], [&b], false);
    assert_eq!(run(&mut lab, &through, "on_failure"), came(Success, None));
}
