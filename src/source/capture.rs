//! Reading a working directory into the store, and checking that it held
//! still while it was read.

use super::secret::{self, SecretBundle};
use super::store::{sha256_hex, write_atomic, ObjectStore, RelPath};
use super::*;
use crate::context::ExecContext;
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;

// ---- git, as bytes --------------------------------------------------------

/// Run git with `pre` before the arguments, the environment of `ctx` plus
/// `envs`, and `stdin`; the whole of stdout, not decoded.
pub(crate) fn git_run(
    ctx: &ExecContext,
    pre: &[OsString],
    args: &[&str],
    envs: &[(&str, &OsStr)],
    stdin: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut all: Vec<OsString> = pre.to_vec();
    all.extend(args.iter().map(OsString::from));
    git_run_os(ctx, None, &all, envs, stdin)
}

/// [`git_run`] with arguments that are bytes (paths) and a working directory.
pub(crate) fn git_run_os(
    ctx: &ExecContext,
    cwd: Option<&Path>,
    args: &[OsString],
    envs: &[(&str, &OsStr)],
    stdin: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut c = ctx.command("git");
    c.args(args);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    for (k, v) in envs {
        c.env(k, v);
    }
    c.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = c.spawn().context("failed to run git")?;
    let writer = stdin.map(|data| {
        let mut si = child.stdin.take().unwrap();
        std::thread::spawn(move || {
            let _ = si.write_all(&data);
        })
    });
    let out = child.wait_with_output().context("failed to run git")?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    if !out.status.success() {
        // `-c key=value` pairs (a filter's command line) are not worth repeating
        let mut shown: Vec<String> = vec![];
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "-c" {
                it.next();
            } else {
                shown.push(a.to_string_lossy().into_owned());
            }
        }
        bail!(
            "git {}: {}",
            shown.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// The original repository, read only: no optional locks, so reading never
/// refreshes its index.
pub(crate) fn og(ctx: &ExecContext, root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_run(
        ctx,
        &["-C".into(), root.into()],
        args,
        &[("GIT_OPTIONAL_LOCKS", OsStr::new("0"))],
        None,
    )
}

pub(crate) fn og_str(ctx: &ExecContext, root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&og(ctx, root, args)?)
        .trim()
        .to_string())
}

/// The private repository: `--git-dir`, plus `envs`.
pub(crate) fn pg(
    ctx: &ExecContext,
    git_dir: &Path,
    args: &[&str],
    envs: &[(&str, &OsStr)],
    stdin: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    git_run(
        ctx,
        &["--git-dir".into(), git_dir.into()],
        args,
        envs,
        stdin,
    )
}

fn split_z(b: &[u8]) -> Vec<&[u8]> {
    b.split(|&c| c == 0).filter(|s| !s.is_empty()).collect()
}

fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// What `git cat-file --batch-check` said of the `n` names it was given, in
/// order: `Some(name)` for one that is missing, `None` for one that is
/// there. `None` when the answer is not `n` lines that are each `<name>
/// missing` or `<id> <type> <size>` (`<name> ambiguous`, for one): what is
/// not understood is not taken for an object that is there.
fn batch_checked(text: &str, n: usize) -> Option<Vec<Option<&str>>> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != n {
        return None;
    }
    lines
        .into_iter()
        .map(|l| match l.strip_suffix(" missing") {
            Some(name) => Some(Some(name)),
            None => match l.split(' ').collect::<Vec<_>>()[..] {
                [id, kind, size]
                    if is_object_id(id)
                        && !kind.is_empty()
                        && !size.is_empty()
                        && size.bytes().all(|c| c.is_ascii_digit()) =>
                {
                    Some(None)
                }
                _ => None,
            },
        })
        .collect()
}

// ---- asked once -----------------------------------------------------------
//
// A git process costs about as much as everything it does here, so a capture
// asks for several things in one where Git can answer them together. Each
// such question has the questions it replaces behind it: when the answer is
// not the expected shape, or git fails, they are asked the way they always
// were, so the result and the error are the same ones.

/// Where a repository is and what kind it is. None of it moves during a
/// capture, so it is asked once ([`locate`]). A field is `None` when that
/// one question was not answered; whoever needs it then asks for it alone.
#[derive(Clone, Default)]
struct Facts {
    shallow: Option<String>,
    object_format: Option<String>,
    common_dir: Option<String>,
    index_path: Option<String>,
}

impl Facts {
    fn common_dir(&self, ctx: &ExecContext, root: &Path) -> Result<String> {
        match &self.common_dir {
            Some(c) => Ok(c.clone()),
            None => og_str(
                ctx,
                root,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ),
        }
    }

    fn index_path(&self, ctx: &ExecContext, root: &Path) -> Result<String> {
        match &self.index_path {
            Some(p) => Ok(p.clone()),
            None => og_str(
                ctx,
                root,
                &["rev-parse", "--path-format=absolute", "--git-path", "index"],
            ),
        }
    }
}

/// The top of the working tree `repo_dir` is in, and the [`Facts`] of its
/// repository.
fn locate(ctx: &ExecContext, repo_dir: &Path) -> Result<(String, Facts)> {
    if let Ok(out) = og(
        ctx,
        repo_dir,
        &[
            "rev-parse",
            "--show-toplevel",
            "--is-shallow-repository",
            "--show-object-format",
            "--path-format=absolute",
            "--git-common-dir",
            "--git-path",
            "index",
        ],
    ) {
        let text = String::from_utf8_lossy(&out);
        // a path with a line break in it, or with a space at an end, is
        // left to the questions that are asked one at a time
        if let [top, shallow, format, common, index, ""] = text.split('\n').collect::<Vec<_>>()[..]
        {
            if [top, shallow, format, common, index]
                .iter()
                .all(|l| !l.is_empty() && l.trim() == *l)
            {
                let facts = Facts {
                    shallow: Some(shallow.to_string()),
                    object_format: Some(format.to_string()),
                    common_dir: Some(common.to_string()),
                    index_path: Some(index.to_string()),
                };
                return Ok((top.to_string(), facts));
            }
        }
    }
    let top = og_str(ctx, repo_dir, &["rev-parse", "--show-toplevel"])
        .context("not inside a git repository")?;
    Ok((top, Facts::default()))
}

