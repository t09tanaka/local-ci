//! A digest of a directory tree by its contents.
//!
//! [`tree_digest`] hashes, for every entry under a path in a fixed order, its
//! path relative to the tree, its kind, whether a file is executable and the
//! hash of its contents (a symlink: the value of the link; a directory: it
//! lists its entries). Two trees have the same digest exactly when they have
//! the same names, kinds, executable bits and contents, wherever they are and
//! whoever made them.
//!
//! Only the executable bit of a file is part of the digest. The other
//! permission bits are the host's policy (umask, a template sealed read-only
//! and made writable again) and differ between copies of one tree.
//!
//! Modification times, owners and inode numbers are not part of it either:
//! they say nothing about what a tool reading the tree will see.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The digest of a tree and how big the tree is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeDigest {
    /// `sha256:<hex>`.
    pub digest: String,
    /// Files (and symlinks) under the path; directories are not counted.
    pub files: u64,
    /// Bytes in those files.
    pub bytes: u64,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// What one entry of the tree is, for the line it adds to the digest.
enum Entry {
    Dir,
    Symlink(String),
    File { path: PathBuf, exec: bool },
    Other,
}

/// The digest of the tree at `path`: a directory (recursively), a file or a
/// symlink (not followed). Fails when something under it cannot be read: a
/// digest of part of a tree is not a digest of the tree.
///
/// The tree is listed first, in order; the files are then hashed on several
/// threads (the cost is mostly opening many small files), and the lines are
/// put together in the listed order, so the result does not depend on how the
/// work was shared out.
pub fn tree_digest(path: &Path) -> Result<TreeDigest> {
    let mut entries: Vec<(String, Entry)> = Vec::new();
    list(path, "", &mut entries)?;
    let files: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, (_, e))| matches!(e, Entry::File { .. }))
        .map(|(i, _)| i)
        .collect();
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8)
        .min(files.len().max(1));
    let hashed: Vec<Result<(usize, String, u64)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut out = Vec::new();
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&i) = files.get(k) else { break };
                        let Entry::File { path, .. } = &entries[i].1 else {
                            continue;
                        };
                        out.push(hash_file(path, &mut buf).map(|(h, n)| (i, h, n)));
                    }
                    out
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("a hashing thread panicked"))
            .collect()
    });
    let mut by_index: Vec<Option<(String, u64)>> = vec![None; entries.len()];
    for r in hashed {
        let (i, h, n) = r?;
        by_index[i] = Some((h, n));
    }
    let mut hasher = Sha256::new();
    let (mut count, mut bytes) = (0, 0);
    for (i, (rel, e)) in entries.iter().enumerate() {
        // one line per entry; the fields cannot contain a NUL, and an escaped
        // name cannot contain a newline
        let rel = rel.escape_default();
        let line = match e {
            Entry::Dir => format!("{rel}\0d\0-\0-\n"),
            Entry::Symlink(t) => {
                count += 1;
                format!("{rel}\0l\0-\0{t}\n")
            }
            Entry::File { exec, .. } => {
                let (h, n) = by_index[i].take().expect("every file was hashed");
                count += 1;
                bytes += n;
                format!("{rel}\0f\0{}\0{h}\n", if *exec { "x" } else { "-" })
            }
            Entry::Other => {
                count += 1;
                format!("{rel}\0o\0-\0-\n")
            }
        };
        hasher.update(line);
    }
    Ok(TreeDigest {
        digest: format!("sha256:{}", hex(&hasher.finalize())),
        files: count,
        bytes,
    })
}

fn hash_file(path: &Path, buf: &mut [u8]) -> Result<(String, u64)> {
    let mut f = fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut h = Sha256::new();
    let mut size = 0u64;
    loop {
        let n = f
            .read(buf)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex(&h.finalize()), size))
}

