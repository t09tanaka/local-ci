//! `secret_files`: ignored files a run needs (a test `.env`), kept out of the
//! store and the manifest.

use super::store::{hex, unhex, write_atomic, RelPath};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SecretFile {
    pub path: RelPath,
    pub mode: u32,
    pub content_hex: String,
}

/// The content of the secret files of one source. Lives in a file of its own
/// (0600), never in the object store or the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretBundle {
    pub schema_version: u32,
    pub(crate) files: Vec<SecretFile>,
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

    pub fn load(path: &Path) -> Result<Self> {
        serde_json::from_slice(&std::fs::read(path)?).context("not a secret bundle")
    }
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
