//! Fixed inputs are planned without a Git checkout or an execution context.
#![allow(clippy::disallowed_methods)]
mod common;
use local_ci::frozen::{DisabledWorkflow, Filters};
use local_ci::source::{self, CaptureOptions, ObjectStore, RelPath, SourceKind, SourceManifest};
use local_ci::{
    build_frozen_plan_from_source as build, ExecContext, FrozenPlan, PinnedPlanError as E,
    PinnedPlanOptions,
};
use std::path::{Path, PathBuf};

const WORKFLOW: &str = "name: fixed\non: push\njobs:\n  check:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo captured\n";
fn options() -> PinnedPlanOptions {
    PinnedPlanOptions {
        workflow_dirs_override: None,
        filters: Filters {
            all: false,
            include_disabled: true,
            workflows_dirs: vec![],
            workflows: vec![],
            jobs: vec![],
        },
        disabled: vec![],
        base_branch: None,
        changed_files: None,
    }
}
struct Fixture {
    _tmp: common::Tmp,
    root: PathBuf,
    store: ObjectStore,
    ctx: ExecContext,
}
impl Fixture {
    fn new(workflow: &str) -> Self {
        let tmp = common::tempdir().unwrap();
        let root = tmp.path().join("input");
        std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
        std::fs::write(root.join(".github/workflows/ci.yml"), workflow).unwrap();
        let store = ObjectStore::open(tmp.path().join("objects")).unwrap();
        let ctx = ExecContext::new(
            [
                ("PATH".into(), String::new()),
                ("HOME".into(), tmp.path().display().to_string()),
            ]
            .into(),
        );
        Self {
            _tmp: tmp,
            root,
            store,
            ctx,
        }
    }
    fn capture(&self) -> SourceManifest {
        source::capture_directory(
            &self.ctx,
            &self.root,
            &CaptureOptions {
                store: Some(self.store.root().into()),
                ..Default::default()
            },
        )
        .unwrap()
        .manifest
    }
    fn error(&self, m: &SourceManifest, o: &PinnedPlanOptions, want: E) {
        assert_eq!(build(&self.store, m, o).unwrap_err(), want);
    }
}
#[test]
fn directory_is_fixed_secret_free_and_bound_in_the_plan() {
    let f = Fixture::new(WORKFLOW);
    std::fs::write(f.root.join(".env"), "TOKEN=private-value").unwrap();
    let snap = source::capture_directory(
        &f.ctx,
        &f.root,
        &CaptureOptions {
            store: Some(f.store.root().into()),
            secret_files: vec![source::SecretSpec {
                glob: ".env".into(),
                required: true,
            }],
            secret_key: vec![7; 32],
            ..Default::default()
        },
    )
    .unwrap();
    let m = snap.manifest;
    let first = build(&f.store, &m, &options()).unwrap();
    assert_eq!(first.schema_version, 7);
    assert_eq!(first.source.source_id, m.source_id);
    assert_eq!(first.source.execution_commit, None);
    assert_eq!(first.source.base, None);
    let pinned = first.source.pinned.as_ref().unwrap();
    assert_eq!(pinned.kind, SourceKind::Directory);
    assert_eq!(pinned.manifest_digest, m.manifest_digest);
    assert_eq!(pinned.cwd_relative, m.cwd_relative);
    assert!(!pinned.changed_files_known);
    assert_eq!(first.workflows.len(), 1);
    first.verify().unwrap();
    let json = serde_json::to_string(&first).unwrap();
    assert!(!json.contains("private-value"));
    assert_eq!(serde_json::from_str::<FrozenPlan>(&json).unwrap(), first);
    std::fs::write(f.root.join(".github/workflows/ci.yml"), "invalid now").unwrap();
    std::fs::remove_file(snap.secrets.unwrap()).unwrap();
    assert_eq!(build(&f.store, &m, &options()).unwrap(), first);
    assert_eq!(
        first.require_git_source().unwrap_err().downcast_ref::<E>(),
        Some(&E::UnsupportedExecution)
    );
}
#[test]
fn unknown_event_metadata_is_an_error_and_known_empty_is_distinct() {
    for (trigger, want) in [
        ("pull_request:\n    branches: [main]", E::MissingBaseBranch),
        ("push:\n    branches: [main]", E::MissingHeadBranch),
        ("push:\n    paths: ['src/**']", E::MissingChangedFiles),
        (
            "push:\n    paths-ignore: ['docs/**']",
            E::MissingChangedFiles,
        ),
    ] {
        let f = Fixture::new(&WORKFLOW.replace("on: push", &format!("on:\n  {trigger}")));
        let m = f.capture();
        f.error(&m, &options(), want);
        let mut o = options();
        o.filters.workflows = vec!["other.yml".into()];
        assert!(build(&f.store, &m, &o).unwrap().workflows.is_empty());
        o.filters.workflows.clear();
        o.filters.include_disabled = false;
        o.disabled = vec![DisabledWorkflow {
            path: ".github/workflows/ci.yml".into(),
            state: "disabled_manually".into(),
        }];
        let p = build(&f.store, &m, &o).unwrap();
        assert_eq!(p.skipped.len(), 1);
        assert!(p.workflows.is_empty());
    }
    let f = Fixture::new(&WORKFLOW.replace(
        "on: push",
        "on:\n  pull_request:\n    branches: [main]\n    paths: ['src/**']",
    ));
    let m = f.capture();
    let mut o = options();
    o.base_branch = Some("main".into());
    f.error(&m, &o, E::MissingChangedFiles);
    o.changed_files = Some(vec![]);
    let empty = build(&f.store, &m, &o).unwrap();
    assert!(empty.workflows.is_empty());
    assert!(empty.source.pinned.as_ref().unwrap().changed_files_known);
    o.changed_files = Some(vec!["src/lib.rs".into()]);
    let changed = build(&f.store, &m, &o).unwrap();
    assert_eq!(changed.workflows.len(), 1);
    assert_ne!(changed.digest, empty.digest);
    o.changed_files = None;
    o.filters.all = true;
    assert_eq!(build(&f.store, &m, &o).unwrap().workflows.len(), 1);
}
#[test]
fn tampering_and_unsafe_planning_inputs_are_rejected_with_static_errors() {
    let f = Fixture::new(WORKFLOW);
    let m = f.capture();
    let mut bad = m.clone();
    bad.source_id.push('a');
    f.error(&bad, &options(), E::InvalidSource);
    let mut bad = m.clone();
    bad.schema_version += 1;
    f.error(&bad, &options(), E::InvalidSource);
    let mut bad = m.clone();
    bad.object_manifest.tree_digest.push('a');
    bad = source::seal_manifest(bad);
    f.error(&bad, &options(), E::InvalidSource);
    let mut bad = m.clone();
    bad.object_manifest.entries[0].size += 1;
    bad = source::seal_manifest(bad);
    f.error(&bad, &options(), E::InvalidSource);
    let mut bad = m.clone();
    bad.cwd_relative = RelPath::new("../outside");
    bad = source::seal_manifest(bad);
    f.error(&bad, &options(), E::InvalidSource);
    let mut bad = m.clone();
    bad.execution_commit = Some("a".repeat(40));
    bad = source::seal_manifest(bad);
    f.error(&bad, &options(), E::InvalidSource);
    let mut bad = m.clone();
    bad.source_kind = SourceKind::Commit;
    bad = source::seal_manifest(bad);
    f.error(&bad, &options(), E::InvalidSource);
    for path in ["../escape", "/absolute", "a/../escape"] {
        let mut o = options();
        o.filters.workflows_dirs = vec![path.into()];
        f.error(&m, &o, E::InvalidOptions);
    }
    let mut o = options();
    o.changed_files = Some(vec!["../secret-value".into()]);
    f.error(&m, &o, E::InvalidOptions);
    let mut o = options();
    o.base_branch = Some(String::new());
    f.error(&m, &o, E::InvalidOptions);
    let entry = &m.object_manifest.entries[0];
    std::fs::write(
        f.store.object_path(&entry.hash),
        "tampered-object-secret-value",
    )
    .unwrap();
    let e = build(&f.store, &m, &options()).unwrap_err();
    assert_eq!(e, E::InvalidSource);
    assert!(!e.to_string().contains("secret-value"));
}
#[test]
fn configured_directories_jobs_and_relative_cwd_use_captured_configuration() {
    let f = Fixture::new(WORKFLOW);
    std::fs::create_dir_all(f.root.join(".ci")).unwrap();
    std::fs::create_dir_all(f.root.join("sub")).unwrap();
    std::fs::write(f.root.join("sub/file"), "data").unwrap();
    std::fs::write(f.root.join(".local-ci.toml"), "workflows-dirs = ['.ci']\nno-cache=['local.yml:check']\nlink-node-modules=true\n[setup]\nignore-env=['CI']\n").unwrap();
    std::fs::write(f.root.join(".ci/local.yml"), WORKFLOW.replace("  check:", "  prerequisite:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo needed\n  check:\n    needs: prerequisite")).unwrap();
    let mut m = f.capture();
    m.cwd_relative = RelPath::new("sub");
    m = source::seal_manifest(m);
    let mut o = options();
    o.filters.jobs = vec!["check".into()];
    let p = build(&f.store, &m, &o).unwrap();
    assert_eq!(p.config.workflow_dirs, vec![".ci"]);
    assert_eq!(p.config.no_cache, vec!["local.yml:check"]);
    assert!(p.config.link_node_modules);
    assert_eq!(p.config.ignore_env, vec!["CI"]);
    assert_eq!(p.units[0].jobs.len(), 2);
    assert_eq!(p.source.pinned.unwrap().cwd_relative, RelPath::new("sub"));
    std::fs::write(
        f.root.join(".local-ci.toml"),
        "bad = 'private-parser-value'",
    )
    .unwrap();
    f.error(&f.capture(), &o, E::InvalidConfig);
}
#[test]
fn symlinks_secrets_and_invalid_workflows_never_become_parser_diagnostics() {
    let f = Fixture::new(WORKFLOW);
    let workflow = f.root.join(".github/workflows/ci.yml");
    std::fs::remove_file(&workflow).unwrap();
    std::os::unix::fs::symlink("/nonexistent/private-value", &workflow).unwrap();
    f.error(&f.capture(), &options(), E::UnavailablePlanningInput);
    std::fs::remove_file(&workflow).unwrap();
    std::fs::write(&workflow, "invalid: [private-value").unwrap();
    f.error(&f.capture(), &options(), E::InvalidWorkflow);
    std::fs::write(&workflow, WORKFLOW).unwrap();
    std::fs::write(f.root.join(".local-ci.toml"), "workflows-dirs=['missing']").unwrap();
    f.error(&f.capture(), &options(), E::UnavailablePlanningInput);
    std::fs::remove_file(f.root.join(".local-ci.toml")).unwrap();
    let mut m = f.capture();
    m.secret_files.push(source::SecretEntry {
        path: RelPath::new(".github/workflows/ci.yml"),
        hmac: "private-value".into(),
    });
    m = source::seal_manifest(m);
    f.error(&m, &options(), E::InvalidSource);
    m.secret_files.clear();
    m.secret_files.push(source::SecretEntry {
        path: RelPath::new(".local-ci.toml"),
        hmac: "private-value".into(),
    });
    m = source::seal_manifest(m);
    f.error(&m, &options(), E::UnavailablePlanningInput);
}
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("/usr/bin/git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
#[test]
fn dirty_worktree_and_explicit_old_commit_stay_fixed_after_original_changes() {
    let f = Fixture::new(WORKFLOW);
    git(&f.root, &["init", "-q", "-b", "main"]);
    git(&f.root, &["add", "."]);
    git(&f.root, &["commit", "-qm", "first"]);
    let old = git(&f.root, &["rev-parse", "HEAD"]);
    let checkout = f._tmp.path().join("old");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "--detach",
            checkout.to_str().unwrap(),
            &old,
        ],
    );
    std::fs::write(
        f.root.join(".github/workflows/ci.yml"),
        WORKFLOW.replace("captured", "dirty"),
    )
    .unwrap();
    let ctx = ExecContext::new(
        [
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), f._tmp.path().display().to_string()),
        ]
        .into(),
    );
    let opts = CaptureOptions {
        store: Some(f.store.root().into()),
        base: Some(old.clone()),
        ..Default::default()
    };
    let dirty = source::capture_source(&ctx, &f.root, &opts)
        .unwrap()
        .manifest;
    let mut commit = source::capture_source(&ctx, &checkout, &opts)
        .unwrap()
        .manifest;
    commit.source_kind = SourceKind::Commit;
    commit.execution_commit = Some(old.clone());
    commit = source::seal_manifest(commit);
    let a = build(&f.store, &dirty, &options()).unwrap();
    let b = build(&f.store, &commit, &options()).unwrap();
    assert_eq!(a.source.pinned.as_ref().unwrap().kind, SourceKind::Worktree);
    assert_eq!(b.source.execution_commit.as_deref(), Some(old.as_str()));
    assert_ne!(a.digest, b.digest);
    std::fs::write(f.root.join(".github/workflows/ci.yml"), "now invalid").unwrap();
    std::fs::write(checkout.join(".github/workflows/ci.yml"), "also invalid").unwrap();
    assert_eq!(build(&f.store, &dirty, &options()).unwrap(), a);
    assert_eq!(build(&f.store, &commit, &options()).unwrap(), b);
    for p in [&a, &b] {
        assert_eq!(
            p.require_git_source().unwrap_err().downcast_ref::<E>(),
            Some(&E::UnsupportedExecution)
        );
    }
}
#[test]
fn captured_schema_and_identity_cannot_be_discarded_or_reinterpreted() {
    let f = Fixture::new(WORKFLOW);
    let p = build(&f.store, &f.capture(), &options()).unwrap();
    for change in 0..6 {
        let mut p = p.clone();
        match change {
            0 => p.schema_version = 6,
            1 => p.source.pinned = None,
            2 => p.source.execution_commit = Some("a".repeat(40)),
            3 => p.source.pinned.as_mut().unwrap().manifest_digest = "bad".into(),
            4 => p.source.pinned.as_mut().unwrap().kind = SourceKind::Commit,
            _ => p.source.pinned.as_mut().unwrap().cwd_relative = RelPath::new("../outside"),
        };
        p.digest = p.compute_digest().unwrap();
        assert!(p.verify().is_err());
    }
}

