//! Moving the Git objects of a capture to another clone.

use super::capture::{git_run, pg};
use super::store::ObjectStore;
use super::*;
use crate::context::ExecContext;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn is_hex40(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn check_source_id(id: &str) -> Result<()> {
    let ok = id.len() == 36
        && id.starts_with("src-")
        && id[4..]
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
    if !ok {
        bail!("not a source id: {id:?}");
    }
    Ok(())
}

/// Run Git for a question it answers with its exit code: `Some` (what it
/// printed) for 0, `None` for 1. Anything else it fails with is an error,
/// not a "no".
fn git_says(
    ctx: &ExecContext,
    pre: &[std::ffi::OsString],
    args: &[&str],
) -> Result<Option<Vec<u8>>> {
    let out = ctx
        .command("git")
        .args(pre)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .context("failed to run git")?;
    match out.status.code() {
        Some(0) => Ok(Some(out.stdout)),
        Some(1) => Ok(None),
        _ => bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// The commit the ref `r` is at, `None` when there is no such ref. A Git
/// failure is an error, not a ref that is not there.
fn ref_value(ctx: &ExecContext, pre: &[std::ffi::OsString], r: &str) -> Result<Option<String>> {
    Ok(git_says(ctx, pre, &["rev-parse", "--verify", "-q", r])?
        .map(|o| String::from_utf8_lossy(&o).trim().to_string()))
}

/// The ids in `m` that end up on a Git command line or in a ref name: a
/// manifest from elsewhere is not trusted to hold nothing but ids.
fn check_ids(m: &SourceManifest) -> Result<()> {
    check_source_id(&m.source_id)?;
    for sha in m.execution_commit.iter().chain(m.merge_base.iter()) {
        if !is_hex40(sha) {
            bail!("not a commit id: {sha:?}");
        }
    }
    if let Some(t) = m
        .index_tree
        .iter()
        .chain(m.worktree_tree.iter())
        .find(|t| !is_hex40(t))
    {
        bail!("not a tree id: {t:?}");
    }
    Ok(())
}

/// The ref that holds the commit wrapped around the index tree of a source.
fn index_ref(source_id: &str) -> String {
    format!("refs/local-ci/index/{source_id}")
}

fn snapshot_ref(source_id: &str) -> String {
    format!("refs/local-ci/snapshots/{source_id}")
}

/// Point `r` at `c` unless it does already. Two exports of one source may run
/// at once: the value is the same (the commit is fixed by the tree), so only
/// the write is contended, and a write that lost the ref's lock is tried once
/// more.
fn set_ref_once(ctx: &ExecContext, dir: &Path, r: &str, c: &str) -> Result<()> {
    let current = |ctx: &ExecContext| {
        pg(ctx, dir, &["rev-parse", "--verify", "-q", r], &[], None)
            .map(|o| String::from_utf8_lossy(&o).trim().to_string())
            .unwrap_or_default()
    };
    if current(ctx) == c {
        return Ok(());
    }
    match pg(ctx, dir, &["update-ref", r, c], &[], None) {
        Ok(_) => Ok(()),
        Err(_) if current(ctx) == c => Ok(()),
        Err(_) => pg(ctx, dir, &["update-ref", r, c], &[], None).map(|_| ()),
    }
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

/// Forget what [`import_objects`] put in `clone_dir` for the source
/// `source_id`: its snapshot ref and its index ref (the objects go with the
/// clone's next `git gc`). A ref that is not there is not an error; a Git
/// failure while a ref is looked for is one. The
/// caller decides when: nothing here knows whether another task of the same
/// source still runs, so the worker that owns the clone calls it from its own
/// garbage collection.
pub fn remove_imported_refs(ctx: &ExecContext, clone_dir: &Path, source_id: &str) -> Result<()> {
    check_source_id(source_id)?;
    let pre = ["-C".into(), clone_dir.into()];
    for r in [snapshot_ref(source_id), index_ref(source_id)] {
        if ref_value(ctx, &pre, &r)?.is_some() {
            git_run(ctx, &pre, &["update-ref", "-d", &r], &[], None)?;
        }
    }
    Ok(())
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
    export_impl(ctx, store, m, None)
}

/// The most commits [`export_with_basis`] takes as `have`: a longer list is
/// refused, since every entry costs Git calls on the exporting host.
pub const MAX_HAVE: usize = 256;

/// The most manifests [`import_objects_for`] takes as `nested`: a longer
/// list is refused.
pub const MAX_NESTED: usize = 256;

/// The most dirty submodule entries an import walks, over the manifest it is
/// given and all the nested ones (from `nested` or from the store): with more
/// the import is refused before Git runs. Each entry costs Git calls on the
/// importing host.
pub const MAX_DIRTY_SUBMODULES: usize = 1024;

/// Like [`export_from`], but cut at the commits the receiving host says it
/// has (`have`) instead of at the merge base with origin: the bundle holds
/// what is reachable from the refs of `m` and not from the entries of `have`
/// that this store knows. An empty `have` (or one this store knows nothing
/// of) gives the whole history of the execution commit. Every entry must be
/// a 40-digit lowercase hex commit id, and there may be [`MAX_HAVE`] of them
/// at most, or the call is refused before Git runs. An entry given twice
/// counts once.
///
/// A ref of the bundle (the snapshot of an unchanged dirty submodule, say)
/// whose tip is reachable from an entry of `have` would be dropped by Git.
/// Then that entry is not used and the parents of the tip are excluded
/// instead, so the ref stays in the bundle. The exclusions of a bundle are
/// not per ref: an entry that is not used is not used for any ref, so the
/// other refs of the bundle bring the history that entry would have cut. If
/// a ref is still missing after that (its tip lies below the parent of
/// another covered tip), the bundle is written once more with no basis at
/// all, that is with the whole history; only if a ref is missing even then
/// does the export fail.
///
/// The bundle's name depends on `have` (not on its order, nor on entries
/// given twice): it is the source id, a `-`, and the first 32 hex digits of
/// the SHA-256 of the sorted entries. So exports with different bases of one
/// source do not replace each other, and every distinct `have` leaves one
/// file under `bundles/` of the store. Nothing here removes or counts them:
/// the caller owns the cleanup. A caller that takes `have` from a network
/// peer should bound how many distinct bases it accepts for one source, or
/// the peer decides how many bundles (each up to the whole history) are
/// written.
pub fn export_with_basis(
    ctx: &ExecContext,
    store: &ObjectStore,
    m: &SourceManifest,
    have: &[String],
) -> Result<PathBuf> {
    if have.len() > MAX_HAVE {
        bail!(
            "too many commits in have: {} (at most {MAX_HAVE})",
            have.len()
        );
    }
    if let Some(h) = have.iter().find(|h| !is_hex40(h)) {
        bail!("not a commit id: {h:?}");
    }
    let mut have = have.to_vec();
    have.sort();
    have.dedup();
    export_impl(ctx, store, m, Some(&have))
}

/// Whether `a` is an ancestor of `b` (or `b` itself). Git says "no" with the
/// exit code 1; anything else it fails with is an error, not a "no".
fn is_ancestor(ctx: &ExecContext, dir: &Path, a: &str, b: &str) -> Result<bool> {
    let pre = ["--git-dir".into(), dir.into()];
    Ok(git_says(ctx, &pre, &["merge-base", "--is-ancestor", a, b])?.is_some())
}

/// The `^` arguments of a bundle made over `refs` with the basis `have`
/// (sorted, no entry twice).
fn basis_exclusions(
    ctx: &ExecContext,
    dir: &Path,
    refs: &[String],
    have: &[String],
) -> Result<Vec<String>> {
    let mut tips = vec![];
    for r in refs {
        let o = pg(ctx, dir, &["rev-parse", "--verify", r], &[], None)?;
        tips.push(String::from_utf8_lossy(&o).trim().to_string());
    }
    let mut out: Vec<String> = vec![];
    let mut below: Vec<&String> = vec![];
    for h in have {
        let known = pg(
            ctx,
            dir,
            &["cat-file", "-e", &format!("{h}^{{commit}}")],
            &[],
            None,
        )
        .is_ok();
        if !known {
            continue;
        }
        let mut covers: Vec<&String> = vec![];
        for t in &tips {
            if is_ancestor(ctx, dir, t, h)? {
                covers.push(t);
            }
        }
        if covers.is_empty() {
            out.push(format!("^{h}"));
        } else {
            below.extend(covers);
        }
    }
    below.sort();
    below.dedup();
    for t in below {
        let o = pg(ctx, dir, &["rev-list", "--parents", "-n1", t], &[], None)?;
        let line = String::from_utf8_lossy(&o).into_owned();
        out.extend(line.split_whitespace().skip(1).map(|p| format!("^{p}")));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// The lines of `git bundle list-heads`: (commit, ref).
fn bundle_heads(
    ctx: &ExecContext,
    pre: &[std::ffi::OsString],
    bundle: &Path,
) -> Result<Vec<(String, String)>> {
    let o = git_run(
        ctx,
        pre,
        &["bundle", "list-heads", &bundle.to_string_lossy()],
        &[],
        None,
    )?;
    Ok(String::from_utf8_lossy(&o)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(c, r)| (c.to_string(), r.to_string()))
        .collect())
}

fn export_impl(
    ctx: &ExecContext,
    store: &ObjectStore,
    m: &SourceManifest,
    have: Option<&[String]>,
) -> Result<PathBuf> {
    check_ids(m)?;
    let Some(commit) = &m.execution_commit else {
        bail!("{} has no Git objects ({:?})", m.source_id, m.source_kind);
    };
    let dir = holder(ctx, store, m)?;
    let mut refs = vec![snapshot_ref(&m.source_id)];
    // The index tree (what is staged) goes along in a commit of its own, so
    // that the worker can tell what was staged (`git diff --cached`).
    if let Some(tree) = &m.index_tree {
        pg(
            ctx,
            &dir,
            &["cat-file", "-e", &format!("{tree}^{{tree}}")],
            &[],
            None,
        )
        .with_context(|| format!("the index tree of {} is not in this store", m.source_id))?;
        let c = super::capture::index_commit(ctx, &dir, tree)?;
        let r = index_ref(&m.source_id);
        set_ref_once(ctx, &dir, &r, &c)?;
        refs.push(r);
    }
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
        if !refs.contains(&r) {
            refs.push(r);
        }
    }
    let out_dir = store.root().join("bundles");
    std::fs::create_dir_all(&out_dir)?;
    let name = match have {
        None => format!("{}.bundle", m.source_id),
        Some(h) => {
            use sha2::{Digest, Sha256};
            // `h` is sorted and holds no entry twice
            let d = Sha256::digest(h.join("\n"));
            let hex: String = d.iter().take(16).map(|b| format!("{b:02x}")).collect();
            format!("{}-{hex}.bundle", m.source_id)
        }
    };
    let dest = out_dir.join(name);
    let tmp = super::store::tmp_name(&out_dir, "bundle");
    // Write the bundle of `refs` with the exclusions `excl` to `tmp`.
    let write = |excl: &[String]| -> Result<()> {
        let mut args: Vec<String> = vec![
            "bundle".into(),
            "create".into(),
            "-q".into(),
            tmp.to_string_lossy().into_owned(),
        ];
        args.extend(refs.iter().cloned());
        args.extend(excl.iter().cloned());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Err(e) = pg(ctx, &dir, &argv, &[], None) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("cannot bundle {commit}"));
        }
        Ok(())
    };
    // Git leaves out a ref its exclusions cover; a bundle that lacks one is
    // never handed on
    // (a bundle whose refs Git cannot list is an error, not one that lacks
    // them)
    let complete = || -> Result<bool> {
        let pre = ["--git-dir".into(), dir.clone().into()];
        let heads = bundle_heads(ctx, &pre, &tmp).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(refs.iter().all(|r| heads.iter().any(|(_, h)| h == r)))
    };
    match have {
        None => {
            let excl: Vec<String> = m.merge_base.iter().map(|mb| format!("^{mb}")).collect();
            write(&excl)?;
        }
        Some(h) => {
            write(&basis_exclusions(ctx, &dir, &refs, h)?)?;
            if !complete()? {
                // the whole history instead: larger, but nothing is missing
                write(&[])?;
                if !complete()? {
                    let _ = std::fs::remove_file(&tmp);
                    bail!("the bundle of {} lacks a ref it needs", m.source_id);
                }
            }
        }
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Where an import learns of the dirty submodules below the ones `m` names.
enum Nested<'a> {
    /// From the manifests in this host's store, as far as it has them.
    Store(Option<&'a ObjectStore>),
    /// From these manifests and nothing else: the dirty submodules below `m`,
    /// each exactly once.
    Given(Vec<&'a SourceManifest>),
}

/// Bring the objects of a bundle into `clone_dir` (the worker's clone of the
/// repository) and check that the execution commit is there, with the tree
/// the manifest recorded. Nothing is fetched from the network: the bundle's
/// prerequisites (the merge base) have to be in the clone already.
///
/// The digest of `m` is checked, and it has no key: it shows that the
/// manifest holds together (nothing in it was changed after it was sealed),
/// not who made it. Anyone can seal a manifest of their own. Whether `m`
/// (and the bundle) come from where the caller thinks is for the caller to
/// establish before it calls this.
///
/// What is written and what is checked:
///
/// - Only the refs of the source are taken from the bundle: the snapshot ref
///   of `m`, the snapshot refs of its dirty submodules, and the index ref of
///   `m` when the manifest names an index tree. Each must be in the bundle
///   exactly once. They are fetched without force, all or none.
/// - The snapshot ref of `m` must be the execution commit of `m`, in the
///   bundle and afterwards in the clone. A descendant of it is refused like
///   any other commit.
/// - The manifest names the index tree, not the commit around it. So the
///   index ref is held to the commit the bundle lists for it, and of that
///   commit two things are checked: its tree is the index tree of `m`, and
///   it has no parent. Its author, dates and message are not checked.
/// - A ref that is in the clone already must be at the commit this import
///   would set it to; with another value the import is refused, before
///   anything is fetched, and the ref is left as it is. Taking the same
///   source a second time is therefore fine.
/// - A manifest holds [`MAX_DIRTY_SUBMODULES`] dirty submodule entries at
///   most, the nested manifests counted in, and the path of each is a plain
///   relative path (not empty, no `/` at its end, no `..`, no NUL or
///   newline); otherwise the import is refused before Git runs. The path of
///   a dirty submodule must be a gitlink in the tree of its commit, and the
///   commit it names must have come along.
///
/// When the import is refused, for whatever reason, the clone holds no ref
/// under `refs/local-ci/` that it did not hold before the call, and none of
/// those it held has moved. The objects a refused bundle brought stay until
/// the clone's next `git gc`.
///
/// The fetch is `git fetch --atomic`, so Git 2.31 or later is needed. Two
/// imports of one source into one clone at once are not kept apart by a lock
/// of their own: one whose fetch loses a ref's lock to the other fails, and
/// writes and removes nothing.
///
/// The dirty submodules below the ones `m` names are known only as far as
/// this host's store has their manifests; a host that has no store uses
/// [`import_objects_for`]. The snapshot ref of a nested dirty submodule is
/// held to the commit the bundle lists for it here (there is no manifest
/// that was sent along to hold it to).
pub fn import_objects(
    ctx: &ExecContext,
    clone_dir: &Path,
    bundle: &Path,
    m: &SourceManifest,
) -> Result<String> {
    let store = default_store(ctx).ok();
    import_impl(ctx, clone_dir, bundle, m, Nested::Store(store.as_ref()))
}

/// Like [`import_objects`], for a receiving host that was sent the manifests
/// of the dirty submodules below `m` (`nested`, at every depth) instead of
/// having them in a store. The store of this host is not asked. Everything
/// [`import_objects`] says of what is checked, and of what a refusal leaves
/// behind, holds here too.
///
/// `nested` is not trusted any more than the bundle is, and like that of `m`
/// the digest of each is unkeyed: it shows a manifest that holds together,
/// not one that comes from the host the caller expects. Who sent `m`,
/// `nested` and the bundle is for the caller to establish.
///
/// - `nested` holds [`MAX_NESTED`] manifests at most, each with ids and a
///   digest that hold;
/// - every dirty submodule of `m`, and of each manifest reached from it,
///   must have its manifest in `nested`, and `nested` must hold no manifest
///   that is not reached that way;
/// - the snapshot ref of each must be the execution commit its manifest
///   names, in the bundle and afterwards in the clone: a ref that is already
///   in the clone with another value is refused and left as it is;
/// - the tree of that commit must be the worktree tree its manifest names,
///   and the gitlink of every dirty submodule, in `m` and in each of
///   `nested`, must be the execution commit of that submodule's manifest.
pub fn import_objects_for(
    ctx: &ExecContext,
    clone_dir: &Path,
    bundle: &Path,
    m: &SourceManifest,
    nested: &[SourceManifest],
) -> Result<String> {
    if nested.len() > MAX_NESTED {
        bail!(
            "too many nested manifests: {} (at most {MAX_NESTED})",
            nested.len()
        );
    }
    let given = reached(m, nested)?;
    import_impl(ctx, clone_dir, bundle, m, Nested::Given(given))
}

/// The manifests of `nested` in the order they are reached from the dirty
/// submodules of `m`. One that is needed and not there is an error, and so
/// is one that is there and not needed.
fn reached<'a>(
    m: &SourceManifest,
    nested: &'a [SourceManifest],
) -> Result<Vec<&'a SourceManifest>> {
    let mut by_id = std::collections::BTreeMap::new();
    for n in nested {
        check_ids(n)?;
        super::capture::check_digest(n)?;
        if n.execution_commit.is_none() || n.worktree_tree.is_none() {
            bail!("the nested manifest {} has no Git objects", n.source_id);
        }
        by_id.insert(n.source_id.as_str(), n);
    }
    let mut out: Vec<&SourceManifest> = vec![];
    let mut seen = std::collections::BTreeSet::new();
    let mut todo: Vec<&[SubmoduleRef]> = vec![&m.submodules];
    while let Some(subs) = todo.pop() {
        for s in subs.iter().filter(|s| s.dirty) {
            let Some(id) = &s.source_id else { continue };
            check_source_id(id)?;
            let Some(n) = by_id.get(id.as_str()).copied() else {
                bail!(
                    "submodule {}: the manifest of {id} was not given",
                    s.path.display()
                );
            };
            // a manifest seen before is not walked again (nor can a ring of
            // them go on for ever)
            if seen.insert(id.as_str()) {
                out.push(n);
                todo.push(&n.submodules);
            }
        }
    }
    if let Some(id) = by_id.keys().find(|id| !seen.contains(*id)) {
        bail!(
            "the manifest {id} is not of a dirty submodule below {}",
            m.source_id
        );
    }
    Ok(out)
}

/// A ref an import sets.
struct Planned {
    name: String,
    /// The commit the bundle has it at (and, where a manifest names one, the
    /// commit the manifest names).
    commit: String,
    /// Whether the ref was in the clone before the import (with that value).
    was_here: bool,
}

fn corrupt(what: String) -> anyhow::Error {
    SourceError::Corrupt { what }.into()
}

/// Count one dirty submodule entry of a manifest toward
/// [`MAX_DIRTY_SUBMODULES`] and look at its path. No Git runs here.
fn admit(entries: &mut usize, s: &SubmoduleRef) -> Result<()> {
    *entries += 1;
    if *entries > MAX_DIRTY_SUBMODULES {
        bail!("too many dirty submodules (at most {MAX_DIRTY_SUBMODULES})");
    }
    check_submodule_path(&s.path)
}

/// The path of a submodule in a manifest from elsewhere, before it is given
/// to Git: relative, not empty, no `/` at its end (Git would list what is
/// below it), no `..`, and neither a NUL nor a newline in it.
fn check_submodule_path(p: &RelPath) -> Result<()> {
    let b = p.0.as_slice();
    let bad = b.is_empty()
        || b.first() == Some(&b'/')
        || b.last() == Some(&b'/')
        || b.iter().any(|c| *c == 0 || *c == b'\n')
        || b.split(|c| *c == b'/').any(|part| part == b"..");
    if bad {
        return Err(corrupt(format!(
            "not a submodule path: {:?}",
            String::from_utf8_lossy(b)
        )));
    }
    Ok(())
}

/// What the store of this host tells of the dirty submodules below `m`: the
/// source ids of all of them (those of a manifest the store has included),
/// and the manifests the store has, by id. Reads files only.
fn store_walk(
    m: &SourceManifest,
    store: Option<&ObjectStore>,
) -> Result<(Vec<String>, BTreeMap<String, SourceManifest>)> {
    let mut ids = BTreeSet::new();
    let mut known = BTreeMap::new();
    let mut entries = 0;
    let mut todo = vec![m.submodules.clone()];
    while let Some(subs) = todo.pop() {
        for s in subs.iter().filter(|s| s.dirty) {
            admit(&mut entries, s)?;
            let Some(id) = &s.source_id else { continue };
            // an id that is not a source id is refused: it ends up in a ref
            // name
            check_source_id(id)?;
            // a manifest seen before is not walked again
            if !ids.insert(id.clone()) {
                continue;
            }
            if let Some(n) = store.map(|st| st.load_manifest(id)).transpose()?.flatten() {
                check_ids(&n)?;
                todo.push(n.submodules.clone());
                known.insert(id.clone(), n);
            }
        }
    }
    Ok((ids.into_iter().collect(), known))
}

fn import_impl(
    ctx: &ExecContext,
    clone_dir: &Path,
    bundle: &Path,
    m: &SourceManifest,
    nested: Nested,
) -> Result<String> {
    // a manifest nobody has verified is not taken for what it says
    super::capture::check_digest(m)?;
    check_ids(m)?;
    let Some(commit) = &m.execution_commit else {
        bail!("{} has no Git objects", m.source_id);
    };
    // the snapshot of `m`, those of its dirty submodules, and the index
    // commit: named one by one, so a bundle cannot put another ref in the
    // clone. With each, the commit a manifest names for it (none for the
    // index commit, nor for a submodule known by its id alone).
    let own = snapshot_ref(&m.source_id);
    let mut wanted: Vec<(String, Option<&str>)> = vec![(own.clone(), Some(commit.as_str()))];
    // Before Git runs: the dirty submodule entries are counted and their
    // paths looked at.
    let known = match &nested {
        Nested::Store(store) => {
            let (ids, known) = store_walk(m, *store)?;
            for id in ids.iter().filter(|id| **id != m.source_id) {
                wanted.push((snapshot_ref(id), None));
            }
            known
        }
        Nested::Given(list) => {
            let mut entries = 0;
            for s in std::iter::once(m)
                .chain(list.iter().copied())
                .flat_map(|n| n.submodules.iter().filter(|s| s.dirty))
            {
                admit(&mut entries, s)?;
            }
            for n in list.iter().filter(|n| n.source_id != m.source_id) {
                wanted.push((snapshot_ref(&n.source_id), n.execution_commit.as_deref()));
            }
            BTreeMap::new()
        }
    };
    if m.index_tree.is_some() {
        wanted.push((index_ref(&m.source_id), None));
    }
    let pre = ["-C".into(), clone_dir.into()];
    git_run(
        ctx,
        &pre,
        &["bundle", "verify", "-q", &bundle.to_string_lossy()],
        &[],
        None,
    )
    .context("the bundle does not fit this clone (its base is missing)")?;
    // What the fetch below would write is looked at first: a ref the bundle
    // lacks or has twice, one it has at a commit the manifest does not name,
    // and one that is in the clone with another value are refused while the
    // clone is still as it was.
    let heads = bundle_heads(ctx, &pre, bundle)?;
    let mut plan: Vec<Planned> = vec![];
    for (r, named) in &wanted {
        let mut found = heads
            .iter()
            .filter(|(_, h)| h == r)
            .map(|(c, _)| c.as_str());
        let at = match (found.next(), found.next()) {
            (Some(c), None) => c,
            (None, _) => {
                let hint = if r.starts_with("refs/local-ci/index/") {
                    " (a bundle made without the index tree has no index ref)"
                } else {
                    ""
                };
                return Err(corrupt(format!("the bundle has no {r}{hint}")));
            }
            (Some(_), Some(_)) => {
                return Err(corrupt(format!("the bundle has {r} more than once")));
            }
        };
        if !is_hex40(at) {
            return Err(corrupt(format!("the bundle has {r} at {at:?}")));
        }
        if let Some(want) = named.filter(|w| *w != at) {
            return Err(corrupt(format!(
                "the bundle has {r} at {at}, its manifest says {want}"
            )));
        }
        let here = ref_value(ctx, &pre, r)?;
        if let Some(c) = here.as_deref().filter(|c| *c != at) {
            return Err(corrupt(match named {
                Some(want) => format!("{r} is already here at {c}, its manifest says {want}"),
                None => format!("{r} is already here at {c}, the bundle has it at {at}"),
            }));
        }
        plan.push(Planned {
            name: r.clone(),
            commit: at.to_string(),
            was_here: here.is_some(),
        });
    }
    // Without force, and all refs or none: a fetch that fails has written no
    // ref.
    let bundle_path = bundle.to_string_lossy();
    let refspecs: Vec<String> = plan.iter().map(|p| format!("{0}:{0}", p.name)).collect();
    let mut fetch = vec![
        "fetch",
        "-q",
        "--no-tags",
        "--no-write-fetch-head",
        "--atomic",
    ];
    fetch.push(&bundle_path);
    fetch.extend(refspecs.iter().map(String::as_str));
    git_run(ctx, &pre, &fetch, &[], None).context("cannot fetch the refs of the bundle")?;
    let checked = check_imported(ctx, &pre, m, commit, &nested, &known, &plan);
    if checked.is_err() {
        // what was refused is not left behind: the refs this call made go
        // (each only while it is still what this call set it to)
        for p in plan.iter().filter(|p| !p.was_here) {
            let _ = git_run(
                ctx,
                &pre,
                &["update-ref", "-d", &p.name, &p.commit],
                &[],
                None,
            );
        }
    }
    checked
}

/// What an import looks at once the fetch is done. Returns the execution
/// commit.
fn check_imported(
    ctx: &ExecContext,
    pre: &[std::ffi::OsString],
    m: &SourceManifest,
    commit: &str,
    nested: &Nested,
    known: &BTreeMap<String, SourceManifest>,
    plan: &[Planned],
) -> Result<String> {
    // every ref is the commit that was looked at before the fetch
    for p in plan {
        let at = ref_value(ctx, pre, &p.name)?.unwrap_or_default();
        if at != p.commit {
            return Err(corrupt(format!(
                "{} is {at:?} after the fetch, not {}",
                p.name, p.commit
            )));
        }
    }
    let got = String::from_utf8_lossy(&git_run(
        ctx,
        pre,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
        &[],
        None,
    )?)
    .trim()
    .to_string();
    if let Some(tree) = &m.worktree_tree {
        check_tree(ctx, pre, commit, tree)?;
    }
    // the index tree came along (in the commit of its own ref) and is the one
    // the manifest names
    if let Some(tree) = &m.index_tree {
        let c = git_run(
            ctx,
            pre,
            &[
                "rev-parse",
                "--verify",
                &format!("{}^{{tree}}", index_ref(&m.source_id)),
            ],
            &[],
            None,
        )
        .map_err(|_| SourceError::Corrupt {
            what: format!("the bundle has no index tree for {}", m.source_id),
        })?;
        let got = String::from_utf8_lossy(&c).trim().to_string();
        if &got != tree {
            return Err(corrupt(format!(
                "the index tree is {got}, the manifest says {tree}"
            )));
        }
        // a commit around the tree and nothing more: no history rides along
        let n = git_run(
            ctx,
            pre,
            &["rev-list", "--count", &index_ref(&m.source_id)],
            &[],
            None,
        )?;
        if String::from_utf8_lossy(&n).trim() != "1" {
            return Err(corrupt(format!(
                "the index commit of {} has a history",
                m.source_id
            )));
        }
    }
    let mut links = Gitlinks {
        ctx,
        pre,
        read: BTreeMap::new(),
    };
    match nested {
        // a dirty submodule's commit is what the gitlink names: it has come
        // along, and so have the dirty ones below it (known from their
        // manifests, when this host has them)
        Nested::Store(_) => {
            let mut done = BTreeSet::new();
            let mut todo = vec![(commit.to_string(), &m.submodules)];
            while let Some((at, subs)) = todo.pop() {
                for sub in subs.iter().filter(|s| s.dirty) {
                    let sha = links.of(&at, sub)?;
                    let Some(id) = &sub.source_id else { continue };
                    let Some(n) = known.get(id) else { continue };
                    if done.insert((sha.clone(), id.as_str())) {
                        todo.push((sha, &n.submodules));
                    }
                }
            }
        }
        Nested::Given(list) => {
            let commit_of = |id: &str| {
                list.iter()
                    .find(|n| n.source_id == id)
                    .and_then(|n| n.execution_commit.as_deref())
            };
            // each nested snapshot (its ref is the commit its manifest
            // names) has the tree its manifest names
            for n in list {
                check_tree(
                    ctx,
                    pre,
                    n.execution_commit.as_deref().unwrap_or_default(),
                    n.worktree_tree.as_deref().unwrap_or_default(),
                )?;
            }
            // at every level, a dirty submodule's gitlink names the commit
            // of that submodule's manifest
            let levels = std::iter::once((commit, &m.submodules)).chain(list.iter().map(|n| {
                (
                    n.execution_commit.as_deref().unwrap_or_default(),
                    &n.submodules,
                )
            }));
            for (at, subs) in levels {
                for sub in subs.iter().filter(|s| s.dirty) {
                    // read once for a commit and a path, compared for every
                    // entry
                    let sha = links.of(at, sub)?;
                    let want = sub.source_id.as_deref().and_then(commit_of);
                    if want.is_some_and(|w| w != sha) {
                        return Err(corrupt(format!(
                            "the gitlink of submodule {} is {sha}, its manifest says {}",
                            sub.path.display(),
                            want.unwrap_or_default()
                        )));
                    }
                }
            }
        }
    }
    Ok(got)
}

/// `commit` must have the tree `tree`.
fn check_tree(
    ctx: &ExecContext,
    pre: &[std::ffi::OsString],
    commit: &str,
    tree: &str,
) -> Result<()> {
    let t = String::from_utf8_lossy(&git_run(
        ctx,
        pre,
        &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
        &[],
        None,
    )?)
    .trim()
    .to_string();
    if t != tree {
        return Err(corrupt(format!(
            "commit {commit} has tree {t}, the manifest says {tree}"
        )));
    }
    Ok(())
}

/// The gitlinks an import has read: one `git ls-tree` for a commit and a
/// path, however many entries of the manifests name them, and
/// [`MAX_DIRTY_SUBMODULES`] of them at most.
struct Gitlinks<'a> {
    ctx: &'a ExecContext,
    pre: &'a [std::ffi::OsString],
    read: BTreeMap<(String, Vec<u8>), String>,
}

impl Gitlinks<'_> {
    fn of(&mut self, commit: &str, sub: &SubmoduleRef) -> Result<String> {
        let key = (commit.to_string(), sub.path.0.clone());
        if let Some(sha) = self.read.get(&key) {
            return Ok(sha.clone());
        }
        if self.read.len() >= MAX_DIRTY_SUBMODULES {
            bail!("too many dirty submodules (at most {MAX_DIRTY_SUBMODULES})");
        }
        let sha = gitlink_here(self.ctx, self.pre, commit, sub)?;
        self.read.insert(key, sha.clone());
        Ok(sha)
    }
}

/// The commit the gitlink of `sub` names in the tree of `commit`: the path
/// must be a gitlink there (not a file, a directory or nothing), and the
/// commit it names must be here.
fn gitlink_here(
    ctx: &ExecContext,
    pre: &[std::ffi::OsString],
    commit: &str,
    sub: &SubmoduleRef,
) -> Result<String> {
    use std::os::unix::ffi::OsStringExt;
    check_submodule_path(&sub.path)?;
    // the path is taken as it is written (no pathspec magic) and from the
    // root of the tree
    let mut args: Vec<std::ffi::OsString> = vec!["--literal-pathspecs".into()];
    args.extend(pre.iter().cloned());
    args.extend(["ls-tree", "-z", "--full-tree", commit, "--"].map(std::ffi::OsString::from));
    args.push(std::ffi::OsString::from_vec(sub.path.0.clone()));
    let out = super::capture::git_run_os(ctx, None, &args, &[], None)?;
    // `<mode> <type> <id>\t<name>\0`, exactly once, and the name is the path
    let records: Vec<&[u8]> = out.split(|c| *c == 0).filter(|r| !r.is_empty()).collect();
    let sha = match records.as_slice() {
        [one] => one
            .iter()
            .position(|c| *c == b'\t')
            .and_then(|i| {
                let meta = std::str::from_utf8(&one[..i]).ok()?;
                (one[i + 1..] == sub.path.0[..]).then_some(meta)
            })
            .and_then(|meta| {
                let mut f = meta.split(' ');
                match (f.next(), f.next(), f.next(), f.next()) {
                    (Some("160000"), Some("commit"), Some(sha), None) if is_hex40(sha) => {
                        Some(sha.to_string())
                    }
                    _ => None,
                }
            }),
        _ => None,
    };
    let Some(sha) = sha else {
        return Err(corrupt(format!(
            "submodule {} is not a gitlink in {commit}",
            sub.path.display()
        )));
    };
    let here = git_run(
        ctx,
        pre,
        &["cat-file", "-e", &format!("{sha}^{{commit}}")],
        &[],
        None,
    )
    .is_ok();
    if !here {
        return Err(corrupt(format!(
            "the commit of submodule {} is not in the bundle",
            sub.path.display()
        )));
    }
    Ok(sha)
}