/// The commit of `HEAD`, its tree, and the branch `HEAD` is on (`None` when
/// it is detached). The tree is `None` when it was not asked for.
fn head_of(ctx: &ExecContext, root: &Path) -> Result<(String, Option<String>, Option<String>)> {
    // `--abbrev-ref=loose` shortens the name exactly as `symbolic-ref
    // --short` does, and prints `HEAD` for a detached one
    if let Ok(out) = og(
        ctx,
        root,
        &[
            "rev-parse",
            "HEAD^{commit}",
            "HEAD^{tree}",
            "--abbrev-ref=loose",
            "HEAD",
        ],
    ) {
        let text = String::from_utf8_lossy(&out);
        if let [head, tree, name, ""] = text.split('\n').collect::<Vec<_>>()[..] {
            if is_object_id(head) && is_object_id(tree) && !name.is_empty() && name.trim() == name {
                let branch = (name != "HEAD").then(|| name.to_string());
                return Ok((head.to_string(), Some(tree.to_string()), branch));
            }
        }
    }
    let head = og_str(ctx, root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .context("the repository has no commits yet")?;
    let branch = og_str(ctx, root, &["symbolic-ref", "-q", "--short", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty());
    Ok((head, None, branch))
}

// ---- selection ------------------------------------------------------------

struct Globs {
    set: GlobSet,
    empty: bool,
}

impl Globs {
    fn new(patterns: &[String]) -> Result<Self> {
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            b.add(Glob::new(p).with_context(|| format!("bad glob {p:?}"))?);
        }
        Ok(Globs {
            set: b.build()?,
            empty: patterns.is_empty(),
        })
    }
    /// The path, or a directory above it, matches.
    fn matches(&self, p: &RelPath) -> bool {
        if self.empty {
            return false;
        }
        let m = |r: &RelPath| self.set.is_match(Path::new(OsStr::from_bytes(&r.0)));
        m(p) || p.ancestors().iter().any(m)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stat {
    mode: u32,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    ino: u64,
    dev: u64,
}

impl Stat {
    fn of(m: &std::fs::Metadata) -> Stat {
        Stat {
            mode: m.mode(),
            size: m.size(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
            ino: m.ino(),
            dev: m.dev(),
        }
    }
}

fn lstat(root: &Path, p: &RelPath) -> Option<Stat> {
    std::fs::symlink_metadata(root.join(p.to_path()))
        .ok()
        .map(|m| Stat::of(&m))
}

/// What a capture reads, before it reads it: the state of Git and the list
/// and the stat of every file. Two equal `Meta`s around a read mean nothing
/// moved under it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Meta {
    head: String,
    branch: Option<String>,
    index_hash: String,
    /// Stage-0 index entries: mode, git sha, path.
    tracked: Vec<(u32, String, RelPath)>,
    untracked: Vec<RelPath>,
    /// Ignored files taken along by `include`.
    included: Vec<RelPath>,
    /// Files that match `secret_files` (declaration index, path).
    secret: Vec<(usize, RelPath)>,
    stats: BTreeMap<RelPath, Option<Stat>>,
    /// The stats of `secret`, apart from `stats`: with a `secret_root` they
    /// are files of another directory.
    secret_stats: BTreeMap<RelPath, Option<Stat>>,
}

struct Selection {
    meta: Meta,
    index_bytes: Vec<u8>,
    /// Tracked files a `secret_files` glob matches: they are commit content,
    /// so they are taken as such (declaration index, path).
    tracked_secrets: Vec<(usize, RelPath)>,
    /// With a `secret_root`: files a declaration matches that the working
    /// tree there tracks and the captured index does not. They are not in
    /// the capture at all, neither as content nor as a secret file.
    tracked_elsewhere: Vec<(usize, RelPath)>,
    /// The tree of `head`, when it was asked for along with it.
    head_tree: Option<String>,
    /// Index entries that Git never compares against the working file
    /// (skip-worktree, assume-unchanged); `None` when the listing did not
    /// say.
    flagged: Option<BTreeSet<RelPath>>,
    /// The blob of every index entry, left out of the capture or not.
    index_blobs: Vec<String>,
}

impl Meta {
    /// Why `self` and `later` differ.
    fn diff(&self, later: &Meta) -> String {
        if self.head != later.head {
            return format!("HEAD moved from {} to {}", self.head, later.head);
        }
        if self.branch != later.branch {
            return "the branch changed".into();
        }
        if self.index_hash != later.index_hash || self.tracked != later.tracked {
            return "the index changed".into();
        }
        if self.untracked != later.untracked
            || self.included != later.included
            || self.secret != later.secret
        {
            return "files were added, removed or renamed".into();
        }
        for (p, s) in &self.stats {
            if later.stats.get(p) != Some(s) {
                return format!("{} changed", p.display());
            }
        }
        for (p, s) in &self.secret_stats {
            if later.secret_stats.get(p) != Some(s) {
                return format!("{} changed", p.display());
            }
        }
        "the file list changed".into()
    }
}

/// A capture saw the input move; try again.
#[derive(Debug)]
struct Unstable(String);

impl std::fmt::Display for Unstable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for Unstable {}

fn unstable(why: impl Into<String>) -> anyhow::Error {
    Unstable(why.into()).into()
}

/// Everything the first step records (spec chapter 6, step 1).
fn select_worktree(ctx: &ExecContext, root: &Path, o: &Opts, facts: &Facts) -> Result<Selection> {
    let (head, head_tree, branch) = head_of(ctx, root)?;
    let index_path = facts.index_path(ctx, root)?;
    let index_bytes = std::fs::read(&index_path).unwrap_or_default();
    let index_hash = sha256_hex(&index_bytes);

    // with `-v` every record starts with a tag and a space: `S` for
    // skip-worktree, a lowercase letter for assume-unchanged
    let (listing, tagged) = match og(ctx, root, &["ls-files", "-z", "-s", "-v"]) {
        Ok(l) => (l, true),
        Err(_) => (og(ctx, root, &["ls-files", "-z", "-s"])?, false),
    };
    let mut tracked = vec![];
    let mut flagged = BTreeSet::new();
    let mut index_blobs = vec![];
    for rec in split_z(&listing) {
        let (tag, rec) = match (tagged, rec) {
            (true, [tag, b' ', rest @ ..]) => (*tag, rest),
            (true, _) => bail!("unexpected ls-files output"),
            (false, rec) => (b'H', rec),
        };
        // `<mode> <sha> <stage>\t<path>`
        let tab = rec
            .iter()
            .position(|&c| c == b'\t')
            .context("unexpected ls-files output")?;
        let head_part = String::from_utf8_lossy(&rec[..tab]).into_owned();
        let mut it = head_part.split(' ');
        let mode = u32::from_str_radix(it.next().unwrap_or(""), 8)?;
        let sha = it.next().unwrap_or("").to_string();
        let stage = it.next().unwrap_or("0");
        let path = RelPath(rec[tab + 1..].to_vec());
        if stage != "0" {
            return Err(SourceError::Unmerged {
                path: path.display(),
            }
            .into());
        }
        if tag == b'S' || tag.is_ascii_lowercase() {
            flagged.insert(path.clone());
        }
        if mode != 0o160000 && !sha.is_empty() {
            index_blobs.push(sha.clone());
        }
        tracked.push((mode, sha, path));
    }
    index_blobs.sort();
    index_blobs.dedup();
    let tracked_set: BTreeSet<&RelPath> = tracked.iter().map(|t| &t.2).collect();

    let mut untracked = vec![];
    for rec in split_z(&og(
        ctx,
        root,
        &["ls-files", "-z", "-o", "--exclude-standard"],
    )?) {
        if rec.ends_with(b"/") {
            // a directory that holds a repository of its own
            let p = RelPath(rec[..rec.len() - 1].to_vec());
            if o.exclude.matches(&p) {
                continue;
            }
            return Err(SourceError::NestedRepository { path: p.display() }.into());
        }
        let p = RelPath(rec.to_vec());
        if !tracked_set.contains(&p) {
            untracked.push(p);
        }
    }

    let mut ignored = vec![];
    if !o.include.empty || !o.secrets.is_empty() {
        for rec in split_z(&og(
            ctx,
            root,
            &["ls-files", "-z", "-o", "-i", "--exclude-standard"],
        )?) {
            if !rec.ends_with(b"/") {
                ignored.push(RelPath(rec.to_vec()));
            }
        }
    }

    // git does not list FIFOs, sockets and devices at all; look for them
    // outside the ignored directories
    {
        let mut skip_dirs: Vec<RelPath> = vec![];
        for rec in split_z(&og(
            ctx,
            root,
            &[
                "ls-files",
                "-z",
                "-o",
                "-i",
                "--exclude-standard",
                "--directory",
            ],
        )?) {
            if rec.ends_with(b"/") {
                skip_dirs.push(RelPath(rec[..rec.len() - 1].to_vec()));
            }
        }
        find_special(root, &RelPath(vec![]), &skip_dirs, o)?;
    }

    // secrets: an untracked file the declaration matches, ignored or not. A
    // tracked one is part of the commit and cannot be kept apart from it.
    let mut secret = vec![];
    let mut secret_paths = BTreeSet::new();
    let mut tracked_secrets = vec![];
    let mut tracked_elsewhere = vec![];
    let secret_root = o.secret_root.as_deref().unwrap_or(root);
    if !o.secrets.is_empty() {
        for p in tracked
            .iter()
            .filter(|t| t.0 != 0o160000)
            .map(|t| &t.2)
            .filter(|p| !o.exclude.matches(p))
        {
            if let Some(i) = o.secrets.iter().position(|g| g.matches(p)) {
                tracked_secrets.push((i, p.clone()));
            }
        }
        // git lists no FIFO, socket or device, so one at a secret path
        // would pass for a file that is not there
        special_secrets(secret_root, &RelPath(vec![]), o)?;
        // a file here that a declaration matches never becomes content of
        // the source, wherever the secret files are taken from
        let own = pick_secrets(o, untracked.iter().chain(ignored.iter()));
        secret_paths = own.iter().map(|s| s.1.clone()).collect();
        secret = match &o.secret_root {
            None => own,
            Some(from) => {
                let (listed, there) = secret_root_files(ctx, from)?;
                let mut picked = pick_secrets(o, listed.iter());
                // what the captured index tracks is commit content, as above
                picked.retain(|s| {
                    let is_tracked = tracked_set.contains(&s.1);
                    if is_tracked && !tracked_secrets.contains(s) {
                        tracked_secrets.push(s.clone());
                    }
                    !is_tracked
                });
                // and what only that tree tracks is in neither place
                let there: Vec<RelPath> = there
                    .into_iter()
                    .filter(|p| !tracked_set.contains(p))
                    .collect();
                tracked_elsewhere = pick_secrets(o, there.iter());
                picked
            }
        };
    }
    let secret_stats: BTreeMap<RelPath, Option<Stat>> = secret
        .iter()
        .map(|s| (s.1.clone(), lstat(secret_root, &s.1)))
        .collect();

    let keep = |p: &RelPath| !o.exclude.matches(p) && !secret_paths.contains(p);
    let tracked: Vec<(u32, String, RelPath)> = tracked.into_iter().filter(|t| keep(&t.2)).collect();
    let untracked: Vec<RelPath> = untracked.into_iter().filter(|p| keep(p)).collect();
    let included: Vec<RelPath> = ignored
        .into_iter()
        .filter(|p| o.include.matches(p) && keep(p))
        .collect();

    let mut stats = BTreeMap::new();
    for p in tracked
        .iter()
        .filter(|t| t.0 != 0o160000)
        .map(|t| &t.2)
        .chain(untracked.iter())
        .chain(included.iter())
    {
        stats.insert(p.clone(), lstat(root, p));
    }
    secret.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(Selection {
        meta: Meta {
            head,
            branch,
            index_hash,
            tracked,
            untracked,
            included,
            secret,
            stats,
            secret_stats,
        },
        index_bytes,
        tracked_secrets,
        tracked_elsewhere,
        head_tree,
        flagged: tagged.then_some(flagged),
        index_blobs,
    })
}

/// The files among `candidates` that a `secret_files` declaration matches,
/// each with the index of the first declaration that does.
fn pick_secrets<'a>(
    o: &Opts,
    candidates: impl Iterator<Item = &'a RelPath>,
) -> Vec<(usize, RelPath)> {
    candidates
        .filter_map(|p| {
            o.secrets
                .iter()
                .position(|g| g.matches(p))
                .map(|i| (i, p.clone()))
        })
        .collect()
}

/// A FIFO, socket or device below `root` that a `secret_files` declaration
/// matches, in an ignored directory or not: an error, since it can be
/// neither read as a secret file nor passed over as if nothing were there.
/// (A repository of its own below `root` is not looked into, as in
/// [`secret_candidates`].)
fn special_secrets(root: &Path, rel: &RelPath, o: &Opts) -> Result<()> {
    let here = root.join(rel.to_path());
    let Ok(dir) = std::fs::read_dir(&here) else {
        return Ok(()); // gone or unreadable; git has said what it lists
    };
    for ent in dir {
        let ent = ent?;
        let name = ent.file_name();
        if name.as_bytes() == b".git" {
            continue;
        }
        let mut b = rel.0.clone();
        if !b.is_empty() {
            b.push(b'/');
        }
        b.extend_from_slice(name.as_bytes());
        let p = RelPath(b);
        let Ok(m) = std::fs::symlink_metadata(root.join(p.to_path())) else {
            continue; // gone; the second look notices
        };
        let t = m.file_type();
        if t.is_dir() {
            if !root.join(p.to_path()).join(".git").exists() {
                special_secrets(root, &p, o)?;
            }
        } else if !t.is_file() && !t.is_symlink() && o.secrets.iter().any(|g| g.matches(&p)) {
            return Err(SourceError::UnsupportedFileType {
                path: p.display(),
                kind: kind_of(&m).into(),
            }
            .into());
        }
    }
    Ok(())
}

/// Every file of the working tree `root` that Git does not track, ignored
/// or not: what a secret file can be. (A repository of its own below it is
/// listed as a directory and is not looked into.)
fn secret_candidates(ctx: &ExecContext, root: &Path) -> Result<Vec<RelPath>> {
    Ok(split_z(&og(ctx, root, &["ls-files", "-z", "-o"])?)
        .into_iter()
        .filter(|rec| !rec.ends_with(b"/"))
        .map(|rec| RelPath(rec.to_vec()))
        .collect())
}

/// What a `secret_root` holds: the [`secret_candidates`] of the working tree
/// `root`, and every path it tracks.
fn secret_root_files(ctx: &ExecContext, root: &Path) -> Result<(Vec<RelPath>, Vec<RelPath>)> {
    // with `-t` every record starts with a tag and a space: `?` for a file
    // that is not tracked
    if let Ok(out) = og(ctx, root, &["ls-files", "-z", "-t", "-c", "-o"]) {
        let recs = split_z(&out);
        if recs.iter().all(|r| r.len() > 2 && r[1] == b' ') {
            let (mut untracked, mut tracked) = (vec![], vec![]);
            for r in recs {
                let path = &r[2..];
                if r[0] != b'?' {
                    tracked.push(RelPath(path.to_vec()));
                } else if !path.ends_with(b"/") {
                    untracked.push(RelPath(path.to_vec()));
                }
            }
            return Ok((untracked, tracked));
        }
    }
    let untracked = secret_candidates(ctx, root)?;
    let tracked = split_z(&og(ctx, root, &["ls-files", "-z"])?)
        .into_iter()
        .map(|rec| RelPath(rec.to_vec()))
        .collect();
    Ok((untracked, tracked))
}

/// The top of the working tree `dir` is in, for
/// [`CaptureOptions::secret_root`].
fn secret_toplevel(ctx: &ExecContext, dir: &Path) -> Result<PathBuf> {
    let top = og_str(ctx, dir, &["rev-parse", "--show-toplevel"]).with_context(|| {
        format!(
            "secret_root {} is not inside a git working tree",
            dir.display()
        )
    })?;
    Ok(std::fs::canonicalize(top)?)
}

/// A FIFO, socket or device in the working directory that is not in an
/// ignored directory, a repository of its own or `exclude`d.
fn find_special(root: &Path, rel: &RelPath, skip: &[RelPath], o: &Opts) -> Result<()> {
    let here = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel.to_path())
    };
    for ent in std::fs::read_dir(&here)? {
        let ent = ent?;
        let name = ent.file_name();
        if rel.is_empty() && name.as_bytes() == b".git" {
            continue;
        }
        let mut b = rel.0.clone();
        if !b.is_empty() {
            b.push(b'/');
        }
        b.extend_from_slice(name.as_bytes());
        let p = RelPath(b);
        let Ok(m) = std::fs::symlink_metadata(root.join(p.to_path())) else {
            continue; // gone; the second look notices
        };
        let t = m.file_type();
        if t.is_dir() {
            if skip.contains(&p) || root.join(p.to_path()).join(".git").exists() {
                continue;
            }
            find_special(root, &p, skip, o)?;
        } else if !t.is_file() && !t.is_symlink() && !o.exclude.matches(&p) {
            return Err(SourceError::UnsupportedFileType {
                path: p.display(),
                kind: kind_of(&m).into(),
            }
            .into());
        }
    }
    Ok(())
}

/// Every file below `dir`, for a directory snapshot.
fn select_directory(dir: &Path, o: &Opts, warnings: &mut Vec<String>) -> Result<Meta> {
    fn walk(
        root: &Path,
        rel: &RelPath,
        o: &Opts,
        out: &mut Vec<RelPath>,
        warnings: &mut Vec<String>,
    ) -> Result<()> {
        let here = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel.to_path())
        };
        let mut names: Vec<OsString> = std::fs::read_dir(&here)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<std::io::Result<_>>()?;
        names.sort();
        for n in names {
            let mut b = rel.0.clone();
            if !b.is_empty() {
                b.push(b'/');
            }
            b.extend_from_slice(n.as_bytes());
            let p = RelPath(b);
            if o.exclude.matches(&p) {
                continue;
            }
            let m = std::fs::symlink_metadata(root.join(p.to_path()))?;
            if n.as_bytes().eq_ignore_ascii_case(b".git") {
                // a `.git` directory is left out; anything else called that
                // could never be written into a workspace
                if m.is_dir() && n.as_bytes() == b".git" {
                    warnings.push(format!("{}: a .git directory is left out", p.display()));
                    continue;
                }
                return Err(SourceError::UnsafePath {
                    path: p.display(),
                    why: "a file or directory named .git cannot be part of a workspace".into(),
                }
                .into());
            }
            if m.is_dir() {
                walk(root, &p, o, out, warnings)?;
            } else {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut files = vec![];
    walk(dir, &RelPath(vec![]), o, &mut files, warnings)?;
    // the secret files are kept apart, as in a worktree
    let secret = pick_secrets(o, files.iter());
    let secret_paths: BTreeSet<&RelPath> = secret.iter().map(|s| &s.1).collect();
    files.retain(|p| !secret_paths.contains(p));
    let stats = files.iter().map(|p| (p.clone(), lstat(dir, p))).collect();
    let secret_stats = secret
        .iter()
        .map(|s| (s.1.clone(), lstat(dir, &s.1)))
        .collect();
    Ok(Meta {
        untracked: files,
        secret,
        stats,
        secret_stats,
        ..Meta::default()
    })
}

struct Opts {
    exclude: Globs,
    include: Globs,
    secrets: Vec<Globs>,
    /// The top of the working tree the secret files are taken from, when it
    /// is not the directory that is captured.
    secret_root: Option<PathBuf>,
}

// ---- reading --------------------------------------------------------------

fn kind_of(m: &std::fs::Metadata) -> &'static str {
    let t = m.file_type();
    if t.is_fifo() {
        "FIFO"
    } else if t.is_socket() {
        "socket"
    } else if t.is_char_device() {
        "character device"
    } else if t.is_block_device() {
        "block device"
    } else {
        "special file"
    }
}

