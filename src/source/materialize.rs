//! Building a workspace from a manifest.

use super::capture::tree_digest;
use super::secret::{self, SecretBundle};
use super::store::{check_safe, prepare_parent, sha256_hex, ObjectStore, RelPath};
use super::*;
use crate::context::ExecContext;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// The store at its default place for `ctx`.
pub fn default_store(ctx: &ExecContext) -> Result<ObjectStore> {
    ObjectStore::open(ctx.cache_dir().join("sources"))
}

/// Write the workspace of `m` into `dest` (which does not exist yet, or is an
/// empty directory) from the default store. See [`materialize_from`].
pub fn materialize_source(ctx: &ExecContext, m: &SourceManifest, dest: &Path) -> Result<()> {
    materialize_from(&default_store(ctx)?, m, dest)
}

/// Write the workspace of `m` into `dest` from `store`, then check that what
/// is there hashes to `m.object_manifest.tree_digest`.
///
/// Paths that are absolute, hold `..` or reach into `.git` are refused, as is
/// anything that would be written through a symlink. Symlinks are made with
/// the link value as captured and never followed. Submodules are materialized
/// from their own manifests. Secret files are not part of this; see
/// [`materialize_secrets`].
pub fn materialize_from(store: &ObjectStore, m: &SourceManifest, dest: &Path) -> Result<()> {
    check_digest_of(m)?;
    match std::fs::read_dir(dest) {
        Ok(it) => {
            // a worktree made with `--no-checkout` holds its `.git` file
            let others = it
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().as_bytes() != b".git");
            if others {
                bail!("{} is not empty", dest.display());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(dest)?,
        Err(e) => return Err(e.into()),
    }
    let sub_paths: Vec<&RelPath> = m.submodules.iter().map(|s| &s.path).collect();
    for e in &m.object_manifest.entries {
        if sub_paths
            .iter()
            .any(|s| e.path.starts_with_dir(s) || &e.path == *s)
        {
            return Err(SourceError::Corrupt {
                what: format!("{} lies inside a submodule", e.path.display()),
            }
            .into());
        }
        let target = prepare_parent(dest, &e.path)?;
        let content = store.read(&e.hash)?;
        if sha256_hex(&content) != e.hash || content.len() as u64 != e.size {
            return Err(SourceError::Corrupt {
                what: format!("object {} does not match its hash", e.hash),
            }
            .into());
        }
        match e.mode {
            0o120000 => std::os::unix::fs::symlink(OsStr::from_bytes(&content), &target)
                .with_context(|| format!("cannot create {}", target.display()))?,
            0o100644 | 0o100755 => {
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(e.mode & 0o777)
                    .open(&target)
                    .with_context(|| format!("cannot create {}", target.display()))?;
                f.write_all(&content)?;
                drop(f);
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(e.mode & 0o777))?;
            }
            other => bail!("{}: unsupported mode {other:o}", e.path.display()),
        }
    }
    for s in &m.submodules {
        let dir = prepare_parent(dest, &s.path)?;
        match std::fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", dir.display())),
        }
        if let Some(id) = &s.source_id {
            let nested = store.load_manifest(id)?.with_context(|| {
                format!(
                    "submodule {}: manifest {id} is not in the store",
                    s.path.display()
                )
            })?;
            materialize_from(store, &nested, &dir)?;
        }
    }
    let got = scan(dest, &sub_paths)?;
    if tree_digest(&got) != m.object_manifest.tree_digest {
        return Err(SourceError::Corrupt {
            what: "the materialized tree does not hash to the manifest".into(),
        }
        .into());
    }
    Ok(())
}

fn check_digest_of(m: &SourceManifest) -> Result<()> {
    super::capture::check_digest(m)
}

