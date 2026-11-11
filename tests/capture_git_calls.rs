//! A capture is the same capture however it asks Git: the manifests of a
//! fixture with a bit of everything are pinned (`tests/golden/`), and the
//! number of git processes one capture starts does not grow with the number
//! of files.
//!
//! `LOCAL_CI_UPDATE_GOLDEN=1` rewrites the golden file;
//! `LOCAL_CI_CAPTURE_PROFILE=<file>` writes the git processes of the
//! captures, with the time each took.

#![allow(clippy::disallowed_methods)] // the tests build their own commands

mod common;
use local_ci::source::{capture_directory, capture_source, CaptureOptions, SecretSpec};
use local_ci::ExecContext;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

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
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // fixed dates: the same fixture is the same commits
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
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

fn write(p: PathBuf, content: impl AsRef<[u8]>) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn chmod(p: PathBuf, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[derive(Clone)]
struct World {
    _tmp: std::sync::Arc<common::Tmp>,
    home: PathBuf,
    /// the dirty working tree
    work: PathBuf,
    /// a clean checkout of its HEAD (a worktree of the same repository)
    clean: PathBuf,
    /// a directory that is not a repository
    plain: PathBuf,
    trace: PathBuf,
    /// the store of the captures
    store: PathBuf,
    /// set, the question git does not answer ([`Unanswered`])
    unanswered: Option<Unanswered>,
}

/// A question the git of `tests/fixtures/git-that-fails` does not answer:
/// see there.
#[derive(Clone)]
struct Unanswered {
    pattern: &'static str,
    stdin: &'static str,
    mode: &'static str,
    log: PathBuf,
}

const PATH: &str = "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin";

fn real_git() -> String {
    PATH.split(':')
        .map(|d| format!("{d}/git"))
        .find(|g| Path::new(g).is_file())
        .unwrap()
}

impl World {
    /// The context of a capture; every git process it starts is written to
    /// `trace`.
    fn ctx(&self) -> ExecContext {
        let mut env = vec![
            ("PATH", PATH.to_string()),
            ("HOME", self.home.to_str().unwrap().to_string()),
            ("LOCAL_CI_HOME", self.home.to_str().unwrap().to_string()),
            ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
            ("GIT_TRACE2_EVENT", self.trace.to_str().unwrap().to_string()),
        ];
        if let Some(u) = &self.unanswered {
            let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/git-that-fails");
            env[0].1 = format!("{dir}:{PATH}");
            env.extend([
                ("GIT_WRAP_REAL", real_git()),
                ("GIT_WRAP_MATCH", u.pattern.to_string()),
                ("GIT_WRAP_STDIN", u.stdin.to_string()),
                ("GIT_WRAP_MODE", u.mode.to_string()),
                ("GIT_WRAP_LOG", u.log.to_str().unwrap().to_string()),
            ]);
        }
        ExecContext::new(env.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    fn opts(&self) -> CaptureOptions {
        CaptureOptions {
            store: Some(self.store.clone()),
            include: vec!["out/**".into()],
            exclude: vec!["docs/n1.txt".into()],
            secret_files: vec![
                SecretSpec {
                    glob: ".env".into(),
                    required: true,
                },
                SecretSpec {
                    glob: "secrets/*".into(),
                    required: false,
                },
                SecretSpec {
                    glob: "nothing/*".into(),
                    required: false,
                },
            ],
            secret_key: KEY.to_vec(),
            ..CaptureOptions::default()
        }
    }
}

fn lfs_pointer(payload: &[u8]) -> (String, String) {
    use sha2::{Digest, Sha256};
    let oid: String = Sha256::digest(payload)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        payload.len()
    );
    (oid, pointer)
}

fn library(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    write(dir.join("lib.txt"), format!("{name}\n"));
    write(dir.join("src/inner.rs"), "pub fn inner() {}\n");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", name]);
}

/// A repository with a bit of everything, `n` times over: every kind of
/// file below exists `n` times, so that what is asked of Git once per file
/// shows as a count that grows with `n`.
fn world(n: usize) -> World {
    let tmp = common::tempdir().unwrap();
    // a fixed name below the temporary one: the id of a submodule is taken
    // from the last two components of where it was cloned from
    let root = std::fs::canonicalize(tmp.path()).unwrap().join("fx");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&home).unwrap();
    library(&root.join("lib"), "lib");
    library(&root.join("lib2"), "lib2");

    let d = &work;
    std::fs::create_dir_all(d).unwrap();
    git(d, &["init", "-q", "-b", "main"]);
    // a filter that is a program: what Git stores is upper case
    git(d, &["config", "filter.upper.clean", "tr a-z A-Z"]);
    git(d, &["config", "filter.upper.smudge", "cat"]);
    git(d, &["config", "filter.upper.required", "true"]);
    git(d, &["config", "filter.unused.clean", "cat"]);
    write(d.join(".gitignore"), "*.log\n.env\nout/\nsecrets/\n");
    write(
        d.join(".gitattributes"),
        "*.txt text\n*.crlf text eol=crlf\n*.dat filter=lfs diff=lfs merge=lfs -text\n*.up filter=upper\n",
    );
    write(d.join("docs/.gitattributes"), "*.md text eol=lf\n");
    let mut pointers = vec![];
    for i in 0..n {
        for j in 0..4 {
            write(
                d.join(format!("src/d{i}/f{j}.rs")),
                format!("fn f{i}_{j}() {{}}\n"),
            );
        }
        write(d.join(format!("docs/n{i}.txt")), format!("note {i}\n"));
        write(d.join(format!("docs/m{i}.md")), format!("# m{i}\n"));
        write(d.join(format!("docs/gone{i}.txt")), "gone\n");
        write(d.join(format!("win{i}.crlf")), "one\r\ntwo\r\n");
        write(d.join(format!("docs/data{i}.up")), format!("lower {i}\n"));
        write(
            d.join(format!("docs/same{i}.up")),
            format!("as it was {i}\n"),
        );
        write(d.join(format!("bin/run{i}.sh")), "#!/bin/sh\necho hi\n");
        chmod(d.join(format!("bin/run{i}.sh")), 0o755);
        write(d.join(format!("bin/tool{i}.sh")), "#!/bin/sh\n");
        chmod(d.join(format!("bin/tool{i}.sh")), 0o755);
        write(
            d.join(format!("blob{i}.bin")),
            [0u8, 159, 146, 150, i as u8],
        );
        write(d.join(format!("sub dir/new\u{3042}{i}.txt")), "kana\n");
        std::os::unix::fs::symlink(format!("docs/n{i}.txt"), d.join(format!("link{i}"))).unwrap();
        std::os::unix::fs::symlink("docs", d.join(format!("dirlink{i}"))).unwrap();
        // LFS, without git-lfs: the pointers are committed as they are
        for kind in ["ptr", "smudged"] {
            let payload = format!("a large {kind} file {i}\n").repeat(50).into_bytes();
            let (oid, pointer) = lfs_pointer(&payload);
            let name = format!("{kind}{i}.dat");
            write(d.join(&name), &pointer);
            let blob = git_out(d, &["hash-object", "-w", "--no-filters", &name]);
            pointers.push((name, blob, oid, payload));
        }
    }
    git(
        d,
        &["add", "--", ".gitignore", ".gitattributes", "docs", "src"],
    );
    git(d, &["add", "--", "bin", "sub dir"]);
    for i in 0..n {
        git(
            d,
            &[
                "add",
                "--",
                &format!("win{i}.crlf"),
                &format!("blob{i}.bin"),
                &format!("link{i}"),
                &format!("dirlink{i}"),
            ],
        );
    }
    for (name, blob, oid, payload) in &pointers {
        git(
            d,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{name}"),
            ],
        );
        write(
            d.join(".git/lfs/objects")
                .join(&oid[..2])
                .join(&oid[2..4])
                .join(oid),
            payload,
        );
        if name.starts_with("smudged") {
            write(d.join(name), payload);
        }
    }
    git(d, &["commit", "-qm", "base"]);
    // relative: `.gitmodules` is content, and must not name a temporary path
    git(d, &["submodule", "add", "-q", "../lib", "vendor/lib"]);
    git(d, &["submodule", "add", "-q", "../lib2", "vendor/lib2"]);
    git(d, &["commit", "-qm", "add submodules"]);
    let clean = root.join("clean");
    git(
        d,
        &["worktree", "add", "-q", "--detach", clean.to_str().unwrap()],
    );

    // ---- and now the working tree moves away from the commit ----
    for i in 0..n {
        // changed, not staged: no attribute, `text`, `eol=crlf`, in a
        // directory with a `.gitattributes` of its own
        write(d.join(format!("src/d{i}/f0.rs")), "fn changed() {}\n");
        write(d.join(format!("docs/n{i}.txt")), "changed\r\nnote\n");
        write(d.join(format!("docs/m{i}.md")), "# changed\r\n");
        write(d.join(format!("win{i}.crlf")), "one\r\ntwo\r\nthree\r\n");
        // through the filter program: changed, and new
        write(d.join(format!("docs/data{i}.up")), format!("changed {i}\n"));
        write(d.join(format!("new{i}.up")), format!("not tracked {i}\n"));
        // staged
        write(d.join(format!("src/d{i}/f1.rs")), "fn staged() {}\n");
        git(d, &["add", "--", &format!("src/d{i}/f1.rs")]);
        // new and staged, one of them changed again
        write(d.join(format!("src/d{i}/added.rs")), "fn added() {}\n");
        write(d.join(format!("docs/added{i}.txt")), "added\n");
        write(d.join(format!("src/d{i}/again.rs")), "fn first() {}\n");
        git(
            d,
            &[
                "add",
                "--",
                &format!("src/d{i}/added.rs"),
                &format!("docs/added{i}.txt"),
                &format!("src/d{i}/again.rs"),
            ],
        );
        write(d.join(format!("src/d{i}/again.rs")), "fn second() {}\n");
        // deleted, mode changed, a link that points elsewhere, binary
        std::fs::remove_file(d.join(format!("docs/gone{i}.txt"))).unwrap();
        chmod(d.join(format!("bin/run{i}.sh")), 0o644);
        std::fs::remove_file(d.join(format!("link{i}"))).unwrap();
        std::os::unix::fs::symlink("/outside/the/repo", d.join(format!("link{i}"))).unwrap();
        write(d.join(format!("blob{i}.bin")), [0u8, 255, 254, 0, i as u8]);
        // untracked, ignored (and included), secret
        write(d.join(format!("untracked{i}.txt")), "new\r\n");
        write(d.join(format!("src/d{i}/untracked.rs")), "fn new() {}\n");
        write(d.join(format!("exec_new{i}.sh")), "#!/bin/sh\n");
        chmod(d.join(format!("exec_new{i}.sh")), 0o755);
        write(d.join(format!("noise{i}.log")), "noise\n");
        write(d.join(format!("out/gen{i}.txt")), "generated\n");
        write(d.join(format!("secrets/key{i}.pem")), format!("key {i}\n"));
        // index flags: never compared against the working file by Git
        write(d.join(format!("src/d{i}/f2.rs")), "fn hidden() {}\n");
        git(
            d,
            &[
                "update-index",
                "--assume-unchanged",
                &format!("src/d{i}/f2.rs"),
            ],
        );
        write(d.join(format!("src/d{i}/f3.rs")), "fn skipped() {}\n");
        git(
            d,
            &[
                "update-index",
                "--skip-worktree",
                &format!("src/d{i}/f3.rs"),
            ],
        );
    }
    // names that a list of paths, one per line, cannot hold as they are
    // (once, whatever `n` is: each of them is hashed by a process of its own)
    write(d.join("docs/line\nbreak.txt"), "odd\r\n");
    write(d.join("\"quoted\".txt"), "odd\r\n");
    write(d.join("docs/return.txt\r"), "odd\n");
    write(d.join(".env"), "API_TOKEN=correct-horse-battery-staple\n");
    chmod(d.join(".env"), 0o600);
    // one submodule is dirty, the other is as committed
    write(d.join("vendor/lib/lib.txt"), "edited in the submodule\n");
    write(d.join("vendor/lib/new.txt"), "new in the submodule\n");

    let plain = root.join("plain");
    for i in 0..n {
        write(plain.join(format!("a{i}.txt")), "a\n");
        write(plain.join(format!("dir/b{i}.sh")), "#!/bin/sh\n");
        chmod(plain.join(format!("dir/b{i}.sh")), 0o755);
        std::os::unix::fs::symlink(format!("a{i}.txt"), plain.join(format!("l{i}"))).unwrap();
        write(plain.join(format!("secrets/key{i}.pem")), "key\n");
    }
    write(plain.join(".env"), "TOKEN=1\n");
    World {
        trace: root.join("trace.json"),
        store: home.join("store"),
        unanswered: None,
        _tmp: std::sync::Arc::new(tmp),
        home,
        work,
        clean,
        plain,
    }
}