/// One file into the store: `None` when it is gone (a deletion).
fn ingest(
    store: &ObjectStore,
    root: &Path,
    p: &RelPath,
    st: &Option<Stat>,
) -> Result<Option<FileEntry>> {
    let Some(st) = st else { return Ok(None) };
    let full = root.join(p.to_path());
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(m) => m,
        Err(_) => return Err(unstable(format!("{} vanished", p.display()))),
    };
    if Stat::of(&meta) != *st {
        return Err(unstable(format!("{} changed", p.display())));
    }
    let ft = meta.file_type();
    if ft.is_dir() {
        return Ok(None);
    }
    if ft.is_symlink() {
        let target = std::fs::read_link(&full)?;
        let (hash, size) = store.put_bytes(target.as_os_str().as_bytes())?;
        return Ok(Some(FileEntry {
            path: p.clone(),
            mode: 0o120000,
            size,
            hash,
        }));
    }
    if !ft.is_file() {
        return Err(SourceError::UnsupportedFileType {
            path: p.display(),
            kind: kind_of(&meta).into(),
        }
        .into());
    }
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&full)?;
    let opened = f.metadata()?;
    if opened.ino() != st.ino || opened.dev() != st.dev || opened.size() != st.size {
        return Err(unstable(format!("{} was replaced", p.display())));
    }
    let (hash, size) = store.put_reader(&mut f)?;
    let after = f.metadata()?;
    if size != st.size || (after.mtime(), after.mtime_nsec()) != st.mtime {
        return Err(unstable(format!("{} changed while read", p.display())));
    }
    Ok(Some(FileEntry {
        path: p.clone(),
        mode: if st.mode & 0o111 != 0 {
            0o100755
        } else {
            0o100644
        },
        size,
        hash,
    }))
}

/// `name` in the directory `dir`, never through a symlink at `name`.
fn open_at(dir: &std::fs::File, name: &[u8], flags: libc::c_int) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name)?;
    // SAFETY: `name` is a NUL-terminated string that outlives the call, and
    // the descriptor that comes back is owned by nothing else.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Exactly `size` bytes of `f`, which is `p`: a file that holds more by now
/// (or less) changed while it was read. Never more than `size + 1` bytes are
/// taken, however much the file has grown.
fn read_sized(f: &mut impl std::io::Read, size: u64, p: &RelPath) -> Result<Vec<u8>> {
    let mut bytes = vec![];
    let mut limited = std::io::Read::take(f, size + 1);
    std::io::Read::read_to_end(&mut limited, &mut bytes)?;
    if bytes.len() as u64 != size {
        return Err(unstable(format!("{} changed while read", p.display())));
    }
    Ok(bytes)
}

/// One secret file of `root`: `None` when it is gone. Only a regular file
/// that is really below `root` is read: a symlink (the file itself or a
/// directory above it), a FIFO or a device is an error, never skipped and
/// never followed. The directories above it are opened one by one from
/// `root`, each without following a symlink, and the file is opened in the
/// last of them, so nothing that is swapped in on the way is gone through;
/// what was opened has to be the very file that was listed (`st`).
///
/// A file above [`MAX_SECRET_FILE_BYTES`], or one that takes `total` (the
/// bytes of the secret files read so far in this capture) above
/// [`MAX_SECRET_TOTAL_BYTES`], is [`SourceError::SecretTooLarge`], said
/// before the file is opened.
fn read_secret(
    root: &Path,
    p: &RelPath,
    st: &Option<Stat>,
    total: &mut u64,
) -> Result<Option<(u32, Vec<u8>)>> {
    let Some(st) = st else { return Ok(None) };
    super::store::check_safe(p)?;
    let mut dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(root)?;
    for a in p.ancestors() {
        let name = a.0.rsplit(|&b| b == b'/').next().unwrap();
        dir = match open_at(&dir, name, libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(unstable(format!("{} vanished", p.display())))
            }
            // a symlink is ELOOP or ENOTDIR, by platform; a file is ENOTDIR
            Err(_) => {
                return Err(SourceError::UnsafePath {
                    path: p.display(),
                    why: format!("{} is not a directory (a symlink or a file)", a.display()),
                }
                .into())
            }
        };
    }
    let not_regular = || -> anyhow::Error {
        SourceError::UnsupportedFileType {
            path: p.display(),
            kind: "non-regular secret file".into(),
        }
        .into()
    };
    // by what was listed, before anything is opened: a FIFO or a device is
    // not opened at all
    // (S_IFMT and S_IFREG, whose integer type differs by platform)
    if st.mode & 0o170000 != 0o100000 {
        return Err(not_regular());
    }
    if st.size > MAX_SECRET_FILE_BYTES {
        return Err(SourceError::SecretTooLarge {
            path: p.display(),
            size: st.size,
            limit: MAX_SECRET_FILE_BYTES,
        }
        .into());
    }
    if *total + st.size > MAX_SECRET_TOTAL_BYTES {
        return Err(SourceError::SecretTooLarge {
            path: p.display(),
            size: *total + st.size,
            limit: MAX_SECRET_TOTAL_BYTES,
        }
        .into());
    }
    let name = p.0.rsplit(|&b| b == b'/').next().unwrap();
    let mut f = match open_at(&dir, name, libc::O_RDONLY | libc::O_NONBLOCK) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(unstable(format!("{} vanished", p.display())))
        }
        // a symlink: ELOOP (EMLINK on some systems)
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::EMLINK)) => {
            return Err(not_regular())
        }
        Err(e) => return Err(e.into()),
    };
    // the whole stat, not only which file it is: the size that was checked
    // above is the size of what is read
    if Stat::of(&f.metadata()?) != *st {
        return Err(unstable(format!("{} changed", p.display())));
    }
    let bytes = read_sized(&mut f, st.size, p)?;
    if Stat::of(&f.metadata()?) != *st {
        return Err(unstable(format!("{} changed while read", p.display())));
    }
    *total += st.size;
    Ok(Some((0o600, bytes)))
}

// ---- LFS ------------------------------------------------------------------

const LFS_VERSION: &str = "version https://git-lfs.github.com/spec/v1";

fn lfs_pointer_text(oid: &str, size: u64) -> String {
    format!("{LFS_VERSION}\noid sha256:{oid}\nsize {size}\n")
}

fn parse_lfs_pointer(b: &[u8]) -> Option<(String, u64)> {
    let t = std::str::from_utf8(b).ok()?;
    let mut lines = t.lines();
    if lines.next()? != LFS_VERSION {
        return None;
    }
    let mut oid = None;
    let mut size = None;
    for l in lines {
        if let Some(h) = l.strip_prefix("oid sha256:") {
            oid = Some(h.to_string());
        } else if let Some(s) = l.strip_prefix("size ") {
            size = s.parse().ok();
        }
    }
    let oid = oid?;
    (oid.len() == 64 && oid.bytes().all(|c| c.is_ascii_hexdigit())).then_some(())?;
    Some((oid, size?))
}

/// For every entry whose `filter` attribute is `lfs`, put the payload in the
/// place of a pointer, and note it. A pointer without a payload is an error.
fn resolve_lfs(
    ctx: &ExecContext,
    store: &ObjectStore,
    root: &Path,
    facts: &Facts,
    entries: &mut [FileEntry],
) -> Result<Vec<LfsRef>> {
    let regular: Vec<usize> = (0..entries.len())
        .filter(|&i| entries[i].mode != 0o120000)
        .collect();
    if regular.is_empty() {
        return Ok(vec![]);
    }
    let mut input = vec![];
    for &i in &regular {
        input.extend_from_slice(&entries[i].path.0);
        input.push(0);
    }
    let out = git_run(
        ctx,
        &["-C".into(), root.into()],
        &["check-attr", "-z", "--stdin", "filter"],
        &[("GIT_OPTIONAL_LOCKS", OsStr::new("0"))],
        Some(input),
    )?;
    let fields: Vec<&[u8]> = out.split(|&c| c == 0).collect();
    let mut lfs_paths = BTreeSet::new();
    for rec in fields.chunks(3) {
        if rec.len() == 3 && rec[2] == b"lfs" {
            lfs_paths.insert(RelPath(rec[0].to_vec()));
        }
    }
    if lfs_paths.is_empty() {
        return Ok(vec![]);
    }
    let common = facts.common_dir(ctx, root)?;
    let lfs_dir = Path::new(&common).join("lfs/objects");
    let mut refs = vec![];
    for e in entries.iter_mut() {
        if !lfs_paths.contains(&e.path) {
            continue;
        }
        if e.size <= 1024 {
            if let Some((oid, size)) = parse_lfs_pointer(&store.read(&e.hash)?) {
                let payload = lfs_dir.join(&oid[..2]).join(&oid[2..4]).join(&oid);
                let missing = || SourceError::LfsPayloadMissing {
                    path: e.path.display(),
                    oid: oid.clone(),
                };
                let mut f = std::fs::File::open(&payload).map_err(|_| missing())?;
                let (hash, n) = store.put_reader(&mut f)?;
                if hash != oid || n != size {
                    return Err(SourceError::Corrupt {
                        what: format!("LFS object {oid} does not match its pointer"),
                    }
                    .into());
                }
                refs.push(LfsRef {
                    path: e.path.clone(),
                    oid: oid.clone(),
                    size,
                    pointer_hash: sha256_hex(lfs_pointer_text(&oid, size).as_bytes()),
                });
                e.hash = hash;
                e.size = n;
                continue;
            }
        }
        // already the payload (the smudge filter ran)
        refs.push(LfsRef {
            path: e.path.clone(),
            oid: e.hash.clone(),
            size: e.size,
            pointer_hash: sha256_hex(lfs_pointer_text(&e.hash, e.size).as_bytes()),
        });
    }
    Ok(refs)
}

// ---- the manifest ---------------------------------------------------------

pub(crate) fn tree_digest(entries: &[FileEntry]) -> String {
    let mut s = String::new();
    for e in entries {
        s.push_str(&format!(
            "{:o} {} {} {}\n",
            e.mode,
            store::hex(&e.path.0),
            e.size,
            e.hash
        ));
    }
    format!("sha256:{}", sha256_hex(s.as_bytes()))
}

fn digest_of(m: &SourceManifest) -> String {
    let mut blank = m.clone();
    blank.source_id = String::new();
    blank.manifest_digest = String::new();
    let v: Value = serde_json::to_value(&blank).expect("a manifest is JSON");
    sha256_hex(crate::frozen::canonical_json(&v).as_bytes())
}

/// `m` with its `manifest_digest` and `source_id` worked out from its content.
pub fn seal_manifest(mut m: SourceManifest) -> SourceManifest {
    let d = digest_of(&m);
    m.source_id = format!("src-{}", &d[..32]);
    m.manifest_digest = format!("sha256:{d}");
    m
}

pub(crate) fn check_digest(m: &SourceManifest) -> Result<()> {
    if m.schema_version != SCHEMA_VERSION {
        bail!(
            "source manifest schema version {} is not supported (this is {SCHEMA_VERSION})",
            m.schema_version
        );
    }
    let d = digest_of(m);
    if m.manifest_digest != format!("sha256:{d}") || m.source_id != format!("src-{}", &d[..32]) {
        return Err(SourceError::Corrupt {
            what: format!("manifest {} does not match its digest", m.source_id),
        }
        .into());
    }
    Ok(())
}

// ---- the private repository ----------------------------------------------

/// The store key of the private repository of a repository: its id and where
/// its common Git directory is on this host.
pub(crate) fn private_key(repository_id: &str, common_dir: &str) -> String {
    sha256_hex(format!("{repository_id}\0{common_dir}").as_bytes())[..16].to_string()
}

