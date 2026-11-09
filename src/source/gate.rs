//! When a result may be posted as the GitHub status of a commit.
//!
//! A status says "this commit passed". It is true only when what was tested
//! is the commit: the same tree. A dirty worktree, a synthetic commit or a
//! directory is not, however green it was. Secret files are not commit
//! content and do not count; neither do ignored files.

use super::{SourceKind, SourceManifest};
use crate::context::ExecContext;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// What a run remembers about its input (`source.json` in the run directory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub source_id: String,
    pub source_kind: SourceKind,
    pub original_head: Option<String>,
    pub execution_commit: Option<String>,
    pub worktree_tree: Option<String>,
}

impl SourceInfo {
    pub fn of(m: &SourceManifest) -> Self {
        SourceInfo {
            source_id: m.source_id.clone(),
            source_kind: m.source_kind,
            original_head: m.original_head.clone(),
            execution_commit: m.execution_commit.clone(),
            worktree_tree: m.worktree_tree.clone(),
        }
    }
}

/// May the result of a run over `info` be posted to `publish_sha`? `Err`
/// says why not.
pub fn publishable(
    ctx: &ExecContext,
    root: &Path,
    info: &SourceInfo,
    publish_sha: &str,
) -> Result<(), String> {
    let short = &publish_sha[..publish_sha.len().min(7)];
    if info.source_kind == SourceKind::Directory {
        return Err("the run tested a directory snapshot, not a commit".into());
    }
    let Some(tested) = &info.worktree_tree else {
        return Err("the run recorded no tree of its input".into());
    };
    let tree = crate::git::git(
        ctx,
        root,
        &["rev-parse", &format!("{publish_sha}^{{tree}}")],
    )
    .map_err(|_| format!("{short} is not a commit of this repository"))?;
    if &tree != tested {
        let what = match info.source_kind {
            SourceKind::Worktree => "uncommitted or synthetic content",
            _ => "content that differs from the commit",
        };
        return Err(format!(
            "the run tested {what} (source {}), not what {short} holds",
            info.source_id
        ));
    }
    if info.source_kind == SourceKind::Commit
        && info.execution_commit.as_deref() != Some(publish_sha)
    {
        return Err(format!("the run tested another commit, not {short}"));
    }
    Ok(())
}
