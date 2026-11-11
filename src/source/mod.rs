//! The input of a run, fixed.
//!
//! [`capture_source`] copies what is in a working directory right now (staged
//! and unstaged changes, untracked files, deletions, modes, symlinks) into a
//! private, content-addressed store and describes it in a
//! [`SourceManifest`]. The original worktree, its index, branches, refs and
//! stash are never written: objects the capture needs go to a private bare
//! repository under the store, which holds its own copy of the history it
//! needs and the blobs Git itself would make of the captured files.
//!
//! From the manifest a workspace can be rebuilt ([`materialize_source`]) on
//! this host or, through a `git bundle` ([`export_objects`] /
//! [`import_objects`]), the synthetic commit of the capture can be checked out
//! in a clone on another one.
//!
//! Paths are bytes ([`RelPath`]); nothing here converts them lossily.

mod capture;
mod gate;
mod materialize;
mod secret;
mod store;
mod transfer;

pub use capture::{capture_directory, capture_source, seal_manifest};
pub use gate::{publishable, SourceInfo};
pub use materialize::{
    add_worktree, default_store, materialize_from, materialize_secrets, materialize_source,
};
pub use secret::{hmac_sha256, secret_digest, SecretBundle};
pub use store::{ObjectStore, RelPath};
pub use transfer::{
    export_from, export_objects, export_with_basis, import_objects, import_objects_for,
    remove_imported_refs,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// Bumped when the shape of a [`SourceManifest`] changes incompatibly.
pub const SCHEMA_VERSION: u32 = 2;

/// A `secret_files` capture needs a key of at least this many bytes: an empty
/// or short key makes the recorded HMACs a guessable fingerprint of the file.
pub const MIN_SECRET_KEY: usize = 16;
/// The most one secret file may hold (1 MiB). A secret file is read into
/// memory and written into the bundle as a whole; it is a key or a small
/// configuration file, not data.
pub const MAX_SECRET_FILE_BYTES: u64 = 1 << 20;
/// The most the secret files of one capture may hold together (8 MiB).
pub const MAX_SECRET_TOTAL_BYTES: u64 = 8 << 20;

/// Captures are retried this many times in all before `SOURCE_BUSY`.
pub const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// The working directory of a repository, uncommitted work included.
    Worktree,
    /// A commit, as it is.
    Commit,
    /// A directory outside Git.
    Directory,
}

/// One captured file or symlink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: RelPath,
    /// `0o100644`, `0o100755` or `0o120000` (a symlink: the content is the
    /// link value).
    pub mode: u32,
    pub size: u64,
    /// SHA-256 of the content, hex.
    pub hash: String,
}

/// What the store holds for a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectManifest {
    /// Sorted by path.
    pub entries: Vec<FileEntry>,
    /// `sha256:<hex>` of `entries` in a fixed form: what a materialized
    /// workspace must hash to.
    pub tree_digest: String,
    /// Commits a bundle has to carry for `execution_commit` (the synthetic
    /// commit; its parent, the unpushed history, comes with it).
    pub git_tips: Vec<String>,
}

/// A submodule, fixed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmoduleRef {
    pub path: RelPath,
    /// The revision the superproject's index records.
    pub revision: String,
    /// The capture of the submodule's working directory, when it is checked
    /// out (always a full capture, so a dirty one is carried too).
    pub source_id: Option<String>,
    /// The checked-out submodule differs from `revision`.
    pub dirty: bool,
}

/// A file whose content lives in Git LFS: the payload is in the store, not the
/// pointer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfsRef {
    pub path: RelPath,
    /// SHA-256 of the payload (the LFS object id).
    pub oid: String,
    pub size: u64,
    /// SHA-256 of the canonical pointer text for `oid` and `size`.
    pub pointer_hash: String,
}

/// A secret file: its path, and an HMAC of its content under a key the
/// caller holds. The content, and any plain hash of it, are not recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretEntry {
    pub path: RelPath,
    /// `hmac-sha256:<hex>` over the path and the content.
    pub hmac: String,
}