/// The bare repository that holds the synthetic commits of one repository.
///
/// It owns its objects: [`import_history`] copies in the history a capture
/// needs, so a rewrite or a `git gc` in the original does not break an export
/// later. The original's `info/attributes` is copied here, because
/// `.gitattributes` files are read from the working tree but this one does
/// not have it.
fn private_repo(
    ctx: &ExecContext,
    store: &ObjectStore,
    root: &Path,
    repository_id: &str,
    facts: &Facts,
) -> Result<PathBuf> {
    let common = facts.common_dir(ctx, root)?;
    let dir = store.git_dir(&private_key(repository_id, &common));
    if !dir.join("HEAD").is_file() {
        let out = ctx
            .command("git")
            .args(["init", "--bare", "-q"])
            .arg(&dir)
            .output()
            .context("failed to run git")?;
        if !out.status.success() {
            bail!("git init: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    let dest = dir.join("info/attributes");
    match std::fs::read(Path::new(&common).join("info/attributes")) {
        Ok(b) => write_atomic(&dest, &b, 0o644)?,
        Err(_) => {
            let _ = std::fs::remove_file(&dest);
        }
    }
    Ok(dir)
}

/// The history of `head` (so the merge base and the unpushed commits too),
/// fetched from the original into the private repository. Reads the original,
/// writes only the private one.
/// A shallow clone has no history to copy, and only SHA-1 object names fit
/// the manifest: both are refused before anything is written.
fn check_supported(ctx: &ExecContext, root: &Path, facts: &Facts) -> Result<()> {
    let shallow = facts.shallow.clone().unwrap_or_else(|| {
        og_str(ctx, root, &["rev-parse", "--is-shallow-repository"]).unwrap_or_default()
    });
    if shallow == "true" {
        return Err(SourceError::ShallowClone.into());
    }
    let format = facts.object_format.clone().unwrap_or_else(|| {
        og_str(ctx, root, &["rev-parse", "--show-object-format"]).unwrap_or_default()
    });
    if !format.is_empty() && format != "sha1" {
        return Err(SourceError::UnsupportedObjectFormat { format }.into());
    }
    Ok(())
}

fn import_history(ctx: &ExecContext, git_dir: &Path, root: &Path, head: &str) -> Result<()> {
    let r = format!("refs/local-ci/heads/{head}");
    if pg(
        ctx,
        git_dir,
        &["rev-parse", "--verify", "-q", &r],
        &[],
        None,
    )
    .is_ok()
    {
        return Ok(());
    }
    let args: Vec<OsString> = vec![
        "--git-dir".into(),
        git_dir.into(),
        "-c".into(),
        "uploadpack.allowAnySHA1InWant=true".into(),
        "fetch".into(),
        "-q".into(),
        "--no-tags".into(),
        "--no-write-fetch-head".into(),
        root.into(),
        format!("+{head}:{r}").into(),
    ];
    git_run_os(
        ctx,
        None,
        &args,
        &[("GIT_OPTIONAL_LOCKS", OsStr::new("0"))],
        None,
    )
    .context("cannot copy the history of HEAD into the source store")?;
    // a fetch that warns and exits 0 leaves no ref behind
    pg(
        ctx,
        git_dir,
        &["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")],
        &[],
        None,
    )
    .with_context(|| format!("the history of {head} did not arrive in the source store"))?;
    Ok(())
}

/// The history of `head` ([`import_history`]) and the blobs `blobs` of the
/// original's index ([`copy_blobs`]) in the private repository. When the
/// history is there already, one look says so and names the blobs that are
/// not.
fn import_objects(
    ctx: &ExecContext,
    git_dir: &Path,
    root: &Path,
    head: &str,
    blobs: &[String],
) -> Result<()> {
    let mut input = format!("refs/local-ci/heads/{head}\n").into_bytes();
    for b in blobs {
        input.extend_from_slice(format!("{b}\n").as_bytes());
    }
    if let Ok(out) = pg(
        ctx,
        git_dir,
        &["cat-file", "--batch-check"],
        &[],
        Some(input),
    ) {
        let text = String::from_utf8_lossy(&out);
        // an answer that is not understood is no answer: the history and
        // the blobs are then looked for the way they were
        if let Some([None, blobs @ ..]) = batch_checked(&text, blobs.len() + 1).as_deref() {
            let missing: Vec<&str> = blobs.iter().flatten().copied().collect();
            return copy_missing(ctx, root, git_dir, &missing);
        }
    }
    import_history(ctx, git_dir, root, head)?;
    let blobs: Vec<&str> = blobs.iter().map(String::as_str).collect();
    copy_blobs(ctx, root, git_dir, &blobs)
}

fn repository_id(ctx: &ExecContext, root: &Path) -> Result<String> {
    if let Some(slug) = crate::git::repo_slug(ctx, root) {
        return Ok(format!("github:{slug}"));
    }
    let roots = og_str(ctx, root, &["rev-list", "--max-parents=0", "HEAD"])?;
    let first = roots.lines().min().context("no root commit")?;
    Ok(format!("root:{first}"))
}

/// Blobs for the store objects `hashes` and for the bytes `inline` in the
/// private repository, as they are (no filter); returns the git id of each.
fn write_blobs(
    ctx: &ExecContext,
    store: &ObjectStore,
    git_dir: &Path,
    hashes: &[&str],
    inline: Vec<Vec<u8>>,
) -> Result<(HashMap<String, String>, Vec<String>)> {
    if hashes.is_empty() && inline.is_empty() {
        return Ok((HashMap::new(), vec![]));
    }
    let marks = store::tmp_name(&store.tmp_dir(), "marks");
    let mut child = ctx
        .command("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["fast-import", "--quiet", "--done"])
        .arg(format!("--export-marks={}", marks.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run git fast-import")?;
    let mut si = child.stdin.take().unwrap();
    let paths: Vec<PathBuf> = hashes.iter().map(|h| store.object_path(h)).collect();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for (i, p) in paths.iter().enumerate() {
            let size = std::fs::metadata(p)?.len();
            write!(si, "blob\nmark :{}\ndata {size}\n", i + 1)?;
            let mut f = std::fs::File::open(p)?;
            std::io::copy(&mut f, &mut si)?;
            si.write_all(b"\n")?;
        }
        for (i, bytes) in inline.iter().enumerate() {
            let mark = paths.len() + i + 1;
            write!(si, "blob\nmark :{mark}\ndata {}\n", bytes.len())?;
            si.write_all(bytes)?;
            si.write_all(b"\n")?;
        }
        si.write_all(b"done\n")
    });
    let out = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("fast-import writer panicked"))??;
    if !out.status.success() {
        bail!(
            "git fast-import: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = std::fs::read_to_string(&marks)?;
    let _ = std::fs::remove_file(&marks);
    let mut shas = HashMap::new();
    let mut inline_shas: BTreeMap<usize, String> = BTreeMap::new();
    for line in text.lines() {
        let (m, sha) = line.split_once(' ').context("bad marks file")?;
        let i: usize = m.trim_start_matches(':').parse()?;
        match hashes.get(i - 1) {
            Some(h) => {
                shas.insert(h.to_string(), sha.to_string());
            }
            None => {
                inline_shas.insert(i, sha.to_string());
            }
        }
    }
    Ok((shas, inline_shas.into_values().collect()))
}

/// The global attributes file the original's Git reads: `core.attributesFile`,
/// else the default one under `$XDG_CONFIG_HOME` / `~/.config`, when it exists.
pub(crate) fn global_attributes_path(ctx: &ExecContext, root: &Path) -> Option<PathBuf> {
    let configured = og_str(
        ctx,
        root,
        &["config", "--type=path", "--get", "core.attributesfile"],
    )
    .ok()
    .filter(|s| !s.is_empty());
    attributes_path(ctx, configured)
}

/// [`global_attributes_path`], given what `core.attributesFile` says.
fn attributes_path(ctx: &ExecContext, configured: Option<String>) -> Option<PathBuf> {
    let p = match configured {
        Some(c) => PathBuf::from(c),
        None => {
            let base = match ctx.var("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
                Some(x) => PathBuf::from(x),
                None => PathBuf::from(ctx.home()?).join(".config"),
            };
            base.join("git/attributes")
        }
    };
    p.is_file().then_some(p)
}

/// The settings of the original that decide how content becomes a blob
/// (filters and line-ending conversion), read in one `git config` and never
/// written.
struct Settings {
    /// `filter.*` and `core.{autocrlf,eol,safecrlf,whitespace}`, in the
    /// order Git reads them: key, value.
    records: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// [`global_attributes_path`].
    attributes_file: Option<PathBuf>,
}

impl Settings {
    fn read(ctx: &ExecContext, root: &Path) -> Self {
        let out = og(
            ctx,
            root,
            &[
                "config",
                "-z",
                "--get-regexp",
                r"^(filter\..*|core\.(autocrlf|eol|safecrlf|whitespace|attributesfile))$",
            ],
        )
        .unwrap_or_default();
        let mut records = vec![];
        let mut configured = false;
        for rec in out.split(|&c| c == 0).filter(|r| !r.is_empty()) {
            let (key, value) = match rec.iter().position(|&c| c == b'\n') {
                Some(i) => (&rec[..i], Some(rec[i + 1..].to_vec())),
                None => (rec, None),
            };
            if key == b"core.attributesfile" {
                configured = true;
            } else {
                records.push((key.to_vec(), value));
            }
        }
        // the value is a path that Git expands (`~`), so Git is asked for
        // it; when it is not set there is nothing to ask
        let attributes_file = if configured {
            global_attributes_path(ctx, root)
        } else {
            attributes_path(ctx, None)
        };
        Settings {
            records,
            attributes_file,
        }
    }

    /// What `git config --get <key>` prints, when that is not nothing.
    fn get(&self, key: &str) -> Option<String> {
        let (_, value) = self
            .records
            .iter()
            .rev()
            .find(|(k, _)| k == key.as_bytes())?;
        let value = String::from_utf8_lossy(value.as_deref().unwrap_or_default())
            .trim()
            .to_string();
        (!value.is_empty()).then_some(value)
    }

    /// The filter drivers whose program can look at the file it is given the
    /// content of: a `clean` command that takes the path (`%f`), or a
    /// `process`, which is told the path of every file. Such a program can
    /// tell a file that is at its path from one that is not.
    fn drivers_given_the_path(&self) -> BTreeSet<String> {
        let mut drivers = BTreeSet::new();
        for (key, value) in &self.records {
            let key = String::from_utf8_lossy(key);
            let value = String::from_utf8_lossy(value.as_deref().unwrap_or_default());
            let Some((name, var)) = key.strip_prefix("filter.").and_then(|k| k.rsplit_once('.'))
            else {
                continue;
            };
            if (var == "clean" && value.contains("%f"))
                || (var == "process" && !value.trim().is_empty())
            {
                drivers.insert(name.to_string());
            }
        }
        drivers
    }

    /// `-c key=value` arguments that repeat the settings. They are passed on
    /// the command line, so the original's config is read and never written.
    fn args(&self) -> Vec<OsString> {
        let mut args = vec![];
        for (key, value) in &self.records {
            let kv = match value {
                Some(v) => [key.as_slice(), b"=", v.as_slice()].concat(),
                None => key.clone(),
            };
            args.push(OsString::from("-c"));
            args.push(OsString::from_vec(kv));
        }
        if let Some(p) = &self.attributes_file {
            args.push(OsString::from("-c"));
            let mut kv = b"core.attributesfile=".to_vec();
            kv.extend_from_slice(p.as_os_str().as_bytes());
            args.push(OsString::from_vec(kv));
        }
        args
    }
}

/// A scratch directory holding the `.gitattributes` files of the capture, as
/// captured: attributes are read from here and not from the original's
/// working tree, which may have moved since.
struct AttrTree {
    dir: PathBuf,
}

impl AttrTree {
    fn new(store: &ObjectStore, entries: &[FileEntry]) -> Result<Self> {
        let dir = store::tmp_name(&store.tmp_dir(), "attrs");
        std::fs::create_dir_all(&dir)?;
        let tree = AttrTree { dir };
        for e in entries {
            if !Self::is_attrs(&e.path) || e.mode == 0o120000 {
                continue;
            }
            let target = tree.dir.join(e.path.to_path());
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, store.read(&e.hash)?)?;
        }
        Ok(tree)
    }

    fn is_attrs(path: &RelPath) -> bool {
        let name = &path.0;
        name.ends_with(b".gitattributes") && (name.len() == 14 || name[name.len() - 15] == b'/')
    }

    /// Put the content `hash` of the regular file `path` of the capture at
    /// its path here, for `git hash-object --stdin-paths` to read. `false`
    /// when it cannot be there under a name that command can be given: a
    /// name it would read differently (it takes one path per line, and a
    /// line that starts with a quote as a quoted one), one that is taken
    /// (two names that differ by case, on a filesystem that does not tell
    /// them apart), or one that this filesystem reads as `.gitattributes`
    /// when it is not: there it would be the attributes of its directory.
    fn place(&self, store: &ObjectStore, path: &RelPath, hash: &str) -> bool {
        let name = &path.0;
        if name.contains(&b'\n') || name.starts_with(b"\"") || name.ends_with(b"\r") {
            return false;
        }
        let target = self.dir.join(path.to_path());
        if Self::is_attrs(path) {
            // `new` wrote this very content, unless another attributes file
            // took its name after it (`A/.gitattributes`, `a/.gitattributes`)
            return matches!(
                (std::fs::read(&target), store.read(hash)),
                (Ok(here), Ok(content)) if here == content
            );
        }
        let Some(parent) = target.parent() else {
            return false;
        };
        if std::fs::symlink_metadata(&target).is_ok() || std::fs::create_dir_all(parent).is_err() {
            return false;
        }
        // asked of the filesystem and not of the name: which names are one
        // (`.GITATTRIBUTES`, and more than a change of case) is its to say
        let attributes = parent.join(".gitattributes");
        let had = std::fs::symlink_metadata(&attributes).is_ok();
        // a copy and not a link: a filter runs in this directory, and what
        // it does to a file here must not reach the store
        if std::fs::copy(store.object_path(hash), &target).is_err() {
            return false;
        }
        if !had && std::fs::symlink_metadata(&attributes).is_ok() {
            let _ = std::fs::remove_file(&target);
            return false;
        }
        true
    }
}

impl Drop for AttrTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The value of each of `names` for each of `paths`, by the attributes of the
/// capture: `(path, attribute, value)`.
fn check_attr(
    ctx: &ExecContext,
    git_dir: &Path,
    attrs: &AttrTree,
    cfg: &[OsString],
    names: &[&str],
    paths: &[&RelPath],
) -> Result<Vec<(RelPath, String, String)>> {
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let mut input = vec![];
    for p in paths {
        input.extend_from_slice(&p.0);
        input.push(0);
    }
    let mut args: Vec<OsString> = vec![
        "--git-dir".into(),
        git_dir.into(),
        "--work-tree".into(),
        attrs.dir.clone().into(),
    ];
    args.extend(cfg.iter().cloned());
    args.extend(["check-attr", "-z", "--stdin"].map(OsString::from));
    args.extend(names.iter().map(OsString::from));
    let out = git_run_os(ctx, Some(&attrs.dir), &args, &[], Some(input))?;
    let fields: Vec<&[u8]> = out.split(|&c| c == 0).collect();
    Ok(fields
        .chunks(3)
        .filter(|r| r.len() == 3)
        .map(|r| {
            (
                RelPath(r[0].to_vec()),
                String::from_utf8_lossy(r[1]).into_owned(),
                String::from_utf8_lossy(r[2]).into_owned(),
            )
        })
        .collect())
}

/// The attribute rules that are not in the tree: the global attributes file,
/// then `info/attributes` of the repository at `git_dir`.
pub(crate) fn attribute_rules(ctx: &ExecContext, root: &Path, git_dir: &Path) -> Vec<u8> {
    rules_of(global_attributes_path(ctx, root).as_deref(), git_dir)
}

/// [`attribute_rules`], given the [`global_attributes_path`].
fn rules_of(global: Option<&Path>, git_dir: &Path) -> Vec<u8> {
    let mut rules = vec![];
    if let Some(g) = global {
        rules.extend(std::fs::read(g).unwrap_or_default());
    }
    if let Ok(info) = std::fs::read(git_dir.join("info/attributes")) {
        if !rules.is_empty() && !rules.ends_with(b"\n") {
            rules.push(b'\n');
        }
        rules.extend(info);
    }
    rules
}

/// The settings that made the blobs, for a worker to repeat.
fn canonical_config(
    ctx: &ExecContext,
    git_dir: &Path,
    settings: &Settings,
    cfg: &[OsString],
    attrs: &AttrTree,
    entries: &[FileEntry],
) -> Result<CanonicalConfig> {
    let get = |k: &str| settings.get(k);
    let rules = rules_of(settings.attributes_file.as_deref(), git_dir);
    let paths: Vec<&RelPath> = entries.iter().map(|e| &e.path).collect();
    let used: BTreeSet<String> = check_attr(ctx, git_dir, attrs, cfg, &["filter"], &paths)?
        .into_iter()
        .map(|(_, _, v)| v)
        .filter(|v| !matches!(v.as_str(), "unspecified" | "unset" | "set"))
        .collect();
    let mut defs: BTreeMap<String, FilterDef> = BTreeMap::new();
    if !used.is_empty() {
        // `filter.<name>.<variable>`
        for (key, val) in &settings.records {
            let key = String::from_utf8_lossy(key);
            let val = String::from_utf8_lossy(val.as_deref().unwrap_or_default());
            let Some((name, var)) = key.strip_prefix("filter.").and_then(|k| k.rsplit_once('.'))
            else {
                continue;
            };
            if var.is_empty() || !used.contains(name) {
                continue;
            }
            let d = defs.entry(name.to_string()).or_insert_with(|| FilterDef {
                name: name.to_string(),
                clean: None,
                smudge: None,
                process: None,
                required: None,
            });
            let v = Some(val.to_string());
            match var {
                "clean" => d.clean = v,
                "smudge" => d.smudge = v,
                "process" => d.process = v,
                "required" => d.required = v,
                _ => {}
            }
        }
    }
    Ok(CanonicalConfig {
        autocrlf: get("core.autocrlf"),
        eol: get("core.eol"),
        safecrlf: get("core.safecrlf"),
        attributes: store::hex(&rules),
        filters: defs.into_values().collect(),
    })
}

/// Hash `content` as Git would when adding the file `path` of the original:
/// attributes from the capture (the `.gitattributes` files it holds, the
/// original's `info/attributes` in the private repository), filters and line
/// endings from the original's config. The blob goes to the private
/// repository.
fn hash_filtered(
    ctx: &ExecContext,
    git_dir: &Path,
    attrs: &AttrTree,
    cfg: &[OsString],
    path: &RelPath,
    content: Vec<u8>,
) -> Result<String> {
    let mut args: Vec<OsString> = vec![
        "--git-dir".into(),
        git_dir.into(),
        "--work-tree".into(),
        attrs.dir.clone().into(),
    ];
    args.extend(cfg.iter().cloned());
    args.extend(["hash-object", "-w", "--stdin"].map(OsString::from));
    args.push(OsString::from_vec(
        [b"--path=".as_slice(), &path.0].concat(),
    ));
    let out = git_run_os(ctx, Some(&attrs.dir), &args, &[], Some(content))
        .with_context(|| format!("cannot hash {}", path.display()))?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// [`hash_filtered`] for the files `files` (path, content hash) of the
/// capture, in one git process: the content is put at its path in the
/// scratch directory of `attrs` and Git reads it from there, which is what
/// it does with a file it adds. A file that cannot be put there, and every
/// file when that process fails or says something unexpected, is hashed the
/// way one file is, so an error is the error of the first file that fails.
///
/// The files `alone` ([`given_the_path`]) are never put there: a filter that
/// looks for the file it is cleaning has always found none, and what it
/// answers then is the blob. They are hashed one at a time, before any other
/// file is in the scratch directory.
fn hash_filtered_all(
    ctx: &ExecContext,
    store: &ObjectStore,
    git_dir: &Path,
    attrs: &AttrTree,
    cfg: &[OsString],
    files: &[(&RelPath, &str)],
    alone: &BTreeSet<RelPath>,
) -> Result<Vec<String>> {
    let mut shas: Vec<Option<Result<String>>> = files
        .iter()
        .map(|(path, hash)| {
            alone
                .contains(*path)
                .then(|| hash_filtered(ctx, git_dir, attrs, cfg, path, store.read(hash)?))
        })
        .collect();
    let rest: Vec<usize> = (0..files.len()).filter(|&i| shas[i].is_none()).collect();
    // one file is one process either way
    if rest.len() > 1 {
        let placed: Vec<usize> = rest
            .into_iter()
            .filter(|&i| attrs.place(store, files[i].0, files[i].1))
            .collect();
        let mut input = vec![];
        for &i in &placed {
            input.extend_from_slice(&files[i].0 .0);
            input.push(b'\n');
        }
        let mut args: Vec<OsString> = vec![
            "--git-dir".into(),
            git_dir.into(),
            "--work-tree".into(),
            attrs.dir.clone().into(),
        ];
        args.extend(cfg.iter().cloned());
        args.extend(["hash-object", "-w", "--stdin-paths"].map(OsString::from));
        if let Ok(out) = git_run_os(ctx, Some(&attrs.dir), &args, &[], Some(input)) {
            let text = String::from_utf8_lossy(&out);
            let ids: Vec<&str> = text.lines().collect();
            if ids.len() == placed.len() && ids.iter().all(|id| is_object_id(id)) {
                for (&i, id) in placed.iter().zip(ids) {
                    shas[i] = Some(Ok(id.to_string()));
                }
            }
        }
    }
    let mut out = vec![];
    for (sha, (path, hash)) in shas.into_iter().zip(files) {
        out.push(match sha {
            Some(s) => s?,
            None => hash_filtered(ctx, git_dir, attrs, cfg, path, store.read(hash)?)?,
        });
    }
    Ok(out)
}

/// The files of `files` whose filter is a program that is given their path
/// ([`Settings::drivers_given_the_path`]). `filter_of` is the `filter`
/// attribute of the paths, when it was asked for already.
fn given_the_path(
    ctx: &ExecContext,
    git_dir: &Path,
    settings: &Settings,
    attrs: &AttrTree,
    cfg: &[OsString],
    filter_of: Option<BTreeMap<RelPath, String>>,
    files: &[(&RelPath, &str)],
) -> Result<BTreeSet<RelPath>> {
    let drivers = settings.drivers_given_the_path();
    if drivers.is_empty() || files.is_empty() {
        return Ok(BTreeSet::new());
    }
    let filter_of = match filter_of {
        Some(f) => f,
        None => {
            let paths: Vec<&RelPath> = files.iter().map(|f| f.0).collect();
            check_attr(ctx, git_dir, attrs, cfg, &["filter"], &paths)?
                .into_iter()
                .map(|(p, _, v)| (p, v))
                .collect()
        }
    };
    Ok(files
        .iter()
        .filter(|f| filter_of.get(f.0).is_some_and(|d| drivers.contains(d)))
        .map(|f| f.0.clone())
        .collect())
}

/// Hash bytes as they are (no path, so no filter).
fn hash_plain(ctx: &ExecContext, git_dir: &Path, content: Vec<u8>) -> Result<String> {
    let out = pg(
        ctx,
        git_dir,
        &["hash-object", "-w", "--stdin"],
        &[],
        Some(content),
    )?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// Paths (of `paths`) that no attribute and no setting touches on their way
/// into Git: their blob is their bytes. And the `filter` attribute of those
/// that have one, when the attributes were asked for.
#[allow(clippy::type_complexity)]
fn untouched_paths(
    ctx: &ExecContext,
    git_dir: &Path,
    settings: &Settings,
    attrs: &AttrTree,
    cfg: &[OsString],
    paths: &[&RelPath],
) -> Result<(BTreeSet<RelPath>, Option<BTreeMap<RelPath, String>>)> {
    let autocrlf = settings.get("core.autocrlf").unwrap_or_default();
    if !(autocrlf.is_empty() || autocrlf == "false") || paths.is_empty() {
        return Ok((BTreeSet::new(), None));
    }
    let found = check_attr(
        ctx,
        git_dir,
        attrs,
        cfg,
        &[
            "text",
            "eol",
            "filter",
            "ident",
            "working-tree-encoding",
            "crlf",
        ],
        paths,
    )?;
    let mut touched = BTreeSet::new();
    let mut filter_of = BTreeMap::new();
    for (path, name, value) in found {
        if value != "unspecified" {
            if name == "filter" {
                filter_of.insert(path.clone(), value);
            }
            touched.insert(path);
        }
    }
    let untouched = paths
        .iter()
        .filter(|p| !touched.contains(**p))
        .map(|p| (*p).clone())
        .collect();
    Ok((untouched, Some(filter_of)))
}

/// Make sure the blobs `shas` of the original are in the private repository
/// (staged content that was never committed is only in the original).
fn copy_blobs(ctx: &ExecContext, root: &Path, git_dir: &Path, shas: &[&str]) -> Result<()> {
    if shas.is_empty() {
        return Ok(());
    }
    let input: Vec<u8> = shas
        .iter()
        .flat_map(|s| format!("{s}\n").into_bytes())
        .collect();
    let out = pg(
        ctx,
        git_dir,
        &["cat-file", "--batch-check"],
        &[],
        Some(input),
    )?;
    let text = String::from_utf8_lossy(&out);
    let missing: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_suffix(" missing"))
        .collect();
    copy_missing(ctx, root, git_dir, &missing)
}

/// Copy the blobs `shas` of the original into the private repository, which
/// does not have them.
fn copy_missing(ctx: &ExecContext, root: &Path, git_dir: &Path, shas: &[&str]) -> Result<()> {
    if shas.is_empty() || pack_across(ctx, root, git_dir, shas) {
        return Ok(());
    }
    for sha in shas {
        let content = og(ctx, root, &["cat-file", "blob", sha])?;
        let got = hash_plain(ctx, git_dir, content)?;
        if got != *sha {
            bail!("blob {sha} was copied as {got}");
        }
    }
    Ok(())
}

/// The objects `shas` of the original, packed there and unpacked in the
/// private repository: two processes for any number of them, and nothing of
/// them held in memory here. An object is stored under the name its content
/// has, so what arrives is what was asked for. `false` when it did not work.
fn pack_across(ctx: &ExecContext, root: &Path, git_dir: &Path, shas: &[&str]) -> bool {
    let Ok(mut pack) = ctx
        .command("git")
        .arg("-C")
        .arg(root)
        .args(["pack-objects", "--stdout", "-q", "--window=0"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let (mut names, packed) = (pack.stdin.take().unwrap(), pack.stdout.take().unwrap());
    let unpack = ctx
        .command("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["unpack-objects", "-q"])
        .stdin(packed)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // a write that fails shows as a pack that fails
    for sha in shas {
        let _ = names.write_all(format!("{sha}\n").as_bytes());
    }
    drop(names);
    let unpacked = unpack.and_then(|mut c| c.wait());
    let packed = pack.wait();
    matches!((packed, unpacked), (Ok(a), Ok(b)) if a.success() && b.success())
}

/// The Git blob of every entry, in the private repository.
///
/// A blob is what Git itself would store, not the bytes of the file: the
/// content as it is in the original's index when the file is unchanged, the
/// result of the original's `.gitattributes` and filters (`hash-object
/// --path`) when it is not, the pointer for an LFS file, and the link value
/// for a symlink. Reading the commit therefore gives what Git gives:
/// a clean repository has the tree of `HEAD`.
#[allow(clippy::too_many_arguments)]
fn blob_ids(
    ctx: &ExecContext,
    store: &ObjectStore,
    root: &Path,
    git_dir: &Path,
    entries: &[FileEntry],
    tracked: &[(u32, String, RelPath)],
    lfs: &[LfsRef],
    attrs: &AttrTree,
    settings: &Settings,
    cfg: &[OsString],
    flagged: Option<&BTreeSet<RelPath>>,
) -> Result<Vec<String>> {
    let lfs_by: HashMap<&RelPath, &LfsRef> = lfs.iter().map(|l| (&l.path, l)).collect();
    let index: HashMap<&RelPath, &str> = tracked
        .iter()
        .filter(|t| t.0 == 0o100644 || t.0 == 0o100755)
        .map(|t| (&t.2, t.1.as_str()))
        .collect();
    let mut changed: BTreeSet<RelPath> =
        split_z(&og(ctx, root, &["diff-files", "--name-only", "-z"])?)
            .into_iter()
            .map(|p| RelPath(p.to_vec()))
            .collect();
    // skip-worktree and assume-unchanged paths are never compared against the
    // index, so a working file that differs from it is not reported: hash them
    match flagged {
        Some(f) => changed.extend(f.iter().cloned()),
        None => {
            let listing = og(ctx, root, &["ls-files", "-v", "-z"])?;
            for rec in split_z(&listing) {
                if rec.len() > 2 && (rec[0] == b'S' || rec[0].is_ascii_lowercase()) {
                    changed.insert(RelPath(rec[2..].to_vec()));
                }
            }
        }
    }

    let mut shas: Vec<Option<String>> = vec![None; entries.len()];
    let mut need_attrs = vec![];
    for (i, e) in entries.iter().enumerate() {
        if e.mode == 0o120000 || lfs_by.contains_key(&e.path) {
            continue;
        }
        match index.get(&e.path) {
            Some(sha) if !changed.contains(&e.path) => {
                shas[i] = Some((*sha).to_string());
            }
            _ => need_attrs.push(i),
        }
    }
    let (untouched, filter_of) = untouched_paths(
        ctx,
        git_dir,
        settings,
        attrs,
        cfg,
        &need_attrs
            .iter()
            .map(|&i| &entries[i].path)
            .collect::<Vec<_>>(),
    )?;

    // bytes that go in as they are: symlinks and files nothing touches
    let mut raw: Vec<&str> = vec![];
    for (i, e) in entries.iter().enumerate() {
        if shas[i].is_none() && (e.mode == 0o120000 || untouched.contains(&e.path)) {
            raw.push(&e.hash);
        }
    }
    raw.sort();
    raw.dedup();
    // the pointer of an LFS file, and what Git makes of every other file
    let mut pointers = vec![];
    let mut filtered = vec![];
    for (i, e) in entries.iter().enumerate() {
        if shas[i].is_some() || e.mode == 0o120000 {
            continue;
        }
        if let Some(l) = lfs_by.get(&e.path) {
            pointers.push((i, lfs_pointer_text(&l.oid, l.size).into_bytes()));
        } else if !untouched.contains(&e.path) {
            filtered.push(i);
        }
    }
    let (raw_ids, pointer_ids) = write_blobs(
        ctx,
        store,
        git_dir,
        &raw,
        pointers.iter().map(|p| p.1.clone()).collect(),
    )?;
    if pointer_ids.len() != pointers.len() {
        bail!("git fast-import did not name every blob");
    }
    for ((i, _), id) in pointers.iter().zip(pointer_ids) {
        shas[*i] = Some(id);
    }
    let files: Vec<(&RelPath, &str)> = filtered
        .iter()
        .map(|&i| (&entries[i].path, entries[i].hash.as_str()))
        .collect();
    let alone = given_the_path(ctx, git_dir, settings, attrs, cfg, filter_of, &files)?;
    let ids = hash_filtered_all(ctx, store, git_dir, attrs, cfg, &files, &alone)?;
    for (&i, id) in filtered.iter().zip(ids) {
        shas[i] = Some(id);
    }
    for (i, e) in entries.iter().enumerate() {
        if shas[i].is_none() {
            shas[i] = Some(raw_ids[&e.hash].clone());
        }
    }
    Ok(shas.into_iter().map(|s| s.unwrap()).collect())
}

/// The tree of `entries` (with the blobs `shas`) and `gitlinks` in the
/// private repository.
fn build_tree(
    ctx: &ExecContext,
    store: &ObjectStore,
    git_dir: &Path,
    entries: &[FileEntry],
    shas: &[String],
    gitlinks: &[(RelPath, String)],
) -> Result<String> {
    let mut input = vec![];
    for (e, sha) in entries.iter().zip(shas) {
        input.extend_from_slice(format!("{:o} {sha}\t", e.mode).as_bytes());
        input.extend_from_slice(&e.path.0);
        input.push(0);
    }
    for (p, sha) in gitlinks {
        input.extend_from_slice(format!("160000 {sha}\t").as_bytes());
        input.extend_from_slice(&p.0);
        input.push(0);
    }
    let idx = store::tmp_name(&store.tmp_dir(), "index");
    let res = (|| -> Result<String> {
        let env = [("GIT_INDEX_FILE", idx.as_os_str())];
        pg(
            ctx,
            git_dir,
            &["update-index", "-z", "--index-info"],
            &env,
            Some(input),
        )?;
        let out = pg(ctx, git_dir, &["write-tree"], &env, None)?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    })();
    let _ = std::fs::remove_file(&idx);
    res
}

/// The tree of the index, taken from a copy of it. The blobs that are staged
/// and not committed exist only in the original: [`import_objects`] has
/// copied them here.
fn index_tree(
    ctx: &ExecContext,
    store: &ObjectStore,
    git_dir: &Path,
    bytes: &[u8],
) -> Result<String> {
    let idx = store::tmp_name(&store.tmp_dir(), "index");
    std::fs::write(&idx, bytes)?;
    let env = [("GIT_INDEX_FILE", idx.as_os_str())];
    let res = pg(ctx, git_dir, &["write-tree"], &env, None)
        .map(|out| String::from_utf8_lossy(&out).trim().to_string());
    let _ = std::fs::remove_file(&idx);
    res
}

/// Point each of `refs` (name, commit) of the private repository at its
/// commit.
fn set_refs(ctx: &ExecContext, git_dir: &Path, refs: &[(String, &str)]) -> Result<()> {
    if refs.len() > 1 {
        let input: String = refs
            .iter()
            .map(|(name, sha)| format!("update {name} {sha}\n"))
            .collect();
        let all = pg(
            ctx,
            git_dir,
            &["update-ref", "--stdin"],
            &[],
            Some(input.into_bytes()),
        );
        if all.is_ok() {
            return Ok(());
        }
    }
    for (name, sha) in refs {
        pg(ctx, git_dir, &["update-ref", name, sha], &[], None)?;
    }
    Ok(())
}

/// [`crate::git::default_base`]: the candidates are looked up in one
/// process.
fn default_base(ctx: &ExecContext, root: &Path) -> Option<String> {
    use crate::git::git;
    let head = ["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"];
    if let Some(r) = git(ctx, root, &head).ok().filter(|s| !s.is_empty()) {
        return Some(r);
    }
    let candidates = ["origin/main", "origin/master", "main", "master"];
    let input: String = candidates
        .iter()
        .map(|b| format!("{b}^{{commit}}\n"))
        .collect();
    if let Ok(out) = git_run(
        ctx,
        &["-C".into(), root.into()],
        &["cat-file", "--batch-check"],
        &[("GIT_OPTIONAL_LOCKS", OsStr::new("0"))],
        Some(input.into_bytes()),
    ) {
        let text = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = text.lines().collect();
        // `<id> <type> <size>`, or `<what was asked> missing`
        let found = |l: &str| l.split_once(' ').is_some_and(|(id, _)| is_object_id(id));
        if lines.len() == candidates.len()
            && lines.iter().all(|l| found(l) || l.ends_with(" missing"))
        {
            let i = lines.iter().position(|l| found(l))?;
            return Some(candidates[i].to_string());
        }
    }
    crate::git::default_base(ctx, root).ok()
}

fn synthetic_commit(ctx: &ExecContext, git_dir: &Path, tree: &str, parent: &str) -> Result<String> {
    fixed_commit(ctx, git_dir, tree, Some(parent), "local-ci source snapshot")
}

/// A commit with no parent that holds the index tree, so that a bundle can
/// carry the tree (a bundle holds commits, not bare trees). The same tree is
/// the same commit.
pub(super) fn index_commit(ctx: &ExecContext, git_dir: &Path, tree: &str) -> Result<String> {
    fixed_commit(ctx, git_dir, tree, None, "local-ci index snapshot")
}

fn fixed_commit(
    ctx: &ExecContext,
    git_dir: &Path,
    tree: &str,
    parent: Option<&str>,
    message: &str,
) -> Result<String> {
    // fixed author and dates: the same content is the same commit
    let who = OsStr::new("local-ci");
    let mail = OsStr::new("local-ci@localhost");
    let date = OsStr::new("1000000000 +0000");
    let env = [
        ("GIT_AUTHOR_NAME", who),
        ("GIT_AUTHOR_EMAIL", mail),
        ("GIT_AUTHOR_DATE", date),
        ("GIT_COMMITTER_NAME", who),
        ("GIT_COMMITTER_EMAIL", mail),
        ("GIT_COMMITTER_DATE", date),
    ];
    let mut args = vec!["commit-tree", tree];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    args.extend(["-m", message]);
    let out = pg(ctx, git_dir, &args, &env, None)?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

// ---- capture --------------------------------------------------------------

fn build_opts(o: &CaptureOptions) -> Result<Opts> {
    Ok(Opts {
        exclude: Globs::new(&o.exclude)?,
        include: Globs::new(&o.include)?,
        secrets: o
            .secret_files
            .iter()
            .map(|s| Globs::new(std::slice::from_ref(&s.glob)))
            .collect::<Result<_>>()?,
        secret_root: None,
    })
}

/// What has to hold before a capture with `secret_files` starts.
fn check_secret_opts(opts: &CaptureOptions) -> Result<()> {
    if opts.secret_files.is_empty() {
        return Ok(());
    }
    if opts.secret_key.len() < MIN_SECRET_KEY {
        return Err(SourceError::SecretKeyTooShort {
            len: opts.secret_key.len(),
        }
        .into());
    }
    if let Some(path) = &opts.secret_bundle {
        // said before anything is read; the write itself checks again
        secret::check_new_path(path)?;
    }
    Ok(())
}

fn store_of(ctx: &ExecContext, o: &CaptureOptions) -> Result<ObjectStore> {
    ObjectStore::open(
        o.store
            .clone()
            .unwrap_or_else(|| ctx.cache_dir().join("sources")),
    )
}

struct Read {
    entries: Vec<FileEntry>,
    secrets: Vec<(usize, RelPath, u32, Vec<u8>)>,
}

fn read_all(
    store: &ObjectStore,
    root: &Path,
    secret_root: &Path,
    meta: &Meta,
    gitlink_paths: &BTreeSet<&RelPath>,
) -> Result<Read> {
    let mut entries = vec![];
    let paths = meta
        .tracked
        .iter()
        .filter(|t| t.0 != 0o160000 && !gitlink_paths.contains(&t.2))
        .map(|t| &t.2)
        .chain(meta.untracked.iter())
        .chain(meta.included.iter());
    for p in paths {
        if let Some(e) = ingest(store, root, p, &meta.stats[p])? {
            entries.push(e);
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut secrets = vec![];
    // of this attempt: the next one starts from nothing
    let mut secret_bytes = 0u64;
    for (i, p) in &meta.secret {
        let st = &meta.secret_stats[p];
        if let Some((mode, bytes)) = read_secret(secret_root, p, st, &mut secret_bytes)? {
            secrets.push((*i, p.clone(), mode, bytes));
        }
    }
    Ok(Read { entries, secrets })
}

/// Capture the working directory of the repository containing `repo_dir`
/// (spec chapter 6). Never writes the repository.
pub fn capture_source(
    ctx: &ExecContext,
    repo_dir: &Path,
    opts: &CaptureOptions,
) -> Result<SourceSnapshot> {
    check_secret_opts(opts)?;
    capture_worktree(ctx, repo_dir, opts, &store_of(ctx, opts)?).map(|c| c.snap)
}

/// The directories of a capture and what they looked like when it read them:
/// the repository itself, then every submodule below it.
type Seen = Vec<(PathBuf, Meta, Facts)>;

struct Captured {
    snap: SourceSnapshot,
    seen: Seen,
    /// The tree of the `HEAD` that was captured, when the selection has it.
    head_tree: Option<String>,
}

fn capture_worktree(
    ctx: &ExecContext,
    repo_dir: &Path,
    opts: &CaptureOptions,
    store: &ObjectStore,
) -> Result<Captured> {
    let (top, facts) = locate(ctx, repo_dir)?;
    let root = std::fs::canonicalize(top)?;
    let cwd = std::fs::canonicalize(repo_dir)?;
    let cwd_relative = RelPath::new(cwd.strip_prefix(&root).unwrap_or(Path::new("")));
    let mut o = build_opts(opts)?;
    if let (Some(dir), false) = (&opts.secret_root, o.secrets.is_empty()) {
        o.secret_root = Some(secret_toplevel(ctx, dir)?);
    }
    check_supported(ctx, &root, &facts)?;
    let repo_id = repository_id(ctx, &root)?;
    let git_dir = private_repo(ctx, store, &root, &repo_id, &facts)?;

    let mut last = String::new();
    let attempts = opts.max_attempts.max(1);
    for attempt in 1..=attempts {
        match one_pass(ctx, &root, &facts, opts, &o, store, attempt) {
            Ok(p) => {
                let mut seen = vec![(root.clone(), p.sel.meta.clone(), facts.clone())];
                seen.extend(p.seen.iter().cloned());
                let head_tree = p.sel.head_tree.clone();
                let snap = finish(ctx, store, &root, &git_dir, opts, repo_id, cwd_relative, p)?;
                return Ok(Captured {
                    snap,
                    seen,
                    head_tree,
                });
            }
            Err(e) => match e.downcast_ref::<Unstable>() {
                Some(u) => last = u.0.clone(),
                None => return Err(e),
            },
        }
    }
    Err(SourceError::Busy { attempts, last }.into())
}

struct Stable {
    sel: Selection,
    read: Read,
    lfs: Vec<LfsRef>,
    submodules: Vec<(SubmoduleRef, String)>,
    /// The submodules below, as they were read.
    seen: Seen,
}

/// Read everything once, then look again: `Err(Unstable)` when it moved.
fn one_pass(
    ctx: &ExecContext,
    root: &Path,
    facts: &Facts,
    opts: &CaptureOptions,
    o: &Opts,
    store: &ObjectStore,
    attempt: u32,
) -> Result<Stable> {
    let sel = select_worktree(ctx, root, o, facts)?;
    // initialized submodules are captured whole, each with its own retries
    let mut submodules = vec![];
    let mut seen: Seen = vec![];
    let mut gitlink_paths = BTreeSet::new();
    for (mode, sha, path) in sel.meta.tracked.iter().filter(|t| t.0 == 0o160000) {
        gitlink_paths.insert(path);
        let sub = root.join(path.to_path());
        if sub.join(".git").exists() {
            let inner = CaptureOptions {
                store: Some(store.root().to_path_buf()),
                max_attempts: opts.max_attempts,
                ..CaptureOptions::default()
            };
            let captured = capture_worktree(ctx, &sub, &inner, store)
                .with_context(|| format!("submodule {}", path.display()))?;
            seen.extend(captured.seen);
            let m = &captured.snap.manifest;
            let head_tree = captured.head_tree.unwrap_or_else(|| {
                og_str(ctx, &sub, &["rev-parse", "HEAD^{tree}"]).unwrap_or_default()
            });
            let dirty = m.original_head.as_deref() != Some(sha.as_str())
                || m.worktree_tree.as_deref() != Some(head_tree.as_str());
            let _ = mode;
            let gitlink = if dirty {
                m.execution_commit.clone().unwrap_or_else(|| sha.clone())
            } else {
                sha.clone()
            };
            submodules.push((
                SubmoduleRef {
                    path: path.clone(),
                    revision: sha.clone(),
                    source_id: Some(m.source_id.clone()),
                    dirty,
                },
                gitlink,
            ));
        } else {
            submodules.push((
                SubmoduleRef {
                    path: path.clone(),
                    revision: sha.clone(),
                    source_id: None,
                    dirty: false,
                },
                sha.clone(),
            ));
        }
    }
    let secret_root = o.secret_root.as_deref().unwrap_or(root);
    let mut read = read_all(store, root, secret_root, &sel.meta, &gitlink_paths)?;
    let lfs = resolve_lfs(ctx, store, root, facts, &mut read.entries)?;
    opts.call(Stage::AfterRead(attempt))?;
    let again = select_worktree(ctx, root, o, facts)?;
    if again.meta != sel.meta {
        return Err(unstable(sel.meta.diff(&again.meta)));
    }
    // the stats of what was read are the stats now
    for p in sel.meta.stats.keys() {
        if lstat(root, p) != sel.meta.stats[p] {
            return Err(unstable(format!("{} changed", p.display())));
        }
    }
    for (p, st) in &sel.meta.secret_stats {
        if lstat(secret_root, p) != *st {
            return Err(unstable(format!("{} changed", p.display())));
        }
    }
    // and a submodule is as it was when its own capture settled: its HEAD,
    // index and every file in it
    let plain = build_opts(&CaptureOptions::default())?;
    for (dir, was, facts) in &seen {
        let now = select_worktree(ctx, dir, &plain, facts)?;
        if &now.meta != was {
            return Err(unstable(format!(
                "submodule {}: {}",
                dir.strip_prefix(root).unwrap_or(dir).display(),
                was.diff(&now.meta)
            )));
        }
    }
    Ok(Stable {
        sel,
        read,
        lfs,
        submodules,
        seen,
    })
}

#[allow(clippy::too_many_arguments)]
fn finish(
    ctx: &ExecContext,
    store: &ObjectStore,
    root: &Path,
    git_dir: &Path,
    opts: &CaptureOptions,
    repo_id: String,
    cwd_relative: RelPath,
    p: Stable,
) -> Result<SourceSnapshot> {
    let mut warnings = vec![];
    let head = p.sel.meta.head.clone();
    let gitlinks: Vec<(RelPath, String)> = p
        .submodules
        .iter()
        .map(|(s, g)| (s.path.clone(), g.clone()))
        .collect();
    import_objects(ctx, git_dir, root, &head, &p.sel.index_blobs)?;
    let settings = Settings::read(ctx, root);
    let cfg = settings.args();
    let attrs = AttrTree::new(store, &p.read.entries)?;
    let shas = blob_ids(
        ctx,
        store,
        root,
        git_dir,
        &p.read.entries,
        &p.sel.meta.tracked,
        &p.lfs,
        &attrs,
        &settings,
        &cfg,
        p.sel.flagged.as_ref(),
    )?;
    let canonical = canonical_config(ctx, git_dir, &settings, &cfg, &attrs, &p.read.entries)?;
    let worktree_tree = build_tree(ctx, store, git_dir, &p.read.entries, &shas, &gitlinks)?;
    let commit = synthetic_commit(ctx, git_dir, &worktree_tree, &head)?;
    let itree = index_tree(ctx, store, git_dir, &p.sel.index_bytes)?;

    let base = opts.base.clone().or_else(|| default_base(ctx, root));
    let merge_base = base.as_deref().and_then(|b| {
        og_str(ctx, root, &["merge-base", b, "HEAD"])
            .ok()
            .filter(|s| !s.is_empty())
    });

    let (secret_files, bundle_files) = secret_entries(
        opts,
        p.read.secrets,
        &p.sel.tracked_secrets,
        &p.sel.tracked_elsewhere,
        &mut warnings,
    )?;

    let entries = p.read.entries;
    let mut submodules: Vec<SubmoduleRef> = p.submodules.into_iter().map(|s| s.0).collect();
    submodules.sort_by(|a, b| a.path.cmp(&b.path));
    let mut lfs = p.lfs;
    lfs.sort_by(|a, b| a.path.cmp(&b.path));
    let m = seal_manifest(SourceManifest {
        schema_version: SCHEMA_VERSION,
        source_id: String::new(),
        manifest_digest: String::new(),
        repository_id: Some(repo_id),
        source_kind: SourceKind::Worktree,
        original_head: Some(head),
        index_tree: Some(itree),
        worktree_tree: Some(worktree_tree),
        execution_commit: Some(commit.clone()),
        base,
        merge_base,
        branch: p.sel.meta.branch.clone(),
        cwd_relative,
        object_manifest: ObjectManifest {
            tree_digest: tree_digest(&entries),
            entries,
            git_tips: vec![commit.clone()],
        },
        submodules,
        lfs,
        canonical,
        secret_files,
    });
    // The index tree is kept by a ref of its own too (a commit around it), so
    // that a prune of the store does not take what was staged and an export
    // can carry it.
    let index = match &m.index_tree {
        Some(tree) => Some(index_commit(ctx, git_dir, tree)?),
        None => None,
    };
    let mut refs = vec![(
        format!("refs/local-ci/snapshots/{}", m.source_id),
        commit.as_str(),
    )];
    if let Some(ic) = &index {
        refs.push((format!("refs/local-ci/index/{}", m.source_id), ic));
    }
    set_refs(ctx, git_dir, &refs)?;
    let secrets = publish(store, opts, &m, bundle_files)?;
    Ok(SourceSnapshot {
        manifest: m,
        secrets,
        warnings,
    })
}

type BundleFile = (RelPath, u32, Vec<u8>);

/// The manifest entries and the bundle content of the secret files that were
/// read. A declaration that matched nothing fails the capture when it is
/// `required` and is a warning otherwise. `elsewhere` are the files that
/// only the working tree of a `secret_root` tracks: they are in neither
/// place, which the warning says instead of "matches no file".
fn secret_entries(
    opts: &CaptureOptions,
    read: Vec<(usize, RelPath, u32, Vec<u8>)>,
    tracked: &[(usize, RelPath)],
    elsewhere: &[(usize, RelPath)],
    warnings: &mut Vec<String>,
) -> Result<(Vec<SecretEntry>, Vec<BundleFile>)> {
    for (_, path) in tracked {
        warnings.push(format!(
            "{} is tracked, so it is commit content and not a secret file",
            path.display()
        ));
    }
    for (_, path) in elsewhere {
        warnings.push(format!(
            "{} is tracked in the working tree of secret_root but not in the captured commit, so it is neither content of the source nor a secret file",
            path.display()
        ));
    }
    for (i, spec) in opts.secret_files.iter().enumerate() {
        if read.iter().all(|s| s.0 != i) && tracked.iter().all(|s| s.0 != i) {
            if spec.required {
                return Err(SourceError::RequiredSecretMissing {
                    glob: spec.glob.clone(),
                }
                .into());
            }
            if elsewhere.iter().all(|s| s.0 != i) {
                warnings.push(format!("secret_files {:?} matches no file", spec.glob));
            }
        }
    }
    let mut secret_files = vec![];
    let mut bundle_files = vec![];
    for (_, path, mode, bytes) in read {
        secret_files.push(SecretEntry {
            hmac: secret::secret_digest(&opts.secret_key, &path, &bytes),
            path: path.clone(),
        });
        bundle_files.push((path, mode, bytes));
    }
    secret_files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((secret_files, bundle_files))
}

/// The last step of a capture: the secret bundle, then the manifest. A
/// bundle at a path the caller chose is taken away again when the manifest
/// is not published after all, so nothing of a capture that failed stays
/// there (and the same path can be used for the next try).
fn publish(
    store: &ObjectStore,
    opts: &CaptureOptions,
    m: &SourceManifest,
    bundle_files: Vec<BundleFile>,
) -> Result<Option<PathBuf>> {
    let secrets = if bundle_files.is_empty() {
        None
    } else {
        let bundle = SecretBundle::new(bundle_files);
        Some(match &opts.secret_bundle {
            Some(path) => {
                bundle.save_new(path)?;
                path.clone()
            }
            None => {
                let path = store.secrets_path(&m.source_id);
                bundle.save(&path)?;
                path
            }
        })
    };
    let published = opts
        .call(Stage::BeforePublish)
        .and_then(|()| store.publish_manifest(m));
    if let Err(e) = published {
        if let (Some(written), true) = (&secrets, opts.secret_bundle.is_some()) {
            let _ = std::fs::remove_file(written);
        }
        return Err(e);
    }
    Ok(secrets)
}

/// Capture a directory that is not a repository: the same manifest, without
/// Git.
pub fn capture_directory(
    ctx: &ExecContext,
    dir: &Path,
    opts: &CaptureOptions,
) -> Result<SourceSnapshot> {
    check_secret_opts(opts)?;
    if opts.secret_root.is_some() && !opts.secret_files.is_empty() {
        bail!("secret_root is for the capture of a repository; a directory capture takes its secret files from the directory itself");
    }
    let store = store_of(ctx, opts)?;
    let root = std::fs::canonicalize(dir)?;
    let o = build_opts(opts)?;
    let mut warnings = vec![];
    let attempts = opts.max_attempts.max(1);
    let mut last = String::new();
    for attempt in 1..=attempts {
        let attempt_res = (|| -> Result<(Meta, Read, Vec<LfsRef>)> {
            let meta = select_directory(&root, &o, &mut warnings)?;
            let read = read_all(&store, &root, &root, &meta, &BTreeSet::new())?;
            opts.call(Stage::AfterRead(attempt))?;
            let mut again_warnings = vec![];
            let again = select_directory(&root, &o, &mut again_warnings)?;
            if again != meta {
                return Err(unstable(meta.diff(&again)));
            }
            Ok((meta, read, vec![]))
        })();
        match attempt_res {
            Ok((_meta, read, lfs)) => {
                let entries = read.entries;
                let (secret_files, bundle_files) =
                    secret_entries(opts, read.secrets, &[], &[], &mut warnings)?;
                let m = seal_manifest(SourceManifest {
                    schema_version: SCHEMA_VERSION,
                    source_id: String::new(),
                    manifest_digest: String::new(),
                    repository_id: None,
                    source_kind: SourceKind::Directory,
                    original_head: None,
                    index_tree: None,
                    worktree_tree: None,
                    execution_commit: None,
                    base: None,
                    merge_base: None,
                    branch: None,
                    cwd_relative: RelPath(vec![]),
                    object_manifest: ObjectManifest {
                        tree_digest: tree_digest(&entries),
                        entries,
                        git_tips: vec![],
                    },
                    submodules: vec![],
                    lfs,
                    canonical: CanonicalConfig::default(),
                    secret_files,
                });
                let secrets = publish(&store, opts, &m, bundle_files)?;
                warnings.sort();
                warnings.dedup();
                return Ok(SourceSnapshot {
                    manifest: m,
                    secrets,
                    warnings,
                });
            }
            Err(e) => match e.downcast_ref::<Unstable>() {
                Some(u) => last = u.0.clone(),
                None => return Err(e),
            },
        }
    }
    Err(SourceError::Busy { attempts, last }.into())
}

#[cfg(test)]
mod read_secret_tests {
    use super::*;

    fn err_of(root: &Path, p: &str) -> SourceError {
        let p = RelPath::new(p);
        // the stat as a selection that followed the path would have taken it
        let st = std::fs::metadata(root.join(p.to_path()))
            .ok()
            .map(|m| Stat::of(&m));
        let e = read_secret(root, &p, &st, &mut 0).unwrap_err();
        e.downcast_ref::<SourceError>()
            .unwrap_or_else(|| panic!("{e:#}"))
            .clone()
    }

    #[test]
    fn a_regular_file_below_the_root_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("conf/deep")).unwrap();
        std::fs::write(root.join("conf/deep/a.key"), "k1\n").unwrap();
        let p = RelPath::new("conf/deep/a.key");
        let got = read_secret(root, &p, &lstat(root, &p), &mut 0).unwrap();
        assert_eq!(got, Some((0o600, b"k1\n".to_vec())));
        // gone when it was listed: nothing, and `required` decides
        assert_eq!(
            read_secret(root, &RelPath::new("conf/none"), &None, &mut 0).unwrap(),
            None
        );
    }

    #[test]
    fn a_directory_above_the_file_that_is_a_symlink_is_not_gone_through() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(outside.join("deep")).unwrap();
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(outside.join("a.key"), "out\n").unwrap();
        std::fs::write(outside.join("deep/b.key"), "out\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("conf")).unwrap();
        std::os::unix::fs::symlink(outside.join("deep"), root.join("real/deep")).unwrap();
        assert_eq!(
            err_of(&root, "conf/a.key"),
            SourceError::UnsafePath {
                path: "conf/a.key".into(),
                why: "conf is not a directory (a symlink or a file)".into()
            }
        );
        // not only the first component
        assert_eq!(
            err_of(&root, "real/deep/b.key"),
            SourceError::UnsafePath {
                path: "real/deep/b.key".into(),
                why: "real/deep is not a directory (a symlink or a file)".into()
            }
        );
    }

    #[test]
    fn a_path_that_leaves_the_root_or_is_not_a_file_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(tmp.path().join("up.key"), "out\n").unwrap();
        std::os::unix::fs::symlink("../up.key", root.join("l.key")).unwrap();
        assert!(matches!(
            err_of(&root, "../up.key"),
            SourceError::UnsafePath { .. }
        ));
        assert_eq!(
            err_of(&root, "l.key"),
            SourceError::UnsupportedFileType {
                path: "l.key".into(),
                kind: "non-regular secret file".into()
            }
        );
        assert!(matches!(
            err_of(&root, "d"),
            SourceError::UnsupportedFileType { .. }
        ));
    }

    #[test]
    fn a_file_that_is_not_the_one_that_was_listed_is_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.key"), "one\n").unwrap();
        let p = RelPath::new("a.key");
        let st = lstat(root, &p);
        std::fs::write(root.join("b.key"), "two\n").unwrap();
        std::fs::rename(root.join("b.key"), root.join("a.key")).unwrap();
        let e = read_secret(root, &p, &st, &mut 0).unwrap_err();
        assert!(e.downcast_ref::<Unstable>().is_some(), "{e:#}");
    }

    fn sized(root: &Path, name: &str, len: u64) -> RelPath {
        let f = std::fs::File::create(root.join(name)).unwrap();
        f.set_len(len).unwrap();
        RelPath::new(name)
    }

    #[test]
    fn a_file_above_the_limit_is_refused_and_one_at_the_limit_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let at = sized(root, "at.key", MAX_SECRET_FILE_BYTES);
        let over = sized(root, "over.key", MAX_SECRET_FILE_BYTES + 1);
        let mut total = 0;
        let got = read_secret(root, &at, &lstat(root, &at), &mut total).unwrap();
        assert_eq!(got.unwrap().1.len() as u64, MAX_SECRET_FILE_BYTES);
        assert_eq!(total, MAX_SECRET_FILE_BYTES);
        let e = read_secret(root, &over, &lstat(root, &over), &mut total).unwrap_err();
        assert_eq!(
            e.downcast_ref::<SourceError>(),
            Some(&SourceError::SecretTooLarge {
                path: "over.key".into(),
                size: MAX_SECRET_FILE_BYTES + 1,
                limit: MAX_SECRET_FILE_BYTES
            })
        );
        // nothing of it was counted
        assert_eq!(total, MAX_SECRET_FILE_BYTES);
    }

    #[test]
    fn the_files_of_one_capture_are_refused_when_they_pass_the_total() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let big = sized(root, "big.key", MAX_SECRET_FILE_BYTES);
        let one = sized(root, "one.key", 1);
        let st = lstat(root, &big);
        let mut total = 0;
        for _ in 0..MAX_SECRET_TOTAL_BYTES / MAX_SECRET_FILE_BYTES {
            read_secret(root, &big, &st, &mut total).unwrap().unwrap();
        }
        // exactly the limit is still fine; one byte more is not
        assert_eq!(total, MAX_SECRET_TOTAL_BYTES);
        let e = read_secret(root, &one, &lstat(root, &one), &mut total).unwrap_err();
        assert_eq!(
            e.downcast_ref::<SourceError>(),
            Some(&SourceError::SecretTooLarge {
                path: "one.key".into(),
                size: MAX_SECRET_TOTAL_BYTES + 1,
                limit: MAX_SECRET_TOTAL_BYTES
            })
        );
        assert_eq!(total, MAX_SECRET_TOTAL_BYTES);
    }

    #[test]
    fn a_file_that_grew_after_it_was_listed_is_a_change_and_is_not_read_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.key"), "one\n").unwrap();
        let p = RelPath::new("a.key");
        let st = lstat(root, &p);
        // appended in place: the same file, more bytes
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("a.key"))
            .unwrap();
        f.write_all(&vec![b'x'; 4 * MAX_SECRET_FILE_BYTES as usize])
            .unwrap();
        drop(f);
        let mut total = 0;
        let e = read_secret(root, &p, &st, &mut total).unwrap_err();
        assert!(e.downcast_ref::<Unstable>().is_some(), "{e:#}");
        assert_eq!(total, 0);
    }

    /// A reader that counts what is asked of it.
    struct Endless(u64);
    impl std::io::Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0 += buf.len() as u64;
            buf.fill(7);
            Ok(buf.len())
        }
    }

    #[test]
    fn a_file_that_grows_while_it_is_read_is_cut_one_byte_past_its_size() {
        let p = RelPath::new("a.key");
        // it grows without end: one byte past the size is all that is taken
        let mut grows = Endless(0);
        let e = read_sized(&mut grows, 10, &p).unwrap_err();
        assert_eq!(
            e.downcast_ref::<Unstable>().map(|u| u.0.as_str()),
            Some("a.key changed while read")
        );
        assert_eq!(grows.0, 11);
        // shorter than listed is a change too
        let e = read_sized(&mut &b"12345"[..], 10, &p).unwrap_err();
        assert!(e.downcast_ref::<Unstable>().is_some(), "{e:#}");
        assert_eq!(read_sized(&mut &b"12345"[..], 5, &p).unwrap(), b"12345");
        assert_eq!(read_sized(&mut &b""[..], 0, &p).unwrap(), b"");
    }

    #[test]
    fn a_directory_that_is_swapped_for_a_symlink_after_the_listing_is_not_gone_through() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(root.join("conf")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("conf/a.key"), "in\n").unwrap();
        std::fs::write(outside.join("a.key"), "out\n").unwrap();
        let p = RelPath::new("conf/a.key");
        let st = lstat(&root, &p);
        std::fs::rename(root.join("conf"), root.join("was")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("conf")).unwrap();
        let e = read_secret(&root, &p, &st, &mut 0).unwrap_err();
        assert!(
            matches!(
                e.downcast_ref::<SourceError>(),
                Some(SourceError::UnsafePath { .. })
            ),
            "{e:#}"
        );
        // a FIFO is refused by what was listed, without being opened
        let fifo = std::ffi::CString::new(root.join("was/p.key").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let p = RelPath::new("was/p.key");
        let e = read_secret(&root, &p, &lstat(&root, &p), &mut 0).unwrap_err();
        assert!(
            matches!(
                e.downcast_ref::<SourceError>(),
                Some(SourceError::UnsupportedFileType { .. })
            ),
            "{e:#}"
        );
    }
}

