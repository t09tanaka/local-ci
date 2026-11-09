//! Moving the Git objects of a capture to another clone.

use super::capture::{git_run, pg};
use super::store::ObjectStore;
use super::*;
use crate::context::ExecContext;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

fn is_hex40(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// The ids in `m` that end up on a Git command line or in a ref name: a
/// manifest from elsewhere is not trusted to hold nothing but ids.
fn check_ids(m: &SourceManifest) -> Result<()> {
    let id_ok = m.source_id.len() == 36
        && m.source_id.starts_with("src-")
        && m.source_id[4..]
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
    if !id_ok {
        bail!("not a source id: {:?}", m.source_id);
    }
    for sha in m.execution_commit.iter().chain(m.merge_base.iter()) {
        if !is_hex40(sha) {
            bail!("not a commit id: {sha:?}");
        }
    }
    Ok(())
}

fn snapshot_ref(source_id: &str) -> String {
    format!("refs/local-ci/snapshots/{source_id}")
}

/// The private repository that holds the snapshot ref of `m`.
fn holder(ctx: &ExecContext, store: &ObjectStore, m: &SourceManifest) -> Result<PathBuf> {
    let r = snapshot_ref(&m.source_id);
    for ent in std::fs::read_dir(store.root().join("git"))? {
        let dir = ent?.path();
        if pg(ctx, &dir, &["rev-parse", "--verify", "-q", &r], &[], None).is_ok() {
            return Ok(dir);
        }
    }
    bail!("the Git objects of {} are not in this store", m.source_id)
}

/// The manifests of the submodules below `m` whose commit is a snapshot of
/// their own (a dirty submodule), nested ones included. A submodule whose
/// manifest is not in the store is an error: the bundle would be missing its
/// objects.
fn dirty_submodules(store: &ObjectStore, m: &SourceManifest) -> Result<Vec<SourceManifest>> {
    let mut out = vec![];
    for s in &m.submodules {
        let Some(id) = &s.source_id else { continue };
        let nested = store.load_manifest(id)?.with_context(|| {
            format!(
                "submodule {}: manifest {id} is not in the store",
                s.path.display()
            )
        })?;
        if s.dirty {
            check_ids(&nested)?;
            out.push(nested.clone());
        }
        out.extend(dirty_submodules(store, &nested)?);
    }
    Ok(out)
}

/// Write the commits `m` needs to a `git bundle`: the synthetic (or given)
/// commit, the unpushed history below it, down to the merge base, which the
/// receiving clone must have. Returns the path of the bundle (in the store).
///
/// The commits of a dirty submodule (the ones its gitlink names) are in the
/// same bundle, with their whole history, so nothing has to be fetched from
/// anywhere else.
pub fn export_objects(ctx: &ExecContext, m: &SourceManifest) -> Result<PathBuf> {
    export_from(ctx, &default_store(ctx)?, m)
}

pub fn export_from(ctx: &ExecContext, store: &ObjectStore, m: &SourceManifest) -> Result<PathBuf> {
    check_ids(m)?;
    let Some(commit) = &m.execution_commit else {
        bail!("{} has no Git objects ({:?})", m.source_id, m.source_kind);
    };
    let dir = holder(ctx, store, m)?;
    let mut refs = vec![snapshot_ref(&m.source_id)];
    for sub in dirty_submodules(store, m)? {
        let r = snapshot_ref(&sub.source_id);
        let from = holder(ctx, store, &sub)?;
        if from != dir {
            pg(
                ctx,
                &dir,
                &[
                    "fetch",
                    "-q",
                    "--no-tags",
                    "--no-write-fetch-head",
                    &from.to_string_lossy(),
                    &format!("+{r}:{r}"),
                ],
                &[],
                None,
            )
            .with_context(|| format!("cannot bring the objects of {} along", sub.source_id))?;
        }
        refs.push(r);
    }
    let out_dir = store.root().join("bundles");
    std::fs::create_dir_all(&out_dir)?;
    let dest = out_dir.join(format!("{}.bundle", m.source_id));
    let tmp = super::store::tmp_name(&out_dir, "bundle");
    let mut args: Vec<String> = vec![
        "bundle".into(),
        "create".into(),
        "-q".into(),
        tmp.to_string_lossy().into_owned(),
    ];
    args.extend(refs);
    if let Some(mb) = &m.merge_base {
        args.push(format!("^{mb}"));
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = pg(ctx, &dir, &refs, &[], None);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot bundle {commit}"));
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Bring the objects of a bundle into `clone_dir` (the worker's clone of the
/// repository) and check that the execution commit is there, with the tree
/// the manifest recorded. Nothing is fetched from the network: the bundle's
/// prerequisites (the merge base) have to be in the clone already.
pub fn import_objects(
    ctx: &ExecContext,
    clone_dir: &Path,
    bundle: &Path,
    m: &SourceManifest,
) -> Result<String> {
    // a manifest nobody has verified is not taken for what it says
    super::capture::check_digest(m)?;
    check_ids(m)?;
    let Some(commit) = &m.execution_commit else {
        bail!("{} has no Git objects", m.source_id);
    };
    let pre = ["-C".into(), clone_dir.into()];
    git_run(
        ctx,
        &pre,
        &["bundle", "verify", "-q", &bundle.to_string_lossy()],
        &[],
        None,
    )
    .context("the bundle does not fit this clone (its base is missing)")?;
    // the snapshot of `m`, and those of its dirty submodules
    let refspec = "+refs/local-ci/snapshots/*:refs/local-ci/snapshots/*";
    git_run(
        ctx,
        &pre,
        &[
            "fetch",
            "-q",
            "--no-tags",
            "--no-write-fetch-head",
            &bundle.to_string_lossy(),
            refspec,
        ],
        &[],
        None,
    )?;
    let got = String::from_utf8_lossy(&git_run(
        ctx,
        &pre,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
        &[],
        None,
    )?)
    .trim()
    .to_string();
    if let Some(tree) = &m.worktree_tree {
        let t = String::from_utf8_lossy(&git_run(
            ctx,
            &pre,
            &["rev-parse", &format!("{commit}^{{tree}}")],
            &[],
            None,
        )?)
        .trim()
        .to_string();
        if &t != tree {
            return Err(SourceError::Corrupt {
                what: format!("commit {commit} has tree {t}, the manifest says {tree}"),
            }
            .into());
        }
    }
    // a dirty submodule's commit is what the gitlink names: it has come along
    for sub in m.submodules.iter().filter(|s| s.dirty) {
        let path = String::from_utf8_lossy(&sub.path.0).into_owned();
        let line = String::from_utf8_lossy(&git_run(
            ctx,
            &pre,
            &["ls-tree", commit, "--", &path],
            &[],
            None,
        )?)
        .into_owned();
        let sha = line.split_whitespace().nth(2).unwrap_or("");
        if !is_hex40(sha)
            || git_run(
                ctx,
                &pre,
                &["cat-file", "-e", &format!("{sha}^{{commit}}")],
                &[],
                None,
            )
            .is_err()
        {
            return Err(SourceError::Corrupt {
                what: format!(
                    "the commit of submodule {} is not in the bundle",
                    sub.path.display()
                ),
            }
            .into());
        }
    }
    Ok(got)
}
