//! Moving the Git objects of a capture to another clone.

use super::capture::{git_run, pg};
use super::store::{write_atomic, ObjectStore};
use super::*;
use crate::context::ExecContext;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

fn snapshot_ref(m: &SourceManifest) -> String {
    format!("refs/local-ci/snapshots/{}", m.source_id)
}

/// The private repository that holds the snapshot ref of `m`.
fn holder(ctx: &ExecContext, store: &ObjectStore, m: &SourceManifest) -> Result<PathBuf> {
    let r = snapshot_ref(m);
    for ent in std::fs::read_dir(store.root().join("git"))? {
        let dir = ent?.path();
        if pg(ctx, &dir, &["rev-parse", "--verify", "-q", &r], &[], None).is_ok() {
            return Ok(dir);
        }
    }
    bail!("the Git objects of {} are not in this store", m.source_id)
}

/// Write the commits `m` needs to a `git bundle`: the synthetic (or given)
/// commit, the unpushed history below it, down to the merge base, which the
/// receiving clone must have. Returns the path of the bundle (in the store).
///
/// A submodule has a bundle of its own; export it from its own manifest.
pub fn export_objects(ctx: &ExecContext, m: &SourceManifest) -> Result<PathBuf> {
    export_from(ctx, &default_store(ctx)?, m)
}

pub fn export_from(ctx: &ExecContext, store: &ObjectStore, m: &SourceManifest) -> Result<PathBuf> {
    let Some(commit) = &m.execution_commit else {
        bail!("{} has no Git objects ({:?})", m.source_id, m.source_kind);
    };
    let dir = holder(ctx, store, m)?;
    let out_dir = store.root().join("bundles");
    std::fs::create_dir_all(&out_dir)?;
    let dest = out_dir.join(format!("{}.bundle", m.source_id));
    let tmp = super::store::tmp_name(&out_dir, "bundle");
    let mut args: Vec<String> = vec![
        "bundle".into(),
        "create".into(),
        "-q".into(),
        tmp.to_string_lossy().into_owned(),
        snapshot_ref(m),
    ];
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
    let r = snapshot_ref(m);
    git_run(
        ctx,
        &pre,
        &[
            "fetch",
            "-q",
            "--no-tags",
            "--no-write-fetch-head",
            &bundle.to_string_lossy(),
            &format!("+{r}:{r}"),
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
    let _ = write_atomic;
    Ok(got)
}
