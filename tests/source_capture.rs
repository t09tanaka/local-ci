//! Capturing a working directory: what it takes, that it leaves the original
//! alone, that it notices movement, and what it hands to another host.

#![allow(clippy::disallowed_methods)] // the tests build their own commands

mod common;
use local_ci::source::{
    capture_directory, capture_source, export_objects, import_objects, materialize_secrets,
    materialize_source, publishable, remove_imported_refs, seal_manifest, secret_digest,
    CaptureOptions, ObjectStore, SecretBundle, SecretSpec, SourceError, SourceInfo, SourceKind,
    Stage,
};
use local_ci::ExecContext;
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .current_dir(dir)
        // no user or system config: a machine-wide git-lfs filter would run
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn git(dir: &Path, args: &[&str]) {
    git_out(dir, args);
}

fn ctx(home: &Path) -> ExecContext {
    ExecContext::new(
        [
            ("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin"),
            ("HOME", home.to_str().unwrap()),
            ("LOCAL_CI_HOME", home.to_str().unwrap()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    )
}

struct Fixture {
    _tmp: common::Tmp,
    home: PathBuf,
    dir: PathBuf,
}

impl Fixture {
    fn ctx(&self) -> ExecContext {
        ctx(&self.home)
    }
}

/// A repository with a bit of everything committed.
fn fixture() -> Fixture {
    let tmp = common::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join(".gitignore"), "ignored.log\n.env\nout/\n").unwrap();
    for (f, c) in [
        ("a.txt", "a\n"),
        ("staged.txt", "s0\n"),
        ("unstaged.txt", "u0\n"),
        ("gone.txt", "g\n"),
        ("run.sh", "#!/bin/sh\necho hi\n"),
    ] {
        std::fs::write(dir.join(f), c).unwrap();
    }
    std::fs::write(dir.join("blob.bin"), [0u8, 159, 146, 150, 1, 2, 3]).unwrap();
    std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
    std::fs::create_dir_all(dir.join("sub dir")).unwrap();
    std::fs::write(dir.join("sub dir/new\u{3042}.txt"), "kana\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    Fixture {
        _tmp: tmp,
        home,
        dir,
    }
}

fn dirty(f: &Fixture) {
    let d = &f.dir;
    std::fs::write(d.join("staged.txt"), "s1\n").unwrap();
    git(d, &["add", "staged.txt"]);
    std::fs::write(d.join("unstaged.txt"), "u1\n").unwrap();
    std::fs::write(d.join("staged_then_changed.txt"), "x0\n").unwrap();
    git(d, &["add", "staged_then_changed.txt"]);
    std::fs::write(d.join("staged_then_changed.txt"), "x1\n").unwrap();
    std::fs::remove_file(d.join("gone.txt")).unwrap();
    std::fs::set_permissions(d.join("run.sh"), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(d.join("blob.bin"), [0u8, 255, 254, 0, 9]).unwrap();
    std::fs::remove_file(d.join("link")).unwrap();
    std::os::unix::fs::symlink("/outside/the/repo", d.join("link")).unwrap();
    std::fs::write(d.join("untracked.txt"), "new\n").unwrap();
    std::fs::write(d.join("ignored.log"), "noise\n").unwrap();
    std::fs::write(d.join("exec_new.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(
        d.join("exec_new.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

/// path -> (kind, executable, content or link value)
fn listing(root: &Path, skip: &[&str]) -> BTreeMap<String, (char, bool, Vec<u8>)> {
    fn walk(
        root: &Path,
        rel: &Path,
        skip: &[&str],
        out: &mut BTreeMap<String, (char, bool, Vec<u8>)>,
    ) {
        for e in std::fs::read_dir(root.join(rel)).unwrap() {
            let e = e.unwrap();
            let p = rel.join(e.file_name());
            let name = p.to_string_lossy().into_owned();
            if skip
                .iter()
                .any(|s| name == *s || name.starts_with(&format!("{s}/")))
            {
                continue;
            }
            let m = std::fs::symlink_metadata(root.join(&p)).unwrap();
            if m.is_dir() {
                walk(root, &p, skip, out);
            } else if m.file_type().is_symlink() {
                let t = std::fs::read_link(root.join(&p)).unwrap();
                out.insert(name, ('l', false, t.to_string_lossy().as_bytes().to_vec()));
            } else {
                out.insert(
                    name,
                    (
                        'f',
                        m.mode() & 0o111 != 0,
                        std::fs::read(root.join(&p)).unwrap(),
                    ),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, Path::new(""), skip, &mut out);
    out
}

/// Every file under `.git` with its size and bytes: any write shows.
fn git_state(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    listing(&dir.join(".git"), &[])
        .into_iter()
        .map(|(k, v)| (k, v.2))
        .collect()
}

fn refs_state(dir: &Path) -> String {
    format!(
        "{}\n{}\n{}",
        git_out(dir, &["for-each-ref"]),
        git_out(dir, &["rev-parse", "HEAD"]),
        git_out(dir, &["stash", "list"])
    )
}

fn opts() -> CaptureOptions {
    CaptureOptions::default()
}

// ---- A11 ------------------------------------------------------------------

#[test]
fn a11_a_dirty_worktree_is_captured_and_the_original_is_untouched() {
    let f = fixture();
    dirty(&f);
    let c = f.ctx();
    // refresh the index's stat data first; `git status` writes it
    git(&f.dir, &["status", "--porcelain"]);
    let before_git = git_state(&f.dir);
    let before_refs = refs_state(&f.dir);
    let before_wt = listing(&f.dir, &[".git"]);

    let snap = capture_source(&c, &f.dir, &opts()).unwrap();

    assert_eq!(git_state(&f.dir), before_git, ".git was written");
    assert_eq!(refs_state(&f.dir), before_refs);
    assert_eq!(
        listing(&f.dir, &[".git"]),
        before_wt,
        "the worktree changed"
    );
    let porcelain_before = git_out(&f.dir, &["status", "--porcelain"]);

    let m = &snap.manifest;
    assert_eq!(m.source_kind, SourceKind::Worktree);
    assert_eq!(
        m.original_head.as_deref(),
        Some(git_out(&f.dir, &["rev-parse", "HEAD"]).as_str())
    );
    assert_eq!(m.branch.as_deref(), Some("main"));
    assert!(m.execution_commit.is_some() && m.worktree_tree.is_some() && m.index_tree.is_some());
    assert_ne!(m.index_tree, m.worktree_tree);

    let dest = f.home.join("ws");
    materialize_source(&c, m, &dest).unwrap();
    let got = listing(&dest, &[]);
    let mut want = listing(&f.dir, &[".git", "ignored.log"]);
    // the ignored file is not captured
    want.remove("ignored.log");
    assert_eq!(got, want);
    // specifically
    assert_eq!(got["staged.txt"].2, b"s1\n");
    assert_eq!(got["unstaged.txt"].2, b"u1\n");
    assert_eq!(got["staged_then_changed.txt"].2, b"x1\n");
    assert!(!got.contains_key("gone.txt"));
    assert!(!got["run.sh"].1 && got["exec_new.sh"].1);
    assert_eq!(got["blob.bin"].2, vec![0u8, 255, 254, 0, 9]);
    assert_eq!(got["link"], ('l', false, b"/outside/the/repo".to_vec()));
    assert_eq!(got["untracked.txt"].2, b"new\n");
    assert!(got.contains_key("sub dir/new\u{3042}.txt"));
    assert!(!got.contains_key("ignored.log"));

    // the synthetic commit: parent is the original HEAD, content is the capture
    let exec = m.execution_commit.clone().unwrap();
    let private = std::fs::read_dir(f.home.join("sources/git"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let tree = git_out(&private, &["rev-parse", &format!("{exec}^{{tree}}")]);
    assert_eq!(Some(tree), m.worktree_tree);
    let parent = git_out(&private, &["rev-parse", &format!("{exec}^")]);
    assert_eq!(Some(parent), m.original_head);
    // and the original has no trace of it
    assert!(
        Command::new("git")
            .args(["cat-file", "-e", &exec])
            .current_dir(&f.dir)
            .status()
            .unwrap()
            .code()
            .unwrap()
            != 0
    );
    assert_eq!(
        git_out(&f.dir, &["status", "--porcelain"]),
        porcelain_before
    );
    assert!(!git_out(&f.dir, &["for-each-ref"]).contains("local-ci"));
}

#[test]
fn a_tracked_file_is_taken_even_when_ignore_matches_it_and_exclude_leaves_things_out() {
    let f = fixture();
    let d = &f.dir;
    std::fs::write(d.join("tracked.log"), "t\n").unwrap();
    std::fs::write(d.join(".gitignore"), "ignored.log\n.env\nout/\n*.log\n").unwrap();
    git(d, &["add", "-f", "tracked.log"]);
    git(d, &["commit", "-qam", "ignore"]);
    std::fs::write(d.join("ignored.log"), "i\n").unwrap();
    std::fs::create_dir_all(d.join("out")).unwrap();
    std::fs::write(d.join("out/build.js"), "b\n").unwrap();
    std::fs::write(d.join("scratch.txt"), "s\n").unwrap();
    let c = f.ctx();

    let paths = |m: &local_ci::source::SourceManifest| -> Vec<String> {
        m.object_manifest
            .entries
            .iter()
            .map(|e| e.path.display())
            .collect()
    };
    let plain = capture_source(&c, d, &opts()).unwrap().manifest;
    assert!(paths(&plain).contains(&"tracked.log".to_string()));
    assert!(!paths(&plain).contains(&"ignored.log".to_string()));
    assert!(!paths(&plain).contains(&"out/build.js".to_string()));
    assert!(paths(&plain).contains(&"scratch.txt".to_string()));

    let mut o = opts();
    o.include = vec!["out/**".into()];
    o.exclude = vec!["scratch.txt".into(), "a.txt".into()];
    let m = capture_source(&c, d, &o).unwrap().manifest;
    assert!(paths(&m).contains(&"out/build.js".to_string()));
    assert!(!paths(&m).contains(&"scratch.txt".to_string()));
    assert!(
        !paths(&m).contains(&"a.txt".to_string()),
        "exclude wins over tracked"
    );
    assert_ne!(m.source_id, plain.source_id);
}

#[test]
fn a_fifo_and_a_nested_repository_are_explicit_errors() {
    let f = fixture();
    let c = f.ctx();
    let fifo = f.dir.join("pipe");
    assert!(Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    let err = capture_source(&c, &f.dir, &opts()).unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<SourceError>(),
            Some(SourceError::UnsupportedFileType { .. })
        ),
        "{err:#}"
    );
    std::fs::remove_file(&fifo).unwrap();

    let nested = f.dir.join("vendor/lib");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    std::fs::write(nested.join("x"), "x").unwrap();
    git(&nested, &["add", "x"]);
    git(&nested, &["commit", "-qm", "x"]);
    let err = capture_source(&c, &f.dir, &opts()).unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<SourceError>(),
            Some(SourceError::NestedRepository { .. })
        ),
        "{err:#}"
    );
    let mut o = opts();
    o.exclude = vec!["vendor".into()];
    capture_source(&c, &f.dir, &o).unwrap();
}

#[test]
fn paths_that_are_not_utf8_survive_the_manifest() {
    use local_ci::source::RelPath;
    let p = RelPath(vec![b'a', 0xff, b'/', b'b']);
    let json = serde_json::to_string(&p).unwrap();
    assert_eq!(serde_json::from_str::<RelPath>(&json).unwrap(), p);
    let q = RelPath::new("plain/path");
    assert_eq!(serde_json::to_string(&q).unwrap(), "\"plain/path\"");
}

#[cfg(target_os = "linux")]
#[test]
fn a_file_with_a_non_utf8_name_is_captured_and_materialized() {
    use std::os::unix::ffi::OsStrExt;
    let f = fixture();
    let name = std::ffi::OsStr::from_bytes(b"odd\xff\xfename");
    std::fs::write(f.dir.join(name), "odd").unwrap();
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let dest = f.home.join("ws");
    materialize_source(&c, &m, &dest).unwrap();
    assert_eq!(std::fs::read(dest.join(name)).unwrap(), b"odd");
}

// ---- A12 ------------------------------------------------------------------

fn hook(
    f: impl Fn(Stage) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Option<local_ci::source::Hook> {
    Some(Arc::new(f))
}

#[test]
fn a12_a_file_written_during_capture_is_retried_and_settles() {
    let f = fixture();
    dirty(&f);
    let c = f.ctx();
    let file = f.dir.join("unstaged.txt");
    let calls = Arc::new(AtomicU32::new(0));
    let n = calls.clone();
    let mut o = opts();
    o.hook = hook(move |s| {
        if let Stage::AfterRead(attempt) = s {
            n.fetch_add(1, Ordering::SeqCst);
            if attempt == 1 {
                std::fs::write(&file, "changed during the read\n").unwrap();
            }
        }
        Ok(())
    });
    let snap = capture_source(&c, &f.dir, &o).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let dest = f.home.join("ws");
    materialize_source(&c, &snap.manifest, &dest).unwrap();
    assert_eq!(
        std::fs::read(dest.join("unstaged.txt")).unwrap(),
        b"changed during the read\n"
    );
}

fn busy_with(f: &Fixture, mutate: impl Fn(u32) + Send + Sync + 'static) -> anyhow::Error {
    let c = f.ctx();
    let mut o = opts();
    o.hook = hook(move |s| {
        if let Stage::AfterRead(n) = s {
            mutate(n);
        }
        Ok(())
    });
    capture_source(&c, &f.dir, &o).unwrap_err()
}

#[test]
fn a12_three_unstable_attempts_are_source_busy() {
    let f = fixture();
    let file = f.dir.join("a.txt");
    let err = busy_with(&f, move |n| {
        std::fs::write(&file, format!("v{n}\n")).unwrap()
    });
    match err.downcast_ref::<SourceError>() {
        Some(SourceError::Busy { attempts, .. }) => assert_eq!(*attempts, 3),
        other => panic!("{other:?}: {err:#}"),
    }
    assert!(format!("{err}").starts_with("SOURCE_BUSY"));
}

#[test]
fn a12_a_rename_or_a_moved_head_during_capture_is_noticed() {
    let f = fixture();
    let d = f.dir.clone();
    let err = busy_with(&f, move |n| {
        let from = d.join(if n == 1 {
            "a.txt".into()
        } else {
            format!("r{}.txt", n - 1)
        });
        std::fs::rename(from, d.join(format!("r{n}.txt"))).unwrap();
    });
    assert!(
        matches!(
            err.downcast_ref::<SourceError>(),
            Some(SourceError::Busy { .. })
        ),
        "{err:#}"
    );

    let f = fixture();
    let d = f.dir.clone();
    let err = busy_with(&f, move |n| {
        std::fs::write(d.join(format!("c{n}.txt")), "c").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["commit", "-qm", &format!("c{n}")]);
    });
    match err.downcast_ref::<SourceError>() {
        Some(SourceError::Busy { last, .. }) => assert!(last.contains("HEAD moved"), "{last}"),
        other => panic!("{other:?}"),
    }
}

// ---- A13 ------------------------------------------------------------------

fn published(store: &Path) -> Vec<String> {
    std::fs::read_dir(store.join("manifests"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn a13_a_capture_that_stops_before_publishing_leaves_nothing_to_run() {
    let f = fixture();
    dirty(&f);
    let c = f.ctx();
    let store = f.home.join("sources");
    let mut o = opts();
    o.hook = hook(|s| {
        if s == Stage::BeforePublish {
            anyhow::bail!("crash");
        }
        Ok(())
    });
    assert!(capture_source(&c, &f.dir, &o).is_err());
    assert!(published(&store).is_empty(), "{:?}", published(&store));

    // run again: published, and the same id a second time
    let a = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let b = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    assert_eq!(a.source_id, b.source_id);
    assert_eq!(published(&store), vec![format!("{}.json", a.source_id)]);
    let loaded = ObjectStore::open(&store)
        .unwrap()
        .load_manifest(&a.source_id)
        .unwrap();
    assert_eq!(loaded.as_ref(), Some(&a));
    // and the same content captured into another store is the same source
    let mut o2 = opts();
    o2.store = Some(f.home.join("other-store"));
    let c2 = capture_source(&c, &f.dir, &o2).unwrap().manifest;
    assert_eq!(c2.source_id, a.source_id);
    assert_eq!(c2.execution_commit, a.execution_commit);
    // different content, different id
    std::fs::write(f.dir.join("untracked.txt"), "other\n").unwrap();
    let d = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    assert_ne!(d.source_id, a.source_id);
}

#[test]
fn a_manifest_that_was_edited_is_not_believed() {
    let f = fixture();
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let store = ObjectStore::open(f.home.join("sources")).unwrap();
    let p = store.manifest_path(&m.source_id);
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("\"main\"", "\"evil\"");
    std::fs::write(&p, text).unwrap();
    assert!(store.load_manifest(&m.source_id).is_err());
}

// ---- bundles --------------------------------------------------------------

#[test]
fn a14_unpushed_commits_and_the_snapshot_reach_a_clone_that_never_saw_them() {
    let tmp = common::tempdir().unwrap();
    let home = tmp.path().join("home");
    let origin = tmp.path().join("origin.git");
    let work = tmp.path().join("work");
    let worker = tmp.path().join("worker");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(
        tmp.path(),
        &["clone", "-q", origin.to_str().unwrap(), "work"],
    );
    std::fs::write(work.join("a.txt"), "a\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "pushed"]);
    git(&work, &["push", "-q", "origin", "HEAD:main"]);
    git(&work, &["fetch", "-q"]);
    git(
        tmp.path(),
        &["clone", "-q", origin.to_str().unwrap(), "worker"],
    );
    // two commits that were never pushed, and dirty work on top
    std::fs::write(work.join("b.txt"), "b\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "unpushed 1"]);
    std::fs::write(work.join("b.txt"), "b2\n").unwrap();
    git(&work, &["commit", "-qam", "unpushed 2"]);
    std::fs::write(work.join("b.txt"), "dirty\n").unwrap();
    std::fs::write(work.join("new.txt"), "new\n").unwrap();

    let c = ctx(&home);
    let mut o = opts();
    o.base = Some("origin/main".into());
    let m = capture_source(&c, &work, &o).unwrap().manifest;
    assert_eq!(
        m.merge_base.as_deref(),
        Some(git_out(&work, &["rev-parse", "origin/main"]).as_str())
    );
    let exec = m.execution_commit.clone().unwrap();
    let head = m.original_head.clone().unwrap();

    let none = |sha: &str| {
        Command::new("git")
            .args(["cat-file", "-e", sha])
            .current_dir(&worker)
            .status()
            .unwrap()
            .code()
            .unwrap()
            != 0
    };
    assert!(none(&exec) && none(&head));

    let bundle = export_objects(&c, &m).unwrap();
    assert!(bundle.is_file());
    assert_eq!(import_objects(&c, &worker, &bundle, &m).unwrap(), exec);
    assert!(!none(&head), "the unpushed history came along");
    git(&worker, &["checkout", "-q", "--detach", &exec]);
    let want = listing(&work, &[".git"]);
    assert_eq!(listing(&worker, &[".git"]), want);
    assert_eq!(std::fs::read(worker.join("b.txt")).unwrap(), b"dirty\n");
    // the worker's own branches are as they were
    assert_eq!(
        git_out(&worker, &["rev-parse", "origin/main"]),
        git_out(&work, &["rev-parse", "origin/main"])
    );

    // the original's refs do not name the snapshot
    assert!(!git_out(&work, &["for-each-ref"]).contains("local-ci"));

    // a clone without the base cannot take the bundle
    let stranger = tmp.path().join("stranger");
    std::fs::create_dir_all(&stranger).unwrap();
    git(&stranger, &["init", "-q"]);
    assert!(import_objects(&c, &stranger, &bundle, &m).is_err());
}

// ---- secret files ---------------------------------------------------------

const KEY: &[u8] = b"cluster-key-0123456789";

fn spec(glob: &str, required: bool) -> SecretSpec {
    SecretSpec {
        glob: glob.into(),
        required,
    }
}

#[test]
fn secret_files_stay_out_of_the_store_the_manifest_and_the_bundle_of_objects() {
    let f = fixture();
    let secret = "API_TOKEN=correct-horse-battery-staple\n";
    std::fs::write(f.dir.join(".env"), secret).unwrap();
    let c = f.ctx();
    let mut o = opts();
    o.secret_files = vec![spec(".env", true)];
    o.secret_key = KEY.to_vec();
    let snap = capture_source(&c, &f.dir, &o).unwrap();
    let m = &snap.manifest;
    assert_eq!(m.secret_files.len(), 1);
    assert_eq!(m.secret_files[0].path.display(), ".env");
    assert!(m.secret_files[0].hmac.starts_with("hmac-sha256:"));
    assert!(!m
        .object_manifest
        .entries
        .iter()
        .any(|e| e.path.display() == ".env"));

    use sha2::{Digest, Sha256};
    let plain_hash: String = Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let hex_secret: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
    let store = f.home.join("sources");
    let bundle_path = snap.secrets.clone().unwrap();
    // nothing but the secret bundle mentions the content, in any spelling
    let mut seen_bundle = false;
    for (name, (_, _, bytes)) in listing(&store, &[]) {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let is_bundle = store.join(&name) == bundle_path;
        seen_bundle |= is_bundle;
        for needle in [
            secret,
            "correct-horse",
            plain_hash.as_str(),
            hex_secret.as_str(),
        ] {
            let hit = text.contains(needle)
                || bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
            assert_eq!(hit, is_bundle && needle == hex_secret, "{name} / {needle}");
        }
    }
    assert!(seen_bundle);
    assert_eq!(
        std::fs::metadata(&bundle_path).unwrap().mode() & 0o777,
        0o600
    );
    // the git bundle of the snapshot has no trace either
    let gb = export_objects(&c, m).unwrap();
    let gbytes = std::fs::read(gb).unwrap();
    assert!(!gbytes.windows(13).any(|w| w == b"correct-horse"));

    // the key matters, the content matters
    let mut o2 = o.clone();
    o2.secret_key = b"another-key-0123456789".to_vec();
    let m2 = capture_source(&c, &f.dir, &o2).unwrap().manifest;
    assert_ne!(m2.secret_files[0].hmac, m.secret_files[0].hmac);
    assert_ne!(m2.source_id, m.source_id);

    // placed in a workspace at 0600, checked against the HMAC
    let dest = f.home.join("ws");
    materialize_source(&c, m, &dest).unwrap();
    let bundle = SecretBundle::load(&bundle_path).unwrap();
    materialize_secrets(m, &bundle, KEY, &dest).unwrap();
    assert_eq!(std::fs::read_to_string(dest.join(".env")).unwrap(), secret);
    assert_eq!(
        std::fs::metadata(dest.join(".env")).unwrap().mode() & 0o777,
        0o600
    );
    let dest2 = f.home.join("ws2");
    materialize_source(&c, m, &dest2).unwrap();
    assert!(materialize_secrets(m, &bundle, b"wrong-key", &dest2).is_err());
    assert!(!dest2.join(".env").exists());
}

#[test]
fn a_required_secret_that_is_missing_fails_the_capture_and_an_optional_one_warns() {
    let f = fixture();
    let c = f.ctx();
    let mut o = opts();
    o.secret_key = KEY.to_vec();
    o.secret_files = vec![spec(".env.test", true)];
    let err = capture_source(&c, &f.dir, &o).unwrap_err();
    assert!(matches!(
        err.downcast_ref::<SourceError>(),
        Some(SourceError::RequiredSecretMissing { .. })
    ));
    o.secret_files = vec![spec(".env.test", false)];
    let snap = capture_source(&c, &f.dir, &o).unwrap();
    assert!(snap.secrets.is_none());
    assert!(snap.warnings.iter().any(|w| w.contains(".env.test")));
}

#[test]
fn a_secret_file_does_not_stop_a_clean_commit_from_matching() {
    let f = fixture();
    std::fs::write(f.dir.join(".env"), "S=1\n").unwrap();
    let c = f.ctx();
    let mut o = opts();
    o.secret_key = KEY.to_vec();
    o.secret_files = vec![spec(".env", false)];
    let m = capture_source(&c, &f.dir, &o).unwrap().manifest;
    let head = git_out(&f.dir, &["rev-parse", "HEAD"]);
    publishable(&c, &f.dir, &SourceInfo::of(&m), &head).unwrap();
}

// ---- A32 ------------------------------------------------------------------

#[test]
fn a32_a_dirty_or_synthetic_or_directory_result_is_not_the_status_of_head() {
    let f = fixture();
    let c = f.ctx();
    let head = git_out(&f.dir, &["rev-parse", "HEAD"]);

    // clean: the captured content is HEAD's content (the synthetic commit is
    // another commit, with the same tree)
    let clean = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    assert_ne!(clean.execution_commit.as_deref(), Some(head.as_str()));
    let info = SourceInfo::of(&clean);
    assert_eq!(
        local_ci::publish_target(
            &c,
            &f.dir,
            clean.execution_commit.as_deref().unwrap(),
            Some(&info)
        )
        .unwrap(),
        head
    );

    // dirty
    dirty(&f);
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let info = SourceInfo::of(&m);
    let err = publishable(&c, &f.dir, &info, &head).unwrap_err();
    assert!(err.contains("uncommitted"), "{err}");
    assert!(local_ci::publish_target(
        &c,
        &f.dir,
        m.execution_commit.as_deref().unwrap(),
        Some(&info)
    )
    .is_err());

    // an untracked file alone is enough
    let f2 = fixture();
    std::fs::write(f2.dir.join("extra.txt"), "x").unwrap();
    let c2 = f2.ctx();
    let m2 = capture_source(&c2, &f2.dir, &opts()).unwrap().manifest;
    let head2 = git_out(&f2.dir, &["rev-parse", "HEAD"]);
    assert!(publishable(&c2, &f2.dir, &SourceInfo::of(&m2), &head2).is_err());

    // a tracked file left out by exclude is not the commit either
    let mut o = opts();
    o.exclude = vec!["a.txt".into()];
    let m3 = capture_source(&c, &f2.dir.clone(), &o);
    let _ = m3; // (captured from f2 with c: a different store, same repo)
    let f3 = fixture();
    let c3 = f3.ctx();
    let m3 = capture_source(&c3, &f3.dir, &o).unwrap().manifest;
    let head3 = git_out(&f3.dir, &["rev-parse", "HEAD"]);
    assert!(publishable(&c3, &f3.dir, &SourceInfo::of(&m3), &head3).is_err());

    // a directory never is
    let plain = f.home.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("x"), "x").unwrap();
    let dm = capture_directory(&c, &plain, &opts()).unwrap().manifest;
    let dinfo = SourceInfo::of(&dm);
    assert!(publishable(&c, &f.dir, &dinfo, &head).is_err());
    assert!(local_ci::publish_target(&c, &f.dir, &head, Some(&dinfo)).is_err());

    // a plain commit run is as before
    assert_eq!(
        local_ci::publish_target(&c, &f.dir, &head, None).unwrap(),
        head
    );
}

// ---- submodules, LFS, filters, directories ---------------------------------

fn with_submodule() -> Fixture {
    let f = fixture();
    let tmp = f.dir.parent().unwrap().to_path_buf();
    let lib = tmp.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    git(&lib, &["init", "-q", "-b", "main"]);
    std::fs::write(lib.join("lib.txt"), "lib\n").unwrap();
    git(&lib, &["add", "."]);
    git(&lib, &["commit", "-qm", "lib"]);
    git(
        &f.dir,
        &[
            "submodule",
            "add",
            "-q",
            lib.to_str().unwrap(),
            "vendor/lib",
        ],
    );
    git(&f.dir, &["commit", "-qm", "add submodule"]);
    f
}

#[test]
fn a14_a_clean_submodule_is_pinned_and_a_dirty_one_is_captured_whole() {
    let f = with_submodule();
    let c = f.ctx();
    let rev = git_out(&f.dir.join("vendor/lib"), &["rev-parse", "HEAD"]);

    let clean = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    assert_eq!(clean.submodules.len(), 1);
    let s = &clean.submodules[0];
    assert_eq!(
        (s.path.display().as_str(), s.revision.as_str(), s.dirty),
        ("vendor/lib", rev.as_str(), false)
    );
    assert!(s.source_id.is_some());
    // nothing dirty anywhere: this is HEAD's content
    let head = git_out(&f.dir, &["rev-parse", "HEAD"]);
    publishable(&c, &f.dir, &SourceInfo::of(&clean), &head).unwrap();

    std::fs::write(f.dir.join("vendor/lib/lib.txt"), "lib, edited\n").unwrap();
    std::fs::write(f.dir.join("vendor/lib/untracked.txt"), "u\n").unwrap();
    let dirty = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let s = &dirty.submodules[0];
    assert!(s.dirty);
    assert_eq!(s.revision, rev);
    assert_ne!(dirty.source_id, clean.source_id);
    assert!(publishable(&c, &f.dir, &SourceInfo::of(&dirty), &head).is_err());

    let dest = f.home.join("ws");
    materialize_source(&c, &dirty, &dest).unwrap();
    assert_eq!(
        std::fs::read(dest.join("vendor/lib/lib.txt")).unwrap(),
        b"lib, edited\n"
    );
    assert_eq!(
        std::fs::read(dest.join("vendor/lib/untracked.txt")).unwrap(),
        b"u\n"
    );
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"a\n");
    // the superproject's index still records the pinned revision
    assert_eq!(git_out(&f.dir, &["rev-parse", ":vendor/lib"]), rev);
}

fn lfs_fixture(payload_in_worktree: bool, payload_in_store: bool) -> (Fixture, String, Vec<u8>) {
    use sha2::{Digest, Sha256};
    let f = fixture();
    let payload = b"a large file\n".repeat(50);
    let oid: String = Sha256::digest(&payload)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        payload.len()
    );
    std::fs::write(
        f.dir.join(".gitattributes"),
        "*.dat filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();
    // no git-lfs here: commit the pointer as it is, without filters
    std::fs::write(f.dir.join("big.dat"), &pointer).unwrap();
    let blob = git_out(&f.dir, &["hash-object", "-w", "--no-filters", "big.dat"]);
    git(&f.dir, &["add", ".gitattributes"]);
    git(
        &f.dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},big.dat"),
        ],
    );
    git(&f.dir, &["commit", "-qm", "lfs"]);
    if payload_in_store {
        let p = f
            .dir
            .join(".git/lfs/objects")
            .join(&oid[..2])
            .join(&oid[2..4]);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join(&oid), &payload).unwrap();
    }
    if payload_in_worktree {
        std::fs::write(f.dir.join("big.dat"), &payload).unwrap();
    }
    (f, oid, payload)
}

#[test]
fn a14_an_lfs_pointer_is_replaced_by_its_payload() {
    let (f, oid, payload) = lfs_fixture(false, true);
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    assert_eq!(m.lfs.len(), 1);
    assert_eq!(
        (m.lfs[0].oid.as_str(), m.lfs[0].size),
        (oid.as_str(), payload.len() as u64)
    );
    let e = m
        .object_manifest
        .entries
        .iter()
        .find(|e| e.path.display() == "big.dat")
        .unwrap();
    assert_eq!(e.hash, oid);
    let dest = f.home.join("ws");
    materialize_source(&c, &m, &dest).unwrap();
    assert_eq!(std::fs::read(dest.join("big.dat")).unwrap(), payload);
    // the commit holds the pointer, as HEAD does: the tree is HEAD's
    let head = git_out(&f.dir, &["rev-parse", "HEAD"]);
    publishable(&c, &f.dir, &SourceInfo::of(&m), &head).unwrap();
}

#[test]
fn a14_a_pointer_without_a_payload_is_an_error_before_anything_runs() {
    let (f, oid, _) = lfs_fixture(false, false);
    let c = f.ctx();
    let err = capture_source(&c, &f.dir, &opts()).unwrap_err();
    match err.downcast_ref::<SourceError>() {
        Some(SourceError::LfsPayloadMissing { path, oid: o }) => {
            assert_eq!((path.as_str(), o.as_str()), ("big.dat", oid.as_str()))
        }
        other => panic!("{other:?}: {err:#}"),
    }
    assert!(format!("{err}").contains("LFS payload missing"));
    assert!(published(&f.home.join("sources")).is_empty());
}

#[test]
fn a14_an_already_smudged_lfs_file_is_taken_as_it_is() {
    let (f, oid, payload) = lfs_fixture(true, false);
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    assert_eq!(m.lfs[0].oid, oid);
    let dest = f.home.join("ws");
    materialize_source(&c, &m, &dest).unwrap();
    assert_eq!(std::fs::read(dest.join("big.dat")).unwrap(), payload);
}

#[test]
fn filters_run_once_by_git_and_never_again_on_captured_content() {
    let f = fixture();
    // clean: lower-case, smudge: upper-case
    git(&f.dir, &["config", "filter.fake.clean", "tr A-Z a-z"]);
    git(&f.dir, &["config", "filter.fake.smudge", "tr a-z A-Z"]);
    std::fs::write(f.dir.join(".gitattributes"), "*.fk filter=fake\n").unwrap();
    std::fs::write(f.dir.join("x.fk"), "hello\n").unwrap();
    git(&f.dir, &["add", "."]);
    git(&f.dir, &["commit", "-qm", "filtered"]);
    // the worktree file as a smudged checkout has it
    std::fs::write(f.dir.join("x.fk"), "HELLO\n").unwrap();
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let e = m
        .object_manifest
        .entries
        .iter()
        .find(|e| e.path.display() == "x.fk")
        .unwrap();
    use sha2::{Digest, Sha256};
    let want: String = Sha256::digest(b"HELLO\n")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(e.hash, want, "the manifest holds the materialized bytes");
    let dest = f.home.join("ws");
    materialize_source(&c, &m, &dest).unwrap();
    assert_eq!(std::fs::read(dest.join("x.fk")).unwrap(), b"HELLO\n");
    // the synthetic commit holds what Git would have stored (the filter's
    // clean output), not the bytes of the working file
    let exec = m.execution_commit.clone().unwrap();
    let private = std::fs::read_dir(f.home.join("sources/git"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        git_out(&private, &["show", &format!("{exec}:x.fk")]),
        "hello"
    );
    let head = git_out(&f.dir, &["rev-parse", "HEAD"]);
    assert_eq!(
        m.worktree_tree.as_deref().unwrap(),
        git_out(&f.dir, &["rev-parse", "HEAD^{tree}"])
    );
    publishable(&c, &f.dir, &SourceInfo::of(&m), &head).unwrap();
}

#[test]
fn a_directory_outside_git_is_captured_in_the_same_form() {
    let tmp = common::tempdir().unwrap();
    let home = tmp.path().join("home");
    let d = tmp.path().join("plain");
    std::fs::create_dir_all(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/x.txt"), "x\n").unwrap();
    std::fs::write(d.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(d.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("sub/x.txt", d.join("l")).unwrap();
    let c = ctx(&home);
    let snap = capture_directory(&c, &d, &opts()).unwrap();
    let m = &snap.manifest;
    assert_eq!(m.source_kind, SourceKind::Directory);
    assert!(m.execution_commit.is_none() && m.original_head.is_none());
    let dest = tmp.path().join("ws");
    materialize_source(&c, m, &dest).unwrap();
    assert_eq!(listing(&dest, &[]), listing(&d, &[]));
    assert!(export_objects(&c, m).is_err());
    // the same again
    assert_eq!(
        capture_directory(&c, &d, &opts())
            .unwrap()
            .manifest
            .source_id,
        m.source_id
    );
    // it notices movement too
    let f = d.join("sub/x.txt");
    let mut o = opts();
    o.hook = hook(move |s| {
        if let Stage::AfterRead(n) = s {
            std::fs::write(&f, format!("{n}\n")).unwrap();
        }
        Ok(())
    });
    let err = capture_directory(&c, &d, &o).unwrap_err();
    assert!(matches!(
        err.downcast_ref::<SourceError>(),
        Some(SourceError::Busy { .. })
    ));
}

#[test]
fn a_manifest_with_an_escaping_path_is_refused_at_materialize() {
    use local_ci::source::{FileEntry, RelPath};
    let f = fixture();
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let store = ObjectStore::open(f.home.join("sources")).unwrap();
    let (hash, size) = store.put_bytes(b"pwned\n").unwrap();
    for bad in [
        "../evil.txt",
        "/abs.txt",
        "a/../../evil.txt",
        ".git/hooks/pre-commit",
        "a//b",
        "",
    ] {
        let mut evil = m.clone();
        evil.object_manifest.entries.push(FileEntry {
            path: RelPath::new(bad),
            mode: 0o100755,
            size,
            hash: hash.clone(),
        });
        let dest = f.home.join("outer/ws");
        let err =
            local_ci::source::materialize_from(&store, &seal_manifest(evil), &dest).unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<SourceError>(),
                Some(SourceError::UnsafePath { .. })
            ),
            "{bad:?}: {err:#}"
        );
        assert!(!f.home.join("outer/evil.txt").exists());
        assert!(!f.home.join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(f.home.join("outer"));
    }
}

#[test]
fn a_symlink_in_the_manifest_cannot_be_written_through() {
    use local_ci::source::{FileEntry, RelPath};
    let f = fixture();
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let store = ObjectStore::open(f.home.join("sources")).unwrap();
    let outside = f.home.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let (lh, ls) = store
        .put_bytes(outside.to_str().unwrap().as_bytes())
        .unwrap();
    let (ch, cs) = store.put_bytes(b"pwned\n").unwrap();
    let mut evil = m.clone();
    evil.object_manifest.entries.push(FileEntry {
        path: RelPath::new("esc"),
        mode: 0o120000,
        size: ls,
        hash: lh,
    });
    evil.object_manifest.entries.push(FileEntry {
        path: RelPath::new("esc/x.txt"),
        mode: 0o100644,
        size: cs,
        hash: ch,
    });
    evil.object_manifest
        .entries
        .sort_by(|a, b| a.path.cmp(&b.path));
    let dest = f.home.join("ws");
    let err = local_ci::source::materialize_from(&store, &seal_manifest(evil), &dest).unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<SourceError>(),
            Some(SourceError::UnsafePath { .. })
        ),
        "{err:#}"
    );
    assert!(!outside.join("x.txt").exists());
}

#[test]
fn a_secret_file_bundle_cannot_place_a_file_outside_the_workspace() {
    use local_ci::source::{RelPath, SecretEntry};
    let f = fixture();
    std::fs::write(f.dir.join(".env"), "S=1\n").unwrap();
    let c = f.ctx();
    let mut o = opts();
    o.secret_files = vec![spec(".env", true)];
    o.secret_key = KEY.to_vec();
    let m = capture_source(&c, &f.dir, &o).unwrap().manifest;
    let evil_path = RelPath::new("../escaped.env");
    let mut evil_m = m.clone();
    evil_m.secret_files = vec![SecretEntry {
        hmac: secret_digest(KEY, &evil_path, b"S=1\n"),
        path: evil_path.clone(),
    }];
    let evil_bundle = SecretBundle::new(vec![(evil_path, 0o600, b"S=1\n".to_vec())]);
    let dest = f.home.join("outer/ws");
    std::fs::create_dir_all(&dest).unwrap();
    let err = materialize_secrets(&evil_m, &evil_bundle, KEY, &dest).unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<SourceError>(),
            Some(SourceError::UnsafePath { .. })
        ),
        "{err:#}"
    );
    assert!(!f.home.join("outer/escaped.env").exists());
}

// ---- what Git would store, and what a worker writes ------------------------

fn private_repo_of(f_home: &Path) -> PathBuf {
    std::fs::read_dir(f_home.join("sources/git"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}

const APPEND_SMUDGE: (&str, &str) = ("sed '/^#smudged$/d'", "cat; echo '#smudged'");

struct World {
    _tmp: common::Tmp,
    home: PathBuf,
    work: PathBuf,
    worker: PathBuf,
    out: PathBuf,
}

/// A repository whose `.txt` files are LF in Git and CRLF on disk, with a
/// filter that appends a line every time it smudges, a workflow that copies
/// files out, and a clone (the worker) that has `main` only.
fn world() -> World {
    let tmp = common::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    let worker = tmp.path().join("worker");
    let out = tmp.path().join("out");
    std::fs::create_dir_all(work.join(".github/workflows")).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "filter.appender.clean", APPEND_SMUDGE.0]);
    git(
        &work,
        &["config", "filter.appender.smudge", APPEND_SMUDGE.1],
    );
    std::fs::write(
        work.join(".gitattributes"),
        "*.txt text eol=crlf\n*.sm filter=appender\n",
    )
    .unwrap();
    std::fs::write(
        work.join(".github/workflows/ci.yml"),
        format!(
            "name: ci\non: pull_request\njobs:\n  check:\n    runs-on: ubuntu-latest\n    env:\n      OUT: {}\n    steps:\n      - run: sh copy.sh\n",
            out.display()
        ),
    )
    .unwrap();
    std::fs::write(
        work.join("copy.sh"),
        "for f in x.txt y.txt z.txt s.sm new.txt; do cp \"$f\" \"$OUT/$f.out\" 2>/dev/null; done; true\n",
    )
    .unwrap();
    std::fs::write(work.join("x.txt"), "one\ntwo\n").unwrap();
    std::fs::write(work.join("z.txt"), "z\n").unwrap();
    std::fs::write(work.join("s.sm"), "v\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "base"]);
    git(
        tmp.path(),
        &["clone", "-q", work.to_str().unwrap(), "worker"],
    );
    git(
        &worker,
        &["config", "filter.appender.clean", APPEND_SMUDGE.0],
    );
    git(
        &worker,
        &["config", "filter.appender.smudge", APPEND_SMUDGE.1],
    );
    git(&work, &["checkout", "-qb", "feature"]);
    std::fs::write(work.join("y.txt"), "y\n").unwrap();
    git(&work, &["add", "y.txt"]);
    git(&work, &["commit", "-qm", "feature"]);
    World {
        _tmp: tmp,
        home,
        work,
        worker,
        out,
    }
}

/// Capture `w.work`, take the snapshot to the worker and run its workflow
/// there over the captured bytes.
fn run_in_worker(
    c: &ExecContext,
    w: &World,
    m: &local_ci::source::SourceManifest,
) -> local_ci::report::RunResult {
    let bundle = export_objects(c, m).unwrap();
    let exec = import_objects(c, &w.worker, &bundle, m).unwrap();
    let o = local_ci::RunOptions {
        target: local_ci::Target {
            base: Some("main".into()),
            sha: exec,
            all: false,
            include_disabled: true,
            workflows_dirs: vec![],
            workflows: vec![],
            jobs: vec![],
            dir: w.worker.clone(),
        },
        parallel: Some(1),
        no_traffic_controller: true,
        no_portool: true,
        containers: 1,
        keep: false,
        job_timeout: 5.0,
        no_cache: true,
        cache_dir: w.home.clone(),
        source: Some(m.clone()),
    };
    let r = local_ci::run(c, o, &mut local_ci::Quiet).unwrap().result;
    assert_eq!(r.state, "success", "{}", r.description);
    r
}

fn crlf_checkout(w: &World) {
    std::fs::write(w.work.join("x.txt"), "one\r\ntwo\r\n").unwrap();
    std::fs::write(w.work.join("y.txt"), "y\r\n").unwrap();
    std::fs::write(w.work.join("z.txt"), "z\r\n").unwrap();
}

#[test]
fn a_clean_repository_with_converted_line_endings_is_head_and_posts_its_status() {
    let w = world();
    crlf_checkout(&w);
    let c = ctx(&w.home);
    let mut o = opts();
    o.base = Some("main".into());
    let m = capture_source(&c, &w.work, &o).unwrap().manifest;
    let head = git_out(&w.work, &["rev-parse", "HEAD"]);
    // CRLF on disk, LF in Git: the tree is HEAD's, so a status is allowed
    assert_eq!(
        m.worktree_tree.as_deref().unwrap(),
        git_out(&w.work, &["rev-parse", "HEAD^{tree}"])
    );
    publishable(&c, &w.work, &SourceInfo::of(&m), &head).unwrap();

    run_in_worker(&c, &w, &m);
    let exec = m.execution_commit.clone().unwrap();
    // a line-ending change is not a change: only the new file differs
    assert_eq!(
        git_out(&w.worker, &["diff", "--numstat", "main", &exec]),
        "1\t0\ty.txt"
    );
    // and what the worker had to work with is the bytes of the original
    assert_eq!(
        std::fs::read(w.out.join("x.txt.out")).unwrap(),
        b"one\r\ntwo\r\n"
    );

    // the status goes to the commit that was tested, and says so
    let bin = w.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = w.home.join("gh.log");
    std::fs::write(
        bin.join("gh"),
        format!("#!/bin/sh\necho \"$@\" > {}\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(bin.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let gc = ExecContext::new(
        [
            (
                "PATH",
                format!("{}:/usr/bin:/bin:/opt/homebrew/bin", bin.display()),
            ),
            ("HOME", w.home.display().to_string()),
            ("LOCAL_CI_HOME", w.home.display().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    );
    let (state, short) = local_ci::status(&gc, &w.worker, &head, &w.home).unwrap();
    assert_eq!(state, "success");
    assert_eq!(short, &head[..7]);
    assert_ne!(short, &exec[..7]);
    let posted = std::fs::read_to_string(&log).unwrap();
    assert!(posted.contains(&head), "{posted}");
    assert!(!posted.contains(&exec), "{posted}");
}

#[test]
fn the_workspace_of_a_run_holds_the_captured_bytes_and_no_conversion_runs_twice() {
    let w = world();
    // LF only in a file Git would make CRLF, an untracked one, a mixed one,
    // and a file behind a filter whose smudge adds a line every time
    std::fs::write(w.work.join("x.txt"), "LF only\nline2\n").unwrap();
    std::fs::write(w.work.join("new.txt"), "n1\nn2\r\n").unwrap();
    std::fs::write(w.work.join("z.txt"), "z1\nz2\r\n").unwrap();
    std::fs::write(w.work.join("s.sm"), "v\nedit\n").unwrap();
    let c = ctx(&w.home);
    let mut o = opts();
    o.base = Some("main".into());
    let m = capture_source(&c, &w.work, &o).unwrap().manifest;
    let exec = m.execution_commit.clone().unwrap();
    run_in_worker(&c, &w, &m);

    for (name, want) in [
        ("x.txt", "LF only\nline2\n"),
        ("new.txt", "n1\nn2\r\n"),
        ("z.txt", "z1\nz2\r\n"),
        ("s.sm", "v\nedit\n"),
    ] {
        assert_eq!(
            std::fs::read(w.out.join(format!("{name}.out"))).unwrap(),
            want.as_bytes(),
            "{name}"
        );
    }
    // Git holds the canonical form
    assert_eq!(
        git_out(&w.worker, &["show", &format!("{exec}:x.txt")]),
        "LF only\nline2"
    );
    assert_eq!(
        git_out(&w.worker, &["show", &format!("{exec}:s.sm")]),
        "v\nedit"
    );
    // the test would catch a checkout: it converts both, differently
    let naive = w._tmp.path().join("naive");
    git(
        &w.worker,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            naive.to_str().unwrap(),
            &exec,
        ],
    );
    assert_eq!(
        std::fs::read(naive.join("x.txt")).unwrap(),
        b"LF only\r\nline2\r\n"
    );
    assert_eq!(
        std::fs::read(naive.join("s.sm")).unwrap(),
        b"v\nedit\n#smudged\n"
    );
}

#[test]
fn a_workspace_materialized_from_the_manifest_is_byte_for_byte_the_original() {
    let w = world();
    std::fs::write(w.work.join("x.txt"), "LF only\r\nmixed\n").unwrap();
    std::fs::write(w.work.join("s.sm"), "v\nedit\n").unwrap();
    let c = ctx(&w.home);
    let m = capture_source(&c, &w.work, &opts()).unwrap().manifest;
    let dest = w.home.join("ws");
    materialize_source(&c, &m, &dest).unwrap();
    for f in ["x.txt", "s.sm", "z.txt", "y.txt"] {
        assert_eq!(
            std::fs::read(dest.join(f)).unwrap(),
            std::fs::read(w.work.join(f)).unwrap(),
            "{f}"
        );
    }
}

#[test]
fn an_lfs_file_is_a_pointer_in_the_commit_and_the_payload_never_enters_the_bundle() {
    let (f, oid, payload) = lfs_fixture(false, true);
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let exec = m.execution_commit.clone().unwrap();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}",
        payload.len()
    );
    let private = private_repo_of(&f.home);
    assert_eq!(
        git_out(&private, &["show", &format!("{exec}:big.dat")]),
        pointer
    );

    let bundle = export_objects(&c, &m).unwrap();
    let fresh = f.home.join("fresh");
    git(
        f.dir.parent().unwrap(),
        &[
            "clone",
            "-q",
            f.dir.to_str().unwrap(),
            fresh.to_str().unwrap(),
        ],
    );
    import_objects(&c, &fresh, &bundle, &m).unwrap();
    assert_eq!(
        git_out(&fresh, &["show", &format!("{exec}:big.dat")]),
        pointer
    );
    let pay = f.home.join("payload.bin");
    std::fs::write(&pay, &payload).unwrap();
    let payload_blob = git_out(
        &fresh,
        &["hash-object", "--no-filters", pay.to_str().unwrap()],
    );
    let has = Command::new("git")
        .args(["cat-file", "-e", &payload_blob])
        .current_dir(&fresh)
        .status()
        .unwrap()
        .success();
    assert!(!has, "the payload is in the bundle");

    // the payload is what a workspace gets
    let dest = f.home.join("ws");
    materialize_source(&c, &m, &dest).unwrap();
    assert_eq!(std::fs::read(dest.join("big.dat")).unwrap(), payload);
}

#[test]
fn a_secret_key_that_is_empty_or_short_is_refused() {
    let f = fixture();
    std::fs::write(f.dir.join(".env"), "S=1\n").unwrap();
    let c = f.ctx();
    for key in [&b""[..], b"short", b"fifteen-bytes!!"] {
        let mut o = opts();
        o.secret_files = vec![spec(".env", false)];
        o.secret_key = key.to_vec();
        let err = capture_source(&c, &f.dir, &o).unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<SourceError>(),
                Some(SourceError::SecretKeyTooShort { .. })
            ),
            "{err:#}"
        );
    }
    // no secret files declared: no key needed
    capture_source(&c, &f.dir, &opts()).unwrap();
}

#[test]
fn a_tracked_file_that_matches_secret_files_is_commit_content_and_warns() {
    let f = fixture();
    std::fs::write(f.dir.join("tracked.env"), "T=1\n").unwrap();
    git(&f.dir, &["add", "tracked.env"]);
    git(&f.dir, &["commit", "-qm", "env"]);
    std::fs::write(f.dir.join(".env"), "S=1\n").unwrap();
    let c = f.ctx();
    let mut o = opts();
    o.secret_key = KEY.to_vec();
    o.secret_files = vec![spec("*.env", true)];
    let snap = capture_source(&c, &f.dir, &o).unwrap();
    let m = &snap.manifest;
    assert_eq!(
        m.secret_files
            .iter()
            .map(|s| s.path.display())
            .collect::<Vec<_>>(),
        vec![".env".to_string()]
    );
    assert!(m
        .object_manifest
        .entries
        .iter()
        .any(|e| e.path.display() == "tracked.env"));
    assert!(
        snap.warnings
            .iter()
            .any(|w| w.contains("tracked.env") && w.contains("tracked")),
        "{:?}",
        snap.warnings
    );
    // it is in the tree, so the tree is still HEAD's
    let head = git_out(&f.dir, &["rev-parse", "HEAD"]);
    publishable(&c, &f.dir, &SourceInfo::of(m), &head).unwrap();
}

fn clone_of(f: &Fixture, name: &str) -> PathBuf {
    let fresh = f.home.join(name);
    git(
        f.dir.parent().unwrap(),
        &[
            "clone",
            "-q",
            f.dir.to_str().unwrap(),
            fresh.to_str().unwrap(),
        ],
    );
    fresh
}

fn has(dir: &Path, sha: &str) -> bool {
    Command::new("git")
        .args(["cat-file", "-e", sha])
        .current_dir(dir)
        .status()
        .unwrap()
        .success()
}

#[test]
fn the_index_tree_reaches_the_worker_so_that_what_was_staged_can_be_told() {
    let f = fixture();
    dirty(&f);
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let itree = m.index_tree.clone().unwrap();
    let head = m.original_head.clone().unwrap();
    let fresh = clone_of(&f, "fresh");
    assert!(!has(&fresh, &format!("{itree}^{{tree}}")));

    let bundle = export_objects(&c, &m).unwrap();
    import_objects(&c, &fresh, &bundle, &m).unwrap();
    assert!(has(&fresh, &format!("{itree}^{{tree}}")));
    // `git diff --cached` of the original, from the worker
    let staged = git_out(&fresh, &["diff", "--name-only", &head, &itree]);
    assert_eq!(staged, "staged.txt\nstaged_then_changed.txt");
    let want = git_out(&f.dir, &["diff", "--cached", "--name-only"]);
    assert_eq!(staged, want);
    // a commit of its own, with no parent, under a ref of its own
    let r = format!("refs/local-ci/index/{}", m.source_id);
    assert_eq!(
        git_out(&fresh, &["rev-parse", &format!("{r}^{{tree}}")]),
        itree
    );
    assert_eq!(git_out(&fresh, &["rev-list", "--count", &r]), "1");
    assert!(!git_out(&f.dir, &["for-each-ref"]).contains("local-ci"));

    // exporting again, also from several threads at once, gives a bundle that
    // imports the same way
    let results: Vec<_> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..4).map(|_| s.spawn(|| export_objects(&c, &m))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let again = clone_of(&f, "again");
    for r in results {
        import_objects(&c, &again, &r.unwrap(), &m).unwrap();
    }
    assert!(has(&again, &format!("{itree}^{{tree}}")));
}

#[test]
fn a_bundle_without_the_index_tree_is_not_taken_for_the_capture() {
    let f = fixture();
    dirty(&f);
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let private = private_repo_of(&f.home);
    // only the snapshot, as a bundle made without the index commit has it
    let old = f.home.join("old.bundle");
    git(
        &private,
        &[
            "bundle",
            "create",
            "-q",
            old.to_str().unwrap(),
            &format!("refs/local-ci/snapshots/{}", m.source_id),
        ],
    );
    let fresh = clone_of(&f, "fresh");
    let err = import_objects(&c, &fresh, &old, &m).unwrap_err();
    assert!(format!("{err:#}").contains("index"), "{err:#}");

    // the manifest names a tree that is not the one the bundle carries
    let bundle = export_objects(&c, &m).unwrap();
    let mut other = m.clone();
    other.index_tree = other.worktree_tree.clone();
    let other = seal_manifest(other);
    let err = import_objects(&c, &fresh, &bundle, &other).unwrap_err();
    assert!(format!("{err}").contains("index tree"), "{err}");
    // an index tree that is not in the store cannot be exported
    let mut gone = m.clone();
    gone.index_tree = Some("0123456789012345678901234567890123456789".into());
    let err = export_objects(&c, &gone).unwrap_err();
    assert!(format!("{err:#}").contains("index tree"), "{err:#}");
    // and one that is not an id at all is refused before git sees it
    let mut odd = m.clone();
    odd.index_tree = Some("--output=x".into());
    let odd = seal_manifest(odd);
    assert!(format!("{}", export_objects(&c, &odd).unwrap_err()).contains("not a tree id"));
    assert!(
        format!("{}", import_objects(&c, &fresh, &bundle, &odd).unwrap_err())
            .contains("not a tree id")
    );
}

#[test]
fn the_capture_keeps_the_index_tree_under_a_ref_so_that_a_prune_cannot_take_it() {
    let f = fixture();
    dirty(&f);
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let itree = m.index_tree.clone().unwrap();
    let private = private_repo_of(&f.home);
    let r = format!("refs/local-ci/index/{}", m.source_id);
    assert_eq!(
        git_out(&private, &["rev-parse", &format!("{r}^{{tree}}")]),
        itree
    );
    assert_eq!(git_out(&private, &["rev-list", "--count", &r]), "1");
    git(&private, &["reflog", "expire", "--expire=now", "--all"]);
    git(&private, &["gc", "-q", "--prune=now"]);
    assert!(has(&private, &format!("{itree}^{{tree}}")));
    export_objects(&c, &m).unwrap();

    // a source captured by a build that kept no such ref, and pruned since,
    // cannot be exported: its index tree is gone
    git(&private, &["update-ref", "-d", &r]);
    git(&private, &["reflog", "expire", "--expire=now", "--all"]);
    git(&private, &["gc", "-q", "--prune=now"]);
    assert!(!has(&private, &format!("{itree}^{{tree}}")));
    let err = export_objects(&c, &m).unwrap_err();
    assert!(format!("{err:#}").contains("index tree"), "{err:#}");
}

#[test]
fn an_export_that_changes_nothing_does_not_write_the_ref_and_a_locked_one_is_an_error() {
    let f = fixture();
    dirty(&f);
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let private = private_repo_of(&f.home);
    let r = format!("refs/local-ci/index/{}", m.source_id);
    // a ref somebody else is writing right now: the same value is not written
    let lock = private.join(format!("{r}.lock"));
    std::fs::write(&lock, "").unwrap();
    export_objects(&c, &m).unwrap();
    // another value would have to be written, and cannot be
    std::fs::remove_file(&lock).unwrap();
    git(&private, &["update-ref", "-d", &r]);
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, "").unwrap();
    let err = export_objects(&c, &m).unwrap_err();
    assert!(format!("{err:#}").contains("lock"), "{err:#}");
    // and once the lock is gone, it is written
    std::fs::remove_file(&lock).unwrap();
    export_objects(&c, &m).unwrap();
    assert_eq!(
        git_out(&private, &["rev-parse", &format!("{r}^{{tree}}")]),
        m.index_tree.clone().unwrap()
    );
}

#[test]
fn only_the_refs_of_the_source_are_taken_from_a_bundle_and_the_index_commit_has_no_history() {
    let f = fixture();
    dirty(&f);
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let private = private_repo_of(&f.home);
    let snap = format!("refs/local-ci/snapshots/{}", m.source_id);
    let idx = format!("refs/local-ci/index/{}", m.source_id);
    let commit = m.execution_commit.clone().unwrap();
    let itree = m.index_tree.clone().unwrap();
    let head = m.original_head.clone().unwrap();
    // a bundle that carries more refs than the source's own
    let other = "src-00000000000000000000000000000000";
    git(
        &private,
        &[
            "update-ref",
            &format!("refs/local-ci/snapshots/{other}"),
            &commit,
        ],
    );
    git(
        &private,
        &["update-ref", "refs/local-ci/index/other", &commit],
    );
    git(&private, &["update-ref", "refs/heads/loose", &commit]);
    let wide = f.home.join("wide.bundle");
    git(
        &private,
        &[
            "bundle",
            "create",
            "-q",
            wide.to_str().unwrap(),
            &snap,
            &idx,
            &format!("refs/local-ci/snapshots/{other}"),
            "refs/local-ci/index/other",
            "refs/heads/loose",
        ],
    );
    let fresh = clone_of(&f, "fresh");
    import_objects(&c, &fresh, &wide, &m).unwrap();
    let refs = git_out(
        &fresh,
        &["for-each-ref", "--format=%(refname)", "refs/local-ci"],
    );
    assert_eq!(refs, format!("{idx}\n{snap}"), "{refs}");
    assert!(!git_out(&fresh, &["for-each-ref", "refs/heads"]).contains("loose"));

    // an index commit with a history is not the commit around the tree
    let with_parent = git_out(&private, &["commit-tree", &itree, "-p", &head, "-m", "x"]);
    git(&private, &["update-ref", &idx, &with_parent]);
    let long = f.home.join("long.bundle");
    git(
        &private,
        &[
            "bundle",
            "create",
            "-q",
            long.to_str().unwrap(),
            &snap,
            &idx,
        ],
    );
    let fresh = clone_of(&f, "fresh2");
    let err = import_objects(&c, &fresh, &long, &m).unwrap_err();
    assert!(format!("{err}").contains("has a history"), "{err}");
}

#[test]
fn the_refs_an_import_made_can_be_removed_by_source_id() {
    let f = fixture();
    dirty(&f);
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let fresh = clone_of(&f, "fresh");
    import_objects(&c, &fresh, &export_objects(&c, &m).unwrap(), &m).unwrap();
    let local = || {
        git_out(
            &fresh,
            &["for-each-ref", "--format=%(refname)", "refs/local-ci"],
        )
    };
    assert!(local().contains(&m.source_id));
    remove_imported_refs(&c, &fresh, &m.source_id).unwrap();
    assert_eq!(local(), "");
    // nothing to remove is not an error; an id that is none is refused
    remove_imported_refs(&c, &fresh, &m.source_id).unwrap();
    for odd in [
        "",
        "src-x",
        "../heads/main",
        "src-0000000000000000000000000000000G",
    ] {
        assert!(remove_imported_refs(&c, &fresh, odd).is_err(), "{odd}");
    }
}

#[test]
fn a_manifest_that_is_not_verified_or_names_odd_ids_is_not_imported() {
    let f = fixture();
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let bundle = export_objects(&c, &m).unwrap();
    let fresh = f.home.join("fresh");
    git(
        f.dir.parent().unwrap(),
        &[
            "clone",
            "-q",
            f.dir.to_str().unwrap(),
            fresh.to_str().unwrap(),
        ],
    );

    let mut edited = m.clone();
    edited.branch = Some("evil".into());
    let err = import_objects(&c, &fresh, &bundle, &edited).unwrap_err();
    assert!(format!("{err}").contains("digest"), "{err}");

    let mut odd = m.clone();
    odd.execution_commit = Some("--upload-pack=sh".into());
    let odd = seal_manifest(odd);
    let err = import_objects(&c, &fresh, &bundle, &odd).unwrap_err();
    assert!(format!("{err}").contains("not a commit id"), "{err}");
    assert!(export_objects(&c, &odd).is_err());

    let mut odd = m.clone();
    odd.source_id = "src-../../x".into();
    assert!(import_objects(&c, &fresh, &bundle, &odd).is_err());
    import_objects(&c, &fresh, &bundle, &m).unwrap();
}

#[test]
fn the_store_keeps_secrets_and_scratch_files_to_its_owner_and_a_dot_git_file_is_refused() {
    let f = fixture();
    let c = f.ctx();
    capture_source(&c, &f.dir, &opts()).unwrap();
    for d in ["secrets", "tmp"] {
        let mode = std::fs::metadata(f.home.join("sources").join(d))
            .unwrap()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{d}");
    }
    let plain = f.home.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("a"), "a").unwrap();
    std::fs::write(plain.join(".git"), "gitdir: /elsewhere\n").unwrap();
    let err = capture_directory(&c, &plain, &opts()).unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<SourceError>(),
            Some(SourceError::UnsafePath { .. })
        ),
        "{err:#}"
    );
}

#[test]
fn the_snapshot_survives_a_rewrite_and_a_gc_of_the_original() {
    let f = fixture();
    dirty(&f);
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let head = m.original_head.clone().unwrap();
    let fresh = f.home.join("fresh");
    git(
        f.dir.parent().unwrap(),
        &[
            "clone",
            "-q",
            f.dir.to_str().unwrap(),
            fresh.to_str().unwrap(),
        ],
    );
    git(
        &f.dir,
        &["commit", "--amend", "-qm", "rewritten", "--allow-empty"],
    );
    git(&f.dir, &["reflog", "expire", "--expire=now", "--all"]);
    git(&f.dir, &["gc", "-q", "--prune=now"]);
    let gone = Command::new("git")
        .args(["cat-file", "-e", &head])
        .current_dir(&f.dir)
        .status()
        .unwrap()
        .success();
    assert!(!gone, "the old HEAD should be gone from the original");

    let bundle = export_objects(&c, &m).unwrap();
    let exec = import_objects(&c, &fresh, &bundle, &m).unwrap();
    git(&fresh, &["checkout", "-q", "--detach", &exec]);
    assert_eq!(
        std::fs::read(fresh.join("untracked.txt")).unwrap(),
        b"new\n"
    );
    git(&fresh, &["cat-file", "-e", &head]);
}

#[test]
fn a_dirty_submodule_comes_along_in_the_bundle_and_a_worker_can_build_the_workspace() {
    let f = with_submodule();
    let c = f.ctx();
    std::fs::write(f.dir.join("vendor/lib/lib.txt"), "lib, edited\n").unwrap();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let sub = m.submodules[0].clone();
    assert!(sub.dirty);
    let sub_m = ObjectStore::open(f.home.join("sources"))
        .unwrap()
        .load_manifest(sub.source_id.as_ref().unwrap())
        .unwrap()
        .unwrap();

    let tmp = f.dir.parent().unwrap().to_path_buf();
    git(&tmp, &["clone", "-q", f.dir.to_str().unwrap(), "worker"]);
    let worker = tmp.join("worker");
    let bundle = export_objects(&c, &m).unwrap();
    let exec = import_objects(&c, &worker, &bundle, &m).unwrap();
    // the gitlink names a commit of the edited submodule, and it is here
    let line = git_out(&worker, &["ls-tree", &exec, "vendor/lib"]);
    let gitlink = line.split_whitespace().nth(2).unwrap().to_string();
    assert_ne!(gitlink, sub.revision);
    assert_eq!(Some(gitlink.clone()), sub_m.execution_commit);
    git(
        &worker,
        &["cat-file", "-e", &format!("{gitlink}^{{commit}}")],
    );
    assert_eq!(
        git_out(&worker, &["rev-parse", &format!("{gitlink}^{{tree}}")]),
        sub_m.worktree_tree.clone().unwrap()
    );

    let wt = f.home.join("wt");
    local_ci::source::add_worktree(
        &c,
        &ObjectStore::open(f.home.join("sources")).unwrap(),
        &worker,
        &wt,
        &m,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(wt.join("vendor/lib/lib.txt")).unwrap(),
        b"lib, edited\n"
    );
    assert_eq!(std::fs::read(wt.join("a.txt")).unwrap(), b"a\n");

    // without the submodule's manifest the bundle would be incomplete: refuse
    let mf = f
        .home
        .join("sources/manifests")
        .join(format!("{}.json", sub.source_id.as_ref().unwrap()));
    std::fs::remove_file(mf).unwrap();
    assert!(export_objects(&c, &m).is_err());
}

#[test]
fn a_change_inside_a_submodule_while_the_superproject_is_read_is_noticed() {
    let f = with_submodule();
    let c = f.ctx();
    let file = f.dir.join("vendor/lib/lib.txt");
    let calls = Arc::new(AtomicU32::new(0));
    let n = calls.clone();
    let mut o = opts();
    o.hook = hook(move |s| {
        if let Stage::AfterRead(attempt) = s {
            n.fetch_add(1, Ordering::SeqCst);
            if attempt == 1 {
                std::fs::write(&file, "changed in the submodule\n").unwrap();
            }
        }
        Ok(())
    });
    let m = capture_source(&c, &f.dir, &o).unwrap().manifest;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(m.submodules[0].dirty);
    let dest = f.home.join("ws");
    materialize_source(&c, &m, &dest).unwrap();
    assert_eq!(
        std::fs::read(dest.join("vendor/lib/lib.txt")).unwrap(),
        b"changed in the submodule\n"
    );

    // and a submodule that never settles is SOURCE_BUSY
    let f = with_submodule();
    let file = f.dir.join("vendor/lib/lib.txt");
    let err = busy_with(&f, move |n| {
        std::fs::write(&file, format!("v{n}\n")).unwrap()
    });
    assert!(
        matches!(
            err.downcast_ref::<SourceError>(),
            Some(SourceError::Busy { .. })
        ),
        "{err:#}"
    );
}

// ---- a worker that has none of the original's settings ----------------------

/// A repository (`work`, set up by the caller and committed) and a clone of
/// it (`worker`) that has none of its configuration, attributes or filters.
struct Pair {
    _tmp: common::Tmp,
    home: PathBuf,
    work: PathBuf,
    worker: PathBuf,
}

fn pair(setup: impl FnOnce(&Path)) -> Pair {
    let tmp = common::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    setup(&work);
    git(
        tmp.path(),
        &["clone", "-q", work.to_str().unwrap(), "worker"],
    );
    Pair {
        worker: tmp.path().join("worker"),
        _tmp: tmp,
        home,
        work,
    }
}

fn config_of(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Take the snapshot to the worker and make a workspace of it there, with the
/// worker's own environment.
fn worker_workspace(
    c: &ExecContext,
    p: &Pair,
    m: &local_ci::source::SourceManifest,
) -> anyhow::Result<PathBuf> {
    let bundle = export_objects(c, m)?;
    import_objects(&ctx(&p.home), &p.worker, &bundle, m)?;
    let wt = p.home.join("wt");
    local_ci::source::add_worktree(
        &ctx(&p.home),
        &ObjectStore::open(p.home.join("sources")).unwrap(),
        &p.worker,
        &wt,
        m,
    )?;
    Ok(wt)
}

fn crlf_original(w: &Path) {
    git(w, &["config", "core.autocrlf", "true"]);
    std::fs::write(w.join("a.txt"), "a\r\n").unwrap();
    git(w, &["add", "."]);
    git(w, &["commit", "-qm", "base"]);
}

fn dirty_crlf(w: &Path) {
    std::fs::write(w.join("a.txt"), "b\r\nc\r\n").unwrap();
    std::fs::write(w.join("new.txt"), "n\r\n").unwrap();
}

#[test]
fn a_worker_without_the_originals_autocrlf_sees_a_clean_status_and_keeps_its_own_config() {
    let p = pair(crlf_original);
    dirty_crlf(&p.work);
    let c = ctx(&p.home);
    let m = capture_source(&c, &p.work, &opts()).unwrap().manifest;
    assert_eq!(m.canonical.autocrlf.as_deref(), Some("true"));
    let wt = worker_workspace(&c, &p, &m).unwrap();
    assert_eq!(git_out(&wt, &["status", "--porcelain"]), "");
    git(&wt, &["diff", "--exit-code"]);
    assert_eq!(std::fs::read(wt.join("a.txt")).unwrap(), b"b\r\nc\r\n");
    assert_eq!(std::fs::read(wt.join("new.txt")).unwrap(), b"n\r\n");
    // the settings are the worktree's, not the clone's or its main checkout's
    assert_eq!(
        config_of(&wt, &["config", "--get", "core.autocrlf"]).as_deref(),
        Some("true")
    );
    assert_eq!(
        config_of(&p.worker, &["config", "--get", "core.autocrlf"]),
        None
    );
    assert_eq!(
        config_of(&p.worker, &["config", "--local", "--get", "core.eol"]),
        None
    );
}

#[test]
fn a_worker_without_the_originals_info_attributes_sees_a_clean_status() {
    let p = pair(|w| {
        std::fs::write(w.join(".git/info/attributes"), "*.txt text eol=crlf\n").unwrap();
        std::fs::write(w.join("a.txt"), "a\n").unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-qm", "base"]);
    });
    std::fs::write(p.work.join("a.txt"), "x\r\ny\r\n").unwrap();
    std::fs::write(p.work.join("n.txt"), "n\r\n").unwrap();
    let c = ctx(&p.home);
    let m = capture_source(&c, &p.work, &opts()).unwrap().manifest;
    assert!(!m.canonical.attributes.is_empty());
    let wt = worker_workspace(&c, &p, &m).unwrap();
    assert_eq!(git_out(&wt, &["status", "--porcelain"]), "");
    assert_eq!(std::fs::read(wt.join("a.txt")).unwrap(), b"x\r\ny\r\n");
    // other worktrees of the worker do not get the rules
    assert!(!p.worker.join(".git/info/attributes").exists());
    assert_eq!(
        config_of(
            &p.worker,
            &["config", "--local", "--get", "core.attributesfile"]
        ),
        None
    );
}

#[test]
fn a_filter_the_worker_does_not_define_is_reproduced_from_the_manifest() {
    // a clean that is not idempotent: running it twice is not running it once
    let p = pair(|w| {
        git(w, &["config", "filter.pre.clean", "sed 's/^/>/'"]);
        git(w, &["config", "filter.pre.smudge", "sed 's/^>//'"]);
        std::fs::write(w.join(".gitattributes"), "*.pf filter=pre\n").unwrap();
        std::fs::write(w.join("x.pf"), "x\n").unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-qm", "base"]);
    });
    std::fs::write(p.work.join("x.pf"), "y\n").unwrap();
    std::fs::write(p.work.join("n.pf"), "n\n").unwrap();
    let c = ctx(&p.home);
    let m = capture_source(&c, &p.work, &opts()).unwrap().manifest;
    assert_eq!(m.canonical.filters.len(), 1);
    assert_eq!(m.canonical.filters[0].name, "pre");
    let exec = m.execution_commit.clone().unwrap();
    assert_eq!(
        git_out(
            &private_repo_of(&p.home),
            &["show", &format!("{exec}:x.pf")]
        ),
        ">y"
    );
    assert_eq!(
        config_of(&p.worker, &["config", "--get", "filter.pre.clean"]),
        None
    );
    let wt = worker_workspace(&c, &p, &m).unwrap();
    assert_eq!(git_out(&wt, &["status", "--porcelain"]), "");
    assert_eq!(std::fs::read(wt.join("x.pf")).unwrap(), b"y\n");
    assert_eq!(std::fs::read(wt.join("n.pf")).unwrap(), b"n\n");
    assert_eq!(
        config_of(&p.worker, &["config", "--get", "filter.pre.clean"]),
        None
    );
}

#[test]
fn a_worker_without_the_program_of_a_filter_is_refused_with_a_typed_error() {
    let (f, _oid, _payload) = lfs_fixture(true, true);
    let tool = f.home.join("bin");
    std::fs::create_dir_all(&tool).unwrap();
    std::fs::write(
        tool.join("git-lfs"),
        "#!/bin/sh\ncat >/dev/null\nprintf 'pointer\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(tool.join("git-lfs"), std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &f.dir,
        &["config", "filter.lfs.clean", "git-lfs clean -- %f"],
    );
    git(
        &f.dir,
        &["config", "filter.lfs.smudge", "git-lfs smudge -- %f"],
    );
    let tmp = f.dir.parent().unwrap().to_path_buf();
    git(&tmp, &["clone", "-q", f.dir.to_str().unwrap(), "worker"]);
    let worker = tmp.join("worker");
    // the original has the program, the worker does not
    let with_tool = ExecContext::new(
        [
            ("PATH", format!("{}:/usr/bin:/bin", tool.display())),
            ("HOME", f.home.display().to_string()),
            ("LOCAL_CI_HOME", f.home.display().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    );
    let m = capture_source(&with_tool, &f.dir, &opts())
        .unwrap()
        .manifest;
    assert_eq!(m.canonical.filters.len(), 1);
    assert_eq!(m.canonical.filters[0].name, "lfs");
    // A PATH of its own with only `sh`: the system directories cannot be
    // used, a Linux image may have `git-lfs` in /usr/bin.
    let bare = f.home.join("bare-bin");
    std::fs::create_dir_all(&bare).unwrap();
    std::os::unix::fs::symlink("/bin/sh", bare.join("sh")).unwrap();
    let bare_path = ExecContext::new(
        [
            ("PATH", bare.display().to_string()),
            ("HOME", f.home.display().to_string()),
            ("LOCAL_CI_HOME", f.home.display().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    );
    let wt = f.home.join("wt");
    let e = local_ci::source::add_worktree(
        &bare_path,
        &ObjectStore::open(f.home.join("sources")).unwrap(),
        &worker,
        &wt,
        &m,
    )
    .unwrap_err();
    match e.downcast_ref::<SourceError>() {
        Some(SourceError::FilterMissing { filter, command }) => {
            assert_eq!((filter.as_str(), command.as_str()), ("lfs", "git-lfs"));
        }
        other => panic!("{e:#} ({other:?})"),
    }
    // nothing was made
    assert!(!wt.exists());
    assert_eq!(git_out(&worker, &["worktree", "list"]).lines().count(), 1);
}

#[test]
fn a_workspace_that_git_still_sees_as_changed_is_an_error_naming_the_paths() {
    let p = pair(crlf_original);
    dirty_crlf(&p.work);
    let c = ctx(&p.home);
    let real = capture_source(&c, &p.work, &opts()).unwrap().manifest;
    let bundle = export_objects(&c, &real).unwrap();
    import_objects(&ctx(&p.home), &p.worker, &bundle, &real).unwrap();
    // the settings that made the blobs are not passed on
    let mut m = real.clone();
    m.canonical = Default::default();
    let m = seal_manifest(m);
    let e = local_ci::source::add_worktree(
        &ctx(&p.home),
        &ObjectStore::open(p.home.join("sources")).unwrap(),
        &p.worker,
        &p.home.join("wt"),
        &m,
    )
    .unwrap_err();
    match e.downcast_ref::<SourceError>() {
        Some(SourceError::WorkspaceNotClean { paths }) => {
            assert!(paths.contains(&"a.txt".to_string()), "{paths:?}");
        }
        other => panic!("{e:#} ({other:?})"),
    }
}

#[test]
fn a_shallow_clone_is_refused_before_anything_is_written() {
    let f = fixture();
    std::fs::write(f.dir.join("a.txt"), "a2\n").unwrap();
    git(&f.dir, &["commit", "-qam", "second"]);
    let tmp = f.dir.parent().unwrap().to_path_buf();
    git(
        &tmp,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", f.dir.display()),
            "shallow",
        ],
    );
    let e = capture_source(&f.ctx(), &tmp.join("shallow"), &opts()).unwrap_err();
    assert_eq!(
        e.downcast_ref::<SourceError>(),
        Some(&SourceError::ShallowClone),
        "{e:#}"
    );
    let repos = std::fs::read_dir(f.home.join("sources/git"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(repos, 0);
}

#[test]
fn a_sha256_repository_is_refused_with_an_explicit_error() {
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    let init = Command::new("git")
        .args(["init", "-q", "--object-format=sha256", "-b", "main"])
        .current_dir(&dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    if !init.status.success() {
        return; // this git has no SHA-256 repositories
    }
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    let home = tmp.path().join("home");
    let e = capture_source(&ctx(&home), &dir, &opts()).unwrap_err();
    assert_eq!(
        e.downcast_ref::<SourceError>(),
        Some(&SourceError::UnsupportedObjectFormat {
            format: "sha256".into()
        }),
        "{e:#}"
    );
}

#[test]
fn skip_worktree_and_assume_unchanged_files_are_taken_as_they_are_on_disk() {
    let f = fixture();
    git(&f.dir, &["update-index", "--skip-worktree", "staged.txt"]);
    git(
        &f.dir,
        &["update-index", "--assume-unchanged", "unstaged.txt"],
    );
    std::fs::write(f.dir.join("staged.txt"), "skip, edited\n").unwrap();
    std::fs::write(f.dir.join("unstaged.txt"), "assume, edited\n").unwrap();
    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    let exec = m.execution_commit.clone().unwrap();
    let private = private_repo_of(&f.home);
    assert_eq!(
        git_out(&private, &["show", &format!("{exec}:staged.txt")]),
        "skip, edited"
    );
    assert_eq!(
        git_out(&private, &["show", &format!("{exec}:unstaged.txt")]),
        "assume, edited"
    );
}

#[test]
fn a_dirty_submodule_inside_a_dirty_submodule_comes_along() {
    let f = with_submodule();
    let tmp = f.dir.parent().unwrap().to_path_buf();
    // the library gets a submodule of its own
    let inner = tmp.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "-q", "-b", "main"]);
    std::fs::write(inner.join("i.txt"), "inner\n").unwrap();
    git(&inner, &["add", "."]);
    git(&inner, &["commit", "-qm", "inner"]);
    let lib = f.dir.join("vendor/lib");
    git(
        &lib,
        &[
            "submodule",
            "add",
            "-q",
            inner.to_str().unwrap(),
            "deep/inner",
        ],
    );
    git(&lib, &["commit", "-qm", "add inner"]);
    git(&f.dir, &["add", "vendor/lib"]);
    git(&f.dir, &["commit", "-qm", "bump lib"]);
    std::fs::write(lib.join("deep/inner/i.txt"), "inner, edited\n").unwrap();

    let c = f.ctx();
    let m = capture_source(&c, &f.dir, &opts()).unwrap().manifest;
    assert!(m.submodules[0].dirty);
    git(&tmp, &["clone", "-q", f.dir.to_str().unwrap(), "worker"]);
    let worker = tmp.join("worker");
    let bundle = export_objects(&c, &m).unwrap();
    import_objects(&c, &worker, &bundle, &m).unwrap();
    let wt = f.home.join("wt");
    local_ci::source::add_worktree(
        &c,
        &ObjectStore::open(f.home.join("sources")).unwrap(),
        &worker,
        &wt,
        &m,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(wt.join("vendor/lib/deep/inner/i.txt")).unwrap(),
        b"inner, edited\n"
    );
}

// ---- a run in the original itself never writes its config -------------------

const LOCAL_WORKFLOW: &str = "name: ci\non: pull_request\njobs:\n  check:\n    runs-on: ubuntu-latest\n    steps:\n      - run: test -f a.txt\n";

/// A repository with a workflow and a dirty `a.txt`, and the options of a run
/// in it over `m`.
fn local_pair(autocrlf: bool) -> Pair {
    let p = pair(|w| {
        std::fs::create_dir_all(w.join(".github/workflows")).unwrap();
        std::fs::write(w.join(".github/workflows/ci.yml"), LOCAL_WORKFLOW).unwrap();
        if autocrlf {
            git(w, &["config", "core.autocrlf", "true"]);
        }
        std::fs::write(w.join("a.txt"), "a\r\n").unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-qm", "base"]);
    });
    dirty_crlf(&p.work);
    p
}

fn run_options(p: &Pair, m: &local_ci::source::SourceManifest) -> local_ci::RunOptions {
    local_ci::RunOptions {
        target: local_ci::Target {
            base: Some("main".into()),
            sha: m.execution_commit.clone().unwrap(),
            all: false,
            include_disabled: true,
            workflows_dirs: vec![],
            workflows: vec![],
            jobs: vec![],
            dir: p.work.clone(),
        },
        parallel: Some(1),
        no_traffic_controller: true,
        no_portool: true,
        containers: 1,
        keep: false,
        job_timeout: 5.0,
        no_cache: true,
        cache_dir: p.home.clone(),
        source: Some(m.clone()),
    }
}

fn config_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join(".git/config")).unwrap()
}

#[test]
fn a_local_run_leaves_the_config_of_the_original_as_it_was() {
    for autocrlf in [false, true] {
        let p = local_pair(autocrlf);
        let c = ctx(&p.home);
        let m = capture_source(&c, &p.work, &opts()).unwrap().manifest;
        let before = config_bytes(&p.work);
        // the commit of the capture has to be in the repository the run is in
        import_objects(&c, &p.work, &export_objects(&c, &m).unwrap(), &m).unwrap();
        let r = local_ci::run(&c, run_options(&p, &m), &mut local_ci::Quiet)
            .unwrap()
            .result;
        assert_eq!(r.state, "success", "{}", r.description);
        assert_eq!(config_bytes(&p.work), before, "autocrlf={autocrlf}");
        assert_eq!(
            config_of(
                &p.work,
                &["config", "--local", "--get", "extensions.worktreeconfig"]
            ),
            None
        );
    }
}

#[test]
fn a_changed_setting_of_the_original_stops_a_local_run_without_writing_to_it() {
    let p = local_pair(true);
    let c = ctx(&p.home);
    let m = capture_source(&c, &p.work, &opts()).unwrap().manifest;
    import_objects(&c, &p.work, &export_objects(&c, &m).unwrap(), &m).unwrap();
    git(&p.work, &["config", "core.autocrlf", "false"]);
    let before = config_bytes(&p.work);
    let e = match local_ci::run(&c, run_options(&p, &m), &mut local_ci::Quiet) {
        Ok(r) => panic!("ran: {}", r.result.description),
        Err(e) => e,
    };
    match e.downcast_ref::<SourceError>() {
        Some(SourceError::OriginalConfigChanged { keys }) => {
            assert_eq!(keys, &vec!["core.autocrlf".to_string()]);
        }
        other => panic!("{e:#} ({other:?})"),
    }
    assert_eq!(config_bytes(&p.work), before);
}

#[test]
fn a_worker_clone_is_written_to_in_its_worktree_config_and_the_switch() {
    let p = pair(crlf_original);
    dirty_crlf(&p.work);
    let c = ctx(&p.home);
    let m = capture_source(&c, &p.work, &opts()).unwrap().manifest;
    let wt = worker_workspace(&c, &p, &m).unwrap();
    assert_eq!(git_out(&wt, &["status", "--porcelain"]), "");
    // the clone is not a capture source, so the switch is the one write
    assert_eq!(
        config_of(
            &p.worker,
            &[
                "config",
                "--local",
                "--bool",
                "--get",
                "extensions.worktreeconfig"
            ]
        )
        .as_deref(),
        Some("true")
    );
    // and the original was only read
    assert_eq!(
        config_of(
            &p.work,
            &["config", "--local", "--get", "extensions.worktreeconfig"]
        ),
        None
    );
}

#[test]
fn info_attributes_that_disagree_with_gitattributes_end_in_an_explicit_error() {
    // Git reads info/attributes first, the worker repeats it in
    // core.attributesFile, which Git reads last: here `-text` loses to
    // `text eol=crlf`, so the files differ from the commit and that is said
    let p = pair(|w| {
        std::fs::write(w.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
        std::fs::write(w.join(".git/info/attributes"), "*.txt -text\n").unwrap();
        std::fs::write(w.join("a.txt"), "a\r\n").unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-qm", "base"]);
    });
    std::fs::write(p.work.join("a.txt"), "x\r\ny\r\n").unwrap();
    let c = ctx(&p.home);
    let m = capture_source(&c, &p.work, &opts()).unwrap().manifest;
    let e = worker_workspace(&c, &p, &m).unwrap_err();
    match e.downcast_ref::<SourceError>() {
        Some(SourceError::WorkspaceNotClean { paths }) => {
            assert!(paths.contains(&"a.txt".to_string()), "{paths:?}");
        }
        other => panic!("{e:#} ({other:?})"),
    }
}

// ---- the comparison is made in the new worktree -----------------------------

/// Capture `p.work`, make the objects reachable there, and add a worktree of
/// the capture in `repo` (a path of the original).
fn add_in(
    p: &Pair,
    m: &local_ci::source::SourceManifest,
    repo: &Path,
    name: &str,
) -> (anyhow::Result<()>, PathBuf) {
    let c = ctx(&p.home);
    let wt = p.home.join(name);
    let r = local_ci::source::add_worktree(
        &c,
        &ObjectStore::open(p.home.join("sources")).unwrap(),
        repo,
        &wt,
        m,
    );
    (r, wt)
}

fn captured(p: &Pair) -> local_ci::source::SourceManifest {
    let c = ctx(&p.home);
    let m = capture_source(&c, &p.work, &opts()).unwrap().manifest;
    import_objects(&c, &p.work, &export_objects(&c, &m).unwrap(), &m).unwrap();
    m
}

/// The keys of the `OriginalConfigChanged` that adding a worktree in the
/// original gives, with the original's config as it was and the new worktree
/// gone from the disk and from Git's list.
fn refused_keys(p: &Pair, m: &local_ci::source::SourceManifest, repo: &Path) -> Vec<String> {
    let before = config_bytes(&p.work);
    let (r, wt) = add_in(p, m, repo, "wt");
    let e = r.unwrap_err();
    let keys = match e.downcast_ref::<SourceError>() {
        Some(SourceError::OriginalConfigChanged { keys }) => keys.clone(),
        other => panic!("{e:#} ({other:?})"),
    };
    assert_eq!(config_bytes(&p.work), before);
    assert!(!wt.exists(), "the new worktree is left behind");
    let list = git_out(&p.work, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains("/wt\n") && !list.ends_with("/wt"), "{list}");
    keys
}

#[test]
fn a_changed_eol_or_safecrlf_or_filter_names_its_key_and_removes_the_new_worktree() {
    let p = pair(|w| {
        git(w, &["config", "filter.pre.clean", "cat"]);
        git(w, &["config", "filter.pre.smudge", "cat"]);
        git(w, &["config", "core.safecrlf", "false"]);
        std::fs::write(w.join(".gitattributes"), "*.pf filter=pre\n").unwrap();
        std::fs::write(w.join("x.pf"), "x\n").unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-qm", "base"]);
    });
    std::fs::write(p.work.join("x.pf"), "y\n").unwrap();
    let m = captured(&p);
    git(&p.work, &["config", "core.eol", "crlf"]);
    assert_eq!(refused_keys(&p, &m, &p.work), vec!["core.eol"]);
    git(&p.work, &["config", "--unset", "core.eol"]);
    git(&p.work, &["config", "core.safecrlf", "warn"]);
    assert_eq!(refused_keys(&p, &m, &p.work), vec!["core.safecrlf"]);
    git(&p.work, &["config", "--unset", "core.safecrlf"]);
    assert_eq!(refused_keys(&p, &m, &p.work), vec!["core.safecrlf"]);
    git(&p.work, &["config", "core.safecrlf", "false"]);
    git(&p.work, &["config", "filter.pre.clean", "tr a b"]);
    assert_eq!(refused_keys(&p, &m, &p.work), vec!["filter.pre.clean"]);
}

#[test]
fn changed_info_attributes_of_the_original_are_the_key_attributes() {
    let p = local_pair(false);
    let m = captured(&p);
    std::fs::write(p.work.join(".git/info/attributes"), "*.txt -text\n").unwrap();
    assert_eq!(refused_keys(&p, &m, &p.work), vec!["attributes"]);
}

#[test]
fn a_setting_that_only_the_path_of_the_original_brings_is_not_read_by_a_new_worktree() {
    // an `includeIf gitdir:` that matches the Git directory of the original
    // alone: a worktree added in it does not read the included file
    let p = pair(|w| {
        let real = std::fs::canonicalize(w).unwrap();
        std::fs::write(w.join("inc.cfg"), "[core]\n\tautocrlf = true\n").unwrap();
        let cfg = w.join(".git/config");
        let mut s = std::fs::read_to_string(&cfg).unwrap();
        s.push_str(&format!(
            "[includeIf \"gitdir:{}/.git\"]\n\tpath = {}\n",
            real.display(),
            real.join("inc.cfg").display()
        ));
        std::fs::write(&cfg, s).unwrap();
        std::fs::write(w.join("a.txt"), "a\r\n").unwrap();
        git(w, &["add", "a.txt"]);
        git(w, &["commit", "-qm", "base"]);
    });
    assert_eq!(
        git_out(&p.work, &["config", "--get", "core.autocrlf"]),
        "true"
    );
    dirty_crlf(&p.work);
    let m = captured(&p);
    assert_eq!(refused_keys(&p, &m, &p.work), vec!["core.autocrlf"]);
}

#[test]
fn an_unset_value_is_its_default_and_a_valueless_filter_variable_is_unset() {
    let p = local_pair(false);
    let m = captured(&p);
    git(&p.work, &["config", "core.autocrlf", "false"]);
    git(&p.work, &["config", "core.eol", "native"]);
    let (r, wt) = add_in(&p, &m, &p.work, "wt");
    r.unwrap();
    assert_eq!(git_out(&wt, &["status", "--porcelain"]), "");

    let p = pair(|w| {
        git(w, &["config", "filter.pre.clean", "cat"]);
        let cfg = w.join(".git/config");
        let mut s = std::fs::read_to_string(&cfg).unwrap();
        s.push_str("[filter \"pre\"]\n\trequired\n");
        std::fs::write(&cfg, s).unwrap();
        std::fs::write(w.join(".gitattributes"), "*.pf filter=pre\n").unwrap();
        std::fs::write(w.join("x.pf"), "x\n").unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-qm", "base"]);
    });
    std::fs::write(p.work.join("x.pf"), "y\n").unwrap();
    let m = captured(&p);
    let (r, wt) = add_in(&p, &m, &p.work, "wt");
    r.unwrap();
    assert_eq!(git_out(&wt, &["status", "--porcelain"]), "");
}

#[test]
fn a_linked_worktree_and_a_symlinked_path_of_the_original_are_the_original() {
    let p = local_pair(true);
    let m = captured(&p);
    let linked = p.home.join("linked");
    git(
        &p.work,
        &[
            "worktree",
            "add",
            "--detach",
            "-q",
            linked.to_str().unwrap(),
        ],
    );
    git(&p.work, &["config", "core.autocrlf", "false"]);
    assert_eq!(refused_keys(&p, &m, &linked), vec!["core.autocrlf"]);
    let link = p.home.join("link");
    std::os::unix::fs::symlink(&p.work, &link).unwrap();
    assert_eq!(refused_keys(&p, &m, &link), vec!["core.autocrlf"]);
}

#[test]
fn a_manifest_without_a_repository_id_is_not_taken_for_the_original() {
    let p = local_pair(true);
    let mut m = captured(&p);
    m.repository_id = None;
    let m = local_ci::source::seal_manifest(m);
    git(&p.work, &["config", "core.autocrlf", "false"]);
    let (r, wt) = add_in(&p, &m, &p.work, "wt");
    r.unwrap();
    assert_eq!(git_out(&wt, &["status", "--porcelain"]), "");
}

#[test]
fn a_directory_that_is_not_a_repository_is_an_error_not_a_worker() {
    let p = local_pair(false);
    let m = captured(&p);
    let plain = p.home.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    // the home may itself sit inside a repository (a mutation run points
    // TMPDIR into the target directory): stop git from looking up from it
    let c = ExecContext::new(
        [
            ("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin"),
            ("HOME", p.home.to_str().unwrap()),
            ("LOCAL_CI_HOME", p.home.to_str().unwrap()),
            ("GIT_CEILING_DIRECTORIES", p.home.to_str().unwrap()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    );
    let wt = p.home.join("wt");
    let e = local_ci::source::add_worktree(
        &c,
        &ObjectStore::open(p.home.join("sources")).unwrap(),
        &plain,
        &wt,
        &m,
    )
    .unwrap_err();
    assert!(
        matches!(
            e.downcast_ref::<SourceError>(),
            Some(SourceError::RepositoryUnreadable { .. })
        ),
        "{e:#}"
    );
    assert!(!wt.exists());
}

// ---- secret files: a directory, a secret root, a bundle path --------------

/// No file below `store` holds `secret`, as text or in hex, except `allowed`.
fn assert_no_trace(store: &Path, secret: &str, allowed: Option<&Path>) {
    let hex_secret: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
    for (name, (_, _, bytes)) in listing(store, &[]) {
        if Some(store.join(&name).as_path()) == allowed {
            continue;
        }
        for needle in [secret, hex_secret.as_str()] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "{name} holds {needle}"
            );
        }
    }
}

fn secret_opts(specs: Vec<SecretSpec>) -> CaptureOptions {
    let mut o = opts();
    o.secret_key = KEY.to_vec();
    o.secret_files = specs;
    o
}

fn entry_paths(m: &local_ci::source::SourceManifest) -> Vec<String> {
    m.object_manifest
        .entries
        .iter()
        .map(|e| e.path.display())
        .collect()
}

fn secret_paths(m: &local_ci::source::SourceManifest) -> Vec<String> {
    m.secret_files.iter().map(|s| s.path.display()).collect()
}

fn source_error(e: &anyhow::Error) -> &SourceError {
    e.downcast_ref::<SourceError>()
        .unwrap_or_else(|| panic!("not a SourceError: {e:#}"))
}

struct Plain {
    _tmp: common::Tmp,
    home: PathBuf,
    dir: PathBuf,
}

const DIR_SECRET: &str = "DB_PASSWORD=tr0ub4dor-and-3\n";
const KEY_SECRET: &str = "-----PRIVATE-----zebra-crossing\n";

/// A directory outside Git with two secret files in it.
fn plain_with_secrets() -> Plain {
    let tmp = common::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = tmp.path().join("plain");
    std::fs::create_dir_all(dir.join("conf")).unwrap();
    std::fs::write(dir.join("main.txt"), "main\n").unwrap();
    std::fs::write(dir.join("conf/app.toml"), "name = 'x'\n").unwrap();
    std::fs::write(dir.join(".env"), DIR_SECRET).unwrap();
    std::fs::write(dir.join("conf/prod.key"), KEY_SECRET).unwrap();
    Plain {
        _tmp: tmp,
        home,
        dir,
    }
}

#[test]
fn a_directory_capture_keeps_its_secret_files_out_of_the_objects() {
    let p = plain_with_secrets();
    let c = ctx(&p.home);
    let o = secret_opts(vec![spec(".env", true), spec("conf/*.key", true)]);
    let snap = capture_directory(&c, &p.dir, &o).unwrap();
    let m = &snap.manifest;
    assert_eq!(m.source_kind, SourceKind::Directory);
    assert_eq!(entry_paths(m), ["conf/app.toml", "main.txt"]);
    assert_eq!(secret_paths(m), [".env", "conf/prod.key"]);
    assert_eq!(
        m.secret_files[0].hmac,
        secret_digest(KEY, &m.secret_files[0].path, DIR_SECRET.as_bytes())
    );

    let store = p.home.join("sources");
    let bundle_path = snap.secrets.clone().unwrap();
    assert_eq!(
        bundle_path,
        store.join(format!("secrets/{}.json", m.source_id))
    );
    assert_eq!(
        std::fs::metadata(&bundle_path).unwrap().mode() & 0o777,
        0o600
    );
    assert_no_trace(&store, DIR_SECRET, Some(&bundle_path));
    assert_no_trace(&store, KEY_SECRET, Some(&bundle_path));
    assert_no_trace(&store, "tr0ub4dor", Some(&bundle_path));

    // the workspace has them only after the bundle is placed
    let dest = p.home.join("ws");
    materialize_source(&c, m, &dest).unwrap();
    assert!(!dest.join(".env").exists() && !dest.join("conf/prod.key").exists());
    let bundle = SecretBundle::load(&bundle_path).unwrap();
    materialize_secrets(m, &bundle, KEY, &dest).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join(".env")).unwrap(),
        DIR_SECRET
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("conf/prod.key")).unwrap(),
        KEY_SECRET
    );

    // the secret files are part of what the source is
    std::fs::write(p.dir.join(".env"), "DB_PASSWORD=another\n").unwrap();
    let again = capture_directory(&c, &p.dir, &o).unwrap().manifest;
    assert_ne!(again.source_id, m.source_id);
    assert_eq!(
        again.object_manifest.tree_digest,
        m.object_manifest.tree_digest
    );

    // and without a declaration nothing changed: the files are content
    let all = capture_directory(&c, &p.dir, &opts()).unwrap();
    assert!(all.secrets.is_none() && all.manifest.secret_files.is_empty());
    assert_eq!(
        entry_paths(&all.manifest),
        [".env", "conf/app.toml", "conf/prod.key", "main.txt"]
    );
}

#[test]
fn a_directory_capture_checks_its_secret_declarations_like_a_worktree() {
    let p = plain_with_secrets();
    let c = ctx(&p.home);
    let store = p.home.join("sources");

    // a short key
    let mut o = secret_opts(vec![spec(".env", true)]);
    o.secret_key = b"short".to_vec();
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert_eq!(
        source_error(&err),
        &SourceError::SecretKeyTooShort { len: 5 }
    );

    // required and missing; optional and missing
    let o = secret_opts(vec![spec(".env", true), spec("nothing.pem", true)]);
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert_eq!(
        source_error(&err),
        &SourceError::RequiredSecretMissing {
            glob: "nothing.pem".into()
        }
    );
    assert!(names_in(&store.join("secrets")).is_empty());
    let o = secret_opts(vec![spec("nothing.pem", false)]);
    let snap = capture_directory(&c, &p.dir, &o).unwrap();
    assert!(snap.secrets.is_none());
    assert_eq!(
        snap.warnings,
        ["secret_files \"nothing.pem\" matches no file"]
    );

    // `exclude` is applied first: what it leaves out is not looked into
    let mut o = secret_opts(vec![spec("conf/*.key", true)]);
    o.exclude = vec!["conf".into()];
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert!(matches!(
        source_error(&err),
        SourceError::RequiredSecretMissing { .. }
    ));

    // a secret root is for a repository
    let mut o = secret_opts(vec![spec(".env", true)]);
    o.secret_root = Some(p.dir.clone());
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert!(format!("{err:#}").contains("secret_root"), "{err:#}");
}

#[test]
fn a_secret_path_that_is_not_a_regular_file_fails_a_directory_capture() {
    let p = plain_with_secrets();
    let c = ctx(&p.home);
    let outside = p.home.join("outside.env");
    std::fs::create_dir_all(&p.home).unwrap();
    std::fs::write(&outside, "OUTSIDE=sesame-open\n").unwrap();

    // a symlink out of the directory is not followed and not skipped
    std::fs::remove_file(p.dir.join(".env")).unwrap();
    std::os::unix::fs::symlink(&outside, p.dir.join(".env")).unwrap();
    let o = secret_opts(vec![spec(".env", false)]);
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert_eq!(
        source_error(&err),
        &SourceError::UnsupportedFileType {
            path: ".env".into(),
            kind: "non-regular secret file".into()
        }
    );
    assert_no_trace(&p.home.join("sources"), "sesame-open", None);

    // nor is a FIFO read
    std::fs::remove_file(p.dir.join(".env")).unwrap();
    assert!(Command::new("mkfifo")
        .arg(p.dir.join(".env"))
        .status()
        .unwrap()
        .success());
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert!(matches!(
        source_error(&err),
        SourceError::UnsupportedFileType { path, .. } if path == ".env"
    ));
}

const ROOT_SECRET: &str = "API_TOKEN=purple-monkey-dishwasher\n";

/// The fixture repository, as the original working tree, and a clean clone
/// of its `HEAD` next to it: what a commit capture runs in.
fn with_clone() -> (Fixture, PathBuf) {
    let f = fixture();
    let clone = f.dir.parent().unwrap().join("clone");
    git(
        f.dir.parent().unwrap(),
        &["clone", "-q", f.dir.to_str().unwrap(), "clone"],
    );
    (f, clone)
}

#[test]
fn a_commit_capture_takes_its_secret_files_from_the_secret_root() {
    let (f, clone) = with_clone();
    let c = f.ctx();
    // `.env` is ignored, `local.key` is only untracked: both are secret files
    std::fs::write(f.dir.join(".env"), ROOT_SECRET).unwrap();
    std::fs::create_dir_all(f.dir.join("conf")).unwrap();
    std::fs::write(f.dir.join("conf/local.key"), KEY_SECRET).unwrap();
    // the checkout has a file of its own under a secret name: it is neither
    // content nor the secret
    std::fs::write(clone.join(".env"), "API_TOKEN=from-the-checkout\n").unwrap();
    // `a.txt` is not tracked in the original any more, but the commit has it
    git(&f.dir, &["rm", "-q", "--cached", "a.txt"]);

    let mut o = secret_opts(vec![
        spec(".env", true),
        spec("conf/*.key", true),
        spec("a.txt", true),
    ]);
    o.secret_root = Some(f.dir.join("sub dir")); // any directory of the tree
    let before = (git_state(&f.dir), listing(&f.dir, &[".git"]));
    let snap = capture_source(&c, &clone, &o).unwrap();
    assert_eq!(before, (git_state(&f.dir), listing(&f.dir, &[".git"])));
    let m = &snap.manifest;
    assert_eq!(secret_paths(m), [".env", "conf/local.key"]);
    assert_eq!(
        m.original_head.as_deref(),
        Some(git_out(&clone, &["rev-parse", "HEAD"]).as_str())
    );
    let entries = entry_paths(m);
    assert!(entries.contains(&"a.txt".to_string()));
    assert!(!entries.iter().any(|p| p == ".env" || p.ends_with(".key")));
    assert_eq!(
        snap.warnings,
        ["a.txt is tracked, so it is commit content and not a secret file"]
    );

    let store = f.home.join("sources");
    let bundle_path = snap.secrets.clone().unwrap();
    assert_no_trace(&store, ROOT_SECRET, Some(&bundle_path));
    assert_no_trace(&store, "purple-monkey", Some(&bundle_path));
    assert_no_trace(&store, KEY_SECRET, Some(&bundle_path));
    assert_no_trace(&store, "from-the-checkout", None);
    let gb = std::fs::read(export_objects(&c, m).unwrap()).unwrap();
    assert!(!gb.windows(13).any(|w| w == b"purple-monkey"));

    let dest = f.home.join("ws");
    materialize_source(&c, m, &dest).unwrap();
    assert!(!dest.join(".env").exists());
    let bundle = SecretBundle::load(&bundle_path).unwrap();
    materialize_secrets(m, &bundle, KEY, &dest).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join(".env")).unwrap(),
        ROOT_SECRET
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("conf/local.key")).unwrap(),
        KEY_SECRET
    );
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"a\n");

    // without the secret root, the checkout's own file is the secret
    o.secret_root = None;
    o.secret_files = vec![spec(".env", true)];
    let own = capture_source(&c, &clone, &o).unwrap();
    let own_bundle = SecretBundle::load(&own.secrets.unwrap()).unwrap();
    let dest = f.home.join("ws-own");
    materialize_source(&c, &own.manifest, &dest).unwrap();
    materialize_secrets(&own.manifest, &own_bundle, KEY, &dest).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join(".env")).unwrap(),
        "API_TOKEN=from-the-checkout\n"
    );
}

