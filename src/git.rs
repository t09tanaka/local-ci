//! The git plumbing local-ci needs: what to test, what changed, and a
//! throwaway worktree to test it in.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn git_ok(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).ok().filter(|s| !s.is_empty())
}

/// `git worktree add` / `prune` / `remove`, one at a time per repository, in
/// this process and across `local-ci` runs. Side by side, an add reads the
/// `.git/worktrees/<name>/commondir` another add is still writing ("failed to
/// read ... commondir"), and a prune can drop an entry that is half made.
fn worktree_admin<T>(repo: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    use fs2::FileExt;
    static LOCAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _local = LOCAL.lock().unwrap_or_else(|e| e.into_inner());
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(Path::new(&common).join("local-ci-worktrees.lock"))
        .context("cannot open the worktree lock")?;
    lock.lock_exclusive()?;
    f()
}

pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        git(dir, &["rev-parse", "--show-toplevel"]).context("not inside a git repository")?,
    ))
}

pub fn rev(dir: &Path, r: &str) -> Result<String> {
    git(dir, &["rev-parse", "--verify", &format!("{r}^{{commit}}")])
}

pub fn is_dirty(dir: &Path) -> bool {
    git(dir, &["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

pub fn current_branch(dir: &Path) -> Option<String> {
    git_ok(dir, &["symbolic-ref", "--short", "-q", "HEAD"])
}

/// The branch name `branches:` filters see. A `--base` that is not a branch
/// (a SHA, `HEAD~3`) stands for origin's default branch.
pub fn base_branch_name(dir: &Path, base: &str) -> String {
    let is_branch = ["refs/heads/", "refs/remotes/", "refs/remotes/origin/"]
        .iter()
        .any(|p| {
            git(
                dir,
                &["show-ref", "--verify", "--quiet", &format!("{p}{base}")],
            )
            .is_ok()
        });
    if is_branch {
        return branch_name(base);
    }
    default_base(dir)
        .map(|b| branch_name(&b))
        .unwrap_or_else(|_| "main".into())
}

/// The ref to compare against: origin's default branch, else a local main/master.
pub fn default_base(dir: &Path) -> Result<String> {
    if let Some(r) = git_ok(
        dir,
        &["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"],
    ) {
        return Ok(r);
    }
    for b in ["origin/main", "origin/master", "main", "master"] {
        if rev(dir, b).is_ok() {
            return Ok(b.to_string());
        }
    }
    bail!("cannot find a base branch; pass --base <ref>")
}

/// `origin/main` → `main`
pub fn branch_name(r: &str) -> String {
    r.strip_prefix("origin/")
        .or_else(|| r.strip_prefix("refs/remotes/origin/"))
        .or_else(|| r.strip_prefix("refs/heads/"))
        .unwrap_or(r)
        .to_string()
}

pub fn changed_files(dir: &Path, base: &str, sha: &str) -> Result<Vec<String>> {
    let from = git_ok(dir, &["merge-base", base, sha]).unwrap_or_else(|| base.to_string());
    let out = git(dir, &["diff", "--name-only", "--no-renames", &from, sha])?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Where a workflow directory was named, for the error when it holds none.
#[derive(Clone, Copy, PartialEq)]
enum Named {
    /// `.github/workflows` with no `.local-ci.toml` to replace it: may be empty.
    Default,
    Config,
    Flag,
}

/// Workflow files at `sha`, as (path, text): each of `configured` (the
/// `workflows-dirs` of `.local-ci.toml`; `.github/workflows` when `None`) and
/// then each of `extra` (`--workflows-dir`). All are relative to the
/// repository root, and every one but the unconfigured `.github/workflows`
/// must hold some.
pub fn workflows_at(
    dir: &Path,
    sha: &str,
    configured: Option<&[String]>,
    extra: &[String],
) -> Result<Vec<(String, String)>> {
    let mut dirs: Vec<(String, Named)> = match configured {
        Some(c) => c.iter().map(|d| (d.clone(), Named::Config)).collect(),
        None => vec![(
            crate::config::DEFAULT_WORKFLOWS_DIR.to_string(),
            Named::Default,
        )],
    };
    for d in extra {
        if Path::new(d).is_absolute() {
            bail!("--workflows-dir {d}: give a path relative to the repository root");
        }
        let d = crate::plan::norm_dir(d);
        if !dirs.iter().any(|(x, _)| x == &d) {
            dirs.push((d, Named::Flag));
        }
    }
    let mut out = Vec::new();
    for (d, named) in &dirs {
        let list = git(dir, &["ls-tree", "--name-only", sha, &format!("{d}/")]).unwrap_or_default();
        let files: Vec<&str> = list
            .lines()
            .filter(|f| f.ends_with(".yml") || f.ends_with(".yaml"))
            .collect();
        if files.is_empty() && *named != Named::Default {
            let source = match named {
                Named::Config => format!("{}: workflows-dirs", crate::config::FILE),
                _ => "--workflows-dir".to_string(),
            };
            bail!(
                "{source} {d}: no .yml / .yaml files there at {} (workflows are read from the commit; commit them first)",
                &sha[..sha.len().min(7)]
            );
        }
        for f in files {
            out.push((f.to_string(), git(dir, &["show", &format!("{sha}:{f}")])?));
        }
    }
    Ok(out)
}

/// `owner/name` from origin's URL.
pub fn repo_slug(dir: &Path) -> Option<String> {
    let url = git_ok(dir, &["remote", "get-url", "origin"])?;
    let s = url.trim_end_matches(".git").trim_end_matches('/');
    let tail = s.rsplit_once(':').map(|(_, t)| t).unwrap_or(s);
    let parts: Vec<&str> = tail.rsplit('/').take(2).collect();
    if parts.len() == 2 {
        Some(format!("{}/{}", parts[1], parts[0]))
    } else {
        None
    }
}

pub fn add_worktree(repo: &Path, path: &Path, sha: &str) -> Result<()> {
    worktree_admin(repo, || {
        git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                "--quiet",
                &path.to_string_lossy(),
                sha,
            ],
        )
    })
    .map(|_| ())
}

/// Another worktree of `sha` at `to`, holding a copy of everything in `from`
/// (setup output such as `node_modules` included). Files are cloned
/// copy-on-write where the filesystem allows it, so this is quick and a write
/// in one copy is never seen in the other.
pub fn clone_worktree(repo: &Path, from: &Path, to: &Path, sha: &str) -> Result<()> {
    worktree_admin(repo, || {
        git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
                "--quiet",
                &to.to_string_lossy(),
                sha,
            ],
        )
    })?;
    let index = |d: &Path| {
        git(
            d,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )
        .map(PathBuf::from)
    };
    std::fs::copy(index(from)?, index(to)?).context("cannot copy the index")?;
    let mut rest = Vec::new();
    for e in std::fs::read_dir(from)? {
        let e = e?;
        if e.file_name() != ".git" && !clone_tree(&e.path(), &to.join(e.file_name())) {
            rest.push(e.path());
        }
    }
    if rest.is_empty() {
        return Ok(());
    }
    let flags: &[&str] = if cfg!(target_os = "macos") {
        &["-Rp"]
    } else {
        &["-a", "--reflink=auto"]
    };
    let out = Command::new("cp")
        .args(flags)
        .args(&rest)
        .arg(to)
        .output()
        .context("failed to run cp")?;
    if !out.status.success() {
        bail!(
            "cp {}: {}",
            from.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Clone a whole directory tree in one call (APFS). About five times faster
/// than `cp -c`, which clones file by file.
#[cfg(target_os = "macos")]
fn clone_tree(from: &Path, to: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    const CLONE_NOFOLLOW: u32 = 0x0001;
    let (Ok(f), Ok(t)) = (
        CString::new(from.as_os_str().as_bytes()),
        CString::new(to.as_os_str().as_bytes()),
    ) else {
        return false;
    };
    unsafe { libc::clonefile(f.as_ptr(), t.as_ptr(), CLONE_NOFOLLOW) == 0 }
}

#[cfg(not(target_os = "macos"))]
fn clone_tree(_from: &Path, _to: &Path) -> bool {
    false
}

/// Move the worktree out of the way, unregister it, and delete the files in the background.
pub fn remove_worktree(repo: &Path, path: &Path, trash: &Path) {
    let _ = std::fs::create_dir_all(trash);
    let target = trash.join(path.file_name().unwrap_or_default());
    if std::fs::rename(path, &target).is_ok() {
        let _ = worktree_admin(repo, || git(repo, &["worktree", "prune"]));
        let _ = Command::new("rm")
            .arg("-rf")
            .arg(&target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    } else {
        let _ = worktree_admin(repo, || {
            git(
                repo,
                &["worktree", "remove", "--force", &path.to_string_lossy()],
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(branch_name("origin/main"), "main");
        assert_eq!(branch_name("refs/heads/dev"), "dev");
        assert_eq!(branch_name("develop"), "develop");
    }

    /// Many units make and drop their copies at the same moment.
    #[test]
    fn copies_made_and_removed_side_by_side() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-qm", "a"]);
        let sha = rev(&repo, "HEAD").unwrap();
        let trash = tmp.path().join("trash");
        std::thread::scope(|s| {
            for i in 0..24 {
                let (repo, sha, trash) = (&repo, &sha, &trash);
                let to = tmp.path().join(format!("c{i}"));
                s.spawn(move || {
                    clone_worktree(repo, repo, &to, sha).unwrap();
                    assert!(to.join("a.txt").is_file());
                    remove_worktree(repo, &to, trash);
                });
            }
        });
        assert_eq!(
            git(&repo, &["worktree", "list"]).unwrap().lines().count(),
            1
        );
    }
}
