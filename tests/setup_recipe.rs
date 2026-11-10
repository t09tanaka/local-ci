//! Setup recipes: what a task prepares, the key a prepared tree is reused
//! under, the receipt of what was prepared, and immutable templates.

#![allow(clippy::disallowed_methods)] // the test builds the binary's environment

mod common;
use local_ci::exec::{Opts, Outcome, Workspace};
use local_ci::setup::{
    derive_setup_recipe, ArtifactDigest, Freshness, PreparedHow, PreparedReceipt, PreparedStep,
    RecipeStep, ReuseScope, SecretDigest, SetupRecipe, TemplateContext, TemplateKey,
};
use local_ci::template::TemplateStore;
use local_ci::{ExecContext, FrozenPlan, Quiet, Target, TaskInputs, TaskResult};
use std::os::unix::fs::MetadataExt;
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

const F: &str = ".github/workflows/ci.yml";

fn id(job: &str) -> String {
    format!("{F}::{job}")
}

const INSTALL: &str = "echo installed > installed.marker\n";

/// A repository whose one workflow is `jobs`, with the install scripts.
fn lab(jobs: &str) -> (common::Tmp, PathBuf, FrozenPlan) {
    lab_files(&[(F, format!("name: ci\non: pull_request\njobs:\n{jobs}"))])
}

/// A repository with these workflow files (path, text), and the install scripts.
fn lab_files(files: &[(&str, String)]) -> (common::Tmp, PathBuf, FrozenPlan) {
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    for (path, text) in files {
        std::fs::write(dir.join(path), text).unwrap();
    }
    for d in ["d1", "d2"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
        std::fs::write(dir.join(d).join(".keep"), "").unwrap();
    }
    std::fs::write(dir.join("install.sh"), INSTALL).unwrap();
    std::fs::write(dir.join("install2.sh"), "echo two > two.marker\n").unwrap();
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "feature"]);
    std::fs::write(dir.join("a.txt"), "b\n").unwrap();
    git(&dir, &["commit", "-qam", "change"]);
    let fp = local_ci::build_frozen_plan(&ctx(tmp.path()), &target(&dir), &mut Quiet).unwrap();
    (tmp, dir, fp)
}

/// Two jobs that start with the same install and then check for its result.
const TWO_JOBS: &str = "  a:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n      - run: test -f installed.marker\n  b:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n      - run: test -f installed.marker && echo b\n";

/// Two jobs that start with the same two installs.
const TWO_INSTALLS: &str = "  a:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n      - run: sh install2.sh\n      - run: test -f two.marker\n  b:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n      - run: sh install2.sh\n      - run: test -f two.marker && echo b\n";

/// A workspace of its own, as a task placed on another host gets.
struct Place<'a> {
    dir: &'a Path,
    fp: &'a FrozenPlan,
    cache: PathBuf,
    home: &'a Path,
}

impl<'a> Place<'a> {
    fn new(tmp: &'a common::Tmp, dir: &'a Path, fp: &'a FrozenPlan) -> Self {
        let cache = tmp.path().join("lab");
        std::fs::create_dir_all(cache.join("locks")).unwrap();
        Place {
            dir,
            fp,
            cache,
            home: tmp.path(),
        }
    }

    fn workspace(&self, name: &str) -> Workspace {
        let wt = self.cache.join("wt").join(name);
        local_ci::git::add_worktree(
            &ctx(self.home),
            self.dir,
            &wt,
            &self.fp.source.execution_commit,
        )
        .unwrap();
        let ws = Workspace {
            root: wt,
            repo_root: self.dir.to_path_buf(),
            run_dir: self.cache.join("runs").join(name),
            tools: self.cache.join("wt").join(format!("{name}.tools")),
            lock_file: self.cache.join("locks/lab.lock"),
        };
        std::fs::create_dir_all(&ws.run_dir).unwrap();
        ws
    }

    fn opts(&self) -> Opts {
        Opts {
            parallel: 1,
            canirun: None,
            portool: None,
            containers: 1,
            job_timeout_min: 5.0,
            keep: true,
            trash: self.cache.join("trash"),
        }
    }

    fn tctx(&self) -> TemplateContext {
        TemplateContext::local(self.fp, "run-1")
    }

    fn setup(&self, ws: &Workspace, recipe: &SetupRecipe) -> local_ci::setup::PreparedSetup {
        let key = recipe.template_key(&self.tctx());
        local_ci::execute_setup(&ctx(self.home), self.fp, recipe, &key, ws, self.opts()).unwrap()
    }

    fn task(&self, ws: &Workspace, task: &str, prepared: Option<&PreparedReceipt>) -> TaskResult {
        self.task_in(ws, task, prepared, &self.tctx())
    }

    fn task_in(
        &self,
        ws: &Workspace,
        task: &str,
        prepared: Option<&PreparedReceipt>,
        tctx: &TemplateContext,
    ) -> TaskResult {
        local_ci::execute_task(
            &ctx(self.home),
            self.fp,
            task,
            &TaskInputs::default(),
            prepared.map(|r| (r, tctx)),
            ws,
            self.opts(),
        )
        .unwrap()
    }
}

fn outcomes(r: &TaskResult) -> Vec<Outcome> {
    r.jobs[0].steps.iter().map(|s| s.outcome.clone()).collect()
}

fn skipped_setup() -> Outcome {
    Outcome::Skip("setup".into())
}

// ---- the recipe ----

#[test]
fn every_task_has_the_installs_it_starts_with_and_the_plan_keeps_the_steps() {
    let (_t, _d, fp) = lab(TWO_JOBS);
    // the plan says nothing about who does the setup: no `execute` flags or
    // owners in what a host is sent
    let text = fp.to_canonical_json().unwrap();
    assert!(!text.contains("\"execute\""), "{text}");
    assert!(!text.contains("needs_hoisted_setup"), "{text}");
    let a = derive_setup_recipe(&fp, &id("a")).unwrap();
    let b = derive_setup_recipe(&fp, &id("b")).unwrap();
    for r in [&a, &b] {
        assert_eq!(r.steps.len(), 1);
        assert_eq!(r.steps[0].run, "sh install.sh");
        assert_eq!(r.steps[0].dir, "");
    }
    // the same install is the same step in both
    assert_eq!(a.steps[0].id, b.steps[0].id);
    assert_eq!(a.steps[0].at[0].job, "a");
    assert_eq!(b.steps[0].at[0].job, "b");
    assert_eq!(
        derive_setup_recipe(&fp, "nope").unwrap_err().to_string(),
        "the plan has no task `nope`"
    );
}