#[cfg(test)]
mod attr_tree_tests {
    use super::*;

    /// A store below a temporary directory, and whether the file system it
    /// is on tells two names that differ by case apart.
    fn store() -> (tempfile::TempDir, ObjectStore, bool) {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(tmp.path().join("store")).unwrap();
        std::fs::create_dir_all(store.tmp_dir()).unwrap();
        std::fs::write(store.tmp_dir().join("Probe"), "").unwrap();
        let folds = store.tmp_dir().join("probe").exists();
        (tmp, store, folds)
    }

    fn entry(store: &ObjectStore, path: &str, content: &str) -> FileEntry {
        let (hash, size) = store.put_bytes(content.as_bytes()).unwrap();
        FileEntry {
            path: RelPath::new(path),
            mode: 0o100644,
            size,
            hash,
        }
    }

    #[test]
    fn an_attributes_file_is_read_from_the_tree_only_when_it_holds_its_content() {
        let (_tmp, store, folds) = store();
        let entries = [
            entry(&store, "A/.gitattributes", "*.txt text\n"),
            entry(&store, "a/.gitattributes", "*.txt -text\n"),
            entry(&store, "b/.gitattributes", "*.md text\n"),
        ];
        let tree = AttrTree::new(&store, &entries).unwrap();
        let placed: Vec<bool> = entries
            .iter()
            .map(|e| tree.place(&store, &e.path, &e.hash))
            .collect();
        for (e, placed) in entries.iter().zip(&placed) {
            if *placed {
                assert_eq!(
                    std::fs::read(tree.dir.join(e.path.to_path())).unwrap(),
                    store.read(&e.hash).unwrap(),
                    "{}",
                    e.path.display()
                );
            }
        }
        assert!(placed[2]);
        if folds {
            // one file holds one of the two contents: the other is not there
            assert_eq!(placed[..2], [false, true]);
        } else {
            eprintln!("note: this file system tells A from a: the collision is not tested");
            assert_eq!(placed[..2], [true, true]);
        }
    }

