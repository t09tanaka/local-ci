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
    crate::git::add_worktree_no_checkout(ctx, repo, path, commit)?;
    materialize_from(store, m, path)?;
    crate::git::git(ctx, path, &["read-tree", "HEAD"])?;
    // stat data for the index, so `git status` does not hash every file; a
    // filter that cannot run here only makes it do so
    let _ = crate::git::git(ctx, path, &["update-index", "-q", "--refresh"]);
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
