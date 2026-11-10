//! The artifacts of a run on disk: what `actions/upload-artifact@v4` and
//! `actions/download-artifact@v4` do within one run.
//!
//! An upload copies its files into the run's artifact area
//! (`<run dir>/artifacts/<name>/v<N>/files/`), read-only, and writes the
//! receipt (`receipt.json`: name, version, each file's path, size and SHA-256,
//! the plan digest, the attempt, the job and task) last and atomically; the
//! step completes after that. A download waits for nothing but the receipt:
//! it takes the newest version that has one, checks every file against it and
//! only then places the files, refusing any write outside the workspace.
//! What to decide is [`crate::artifact`]'s; this module does the file work.

use crate::artifact::{self, Kind, NoFiles};
use crate::plan::Plan;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The run's artifact area and what a receipt records about the run.
pub struct Area {
    pub root: PathBuf,
    pub plan_digest: String,
    pub attempt: String,
}

impl Area {
    /// The area of a run directory.
    pub fn of_run(run_dir: &Path, plan_digest: &str) -> Area {
        Area {
            root: run_dir.join("artifacts"),
            plan_digest: plan_digest.to_string(),
            attempt: run_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

/// Which job an upload is of.
pub struct Who {
    /// `<workflow file>::<job id>`.
    pub job: String,
    /// The job's name as the run shows it (a matrix entry has its variant).
    pub display: String,
    /// The task, when the plan has exactly one that holds the job.
    pub task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    pub name: String,
    pub version: u32,
    pub files: Vec<FileEntry>,
    pub plan_digest: String,
    pub attempt_id: String,
    pub job: String,
    pub display: String,
    pub task: Option<String>,
    pub retention_days: Option<u32>,
}

/// What can be said about the artifact steps of a plan before it runs: every
/// one is of a supported use with supported options. All problems, one per
/// step.
pub fn validate_plan(plan: &Plan) -> Result<(), String> {
    let mut problems = Vec::new();
    for sel in &plan.selected {
        for job in &sel.wf.jobs {
            for step in &job.steps {
                let Some(uses) = &step.uses else { continue };
                let Some(kind) = artifact::parse_uses(uses) else {
                    continue;
                };
                let why =
                    kind.and_then(|k| artifact::validate_with(k, &step.with, job.matrix.is_some()));
                if let Err(e) = why {
                    problems.push(format!(
                        "{}: job `{}`, step `{}`: {e}",
                        sel.wf.file,
                        job.id,
                        step.label()
                    ));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "artifact steps local-ci cannot run:\n  {}",
            problems.join("\n  ")
        ))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha_of(path: &Path) -> Result<(u64, String)> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok((bytes.len() as u64, hex(&Sha256::digest(&bytes))))
}

/// Fail when a component of `rel` under `root` that exists is a symlink.
fn no_symlink_in(root: &Path, rel: &str) -> Result<()> {
    let mut at = root.to_path_buf();
    for c in rel.split('/').filter(|c| !c.is_empty()) {
        at.push(c);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!("`{rel}` goes through the symlink `{c}`, which is refused")
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// Every file under `ws/rel`, as paths relative to `ws`. Symlinks are not
/// followed or taken; `.git` is not entered.
fn walk(ws: &Path, rel: &str, out: &mut BTreeSet<String>) -> Result<()> {
    no_symlink_in(ws, rel)?;
    let path = ws.join(rel);
    let Ok(meta) = fs::symlink_metadata(&path) else {
        return Ok(());
    };
    if meta.is_file() {
        out.insert(rel.to_string());
    } else if meta.is_dir() {
        for e in fs::read_dir(&path)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" || e.file_type()?.is_symlink() {
                continue;
            }
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            walk(ws, &child, out)?;
        }
    }
    Ok(())
}

fn version_dirs(dir: &Path) -> Vec<(u32, PathBuf)> {
    let mut v: Vec<(u32, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e
                .file_name()
                .to_string_lossy()
                .strip_prefix('v')?
                .parse()
                .ok()?;
            Some((n, e.path()))
        })
        .collect();
    v.sort();
    v
}

fn read_receipt(vdir: &Path) -> Option<Receipt> {
    serde_json::from_slice(&fs::read(vdir.join("receipt.json")).ok()?).ok()
}

/// The newest version of `name` that has a receipt.
fn latest_receipt(area: &Area, name: &str) -> Option<(PathBuf, Receipt)> {
    version_dirs(&area.root.join(name))
        .into_iter()
        .rev()
        .find_map(|(_, d)| read_receipt(&d).map(|r| (d, r)))
}

/// The names of the artifacts that have a receipt.
pub fn published_names(run_dir: &Path) -> Vec<String> {
    let root = run_dir.join("artifacts");
    let mut out: Vec<String> = fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            version_dirs(&root.join(n))
                .iter()
                .any(|(_, d)| d.join("receipt.json").is_file())
        })
        .collect();
    out.sort();
    out
}

/// The names of the artifacts uploaded by one of `jobs` (`<file>::<id>`).
pub fn published_by(run_dir: &Path, jobs: &[String]) -> Vec<String> {
    let area = Area::of_run(run_dir, "");
    published_names(run_dir)
        .into_iter()
        .filter(|n| {
            version_dirs(&area.root.join(n))
                .iter()
                .filter_map(|(_, d)| read_receipt(d))
                .any(|r| jobs.contains(&r.job))
        })
        .collect()
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".lci-tmp");
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// `actions/upload-artifact@v4`: `with` holds the step's values, already
/// expanded. The note says what was done.
pub fn upload(
    area: &Area,
    who: &Who,
    ws: &Path,
    with: &[(String, String)],
) -> Result<String, String> {
    upload_inner(area, who, ws, with).map_err(|e| format!("{e:#}"))
}

fn upload_inner(area: &Area, who: &Who, ws: &Path, with: &[(String, String)]) -> Result<String> {
    artifact::validate_with(Kind::Upload, with, false).map_err(|e| anyhow!(e))?;
    let get = |k: &str| {
        with.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    };
    let name = get("name").unwrap_or("artifact");
    let policy = artifact::missing_policy(get("if-no-files-found")).map_err(|e| anyhow!(e))?;
    let overwrite = artifact::parse_bool("overwrite", get("overwrite")).map_err(|e| anyhow!(e))?;
    let hidden = artifact::parse_bool("include-hidden-files", get("include-hidden-files"))
        .map_err(|e| anyhow!(e))?;
    let retention = artifact::parse_retention(get("retention-days")).map_err(|e| anyhow!(e))?;
    let lines = artifact::path_lines(get("path").unwrap_or(""), &ws.to_string_lossy())
        .map_err(|e| anyhow!(e))?;

    let mut all = BTreeSet::new();
    for l in lines.iter().filter(|l| !l.exclude) {
        walk(ws, &artifact::literal_prefix(&l.pattern), &mut all)?;
    }
    let all: Vec<String> = all.into_iter().collect();
    let selected = artifact::select_files(&lines, &all, hidden);
    match artifact::when_no_files(policy, selected.len()) {
        NoFiles::Upload => {}
        NoFiles::Warn => {
            return Ok(format!(
                "warning: no files were found with the path, so nothing was uploaded as `{name}`"
            ))
        }
        NoFiles::Nothing => return Ok(format!("no files found; nothing uploaded as `{name}`")),
        NoFiles::Fail => bail!("no files were found with the path `{}` for `{name}`", {
            lines
                .iter()
                .map(|l| l.pattern.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }),
    }

    let dir = area.root.join(name);
    let latest = version_dirs(&dir).last().map(|(n, _)| *n);
    let version = artifact::decide_version(name, latest, overwrite).map_err(|e| anyhow!(e))?;
    fs::create_dir_all(&dir)?;
    let vdir = dir.join(format!("v{version}"));
    // creating the directory is what claims the version: a second upload of
    // the name at the same moment finds it taken
    if let Err(e) = fs::create_dir(&vdir) {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(anyhow!(artifact::decide_version(
                name,
                Some(version),
                false
            )
            .unwrap_err()));
        }
        return Err(e.into());
    }
    let mut files = Vec::new();
    for (src, stored) in artifact::stored_paths(&lines, &selected) {
        let from = ws.join(&src);
        let bytes = fs::read(&from).with_context(|| format!("read {}", from.display()))?;
        let mode = fs::metadata(&from)?.permissions().mode() & 0o555;
        let to = vdir.join("files").join(&stored);
        fs::create_dir_all(to.parent().expect("a file has a parent"))?;
        write_atomic(&to, &bytes, mode)?;
        files.push(FileEntry {
            path: stored,
            size: bytes.len() as u64,
            sha256: hex(&Sha256::digest(&bytes)),
        });
    }
    let total: u64 = files.iter().map(|f| f.size).sum();
    let count = files.len();
    let receipt = Receipt {
        name: name.to_string(),
        version,
        files,
        plan_digest: area.plan_digest.clone(),
        attempt_id: area.attempt.clone(),
        job: who.job.clone(),
        display: who.display.clone(),
        task: who.task.clone(),
        retention_days: retention,
    };
    // the receipt is written last: a version without one is not published
    write_atomic(
        &vdir.join("receipt.json"),
        &serde_json::to_vec_pretty(&receipt)?,
        0o444,
    )?;
    Ok(format!(
        "uploaded `{name}` v{version}: {count} file{}, {total} bytes",
        if count == 1 { "" } else { "s" }
    ))
}

/// `actions/download-artifact@v4`. The note says what was done.
pub fn download(area: &Area, ws: &Path, with: &[(String, String)]) -> Result<String, String> {
    download_inner(area, ws, with).map_err(|e| format!("{e:#}"))
}

fn download_inner(area: &Area, ws: &Path, with: &[(String, String)]) -> Result<String> {
    artifact::validate_with(Kind::Download, with, false).map_err(|e| anyhow!(e))?;
    let get = |k: &str| {
        with.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    };
    let merge =
        artifact::parse_bool("merge-multiple", get("merge-multiple")).map_err(|e| anyhow!(e))?;
    let base = artifact::normalize_rel(get("path").unwrap_or("")).map_err(|e| anyhow!(e))?;
    let name = get("name");
    let available = published_names(
        area.root
            .parent()
            .expect("the artifact area is inside a run directory"),
    );
    let picked = artifact::select_artifacts(&available, name, get("pattern"))
        .map_err(|e| anyhow!("{e} (the job must `needs` the job that uploads it)"))?;
    let direct = name.is_some() || merge;

    // verify everything first, then place
    let mut plan: Vec<(PathBuf, String, u32)> = Vec::new();
    let mut seen = BTreeSet::new();
    for n in &picked {
        let (vdir, receipt) =
            latest_receipt(area, n).ok_or_else(|| anyhow!("artifact `{n}` has no receipt"))?;
        let dir = artifact::place_dir(&base, n, direct);
        for f in &receipt.files {
            let dest = artifact::target_rel(&dir, &f.path).map_err(|e| anyhow!(e))?;
            let src = vdir
                .join("files")
                .join(artifact::stored_rel(&f.path).map_err(|e| anyhow!(e))?);
            let (size, sha) = sha_of(&src)?;
            if size != f.size || sha != f.sha256 {
                bail!(
                    "`{}` of artifact `{n}` does not match its receipt (size {size} against {}, sha256 {sha} against {})",
                    f.path, f.size, f.sha256
                );
            }
            if !seen.insert(dest.clone()) {
                bail!("two artifacts would both write `{dest}`");
            }
            let mode = fs::metadata(&src)?.permissions().mode();
            plan.push((src, dest, mode));
        }
    }
    for (src, dest, mode) in &plan {
        no_symlink_in(ws, dest)?;
        let to = ws.join(dest);
        let parent = to.parent().expect("a file has a parent");
        fs::create_dir_all(parent)?;
        // the directories made now are checked too
        no_symlink_in(ws, dest)?;
        if let Ok(m) = fs::symlink_metadata(&to) {
            if m.file_type().is_symlink() || m.is_dir() {
                bail!("`{dest}` exists and is not a file; it is not replaced");
            }
        }
        write_atomic(&to, &fs::read(src)?, (mode & 0o755) | 0o200)?;
    }
    Ok(format!(
        "downloaded {} artifact{} ({} file{}) into `{}`",
        picked.len(),
        if picked.len() == 1 { "" } else { "s" },
        plan.len(),
        if plan.len() == 1 { "" } else { "s" },
        if base.is_empty() { "." } else { &base }
    ))
}
