//! Prepared templates: a tree a recipe prepared, kept so that tasks can start
//! from a copy of it.
//!
//! A template is made once under its [`TemplateKey`] and never changed: it is
//! made read-only, the store never replaces one, and a consumer gets a copy of
//! its own ([`PreparedTemplate::materialize`], a copy-on-write clone where the
//! filesystem has one, with a receipt bound to the copy), never a link into it. Requirements the key does not
//! hold (A+B for a template made for A) are another key and another template.

use crate::context::ExecContext;
use crate::setup::{PreparedReceipt, TemplateKey};
use anyhow::{bail, Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Where the templates of a host are kept.
#[derive(Debug, Clone)]
pub struct TemplateStore {
    root: PathBuf,
}

/// A finished template. It has no way to change what it holds.
#[derive(Debug, Clone)]
pub struct PreparedTemplate {
    dir: PathBuf,
    receipt: PreparedReceipt,
}

fn dir_name(key: &TemplateKey) -> String {
    key.digest().replace(':', "-")
}

impl TemplateStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        TemplateStore { root: root.into() }
    }

    /// The template made for `key`, if there is one.
    pub fn get(&self, key: &TemplateKey) -> Option<PreparedTemplate> {
        let dir = self.root.join(dir_name(key));
        let text = std::fs::read_to_string(dir.join("receipt.json")).ok()?;
        let receipt: PreparedReceipt = serde_json::from_str(&text).ok()?;
        dir.join("tree")
            .is_dir()
            .then_some(PreparedTemplate { dir, receipt })
    }

    /// Keep a copy of `built` (a workspace `receipt` was made in) as the
    /// template of `key`. A template that is there already is returned as it
    /// is: it is not written again.
    pub fn publish(
        &self,
        ctx: &ExecContext,
        key: &TemplateKey,
        receipt: &PreparedReceipt,
        built: &Path,
    ) -> Result<PreparedTemplate> {
        if receipt.template_key != key.digest() {
            bail!("the receipt is for another template key");
        }
        if let Some(t) = self.get(key) {
            return Ok(t);
        }
        std::fs::create_dir_all(&self.root)?;
        let dir = self.root.join(dir_name(key));
        // built beside it and moved into place, so a half-made template is
        // never seen under its key
        let tmp = self
            .root
            .join(format!(".{}.{}", dir_name(key), std::process::id()));
        let _ = remove_tree(&tmp);
        std::fs::create_dir_all(&tmp)?;
        copy_tree(ctx, built, &tmp.join("tree"))?;
        std::fs::write(
            tmp.join("receipt.json"),
            serde_json::to_string_pretty(receipt)?,
        )?;
        seal(&tmp)?;
        match std::fs::rename(&tmp, &dir) {
            Ok(()) => {}
            Err(_) if dir.join("receipt.json").is_file() => {
                // another builder of the same key finished first
                remove_tree(&tmp)?;
            }
            Err(e) => {
                let _ = remove_tree(&tmp);
                return Err(e).context("cannot keep the template");
            }
        }
        self.get(key).context("the template is not readable")
    }
}

impl PreparedTemplate {
    pub fn receipt(&self) -> &PreparedReceipt {
        &self.receipt
    }

    /// The prepared tree. Read it; it is not writable.
    pub fn tree(&self) -> PathBuf {
        self.dir.join("tree")
    }

    /// A copy of the tree at `dest` (which must not exist), writable and
    /// sharing nothing with the template. The copy is a clone where the
    /// filesystem has clones.
    ///
    /// `dest` is made a workspace of its own (it gets an id, see
    /// [`crate::setup::workspace_id`]) and the receipt returned is the
    /// template's, bound to it: [`crate::execute_task`] given that receipt in
    /// `dest` skips the steps the template prepared (it checks the tree again
    /// before it believes the receipt, as the receipt may be used later, after
    /// something wrote in `dest`). The copy is checked against the receipt
    /// right after it is made; a template that does not match is an error. A tree copied by any other
    /// means has no such receipt and prepares everything itself.
    pub fn materialize(&self, ctx: &ExecContext, dest: &Path) -> Result<PreparedReceipt> {
        if dest.exists() {
            bail!("{} exists", dest.display());
        }
        copy_tree(ctx, &self.tree(), dest)?;
        make_writable(dest)?;
        // Nobody has written in the copy yet: it must hold what the receipt
        // says the setup made.
        if let Err(why) = crate::setup::verify_artifacts(&self.receipt, dest, true) {
            let _ = remove_tree(dest);
            bail!("the copy of the template differs from what its receipt describes: {why}");
        }
        let id = crate::setup::ensure_workspace_id(dest)?;
        Ok(self.receipt.rebound(&id))
    }

    /// Delete the template.
    pub fn remove(self) -> Result<()> {
        remove_tree(&self.dir)
    }
}

/// `from` as `to`: `cp -c -R` (a clone) where that works, else `cp -R`.
fn copy_tree(ctx: &ExecContext, from: &Path, to: &Path) -> Result<()> {
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    for flags in [&["-c", "-R"][..], &["-R"][..]] {
        if flags.contains(&"-c") && !cfg!(target_os = "macos") {
            continue;
        }
        let out = ctx
            .command("cp")
            .args(flags)
            .arg(from)
            .arg(to)
            .output()
            .context("cannot run cp")?;
        if out.status.success() {
            return Ok(());
        }
        let _ = std::fs::remove_dir_all(to);
    }
    bail!("cannot copy {} to {}", from.display(), to.display())
}

fn walk(path: &Path, f: &mut dyn FnMut(&Path, &std::fs::Metadata) -> Result<()>) -> Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if md.is_dir() {
        // a directory has to be writable before it is entered when it is made
        // writable, and entered before it is sealed when it is made read-only;
        // the callers order that with the permission they set
        f(path, &md)?;
        for e in std::fs::read_dir(path)? {
            walk(&e?.path(), f)?;
        }
    } else {
        f(path, &md)?;
    }
    Ok(())
}

/// Make everything under `root` read-only (files first, then directories).
fn seal(root: &Path) -> Result<()> {
    let mut all = Vec::new();
    walk(root, &mut |p, md| {
        if !md.file_type().is_symlink() {
            all.push((p.to_path_buf(), md.permissions().mode(), md.is_dir()));
        }
        Ok(())
    })?;
    for (p, mode, _) in all.iter().rev() {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode & !0o222))?;
    }
    Ok(())
}

/// Give the owner write permission on everything under `root`.
fn make_writable(root: &Path) -> Result<()> {
    walk_writable(root)
}

fn walk_writable(path: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if md.file_type().is_symlink() {
        return Ok(());
    }
    std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(md.permissions().mode() | 0o200),
    )?;
    if md.is_dir() {
        for e in std::fs::read_dir(path)? {
            walk_writable(&e?.path())?;
        }
    }
    Ok(())
}

fn remove_tree(path: &Path) -> Result<()> {
    if path.symlink_metadata().is_err() {
        return Ok(());
    }
    walk_writable(path)?;
    std::fs::remove_dir_all(path).with_context(|| format!("cannot remove {}", path.display()))
}