#[test]
fn the_toolchain_a_task_asks_for_is_in_its_recipe() {
    let (_t, _d, fp) = lab(
        "  a:\n    runs-on: x\n    steps:\n      - uses: actions/setup-node@v4\n        with:\n          node-version: '20'\n      - run: sh install.sh\n  b:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n",
    );
    let a = derive_setup_recipe(&fp, &id("a")).unwrap();
    assert_eq!(a.toolchain.len(), 1);
    assert_eq!(a.toolchain[0].tool, "node");
    assert_eq!(a.toolchain[0].version, "20");
    assert_eq!(a.toolchain[0].from, "actions/setup-node");
    assert!(derive_setup_recipe(&fp, &id("b"))
        .unwrap()
        .toolchain
        .is_empty());
}

#[test]
fn preparation_that_depends_on_the_run_is_left_in_its_job() {
    let (_t, _d, fp) = lab(concat!(
        "  runtime:\n    runs-on: x\n    strategy:\n      matrix:\n        d: [a, b]\n    steps:\n      - run: npm ci --prefix ${{ matrix.d }}\n",
        "  sideeffect:\n    runs-on: x\n    steps:\n      - run: echo \"/opt/x\" >> \"$GITHUB_PATH\"\n",
        "  venv:\n    runs-on: x\n    steps:\n      - run: python3 -m venv .venv\n",
        "  cond:\n    runs-on: x\n    steps:\n      - run: npm ci\n        if: github.event_name == 'pull_request'\n",
        "  svc:\n    runs-on: x\n    services:\n      db:\n        image: postgres\n        ports: ['5432:5432']\n    steps:\n      - run: npm ci --db localhost:5432\n",
    ));
    let mut all = Vec::new();
    for t in &fp.tasks {
        let r = derive_setup_recipe(&fp, &t.id).unwrap();
        assert!(r.steps.is_empty(), "{}: {:?}", t.id, r.steps);
        all.extend(r.left_in_place);
    }
    use local_ci::plan::StayReason::*;
    let reason = |job: &str| {
        all.iter()
            .find(|l| l.job == job)
            .unwrap_or_else(|| panic!("{job} is not listed: {all:?}"))
            .reason
    };
    assert_eq!(reason("runtime"), Expression);
    assert_eq!(reason("sideeffect"), EnvFileEffect);
    assert_eq!(reason("venv"), AbsolutePath);
    assert_eq!(reason("cond"), Conditional);
    assert_eq!(reason("svc"), ServiceDependent);
}

#[test]
fn a_job_that_stays_in_place_still_runs_the_step_itself() {
    // the install writes GITHUB_ENV: it cannot be captured, so it is the job's own
    let (tmp, dir, fp) = lab(
        "  a:\n    runs-on: x\n    steps:\n      - run: sh install.sh && echo \"X=1\" >> \"$GITHUB_ENV\"\n      - run: test \"$X\" = 1 && test -f installed.marker\n  b:\n    runs-on: x\n    steps:\n      - run: sh install.sh && echo \"X=1\" >> \"$GITHUB_ENV\"\n      - run: test \"$X\" = 1\n",
    );
    assert!(derive_setup_recipe(&fp, &id("a")).unwrap().steps.is_empty());
    let place = Place::new(&tmp, &dir, &fp);
    let ws = place.workspace("w1");
    let r = place.task(&ws, &id("a"), None);
    assert_eq!(outcomes(&r), vec![Outcome::Pass, Outcome::Pass]);
}

// ---- the workspace id stays out of the working tree ----

#[test]
fn the_workspace_id_is_not_a_file_in_the_working_tree_and_a_copy_does_not_inherit_it() {
    let (tmp, dir, fp) = lab(TWO_JOBS);
    let place = Place::new(&tmp, &dir, &fp);
    let recipe_a = derive_setup_recipe(&fp, &id("a")).unwrap();
    let ws = place.workspace("rep");
    let done = place.setup(&ws, &recipe_a);
    assert!(done.ok());
    let id = local_ci::setup::workspace_id(&ws.root).unwrap();
    assert_eq!(done.receipt.workspace, id);

    // nothing of the id is visible from the working tree (the install's own
    // output is, and is the only thing there)
    assert!(!ws.root.join(".local-ci").exists());
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&ws.root)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&status.stdout).into_owned();
    assert!(!status.contains("local-ci"), "{status}");
    assert!(!status.contains("workspace-id"), "{status}");

    // a plain copy of the tree (its .git still points at the same git dir) is
    // not the same workspace
    let copy = place.cache.join("wt").join("rep-copy");
    let cp = std::process::Command::new("cp")
        .arg("-R")
        .arg(&ws.root)
        .arg(&copy)
        .status()
        .unwrap();
    assert!(cp.success());
    assert_ne!(local_ci::setup::workspace_id(&copy), Some(id.clone()));
    assert_eq!(local_ci::setup::workspace_id(&ws.root), Some(id));
    // and asking for its id does not take the original's away
    local_ci::setup::workspace_id(&copy);
    assert_eq!(
        local_ci::setup::workspace_id(&ws.root).as_deref(),
        Some(done.receipt.workspace.as_str())
    );
}

// ---- A15 ----