#[test]
fn every_legacy_execution_entry_rejects_captured_plans_before_writes_or_tools() {
    struct Sentinel(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl local_ci::StepExecutor for Sentinel {
        fn run(&self, _: &local_ci::StepRequest) -> anyhow::Result<local_ci::StepExit> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("unexpected sentinel execution")
        }
    }
    let called = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let f = Fixture::new(&WORKFLOW.replace("echo captured", "npm ci"));
    let p = build(&f.store, &f.capture(), &options()).unwrap();
    let area = f._tmp.path().join("must-not-exist");
    let ws = local_ci::exec::Workspace {
        root: area.join("root"),
        repo_root: area.join("repo"),
        run_dir: area.join("run"),
        tools: area.join("tools"),
        lock_file: area.join("lock"),
    };
    let opts = || local_ci::exec::Opts {
        parallel: 1,
        traffic_controller: Some(area.join("no-controller")),
        executor: Some(std::sync::Arc::new(Sentinel(called.clone()))),
        portool: Some(area.join("no-portool")),
        containers: 1,
        job_timeout_min: 1.,
        keep: false,
        trash: area.join("trash"),
    };
    let check =
        |e: anyhow::Error| assert_eq!(e.downcast_ref::<E>(), Some(&E::UnsupportedExecution));
    check(local_ci::execute_plan(&f.ctx, &p, &ws, opts()).unwrap_err());
    check(
        local_ci::execute_task(
            &f.ctx,
            &p,
            "not-a-task",
            &Default::default(),
            None,
            &ws,
            opts(),
        )
        .unwrap_err(),
    );
    let recipe = local_ci::setup::derive_setup_recipe(&p, &p.tasks[0].id).unwrap();
    let ctx = local_ci::setup::TemplateContext::local(&p, "attempt");
    assert_eq!(
        ctx.source_digest,
        p.source.pinned.as_ref().unwrap().manifest_digest
    );
    let key = recipe.template_key(&ctx);
    check(local_ci::execute_setup(&f.ctx, &p, &recipe, &key, &ws, opts()).unwrap_err());
    check(
        local_ci::exec::Runner::new(&f.ctx, &p, &ws, opts())
            .err()
            .expect("captured constructor must fail"),
    );
    check(local_ci::report::assemble(&p, ws.run_dir.clone(), vec![], vec![], 0.).unwrap_err());
    // The runner exposes its plan reference publicly. Recheck each direct
    // execution boundary even if a caller replaces that reference after new.
    let mut legacy = p.clone();
    legacy.schema_version = 6;
    legacy.source.pinned = None;
    legacy.source.execution_commit = Some("a".repeat(40));
    legacy.source.base = Some("main".into());
    let mut runner = local_ci::exec::Runner::new(&f.ctx, &legacy, &ws, opts()).unwrap();
    runner.fp = &p;
    check(runner.prepare_tools().unwrap_err());
    assert!(!recipe.steps.is_empty());
    check(runner.run_recipe(&recipe, key.digest()).unwrap_err());
    let mut empty_recipe = recipe.clone();
    empty_recipe.steps.clear();
    check(runner.run_recipe(&empty_recipe, key.digest()).unwrap_err());
    check(runner.run_units().unwrap_err());
    check(runner.release_ports(&ws.root).unwrap_err());
    assert!(!area.exists());
    assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 0);
    let same = p.with_shards(&[]).unwrap();
    assert_eq!(same, p);
    same.verify().unwrap();
}

