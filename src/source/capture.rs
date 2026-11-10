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
    /// Files that match `secret_files`.
    secret: Vec<(usize, RelPath)>,
    stats: BTreeMap<RelPath, Option<Stat>>,
}

struct Selection {
    meta: Meta,
    index_bytes: Vec<u8>,
    /// Tracked files a `secret_files` glob matches: they are commit content,
    /// so they are taken as such (declaration index, path).
    tracked_secrets: Vec<(usize, RelPath)>,
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
        if self.untracked != later.untracked || self.included != later.included {
            return "files were added, removed or renamed".into();
        }
        for (p, s) in &self.stats {
            if later.stats.get(p) != Some(s) {
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
fn select_worktree(ctx: &ExecContext, root: &Path, o: &Opts) -> Result<Selection> {
    let head = og_str(ctx, root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .context("the repository has no commits yet")?;
    let branch = og_str(ctx, root, &["symbolic-ref", "-q", "--short", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty());
    let index_path = og_str(
        ctx,
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?;
    let index_bytes = std::fs::read(&index_path).unwrap_or_default();
    let index_hash = sha256_hex(&index_bytes);

    let mut tracked = vec![];
    for rec in split_z(&og(ctx, root, &["ls-files", "-z", "-s"])?) {
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
        tracked.push((mode, sha, path));
    }
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
        for p in untracked.iter().chain(ignored.iter()) {
            for (i, g) in o.secrets.iter().enumerate() {
                if g.matches(p) {
                    secret.push((i, p.clone()));
                    secret_paths.insert(p.clone());
                    break;
                }
            }
        }
    }

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
        .chain(secret.iter().map(|s| &s.1))
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
        },
        index_bytes,
        tracked_secrets,
    })
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
    let mut stats = BTreeMap::new();
    for p in &files {
        stats.insert(p.clone(), lstat(dir, p));
    }
    Ok(Meta {
        untracked: files,
        stats,
        ..Meta::default()
    })
}

struct Opts {
    exclude: Globs,
    include: Globs,
    secrets: Vec<Globs>,
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

fn read_secret(root: &Path, p: &RelPath, st: &Option<Stat>) -> Result<Option<(u32, Vec<u8>)>> {
    let Some(st) = st else { return Ok(None) };
    let full = root.join(p.to_path());
    let meta = std::fs::symlink_metadata(&full)?;
    if !meta.file_type().is_file() {
        return Err(SourceError::UnsupportedFileType {
            path: p.display(),
            kind: "non-regular secret file".into(),
        }
        .into());
    }
    if Stat::of(&meta) != *st {
        return Err(unstable(format!("{} changed", p.display())));
    }
    let bytes = std::fs::read(&full)?;
    let after = std::fs::symlink_metadata(&full)?;
    if Stat::of(&after) != *st || bytes.len() as u64 != st.size {
        return Err(unstable(format!("{} changed while read", p.display())));
    }
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
    let common = og_str(
        ctx,
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
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
) -> Result<PathBuf> {
    let common = og_str(
        ctx,
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
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
fn check_supported(ctx: &ExecContext, root: &Path) -> Result<()> {
    let shallow = og_str(ctx, root, &["rev-parse", "--is-shallow-repository"]).unwrap_or_default();
    if shallow == "true" {
        return Err(SourceError::ShallowClone.into());
    }
    let format = og_str(ctx, root, &["rev-parse", "--show-object-format"]).unwrap_or_default();
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

fn repository_id(ctx: &ExecContext, root: &Path) -> Result<String> {
    if let Some(slug) = crate::git::repo_slug(ctx, root) {
        return Ok(format!("github:{slug}"));
    }
    let roots = og_str(ctx, root, &["rev-list", "--max-parents=0", "HEAD"])?;
    let first = roots.lines().min().context("no root commit")?;
    Ok(format!("root:{first}"))
}

/// Blobs for the store objects `hashes` in the private repository; returns
/// the git id of each.
fn write_blobs(
    ctx: &ExecContext,
    store: &ObjectStore,
    git_dir: &Path,
    hashes: &[&str],
) -> Result<HashMap<String, String>> {
    if hashes.is_empty() {
        return Ok(HashMap::new());
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
    for line in text.lines() {
        let (m, sha) = line.split_once(' ').context("bad marks file")?;
        let i: usize = m.trim_start_matches(':').parse()?;
        shas.insert(hashes[i - 1].to_string(), sha.to_string());
    }
    Ok(shas)
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

/// `-c key=value` arguments for the settings of the original that decide how
/// content becomes a blob: filters and line-ending conversion. They are passed
/// on the command line, so the original's config is read and never written.
fn filter_config(ctx: &ExecContext, root: &Path) -> Vec<OsString> {
    let out = og(
        ctx,
        root,
        &[
            "config",
            "-z",
            "--get-regexp",
            r"^(filter\..*|core\.(autocrlf|eol|safecrlf|whitespace))$",
        ],
    )
    .unwrap_or_default();
    let mut args = vec![];
    for rec in out.split(|&c| c == 0).filter(|r| !r.is_empty()) {
        let kv = match rec.iter().position(|&c| c == b'\n') {
            Some(i) => [&rec[..i], b"=", &rec[i + 1..]].concat(),
            None => rec.to_vec(),
        };
        args.push(OsString::from("-c"));
        args.push(OsString::from_vec(kv));
    }
    if let Some(p) = global_attributes_path(ctx, root) {
        args.push(OsString::from("-c"));
        let mut kv = b"core.attributesfile=".to_vec();
        kv.extend_from_slice(p.as_os_str().as_bytes());
        args.push(OsString::from_vec(kv));
    }
    args
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
            let name = &e.path.0;
            let is_attrs = name.ends_with(b".gitattributes")
                && (name.len() == 14 || name[name.len() - 15] == b'/');
            if !is_attrs || e.mode == 0o120000 {
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
    let mut rules = vec![];
    if let Some(g) = global_attributes_path(ctx, root) {
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
    root: &Path,
    git_dir: &Path,
    cfg: &[OsString],
    attrs: &AttrTree,
    entries: &[FileEntry],
) -> Result<CanonicalConfig> {
    let get = |k: &str| {
        og_str(ctx, root, &["config", "--get", k])
            .ok()
            .filter(|v| !v.is_empty())
    };
    let rules = attribute_rules(ctx, root, git_dir);
    let paths: Vec<&RelPath> = entries.iter().map(|e| &e.path).collect();
    let used: BTreeSet<String> = check_attr(ctx, git_dir, attrs, cfg, &["filter"], &paths)?
        .into_iter()
        .map(|(_, _, v)| v)
        .filter(|v| !matches!(v.as_str(), "unspecified" | "unset" | "set"))
        .collect();
    let mut defs: BTreeMap<String, FilterDef> = BTreeMap::new();
    if !used.is_empty() {
        let out = og(
            ctx,
            root,
            &["config", "-z", "--get-regexp", r"^filter\..*\.[^.]+$"],
        )
        .unwrap_or_default();
        for rec in out.split(|&c| c == 0).filter(|r| !r.is_empty()) {
            let text = String::from_utf8_lossy(rec).into_owned();
            let (key, val) = text.split_once('\n').unwrap_or((text.as_str(), ""));
            let Some((name, var)) = key.strip_prefix("filter.").and_then(|k| k.rsplit_once('.'))
            else {
                continue;
            };
            if !used.contains(name) {
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
/// into Git: their blob is their bytes.
fn untouched_paths(
    ctx: &ExecContext,
    root: &Path,
    git_dir: &Path,
    attrs: &AttrTree,
    cfg: &[OsString],
    paths: &[&RelPath],
) -> Result<BTreeSet<RelPath>> {
    let autocrlf = og_str(ctx, root, &["config", "--get", "core.autocrlf"]).unwrap_or_default();
    if !(autocrlf.is_empty() || autocrlf == "false") || paths.is_empty() {
        return Ok(BTreeSet::new());
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
    let touched: BTreeSet<RelPath> = found
        .into_iter()
        .filter(|(_, _, v)| v != "unspecified")
        .map(|(p, _, _)| p)
        .collect();
    Ok(paths
        .iter()
        .filter(|p| !touched.contains(**p))
        .map(|p| (*p).clone())
        .collect())
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
    for line in String::from_utf8_lossy(&out).lines() {
        let Some(sha) = line.strip_suffix(" missing") else {
            continue;
        };
        let content = og(ctx, root, &["cat-file", "blob", sha])?;
        let got = hash_plain(ctx, git_dir, content)?;
        if got != sha {
            bail!("blob {sha} was copied as {got}");
        }
    }
    Ok(())
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
    cfg: &[OsString],
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
    let listing = og(ctx, root, &["ls-files", "-v", "-z"])?;
    for rec in split_z(&listing) {
        if rec.len() > 2 && (rec[0] == b'S' || rec[0].is_ascii_lowercase()) {
            changed.insert(RelPath(rec[2..].to_vec()));
        }
    }

    let mut shas: Vec<Option<String>> = vec![None; entries.len()];
    let mut from_index: Vec<String> = vec![];
    let mut need_attrs = vec![];
    for (i, e) in entries.iter().enumerate() {
        if e.mode == 0o120000 || lfs_by.contains_key(&e.path) {
            continue;
        }
        match index.get(&e.path) {
            Some(sha) if !changed.contains(&e.path) => {
                shas[i] = Some((*sha).to_string());
                from_index.push((*sha).to_string());
            }
            _ => need_attrs.push(i),
        }
    }
    let untouched = untouched_paths(
        ctx,
        root,
        git_dir,
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
    let raw_ids = write_blobs(ctx, store, git_dir, &raw)?;

    for (i, e) in entries.iter().enumerate() {
        if shas[i].is_some() {
            continue;
        }
        shas[i] = Some(if e.mode == 0o120000 {
            raw_ids[&e.hash].clone()
        } else if let Some(l) = lfs_by.get(&e.path) {
            hash_plain(ctx, git_dir, lfs_pointer_text(&l.oid, l.size).into_bytes())?
        } else if untouched.contains(&e.path) {
            raw_ids[&e.hash].clone()
        } else {
            hash_filtered(ctx, git_dir, attrs, cfg, &e.path, store.read(&e.hash)?)?
        });
    }
    let from_index: Vec<&str> = from_index.iter().map(String::as_str).collect();
    copy_blobs(ctx, root, git_dir, &from_index)?;
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
/// and not committed exist only in the original: they are copied here first.
fn index_tree(
    ctx: &ExecContext,
    store: &ObjectStore,
    root: &Path,
    git_dir: &Path,
    bytes: &[u8],
) -> Result<String> {
    let idx = store::tmp_name(&store.tmp_dir(), "index");
    std::fs::write(&idx, bytes)?;
    let res = (|| -> Result<String> {
        let env = [("GIT_INDEX_FILE", idx.as_os_str())];
        let listing = pg(ctx, git_dir, &["ls-files", "-z", "-s"], &env, None)?;
        let mut shas = vec![];
        for rec in split_z(&listing) {
            let text = String::from_utf8_lossy(rec);
            let mut it = text.split(' ');
            let (mode, sha) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            if mode != "160000" && !sha.is_empty() {
                shas.push(sha.to_string());
            }
        }
        shas.sort();
        shas.dedup();
        let shas: Vec<&str> = shas.iter().map(String::as_str).collect();
        copy_blobs(ctx, root, git_dir, &shas)?;
        let out = pg(ctx, git_dir, &["write-tree"], &env, None)?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    })();
    let _ = std::fs::remove_file(&idx);
    res
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
    })
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
    for (i, p) in &meta.secret {
        if let Some((mode, bytes)) = read_secret(root, p, &meta.stats[p])? {
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
    if !opts.secret_files.is_empty() && opts.secret_key.len() < MIN_SECRET_KEY {
        return Err(SourceError::SecretKeyTooShort {
            len: opts.secret_key.len(),
        }
        .into());
    }
    capture_worktree(ctx, repo_dir, opts, &store_of(ctx, opts)?).map(|(snap, _)| snap)
}

/// The directories of a capture and what they looked like when it read them:
/// the repository itself, then every submodule below it.
type Seen = Vec<(PathBuf, Meta)>;

fn capture_worktree(
    ctx: &ExecContext,
    repo_dir: &Path,
    opts: &CaptureOptions,
    store: &ObjectStore,
) -> Result<(SourceSnapshot, Seen)> {
    let top = og_str(ctx, repo_dir, &["rev-parse", "--show-toplevel"])
        .context("not inside a git repository")?;
    let root = std::fs::canonicalize(top)?;
    let cwd = std::fs::canonicalize(repo_dir)?;
    let cwd_relative = RelPath::new(cwd.strip_prefix(&root).unwrap_or(Path::new("")));
    let o = build_opts(opts)?;
    check_supported(ctx, &root)?;
    let repo_id = repository_id(ctx, &root)?;
    let git_dir = private_repo(ctx, store, &root, &repo_id)?;

    let mut last = String::new();
    let attempts = opts.max_attempts.max(1);
    for attempt in 1..=attempts {
        match one_pass(ctx, &root, opts, &o, store, attempt) {
            Ok(p) => {
                let mut seen = vec![(root.clone(), p.sel.meta.clone())];
                seen.extend(p.seen.iter().cloned());
                let snap = finish(ctx, store, &root, &git_dir, opts, repo_id, cwd_relative, p)?;
                return Ok((snap, seen));
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
    opts: &CaptureOptions,
    o: &Opts,
    store: &ObjectStore,
    attempt: u32,
) -> Result<Stable> {
    let sel = select_worktree(ctx, root, o)?;
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
            let (snap, sub_seen) = capture_worktree(ctx, &sub, &inner, store)
                .with_context(|| format!("submodule {}", path.display()))?;
            seen.extend(sub_seen);
            let m = &snap.manifest;
            let head_tree = og_str(ctx, &sub, &["rev-parse", "HEAD^{tree}"]).unwrap_or_default();
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
    let mut read = read_all(store, root, &sel.meta, &gitlink_paths)?;
    let lfs = resolve_lfs(ctx, store, root, &mut read.entries)?;
    opts.call(Stage::AfterRead(attempt))?;
    let again = select_worktree(ctx, root, o)?;
    if again.meta != sel.meta {
        return Err(unstable(sel.meta.diff(&again.meta)));
    }
    // the stats of what was read are the stats now
    for p in sel.meta.stats.keys() {
        if lstat(root, p) != sel.meta.stats[p] {
            return Err(unstable(format!("{} changed", p.display())));
        }
    }
    // and a submodule is as it was when its own capture settled: its HEAD,
    // index and every file in it
    let plain = build_opts(&CaptureOptions::default())?;
    for (dir, was) in &seen {
        let now = select_worktree(ctx, dir, &plain)?;
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
    import_history(ctx, git_dir, root, &head)?;
    let cfg = filter_config(ctx, root);
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
        &cfg,
    )?;
    let canonical = canonical_config(ctx, root, git_dir, &cfg, &attrs, &p.read.entries)?;
    let worktree_tree = build_tree(ctx, store, git_dir, &p.read.entries, &shas, &gitlinks)?;
    let commit = synthetic_commit(ctx, git_dir, &worktree_tree, &head)?;
    let itree = index_tree(ctx, store, root, git_dir, &p.sel.index_bytes)?;

    let base = opts
        .base
        .clone()
        .or_else(|| crate::git::default_base(ctx, root).ok());
    let merge_base = base.as_deref().and_then(|b| {
        og_str(ctx, root, &["merge-base", b, "HEAD"])
            .ok()
            .filter(|s| !s.is_empty())
    });

    let mut secret_files = vec![];
    let mut bundle_files = vec![];
    for (i, path, mode, bytes) in &p.read.secrets {
        let _ = i;
        secret_files.push(SecretEntry {
            path: path.clone(),
            hmac: secret::secret_digest(&opts.secret_key, path, bytes),
        });
        bundle_files.push((path.clone(), *mode, bytes.clone()));
    }
    for (_, path) in &p.sel.tracked_secrets {
        warnings.push(format!(
            "{} is tracked, so it is commit content and not a secret file",
            path.display()
        ));
    }
    for (i, spec) in opts.secret_files.iter().enumerate() {
        if p.read.secrets.iter().all(|s| s.0 != i) && p.sel.tracked_secrets.iter().all(|s| s.0 != i)
        {
            if spec.required {
                return Err(SourceError::RequiredSecretMissing {
                    glob: spec.glob.clone(),
                }
                .into());
            }
            warnings.push(format!("secret_files {:?} matches no file", spec.glob));
        }
    }
    secret_files.sort_by(|a, b| a.path.cmp(&b.path));

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
    pg(
        ctx,
        git_dir,
        &[
            "update-ref",
            &format!("refs/local-ci/snapshots/{}", m.source_id),
            &commit,
        ],
        &[],
        None,
    )?;
    let secrets = if bundle_files.is_empty() {
        None
    } else {
        let path = store.secrets_path(&m.source_id);
        SecretBundle::new(bundle_files).save(&path)?;
        Some(path)
    };
    opts.call(Stage::BeforePublish)?;
    store.publish_manifest(&m)?;
    Ok(SourceSnapshot {
        manifest: m,
        secrets,
        warnings,
    })
}

/// Capture a directory that is not a repository: the same manifest, without
/// Git.
pub fn capture_directory(
    ctx: &ExecContext,
    dir: &Path,
    opts: &CaptureOptions,
) -> Result<SourceSnapshot> {
    let store = store_of(ctx, opts)?;
    let root = std::fs::canonicalize(dir)?;
    let o = build_opts(opts)?;
    let mut warnings = vec![];
    let attempts = opts.max_attempts.max(1);
    let mut last = String::new();
    for attempt in 1..=attempts {
        let attempt_res = (|| -> Result<(Meta, Read, Vec<LfsRef>)> {
            let meta = select_directory(&root, &o, &mut warnings)?;
            let read = read_all(&store, &root, &meta, &BTreeSet::new())?;
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
                    secret_files: vec![],
                });
                opts.call(Stage::BeforePublish)?;
                store.publish_manifest(&m)?;
                warnings.sort();
                warnings.dedup();
                return Ok(SourceSnapshot {
                    manifest: m,
                    secrets: None,
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
