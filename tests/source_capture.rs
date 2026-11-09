//! Capturing a working directory: what it takes, that it leaves the original
//! alone, that it notices movement, and what it hands to another host.

#![allow(clippy::disallowed_methods)] // the tests build their own commands

use local_ci::source::{
    capture_directory, capture_source, export_objects, import_objects, materialize_secrets,
    materialize_source, publishable, seal_manifest, secret_digest, CaptureOptions, ObjectStore,
    SecretBundle, SecretSpec, SourceError, SourceInfo, SourceKind, Stage,
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
    _tmp: tempfile::TempDir,
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
    let tmp = tempfile::tempdir().unwrap();
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
    let tmp = tempfile::tempdir().unwrap();
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
    let tmp = tempfile::tempdir().unwrap();
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
    _tmp: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    worker: PathBuf,
    out: PathBuf,
}

/// A repository whose `.txt` files are LF in Git and CRLF on disk, with a
/// filter that appends a line every time it smudges, a workflow that copies
/// files out, and a clone (the worker) that has `main` only.
fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
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
        no_canirun: true,
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
    _tmp: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    worker: PathBuf,
}

fn pair(setup: impl FnOnce(&Path)) -> Pair {
    let tmp = tempfile::tempdir().unwrap();
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
    let bare_path = ExecContext::new(
        [
            ("PATH", "/usr/bin:/bin".to_string()),
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
    let tmp = tempfile::tempdir().unwrap();
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
        no_canirun: true,
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