#[test]
fn a_secret_file_missing_from_the_secret_root_is_an_error_not_a_skip() {
    let (f, clone) = with_clone();
    let c = f.ctx();
    // only the checkout has it: that is not where secrets come from
    std::fs::write(clone.join(".env"), "API_TOKEN=from-the-checkout\n").unwrap();
    let mut o = secret_opts(vec![spec(".env", true)]);
    o.secret_root = Some(f.dir.clone());
    let err = capture_source(&c, &clone, &o).unwrap_err();
    assert_eq!(
        source_error(&err),
        &SourceError::RequiredSecretMissing {
            glob: ".env".into()
        }
    );
    let store = f.home.join("sources");
    assert!(published(&store).is_empty());
    assert_no_trace(&store, "from-the-checkout", None);

    // optional: said, and still not taken from the checkout
    o.secret_files = vec![spec(".env", false)];
    let snap = capture_source(&c, &clone, &o).unwrap();
    assert!(snap.secrets.is_none() && snap.manifest.secret_files.is_empty());
    assert_eq!(snap.warnings, ["secret_files \".env\" matches no file"]);
    assert!(!entry_paths(&snap.manifest).contains(&".env".to_string()));

    // a root that is not a working tree is refused, whatever is in it
    let plain = f.home.join("not-a-repo");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join(".env"), ROOT_SECRET).unwrap();
    let mut o = secret_opts(vec![spec(".env", false)]);
    for bad in [plain, f.home.join("no-such-directory")] {
        o.secret_root = Some(bad);
        let c = ExecContext::new(
            [
                ("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin"),
                ("HOME", f.home.to_str().unwrap()),
                ("LOCAL_CI_HOME", f.home.to_str().unwrap()),
                // a test may itself run inside a repository
                ("GIT_CEILING_DIRECTORIES", f.home.to_str().unwrap()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        );
        let err = capture_source(&c, &clone, &o).unwrap_err();
        assert!(
            format!("{err:#}").contains("is not inside a git working tree"),
            "{err:#}"
        );
    }
}

#[test]
fn a_symlink_or_a_special_file_in_the_secret_root_is_never_read() {
    let (f, clone) = with_clone();
    let c = f.ctx();
    let outside = f.home.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("stolen.key"), "OUTSIDE=sesame-open\n").unwrap();
    let store = f.home.join("sources");

    // the secret path is a symlink that leaves the root
    std::os::unix::fs::symlink(outside.join("stolen.key"), f.dir.join(".env")).unwrap();
    let mut o = secret_opts(vec![spec(".env", false)]);
    o.secret_root = Some(f.dir.clone());
    let err = capture_source(&c, &clone, &o).unwrap_err();
    assert_eq!(
        source_error(&err),
        &SourceError::UnsupportedFileType {
            path: ".env".into(),
            kind: "non-regular secret file".into()
        }
    );
    // even one that stays inside it
    std::fs::remove_file(f.dir.join(".env")).unwrap();
    std::os::unix::fs::symlink("a.txt", f.dir.join(".env")).unwrap();
    let err = capture_source(&c, &clone, &o).unwrap_err();
    assert!(matches!(
        source_error(&err),
        SourceError::UnsupportedFileType { .. }
    ));
    std::fs::remove_file(f.dir.join(".env")).unwrap();

    // a directory above it is a symlink out of the root: nothing below it is
    // a file of the tree, so a required declaration fails
    std::os::unix::fs::symlink(&outside, f.dir.join("conf")).unwrap();
    o.secret_files = vec![spec("conf/*.key", true)];
    let err = capture_source(&c, &clone, &o).unwrap_err();
    assert_eq!(
        source_error(&err),
        &SourceError::RequiredSecretMissing {
            glob: "conf/*.key".into()
        }
    );
    std::fs::remove_file(f.dir.join("conf")).unwrap();

    // a FIFO is not a file either (and is not opened: that would block)
    // required or not, and also in a directory Git ignores
    for (at, glob, required) in [
        (".env", ".env", true),
        (".env", ".env", false),
        ("out/deep/pipe.key", "out/**/*.key", false),
    ] {
        let fifo = f.dir.join(at);
        std::fs::create_dir_all(fifo.parent().unwrap()).unwrap();
        assert!(Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        o.secret_files = vec![spec(glob, required)];
        let err = capture_source(&c, &clone, &o).unwrap_err();
        assert_eq!(
            source_error(&err),
            &SourceError::UnsupportedFileType {
                path: at.into(),
                kind: "FIFO".into()
            }
        );
        // the same when the tree is captured itself
        let mut own = o.clone();
        own.secret_root = None;
        let err = capture_source(&c, &f.dir, &own).unwrap_err();
        assert!(matches!(
            source_error(&err),
            SourceError::UnsupportedFileType { path, .. } if path == at
        ));
        std::fs::remove_file(&fifo).unwrap();
    }
    // one that no declaration matches is not in the way
    assert!(Command::new("mkfifo")
        .arg(f.dir.join("out/other"))
        .status()
        .unwrap()
        .success());
    o.secret_files = vec![spec(".env", false)];
    capture_source(&c, &clone, &o).unwrap();
    std::fs::remove_file(f.dir.join("out/other")).unwrap();
    let _ = std::fs::remove_dir_all(store.join("manifests"));
    std::fs::create_dir_all(store.join("manifests")).unwrap();

    assert!(published(&store).is_empty());
    assert!(names_in(&store.join("secrets")).is_empty());
    assert_no_trace(&store, "sesame-open", None);
}

#[test]
fn a_secret_file_that_moves_in_the_secret_root_during_capture_is_noticed() {
    let (f, clone) = with_clone();
    let c = f.ctx();
    let env = f.dir.join(".env");
    std::fs::write(&env, "API_TOKEN=first-version\n").unwrap();
    let mut o = secret_opts(vec![spec(".env", true)]);
    o.secret_root = Some(f.dir.clone());

    // once: the next attempt settles on what is there now
    let file = env.clone();
    o.hook = hook(move |s| {
        if s == Stage::AfterRead(1) {
            std::fs::write(&file, "API_TOKEN=second-version-longer\n").unwrap();
        }
        Ok(())
    });
    let snap = capture_source(&c, &clone, &o).unwrap();
    let bundle = SecretBundle::load(&snap.secrets.unwrap()).unwrap();
    let dest = f.home.join("ws");
    materialize_source(&c, &snap.manifest, &dest).unwrap();
    materialize_secrets(&snap.manifest, &bundle, KEY, &dest).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join(".env")).unwrap(),
        "API_TOKEN=second-version-longer\n"
    );

    // every time: busy, and it says which file
    let file = env.clone();
    o.hook = hook(move |s| {
        if let Stage::AfterRead(n) = s {
            std::fs::write(&file, format!("API_TOKEN=v{}\n", "x".repeat(n as usize))).unwrap();
        }
        Ok(())
    });
    let err = capture_source(&c, &clone, &o).unwrap_err();
    match source_error(&err) {
        SourceError::Busy { attempts, last } => {
            assert_eq!(*attempts, 3);
            assert_eq!(last, ".env changed");
        }
        other => panic!("{other:?}"),
    }

    // a secret file that appears or goes away is a change of the list
    std::fs::create_dir_all(f.dir.join("conf")).unwrap();
    let late = f.dir.join("conf/late.key");
    o.secret_files = vec![spec(".env", true), spec("conf/*.key", false)];
    o.hook = hook(move |s| {
        if let Stage::AfterRead(n) = s {
            if n % 2 == 1 {
                std::fs::write(&late, "k\n").unwrap();
            } else {
                std::fs::remove_file(&late).unwrap();
            }
        }
        Ok(())
    });
    let err = capture_source(&c, &clone, &o).unwrap_err();
    match source_error(&err) {
        SourceError::Busy { last, .. } => {
            assert_eq!(last, "files were added, removed or renamed")
        }
        other => panic!("{other:?}"),
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn the_secret_bundle_is_written_where_the_caller_says_and_only_there() {
    let p = plain_with_secrets();
    let c = ctx(&p.home);
    let store = p.home.join("sources");
    let run = p.home.join("runs/run-1");
    std::fs::create_dir_all(&run).unwrap();
    let mut o = secret_opts(vec![spec(".env", true)]);
    o.secret_bundle = Some(run.join("secrets.json"));
    let snap = capture_directory(&c, &p.dir, &o).unwrap();
    assert_eq!(
        snap.secrets.as_deref(),
        Some(run.join("secrets.json").as_path())
    );
    assert_eq!(names_in(&run), ["secrets.json"]);
    assert_eq!(
        std::fs::metadata(run.join("secrets.json")).unwrap().mode() & 0o777,
        0o600
    );
    assert!(names_in(&store.join("secrets")).is_empty());
    assert_no_trace(&store, DIR_SECRET, None);
    let bundle = SecretBundle::load(&run.join("secrets.json")).unwrap();
    let dest = p.home.join("ws");
    materialize_source(&c, &snap.manifest, &dest).unwrap();
    materialize_secrets(&snap.manifest, &bundle, KEY, &dest).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join(".env")).unwrap(),
        DIR_SECRET
    );

    // a second run of the same source has a file of its own
    let run2 = p.home.join("runs/run-2");
    std::fs::create_dir_all(&run2).unwrap();
    o.secret_bundle = Some(run2.join("secrets.json"));
    let snap2 = capture_directory(&c, &p.dir, &o).unwrap();
    assert_eq!(snap2.manifest.source_id, snap.manifest.source_id);
    assert_eq!(
        std::fs::read(run2.join("secrets.json")).unwrap(),
        std::fs::read(run.join("secrets.json")).unwrap()
    );

    // the same for a repository
    let f = fixture();
    std::fs::write(f.dir.join(".env"), ROOT_SECRET).unwrap();
    let fc = f.ctx();
    let at = f.home.join("bundle.json");
    std::fs::create_dir_all(&f.home).unwrap();
    let mut o = secret_opts(vec![spec(".env", true)]);
    o.secret_bundle = Some(at.clone());
    let snap = capture_source(&fc, &f.dir, &o).unwrap();
    assert_eq!(snap.secrets.as_deref(), Some(at.as_path()));
    assert!(names_in(&f.home.join("sources/secrets")).is_empty());
    assert_no_trace(&f.home.join("sources"), ROOT_SECRET, None);
    assert_eq!(
        SecretBundle::load(&at).unwrap().paths()[0].display(),
        ".env"
    );

    // nothing matched: nothing is written
    let none = f.home.join("none.json");
    let mut o = secret_opts(vec![spec("nothing.pem", false)]);
    o.secret_bundle = Some(none.clone());
    let snap = capture_source(&fc, &f.dir, &o).unwrap();
    assert!(snap.secrets.is_none());
    assert!(!none.exists());
}

