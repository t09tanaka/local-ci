//! Digests of the documents that cross between hosts.

use crate::wire::check_version;
use serde::Serialize;

/// A type that crosses the wire and has a digest: the SHA-256 of its canonical
/// JSON (keys sorted at every level), `sha256:<hex>`. The digest is not a
/// field of the type: the sender puts it next to the document, and the reader
/// checks it with [`Digested::verify_digest`], so the document cannot hold a
/// digest that is out of date.
///
/// The digest is defined only for what this implementation's serializer
/// writes: a duration is a floating-point number (`secs`) printed the way
/// `serde_json` prints it, so another implementation (or another version of
/// the serializer) that prints the same number differently gives another
/// digest. A reader on another build should treat a mismatch as "cannot be
/// checked", not as proof of tampering.
///
/// A receiver reads a document with [`Digested::from_json_checked`], not with
/// `serde_json::from_str` and the digest left for later.
pub trait Digested: Serialize {
    /// The `schema_version` the document holds.
    fn schema_version(&self) -> u32;

    fn digest(&self) -> anyhow::Result<String> {
        value_digest(serde_json::to_value(self))
    }

    /// Fail unless the digest of `self` is `expected`.
    fn verify_digest(&self, expected: &str) -> anyhow::Result<()> {
        same_digest(self.digest(), expected)
    }

    /// Read a document from `text` the way a receiver must: it parses, it is
    /// of the schema version this build reads ([`check_version`]), and its
    /// digest is `digest` ([`Digested::verify_digest`]), in that order. At
    /// most [`crate::result_wire::MAX_WIRE_BYTES`] of text are read.
    ///
    /// Types that hold an `#[serde(flatten)]` field (the results of
    /// [`crate::result_wire`]) cannot refuse a key they do not know, so a
    /// document with a key of a newer writer is read without it; its digest,
    /// made over what this build re-serializes, then does not match, which is
    /// how it is noticed.
    fn from_json_checked(text: &str, digest: &str) -> anyhow::Result<Self>
    where
        Self: Sized + serde::de::DeserializeOwned,
    {
        let doc: Self = parse_document(text)?;
        check_version("document", doc.schema_version())?;
        doc.verify_digest(digest)?;
        Ok(doc)
    }
}

fn value_digest(v: serde_json::Result<serde_json::Value>) -> anyhow::Result<String> {
    Ok(v.map(|v| crate::frozen::sha256_of(&crate::frozen::canonical_json(&v)))?)
}

fn same_digest(got: anyhow::Result<String>, expected: &str) -> anyhow::Result<()> {
    let got = got?;
    if got != expected {
        anyhow::bail!("digest {expected} does not match the content ({got})");
    }
    Ok(())
}

fn parse_document<T: serde::de::DeserializeOwned>(text: &str) -> anyhow::Result<T> {
    use anyhow::Context;
    if text.len() > crate::result_wire::MAX_WIRE_BYTES {
        anyhow::bail!(
            "the document is {} bytes; at most {} are read",
            text.len(),
            crate::result_wire::MAX_WIRE_BYTES
        );
    }
    serde_json::from_str(text).context("cannot read the document")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::WIRE_SCHEMA_VERSION;

    #[derive(Serialize)]
    struct Doc {
        b: u32,
        a: Vec<u32>,
    }
    impl Digested for Doc {
        fn schema_version(&self) -> u32 {
            WIRE_SCHEMA_VERSION
        }
    }

    #[test]
    fn a_digest_that_cannot_be_made_is_an_error_not_a_mismatch() {
        let bad = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert!(value_digest(Err(bad)).is_err());
        let e = same_digest(Err(anyhow::anyhow!("no digest")), "x").unwrap_err();
        assert_eq!(e.to_string(), "no digest");
    }

    #[test]
    fn a_document_says_its_version() {
        assert_eq!(
            Doc { b: 1, a: vec![] }.schema_version(),
            WIRE_SCHEMA_VERSION
        );
    }

    #[test]
    fn a_digest_is_of_the_canonical_form_and_checked() {
        let d = Doc { b: 1, a: vec![2] };
        let want = crate::frozen::sha256_of("{\"a\":[2],\"b\":1}");
        assert_eq!(d.digest().unwrap(), want);
        d.verify_digest(&want).unwrap();
        let other = Doc { b: 2, a: vec![2] };
        let e = other.verify_digest(&want).unwrap_err().to_string();
        assert!(e.contains("does not match the content"), "{e}");
    }
}