#[test]
fn a15_a_receipt_is_believed_only_in_the_workspace_it_was_made_in() {
    let (tmp, dir, fp) = lab(TWO_JOBS);
    let place = Place::new(&tmp, &dir, &fp);
    let recipe_a = derive_setup_recipe(&fp, &id("a")).unwrap();

    // the representative does the setup in its own workspace
    let ws_a = place.workspace("rep");
    let done = place.setup(&ws_a, &recipe_a);
    assert!(done.ok());
    assert!(ws_a.root.join("installed.marker").is_file());
    assert_eq!(done.receipt.steps.len(), 1);
    assert_eq!(
        done.receipt.workspace,
        local_ci::setup::workspace_id(&ws_a.root).unwrap()
    );

    // in that workspace the receipt is believed
    let r = place.task(&ws_a, &id("a"), Some(&done.receipt));
    assert_eq!(r.receipt_ignored, None);
    assert_eq!(r.jobs[0].steps[0].outcome, skipped_setup());

    // job `b` runs somewhere else, in a checkout the install never reached.
    // The receipt of the other workspace is ignored: it does its own install,
    // so nothing is missing, and the result says why.
    let ws_b = place.workspace("consumer");
    assert!(!ws_b.root.join("installed.marker").exists());
    let r = place.task(&ws_b, &id("b"), Some(&done.receipt));
    assert_eq!(outcomes(&r), vec![Outcome::Pass, Outcome::Pass]);
    assert!(ws_b.root.join("installed.marker").is_file());
    assert!(
        r.receipt_ignored.as_deref().unwrap().contains("workspace"),
        "{:?}",
        r.receipt_ignored
    );

    // a workspace that has been prepared on its own has another id
    let ws_c = place.workspace("consumer2");
    let other = place.setup(&ws_c, &recipe_a);
    assert_ne!(other.receipt.workspace, done.receipt.workspace);
    std::fs::remove_file(ws_c.root.join("installed.marker")).unwrap();
    let r = place.task(&ws_c, &id("b"), Some(&done.receipt));
    assert_eq!(outcomes(&r), vec![Outcome::Pass, Outcome::Pass]);

    // without a receipt the task prepares everything itself
    let ws_d = place.workspace("consumer3");
    let r = place.task(&ws_d, &id("b"), None);
    assert_eq!(outcomes(&r), vec![Outcome::Pass, Outcome::Pass]);
    assert_eq!(r.receipt_ignored, None);
}

#[test]
fn a15_a_receipt_of_another_toolchain_or_host_is_ignored() {
    let (tmp, dir, fp) = lab(
        "  a:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n      - uses: actions/setup-node@v4\n        with:\n          node-version: '20'\n  b:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n",
    );
    let place = Place::new(&tmp, &dir, &fp);
    let recipe = derive_setup_recipe(&fp, &id("a")).unwrap();
    assert_eq!(recipe.toolchain.len(), 1);
    let ws = place.workspace("rep");
    let done = place.setup(&ws, &recipe);

    let check = |receipt: &PreparedReceipt, tctx: &TemplateContext, what: &str| {
        std::fs::remove_file(ws.root.join("installed.marker")).ok();
        let r = place.task_in(&ws, &id("a"), Some(receipt), tctx);
        assert!(r.receipt_ignored.is_some(), "{what}");
        assert_eq!(r.jobs[0].steps[0].outcome, Outcome::Pass, "{what}");
        assert!(ws.root.join("installed.marker").is_file(), "{what}");
    };
    // the same workspace and receipt, believed
    let r = place.task(&ws, &id("a"), Some(&done.receipt));
    assert_eq!(r.receipt_ignored, None);

    // made for a recipe with another toolchain
    let mut other_tool = recipe.clone();
    other_tool.toolchain[0].version = "22".into();
    let mut receipt = done.receipt.clone();
    receipt.template_key = other_tool.template_key(&place.tctx()).digest().into();
    check(&receipt, &place.tctx(), "toolchain");

    // the host the task runs on is another os, arch or profile
    for (what, f) in [
        (
            "os",
            Box::new(|c: &mut TemplateContext| c.os = "x".into())
                as Box<dyn Fn(&mut TemplateContext)>,
        ),
        ("arch", Box::new(|c| c.arch = "x".into())),
        ("profile", Box::new(|c| c.profile = "x".into())),
    ] {
        let mut c = place.tctx();
        f(&mut c);
        check(&done.receipt, &c, what);
    }
}

#[test]
fn a_receipt_skips_only_the_steps_it_names() {
    let (tmp, dir, fp) = lab(TWO_INSTALLS);
    let place = Place::new(&tmp, &dir, &fp);
    let recipe = derive_setup_recipe(&fp, &id("a")).unwrap();
    assert_eq!(recipe.steps.len(), 2);
    let ws = place.workspace("w");
    let done = place.setup(&ws, &recipe);
    // a receipt for the first install only
    let mut receipt = done.receipt.clone();
    receipt.steps.truncate(1);
    assert_eq!(receipt.steps[0].id, recipe.steps[0].id);
    receipt.artifacts.retain(|a| a.path == "installed.marker");
    std::fs::remove_file(ws.root.join("two.marker")).unwrap();
    let r = place.task(&ws, &id("a"), Some(&receipt));
    let o = outcomes(&r);
    assert_eq!(o[0], skipped_setup());
    // the second install is not named, so the task runs it
    assert_eq!(o[1], Outcome::Pass);
    assert!(ws.root.join("two.marker").is_file());

    // a receipt of another commit is not believed at all
    let mut other = receipt.clone();
    other.source = "0".repeat(40);
    let r = place.task(&ws, &id("a"), Some(&other));
    assert_eq!(outcomes(&r)[0], Outcome::Pass);
    assert!(r.receipt_ignored.is_some());
}

#[test]
fn a_step_that_comes_again_after_others_is_run_again() {
    // install A, install B, install A: the second A is not the same
    // preparation as the first (B came between), so the job runs it
    let job = |n: &str| {
        format!("  {n}:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n      - run: sh install2.sh\n      - run: sh install.sh\n      - run: test -f two.marker\n")
    };
    let (tmp, dir, fp) = lab(&format!("{}{}", job("a"), job("b")));
    let place = Place::new(&tmp, &dir, &fp);
    let recipe = derive_setup_recipe(&fp, &id("a")).unwrap();
    assert_eq!(recipe.steps.len(), 2);
    // the second `sh install.sh` is not a place of the first
    assert_eq!(
        recipe.steps[0]
            .at
            .iter()
            .map(|a| a.index)
            .collect::<Vec<_>>(),
        vec![0]
    );
    let ws = place.workspace("w");
    let done = place.setup(&ws, &recipe);
    let r = place.task(&ws, &id("a"), Some(&done.receipt));
    assert_eq!(
        outcomes(&r),
        vec![
            skipped_setup(),
            skipped_setup(),
            Outcome::Pass,
            Outcome::Pass
        ]
    );
}