/// The git processes a capture started itself (not those Git started), in
/// order: the arguments and the seconds each took.
fn spawned(trace: &Path) -> Vec<(Vec<String>, f64)> {
    let text = std::fs::read_to_string(trace).unwrap_or_default();
    let mut order = vec![];
    let mut by_sid = std::collections::HashMap::new();
    for line in text.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let sid = v["sid"].as_str().unwrap().to_string();
        if sid.contains('/') {
            continue;
        }
        match v["event"].as_str().unwrap() {
            "start" => {
                let argv: Vec<String> = v["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_string())
                    .collect();
                order.push(sid.clone());
                by_sid.insert(sid, (argv, 0.0));
            }
            "atexit" => {
                by_sid.get_mut(&sid).unwrap().1 = v["t_abs"].as_f64().unwrap();
            }
            _ => {}
        }
    }
    order
        .into_iter()
        .map(|s| by_sid.remove(&s).unwrap())
        .collect()
}

/// The git subcommand of a command line (after `-C x`, `--git-dir x`, `-c x`).
fn subcommand(argv: &[String]) -> String {
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-C" | "--git-dir" | "--work-tree" | "-c" => {
                it.next();
            }
            s => return s.to_string(),
        }
    }
    String::new()
}

struct Captured {
    /// what is pinned: manifest, warnings, the secret files of the bundle
    json: serde_json::Value,
    /// the git processes the capture started
    spawns: usize,
}