#[test]
fn directory_discovery_and_selection_have_explicit_boundaries() {
    #[cfg(target_os = "linux")]
    use std::ffi::OsString;
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStringExt;
    let f = Fixture::new(WORKFLOW);
    #[cfg(target_os = "linux")]
    std::fs::write(f.root.join(OsString::from_vec(vec![0xff])), b"unrelated").unwrap();
    std::fs::write(f.root.join("root.yaml"), WORKFLOW.replace("fixed", "extra")).unwrap();
    let m = f.capture();
    let mut o = options();
    o.filters.workflows_dirs = vec!["./".into(), ".github/workflows/".into()];
    let p = build(&f.store, &m, &o).unwrap();
    assert_eq!(p.workflows.len(), 2);
    assert_eq!(p.filters.workflows_dirs, vec!["", ".github/workflows"]);
    o.filters.include_disabled = false;
    o.filters.workflows = vec!["ci.yml".into()];
    o.disabled = vec![DisabledWorkflow {
        path: ".github/workflows/ci.yml".into(),
        state: "disabled_manually".into(),
    }];
    assert_eq!(build(&f.store, &m, &o).unwrap().skipped.len(), 1);
    o.filters.workflows = vec!["root.yaml".into()];
    let p = build(&f.store, &m, &o).unwrap();
    assert_eq!(p.workflows.len(), 1);
    assert!(p.skipped.is_empty());
    o.disabled.clear();
    assert_eq!(build(&f.store, &m, &o).unwrap().workflows.len(), 1);
    let mut m = m.clone();
    m.cwd_relative = RelPath::new("empty-subdir");
    m = source::seal_manifest(m);
    assert_eq!(
        build(&f.store, &m, &o)
            .unwrap()
            .source
            .pinned
            .unwrap()
            .cwd_relative,
        m.cwd_relative
    );
    #[cfg(target_os = "linux")]
    {
        std::fs::write(
            f.root
                .join(".github/workflows")
                .join(OsString::from_vec(b"bad-\xff.yml".to_vec())),
            WORKFLOW,
        )
        .unwrap();
        f.error(&f.capture(), &options(), E::UnavailablePlanningInput);
    }
}
#[test]
fn unsupported_inputs_and_tampered_repository_identity_fail_safely() {
    let f = Fixture::new(&WORKFLOW.replace("on: push", "on: workflow_dispatch"));
    let m = f.capture();
    assert!(build(&f.store, &m, &options())
        .unwrap()
        .workflows
        .is_empty());
    let mut bad = m.clone();
    bad.submodules.push(source::SubmoduleRef {
        path: RelPath::new("nested"),
        revision: "a".repeat(40),
        source_id: None,
        dirty: false,
    });
    bad = source::seal_manifest(bad);
    f.error(&bad, &options(), E::UnsupportedSource);
    let mut p = build(&f.store, &m, &options()).unwrap();
    p.repository = Some("owner/other".into());
    p.digest = p.compute_digest().unwrap();
    assert!(p.verify().is_err());
    let mut bad = m.clone();
    bad.branch = Some("unbound".into());
    bad = source::seal_manifest(bad);
    f.error(&bad, &options(), E::InvalidSource);
    let mut bad = m.clone();
    bad.secret_files.push(source::SecretEntry {
        path: RelPath::new(".github/workflows/secret.yml"),
        hmac: "private-value".into(),
    });
    bad = source::seal_manifest(bad);
    f.error(&bad, &options(), E::UnavailablePlanningInput);
    for e in [
        E::InvalidSource,
        E::InvalidOptions,
        E::InvalidConfig,
        E::InvalidWorkflow,
        E::UnavailablePlanningInput,
        E::MissingBaseBranch,
        E::MissingHeadBranch,
        E::MissingChangedFiles,
        E::UnsupportedSource,
        E::InvalidPlan,
        E::UnsupportedExecution,
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn each_identity_constraint_rejects_an_independently_resealed_plan() {
    let f = Fixture::new(WORKFLOW);
    let original = build(&f.store, &f.capture(), &options()).unwrap();
    for case in 0..13 {
        let mut p = original.clone();
        match case {
            0 => {
                p.source.source_id = format!("src-{}", "a".repeat(32));
                p.source.pinned.as_mut().unwrap().manifest_digest =
                    format!("sha256:{}", "a".repeat(63))
            }
            1 => {
                p.source.source_id = format!("src-{}", "z".repeat(32));
                p.source.pinned.as_mut().unwrap().manifest_digest =
                    format!("sha256:{}", "z".repeat(64))
            }
            2 => p.source.source_id.push('a'),
            3 => p.changed_files = vec!["src/lib.rs".into()],
            4 => p.source.pinned.as_mut().unwrap().base_branch = Some(String::new()),
            5 => p.source.base = Some(String::new()),
            6 => p.source.base = Some("main".into()),
            7 => p.branch = Some("feature".into()),
            8 => p.source.execution_commit = Some("a".repeat(40)),
            9 => {
                p.source.pinned.as_mut().unwrap().kind = SourceKind::Commit;
                p.source.execution_commit = Some("a".into());
            }
            10 => {
                p.source.pinned.as_mut().unwrap().kind = SourceKind::Worktree;
                p.source.execution_commit = Some("z".repeat(40));
            }
            11 => p.source.pinned.as_mut().unwrap().cwd_relative = RelPath::new("../escape"),
            _ => p.repository = Some("owner/other".into()),
        }
        p.digest = p.compute_digest().unwrap();
        assert!(p.verify().is_err(), "case {case}");
    }
}

#[test]
fn unavailable_default_workflow_directory_is_not_treated_as_absent() {
    let f = Fixture::new(WORKFLOW);
    std::fs::remove_file(f.root.join(".github/workflows/ci.yml")).unwrap();
    std::fs::remove_dir(f.root.join(".github/workflows")).unwrap();
    std::os::unix::fs::symlink(
        "/nonexistent/private-value",
        f.root.join(".github/workflows"),
    )
    .unwrap();
    f.error(&f.capture(), &options(), E::UnavailablePlanningInput);
    std::fs::remove_file(f.root.join(".github/workflows")).unwrap();
    std::fs::remove_dir(f.root.join(".github")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/private-value", f.root.join(".github")).unwrap();
    f.error(&f.capture(), &options(), E::UnavailablePlanningInput);
    std::fs::remove_file(f.root.join(".github")).unwrap();
    let m = f.capture();
    assert!(build(&f.store, &m, &options())
        .unwrap()
        .workflows
        .is_empty());
    std::fs::write(
        f.root.join(".local-ci.toml"),
        "workflows-dirs=['bad\\path']",
    )
    .unwrap();
    f.error(&f.capture(), &options(), E::InvalidConfig);
}

#[test]
fn authoritative_workflow_directories_override_captured_config_then_add_extras() {
    let f = Fixture::new(WORKFLOW);
    std::fs::write(
        f.root.join(".local-ci.toml"),
        "workflows-dirs=['missing']\nno-cache=['ci.yml:check']",
    )
    .unwrap();
    std::fs::create_dir_all(f.root.join("extra")).unwrap();
    std::fs::write(
        f.root.join("extra/extra.yml"),
        WORKFLOW.replace("fixed", "extra"),
    )
    .unwrap();
    let m = f.capture();
    f.error(&m, &options(), E::UnavailablePlanningInput);
    let mut o = options();
    o.workflow_dirs_override = Some(vec![
        "./.github/workflows/".into(),
        ".github/workflows".into(),
    ]);
    let p = build(&f.store, &m, &o).unwrap();
    assert_eq!(p.workflows.len(), 1);
    assert_eq!(p.config.workflow_dirs, vec![".github/workflows"]);
    assert_eq!(p.config.no_cache, vec!["ci.yml:check"]);
    o.filters.workflows_dirs = vec!["extra".into()];
    let extra = build(&f.store, &m, &o).unwrap();
    assert_eq!(extra.workflows.len(), 2);
    assert_eq!(
        extra.config.workflow_dirs,
        vec![".github/workflows", "extra"]
    );
    assert_ne!(extra.digest, p.digest);
    for dirs in [
        vec![],
        vec!["../outside".into()],
        vec!["/absolute".into()],
        vec![".".into()],
    ] {
        o.workflow_dirs_override = Some(dirs);
        f.error(&m, &o, E::InvalidOptions);
    }
}

#[test]
fn known_skips_do_not_require_irrelevant_event_metadata() {
    for (trigger, base, changed, reason) in [
        (
            "pull_request:\n    branches: [main]\n    paths: ['src/**']",
            Some("dev"),
            None,
            "branch filter",
        ),
        (
            "pull_request:\n    types: [closed]\n    branches: [main]\n    paths: ['src/**']",
            None,
            None,
            "types closed",
        ),
        (
            "push:\n    tags: ['v*']\n    paths: ['src/**']",
            None,
            None,
            "tags only",
        ),
        (
            "pull_request:\n    branches: [main]\n    paths: ['src/**']",
            None,
            Some(vec!["docs/readme.md".into()]),
            "paths not matched",
        ),
    ] {
        let f = Fixture::new(&WORKFLOW.replace("on: push", &format!("on:\n  {trigger}")));
        let mut o = options();
        o.base_branch = base.map(str::to_string);
        o.changed_files = changed;
        let p = build(&f.store, &f.capture(), &o).unwrap();
        assert!(p.workflows.is_empty());
        assert_eq!(p.skipped.len(), 1);
        assert!(
            p.skipped[0].reason.contains(reason),
            "{}",
            p.skipped[0].reason
        );
    }
}
#[test]
fn ordered_trigger_alternatives_preserve_the_event_or_report_real_uncertainty() {
    for (triggers, expected) in [
        (
            "push: {}\n  pull_request:\n    branches: [main]",
            Ok("push"),
        ),
        (
            "pull_request:\n    paths: ['src/**']\n  pull_request_target: {}",
            Ok("pull_request"),
        ),
        (
            "pull_request:\n    branches: [main]\n  pull_request_target: {}",
            Ok("pull_request"),
        ),
        (
            "pull_request:\n    branches: [main]\n  push: {}",
            Err(E::MissingBaseBranch),
        ),
        (
            "push:\n    paths: ['src/**']\n  pull_request: {}",
            Err(E::MissingChangedFiles),
        ),
        (
            "pull_request:\n    types: [closed]\n    paths: ['src/**']\n  push: {}",
            Ok("push"),
        ),
    ] {
        let f = Fixture::new(&WORKFLOW.replace("on: push", &format!("on:\n  {triggers}")));
        match expected {
            Ok(event) => {
                let p = build(&f.store, &f.capture(), &options()).unwrap();
                assert_eq!(p.workflows.len(), 1);
                assert_eq!(p.workflows[0].event, event);
            }
            Err(error) => f.error(&f.capture(), &options(), error),
        }
    }
}
