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

/// The source ids of the dirty submodules in `subs`, those below them
/// included as far as the manifests this host has tell. An id that is not a
/// source id is refused: it ends up in a ref name.
fn dirty_submodule_ids(subs: &[SubmoduleRef], store: Option<&ObjectStore>) -> Result<Vec<String>> {
    let mut out = vec![];
    for s in subs.iter().filter(|s| s.dirty) {
        let Some(id) = &s.source_id else { continue };
        check_source_id(id)?;
        out.push(id.clone());
        if let Some(n) = store.map(|st| st.load_manifest(id)).transpose()?.flatten() {
            out.extend(dirty_submodule_ids(&n.submodules, store)?);
        }
    }
    Ok(out)
}

/// Forget what [`import_objects`] put in `clone_dir` for the source
/// `source_id`: its snapshot ref and its index ref (the objects go with the
/// clone's next `git gc`). A ref that is not there is not an error. The
/// caller decides when: nothing here knows whether another task of the same
/// source still runs, so the worker that owns the clone calls it from its own
/// garbage collection.
pub fn remove_imported_refs(ctx: &ExecContext, clone_dir: &Path, source_id: &str) -> Result<()> {
    check_source_id(source_id)?;
    let pre = ["-C".into(), clone_dir.into()];
    for r in [snapshot_ref(source_id), index_ref(source_id)] {
        let here = git_run(ctx, &pre, &["rev-parse", "--verify", "-q", &r], &[], None).is_ok();
        if here {
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

/// Like [`export_from`], but cut at the commits the receiving host says it
/// has (`have`) instead of at the merge base with origin: the bundle holds
/// what is reachable from the refs of `m` and not from the entries of `have`
/// that this store knows. An empty `have` (or one this store knows nothing
/// of) gives the whole history of the execution commit. Every entry must be
/// a 40-digit lowercase hex commit id, or the call is refused before Git runs.
///
/// A ref of the bundle (the snapshot of an unchanged dirty submodule, say)
/// whose tip is reachable from an entry of `have` would be dropped by Git.
/// Then that entry is not used and the parents of the tip are excluded
/// instead, so the ref stays in the bundle.
///
/// The bundle's name depends on `have`, so exports with different bases of
/// one source do not replace each other.
pub fn export_with_basis(
    ctx: &ExecContext,
    store: &ObjectStore,
    m: &SourceManifest,
    have: &[String],
) -> Result<PathBuf> {
    if let Some(h) = have.iter().find(|h| !is_hex40(h)) {
        bail!("not a commit id: {h:?}");
    }
    export_impl(ctx, store, m, Some(have))
}

/// The `^` arguments of a bundle made over `refs` with the basis `have`.
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
        let covers: Vec<&String> = tips
            .iter()
            .filter(|t| pg(ctx, dir, &["merge-base", "--is-ancestor", t, h], &[], None).is_ok())
            .collect();
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
        refs.push(r);
    }
    let out_dir = store.root().join("bundles");
    std::fs::create_dir_all(&out_dir)?;
    let name = match have {
        None => format!("{}.bundle", m.source_id),
        Some(h) => {
            use sha2::{Digest, Sha256};
            let mut sorted: Vec<&str> = h.iter().map(String::as_str).collect();
            sorted.sort();
            sorted.dedup();
            let d = Sha256::digest(sorted.join("\n"));
            let hex: String = d.iter().take(8).map(|b| format!("{b:02x}")).collect();
            format!("{}-{hex}.bundle", m.source_id)
        }
    };
    let dest = out_dir.join(name);
    let tmp = super::store::tmp_name(&out_dir, "bundle");
    let mut args: Vec<String> = vec![
        "bundle".into(),
        "create".into(),
        "-q".into(),
        tmp.to_string_lossy().into_owned(),
    ];
    args.extend(refs.iter().cloned());
    match have {
        None => {
            if let Some(mb) = &m.merge_base {
                args.push(format!("^{mb}"));
            }
        }
        Some(h) => args.extend(basis_exclusions(ctx, &dir, &refs, h)?),
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = pg(ctx, &dir, &argv, &[], None);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot bundle {commit}"));
    }
    if have.is_some() {
        // Git leaves out a ref its exclusions cover; a bundle that lacks one
        // is never handed on
        let heads = pg(
            ctx,
            &dir,
            &["bundle", "list-heads", &tmp.to_string_lossy()],
            &[],
            None,
        )
        .map(|o| String::from_utf8_lossy(&o).into_owned())
        .unwrap_or_default();
        let all = refs
            .iter()
            .all(|r| heads.lines().any(|l| l.ends_with(&format!(" {r}"))));
        if !all {
            let _ = std::fs::remove_file(&tmp);
            bail!("the bundle of {} lacks a ref it needs", m.source_id);
        }
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
    import_objects_for(ctx, clone_dir, bundle, m, &[])
}

/// Like [`import_objects`], and also fetches the snapshot ref of each source
/// id in `ids`: the dirty submodules below `m` that this host has no manifest
/// of (a receiving host that has no store yet). Each id is validated as a
/// source id before it reaches a ref name; ids already known from `m` and the
/// store are not repeated.
pub fn import_objects_for(
    ctx: &ExecContext,
    clone_dir: &Path,
    bundle: &Path,
    m: &SourceManifest,
    extra: &[String],
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
    // the snapshot of `m`, those of its dirty submodules, and the index
    // commit: named one by one, so a bundle cannot put another ref in the
    // clone
    let store = default_store(ctx).ok();
    let mut refspecs: Vec<String> = vec![];
    let mut ids = vec![m.source_id.clone()];
    ids.extend(dirty_submodule_ids(&m.submodules, store.as_ref())?);
    for id in extra {
        check_source_id(id)?;
        ids.push(id.clone());
    }
    ids.sort();
    ids.dedup();
    for id in &ids {
        let r = snapshot_ref(id);
        refspecs.push(format!("+{r}:{r}"));
    }
    if m.index_tree.is_some() {
        let r = index_ref(&m.source_id);
        refspecs.push(format!("+{r}:{r}"));
    }
    let bundle_path = bundle.to_string_lossy();
    let mut fetch = vec!["fetch", "-q", "--no-tags", "--no-write-fetch-head"];
    fetch.push(&bundle_path);
    fetch.extend(refspecs.iter().map(String::as_str));
    git_run(ctx, &pre, &fetch, &[], None).context(
        "the bundle lacks a ref of this source (a bundle made without the index tree has no index ref)",
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
    // the index tree came along (in the commit of its own ref) and is the one
    // the manifest names
    if let Some(tree) = &m.index_tree {
        let c = git_run(
            ctx,
            &pre,
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
            return Err(SourceError::Corrupt {
                what: format!("the index tree is {got}, the manifest says {tree}"),
            }
            .into());
        }
        // a commit around the tree and nothing more: no history rides along
        let n = git_run(
            ctx,
            &pre,
            &["rev-list", "--count", &index_ref(&m.source_id)],
            &[],
            None,
        )?;
        if String::from_utf8_lossy(&n).trim() != "1" {
            return Err(SourceError::Corrupt {
                what: format!("the index commit of {} has a history", m.source_id),
            }
            .into());
        }
    }
    // a dirty submodule's commit is what the gitlink names: it has come along,
    // and so have the dirty ones below it (known from their manifests, when
    // this host has them)
    check_gitlinks(ctx, &pre, commit, &m.submodules, store.as_ref())?;
    Ok(got)
}

/// The gitlink of every dirty submodule in `subs`, read from `commit` (the
/// tree of the repository they are in), must name a commit that is here; so
/// must those of its own dirty submodules, down as far as manifests tell.
fn check_gitlinks(
    ctx: &ExecContext,
    pre: &[std::ffi::OsString],
    commit: &str,
    subs: &[SubmoduleRef],
    store: Option<&ObjectStore>,
) -> Result<()> {
    use std::os::unix::ffi::OsStringExt;
    for sub in subs.iter().filter(|s| s.dirty) {
        let mut args: Vec<std::ffi::OsString> = pre.to_vec();
        args.extend(["ls-tree", "-z", commit, "--"].map(std::ffi::OsString::from));
        args.push(std::ffi::OsString::from_vec(sub.path.0.clone()));
        let line = super::capture::git_run_os(ctx, None, &args, &[], None)?;
        let line = String::from_utf8_lossy(&line).into_owned();
        let sha = line.split_whitespace().nth(2).unwrap_or("").to_string();
        let here = is_hex40(&sha)
            && git_run(
                ctx,
                pre,
                &["cat-file", "-e", &format!("{sha}^{{commit}}")],
                &[],
                None,
            )
            .is_ok();
        if !here {
            return Err(SourceError::Corrupt {
                what: format!(
                    "the commit of submodule {} is not in the bundle",
                    sub.path.display()
                ),
            }
            .into());
        }
        let nested = match (&sub.source_id, store) {
            (Some(id), Some(st)) => st.load_manifest(id)?,
            _ => None,
        };
        if let Some(n) = nested {
            check_ids(&n)?;
            check_gitlinks(ctx, pre, &sha, &n.submodules, store)?;
        }
    }
    Ok(())
}