fn profile_line(name: &str, w: &World, wall: f64) -> String {
    let sp = spawned(&w.trace);
    let mut by: std::collections::BTreeMap<String, (usize, f64)> = Default::default();
    for (argv, t) in &sp {
        let e = by.entry(subcommand(argv)).or_default();
        e.0 += 1;
        e.1 += t;
    }
    let mut s = format!("{name}: {} git processes, {:.3} s wall\n", sp.len(), wall);
    for (k, (c, t)) in by {
        s.push_str(&format!("    {c:4} x {k:<14} {t:.3} s inside git\n"));
    }
    s
}

fn run(
    w: &World,
    name: &str,
    f: impl FnOnce(&ExecContext) -> local_ci::source::SourceSnapshot,
) -> Captured {
    let _ = std::fs::remove_file(&w.trace);
    let t0 = std::time::Instant::now();
    let snap = f(&w.ctx());
    let seconds = t0.elapsed().as_secs_f64();
    let bundle = snap.secrets.as_ref().map(|p| {
        let text = std::fs::read_to_string(p).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()
    });
    if let Ok(p) = std::env::var("LOCAL_CI_CAPTURE_PROFILE") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .unwrap();
        f.write_all(profile_line(name, w, seconds).as_bytes())
            .unwrap();
    }
    Captured {
        json: serde_json::json!({
            "manifest": serde_json::to_value(&snap.manifest).unwrap(),
            "warnings": snap.warnings,
            "secret_bundle": bundle,
        }),
        spawns: spawned(&w.trace).len(),
    }
}

