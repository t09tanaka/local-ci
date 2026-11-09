//! The job graph, the tasks a plan is cut into, and running a task from the
//! results of the ones before it.

use local_ci::dag::{GroupedReason, NodeKind};
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
fn a_cycle_and_a_needs_of_nothing_are_errors() {
    let (tmp, dir) = repo(
        "  a:\n    needs: b\n    runs-on: x\n    steps:\n      - run: echo a\n  b:\n    needs: a\n    runs-on: x\n    steps:\n      - run: echo b\n",
    );
    let e = local_ci::build_frozen_plan(&ctx(tmp.path()), &target(&dir), &mut Quiet).unwrap_err();
    assert!(format!("{e:#}").contains("cycle"), "{e:#}");
    let (tmp, dir) =
        repo("  a:\n    needs: nope\n    runs-on: x\n    steps:\n      - run: echo a\n");
    let e = local_ci::build_frozen_plan(&ctx(tmp.path()), &target(&dir), &mut Quiet).unwrap_err();
    let text = format!("{e:#}");
    assert!(text.contains("nope") && text.contains("needs"), "{text}");
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
        self.n += 1;
        let name = format!("t{}", self.n);
        let wt = self.cache.join("wt").join(&name);
        local_ci::git::add_worktree(&self.dir, &wt, &self.fp.source.sha).unwrap();
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
        let c = ctx(self.tmp.path());
        let r = local_ci::execute_task(&c, &self.fp, task, inputs, &ws, opts);
        local_ci::git::remove_worktree(&self.dir, &wt, &self.cache.join("trash"));
        r
    }

    /// All the tasks, each in a checkout of its own, passing results on as
    /// inputs. `reverse_variants`: matrix variants run last to first.
    fn run_all(&mut self, reverse_variants: bool) -> (BTreeMap<String, String>, TaskInputs) {
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
                        serde_json::to_string(&j.outcome).unwrap(),
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

#[test]
fn tasks_run_one_by_one_in_checkouts_of_their_own_give_what_a_run_gives() {
    let mut lab = Lab::new(WORKFLOW);
    let c = ctx(lab.tmp.path());
    let ran = local_ci::run(
        &c,
        local_ci::RunOptions {
            target: target(&lab.dir),
            parallel: Some(2),
            no_canirun: true,
            no_portool: true,
            containers: 1,
            keep: false,
            job_timeout: 5.0,
            no_cache: true,
            cache_dir: lab.tmp.path().join("run-cache"),
        },
        &mut Quiet,
    )
    .unwrap()
    .result;
    let mut want = BTreeMap::new();
    for j in ran.units.iter().flat_map(|u| &u.jobs) {
        want.insert(
            j.display.clone(),
            serde_json::to_string(&j.outcome).unwrap(),
        );
    }
    assert_eq!(want.len(), 11, "{want:?}");

    let (got, inputs) = lab.run_all(false);
    assert_eq!(got, want);
    // what each job came to, as the jobs after it saw it
    let status = |j: &str| inputs.needs[j].status;
    assert_eq!(status("prep"), JobStatus::Success);
    assert_eq!(status("use"), JobStatus::Success);
    assert_eq!(status("broken"), JobStatus::Failure);
    assert_eq!(status("after_broken"), JobStatus::Skipped);
    assert_eq!(status("cleanup"), JobStatus::Success);
    assert_eq!(status("only_on_failure"), JobStatus::Success);
    assert_eq!(status("only_on_success"), JobStatus::Skipped);
    assert_eq!(status("gather"), JobStatus::Success);
    assert_eq!(
        inputs.needs["after_broken"].skipped.as_deref(),
        Some("needs failed")
    );
    assert_eq!(
        inputs.needs["only_on_success"].skipped.as_deref(),
        Some("if: false")
    );
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
            },
        )]
        .into(),
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
    // cancelled()/needs.*.result are evaluated from the inputs too
    let sk = lab
        .run(&id("only_on_success"), &given(JobStatus::Skipped, "x"))
        .unwrap();
    assert_eq!(sk.outcomes[0].status, JobStatus::Success);
    // a task is not run without the results it needs, and no other job starts
    let e = lab.run(&id("use"), &TaskInputs::default()).unwrap_err();
    assert!(format!("{e:#}").contains("prep"), "{e:#}");
    assert!(lab.run("no-such-task", &TaskInputs::default()).is_err());
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

#[test]
fn the_commands_the_engine_starts_see_the_context_env_and_not_the_processs() {
    // Every `Command::new` in these files is followed by `env_clear()` (the
    // context's `command()` does it for the rest). git.rs, cache.rs and
    // portool.rs still start git, cp and portool with the process's
    // environment; they are not covered yet.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in ["exec.rs", "run.rs", "context.rs", "task.rs"] {
        let text = std::fs::read_to_string(src.join(file)).unwrap();
        let code = text.split("#[cfg(test)]").next().unwrap();
        for (at, _) in code.match_indices("Command::new(") {
            let near: String = code[at..].chars().take(400).collect();
            assert!(
                near.contains("env_clear()"),
                "{file}: {}",
                &near[..80.min(near.len())]
            );
        }
    }
}