    #[test]
    fn a_file_whose_name_the_file_system_reads_as_gitattributes_is_not_put_in_the_tree() {
        let (_tmp, store, folds) = store();
        let tree = AttrTree::new(&store, &[]).unwrap();
        // the last one ends in a long s, which folds to `s`
        for (dir, name) in [("d", ".GITATTRIBUTES"), ("e", ".gitattribute\u{17f}")] {
            let e = entry(&store, &format!("{dir}/{name}"), "* filter=upper\n");
            let taken = tree.dir.join(dir).join(".gitattributes");
            let placed = tree.place(&store, &e.path, &e.hash);
            if placed {
                assert!(!taken.exists(), "{name} is an attributes file");
            } else {
                assert!(std::fs::read_dir(tree.dir.join(dir))
                    .unwrap()
                    .next()
                    .is_none());
            }
            if folds && name.is_ascii() {
                assert!(!placed, "{name}");
            } else if !folds {
                eprintln!("note: this file system tells {name} from .gitattributes: not tested");
                assert!(placed, "{name}");
            }
        }
        // any other name is put there, once
        let e = entry(&store, "d/plain.txt", "text\n");
        assert!(tree.place(&store, &e.path, &e.hash));
        assert!(!tree.place(&store, &e.path, &e.hash));
    }