fn worktree(w: &World) -> Captured {
    run(w, "worktree", |c| {
        capture_source(c, &w.work, &w.opts()).unwrap()
    })
}

/// The clean checkout of a commit, with the secret files of the working
/// tree it was made from.
fn commit(w: &World) -> Captured {
    run(w, "commit", |c| {
        let mut o = w.opts();
        o.secret_root = Some(w.work.join("docs"));
        capture_source(c, &w.clean, &o).unwrap()
    })
}

fn directory(w: &World) -> Captured {
    run(w, "directory", |c| {
        let mut o = w.opts();
        o.include = vec![];
        capture_directory(c, &w.plain, &o).unwrap()
    })
}

/// The refs of the private repositories of the store (one per repository:
/// the superproject and the dirty submodule), without the keys of their
/// directories, which hold a temporary path.
fn private_refs(w: &World) -> Vec<String> {
    let mut all = vec![];
    for e in std::fs::read_dir(w.store.join("git")).unwrap() {
        let dir = e.unwrap().path();
        // every object the refs reach is there
        git(&dir, &["fsck", "--connectivity-only"]);
        all.push(git_out(
            &dir,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        ));
    }
    all.sort();
    all
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/capture.json")
}

#[test]
fn the_captures_of_a_fixture_with_a_bit_of_everything_are_what_they_were() {
    let w = world(2);
    let wt = worktree(&w);
    let refs_after_worktree = private_refs(&w);
    let ci = commit(&w);
    let dir = directory(&w);
    let got = serde_json::json!({
        "worktree": wt.json,
        "refs_after_worktree": refs_after_worktree,
        "commit": ci.json,
        "refs_after_commit": private_refs(&w),
        "directory": dir.json,
    });
    let golden = golden_path();
    let text = serde_json::to_string_pretty(&got).unwrap() + "\n";
    if std::env::var_os("LOCAL_CI_UPDATE_GOLDEN").is_some() {
        write(golden.clone(), &text);
    }
    let want = std::fs::read_to_string(&golden).unwrap();
    assert!(
        text == want,
        "the capture differs from tests/golden/capture.json:\n{text}"
    );
    // the original is as it was: a second capture is the same capture
    assert_eq!(worktree(&w).json, got["worktree"]);
}

