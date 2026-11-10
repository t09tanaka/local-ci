//! `secret_files`: ignored files a run needs (a test `.env`), kept out of the
//! store and the manifest.

use super::store::{hex, tmp_name, unhex, write_atomic, RelPath};
use super::SourceError;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.iter().map(|b| b ^ 0x36).collect::<Vec<_>>());
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.iter().map(|b| b ^ 0x5c).collect::<Vec<_>>());
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// The digest the manifest records for one secret file: an HMAC over the path
/// and the content, so equal content at two paths gives two digests.
pub fn secret_digest(key: &[u8], path: &RelPath, content: &[u8]) -> String {
    let mut msg = path.0.clone();
    msg.push(0);
    msg.extend_from_slice(content);
    format!("hmac-sha256:{}", hex(&hmac_sha256(key, &msg)))
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SecretFile {
    pub path: RelPath,
    pub mode: u32,
    pub content_hex: String,
}

/// The content of the secret files of one source. Lives in a file of its own
/// (0600), never in the object store or the manifest.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretBundle {
    pub schema_version: u32,
    pub(crate) files: Vec<SecretFile>,
}

/// Names the files and never shows what is in them.
impl std::fmt::Debug for SecretBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretBundle")
            .field("schema_version", &self.schema_version)
            .field(
                "paths",
                &self.paths().iter().map(|p| p.display()).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl SecretBundle {
    pub fn new(files: Vec<(RelPath, u32, Vec<u8>)>) -> Self {
        SecretBundle {
            schema_version: 1,
            files: files
                .into_iter()
                .map(|(path, mode, c)| SecretFile {
                    path,
                    mode,
                    content_hex: hex(&c),
                })
                .collect(),
        }
    }

    pub fn paths(&self) -> Vec<&RelPath> {
        self.files.iter().map(|f| &f.path).collect()
    }

    pub(crate) fn content(&self, f: &SecretFile) -> Result<Vec<u8>> {
        unhex(&f.content_hex).context("secret bundle: bad content")
    }

    /// Save with mode 0600, atomically.
    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec(self)?, 0o600)
    }

    /// Save with mode 0600 at a path that must not exist yet, in a directory
    /// that must: nothing is replaced and no directory is made. The file
    /// appears complete or not at all.
    pub fn save_new(&self, path: &Path) -> Result<()> {
        let dir = check_new_path(path)?;
        let refused = |why: String| -> anyhow::Error {
            SourceError::SecretBundlePath {
                path: path.display().to_string(),
                why,
            }
            .into()
        };
        let tmp = tmp_name(dir, "secrets");
        let res = (|| -> Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec(self)?)?;
            f.sync_all()?;
            // a link, unlike a rename, fails when the name is taken
            std::fs::hard_link(&tmp, path).map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => refused("the file already exists".into()),
                _ => refused(e.to_string()),
            })
        })();
        let _ = std::fs::remove_file(&tmp);
        res
    }

    pub fn load(path: &Path) -> Result<Self> {
        serde_json::from_slice(&std::fs::read(path)?).context("not a secret bundle")
    }
}

/// The directory of a bundle that [`SecretBundle::save_new`] may write:
/// refused when `path` exists (as anything, a dangling symlink included) or
/// its directory does not.
pub(crate) fn check_new_path(path: &Path) -> Result<&Path> {
    let refused = |why: &str| -> anyhow::Error {
        SourceError::SecretBundlePath {
            path: path.display().to_string(),
            why: why.into(),
        }
        .into()
    };
    if path.file_name().is_none() {
        return Err(refused("not the path of a file"));
    }
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    if !dir.is_dir() {
        return Err(refused("its directory does not exist"));
    }
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(refused("the file already exists"));
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        // test case 1
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // test case 6: a key longer than the block
        let mac = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn debug_output_names_the_files_and_not_their_content() {
        let b = SecretBundle::new(vec![(
            RelPath::new(".env"),
            0o600,
            b"hunter2-token".to_vec(),
        )]);
        let shown = format!("{b:?}");
        assert!(shown.contains(".env"));
        assert!(!shown.contains("hunter2"));
        assert!(!shown.contains(&hex(b"hunter2-token")));
    }
}

#[cfg(test)]
mod save_new_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn bundle() -> SecretBundle {
        SecretBundle::new(vec![(RelPath::new(".env"), 0o600, b"TOKEN=1\n".to_vec())])
    }

    fn why(e: anyhow::Error) -> String {
        match e.downcast_ref::<SourceError>() {
            Some(SourceError::SecretBundlePath { why, .. }) => why.clone(),
            other => panic!("{other:?}: {e:#}"),
        }
    }

    #[test]
    fn a_new_file_is_written_whole_at_0600_and_no_temporary_file_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("b.json");
        bundle().save_new(&path).unwrap();
        assert_eq!(SecretBundle::load(&path).unwrap(), bundle());
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 1);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn an_existing_name_is_never_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("b.json");
        std::fs::write(&path, "kept").unwrap();
        assert_eq!(
            why(bundle().save_new(&path).unwrap_err()),
            "the file already exists"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept");
        // a directory and a dangling symlink are names that are taken, too
        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(
            why(bundle().save_new(&dir).unwrap_err()),
            "the file already exists"
        );
        let link = tmp.path().join("l");
        std::os::unix::fs::symlink(tmp.path().join("target"), &link).unwrap();
        assert_eq!(
            why(bundle().save_new(&link).unwrap_err()),
            "the file already exists"
        );
        assert!(!tmp.path().join("target").exists());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 3);
    }

    #[test]
    fn the_directory_has_to_be_there_and_the_path_has_to_name_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing/b.json");
        assert_eq!(
            why(bundle().save_new(&path).unwrap_err()),
            "its directory does not exist"
        );
        assert!(!tmp.path().join("missing").exists());
        // a file where the directory should be
        std::fs::write(tmp.path().join("f"), "").unwrap();
        let under_file = tmp.path().join("f/b.json");
        assert_eq!(
            why(bundle().save_new(&under_file).unwrap_err()),
            "its directory does not exist"
        );
        for no_name in ["/", ""] {
            assert_eq!(
                why(bundle().save_new(Path::new(no_name)).unwrap_err()),
                "not the path of a file"
            );
        }
    }

    #[test]
    fn a_bare_file_name_is_a_file_of_the_current_directory() {
        // only the check: the directory of a relative name is `.`
        assert_eq!(
            check_new_path(Path::new("l1c-no-such-bundle.json")).unwrap(),
            Path::new(".")
        );
    }
}