fn bundle_path_error(e: &anyhow::Error) -> (String, String) {
    match source_error(e) {
        SourceError::SecretBundlePath { path, why } => (path.clone(), why.clone()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_bundle_path_that_exists_or_has_no_directory_is_refused_and_nothing_is_replaced() {
    let p = plain_with_secrets();
    let c = ctx(&p.home);
    let store = p.home.join("sources");
    std::fs::create_dir_all(&p.home).unwrap();
    let mut o = secret_opts(vec![spec(".env", true)]);

    // the file is there already
    let taken = p.home.join("taken.json");
    std::fs::write(&taken, "somebody else's\n").unwrap();
    o.secret_bundle = Some(taken.clone());
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert_eq!(
        bundle_path_error(&err),
        (
            taken.display().to_string(),
            "the file already exists".to_string()
        )
    );
    assert!(format!("{err}").contains("cannot write the secret bundle to"));
    assert_eq!(
        std::fs::read_to_string(&taken).unwrap(),
        "somebody else's\n"
    );
    // said before anything was read
    assert!(!store.exists());

    // a dangling symlink is a name that is taken too
    let dangling = p.home.join("dangling.json");
    std::os::unix::fs::symlink(p.home.join("nowhere"), &dangling).unwrap();
    o.secret_bundle = Some(dangling.clone());
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert_eq!(bundle_path_error(&err).1, "the file already exists");
    assert!(!p.home.join("nowhere").exists());

    // the directory is not there: it is not made
    let orphan = p.home.join("no-such-run/secrets.json");
    o.secret_bundle = Some(orphan.clone());
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert_eq!(
        bundle_path_error(&err),
        (
            orphan.display().to_string(),
            "its directory does not exist".to_string()
        )
    );
    assert!(!p.home.join("no-such-run").exists());
    assert!(!store.exists());

    // the name is taken while the capture runs: still not replaced, and the
    // capture fails without publishing
    let run = p.home.join("run");
    std::fs::create_dir_all(&run).unwrap();
    let raced = run.join("secrets.json");
    o.secret_bundle = Some(raced.clone());
    let r = raced.clone();
    o.hook = hook(move |s| {
        if s == Stage::AfterRead(1) {
            std::fs::write(&r, "arrived first\n").unwrap();
        }
        Ok(())
    });
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert_eq!(bundle_path_error(&err).1, "the file already exists");
    assert_eq!(std::fs::read_to_string(&raced).unwrap(), "arrived first\n");
    assert_eq!(names_in(&run), ["secrets.json"]);
    assert!(published(&store).is_empty());
    assert_no_trace(&p.home, "tr0ub4dor", None);
}

#[test]
fn a_bundle_at_a_chosen_path_does_not_outlive_a_capture_that_fails() {
    let p = plain_with_secrets();
    let c = ctx(&p.home);
    let run = p.home.join("run");
    std::fs::create_dir_all(&run).unwrap();
    let at = run.join("secrets.json");
    let mut o = secret_opts(vec![spec(".env", true)]);
    o.secret_bundle = Some(at.clone());
    let seen = Arc::new(AtomicU32::new(0));
    let (n, probe) = (seen.clone(), at.clone());
    o.hook = hook(move |s| {
        if s == Stage::BeforePublish {
            // the bundle is written before the manifest is published
            n.store(probe.exists() as u32 + 1, Ordering::SeqCst);
            anyhow::bail!("stopped before publishing");
        }
        Ok(())
    });
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert!(format!("{err:#}").contains("stopped before publishing"));
    assert_eq!(seen.load(Ordering::SeqCst), 2);
    assert!(names_in(&run).is_empty());
    assert!(published(&p.home.join("sources")).is_empty());
    assert_no_trace(&p.home, "tr0ub4dor", None);

    // the same path serves the next try
    o.hook = None;
    let snap = capture_directory(&c, &p.dir, &o).unwrap();
    assert_eq!(snap.secrets.as_deref(), Some(at.as_path()));

    // the store's own bundle is left as it always was when a capture stops
    // there (the caller owns that file)
    let f = fixture();
    std::fs::write(f.dir.join(".env"), ROOT_SECRET).unwrap();
    let mut o = secret_opts(vec![spec(".env", true)]);
    o.hook = hook(|s| {
        if s == Stage::BeforePublish {
            anyhow::bail!("stopped before publishing");
        }
        Ok(())
    });
    capture_source(&f.ctx(), &f.dir, &o).unwrap_err();
    assert_eq!(names_in(&f.home.join("sources/secrets")).len(), 1);
}

#[test]
fn a_secret_file_that_is_too_large_fails_the_capture_and_is_not_skipped() {
    use local_ci::source::{MAX_SECRET_FILE_BYTES, MAX_SECRET_TOTAL_BYTES};
    let p = plain_with_secrets();
    let c = ctx(&p.home);
    let store = p.home.join("sources");
    let big = std::fs::File::create(p.dir.join("conf/huge.key")).unwrap();
    big.set_len(MAX_SECRET_FILE_BYTES + 1).unwrap();
    // optional or not: it is there, so it is not a declaration without a file
    for required in [true, false] {
        let o = secret_opts(vec![spec(".env", true), spec("conf/*.key", required)]);
        let err = capture_directory(&c, &p.dir, &o).unwrap_err();
        assert_eq!(
            source_error(&err),
            &SourceError::SecretTooLarge {
                path: "conf/huge.key".into(),
                size: MAX_SECRET_FILE_BYTES + 1,
                limit: MAX_SECRET_FILE_BYTES
            }
        );
        assert!(format!("{err:#}").contains("conf/huge.key is too large"));
    }
    assert!(published(&store).is_empty());
    assert!(names_in(&store.join("secrets")).is_empty());
    assert_no_trace(&p.home, "tr0ub4dor", None);

    // each file is within its limit, all of them together are not: the same
    // error, with the limit of the capture, in a repository as well
    std::fs::remove_file(p.dir.join("conf/huge.key")).unwrap();
    let f = fixture();
    std::fs::create_dir_all(f.dir.join("keys")).unwrap();
    let n = MAX_SECRET_TOTAL_BYTES / MAX_SECRET_FILE_BYTES;
    for root in [&p.dir, &f.dir] {
        std::fs::create_dir_all(root.join("keys")).unwrap();
        for i in 0..n {
            let part = std::fs::File::create(root.join(format!("keys/{i}.pem"))).unwrap();
            part.set_len(MAX_SECRET_FILE_BYTES).unwrap();
        }
        std::fs::write(root.join("keys/z.pem"), "1").unwrap();
    }
    let o = secret_opts(vec![spec("keys/*.pem", true)]);
    let want = SourceError::SecretTooLarge {
        path: "keys/z.pem".into(),
        size: MAX_SECRET_TOTAL_BYTES + 1,
        limit: MAX_SECRET_TOTAL_BYTES,
    };
    let err = capture_directory(&c, &p.dir, &o).unwrap_err();
    assert_eq!(source_error(&err), &want);
    let err = capture_source(&f.ctx(), &f.dir, &o).unwrap_err();
    assert_eq!(source_error(&err), &want);
    assert!(published(&f.home.join("sources")).is_empty());

    // exactly the limit of the capture is taken
    std::fs::remove_file(p.dir.join("keys/z.pem")).unwrap();
    let snap = capture_directory(&c, &p.dir, &o).unwrap();
    assert_eq!(snap.manifest.secret_files.len() as u64, n);
}

#[test]
fn a_file_only_the_secret_root_tracks_is_said_to_be_in_neither_place() {
    let (f, clone) = with_clone();
    let c = f.ctx();
    // committed in the original after the clone was made: the captured
    // commit does not have it, and where it is, it is not untracked
    std::fs::create_dir_all(f.dir.join("conf")).unwrap();
    std::fs::write(f.dir.join("conf/later.key"), KEY_SECRET).unwrap();
    git(&f.dir, &["add", "conf/later.key"]);
    git(&f.dir, &["commit", "-qm", "later"]);
    std::fs::write(f.dir.join(".env"), ROOT_SECRET).unwrap();

    let mut o = secret_opts(vec![
        spec(".env", true),
        spec("conf/*.key", false),
        spec("nothing.pem", false),
    ]);
    o.secret_root = Some(f.dir.clone());
    let snap = capture_source(&c, &clone, &o).unwrap();
    let m = &snap.manifest;
    assert_eq!(secret_paths(m), [".env"]);
    assert!(!entry_paths(m).contains(&"conf/later.key".to_string()));
    assert_eq!(
        snap.warnings,
        [
            "conf/later.key is tracked in the working tree of secret_root but not in the captured commit, so it is neither content of the source nor a secret file",
            "secret_files \"nothing.pem\" matches no file",
        ]
    );
    assert_no_trace(&f.home.join("sources"), "zebra-crossing", None);

    // it does not stand in for a required file
    o.secret_files = vec![spec("conf/*.key", true)];
    let err = capture_source(&c, &clone, &o).unwrap_err();
    assert_eq!(
        source_error(&err),
        &SourceError::RequiredSecretMissing {
            glob: "conf/*.key".into()
        }
    );
}

#[test]
fn a_secret_file_is_not_written_through_a_symlink_the_commit_made() {
    use local_ci::source::{RelPath, SecretEntry};
    let f = fixture();
    let c = f.ctx();
    // the commit has `conf` as a symlink to a directory outside the
    // workspace; the secret path below it comes from another tree
    let outside = f.home.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, f.dir.join("conf")).unwrap();
    std::os::unix::fs::symlink("sub dir", f.dir.join("inside")).unwrap();
    git(&f.dir, &["add", "conf", "inside"]);
    git(&f.dir, &["commit", "-qm", "links"]);
    let mut o = opts();
    o.secret_key = KEY.to_vec();
    let m = capture_source(&c, &f.dir, &o).unwrap().manifest;
    let dest = f.home.join("ws");
    materialize_source(&c, &m, &dest).unwrap();
    assert!(dest.join("conf").is_symlink());

    for path in ["conf/prod.key", "inside/prod.key"] {
        let path = RelPath::new(path);
        let mut with_secret = m.clone();
        with_secret.secret_files = vec![SecretEntry {
            hmac: secret_digest(KEY, &path, b"S=1\n"),
            path: path.clone(),
        }];
        let bundle = SecretBundle::new(vec![(path.clone(), 0o600, b"S=1\n".to_vec())]);
        let err = materialize_secrets(&with_secret, &bundle, KEY, &dest).unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<SourceError>(),
                Some(SourceError::UnsafePath { .. })
            ),
            "{err:#}"
        );
    }
    assert!(names_in(&outside).is_empty());
    assert!(!dest.join("sub dir/prod.key").exists());

    // the file itself: a symlink of the commit at the secret path is not
    // written through either
    let path = RelPath::new("link");
    let mut with_secret = m.clone();
    with_secret.secret_files = vec![SecretEntry {
        hmac: secret_digest(KEY, &path, b"S=1\n"),
        path: path.clone(),
    }];
    let bundle = SecretBundle::new(vec![(path, 0o600, b"S=1\n".to_vec())]);
    assert!(materialize_secrets(&with_secret, &bundle, KEY, &dest).is_err());
    assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "a\n");
}