#[test]
fn the_number_of_git_processes_does_not_grow_with_the_number_of_files() {
    let (small, large) = (world(1), world(6));
    for (name, f) in [
        ("worktree", worktree as fn(&World) -> Captured),
        ("commit", commit),
        ("directory", directory),
    ] {
        let (a, b) = (f(&small), f(&large));
        assert_eq!(
            a.spawns, b.spawns,
            "{name}: {} git processes for the small fixture, {} for the large one",
            a.spawns, b.spawns
        );
        // and a second capture, which finds the history and the blobs
        // already in the store, starts no more than the first
        let again = f(&large);
        assert!(again.spawns <= b.spawns, "{name}: {}", again.spawns);
    }
    assert_eq!(directory(&small).spawns, 0);
}

/// Each question a capture asks of Git for several things at once has the
/// questions it replaces behind it. Here git does not answer one of them (it
/// fails, or says something else), and the captures are still the pinned
/// ones.
#[test]
fn a_capture_is_the_same_when_git_does_not_answer_a_question_that_was_put_together() {
    let (toplevel, in_store) = (
        "*rev-parse --show-toplevel --is-shallow-repository*",
        "--git-dir * cat-file --batch-check",
    );
    // what is asked, the command line, the first line of its input, and
    // what git does instead of answering
    let cases = [
        // `locate`
        ("where the repository is", toplevel, "", "fail"),
        ("where the repository is", toplevel, "", "garble"),
        // `head_of`
        ("HEAD", "*--abbrev-ref=loose*", "", "fail"),
        ("HEAD", "*--abbrev-ref=loose*", "", "garble"),
        // the index with its flags: then `ls-files -s`, and `ls-files -v`
        // for the flags
        ("the index and its flags", "*ls-files -z -s -v", "", "fail"),
        // the files of a `secret_root`
        ("tracked and not", "*ls-files -z -t -c -o", "", "fail"),
        ("tracked and not", "*ls-files -z -t -c -o", "", "garble"),
        // `import_objects`: the look that says what the store has (the
        // look for blobs alone, which is behind it, is answered)
        ("what the store has", in_store, "refs/*", "fail"),
        ("what the store has", in_store, "refs/*", "garble"),
        ("what the store has", in_store, "refs/*", "ambiguous"),
        // `pack_across`
        (
            "the staged blobs",
            "-C * pack-objects --stdout*",
            "",
            "fail",
        ),
        (
            "the staged blobs",
            "--git-dir * unpack-objects -q",
            "",
            "fail",
        ),
        // `hash_filtered_all`
        ("the blobs of changed files", "*--stdin-paths", "", "fail"),
        ("the blobs of changed files", "*--stdin-paths", "", "garble"),
        // `set_refs`
        ("the two refs", "*update-ref --stdin", "", "fail"),
        // `default_base`
        ("the base", "-C * cat-file --batch-check", "", "fail"),
        ("the base", "-C * cat-file --batch-check", "", "garble"),
    ];
    let world = world(2);
    let want: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(golden_path()).unwrap()).unwrap();
    // a capture only reads the original, so the cases share it; each has a
    // store of its own, where what is copied once is copied again
    let case =
        |i: usize,
         (what, pattern, stdin, mode): (&str, &'static str, &'static str, &'static str)| {
            let name = format!("{what} ({mode})");
            let mut w = world.clone();
            w.store = w.home.join(format!("store{i}"));
            w.trace = w.home.join(format!("trace{i}.json"));
            let log = w.home.join(format!("unanswered{i}"));
            w.unanswered = Some(Unanswered {
                pattern,
                stdin,
                mode,
                log: log.clone(),
            });
            assert_eq!(worktree(&w).json, want["worktree"], "{name}");
            let refs = serde_json::json!(private_refs(&w));
            assert_eq!(refs, want["refs_after_worktree"], "{name}");
            assert_eq!(commit(&w).json, want["commit"], "{name}");
            let refs = serde_json::json!(private_refs(&w));
            assert_eq!(refs, want["refs_after_commit"], "{name}");
            // and the question was asked, and not answered
            let asked = std::fs::read_to_string(&log).unwrap_or_default();
            assert!(!asked.is_empty(), "{name}: git was never asked {pattern}");
        };
    std::thread::scope(|s| {
        for (i, c) in cases.into_iter().enumerate() {
            let case = &case;
            s.spawn(move || case(i, c));
        }
    });
}