#[test]
fn a_receipt_lists_only_the_artifacts_of_its_tasks_directories() {
    let job = |n: &str, d: &str| {
        format!("  {n}:\n    runs-on: x\n    defaults:\n      run:\n        working-directory: {d}\n    steps:\n      - run: sh ../install.sh\n")
    };
    let (tmp, dir, fp) = lab(&format!(
        "{}{}{}{}",
        job("a1", "d1"),
        job("a2", "d1"),
        job("b1", "d2"),
        job("b2", "d2")
    ));
    let place = Place::new(&tmp, &dir, &fp);
    let ra = derive_setup_recipe(&fp, &id("a1")).unwrap();
    let rb = derive_setup_recipe(&fp, &id("b1")).unwrap();
    assert_eq!((ra.steps.len(), rb.steps.len()), (1, 1));
    let all = SetupRecipe::union(&[&ra, &rb]);
    let ws = place.workspace("w");
    let done = place.setup(&ws, &all);
    assert!(done.ok());
    let paths = |r: &PreparedReceipt| {
        r.artifacts
            .iter()
            .map(|a| a.path.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        paths(&done.receipt),
        vec!["d1/installed.marker", "d2/installed.marker"]
    );
    let key = ra.template_key(&place.tctx());
    assert_eq!(
        paths(&done.receipt.restricted_to(&ra, &key)),
        vec!["d1/installed.marker"]
    );
    let key = rb.template_key(&place.tctx());
    assert_eq!(
        paths(&done.receipt.restricted_to(&rb, &key)),
        vec!["d2/installed.marker"]
    );
}

#[test]
fn equal_installs_are_done_once_and_both_are_receipted() {
    let (tmp, dir, fp) = lab(
        "  a:\n    runs-on: x\n    steps:\n      - run: npm ci\n  b:\n    runs-on: x\n    steps:\n      - run: npm ci --no-audit\n",
    );
    let ra = derive_setup_recipe(&fp, &id("a")).unwrap();
    let rb = derive_setup_recipe(&fp, &id("b")).unwrap();
    let all = SetupRecipe::union(&[&ra, &rb]);
    assert_eq!(all.steps.len(), 2);
    let covered = local_ci::setup::covered_by(&all);
    assert_eq!(covered.len(), 1);
    assert_eq!(covered[&rb.steps[0].id], ra.steps[0].id);
    let _ = (tmp, dir);
}

// ---- A16 ----

fn key_of(fp: &FrozenPlan, job: &str, ctx: &TemplateContext) -> String {
    derive_setup_recipe(fp, &id(job))
        .unwrap()
        .template_key(ctx)
        .digest()
        .to_string()
}

#[test]
fn a16_the_same_script_with_another_env_or_shell_is_another_key() {
    let (_t, _d, fp) = lab(concat!(
        "  same1:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n",
        "  same2:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n",
        "  env1:\n    runs-on: x\n    env:\n      NODE_ENV: production\n    steps:\n      - run: sh install.sh\n",
        "  env1b:\n    runs-on: x\n    env:\n      NODE_ENV: production\n    steps:\n      - run: sh install.sh\n",
        "  env2:\n    runs-on: x\n    env:\n      NODE_ENV: development\n    steps:\n      - run: sh install.sh\n",
        "  env2b:\n    runs-on: x\n    env:\n      NODE_ENV: development\n    steps:\n      - run: sh install.sh\n",
        "  sh1:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n        shell: sh\n",
        "  sh1b:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n        shell: sh\n",
    ));
    let c = TemplateContext::local(&fp, "run-1");
    let k = |j: &str| key_of(&fp, j, &c);
    assert_eq!(k("same1"), k("same2"));
    assert_ne!(k("same1"), k("env1"));
    assert_ne!(k("env1"), k("env2"));
    assert_ne!(k("same1"), k("sh1"));
    // the step differs too, not only the key: it would not be skipped for the other
    let step = |j: &str| {
        derive_setup_recipe(&fp, &id(j)).unwrap().steps[0]
            .id
            .clone()
    };
    assert_ne!(step("env1"), step("env2"));
    assert_ne!(step("same1"), step("sh1"));
    assert_eq!(step("same1"), step("same2"));
}

/// Run the whole plan in a workspace of its own with an `npm` that records the
/// `NODE_ENV` it was run with in `dep.marker` of the directory it ran in.
fn run_with_fake_npm(
    tmp: &common::Tmp,
    dir: &Path,
    fp: &FrozenPlan,
    parallel: usize,
) -> local_ci::report::RunResult {
    let bin = tmp.path().join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("npm"),
        "#!/bin/sh\nprintf '%s' \"$NODE_ENV\" > dep.marker\n",
    )
    .unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(bin.join("npm"))
        .status()
        .unwrap();
    let c = ctx_with(
        tmp.path(),
        &format!(
            "{}:/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin",
            bin.display()
        ),
    );
    let mut place = Place::new(tmp, dir, fp);
    place.home = tmp.path();
    let ws = place.workspace(&format!("whole{parallel}"));
    let mut opts = place.opts();
    opts.parallel = parallel;
    opts.keep = false;
    local_ci::execute_plan(&c, fp, &ws, opts).unwrap()
}

fn passed(r: &local_ci::report::RunResult) -> Vec<(String, Outcome)> {
    assert!(
        r.setup.iter().all(|s| s.outcome == Outcome::Pass),
        "{:?}",
        r.setup
    );
    r.units
        .iter()
        .flat_map(|u| u.jobs.iter().map(|j| (j.job.clone(), j.outcome.clone())))
        .collect()
}

fn stays_for_conflict(fp: &FrozenPlan, task: &str) {
    let r = derive_setup_recipe(fp, task).unwrap();
    assert!(r.steps.is_empty(), "{task}: {:?}", r.steps);
    assert!(
        r.left_in_place
            .iter()
            .any(|l| l.reason == local_ci::plan::StayReason::ConflictingInstall),
        "{task}: {:?}",
        r.left_in_place
    );
}

fn npm_job(name: &str, extra: &str, check: &str) -> String {
    format!("  {name}:\n    runs-on: x\n{extra}    steps:\n      - run: npm ci\n      - run: test \"$(cat dep.marker)\" = {check}\n")
}

