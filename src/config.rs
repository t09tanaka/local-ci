//! `.local-ci.toml` at the repository root, read from the commit under test
//! like the workflows are.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

pub const FILE: &str = ".local-ci.toml";

/// Where workflows are read from when `.local-ci.toml` does not say.
pub const DEFAULT_WORKFLOWS_DIR: &str = ".github/workflows";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(rename = "workflows-dirs")]
    workflows_dirs: Option<Vec<String>>,
    #[serde(rename = "no-cache")]
    no_cache: Option<Vec<String>>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    /// Directories (relative to the repository root, normalized) that replace
    /// `.github/workflows`. `None` when the file or the key is absent.
    pub workflows_dirs: Option<Vec<String>>,
    /// Workflows (`ci.yml`) or jobs (`ci.yml:e2e`) whose results are never
    /// reused.
    pub no_cache: Vec<String>,
}

impl Config {
    /// The directory whose prefix output leaves off workflow paths.
    pub fn default_workflows_dir(&self) -> &str {
        self.workflows_dirs
            .as_ref()
            .and_then(|d| d.first())
            .map(String::as_str)
            .unwrap_or(DEFAULT_WORKFLOWS_DIR)
    }
}

pub fn parse(text: &str) -> Result<Config> {
    let raw: Raw = toml::from_str(text).with_context(|| format!("{FILE}: cannot read it"))?;
    let workflows_dirs = match raw.workflows_dirs {
        None => None,
        Some(dirs) => {
            if dirs.is_empty() {
                bail!("{FILE}: workflows-dirs is empty; list at least one directory or leave the key out");
            }
            let mut out: Vec<String> = Vec::new();
            for d in dirs {
                if Path::new(&d).is_absolute() {
                    bail!(
                        "{FILE}: workflows-dirs {d}: give a path relative to the repository root"
                    );
                }
                let n = crate::plan::norm_dir(&d);
                if n.is_empty() || n.split('/').any(|c| c == "..") {
                    bail!(
                        "{FILE}: workflows-dirs {d:?}: give a directory below the repository root"
                    );
                }
                if !out.contains(&n) {
                    out.push(n);
                }
            }
            Some(out)
        }
    };
    let no_cache = raw.no_cache.unwrap_or_default();
    if let Some(e) = no_cache.iter().find(|e| e.trim().is_empty()) {
        bail!("{FILE}: no-cache {e:?}: give a workflow file (`ci.yml`) or a job (`ci.yml:e2e`)");
    }
    Ok(Config {
        workflows_dirs,
        no_cache,
    })
}

/// `.local-ci.toml` at `sha`, or the defaults when the commit has none.
pub fn at(dir: &Path, sha: &str) -> Result<Config> {
    // `<mode> <type> <object>\t<path>`, or nothing when the commit has no such path
    let listed = crate::git::git(dir, &["ls-tree", sha, "--", FILE])?;
    if listed.trim().is_empty() {
        return Ok(Config::default());
    }
    if !listed.starts_with("100644 blob ") && !listed.starts_with("100755 blob ") {
        bail!("{FILE}: not a regular file at {}", &sha[..sha.len().min(7)]);
    }
    parse(&crate::git::git(dir, &["show", &format!("{sha}:{FILE}")])?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_keeps_the_defaults() {
        let c = parse("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.default_workflows_dir(), ".github/workflows");
    }

    #[test]
    fn dirs_are_normalized_and_deduplicated() {
        let c = parse("workflows-dirs = [\"./.ci/workflows/\", \"ci/local\", \".ci/workflows\"]\n")
            .unwrap();
        assert_eq!(
            c.workflows_dirs,
            Some(vec![".ci/workflows".to_string(), "ci/local".to_string()])
        );
        assert_eq!(c.default_workflows_dir(), ".ci/workflows");
    }

    #[test]
    fn no_cache_lists_workflows_and_jobs() {
        let c = parse("no-cache = [\"audit.yml\", \"api-ci.yml:e2e\"]\n").unwrap();
        assert_eq!(c.no_cache, vec!["audit.yml", "api-ci.yml:e2e"]);
        assert!(parse("no-cache = [\" \"]").is_err());
    }

    #[test]
    fn rejects_what_it_cannot_use() {
        for (text, msg) in [
            (
                "workflows-dirs = [\"/abs/wf\"]",
                "relative to the repository root",
            ),
            ("workflows-dirs = []", "is empty"),
            ("workflows-dirs = [\".\"]", "below the repository root"),
            ("workflows-dirs = [\"../wf\"]", "below the repository root"),
            (
                "workflows-dirs = [\"a/../../wf\"]",
                "below the repository root",
            ),
            ("workflow-dirs = [\".ci/workflows\"]", "unknown field"),
            ("workflows-dirs = \".ci/workflows\"", "cannot read it"),
        ] {
            let e = format!("{:#}", parse(text).unwrap_err());
            assert!(e.starts_with(".local-ci.toml: "), "{text}: {e}");
            assert!(e.contains(msg), "{text}: {e}");
        }
    }
}