/// A Git filter the capture ran (`filter.<name>.*` of the original). A worker
/// that has to make the same blobs out of the same bytes needs the same
/// definition, whatever its own configuration says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterDef {
    pub name: String,
    pub clean: Option<String>,
    pub smudge: Option<String>,
    pub process: Option<String>,
    pub required: Option<String>,
}

/// What turned the captured bytes into Git's blobs: the settings of the
/// original that decide the canonical form. A worker applies them to its
/// worktree (and to nothing else), so `git status` there agrees with the
/// commit about which files are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalConfig {
    pub autocrlf: Option<String>,
    pub eol: Option<String>,
    pub safecrlf: Option<String>,
    /// Hex of the attribute rules that are not in the tree: the global
    /// attributes file, then `.git/info/attributes` (empty when there are
    /// none). The `.gitattributes` files are in the captured entries.
    pub attributes: String,
    /// The filters the captured paths go through, sorted by name.
    pub filters: Vec<FilterDef>,
}

/// The input of a run (spec chapter 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceManifest {
    pub schema_version: u32,
    /// `src-<32 hex>`: the same content gives the same id.
    pub source_id: String,
    /// `sha256:<hex>` of the manifest without `source_id` and this field.
    pub manifest_digest: String,
    /// Names the repository without a path of this host: `github:owner/name`
    /// from the `origin` remote, else `root:<sha of the first root commit>`.
    pub repository_id: Option<String>,
    pub source_kind: SourceKind,
    pub original_head: Option<String>,
    /// The tree of the index (what is staged).
    pub index_tree: Option<String>,
    /// The tree of the captured content.
    pub worktree_tree: Option<String>,
    /// The commit CI runs against: the synthetic commit of a worktree
    /// capture (parent: `original_head`), or the commit of a commit capture.
    pub execution_commit: Option<String>,
    pub base: Option<String>,
    pub merge_base: Option<String>,
    /// The branch `HEAD` was on (`None` when detached).
    pub branch: Option<String>,
    /// Where the capture was started, relative to the repository root
    /// (empty for the root).
    pub cwd_relative: RelPath,
    pub object_manifest: ObjectManifest,
    pub submodules: Vec<SubmoduleRef>,
    pub lfs: Vec<LfsRef>,
    /// How the captured bytes became Git's blobs.
    pub canonical: CanonicalConfig,
    /// Sorted by path.
    pub secret_files: Vec<SecretEntry>,
}

/// The result of a capture.
#[derive(Debug, Clone)]
pub struct SourceSnapshot {
    pub manifest: SourceManifest,
    /// The secret files, kept apart from the objects (0600), when there are
    /// any: `<store>/secrets/<source_id>.json`, or
    /// [`CaptureOptions::secret_bundle`] when that is set.
    pub secrets: Option<PathBuf>,
    /// Things worth saying that did not stop the capture.
    pub warnings: Vec<String>,
}

/// A `secret_files` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSpec {
    /// Glob, relative to the repository root.
    pub glob: String,
    /// The capture fails when nothing matches.
    pub required: bool,
}

/// Points at which a capture calls its hook. A test seam: a hook that writes
/// to the worktree at [`Stage::AfterRead`] is a concurrent edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// All content of attempt `n` (from 1) is read; re-verification is next.
    AfterRead(u32),
    /// The content is stable; the manifest is about to be published. An error
    /// from the hook stops the capture there.
    BeforePublish,
}

pub type Hook = Arc<dyn Fn(Stage) -> Result<()> + Send + Sync>;