#[test]
fn a16_installs_with_another_env_in_one_directory_stay_in_their_jobs() {
    // a and b install in the same directory with another NODE_ENV: one
    // workspace cannot hold both results, so neither is setup and each job
    // sees the result of its own install
    let (tmp, dir, fp) = lab(&format!(
        "{}{}",
        npm_job("a", "    env:\n      NODE_ENV: production\n", "production"),
        npm_job(
            "b",
            "    env:\n      NODE_ENV: development\n",
            "development"
        ),
    ));
    stays_for_conflict(&fp, &id("a"));
    stays_for_conflict(&fp, &id("b"));
    for parallel in [1, 2] {
        let r = run_with_fake_npm(&tmp, &dir, &fp, parallel);
        let p = passed(&r);
        assert_eq!(p.len(), 2);
        assert!(
            p.iter().all(|(_, o)| *o == Outcome::Pass),
            "parallel {parallel}: {p:?}"
        );
    }
}

#[test]
fn a16_installs_with_another_shell_stay_in_their_jobs() {
    let (_t, _d, fp) = lab(&format!(
        "{}{}",
        npm_job("a", "", "x"),
        npm_job("b", "    defaults:\n      run:\n        shell: sh\n", "x"),
    ));
    stays_for_conflict(&fp, &id("a"));
    stays_for_conflict(&fp, &id("b"));
}

#[test]
fn a16_installs_with_another_workflow_env_stay_in_their_jobs() {
    let wf = |env: &str| {
        format!(
            "name: ci\non: pull_request\nenv:\n  NODE_ENV: {env}\njobs:\n{}",
            npm_job("j", "", env)
        )
    };
    let (_t, _d, fp) = lab_files(&[
        (".github/workflows/one.yml", wf("production")),
        (".github/workflows/two.yml", wf("development")),
    ]);
    for f in ["one", "two"] {
        stays_for_conflict(&fp, &format!(".github/workflows/{f}.yml::j"));
    }
}

#[test]
fn a16_installs_in_other_directories_are_still_setup() {
    let (tmp, dir, fp) = lab(&format!(
        "{}{}",
        npm_job("a", "    defaults:\n      run:\n        working-directory: d1\n    env:\n      NODE_ENV: production\n", "production"),
        npm_job("b", "    defaults:\n      run:\n        working-directory: d2\n    env:\n      NODE_ENV: development\n", "development"),
    ));
    for j in ["a", "b"] {
        let r = derive_setup_recipe(&fp, &id(j)).unwrap();
        assert_eq!(r.steps.len(), 1, "{j}");
        assert!(r.left_in_place.is_empty());
    }
    let r = run_with_fake_npm(&tmp, &dir, &fp, 2);
    assert!(passed(&r).iter().all(|(_, o)| *o == Outcome::Pass));
}

#[test]
fn a_shell_local_ci_cannot_start_leaves_its_setup_in_the_job_and_the_run_goes_on() {
    let (tmp, dir, fp) = lab(&format!(
        "{}{}",
        npm_job("a", "    defaults:\n      run:\n        shell: pwsh\n", "x"),
        "  b:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n      - run: test -f installed.marker\n  c:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n      - run: test -f installed.marker\n",
    ));
    let a = derive_setup_recipe(&fp, &id("a")).unwrap();
    assert!(a.steps.is_empty());
    assert_eq!(
        a.left_in_place.iter().map(|l| l.reason).collect::<Vec<_>>(),
        vec![local_ci::plan::StayReason::UnsupportedShell]
    );
    let r = run_with_fake_npm(&tmp, &dir, &fp, 2);
    let p = passed(&r);
    assert_eq!(p.len(), 3);
    let o = |j: &str| p.iter().find(|(x, _)| x == j).unwrap().1.clone();
    assert!(matches!(o("a"), Outcome::Fail | Outcome::Error(_)), "{p:?}");
    assert_eq!(o("b"), Outcome::Pass);
    assert_eq!(o("c"), Outcome::Pass);
}

#[test]
fn a_template_key_is_made_of_everything_the_preparation_depends_on() {
    let (_t, _d, fp) = lab(TWO_JOBS);
    let r = derive_setup_recipe(&fp, &id("a")).unwrap();
    let base = TemplateContext::local(&fp, "run-1");
    let key = |c: &TemplateContext| r.template_key(c).digest().to_string();
    // deterministic
    assert_eq!(key(&base), key(&base.clone()));
    assert!(key(&base).starts_with("sha256:"));
    let mut cases: Vec<(&str, TemplateContext)> = Vec::new();
    for (name, f) in [
        (
            "source",
            Box::new(|c: &mut TemplateContext| c.source_digest = "x".into())
                as Box<dyn Fn(&mut TemplateContext)>,
        ),
        ("os", Box::new(|c| c.os = "other".into())),
        ("arch", Box::new(|c| c.arch = "other".into())),
        ("profile", Box::new(|c| c.profile = "other".into())),
        ("cache schema", Box::new(|c| c.cache_schema += 1)),
        (
            "run id",
            Box::new(|c| {
                c.scope = ReuseScope::Run {
                    run_id: "run-2".into(),
                }
            }),
        ),
        ("scope", Box::new(|c| c.scope = ReuseScope::Persistent)),
        (
            "freshness",
            Box::new(|c| c.freshness = Freshness::MaxAgeSecs { secs: 60 }),
        ),
        (
            "secret",
            Box::new(|c| {
                c.secrets.push(SecretDigest {
                    name: "TOKEN".into(),
                    digest: "hmac:1".into(),
                })
            }),
        ),
    ] {
        let mut c = base.clone();
        f(&mut c);
        cases.push((name, c));
    }
    for (name, c) in &cases {
        assert_ne!(key(&base), key(c), "{name} is not in the key");
    }
    // another version of a secret is another key; its value is never stored
    let mut s1 = base.clone();
    s1.secrets.push(SecretDigest {
        name: "TOKEN".into(),
        digest: "hmac:1".into(),
    });
    let mut s2 = base.clone();
    s2.secrets.push(SecretDigest {
        name: "TOKEN".into(),
        digest: "hmac:2".into(),
    });
    assert_ne!(key(&s1), key(&s2));
    // a persistent template has no run id: runs share it
    let mut p1 = base.clone();
    p1.scope = ReuseScope::Persistent;
    assert!(!r.template_key(&p1).inputs().to_string().contains("run-1"));
    // the order of the steps is part of it
    let (_t2, _d2, fp2) = lab(TWO_INSTALLS);
    let two = derive_setup_recipe(&fp2, &id("a")).unwrap();
    let mut swapped = two.clone();
    swapped.steps.reverse();
    assert_ne!(
        two.template_key(&base).digest(),
        swapped.template_key(&base).digest()
    );
}

