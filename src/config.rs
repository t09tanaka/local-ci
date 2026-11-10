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
    #[serde(rename = "link-node-modules")]
    link_node_modules: Option<bool>,
    setup: Option<RawSetup>,
    source: Option<RawSource>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    secret_files: Option<Vec<RawSecret>>,
}

/// `"glob"` or `{ glob = "...", required = true }`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawSecret {
    Glob(String),
    Full {
        glob: String,
        #[serde(default)]
        required: bool,
    },
}

/// `[source]`: what a capture of the working directory takes and leaves.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SourceConfig {
    /// Ignored, untracked files to take along (globs, repository-root relative).
    pub include: Vec<String>,
    /// Files left out of a capture, tracked or not.
    pub exclude: Vec<String>,
    /// Ignored files the run needs; sent apart from the snapshot.
    pub secret_files: Vec<crate::source::SecretSpec>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSetup {
    #[serde(rename = "ignore-env")]
    ignore_env: Option<Vec<String>>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    /// Directories (relative to the repository root, normalized) that replace
    /// `.github/workflows`. `None` when the file or the key is absent.
    pub workflows_dirs: Option<Vec<String>>,
    /// Workflows (`ci.yml`) or jobs (`ci.yml:e2e`) whose results are never
    /// reused.
    pub no_cache: Vec<String>,
    /// Units' copies link to setup's `node_modules` instead of cloning it.
    pub link_node_modules: bool,
    /// `[setup] ignore-env`: `env:` names that do not decide what an install
    /// does (`CI`, `DATABASE_URL`). They are left out of the keys of setup
    /// steps, so jobs that differ only in them still share one install. Sorted.
    pub ignore_env: Vec<String>,
    /// `[source]`.
    pub source: SourceConfig,
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
    let mut ignore_env = raw.setup.and_then(|s| s.ignore_env).unwrap_or_default();
    if let Some(e) = ignore_env
        .iter()
        .find(|e| e.trim().is_empty() || e.contains('='))
    {
        bail!("{FILE}: [setup] ignore-env {e:?}: give the name of an env variable");
    }
    ignore_env.sort();
    ignore_env.dedup();
    let source = match raw.source {
        None => SourceConfig::default(),
        Some(s) => {
            let globs = s
                .include
                .iter()
                .flatten()
                .chain(s.exclude.iter().flatten())
                .chain(s.secret_files.iter().flatten().map(|r| match r {
                    RawSecret::Glob(g) => g,
                    RawSecret::Full { glob, .. } => glob,
                }));
            for g in globs {
                if g.trim().is_empty()
                    || Path::new(g).is_absolute()
                    || g.split('/').any(|c| c == "..")
                {
                    bail!("{FILE}: [source] {g:?}: give a glob relative to the repository root");
                }
            }
            SourceConfig {
                include: s.include.unwrap_or_default(),
                exclude: s.exclude.unwrap_or_default(),
                secret_files: s
                    .secret_files
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| match r {
                        RawSecret::Glob(glob) => crate::source::SecretSpec {
                            glob,
                            required: false,
                        },
                        RawSecret::Full { glob, required } => {
                            crate::source::SecretSpec { glob, required }
                        }
                    })
                    .collect(),
            }
        }
    };
    Ok(Config {
        workflows_dirs,
        no_cache,
        link_node_modules: raw.link_node_modules.unwrap_or(false),
        ignore_env,
        source,
    })
}

/// `.local-ci.toml` at `sha`, or the defaults when the commit has none.
pub fn at(ctx: &crate::context::ExecContext, dir: &Path, sha: &str) -> Result<Config> {
    // `<mode> <type> <object>\t<path>`, or nothing when the commit has no such path
    let listed = crate::git::git(ctx, dir, &["ls-tree", sha, "--", FILE])?;
    if listed.trim().is_empty() {
        return Ok(Config::default());
    }
    if !listed.starts_with("100644 blob ") && !listed.starts_with("100755 blob ") {
        bail!("{FILE}: not a regular file at {}", &sha[..sha.len().min(7)]);
    }
    parse(&crate::git::git(
        ctx,
        dir,
        &["show", &format!("{sha}:{FILE}")],
    )?)
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
        assert!(!c.link_node_modules);
        assert!(parse("link-node-modules = true").unwrap().link_node_modules);
        assert!(parse("no-cache = [\" \"]").is_err());
    }

    #[test]
    fn setup_ignore_env_is_sorted_and_checked() {
        assert!(parse("").unwrap().ignore_env.is_empty());
        let c = parse("[setup]\nignore-env = [\"DATABASE_URL\", \"CI\", \"CI\"]\n").unwrap();
        assert_eq!(c.ignore_env, vec!["CI", "DATABASE_URL"]);
        for text in [
            "[setup]\nignore-env = [\"\"]",
            "[setup]\nignore-env = [\"A=b\"]",
            "[setup]\nignore_env = [\"A\"]",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
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