/// A filter program that is given the path of the file (`%f`) can look for
/// it. The content a capture has Git clean is not a file of a working tree,
/// and no file is at that path where the filter runs: what such a filter
/// answers is the same whether one file changed or many.
#[test]
fn a_filter_that_looks_for_the_file_finds_none_however_many_files_changed() {
    let tmp = common::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let (home, d) = (root.join("home"), root.join("work"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&d).unwrap();
    git(&d, &["init", "-q", "-b", "main"]);
    let looks = r#"sh -c 'if test -e "$1"; then echo there; else echo absent; fi; cat' - %f"#;
    git(&d, &["config", "filter.looks.clean", looks]);
    git(&d, &["config", "filter.upper.clean", "tr a-z A-Z"]);
    write(
        d.join(".gitattributes"),
        "*.see filter=looks\n*.up filter=upper\n*.txt text\n",
    );
    for i in 0..4 {
        write(d.join(format!("dir/f{i}.see")), "first\n");
        write(d.join(format!("g{i}.up")), "first\n");
        write(d.join(format!("t{i}.txt")), "first\n");
    }
    git(&d, &["add", "."]);
    git(&d, &["commit", "-qm", "base"]);

    let ctx = ExecContext::new(
        [
            ("PATH", PATH),
            ("HOME", home.to_str().unwrap()),
            ("LOCAL_CI_HOME", home.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    );
    let store = home.join("store");
    // a capture of `d`: the content of a path in the commit it leaves
    let capture = || {
        let opts = CaptureOptions {
            store: Some(store.clone()),
            ..CaptureOptions::default()
        };
        let snap = capture_source(&ctx, &d, &opts).unwrap();
        let commit = snap.manifest.execution_commit.unwrap();
        let dirs: Vec<PathBuf> = std::fs::read_dir(store.join("git"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(dirs.len(), 1);
        move |path: &str| git_out(&dirs[0], &["cat-file", "blob", &format!("{commit}:{path}")])
    };

    // committed by Git itself, in a working tree: the file was there
    let blob = capture();
    assert_eq!(blob("dir/f0.see"), "there\nfirst");

    // one file changed
    write(d.join("dir/f0.see"), "second 0\n");
    let blob = capture();
    assert_eq!(blob("dir/f0.see"), "absent\nsecond 0");
    assert_eq!(blob("dir/f1.see"), "there\nfirst");

    // many changed, new ones too, among files of other filters and of none
    for i in 0..4 {
        write(d.join(format!("dir/f{i}.see")), format!("third {i}\n"));
        write(d.join(format!("new{i}.see")), format!("new {i}\n"));
        write(d.join(format!("g{i}.up")), format!("third {i}\n"));
        write(d.join(format!("t{i}.txt")), format!("third {i}\r\n"));
    }
    let blob = capture();
    for i in 0..4 {
        assert_eq!(blob(&format!("dir/f{i}.see")), format!("absent\nthird {i}"));
        assert_eq!(blob(&format!("new{i}.see")), format!("absent\nnew {i}"));
        assert_eq!(blob(&format!("g{i}.up")), format!("THIRD {i}"));
        assert_eq!(blob(&format!("t{i}.txt")), format!("third {i}"));
    }

    // with a setting that touches every file, the attributes are not asked
    // for where they are otherwise: the filter of a file is asked on its own
    git(&d, &["config", "core.autocrlf", "input"]);
    let blob = capture();
    for i in 0..4 {
        assert_eq!(blob(&format!("dir/f{i}.see")), format!("absent\nthird {i}"));
        assert_eq!(blob(&format!("new{i}.see")), format!("absent\nnew {i}"));
        assert_eq!(blob(&format!("g{i}.up")), format!("THIRD {i}"));
    }
}