    #[test]
    fn an_answer_of_batch_check_is_read_only_when_every_line_is_one_it_gives() {
        let id = "b7fcd48714df21ff5b66c934c5c2359f72ad5ed6";
        let gone = "0000000000000000000000000000000000000001";
        let text = format!("{id} commit 212\n{gone} missing\n{id} blob 3\n");
        assert_eq!(batch_checked(&text, 3), Some(vec![None, Some(gone), None]));
        assert_eq!(batch_checked(&text, 2), None);
        assert_eq!(batch_checked(&text, 4), None);
        assert_eq!(batch_checked("", 0), Some(vec![]));
        for odd in [
            "refs/local-ci/heads/x ambiguous\n".to_string(),
            format!("{id}\n"),
            format!("{id} commit\n"),
            format!("{id} commit 212 more\n"),
            format!("{id} commit many\n"),
            format!("{} commit 212\n", &id[..39]),
            "warning: something\n".to_string(),
            "\n".to_string(),
        ] {
            assert_eq!(batch_checked(&odd, 1), None, "{odd:?}");
        }
    }

    #[test]
    fn the_drivers_that_are_given_the_path_are_those_with_a_percent_f_or_a_process() {
        let rec =
            |k: &str, v: Option<&str>| (k.as_bytes().to_vec(), v.map(|v| v.as_bytes().to_vec()));
        let settings = Settings {
            records: vec![
                rec("filter.upper.clean", Some("tr a-z A-Z")),
                rec("filter.upper.smudge", Some("cat %f")),
                rec("filter.upper.required", None),
                rec("filter.lfs.clean", Some("git-lfs clean -- %f")),
                rec("filter.long.process", Some("a-filter")),
                rec("filter.none.process", Some("")),
                rec("filter.none.clean", None),
                rec("filter.a.b.clean", Some("x \"%f\"")),
                rec("core.autocrlf", Some("%f")),
            ],
            attributes_file: None,
        };
        let want: BTreeSet<String> = ["a.b", "lfs", "long"].map(String::from).into();
        assert_eq!(settings.drivers_given_the_path(), want);
    }
}