/// What is under `dest` (not looking into `skip`), in manifest form.
fn scan(dest: &Path, skip: &[&RelPath]) -> Result<Vec<FileEntry>> {
    fn walk(root: &Path, rel: &RelPath, skip: &[&RelPath], out: &mut Vec<FileEntry>) -> Result<()> {
        let here = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel.to_path())
        };
        for ent in std::fs::read_dir(&here)? {
            let name = ent?.file_name();
            if rel.is_empty() && name.as_bytes() == b".git" {
                continue; // the worktree's own link to its repository
            }
            let mut b = rel.0.clone();
            if !b.is_empty() {
                b.push(b'/');
            }
            b.extend_from_slice(name.as_bytes());
            let p = RelPath(b);
            let full = root.join(p.to_path());
            let md = std::fs::symlink_metadata(&full)?;
            if md.is_dir() {
                if skip.iter().any(|s| **s == p) {
                    continue;
                }
                walk(root, &p, skip, out)?;
            } else if md.file_type().is_symlink() {
                let t = std::fs::read_link(&full)?;
                let bytes = t.as_os_str().as_bytes();
                out.push(FileEntry {
                    path: p,
                    mode: 0o120000,
                    size: bytes.len() as u64,
                    hash: sha256_hex(bytes),
                });
            } else {
                let bytes = std::fs::read(&full)?;
                out.push(FileEntry {
                    path: p,
                    mode: if md.mode() & 0o111 != 0 {
                        0o100755
                    } else {
                        0o100644
                    },
                    size: bytes.len() as u64,
                    hash: sha256_hex(&bytes),
                });
            }
        }
        Ok(())
    }
    let mut out = vec![];
    walk(dest, &RelPath(vec![]), skip, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// A worktree of `repo` at the execution commit of `m` whose files are the
/// captured bytes.
///
/// The commit holds Git's canonical content (what `HEAD` would hold), not the
/// bytes that were captured: a checkout would convert line endings and run
/// smudge filters over them a second time. So the worktree is made without a
/// checkout, the manifest's bytes are written, and the index is read from the
/// commit; Git then sees the same content as clean that the capture saw.
///
/// The settings of the manifest are compared with what `repo` reads. If they
/// all agree (a run on the machine that captured, in the original itself,
/// always does) no config is written anywhere. If not, they are repeated in
/// the worktree only, but never when `repo` is a repository the manifest was
/// captured from: a run does not write the original's config, so that is
/// `SourceError::OriginalConfigChanged` and nothing is added.
pub fn add_worktree(
    ctx: &ExecContext,
    store: &ObjectStore,
    repo: &Path,
    path: &Path,
    m: &SourceManifest,
) -> Result<()> {
    let Some(commit) = &m.execution_commit else {
        bail!("{} has no execution commit", m.source_id);
    };
    check_filters(ctx, &m.canonical)?;
    // decided before anything is added to `repo`
    let differs = !differing_settings(ctx, repo, &m.canonical).is_empty();
    if differs {
        let keys = differing_settings(ctx, repo, &m.canonical);
        if is_capture_source(ctx, store, repo, m) {
            return Err(SourceError::OriginalConfigChanged { keys }.into());
        }
    }
    crate::git::add_worktree_no_checkout(ctx, repo, path, commit)?;
    if differs {
        apply_canonical(ctx, repo, path, &m.canonical)?;
    }
    materialize_from(store, m, path)?;
    crate::git::git(ctx, path, &["read-tree", "HEAD"])?;
    // stat data for the index, so `git status` does not hash every file
    crate::git::git(ctx, path, &["update-index", "-q", "--refresh"])?;
    // and what `git status` will say: the commit and the bytes must agree
    let pre = ["-C".into(), path.into()];
    let diff = super::capture::git_run(ctx, &pre, &["diff-files", "--name-only", "-z"], &[], None)?;
    let paths: Vec<String> = diff
        .split(|&c| c == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    if !paths.is_empty() {
        return Err(SourceError::WorkspaceNotClean { paths }.into());
    }
    Ok(())
}

/// The program a filter command line starts with, skipping `VAR=value`
/// prefixes.
fn program_of(cmd: &str) -> Option<&str> {
    cmd.split(|c: char| c.is_whitespace() || ";|&()<>".contains(c))
        .find(|t| !t.is_empty() && !t.contains('='))
}

/// Every filter the capture ran needs its program here: without it Git passes
/// the bytes through, and they would not hash to the blobs of the commit.
fn check_filters(ctx: &ExecContext, c: &CanonicalConfig) -> Result<()> {
    for f in &c.filters {
        // `process` takes the place of `clean` when it is set
        let Some(cmd) = f.process.as_ref().or(f.clean.as_ref()) else {
            continue;
        };
        let Some(prog) = program_of(cmd) else {
            continue;
        };
        let found = ctx
            .command("sh")
            .args(["-c", "command -v -- \"$1\" >/dev/null 2>&1", "sh", prog])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !found {
            return Err(SourceError::FilterMissing {
                filter: f.name.clone(),
                command: prog.to_string(),
            }
            .into());
        }
    }
    Ok(())
}

/// The keys of `c` that `repo` does not have the same value for, as its own
/// Git reads them. The attribute rules are compared as bytes, in the order the
/// capture wrote them.
fn differing_settings(ctx: &ExecContext, repo: &Path, c: &CanonicalConfig) -> Vec<String> {
    use super::capture::{attribute_rules, og_str};
    let get = |k: &str| {
        og_str(ctx, repo, &["config", "--get", k])
            .ok()
            .filter(|v| !v.is_empty())
    };
    let mut keys = vec![];
    for (k, want) in [
        ("core.autocrlf", &c.autocrlf),
        ("core.eol", &c.eol),
        ("core.safecrlf", &c.safecrlf),
    ] {
        if &get(k) != want {
            keys.push(k.to_string());
        }
    }
    let git_dir = og_str(
        ctx,
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    let rules = git_dir
        .map(|d| attribute_rules(ctx, repo, Path::new(&d)))
        .unwrap_or_default();
    if super::store::hex(&rules) != c.attributes {
        keys.push("attributes".to_string());
    }
    for f in &c.filters {
        for (var, want) in [
            ("clean", &f.clean),
            ("smudge", &f.smudge),
            ("process", &f.process),
            ("required", &f.required),
        ] {
            let key = format!("filter.{}.{var}", f.name);
            if &get(&key) != want {
                keys.push(key);
            }
        }
    }
    keys
}

/// Whether `repo` is a repository `m` was captured from on this host: the
/// store holds the private repository of its id and common Git directory.
fn is_capture_source(
    ctx: &ExecContext,
    store: &ObjectStore,
    repo: &Path,
    m: &SourceManifest,
) -> bool {
    let Some(id) = &m.repository_id else {
        return false;
    };
    let Ok(common) = super::capture::og_str(
        ctx,
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) else {
        return false;
    };
    store
        .git_dir(&super::capture::private_key(id, &common))
        .join("HEAD")
        .is_file()
}

/// Make Git in the worktree at `path` read the settings that made the blobs,
/// when `repo` does not already read them (see `add_worktree`): they go to the
/// worktree's own config (which needs `extensions.worktreeConfig` in the
/// shared config of `repo`: only a worker's own clone is written that way) and
/// to an attributes file in the worktree's own Git directory.
///
/// One difference remains: Git reads `info/attributes` before the
/// `.gitattributes` files, but the repeated rules sit in
/// `core.attributesFile`, which it reads last. When the two disagree about a
/// path the worktree is not the commit, and that is `WorkspaceNotClean`, never
/// a different content without an error.
fn apply_canonical(ctx: &ExecContext, repo: &Path, path: &Path, c: &CanonicalConfig) -> Result<()> {
    use crate::git::git;
    crate::git::enable_worktree_config(ctx, repo)?;
    let set = |k: &str, v: &str| git(ctx, path, &["config", "--worktree", k, v]).map(|_| ());
    // an unset value is a default, and the worker's own setting must not win
    set("core.autocrlf", c.autocrlf.as_deref().unwrap_or("false"))?;
    set("core.eol", c.eol.as_deref().unwrap_or("native"))?;
    if let Some(v) = &c.safecrlf {
        set("core.safecrlf", v)?;
    }
    let git_dir = git(
        ctx,
        path,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )?;
    let file = Path::new(git_dir.trim()).join("local-ci-attributes");
    let rules = super::store::unhex(&c.attributes).ok_or_else(|| SourceError::Corrupt {
        what: "the attribute rules of the manifest are not hex".into(),
    })?;
    std::fs::write(&file, rules)?;
    set("core.attributesFile", &file.to_string_lossy())?;
    for f in &c.filters {
        for (var, v) in [
            ("clean", &f.clean),
            ("smudge", &f.smudge),
            ("process", &f.process),
            ("required", &f.required),
        ] {
            if let Some(v) = v {
                set(&format!("filter.{}.{var}", f.name), v)?;
            }
        }
    }
    Ok(())
}

/// Put the secret files in `dest`, 0600, after checking each against the HMAC
/// the manifest holds (under `key`). A file whose path would leave `dest`, or
/// that is already there, is refused.
pub fn materialize_secrets(
    m: &SourceManifest,
    bundle: &SecretBundle,
    key: &[u8],
    dest: &Path,
) -> Result<()> {
    let want: BTreeSet<&RelPath> = m.secret_files.iter().map(|s| &s.path).collect();
    let have: BTreeSet<&RelPath> = bundle.paths().into_iter().collect();
    if want != have {
        return Err(SourceError::Corrupt {
            what: "the secret bundle does not hold the files the manifest names".into(),
        }
        .into());
    }
    let mut staged = vec![];
    for f in &bundle.files {
        check_safe(&f.path)?;
        let content = bundle.content(f)?;
        let entry = m.secret_files.iter().find(|s| s.path == f.path).unwrap();
        if secret::secret_digest(key, &f.path, &content) != entry.hmac {
            return Err(SourceError::Corrupt {
                what: format!("secret file {} does not match its HMAC", f.path.display()),
            }
            .into());
        }
        staged.push((f.path.clone(), content));
    }
    for (path, content) in staged {
        let target = prepare_parent(dest, &path)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)
            .with_context(|| format!("cannot place secret file {}", path.display()))?;
        f.write_all(&content)?;
        drop(f);
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
