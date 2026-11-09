//! Setup recipes: what a task prepares, the key a prepared tree is reused
//! under, the receipt of what was prepared, and immutable templates.

#![allow(clippy::disallowed_methods)] // the test builds the binary's environment

use local_ci::exec::{Opts, Outcome, Workspace};
use local_ci::setup::{
    derive_setup_recipe, Freshness, PreparedHow, PreparedReceipt, PreparedStep, RecipeStep,
    ReuseScope, SecretDigest, SetupRecipe, TemplateContext,
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
fn lab(jobs: &str) -> (tempfile::TempDir, PathBuf, FrozenPlan) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(
        dir.join(F),
        format!("name: ci\non: pull_request\njobs:\n{jobs}"),
    )
    .unwrap();
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
    fn new(tmp: &'a tempfile::TempDir, dir: &'a Path, fp: &'a FrozenPlan) -> Self {
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
        local_ci::git::add_worktree(&ctx(self.home), self.dir, &wt, &self.fp.source.sha).unwrap();
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

    fn setup(&self, ws: &Workspace, recipe: &SetupRecipe) -> local_ci::setup::PreparedSetup {
        let key = recipe.template_key(&TemplateContext::local(self.fp, "run-1"));
        local_ci::execute_setup(&ctx(self.home), self.fp, recipe, &key, ws, self.opts()).unwrap()
    }

    fn task(&self, ws: &Workspace, task: &str, prepared: Option<&PreparedReceipt>) -> TaskResult {
        local_ci::execute_task(
            &ctx(self.home),
            self.fp,
            task,
            &TaskInputs::default(),
            prepared,
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

// ---- A15 ----

#[test]
fn a15_a_consumer_in_another_workspace_still_gets_its_install() {
    let (tmp, dir, fp) = lab(TWO_JOBS);
    let place = Place::new(&tmp, &dir, &fp);
    let recipe_a = derive_setup_recipe(&fp, &id("a")).unwrap();

    // the representative does the setup in its own workspace
    let ws_a = place.workspace("rep");
    let done = place.setup(&ws_a, &recipe_a);
    assert!(done.ok());
    assert!(ws_a.root.join("installed.marker").is_file());
    assert_eq!(done.receipt.steps.len(), 1);

    // job `b` runs somewhere else, in a checkout the install never reached.
    // Without a receipt it does its own install, so nothing is missing.
    let ws_b = place.workspace("consumer");
    assert!(!ws_b.root.join("installed.marker").exists());
    let r = place.task(&ws_b, &id("b"), None);
    assert_eq!(outcomes(&r), vec![Outcome::Pass, Outcome::Pass]);
    assert!(ws_b.root.join("installed.marker").is_file());

    // and with a receipt from a workspace that is not its own, a step the
    // receipt names is skipped (the caller vouches for the receipt, so it must
    // pass the receipt of the workspace the task runs in)
    let ws_c = place.workspace("consumer2");
    let r = place.task(&ws_c, &id("b"), Some(&done.receipt));
    assert_eq!(r.jobs[0].steps[0].outcome, skipped_setup());
    assert!(!ws_c.root.join("installed.marker").exists());
}

#[test]
fn a_receipt_skips_only_the_steps_it_names() {
    let (tmp, dir, fp) = lab(TWO_INSTALLS);
    let place = Place::new(&tmp, &dir, &fp);
    let recipe = derive_setup_recipe(&fp, &id("a")).unwrap();
    assert_eq!(recipe.steps.len(), 2);
    let key = recipe.template_key(&TemplateContext::local(&fp, "r"));
    // a receipt for the first install only
    let receipt = PreparedReceipt {
        template_key: key.digest().into(),
        source: fp.source.sha.clone(),
        steps: vec![PreparedStep {
            id: recipe.steps[0].id.clone(),
            how: PreparedHow::Ran,
        }],
        artifacts: vec![],
    };
    let ws = place.workspace("w");
    let r = place.task(&ws, &id("a"), Some(&receipt));
    let o = outcomes(&r);
    assert_eq!(o[0], skipped_setup());
    // the second install is not named, so the task runs it
    assert_eq!(o[1], Outcome::Pass);
    assert!(ws.root.join("two.marker").is_file());
    assert!(!ws.root.join("installed.marker").exists());

    // a receipt of another commit is not believed at all
    let mut other = receipt.clone();
    other.source = "0".repeat(40);
    let ws = place.workspace("w2");
    let r = place.task(&ws, &id("a"), Some(&other));
    assert_eq!(outcomes(&r)[0], Outcome::Pass);
    assert!(ws.root.join("installed.marker").is_file());
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
        "  env2:\n    runs-on: x\n    env:\n      NODE_ENV: development\n    steps:\n      - run: sh install.sh\n",
        "  sh1:\n    runs-on: x\n    steps:\n      - run: sh install.sh\n        shell: sh\n",
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
        label: run.into(),
        workflow: F.into(),
        job: "a".into(),
        at: vec![],
    }
}

#[test]
fn a17_building_a_plus_b_does_not_change_a_and_consumers_are_apart() {
    let tmp = tempfile::tempdir().unwrap();
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
    let rec_a = PreparedReceipt::empty(ka.digest(), "src");
    let a = store.publish(&c1, &ka, &rec_a, &built_a).unwrap();
    let before = tree_digest(&a.tree());

    // A is in use by one consumer when A+B is built
    let use1 = tmp.path().join("use1");
    a.materialize(&c1, &use1).unwrap();
    let built_ab = tmp.path().join("built-ab");
    std::fs::create_dir_all(built_ab.join("node_modules/a")).unwrap();
    std::fs::create_dir_all(built_ab.join("node_modules/b")).unwrap();
    std::fs::write(built_ab.join("node_modules/a/index.js"), "a").unwrap();
    std::fs::write(built_ab.join("node_modules/b/index.js"), "b").unwrap();
    let rec_ab = PreparedReceipt::empty(kab.digest(), "src");
    let ab = store.publish(&c1, &kab, &rec_ab, &built_ab).unwrap();
    assert_eq!(tree_digest(&a.tree()), before);
    assert_ne!(tree_digest(&ab.tree()), before);

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

    // one consumer is cancelled; the other, and the template, are not affected
    c1.cancel.cancel();
    assert!(c1.cancel.is_cancelled() && !c2.cancel.is_cancelled());
    let use2 = tmp.path().join("use2");
    a.materialize(&c2, &use2).unwrap();
    assert_eq!(
        std::fs::read_to_string(use2.join("node_modules/a/index.js")).unwrap(),
        "a"
    );
    assert_eq!(tree_digest(&a.tree()), before);
    assert!(
        a.materialize(&c2, &use2).is_err(),
        "an existing path is not overwritten"
    );
}

// ---- every command comes from the context ----

#[test]
fn library_commands_use_the_context_env_and_not_the_processs() {
    let tmp = tempfile::tempdir().unwrap();
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
