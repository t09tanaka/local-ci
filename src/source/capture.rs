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
use std::os::unix::ffi::OsStrExt;
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
    let mut c = ctx.command("git");
    c.args(pre).args(args);
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
        bail!(
            "git {}: {}",
            args.join(" "),
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

fn og_str(ctx: &ExecContext, root: &Path, args: &[&str]) -> Result<String> {
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

    // secrets: any file the declaration matches, tracked or not, ignored or not
    let mut secret = vec![];
    let mut secret_paths = BTreeSet::new();
    if !o.secrets.is_empty() {
        let all = tracked
            .iter()
            .filter(|t| t.0 != 0o160000)
            .map(|t| &t.2)
            .chain(untracked.iter())
            .chain(ignored.iter());
        for p in all {
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
            if m.is_dir() {
                if n.as_bytes() == b".git" {
                    warnings.push(format!("{}: a .git directory is left out", p.display()));
                    continue;
                }
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

/// The bare repository that holds the synthetic commits of one repository. It
/// reads the original's objects through `alternates` and writes only its own.
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
    let format = og_str(ctx, root, &["rev-parse", "--show-object-format"]).unwrap_or_default();
    if !format.is_empty() && format != "sha1" {
        bail!("object format {format} is not supported");
    }
    let key = &sha256_hex(format!("{repository_id}\0{common}").as_bytes())[..16];
    let dir = store.git_dir(key);
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
    let alt = format!("{common}/objects\n");
    write_atomic(&dir.join("objects/info/alternates"), alt.as_bytes(), 0o644)?;
    Ok(dir)
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

/// The tree of `entries` and `gitlinks` in the private repository.
fn build_tree(
    ctx: &ExecContext,
    store: &ObjectStore,
    git_dir: &Path,
    entries: &[FileEntry],
    gitlinks: &[(RelPath, String)],
) -> Result<String> {
    let mut uniq: Vec<&str> = entries.iter().map(|e| e.hash.as_str()).collect();
    uniq.sort();
    uniq.dedup();
    let shas = write_blobs(ctx, store, git_dir, &uniq)?;
    let mut input = vec![];
    for e in entries {
        input.extend_from_slice(format!("{:o} {}\t", e.mode, shas[&e.hash]).as_bytes());
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

/// The tree of the index, taken from a copy of it.
fn index_tree(
    ctx: &ExecContext,
    store: &ObjectStore,
    git_dir: &Path,
    bytes: &[u8],
) -> Result<String> {
    let idx = store::tmp_name(&store.tmp_dir(), "index");
    std::fs::write(&idx, bytes)?;
    let res = pg(
        ctx,
        git_dir,
        &["write-tree"],
        &[("GIT_INDEX_FILE", idx.as_os_str())],
        None,
    );
    let _ = std::fs::remove_file(&idx);
    Ok(String::from_utf8_lossy(&res?).trim().to_string())
}

fn synthetic_commit(ctx: &ExecContext, git_dir: &Path, tree: &str, parent: &str) -> Result<String> {
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
    let out = pg(
        ctx,
        git_dir,
        &[
            "commit-tree",
            tree,
            "-p",
            parent,
            "-m",
            "local-ci source snapshot",
        ],
        &env,
        None,
    )?;
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
    capture_worktree(ctx, repo_dir, opts, &store_of(ctx, opts)?)
}

fn capture_worktree(
    ctx: &ExecContext,
    repo_dir: &Path,
    opts: &CaptureOptions,
    store: &ObjectStore,
) -> Result<SourceSnapshot> {
    let top = og_str(ctx, repo_dir, &["rev-parse", "--show-toplevel"])
        .context("not inside a git repository")?;
    let root = std::fs::canonicalize(top)?;
    let cwd = std::fs::canonicalize(repo_dir)?;
    let cwd_relative = RelPath::new(cwd.strip_prefix(&root).unwrap_or(Path::new("")));
    let o = build_opts(opts)?;
    let repo_id = repository_id(ctx, &root)?;
    let git_dir = private_repo(ctx, store, &root, &repo_id)?;

    let mut last = String::new();
    let attempts = opts.max_attempts.max(1);
    for attempt in 1..=attempts {
        match one_pass(ctx, &root, opts, &o, store, &git_dir, attempt) {
            Ok(p) => return finish(ctx, store, &root, &git_dir, opts, repo_id, cwd_relative, p),
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
}

/// Read everything once, then look again: `Err(Unstable)` when it moved.
fn one_pass(
    ctx: &ExecContext,
    root: &Path,
    opts: &CaptureOptions,
    o: &Opts,
    store: &ObjectStore,
    git_dir: &Path,
    attempt: u32,
) -> Result<Stable> {
    let _ = git_dir;
    let sel = select_worktree(ctx, root, o)?;
    // initialized submodules are captured whole, each with its own retries
    let mut submodules = vec![];
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
            let snap = capture_worktree(ctx, &sub, &inner, store)
                .with_context(|| format!("submodule {}", path.display()))?;
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
    Ok(Stable {
        sel,
        read,
        lfs,
        submodules,
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
    let worktree_tree = build_tree(ctx, store, git_dir, &p.read.entries, &gitlinks)?;
    let commit = synthetic_commit(ctx, git_dir, &worktree_tree, &head)?;
    let itree = index_tree(ctx, store, git_dir, &p.sel.index_bytes)?;

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
    for (i, spec) in opts.secret_files.iter().enumerate() {
        if p.read.secrets.iter().all(|s| s.0 != i) {
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
