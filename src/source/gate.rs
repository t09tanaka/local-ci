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
    // The tree is only asked of git when the cheap checks could not decide.
    decide(info, publish_sha, || {
        crate::git::git(
            ctx,
            root,
            &["rev-parse", &format!("{publish_sha}^{{tree}}")],
        )
        .ok()
    })
}

/// The decision itself, with git behind `tree_of_publish`: the tree of
/// `publish_sha`, or `None` when it is not a commit of the repository.
pub(super) fn decide(
    info: &SourceInfo,
    publish_sha: &str,
    tree_of_publish: impl FnOnce() -> Option<String>,
) -> Result<(), String> {
    let short = &publish_sha[..publish_sha.len().min(7)];
    if info.source_kind == SourceKind::Directory {
        return Err("the run tested a directory snapshot, not a commit".into());
    }
    let Some(tested) = &info.worktree_tree else {
        return Err("the run recorded no tree of its input".into());
    };
    let tree =
        tree_of_publish().ok_or_else(|| format!("{short} is not a commit of this repository"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info(kind: SourceKind, tree: Option<&str>, commit: Option<&str>) -> SourceInfo {
        SourceInfo {
            source_id: "src1".into(),
            source_kind: kind,
            original_head: None,
            execution_commit: commit.map(String::from),
            worktree_tree: tree.map(String::from),
        }
    }

    const SHA: &str = "0123456789abcdef";

    fn tree(t: &'static str) -> impl FnOnce() -> Option<String> {
        move || Some(t.to_string())
    }

    #[test]
    fn a_directory_is_never_publishable_and_git_is_not_asked() {
        let i = info(SourceKind::Directory, Some("t"), None);
        // Were git asked first, a tree equal to the recorded one would pass
        // the check this test pins.
        let err = decide(&i, SHA, tree("t")).unwrap_err();
        assert!(err.contains("directory snapshot"));
    }

    #[test]
    fn no_recorded_tree_is_refused_before_git_is_asked() {
        let i = info(SourceKind::Commit, None, Some(SHA));
        // Were git asked first, a tree equal to the recorded one would pass
        // the check this test pins.
        let err = decide(&i, SHA, tree("t")).unwrap_err();
        assert_eq!(err, "the run recorded no tree of its input");
    }

    #[test]
    fn an_unknown_commit_is_named_by_its_short_sha() {
        let i = info(SourceKind::Commit, Some("t"), Some(SHA));
        let err = decide(&i, SHA, || None).unwrap_err();
        assert_eq!(err, "0123456 is not a commit of this repository");
    }

    #[test]
    fn a_short_sha_is_not_truncated_further() {
        let i = info(SourceKind::Commit, Some("t"), Some("abc"));
        let err = decide(&i, "abc", || None).unwrap_err();
        assert_eq!(err, "abc is not a commit of this repository");
    }

    #[test]
    fn a_different_tree_of_a_worktree_is_uncommitted_content() {
        let i = info(SourceKind::Worktree, Some("t1"), None);
        let err = decide(&i, SHA, tree("t2")).unwrap_err();
        assert_eq!(
            err,
            "the run tested uncommitted or synthetic content (source src1), not what 0123456 holds"
        );
    }

    #[test]
    fn a_different_tree_of_a_commit_run_differs_from_the_commit() {
        let i = info(SourceKind::Commit, Some("t1"), Some(SHA));
        let err = decide(&i, SHA, tree("t2")).unwrap_err();
        assert_eq!(
            err,
            "the run tested content that differs from the commit (source src1), not what 0123456 holds"
        );
    }

    #[test]
    fn a_commit_run_of_another_commit_with_the_same_tree_is_refused() {
        let i = info(SourceKind::Commit, Some("t"), Some("ffffffffffff"));
        let err = decide(&i, SHA, tree("t")).unwrap_err();
        assert_eq!(err, "the run tested another commit, not 0123456");
        let none = info(SourceKind::Commit, Some("t"), None);
        assert!(decide(&none, SHA, tree("t")).is_err());
    }

    #[test]
    fn the_same_commit_and_tree_is_publishable() {
        let i = info(SourceKind::Commit, Some("t"), Some(SHA));
        assert_eq!(decide(&i, SHA, tree("t")), Ok(()));
    }

    #[test]
    fn a_worktree_with_the_same_tree_is_publishable_without_a_commit() {
        let i = info(SourceKind::Worktree, Some("t"), None);
        assert_eq!(decide(&i, SHA, tree("t")), Ok(()));
    }
}
