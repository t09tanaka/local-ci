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
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// `compression-level` as written: accepted and recorded, with no effect
    /// (the files are stored as they are).
    #[serde(default)]
    pub compression_level: Option<u32>,
    /// The attempt this upload was first made in, when the receipt is a
    /// restore of a kept result (see [`restore_artifacts`]); `attempt_id` and
    /// `plan_digest` are then the restoring run's.
    #[serde(default)]
    pub restored_from: Option<String>,
}

/// What can be said before the run about the artifact steps of the jobs of
/// workflow `wf` that the plan will run (the jobs a filter left out are not
/// looked at; a job whose `if:` is decided when it runs is): every step is of
/// a supported use with supported options. One problem per step, each naming
/// the workflow, job and step.
pub fn workflow_problems(plan: &Plan, wf: usize) -> Vec<String> {
    let w = &plan.selected[wf].wf;
    let mut problems = Vec::new();
    for u in plan.units.iter().filter(|u| u.wf == wf) {
        for id in &u.jobs {
            let Some(job) = w.jobs.iter().find(|j| &j.id == id) else {
                continue;
            };
            let axes = job
                .matrix
                .as_ref()
                .map(|m| artifact::matrix_axes(&crate::matrix::yaml_to_json(m)));
            let matrix = match &axes {
                None => artifact::Matrix::None,
                Some(None) => artifact::Matrix::Unknown,
                Some(Some(a)) => artifact::Matrix::Axes(a),
            };
            for step in &job.steps {
                let Some(uses) = &step.uses else { continue };
                let Some(kind) = artifact::parse_uses(uses) else {
                    continue;
                };
                if let Err(e) = kind.and_then(|k| artifact::validate_with(k, &step.with, matrix)) {
                    problems.push(format!(
                        "{}: job `{}`, step `{}`: {e}",
                        w.file,
                        job.id,
                        step.label()
                    ));
                }
            }
        }
    }
    problems
}

