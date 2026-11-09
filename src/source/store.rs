//! Paths as bytes, and the content-addressed store.

use super::{SourceError, SourceManifest};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A path relative to a root, as the bytes the file system has. In JSON it is
/// a string when it is UTF-8 and `{"hex": "..."}` when it is not.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(pub Vec<u8>);

impl RelPath {
    pub fn new(p: impl AsRef<OsStr>) -> Self {
        RelPath(p.as_ref().as_bytes().to_vec())
    }
    pub fn to_path(&self) -> PathBuf {
        PathBuf::from(OsString::from_vec(self.0.clone()))
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// For messages only.
    pub fn display(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
    pub(crate) fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0.split(|&b| b == b'/')
    }
    /// `a/b/c` -> `a`, `a/b` (the directories above the last component).
    pub(crate) fn ancestors(&self) -> Vec<RelPath> {
        let mut out = vec![];
        for (i, b) in self.0.iter().enumerate() {
            if *b == b'/' {
                out.push(RelPath(self.0[..i].to_vec()));
            }
        }
        out
    }
    pub(crate) fn starts_with_dir(&self, dir: &RelPath) -> bool {
        self.0.len() > dir.0.len() && self.0.starts_with(&dir.0) && self.0[dir.0.len()] == b'/'
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Repr {
    Text(String),
    Bytes { hex: String },
}

impl Serialize for RelPath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match std::str::from_utf8(&self.0) {
            Ok(t) => Repr::Text(t.to_string()).serialize(s),
            Err(_) => Repr::Bytes { hex: hex(&self.0) }.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for RelPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Repr::deserialize(d)? {
            Repr::Text(t) => Ok(RelPath(t.into_bytes())),
            Repr::Bytes { hex: h } => unhex(&h)
                .map(RelPath)
                .ok_or_else(|| serde::de::Error::custom("bad hex path")),
        }
    }
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[allow(clippy::manual_is_multiple_of)] // is_multiple_of is newer than the MSRV
pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

pub(crate) fn sha256_hex(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A name for a temporary file next to its destination.
pub(crate) fn tmp_name(dir: &Path, stem: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{stem}.tmp-{}-{n}", std::process::id()))
}

/// Write `bytes` to `dest` atomically (a temporary file, then a rename), with
/// `mode`.
pub(crate) fn write_atomic(dest: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = dest.parent().context("no parent directory")?;
    std::fs::create_dir_all(dir)?;
    let tmp = tmp_name(dir, "write");
    let res = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dest)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// The private store of captured sources, under one directory:
///
/// - `objects/<aa>/<hash>`: content, named by its SHA-256
/// - `manifests/<source_id>.json`: published manifests (written last, by
///   rename, so a manifest that is there is complete)
/// - `secrets/<source_id>.json`: secret bundles (0600)
/// - `git/<key>.git`: the private bare repository of synthetic commits
#[derive(Debug, Clone)]
pub struct ObjectStore {
    root: PathBuf,
}

impl ObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for d in ["objects", "manifests", "secrets", "git", "tmp"] {
            std::fs::create_dir_all(root.join(d))
                .with_context(|| format!("cannot create the source store at {}", root.display()))?;
        }
        Ok(ObjectStore { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub(crate) fn git_dir(&self, key: &str) -> PathBuf {
        self.root.join("git").join(format!("{key}.git"))
    }

    pub fn object_path(&self, hash: &str) -> PathBuf {
        self.root.join("objects").join(&hash[..2]).join(hash)
    }

    pub fn has(&self, hash: &str) -> bool {
        hash.len() == 64 && self.object_path(hash).is_file()
    }

    /// Store what `r` yields; returns its hash and size. Content that is
    /// already there is not written again.
    pub fn put_reader(&self, r: &mut impl Read) -> Result<(String, u64)> {
        let tmp = tmp_name(&self.tmp_dir(), "obj");
        let res = (|| -> Result<(String, u64)> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            let mut h = Sha256::new();
            let mut n = 0u64;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let k = r.read(&mut buf)?;
                if k == 0 {
                    break;
                }
                h.update(&buf[..k]);
                f.write_all(&buf[..k])?;
                n += k as u64;
            }
            f.sync_all()?;
            let hash = hex(&h.finalize());
            let dest = self.object_path(&hash);
            if dest.exists() {
                std::fs::remove_file(&tmp)?;
            } else {
                std::fs::create_dir_all(dest.parent().unwrap())?;
                std::fs::rename(&tmp, &dest)?;
            }
            Ok((hash, n))
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res
    }

    pub fn put_bytes(&self, b: &[u8]) -> Result<(String, u64)> {
        self.put_reader(&mut &b[..])
    }

    pub fn read(&self, hash: &str) -> Result<Vec<u8>> {
        if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("not a content hash: {hash}");
        }
        std::fs::read(self.object_path(hash))
            .with_context(|| format!("object {hash} is not in the source store"))
    }

    pub fn manifest_path(&self, source_id: &str) -> PathBuf {
        self.root
            .join("manifests")
            .join(format!("{source_id}.json"))
    }

    pub fn secrets_path(&self, source_id: &str) -> PathBuf {
        self.root.join("secrets").join(format!("{source_id}.json"))
    }

    /// Make the manifest visible, all at once.
    pub(crate) fn publish_manifest(&self, m: &SourceManifest) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(m)?;
        write_atomic(&self.manifest_path(&m.source_id), &bytes, 0o644)
    }

    /// A published manifest; `None` when there is none (not captured, or a
    /// capture that stopped before it was published).
    pub fn load_manifest(&self, source_id: &str) -> Result<Option<SourceManifest>> {
        if !source_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            bail!("not a source id: {source_id}");
        }
        let p = self.manifest_path(source_id);
        match std::fs::read(&p) {
            Ok(b) => {
                let m: SourceManifest = serde_json::from_slice(&b)
                    .with_context(|| format!("{} is not a manifest", p.display()))?;
                super::capture::check_digest(&m)?;
                Ok(Some(m))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// `p` has to be a plain relative path to be written under a workspace:
/// no root, no `..`, no empty or `.` components, nothing in `.git`.
pub(crate) fn check_safe(p: &RelPath) -> Result<()> {
    let bad = |why: &str| -> anyhow::Error {
        SourceError::UnsafePath {
            path: p.display(),
            why: why.into(),
        }
        .into()
    };
    if p.0.is_empty() {
        return Err(bad("empty"));
    }
    if p.0.contains(&0) {
        return Err(bad("contains NUL"));
    }
    for c in p.components() {
        match c {
            b"" => return Err(bad("empty component or absolute path")),
            b"." | b".." => return Err(bad("`.` or `..` component")),
            c if c.eq_ignore_ascii_case(b".git") => return Err(bad("inside .git")),
            _ => {}
        }
    }
    Ok(())
}

/// Create the directories above `dest/p`, refusing to go through a symlink
/// or a file; returns the path to write.
pub(crate) fn prepare_parent(dest: &Path, p: &RelPath) -> Result<PathBuf> {
    check_safe(p)?;
    let mut cur = dest.to_path_buf();
    for a in p.ancestors() {
        let comp = a.0.rsplit(|&b| b == b'/').next().unwrap();
        cur.push(OsStr::from_bytes(comp));
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(SourceError::UnsafePath {
                    path: p.display(),
                    why: format!("{} is not a directory (a symlink or a file)", a.display()),
                }
                .into())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&cur)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(dest.join(p.to_path()))
}