#[derive(Clone)]
pub struct CaptureOptions {
    /// The private store; default `<cache dir>/sources`.
    pub store: Option<PathBuf>,
    /// The base the logical context is taken against (a ref or a sha).
    pub base: Option<String>,
    /// Ignored, untracked files to take along (globs, repository-root relative).
    pub include: Vec<String>,
    /// Files to leave out, tracked or not (globs; a directory name leaves its
    /// content out).
    pub exclude: Vec<String>,
    pub secret_files: Vec<SecretSpec>,
    /// The key of the `secret_files` HMACs.
    pub secret_key: Vec<u8>,
    /// Where the secret files are looked for, when that is not the directory
    /// that is captured: the working tree a commit was checked out from (the
    /// capture itself then runs in a clean checkout, which has none of them).
    /// Any directory inside that working tree; the globs are relative to its
    /// top. The files are chosen, read and checked there exactly as a
    /// worktree capture does in its own directory. A file of this tree that
    /// the captured index tracks is commit content and is not taken. A file
    /// that this tree tracks and the captured index does not (it was added
    /// after the commit, or belongs to another branch) is in neither place:
    /// it is not content of the commit and, being tracked where it is, not a
    /// secret file either. A warning names it; a `required` declaration that
    /// matches nothing else still fails. A secret file is at most
    /// [`MAX_SECRET_FILE_BYTES`], and those of a capture together at most
    /// [`MAX_SECRET_TOTAL_BYTES`] ([`SourceError::SecretTooLarge`]). Only
    /// for [`capture_source`]; [`capture_directory`] refuses it when there
    /// are `secret_files`. Without `secret_files` it has no effect.
    pub secret_root: Option<PathBuf>,
    /// Write the secret bundle here instead of `<store>/secrets/<source_id>.json`
    /// (a path of the run, so that two captures of the same source do not
    /// share a file). The file is created, never replaced: a path that exists
    /// and a parent directory that does not are both
    /// [`SourceError::SecretBundlePath`]. When the capture fails after the
    /// file was written, it is removed again. Nothing is written when no
    /// secret file was found. The directory has to be on a filesystem with
    /// hard links, and a process that is killed during the write can leave a
    /// `.secrets.tmp-*` file (0600, the secret files in plain) in it, which
    /// the caller sweeps: see [`SecretBundle::save_new`].
    pub secret_bundle: Option<PathBuf>,
    pub max_attempts: u32,
    pub hook: Option<Hook>,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        CaptureOptions {
            store: None,
            base: None,
            include: vec![],
            exclude: vec![],
            secret_files: vec![],
            secret_key: vec![],
            secret_root: None,
            secret_bundle: None,
            max_attempts: MAX_ATTEMPTS,
            hook: None,
        }
    }
}

impl CaptureOptions {
    /// The `[source]` table of `.local-ci.toml`.
    pub fn with_config(mut self, c: &crate::config::SourceConfig) -> Self {
        self.include.extend(c.include.iter().cloned());
        self.exclude.extend(c.exclude.iter().cloned());
        self.secret_files.extend(c.secret_files.iter().cloned());
        self
    }

    pub(crate) fn call(&self, stage: Stage) -> Result<()> {
        match &self.hook {
            Some(h) => h(stage),
            None => Ok(()),
        }
    }
}