/// Why the jobs of workflow `wf` are not run, when an artifact step of one of
/// them cannot be: the same text for each job of the workflow, so a job whose
/// own steps are fine says what stopped it.
pub fn workflow_refusal(plan: &Plan, wf: usize) -> Option<String> {
    let problems = workflow_problems(plan, wf);
    (!problems.is_empty()).then(|| {
        format!(
            "not run: artifact steps local-ci cannot run: {}",
            problems.join("; ")
        )
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest_of(bytes: &[u8]) -> (u64, String) {
    (bytes.len() as u64, hex(&Sha256::digest(bytes)))
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

/// What is at each component of `rel` under `ws`, from the first, up to the
/// first one that is missing.
fn found_along(ws: &Path, rel: &str) -> Vec<artifact::Found> {
    let mut at = ws.to_path_buf();
    let mut out = Vec::new();
    for c in rel.split('/').filter(|c| !c.is_empty()) {
        at.push(c);
        let f = match fs::symlink_metadata(&at) {
            Err(_) => artifact::Found::Missing,
            Ok(m) if m.is_dir() => artifact::Found::Dir,
            Ok(m) if m.is_file() => artifact::Found::File,
            Ok(_) => artifact::Found::Other,
        };
        out.push(f);
        if f == artifact::Found::Missing {
            break;
        }
    }
    out
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
fn latest_receipt(area: &Area, name: &str) -> Option<(u32, PathBuf, Receipt)> {
    version_dirs(&area.root.join(name))
        .into_iter()
        .rev()
        .find_map(|(n, d)| read_receipt(&d).map(|r| (n, d, r)))
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

/// A kept version of an artifact: its name, version and the digest of its
/// files ([`artifact::files_digest`]).
pub type Kept = (String, u32, String);

fn receipt_digest(r: &Receipt) -> String {
    artifact::files_digest(
        &r.files
            .iter()
            .map(|f| (f.path.clone(), f.size, f.sha256.clone()))
            .collect::<Vec<_>>(),
    )
}

/// The digest of the files of the newest version of `name` in a run, or
/// `None` when there is none.
pub fn latest_digest(run_dir: &Path, name: &str) -> Option<String> {
    latest_receipt(&Area::of_run(run_dir, ""), name).map(|(_, _, r)| receipt_digest(&r))
}

/// Copy one version (`receipt.json` and `files/`) from `from` to `to`,
/// checking every file against `receipt` as it is read: what is checked is
/// what is written. The receipt written is `write` (the same, or the restored
/// one), last, so a version without it is not a published one.
fn copy_version(from: &Path, to: &Path, receipt: &Receipt, write: &Receipt) -> Result<()> {
    for f in &receipt.files {
        let rel = artifact::stored_rel(&f.path).map_err(|e| anyhow!(e))?;
        let files = from.join("files");
        no_symlink_in(&files, &rel)?;
        let src = files.join(&rel);
        let meta = fs::symlink_metadata(&src).with_context(|| format!("read {}", src.display()))?;
        if !meta.is_file() {
            bail!("`{}` of `{}` is not a file", f.path, receipt.name);
        }
        let bytes = fs::read(&src).with_context(|| format!("read {}", src.display()))?;
        let (size, sha) = digest_of(&bytes);
        artifact::check_digest(&f.path, &receipt.name, (f.size, &f.sha256), (size, &sha))
            .map_err(|e| anyhow!(e))?;
        let dest = to.join("files").join(&rel);
        fs::create_dir_all(dest.parent().expect("a file has a parent"))?;
        write_atomic(&dest, &bytes, meta.permissions().mode() & 0o555)?;
    }
    fs::create_dir_all(to)?;
    write_atomic(
        &to.join("receipt.json"),
        &serde_json::to_vec_pretty(write)?,
        0o444,
    )
}

/// Keep, in `dest`, every version of an artifact that one of `jobs`
/// (`<file>::<id>`) uploaded in the run (`<name>/v<N>/` as in the run), each
/// file checked against its receipt. What is kept, by name, version and
/// digest. Fails, leaving `dest` to be removed, when a file does not match.
pub fn save_unit_artifacts(run_dir: &Path, jobs: &[String], dest: &Path) -> Result<Vec<Kept>> {
    let area = Area::of_run(run_dir, "");
    let mut kept = Vec::new();
    for name in published_names(run_dir) {
        for (n, vdir) in version_dirs(&area.root.join(&name)) {
            let Some(r) = read_receipt(&vdir) else {
                continue;
            };
            if !jobs.contains(&r.job) {
                continue;
            }
            if r.name != name || r.version != n {
                bail!(
                    "the receipt in `{name}/v{n}` is of `{}` v{}",
                    r.name,
                    r.version
                );
            }
            copy_version(&vdir, &dest.join(&name).join(format!("v{n}")), &r, &r)?;
            kept.push((name.clone(), n, receipt_digest(&r)));
        }
    }
    Ok(kept)
}

/// Put what [`save_unit_artifacts`] kept in `src` into the run's own area, as
/// if the unit had uploaded it here: every file is checked against its receipt
/// first, the receipt is rewritten with this run's plan digest and attempt (the
/// original attempt goes to `restored_from`), and a version is only put where
/// none is. Nothing is left behind when it fails. `expect` is what the
/// results cache recorded as kept: anything else in `src` is a failure too.
pub fn restore_artifacts(
    src: &Path,
    run_dir: &Path,
    plan_digest: &str,
    expect: &[Kept],
) -> Result<()> {
    let area = Area::of_run(run_dir, plan_digest);
    let mut found: Vec<(String, u32, PathBuf, Receipt)> = Vec::new();
    for e in fs::read_dir(src)
        .with_context(|| format!("read {}", src.display()))?
        .flatten()
    {
        if !e.file_type()?.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        artifact::runtime_name(&name).map_err(|e| anyhow!(e))?;
        for (n, vdir) in version_dirs(&e.path()) {
            let r = read_receipt(&vdir)
                .ok_or_else(|| anyhow!("`{name}/v{n}` of the kept result has no receipt"))?;
            if r.name != name || r.version != n {
                bail!(
                    "the receipt in `{name}/v{n}` is of `{}` v{}",
                    r.name,
                    r.version
                );
            }
            found.push((name.clone(), n, vdir, r));
        }
    }
    let mut got: Vec<Kept> = found
        .iter()
        .map(|(n, v, _, r)| (n.clone(), *v, receipt_digest(r)))
        .collect();
    got.sort();
    let mut want = expect.to_vec();
    want.sort();
    if got != want {
        bail!("the kept artifacts are not the ones the result recorded");
    }
    let stage = run_dir.join(format!(".restore-{}", std::process::id()));
    let _ = fs::remove_dir_all(&stage);
    let mut moved: Vec<PathBuf> = Vec::new();
    let done = (|| -> Result<()> {
        let existing: Vec<String> = fs::read_dir(&area.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        for (name, n, vdir, r) in &found {
            artifact::name_collision(&existing, name).map_err(|e| anyhow!(e))?;
            if area.root.join(name).join(format!("v{n}")).exists() {
                bail!("`{name}` v{n} is already in this run");
            }
            let again = Receipt {
                plan_digest: area.plan_digest.clone(),
                attempt_id: area.attempt.clone(),
                restored_from: Some(r.attempt_id.clone()),
                ..r.clone()
            };
            copy_version(vdir, &stage.join(name).join(format!("v{n}")), r, &again)?;
        }
        for (name, n, _, _) in &found {
            fs::create_dir_all(area.root.join(name))?;
            let to = area.root.join(name).join(format!("v{n}"));
            fs::rename(stage.join(name).join(format!("v{n}")), &to)?;
            moved.push(to);
        }
        Ok(())
    })();
    if done.is_err() {
        for m in &moved {
            let _ = fs::remove_dir_all(m);
        }
    }
    let _ = fs::remove_dir_all(&stage);
    done
}

// File work relative to a directory that is held open.
//
// Every directory of a path is opened from the one before it with
// `O_NOFOLLOW`, and the file last, so a symlink put in the place of any
// component after a check cannot take a read or a write out of the
// workspace: there is no path to be resolved a second time. (`mkdirat`,
// `openat` and `renameat` act on the held directory, and `renameat` replaces
// a symlink at the final name rather than following it.) The one thing not
// held is the workspace root itself, which is opened once by the path the run
// gave and is the run's own.

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn open_at(dir: &File, name: &str, flags: libc::c_int, mode: u32) -> io::Result<File> {
    let c = cstr(name)?;
    // SAFETY: `c` is a NUL-terminated string that outlives the call and the
    // descriptor of `dir` is open; the descriptor returned is owned by the File.
    let fd = cvt(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    })?;
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// The file type bits of `name` in `dir`, without following a symlink.
fn type_at(dir: &File, name: &str) -> Option<libc::mode_t> {
    let c = cstr(name).ok()?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: as for `open_at`; `st` is written by the call when it succeeds.
    let r = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    (r == 0).then(|| unsafe { st.assume_init() }.st_mode & libc::S_IFMT)
}

fn unlink_at(dir: &File, name: &str) -> io::Result<()> {
    let c = cstr(name)?;
    // SAFETY: as for `open_at`.
    cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) }).map(|_| ())
}

/// Why `name` in `dir` could not be opened as what `shown` needs.
fn refuse(dir: &File, name: &str, shown: &str, e: io::Error) -> anyhow::Error {
    match type_at(dir, name) {
        Some(libc::S_IFLNK) => {
            anyhow!("`{shown}` goes through the symlink `{name}`, which is refused")
        }
        Some(t) if t != libc::S_IFDIR && e.raw_os_error() == Some(libc::ENOTDIR) => {
            anyhow!("`{shown}` goes through `{name}`, which is not a directory")
        }
        _ => anyhow!(e).context(format!("open `{shown}`")),
    }
}

/// The directory `comps` under `root`, opened one component at a time without
/// following a symlink; with `create`, the ones that are not there are made.
fn open_dirs(root: &File, comps: &[&str], create: bool, shown: &str) -> Result<File> {
    let mut cur = root.try_clone()?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    for c in comps {
        let next = match open_at(&cur, c, flags, 0) {
            Ok(f) => f,
            Err(e) if create && e.raw_os_error() == Some(libc::ENOENT) => {
                let name = cstr(c)?;
                // SAFETY: as for `open_at`.
                let made = unsafe { libc::mkdirat(cur.as_raw_fd(), name.as_ptr(), 0o755) };
                if made < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                    return Err(
                        anyhow!(io::Error::last_os_error()).context(format!("create `{c}`"))
                    );
                }
                open_at(&cur, c, flags, 0).map_err(|e| refuse(&cur, c, shown, e))?
            }
            Err(e) => return Err(refuse(&cur, c, shown, e)),
        };
        cur = next;
    }
    Ok(cur)
}

fn components(rel: &str) -> Vec<&str> {
    rel.split('/').filter(|c| !c.is_empty()).collect()
}

/// A regular file of the workspace, read through descriptors: its bytes and
/// mode. A symlink at any component, a directory, a device or a pipe is
/// refused, whatever it was when the files were listed.
fn read_nofollow(root: &File, rel: &str) -> Result<(Vec<u8>, u32)> {
    let comps = components(rel);
    let (name, dirs) = comps.split_last().context("no file named")?;
    let dir = open_dirs(root, dirs, false, rel)?;
    let mut f = open_at(
        &dir,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )
    .map_err(|e| refuse(&dir, name, rel, e))?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        bail!("`{rel}` is not a regular file");
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    f.read_to_end(&mut bytes)
        .with_context(|| format!("read `{rel}`"))?;
    Ok((bytes, meta.permissions().mode()))
}

/// Write `bytes` as `name` of `dir`: into a temp file made new, under a name
/// that is this placement's (`O_EXCL`, `O_NOFOLLOW`: never through a link
/// someone left there, and what is there is never removed), then renamed over
/// the name. A name that is a directory or a symlink is not replaced.
fn write_at(dir: &File, name: &str, bytes: &[u8], mode: u32, shown: &str) -> Result<()> {
    if matches!(type_at(dir, name), Some(libc::S_IFLNK | libc::S_IFDIR)) {
        bail!("`{shown}` exists and is not a file; it is not replaced");
    }
    // a name of its own for this placement: nothing that is there is touched
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
    let (tmp, mut f) = {
        let mut tries = 0;
        loop {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let tmp = format!(
                "{name}.lci-tmp-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            );
            match open_at(dir, &tmp, flags, 0o600) {
                Ok(f) => break (tmp, f),
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) && tries < 16 => tries += 1,
                Err(e) => return Err(anyhow!(e).context(format!("create `{tmp}`"))),
            }
        }
    };
    let done = (|| -> Result<()> {
        f.write_all(bytes)?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        let (from, to) = (cstr(&tmp)?, cstr(name)?);
        // SAFETY: as for `open_at`.
        cvt(unsafe {
            libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr())
        })?;
        Ok(())
    })();
    if done.is_err() {
        let _ = unlink_at(dir, &tmp);
    }
    done
}

/// Put `bytes` at `dest` under the workspace root, making the directories on
/// the way; none of them is followed if it is a symlink.
fn place_nofollow(root: &File, dest: &str, bytes: &[u8], mode: u32) -> Result<()> {
    let comps = components(dest);
    let (name, dirs) = comps.split_last().context("no file named")?;
    let dir = open_dirs(root, dirs, true, dest)?;
    write_at(&dir, name, bytes, mode, dest)
}

/// Write a file of the run's own artifact area.
fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = File::open(path.parent().expect("a file has a parent"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("a file name that is text")?;
    write_at(&parent, name, bytes, mode, name)
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
    artifact::validate_run(Kind::Upload, with).map_err(|e| anyhow!(e))?;
    let get = |k: &str| {
        with.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    };
    // checked here whatever the plan could check: the name is a directory
    let name = artifact::runtime_name(get("name").unwrap_or("artifact")).map_err(|e| anyhow!(e))?;
    let name = name.as_str();
    let policy = artifact::missing_policy(get("if-no-files-found")).map_err(|e| anyhow!(e))?;
    let overwrite = artifact::parse_bool("overwrite", get("overwrite")).map_err(|e| anyhow!(e))?;
    let hidden = artifact::parse_bool("include-hidden-files", get("include-hidden-files"))
        .map_err(|e| anyhow!(e))?;
    let retention = artifact::parse_retention(get("retention-days")).map_err(|e| anyhow!(e))?;
    let compression =
        artifact::parse_compression(get("compression-level")).map_err(|e| anyhow!(e))?;
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

    let existing: Vec<String> = fs::read_dir(&area.root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    artifact::name_collision(&existing, name).map_err(|e| anyhow!(e))?;
    let dir = area.root.join(name);
    fs::create_dir_all(&dir)?;
    // creating the directory is what claims the version: a second upload of
    // the name at the same moment finds it taken, and goes on to the next
    // version (overwrite) or reports the conflict
    let latest = version_dirs(&dir).last().map(|(n, _)| *n);
    let mut version = artifact::decide_version(name, latest, overwrite).map_err(|e| anyhow!(e))?;
    let vdir = loop {
        let vdir = dir.join(format!("v{version}"));
        match fs::create_dir(&vdir) {
            Ok(()) => break vdir,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                version = artifact::version_after_race(name, version, overwrite)
                    .map_err(|e| anyhow!(e))?;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let root = File::open(ws).with_context(|| format!("open {}", ws.display()))?;
    let mut files = Vec::new();
    for (src, stored) in artifact::stored_paths(&lines, &selected) {
        // one read: what is hashed is what is stored
        let (bytes, mode) = read_nofollow(&root, &src)?;
        let to = vdir.join("files").join(&stored);
        fs::create_dir_all(to.parent().expect("a file has a parent"))?;
        write_atomic(&to, &bytes, mode & 0o555)?;
        let (size, sha256) = digest_of(&bytes);
        files.push(FileEntry {
            path: stored,
            size,
            sha256,
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
        compression_level: compression,
        restored_from: None,
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
    artifact::validate_run(Kind::Download, with).map_err(|e| anyhow!(e))?;
    let get = |k: &str| {
        with.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    };
    let merge =
        artifact::parse_bool("merge-multiple", get("merge-multiple")).map_err(|e| anyhow!(e))?;
    let base = artifact::workspace_rel(get("path").unwrap_or(""), &ws.to_string_lossy())
        .map_err(|e| anyhow!(e))?;
    let name = get("name");
    if let Some(n) = name {
        artifact::runtime_name(n).map_err(|e| anyhow!(e))?;
    }
    let available = published_names(
        area.root
            .parent()
            .expect("the artifact area is inside a run directory"),
    );
    let picked = artifact::select_artifacts(&available, name, get("pattern"))
        .map_err(|e| anyhow!("{e} (the job must `needs` the job that uploads it)"))?;
    let direct = name.is_some() || merge;

    // every rejection comes before the first file is placed
    struct Item {
        artifact: String,
        src: PathBuf,
        dest: String,
        entry: FileEntry,
        mode: u32,
    }
    let mut items: Vec<Item> = Vec::new();
    for n in &picked {
        let (version, vdir, receipt) =
            latest_receipt(area, n).ok_or_else(|| anyhow!("artifact `{n}` has no receipt"))?;
        artifact::check_receipt(
            (n, version, &area.plan_digest),
            (&receipt.name, receipt.version, &receipt.plan_digest),
        )
        .map_err(|e| anyhow!(e))?;
        let dir = artifact::place_dir(&base, n, direct);
        for f in receipt.files {
            let dest = artifact::target_rel(&dir, &f.path).map_err(|e| anyhow!(e))?;
            let src = vdir
                .join("files")
                .join(artifact::stored_rel(&f.path).map_err(|e| anyhow!(e))?);
            let mode = fs::metadata(&src)
                .with_context(|| format!("read {}", src.display()))?
                .permissions()
                .mode();
            items.push(Item {
                artifact: n.clone(),
                src,
                dest,
                entry: f,
                mode,
            });
        }
    }
    let dests: Vec<String> = items.iter().map(|i| i.dest.clone()).collect();
    artifact::find_collision(&dests).map_err(|e| anyhow!(e))?;
    let load = |i: &Item| -> Result<Vec<u8>> {
        let bytes = fs::read(&i.src).with_context(|| format!("read {}", i.src.display()))?;
        let (size, sha) = digest_of(&bytes);
        artifact::check_digest(
            &i.entry.path,
            &i.artifact,
            (i.entry.size, &i.entry.sha256),
            (size, &sha),
        )
        .map_err(|e| anyhow!(e))?;
        Ok(bytes)
    };
    for i in &items {
        load(i)?;
        no_symlink_in(ws, &i.dest)?;
        artifact::check_path_kinds(&i.dest, &found_along(ws, &i.dest)).map_err(|e| anyhow!(e))?;
    }
    // placing: each file is read once more, checked against the receipt
    // again, and that same buffer is what is written
    let root = File::open(ws).with_context(|| format!("open {}", ws.display()))?;
    for i in &items {
        let bytes = load(i)?;
        place_nofollow(&root, &i.dest, &bytes, (i.mode & 0o755) | 0o200)?;
    }
    Ok(format!(
        "downloaded {} artifact{} ({} file{}) into `{}`",
        picked.len(),
        if picked.len() == 1 { "" } else { "s" },
        items.len(),
        if items.len() == 1 { "" } else { "s" },
        if base.is_empty() { "." } else { &base }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn root(p: &Path) -> File {
        File::open(p).unwrap()
    }

    #[test]
    fn a_read_refuses_a_symlink_at_the_end_in_the_middle_or_a_non_file() {
        let t = tempfile::tempdir().unwrap();
        let ws = t.path().join("ws");
        let out = t.path().join("out");
        fs::create_dir_all(ws.join("d")).unwrap();
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("secret"), "s").unwrap();
        fs::write(ws.join("d/f"), "data").unwrap();
        symlink(out.join("secret"), ws.join("d/leaf")).unwrap();
        symlink(&out, ws.join("mid")).unwrap();
        let r = root(&ws);
        let (bytes, mode) = read_nofollow(&r, "d/f").unwrap();
        assert_eq!(bytes, b"data");
        assert_eq!(mode & 0o200, 0o200);
        let e = read_nofollow(&r, "d/leaf").unwrap_err().to_string();
        assert!(e.contains("symlink `leaf`"), "{e}");
        let e = read_nofollow(&r, "mid/secret").unwrap_err().to_string();
        assert!(e.contains("symlink `mid`"), "{e}");
        let e = read_nofollow(&r, "d").unwrap_err().to_string();
        assert!(e.contains("not a regular file"), "{e}");
        let e = read_nofollow(&r, "d/f/x").unwrap_err().to_string();
        assert!(e.contains("not a directory"), "{e}");
        assert!(read_nofollow(&r, "d/none").is_err());
        assert!(read_nofollow(&r, "").is_err());
        // a pipe is not read (and does not block)
        let fifo = cstr(&ws.join("p").to_string_lossy()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        let e = read_nofollow(&r, "p").unwrap_err().to_string();
        assert!(e.contains("not a regular file"), "{e}");
    }

    #[test]
    fn a_write_does_not_follow_a_link_left_as_its_temp_file_or_as_a_directory() {
        let t = tempfile::tempdir().unwrap();
        let ws = t.path().join("ws");
        let out = t.path().join("out");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("victim"), "keep").unwrap();
        // what sits under the old temp name is not ours: left as it is, not written through
        symlink(out.join("victim"), ws.join("f.lci-tmp")).unwrap();
        let r = root(&ws);
        place_nofollow(&r, "f", b"new", 0o644).unwrap();
        assert_eq!(fs::read(ws.join("f")).unwrap(), b"new");
        assert_eq!(fs::read(out.join("victim")).unwrap(), b"keep");
        assert!(ws.join("f.lci-tmp").symlink_metadata().is_ok());
        fs::write(ws.join("g.lci-tmp"), "someone's").unwrap();
        fs::create_dir(ws.join("h.lci-tmp")).unwrap();
        place_nofollow(&r, "g", b"x", 0o644).unwrap();
        place_nofollow(&r, "h", b"x", 0o644).unwrap();
        assert_eq!(fs::read(ws.join("g.lci-tmp")).unwrap(), b"someone's");
        assert!(ws.join("h.lci-tmp").is_dir());
        // every placement has a temp name of its own, and none stays
        for _ in 0..3 {
            place_nofollow(&r, "g", b"y", 0o644).unwrap();
        }
        let names: Vec<String> = fs::read_dir(&ws)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 6, "{names:?}");
        // the name itself being a symlink or a directory is not replaced
        symlink(out.join("victim"), ws.join("l")).unwrap();
        let e = place_nofollow(&r, "l", b"x", 0o644)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not replaced"), "{e}");
        fs::create_dir(ws.join("dd")).unwrap();
        assert!(place_nofollow(&r, "dd", b"x", 0o644).is_err());
        assert_eq!(fs::read(out.join("victim")).unwrap(), b"keep");
    }

    #[test]
    fn directories_are_made_one_at_a_time_and_a_link_among_them_stops_it() {
        let t = tempfile::tempdir().unwrap();
        let ws = t.path().join("ws");
        let out = t.path().join("out");
        fs::create_dir_all(ws.join("a")).unwrap();
        fs::create_dir_all(&out).unwrap();
        symlink(&out, ws.join("a/link")).unwrap();
        let r = root(&ws);
        place_nofollow(&r, "a/b/c/f", b"ok", 0o644).unwrap();
        assert_eq!(fs::read(ws.join("a/b/c/f")).unwrap(), b"ok");
        let e = place_nofollow(&r, "a/link/sub/f", b"x", 0o644)
            .unwrap_err()
            .to_string();
        assert!(e.contains("symlink `link`"), "{e}");
        assert!(fs::read_dir(&out).unwrap().next().is_none());
        // a file where a directory is needed
        let e = place_nofollow(&r, "a/b/c/f/g", b"x", 0o644)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a directory"), "{e}");
    }
}