// ---- A17 ----

fn tree_digest(root: &Path) -> Vec<String> {
    fn walk(p: &Path, root: &Path, out: &mut Vec<String>) {
        let md = std::fs::symlink_metadata(p).unwrap();
        let rel = p.strip_prefix(root).unwrap().display().to_string();
        if md.is_dir() {
            out.push(format!("d {rel}"));
            let mut es: Vec<_> = std::fs::read_dir(p)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            es.sort();
            for e in es {
                walk(&e, root, out);
            }
        } else if md.file_type().is_symlink() {
            out.push(format!("l {rel}"));
        } else {
            out.push(format!("f {rel} {}", std::fs::read_to_string(p).unwrap()));
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

fn recipe_step(run: &str) -> RecipeStep {
    RecipeStep {
        id: format!("id-{run}"),
        dir: String::new(),
        run: run.into(),
        shell: None,
        env: vec![],
        unresolved_env: vec![],
        ignored_env: vec![],
        ignored_env_differs: vec![],
        label: run.into(),
        workflow: F.into(),
        job: "a".into(),
        at: vec![],
    }
}

/// The receipt of a setup that ran every step of `recipe` and made `artifact`.
fn receipt_for(
    recipe: &SetupRecipe,
    key: &TemplateKey,
    built: &Path,
    artifact: &str,
) -> PreparedReceipt {
    PreparedReceipt {
        template_key: key.digest().into(),
        source: "src".into(),
        workspace: "builder".into(),
        steps: recipe
            .steps
            .iter()
            .map(|s| PreparedStep {
                id: s.id.clone(),
                how: PreparedHow::Ran,
            })
            .collect(),
        artifacts: vec![ArtifactDigest {
            path: artifact.into(),
            digest: local_ci::tree::tree_digest(&built.join(artifact))
                .unwrap()
                .digest,
        }],
        notes: vec![],
    }
}

#[test]
fn a17_building_a_plus_b_does_not_change_a_and_consumers_are_apart() {
    let tmp = common::tempdir().unwrap();
    let c1 = ctx(tmp.path());
    let c2 = c1.fresh_attempt();
    let store = TemplateStore::new(tmp.path().join("templates"));
    let tctx = TemplateContext {
        source_digest: "src".into(),
        os: "os".into(),
        arch: "arch".into(),
        profile: "p".into(),
        cache_schema: 1,
        scope: ReuseScope::Persistent,
        freshness: Freshness::Any,
        secrets: vec![],
    };
    let mut ra = SetupRecipe::default();
    ra.steps.push(recipe_step("install a"));
    let mut rab = ra.clone();
    rab.steps.push(recipe_step("install b"));
    let (ka, kab) = (ra.template_key(&tctx), rab.template_key(&tctx));
    assert_ne!(ka.digest(), kab.digest());

    let built_a = tmp.path().join("built-a");
    std::fs::create_dir_all(built_a.join("node_modules/a")).unwrap();
    std::fs::write(built_a.join("node_modules/a/index.js"), "a").unwrap();
    let rec_a = receipt_for(&ra, &ka, &built_a, "node_modules");
    let a = store.publish(&c1, &ka, &rec_a, &built_a).unwrap();
    let before = tree_digest(&a.tree());

    // A is in use by one consumer when A+B is built
    let use1 = tmp.path().join("use1");
    let rec_use1 = a.materialize(&c1, &use1).unwrap();
    assert_eq!(
        rec_use1.workspace,
        local_ci::setup::workspace_id(&use1).unwrap()
    );
    assert_ne!(rec_use1.workspace, rec_a.workspace);
    assert_eq!(rec_use1.steps, rec_a.steps);

    // the template cannot be written, and a second publish of the same key
    // does not replace it
    assert!(std::fs::write(a.tree().join("node_modules/a/index.js"), "x").is_err());
    assert!(std::fs::write(a.tree().join("new"), "x").is_err());
    std::fs::write(built_a.join("node_modules/a/index.js"), "changed").unwrap();
    let again = store.publish(&c1, &ka, &rec_a, &built_a).unwrap();
    assert_eq!(tree_digest(&again.tree()), before);
    assert!(store.get(&ka).is_some());

    // a consumer's copy is its own: writable, not linked to the template
    let tpl_file = a.tree().join("node_modules/a/index.js");
    let own = use1.join("node_modules/a/index.js");
    assert_ne!(
        std::fs::metadata(&tpl_file).unwrap().ino(),
        std::fs::metadata(&own).unwrap().ino()
    );
    std::fs::write(&own, "edited").unwrap();
    assert_eq!(std::fs::read_to_string(&tpl_file).unwrap(), "a");

    // c2's consumer exists before anything is cancelled
    let use2 = tmp.path().join("use2");
    let rec_use2 = a.materialize(&c2, &use2).unwrap();
    assert_ne!(rec_use1.workspace, rec_use2.workspace);
    let use2_before = tree_digest(&use2);

    // a process of c1 is writing in use1 (a copy of A) ...
    let (_lab_tmp, _lab_dir, fp) = lab(
        "  w:\n    runs-on: x\n    steps:\n      - run: while :; do echo x >> writer.log; sleep 0.05; done\n",
    );
    let ws = Workspace {
        root: use1.clone(),
        repo_root: use1.clone(),
        run_dir: tmp.path().join("runs/use1"),
        tools: tmp.path().join("use1.tools"),
        lock_file: tmp.path().join("use1.lock"),
    };
    std::fs::create_dir_all(&ws.run_dir).unwrap();
    let opts = Opts {
        parallel: 1,
        canirun: None,
        portool: None,
        containers: 1,
        job_timeout_min: 5.0,
        keep: true,
        trash: tmp.path().join("trash"),
    };
    let log = use1.join("writer.log");
    let result = std::thread::scope(|scope| {
        let h = scope.spawn(|| {
            local_ci::execute_task(
                &c1,
                &fp,
                &id("w"),
                &TaskInputs::default(),
                Some((&rec_use1, &tctx)),
                &ws,
                opts,
            )
        });
        let t0 = std::time::Instant::now();
        while !log.is_file() && t0.elapsed().as_secs() < 30 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(log.is_file(), "the writer never started");
        // ... c1 is cancelled while A+B is built and published
        c1.cancel.cancel();
        let built_ab = tmp.path().join("built-ab");
        std::fs::create_dir_all(built_ab.join("node_modules/a")).unwrap();
        std::fs::create_dir_all(built_ab.join("node_modules/b")).unwrap();
        std::fs::write(built_ab.join("node_modules/a/index.js"), "a").unwrap();
        std::fs::write(built_ab.join("node_modules/b/index.js"), "b").unwrap();
        let rec_ab = receipt_for(&rab, &kab, &built_ab, "node_modules");
        let ab = store.publish(&c2, &kab, &rec_ab, &built_ab).unwrap();
        (h.join().unwrap(), ab)
    });
    let (task, ab) = result;
    let task = task.unwrap();
    assert!(
        !matches!(task.jobs[0].outcome, Outcome::Pass),
        "{:?}",
        task.jobs[0].outcome
    );
    // the writer is gone: the file does not grow any more
    let size = std::fs::metadata(&log).unwrap().len();
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert_eq!(std::fs::metadata(&log).unwrap().len(), size);

    // A did not change, A+B is another tree, and c2's copy is intact
    assert_eq!(tree_digest(&a.tree()), before);
    assert_ne!(tree_digest(&ab.tree()), before);
    assert_eq!(tree_digest(&use2), use2_before);
    assert!(c1.cancel.is_cancelled() && !c2.cancel.is_cancelled());
    assert_eq!(
        std::fs::read_to_string(use2.join("node_modules/a/index.js")).unwrap(),
        "a"
    );
    assert!(
        a.materialize(&c2, &use2).is_err(),
        "an existing path is not overwritten"
    );
}

// ---- every command comes from the context ----

#[test]
fn library_commands_use_the_context_env_and_not_the_processs() {
    let tmp = common::tempdir().unwrap();
    // the test process finds git and cp; a context with an empty PATH does not
    let bare = ctx_with(tmp.path(), "/nonexistent");
    assert!(local_ci::git::git(&bare, tmp.path(), &["--version"]).is_err());
    let good = ctx(tmp.path());
    assert!(local_ci::git::git(&good, tmp.path(), &["--version"]).is_ok());
    // a shim on the context's PATH is what runs
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("git"), "#!/bin/sh\necho shim\n").unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(bin.join("git"))
        .status()
        .unwrap();
    let shimmed = ctx_with(tmp.path(), &bin.to_string_lossy());
    assert_eq!(
        local_ci::git::git(&shimmed, tmp.path(), &["status"]).unwrap(),
        "shim"
    );
    // the template store copies with the context's cp
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let store = TemplateStore::new(tmp.path().join("t"));
    let key = SetupRecipe::default().template_key(&TemplateContext {
        source_digest: "s".into(),
        os: "o".into(),
        arch: "a".into(),
        profile: "p".into(),
        cache_schema: 1,
        scope: ReuseScope::Persistent,
        freshness: Freshness::Any,
        secrets: vec![],
    });
    let receipt = PreparedReceipt::empty(key.digest(), "s");
    assert!(store.publish(&bare, &key, &receipt, &src).is_err());
    assert!(store.publish(&good, &key, &receipt, &src).is_ok());
}

#[test]
fn plan_output_says_what_stays_in_place_and_why() {
    let (tmp, dir, _fp) = lab(concat!(
        "  venv:\n    runs-on: x\n    steps:\n      - run: python3 -m venv .venv\n",
        "  other:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n",
        "  other2:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n",
    ));
    let o = Command::new(env!("CARGO_BIN_EXE_local-ci"))
        .args(["plan", "--base", "main"])
        .current_dir(&dir)
        .env("LOCAL_CI_HOME", tmp.path().join("home"))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(
        o.status.success(),
        "{out}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(out.contains("setup (once): .$ sh install.sh"), "{out}");
    assert!(
        out.contains("setup stays in venv: python3 -m venv .venv (builds an environment tied to an absolute path)"),
        "{out}"
    );
}

// ---- derived workspaces are bound explicitly ----

#[test]
fn a_materialized_template_skips_the_steps_it_prepared_and_a_plain_copy_does_not() {
    let (tmp, dir, fp) = lab(TWO_JOBS);
    let place = Place::new(&tmp, &dir, &fp);
    let recipe_a = derive_setup_recipe(&fp, &id("a")).unwrap();
    let key = recipe_a.template_key(&place.tctx());
    let built = place.workspace("builder");
    let done = place.setup(&built, &recipe_a);
    assert!(done.ok());
    let c = ctx(tmp.path());
    let store = TemplateStore::new(tmp.path().join("templates"));
    let tpl = store.publish(&c, &key, &done.receipt, &built.root).unwrap();
    let tpl = store.get(&key).unwrap_or(tpl);

    let consumer = |name: &str| Workspace {
        root: place.cache.join("wt").join(name),
        repo_root: dir.clone(),
        run_dir: place.cache.join("runs").join(name),
        tools: place.cache.join("wt").join(format!("{name}.tools")),
        lock_file: place.cache.join("locks/lab.lock"),
    };

    // through the API: the receipt belongs to the new tree
    let ws = consumer("materialized");
    std::fs::create_dir_all(&ws.run_dir).unwrap();
    let receipt = tpl.materialize(&c, &ws.root).unwrap();
    assert_ne!(receipt.workspace, done.receipt.workspace);
    let r = place.task(&ws, &id("a"), Some(&receipt));
    assert_eq!(r.receipt_ignored, None);
    assert_eq!(outcomes(&r), vec![skipped_setup(), Outcome::Pass]);

    // the template's own receipt is the builder's: not believed in the copy
    let r = place.task(&ws, &id("a"), Some(tpl.receipt()));
    assert!(r.receipt_ignored.as_deref().unwrap().contains("workspace"));

    // a copy made by any other means is another workspace
    let plain = consumer("plain");
    std::fs::create_dir_all(&plain.run_dir).unwrap();
    assert!(Command::new("cp")
        .arg("-R")
        .arg(&ws.root)
        .arg(&plain.root)
        .status()
        .unwrap()
        .success());
    let r = place.task(&plain, &id("a"), Some(&receipt));
    assert_eq!(outcomes(&r), vec![Outcome::Pass, Outcome::Pass]);
    assert!(r.receipt_ignored.as_deref().unwrap().contains("workspace"));
    // the original keeps its own id
    assert_eq!(
        local_ci::setup::workspace_id(&ws.root).as_deref(),
        Some(receipt.workspace.as_str())
    );
}

#[test]
fn units_run_in_copies_that_are_bound_to_the_setup_once_for_any_installer() {
    // two workflows are two units, each run in a copy of the workspace. `yarn`
    // is not covered by the npm shim's marks, so only the receipt keeps the
    // copies from installing again.
    let wf = |name: &str| {
        (
            format!(".github/workflows/{name}.yml"),
            format!(
                "name: {name}\non: pull_request\njobs:\n  j:\n    runs-on: x\n    steps:\n      - run: yarn install\n      - run: test -d node_modules/pkg\n"
            ),
        )
    };
    let (one, two) = (wf("one"), wf("two"));
    let (tmp, dir, fp) = lab_files(&[(&one.0, one.1), (&two.0, two.1)]);
    assert_eq!(fp.units.len(), 2);
    let bin = tmp.path().join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let count = tmp.path().join("yarn.count");
    std::fs::write(
        bin.join("yarn"),
        format!(
            "#!/bin/sh\necho run >> {}\nmkdir -p node_modules/pkg\n",
            count.display()
        ),
    )
    .unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(bin.join("yarn"))
        .status()
        .unwrap();
    let c = ctx_with(
        tmp.path(),
        &format!(
            "{}:/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin",
            bin.display()
        ),
    );
    let place = Place::new(&tmp, &dir, &fp);
    let ws = place.workspace("whole");
    let mut opts = place.opts();
    opts.parallel = 2;
    opts.keep = false;
    let r = local_ci::execute_plan(&c, &fp, &ws, opts).unwrap();
    let p = passed(&r);
    assert_eq!(p.len(), 2);
    assert!(p.iter().all(|(_, o)| *o == Outcome::Pass), "{p:?}");
    let runs = std::fs::read_to_string(&count).unwrap();
    assert_eq!(runs.lines().count(), 1, "yarn ran {runs:?}");
}

// ---- the receipt is checked against what the workspace holds ----

#[test]
fn a_receipt_is_ignored_when_what_the_setup_made_is_gone() {
    let (tmp, dir, fp) = lab(TWO_JOBS);
    let place = Place::new(&tmp, &dir, &fp);
    let recipe_a = derive_setup_recipe(&fp, &id("a")).unwrap();
    let ws = place.workspace("rep");
    let done = place.setup(&ws, &recipe_a);
    assert!(done.ok());
    assert!(done
        .receipt
        .artifacts
        .iter()
        .any(|a| a.path == "installed.marker"));
    // `git clean -fdx` leaves the id in the git dir
    std::fs::remove_file(ws.root.join("installed.marker")).unwrap();
    assert_eq!(
        local_ci::setup::workspace_id(&ws.root).as_deref(),
        Some(done.receipt.workspace.as_str())
    );
    let r = place.task(&ws, &id("a"), Some(&done.receipt));
    assert_eq!(outcomes(&r), vec![Outcome::Pass, Outcome::Pass]);
    let why = r.receipt_ignored.unwrap();
    assert!(why.contains("installed.marker"), "{why}");
    assert!(ws.root.join("installed.marker").is_file());
}

// ---- shell and env in the keys ----

#[test]
fn a_shell_written_as_bash_is_the_default_shell() {
    let (_t, _d, fp) = lab(concat!(
        "  a:\n    runs-on: x\n    steps:\n      - run: npm ci\n        shell: bash\n",
        "  b:\n    runs-on: x\n    steps:\n      - run: npm ci\n",
    ));
    let (a, b) = (
        derive_setup_recipe(&fp, &id("a")).unwrap(),
        derive_setup_recipe(&fp, &id("b")).unwrap(),
    );
    assert_eq!(a.steps.len(), 1);
    assert!(a.left_in_place.is_empty() && b.left_in_place.is_empty());
    assert_eq!(a.steps[0].id, b.steps[0].id);
    let c = TemplateContext::local(&fp, "r");
    assert_eq!(a.template_key(&c).digest(), b.template_key(&c).digest());
}

const DB_JOBS: &str = concat!(
    "  a:\n    runs-on: x\n    env:\n      DATABASE_URL: db-a\n      NODE_ENV: test\n    steps:\n      - run: npm ci\n",
    "  b:\n    runs-on: x\n    env:\n      DATABASE_URL: db-b\n      NODE_ENV: test\n    steps:\n      - run: npm ci\n",
);

#[test]
fn an_install_with_another_job_env_is_not_hoisted_unless_the_name_is_ignored() {
    let (_t, _d, fp) = lab(DB_JOBS);
    stays_for_conflict(&fp, &id("a"));
    stays_for_conflict(&fp, &id("b"));

    let toml = (
        ".local-ci.toml",
        "[setup]\nignore-env = [\"DATABASE_URL\"]\n".to_string(),
    );
    let (_t, _d, fp) = lab_files(&[
        (F, format!("name: ci\non: pull_request\njobs:\n{DB_JOBS}")),
        toml,
    ]);
    let (a, b) = (
        derive_setup_recipe(&fp, &id("a")).unwrap(),
        derive_setup_recipe(&fp, &id("b")).unwrap(),
    );
    assert_eq!((a.steps.len(), b.steps.len()), (1, 1));
    assert_eq!(a.steps[0].id, b.steps[0].id);
    let c = TemplateContext::local(&fp, "r");
    assert_eq!(a.template_key(&c).digest(), b.template_key(&c).digest());
    // the name is not in the key, but the install still runs with the value
    assert_eq!(a.steps[0].env, vec![("NODE_ENV".into(), "test".into())]);
    assert_eq!(
        a.steps[0].ignored_env,
        vec![("DATABASE_URL".to_string(), "db-a".to_string())]
    );
}

#[test]
fn a_name_that_is_not_ignored_still_separates_installs() {
    let (_t, _d, fp) = lab_files(&[
        (
            F,
            format!(
                "name: ci\non: pull_request\njobs:\n{}",
                DB_JOBS.replace(
                    "NODE_ENV: test\n    steps:\n      - run: npm ci\n  b",
                    "NODE_ENV: production\n    steps:\n      - run: npm ci\n  b"
                )
            ),
        ),
        (
            ".local-ci.toml",
            "[setup]\nignore-env = [\"DATABASE_URL\"]\n".to_string(),
        ),
    ]);
    stays_for_conflict(&fp, &id("a"));
}