/// Why a capture, or a use of one, was refused. Wrapped in `anyhow::Error`;
/// `downcast_ref::<SourceError>()` tells them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The input kept changing while it was read.
    Busy { attempts: u32, last: String },
    /// A tracked LFS file whose payload is neither in the worktree nor in the
    /// LFS store.
    LfsPayloadMissing { path: String, oid: String },
    /// A FIFO, socket or device.
    UnsupportedFileType { path: String, kind: String },
    /// A nested repository that is neither tracked nor excluded.
    NestedRepository { path: String },
    /// A `required` secret file that does not exist.
    RequiredSecretMissing { glob: String },
    /// A path that would leave the workspace, or reach into `.git`.
    UnsafePath { path: String, why: String },
    /// A merge in progress.
    Unmerged { path: String },
    /// The content in the store does not match its manifest.
    Corrupt { what: String },
    /// `secret_files` is declared with a key that is empty or too short.
    SecretKeyTooShort { len: usize },
    /// The secret bundle cannot be written where
    /// [`CaptureOptions::secret_bundle`] says: the file is already there, or
    /// its directory is not.
    SecretBundlePath { path: String, why: String },
    /// A secret file is too large to be one: `path` holds `size` bytes, above
    /// [`MAX_SECRET_FILE_BYTES`], or it is the file with which the secret
    /// files of the capture come to `size` bytes together, above
    /// [`MAX_SECRET_TOTAL_BYTES`]. `limit` is the one of the two that was
    /// passed. The capture fails; the file is never left out instead.
    SecretTooLarge { path: String, size: u64, limit: u64 },
    /// A shallow clone: its history is not all there to copy.
    ShallowClone,
    /// A repository that does not use SHA-1 object names.
    UnsupportedObjectFormat { format: String },
    /// The worker has no program for a filter the capture ran, so it cannot
    /// make the blobs of the commit out of the captured bytes. A scheduler
    /// can leave this worker out.
    FilterMissing { filter: String, command: String },
    /// After the captured settings were applied, Git in the workspace still
    /// sees these paths as different from the commit.
    WorkspaceNotClean { paths: Vec<String> },
    /// The repository that was captured from (the original, where a local run
    /// happens) no longer has the settings the capture ran with. They cannot
    /// be repeated without writing the original's shared config, which a run
    /// never does: capture again.
    OriginalConfigChanged { keys: Vec<String> },
    /// Git could not say where the repository's common directory is, so the
    /// settings it reads cannot be compared.
    RepositoryUnreadable { path: String, detail: String },
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Busy { attempts, last } => write!(
                f,
                "SOURCE_BUSY: the input changed while it was read, {attempts} attempts in all ({last})"
            ),
            SourceError::LfsPayloadMissing { path, oid } => write!(
                f,
                "LFS payload missing for {path} (oid {oid}): run `git lfs pull` and capture again"
            ),
            SourceError::UnsupportedFileType { path, kind } => {
                write!(f, "{path}: a {kind} cannot be captured; exclude it")
            }
            SourceError::NestedRepository { path } => write!(
                f,
                "{path}: a repository inside the worktree that is not a submodule; add it as one or exclude it"
            ),
            SourceError::RequiredSecretMissing { glob } => {
                write!(f, "secret_files {glob:?} is required but nothing matches")
            }
            SourceError::UnsafePath { path, why } => write!(f, "refusing path {path:?}: {why}"),
            SourceError::Unmerged { path } => {
                write!(f, "{path}: unmerged; resolve the merge before capturing")
            }
            SourceError::Corrupt { what } => write!(f, "source store: {what}"),
            SourceError::SecretBundlePath { path, why } => {
                write!(f, "cannot write the secret bundle to {path}: {why}")
            }
            SourceError::SecretTooLarge { path, size, limit } => write!(
                f,
                "secret file {path} is too large: {size} bytes with a limit of {limit} (per file {MAX_SECRET_FILE_BYTES}, per capture {MAX_SECRET_TOTAL_BYTES}); narrow secret_files or exclude it"
            ),
            SourceError::SecretKeyTooShort { len } => write!(
                f,
                "secret_files needs a secret key of at least {MIN_SECRET_KEY} bytes (got {len})"
            ),
            SourceError::ShallowClone => write!(
                f,
                "shallow clones are not supported: run `git fetch --unshallow` and capture again"
            ),
            SourceError::UnsupportedObjectFormat { format } => write!(
                f,
                "object format {format} is not supported (only sha1)"
            ),
            SourceError::FilterMissing { filter, command } => write!(
                f,
                "this worker has no {filter} filter program ({command:?}): the captured files cannot be made into the commit's blobs here"
            ),
            SourceError::OriginalConfigChanged { keys } => write!(
                f,
                "the original repository's settings changed after the capture ({}): capture again; a run does not write the original's config",
                keys.join(", ")
            ),
            SourceError::RepositoryUnreadable { path, detail } => write!(
                f,
                "cannot read the Git directory of {path}: {detail}"
            ),
            SourceError::WorkspaceNotClean { paths } => write!(
                f,
                "the workspace differs from the commit for {} path(s) after applying the captured settings: {}",
                paths.len(),
                paths.join(", ")
            ),
        }
    }
}

impl std::error::Error for SourceError {}