fn list(path: &Path, rel: &str, out: &mut Vec<(String, Entry)>) -> Result<()> {
    let md =
        fs::symlink_metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    let ft = md.file_type();
    if ft.is_symlink() {
        let target =
            fs::read_link(path).with_context(|| format!("cannot read {}", path.display()))?;
        out.push((
            rel.to_string(),
            Entry::Symlink(hex(target.to_string_lossy().as_bytes())),
        ));
    } else if ft.is_dir() {
        out.push((rel.to_string(), Entry::Dir));
        let mut names: Vec<_> = fs::read_dir(path)
            .with_context(|| format!("cannot read {}", path.display()))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("cannot read {}", path.display()))?;
        names.sort_by(|a, b| {
            use std::os::unix::ffi::OsStrExt;
            a.as_bytes().cmp(b.as_bytes())
        });
        for name in names {
            let n = name.to_string_lossy();
            let child = if rel.is_empty() {
                n.to_string()
            } else {
                format!("{rel}/{n}")
            };
            list(&path.join(&name), &child, out)?;
        }
    } else if ft.is_file() {
        use std::os::unix::fs::PermissionsExt;
        out.push((
            rel.to_string(),
            Entry::File {
                path: path.to_path_buf(),
                exec: md.permissions().mode() & 0o111 != 0,
            },
        ));
    } else {
        out.push((rel.to_string(), Entry::Other));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn sample(dir: &Path) {
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/b/f.txt"), "one").unwrap();
        fs::write(dir.join("a/g.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("a/g.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("b/f.txt", dir.join("a/link")).unwrap();
    }

    fn digest(p: &Path) -> String {
        tree_digest(p).unwrap().digest
    }

    #[test]
    fn same_contents_same_digest_wherever_the_tree_is() {
        let t = tempfile::tempdir().unwrap();
        sample(&t.path().join("x"));
        sample(&t.path().join("y"));
        let d = tree_digest(&t.path().join("x")).unwrap();
        assert_eq!(d.digest, digest(&t.path().join("y")));
        assert_eq!(d.digest, digest(&t.path().join("x")));
        assert!(d.digest.starts_with("sha256:"));
        assert_eq!((d.files, d.bytes), (3, 13));
    }

    #[test]
    fn a_change_of_content_mode_link_name_or_a_missing_file_changes_it() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("x");
        sample(&root);
        let base = digest(&root);
        let changed = |f: &dyn Fn(&Path)| {
            let copy = t.path().join("c");
            let _ = fs::remove_dir_all(&copy);
            sample(&copy);
            f(&copy);
            digest(&copy)
        };
        // same size, other content
        assert_ne!(
            base,
            changed(&|r| fs::write(r.join("a/b/f.txt"), "two").unwrap())
        );
        // the executable bit
        assert_ne!(
            base,
            changed(&|r| fs::set_permissions(
                r.join("a/b/f.txt"),
                fs::Permissions::from_mode(0o755)
            )
            .unwrap())
        );
        // the value of a link, with the target the same size
        assert_ne!(
            base,
            changed(&|r| {
                fs::remove_file(r.join("a/link")).unwrap();
                symlink("b/f.tx_", r.join("a/link")).unwrap();
            })
        );
        // a link replaced by a file with the content of its target
        assert_ne!(
            base,
            changed(&|r| {
                fs::remove_file(r.join("a/link")).unwrap();
                fs::write(r.join("a/link"), "one").unwrap();
            })
        );
        // a name
        assert_ne!(
            base,
            changed(&|r| fs::rename(r.join("a/g.sh"), r.join("a/h.sh")).unwrap())
        );
        // a file gone, an empty directory added
        assert_ne!(
            base,
            changed(&|r| fs::remove_file(r.join("a/g.sh")).unwrap())
        );
        assert_ne!(
            base,
            changed(&|r| fs::create_dir(r.join("a/empty")).unwrap())
        );
    }

    #[test]
    fn read_and_write_bits_and_times_are_not_part_of_it() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("x");
        sample(&root);
        let base = digest(&root);
        fs::set_permissions(root.join("a/b/f.txt"), fs::Permissions::from_mode(0o444)).unwrap();
        fs::write(root.join("a/b/f.txt"), "one").ok();
        assert_eq!(base, digest(&root));
    }

    #[test]
    fn a_missing_path_or_an_unreadable_file_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        assert!(tree_digest(&t.path().join("nope")).is_err());
        let root = t.path().join("x");
        sample(&root);
        fs::set_permissions(root.join("a/b/f.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        // root can read anything; the check means something only for others
        if fs::File::open(root.join("a/b/f.txt")).is_err() {
            assert!(tree_digest(&root).is_err());
        }
    }

    /// How long a tree of 10 000 files takes (run with `--ignored --nocapture`).
    #[test]
    #[ignore]
    fn bench_ten_thousand_files() {
        let t = tempfile::tempdir().unwrap();
        let body = vec![b'x'; 12 * 1024];
        for d in 0..100 {
            let dir = t.path().join(format!("pkg{d}/lib"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..100 {
                fs::write(dir.join(format!("f{f}.js")), &body).unwrap();
            }
        }
        let t0 = std::time::Instant::now();
        let d = tree_digest(t.path()).unwrap();
        let first = t0.elapsed();
        let t1 = std::time::Instant::now();
        let again = tree_digest(t.path()).unwrap();
        println!(
            "tree_digest: {} files, {} bytes: {:?} (second run {:?})",
            d.files,
            d.bytes,
            first,
            t1.elapsed()
        );
        assert_eq!(d, again);
        assert_eq!(d.files, 10_000);
    }
}
