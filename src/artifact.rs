//! The decisions behind `actions/upload-artifact@v4` and
//! `actions/download-artifact@v4` within one run: which options are
//! supported, which paths are refused, which files a `path:` selects and what
//! they are called inside the artifact, what a missing file or a second upload
//! of a name does, and which artifacts a download takes.
//!
//! Nothing here touches the file system; [`crate::artifact_io`] does that and
//! asks this module what to do.

/// Which of the two actions a step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Upload,
    Download,
}

/// `Some` for a step that uses one of the two actions. The error is for a use
/// local-ci cannot run: another major version, or a sub-action such as
/// `upload-artifact/merge`.
pub fn parse_uses(uses: &str) -> Option<Result<Kind, String>> {
    let (path, git_ref) = uses.split_once('@').unwrap_or((uses, ""));
    let path = path.to_ascii_lowercase();
    let kind = match path.as_str() {
        "actions/upload-artifact" => Kind::Upload,
        "actions/download-artifact" => Kind::Download,
        p if p.starts_with("actions/upload-artifact/")
            || p.starts_with("actions/download-artifact/") =>
        {
            return Some(Err(format!("`{uses}`: sub-actions are not supported")));
        }
        _ => return None,
    };
    let v4 = git_ref == "v4" || git_ref.starts_with("v4.") || is_commit_sha(git_ref);
    Some(if v4 {
        Ok(kind)
    } else {
        Err(format!(
            "`{uses}`: only @v4 (or a commit of it) is supported"
        ))
    })
}

/// A full commit SHA: 40 hex digits.
fn is_commit_sha(git_ref: &str) -> bool {
    git_ref.len() == 40 && git_ref.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The warning for a step pinned to a commit: the version of a commit cannot
/// be checked here, so it runs as v4. `None` for any other `uses`.
pub fn pin_warning(uses: &str) -> Option<String> {
    let (_, git_ref) = uses.split_once('@')?;
    if !matches!(parse_uses(uses), Some(Ok(_))) || !is_commit_sha(git_ref) {
        return None;
    }
    Some(format!(
        "warning: `{uses}` is pinned to a commit, so its version cannot be checked here and it runs as v4"
    ))
}

fn literal(v: &str) -> bool {
    !v.contains("${{")
}

fn get<'a>(with: &'a [(String, String)], key: &str) -> Option<&'a str> {
    with.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.trim().is_empty())
}

/// An artifact name: one file name, so it can be a directory of the area.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("the artifact name is empty".into());
    }
    if name == "." || name == ".." || name.len() > 200 {
        return Err(format!("`{name}` is not a usable artifact name"));
    }
    match name
        .chars()
        .find(|c| c.is_control() || "\"/\\:<>|*?".contains(*c))
    {
        Some(c) => Err(format!(
            "the artifact name `{name}` has the character {c:?}, which is not allowed"
        )),
        None => Ok(()),
    }
}

/// A path under the workspace (or the managed area) in its plain form:
/// `./a//b` is `a/b`; empty is the root itself. Absolute paths, `..`, a
/// backslash and a NUL are refused.
pub fn normalize_rel(path: &str) -> Result<String, String> {
    if path.contains('\0') || path.contains('\\') {
        return Err(format!(
            "the path `{path}` has a character that is not allowed"
        ));
    }
    let b = path.as_bytes();
    if path.starts_with('/') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') {
        return Err(format!("the path `{path}` is absolute"));
    }
    let mut parts = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => return Err(format!("the path `{path}` leaves its directory with `..`")),
            c => parts.push(c),
        }
    }
    Ok(parts.join("/"))
}

/// A path as stored in a receipt: relative, not the root itself.
pub fn stored_rel(path: &str) -> Result<String, String> {
    let rel = normalize_rel(path)?;
    if rel.is_empty() {
        return Err(format!("`{path}` names no file"));
    }
    Ok(rel)
}

/// `path` without the workspace in front: `<workspace>/x` is `x` and the
/// workspace itself is the root (empty). A path that is not under the
/// workspace is taken as it is, so an absolute one is refused by
/// [`normalize_rel`]. Upload and download both read their paths this way.
pub fn workspace_rel(path: &str, workspace: &str) -> Result<String, String> {
    let ws = workspace.trim_end_matches('/');
    let inside = if ws.is_empty() {
        path
    } else if path == ws {
        ""
    } else {
        path.strip_prefix(ws)
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or(path)
    };
    normalize_rel(inside)
}

/// The name an artifact step has once its expressions are expanded. Every
/// name that reaches the file system passes here, whether or not the plan
/// could check it: a name that still holds `${{` is not exempt.
pub fn runtime_name(name: &str) -> Result<String, String> {
    validate_name(name)?;
    normalize_rel(name)
}

/// One line of an upload's `path:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub pattern: String,
    pub exclude: bool,
}

/// The lines of an upload's `path:`: one pattern per line, `!` excludes. A
/// path under `workspace` is taken relative to it; an empty path is the
/// whole workspace (`**`).
pub fn path_lines(input: &str, workspace: &str) -> Result<Vec<Line>, String> {
    let mut lines = Vec::new();
    for raw in input.lines() {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        let (exclude, body) = match t.strip_prefix('!') {
            Some(b) => (true, b.trim()),
            None => (false, t),
        };
        let rel = workspace_rel(body, workspace)?;
        let pattern = if rel.is_empty() {
            "**".to_string()
        } else {
            rel
        };
        lines.push(Line { pattern, exclude });
    }
    if lines.iter().all(|l| l.exclude) {
        return Err("`path` names nothing to upload".into());
    }
    Ok(lines)
}

/// Whether a path pattern has a wildcard.
pub fn has_glob(s: &str) -> bool {
    s.contains(['*', '?'])
}

/// The directory a pattern starts in: its components before the first one with
/// a wildcard.
pub fn literal_prefix(pattern: &str) -> String {
    pattern
        .split('/')
        .take_while(|c| !has_glob(c))
        .collect::<Vec<_>>()
        .join("/")
}

fn seg_match(p: &[char], s: &[char]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some(('*', rest)) => (0..=s.len()).any(|i| seg_match(rest, &s[i..])),
        Some(('?', rest)) => s.split_first().is_some_and(|(_, r)| seg_match(rest, r)),
        Some((c, rest)) => s.first() == Some(c) && seg_match(rest, &s[1..]),
    }
}

fn segs_match(p: &[&str], s: &[&str]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((&"**", rest)) => (0..=s.len()).any(|i| segs_match(rest, &s[i..])),
        Some((seg, rest)) => match s.split_first() {
            Some((x, r)) => {
                let (a, b): (Vec<char>, Vec<char>) = (seg.chars().collect(), x.chars().collect());
                seg_match(&a, &b) && segs_match(rest, r)
            }
            None => false,
        },
    }
}

/// `*` and `?` inside a path component, `**` for any number of components.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    segs_match(&p, &s)
}

/// A pattern selects the files it matches and, as a directory, everything
/// under what it matches.
fn line_matches(pattern: &str, file: &str) -> bool {
    glob_match(pattern, file) || glob_match(&format!("{pattern}/**"), file)
}

fn hidden(file: &str) -> bool {
    file.split('/').any(|c| c.starts_with('.'))
}

/// The files of `all` (paths relative to the workspace) the lines select,
/// sorted. Hidden files (a component starting with a dot) are left out unless
/// `include_hidden`.
pub fn select_files(lines: &[Line], all: &[String], include_hidden: bool) -> Vec<String> {
    let mut out: Vec<String> = all
        .iter()
        .filter(|f| include_hidden || !hidden(f))
        .filter(|f| {
            lines
                .iter()
                .any(|l| !l.exclude && line_matches(&l.pattern, f))
        })
        .filter(|f| {
            !lines
                .iter()
                .any(|l| l.exclude && line_matches(&l.pattern, f))
        })
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

fn parent(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(p, _)| p.to_string())
        .unwrap_or_default()
}

fn common_dir(dirs: &[String]) -> String {
    let mut common: Option<Vec<&str>> = None;
    for d in dirs {
        let parts: Vec<&str> = d.split('/').filter(|c| !c.is_empty()).collect();
        common = Some(match common {
            None => parts,
            Some(c) => c
                .iter()
                .zip(&parts)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    common.unwrap_or_default().join("/")
}

/// The name each selected file has inside the artifact: its path relative to
/// the least common ancestor of what the lines name (the directory of a
/// single file, the directory itself for a directory, the part before the
/// wildcard for a pattern), as `actions/upload-artifact` does. `(source,
/// stored)` pairs, in the order of `selected`.
pub fn stored_paths(lines: &[Line], selected: &[String]) -> Vec<(String, String)> {
    let mut roots = Vec::new();
    for l in lines.iter().filter(|l| !l.exclude) {
        let hits: Vec<&String> = selected
            .iter()
            .filter(|f| line_matches(&l.pattern, f))
            .collect();
        if hits.is_empty() {
            continue;
        }
        roots.push(if has_glob(&l.pattern) {
            literal_prefix(&l.pattern)
        } else if hits.len() == 1 && *hits[0] == l.pattern {
            parent(&l.pattern)
        } else {
            l.pattern.clone()
        });
    }
    let base = common_dir(&roots);
    selected
        .iter()
        .map(|f| {
            let stored = f
                .strip_prefix(&format!("{base}/"))
                .filter(|_| !base.is_empty())
                .unwrap_or(f);
            (f.clone(), stored.to_string())
        })
        .collect()
}

/// `if-no-files-found`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Warn,
    Error,
    Ignore,
}

pub fn missing_policy(v: Option<&str>) -> Result<Missing, String> {
    match v.map(str::trim) {
        None | Some("") | Some("warn") => Ok(Missing::Warn),
        Some("error") => Ok(Missing::Error),
        Some("ignore") => Ok(Missing::Ignore),
        Some(o) => Err(format!(
            "`if-no-files-found: {o}` is not warn, error or ignore"
        )),
    }
}

/// What an upload does with the number of files its `path` selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoFiles {
    Upload,
    Warn,
    Fail,
    Nothing,
}

pub fn when_no_files(policy: Missing, files: usize) -> NoFiles {
    if files > 0 {
        return NoFiles::Upload;
    }
    match policy {
        Missing::Warn => NoFiles::Warn,
        Missing::Error => NoFiles::Fail,
        Missing::Ignore => NoFiles::Nothing,
    }
}

/// The version a new upload of `name` becomes, given the newest version there
/// is: the first is 1; another is a conflict unless it overwrites, then it is
/// a new version (the older ones stay).
pub fn decide_version(name: &str, latest: Option<u32>, overwrite: bool) -> Result<u32, String> {
    match latest {
        None => Ok(1),
        Some(v) if overwrite => Ok(v + 1),
        Some(_) => Err(format!(
            "an artifact named `{name}` was already uploaded in this run (set `overwrite: true` to replace it, or give each upload its own name)"
        )),
    }
}

/// The version a new upload takes after finding the one it chose already
/// taken (another upload of the name claimed it a moment earlier): with
/// `overwrite` the next one, otherwise the conflict.
pub fn version_after_race(name: &str, taken: u32, overwrite: bool) -> Result<u32, String> {
    if overwrite {
        Ok(taken + 1)
    } else {
        decide_version(name, Some(taken), false)
    }
}

/// An option that is `true` or `false`; absent is `false`.
pub fn parse_bool(key: &str, v: Option<&str>) -> Result<bool, String> {
    match v.map(str::trim) {
        None | Some("") | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(o) => Err(format!("`{key}: {o}` is not true or false")),
    }
}

/// `retention-days`: whole days from 1 to 90. Recorded in the receipt; a
/// local run's artifacts are kept as long as its run directory is.
pub fn parse_retention(v: Option<&str>) -> Result<Option<u32>, String> {
    match v.map(str::trim) {
        None | Some("") => Ok(None),
        Some(t) => match t.parse::<u32>() {
            Ok(d) if (1..=90).contains(&d) => Ok(Some(d)),
            _ => Err(format!(
                "`retention-days: {t}` is not a number from 1 to 90"
            )),
        },
    }
}

/// `compression-level`: a whole number from 0 to 9. Recorded in the receipt
/// and otherwise ignored: the files are stored as they are, so it changes
/// nothing about the result.
pub fn parse_compression(v: Option<&str>) -> Result<Option<u32>, String> {
    match v.map(str::trim) {
        None | Some("") => Ok(None),
        Some(t) => match t.parse::<u32>() {
            Ok(l) if l <= 9 => Ok(Some(l)),
            _ => Err(format!(
                "`compression-level: {t}` is not a number from 0 to 9"
            )),
        },
    }
}

const UPLOAD_KEYS: &[&str] = &[
    "name",
    "path",
    "if-no-files-found",
    "overwrite",
    "retention-days",
    "compression-level",
    "include-hidden-files",
];
const DOWNLOAD_KEYS: &[&str] = &["name", "path", "pattern", "merge-multiple"];

/// What local-ci can say about a step's `with:` before anything runs: an
/// option it does not support (cross-run access, `github-token`, `run-id`,
/// `repository`, `compression-level`, ...), a value that is not valid, and an
/// upload in a matrix job whose name does not hold the variant (two variants
/// would upload the same name). A value that holds an expression is checked
/// when it is known, at run time.
pub fn validate_with(kind: Kind, with: &[(String, String)], matrix: Matrix) -> Result<(), String> {
    validate_inner(kind, with, matrix, false)
}

/// The matrix of the job a step belongs to, as far as the plan knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix<'a> {
    /// The job has no matrix.
    None,
    /// The keys of a matrix that is written out, `include` entries too.
    Axes(&'a [String]),
    /// A matrix that is an expression: its keys are known when it runs.
    Unknown,
}

/// The keys that tell the variants of a written-out `strategy.matrix` apart:
/// its axes. `include` entries add no key to ask for: one that adds keys to a
/// variant that is there (`{os: a, experimental: true}`) does not make
/// variants differ by them. A matrix of `include` entries alone has no axes,
/// so the keys on which the entries differ stand in for them. `None` for a
/// matrix (or an `include`) that is an expression.
pub fn matrix_axes(m: &serde_json::Value) -> Option<Vec<String>> {
    let obj = m.as_object()?;
    let mut keys: Vec<String> = obj
        .keys()
        .filter(|k| *k != "include" && *k != "exclude")
        .cloned()
        .collect();
    match obj.get("include") {
        Some(serde_json::Value::Array(entries)) if keys.is_empty() => {
            let entries: Vec<_> = entries.iter().filter_map(|e| e.as_object()).collect();
            let all: std::collections::BTreeSet<&String> =
                entries.iter().flat_map(|e| e.keys()).collect();
            for k in all {
                let first = entries.first().and_then(|e| e.get(k));
                if entries.iter().any(|e| e.get(k) != first) {
                    keys.push(k.clone());
                }
            }
        }
        Some(serde_json::Value::String(_)) => return None,
        _ => {}
    }
    keys.sort();
    keys.dedup();
    Some(keys)
}

/// The bodies of the `${{ ... }}` expressions in `text`, spaces trimmed.
fn expressions(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("${{") {
        let after = &rest[i + 3..];
        let Some(j) = after.find("}}") else { break };
        out.push(after[..j].trim());
        rest = &after[j + 2..];
    }
    out
}

/// Whether `body` reads `matrix.<key>` (or `matrix['key']`) as a whole word.
fn reads_key(body: &str, key: &str) -> bool {
    let dotted = format!("matrix.{key}");
    let at_word_end = |i: usize| {
        !body[i..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    for (start, _) in body.match_indices(&dotted) {
        let end = start + dotted.len();
        let word_start = !body[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        if word_start && at_word_end(end) {
            return true;
        }
    }
    body.contains(&format!("matrix['{key}']")) || body.contains(&format!("matrix[\"{key}\"]"))
}

fn squashed(body: &str) -> String {
    body.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Whether `name` holds the whole matrix (`${{ matrix }}`, `toJSON(matrix)`):
/// that expands to JSON, which a name cannot hold, so the upload would always
/// fail when it runs.
pub fn names_whole_matrix(name: &str) -> bool {
    expressions(name).iter().any(|b| {
        let q = squashed(b);
        q == "matrix" || q.contains("tojson(matrix)")
    })
}

/// Whether an upload's `name` tells the variants of a matrix apart: it reads
/// every key of the matrix or `strategy.job-index`. For a matrix that is an
/// expression (`axes` is `None`) it must read the matrix in some way.
pub fn name_separates_variants(name: &str, axes: Option<&[String]>) -> bool {
    let bodies = expressions(name);
    if bodies
        .iter()
        .any(|b| squashed(b).contains("strategy.job-index"))
    {
        return true;
    }
    match axes {
        Some(keys) => keys.iter().all(|k| bodies.iter().any(|b| reads_key(b, k))),
        None => bodies
            .iter()
            .any(|b| b.contains("matrix.") || b.contains("matrix[")),
    }
}

/// [`validate_with`] for a step about to run, its values expanded: the name is
/// checked whatever it holds, and `path` is left to the code that reads it
/// against the workspace (an absolute path inside it is what
/// `${{ github.workspace }}/x` expands to).
pub fn validate_run(kind: Kind, with: &[(String, String)]) -> Result<(), String> {
    validate_inner(kind, with, Matrix::None, true)
}

fn validate_inner(
    kind: Kind,
    with: &[(String, String)],
    matrix: Matrix,
    run: bool,
) -> Result<(), String> {
    let allowed = match kind {
        Kind::Upload => UPLOAD_KEYS,
        Kind::Download => DOWNLOAD_KEYS,
    };
    for (k, _) in with {
        if !allowed.contains(&k.as_str()) {
            return Err(format!(
                "`{k}` is not supported (local-ci runs {} within one run, with {})",
                match kind {
                    Kind::Upload => "upload-artifact",
                    Kind::Download => "download-artifact",
                },
                allowed.join(", ")
            ));
        }
    }
    let name = get(with, "name");
    if run {
        name.map(runtime_name).transpose()?;
    } else if let Some(n) = name.filter(|n| literal(n)) {
        validate_name(n)?;
    }
    match kind {
        Kind::Upload => {
            let Some(path) = get(with, "path") else {
                return Err("`path` is required".into());
            };
            if !run && literal(path) {
                path_lines(path, "")?;
            }
            missing_policy(get(with, "if-no-files-found"))?;
            parse_bool("overwrite", get(with, "overwrite"))?;
            parse_bool("include-hidden-files", get(with, "include-hidden-files"))?;
            parse_retention(get(with, "retention-days"))?;
            parse_compression(get(with, "compression-level"))?;
            let shown = name.unwrap_or("artifact");
            let axes = match matrix {
                Matrix::Axes(a) => Some(Some(a)),
                Matrix::Unknown => Some(None),
                Matrix::None => None,
            };
            if !run && names_whole_matrix(shown) {
                return Err(format!(
                    "the name `{shown}` holds the whole matrix, which is JSON, and a name cannot hold that: the upload would always fail; read the keys (`name: report-${{{{ matrix.os }}}}`) or use `${{{{ strategy.job-index }}}}`"
                ));
            }
            if let Some(axes) = axes.filter(|_| !run) {
                if !name_separates_variants(shown, axes) {
                    return Err(format!(
                        "the name `{shown}` does not tell the variants of the matrix apart{}; put every matrix key in it (`name: report-${{{{ matrix.os }}}}`) or use `${{{{ strategy.job-index }}}}`",
                        match axes {
                            Some(k) => format!(" (it must read {})", k.iter().map(|k| format!("`matrix.{k}`")).collect::<Vec<_>>().join(", ")),
                            None => String::new(),
                        }
                    ));
                }
            }
        }
        Kind::Download => {
            if name.is_some() && get(with, "pattern").is_some() {
                return Err("`name` and `pattern` cannot both be set".into());
            }
            parse_bool("merge-multiple", get(with, "merge-multiple"))?;
            if let Some(p) = get(with, "path").filter(|p| !run && literal(p)) {
                normalize_rel(p)?;
            }
        }
    }
    Ok(())
}

/// Whether an `if:` is true whenever the step is reached, so the step is as
/// unconditional as one without it: `always()`, `success()`, `!cancelled()` or
/// `true`, with or without the `${{ }}` wrapper and spaces around.
pub fn always_true(cond: &str) -> bool {
    let c = cond.trim();
    let c = match c.strip_prefix("${{").and_then(|r| r.strip_suffix("}}")) {
        Some(inner) => inner.trim(),
        None => c,
    };
    matches!(c, "always()" | "success()" | "!cancelled()" | "true")
}

/// The artifact an upload step must publish for the run to succeed, when that
/// is known from the plan: a literal name, in a job without a matrix, on a step
/// that always runs (`optional` is false) and fails without files
/// (`if-no-files-found: error`).
pub fn required_upload(uses: &str, with: &[(String, String)], optional: bool) -> Option<String> {
    if optional || !matches!(parse_uses(uses), Some(Ok(Kind::Upload))) {
        return None;
    }
    if missing_policy(get(with, "if-no-files-found")) != Ok(Missing::Error) {
        return None;
    }
    let name = get(with, "name").unwrap_or("artifact");
    literal(name).then(|| name.to_string())
}

/// A digest of what an artifact version holds: one line per file (path, size,
/// SHA-256) in path order, hashed. Two versions with the same files have the
/// same digest, whatever run or attempt wrote them.
pub fn files_digest(files: &[(String, u64, String)]) -> String {
    use sha2::{Digest, Sha256};
    let mut lines: Vec<String> = files
        .iter()
        .map(|(p, n, h)| format!("{p}\t{n}\t{h}\n"))
        .collect();
    lines.sort();
    let mut hasher = Sha256::new();
    for l in &lines {
        hasher.update(l.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The artifact a download step takes by name.
pub fn needed_download(uses: &str, with: &[(String, String)]) -> Option<String> {
    if !matches!(parse_uses(uses), Some(Ok(Kind::Download))) {
        return None;
    }
    get(with, "name").filter(|n| literal(n)).map(str::to_string)
}

/// The artifacts a download takes of the ones there are: the one called
/// `name`, those whose name matches `pattern`, or all.
pub fn select_artifacts(
    available: &[String],
    name: Option<&str>,
    pattern: Option<&str>,
) -> Result<Vec<String>, String> {
    if let Some(n) = name {
        return if available.iter().any(|a| a == n) {
            Ok(vec![n.to_string()])
        } else {
            Err(format!("no artifact named `{n}` was uploaded in this run"))
        };
    }
    let picked: Vec<String> = available
        .iter()
        .filter(|a| pattern.is_none_or(|p| glob_match(p, a)))
        .cloned()
        .collect();
    if picked.is_empty() {
        return Err(match pattern {
            Some(p) => format!("no artifact matches `{p}`"),
            None => "no artifact was uploaded in this run".into(),
        });
    }
    Ok(picked)
}

/// Where the files of one artifact go, relative to the workspace: straight
/// into `base` for one artifact taken by name and with `merge-multiple`,
/// otherwise into a directory named after the artifact.
pub fn place_dir(base: &str, artifact: &str, direct: bool) -> String {
    if direct {
        base.to_string()
    } else if base.is_empty() {
        artifact.to_string()
    } else {
        format!("{base}/{artifact}")
    }
}

/// Where one file of an artifact goes: `dir` and the path from the receipt,
/// which is checked again (a receipt is a file anyone could have edited).
pub fn target_rel(dir: &str, file: &str) -> Result<String, String> {
    let f = stored_rel(file)?;
    Ok(if dir.is_empty() {
        f
    } else {
        format!("{dir}/{f}")
    })
}

/// What a receipt says against what the download asked for: `(name, version,
/// plan digest)` of the directory it was read from and of the current run
/// against the same three in the receipt.
pub fn check_receipt(want: (&str, u32, &str), got: (&str, u32, &str)) -> Result<(), String> {
    if want.0 != got.0 {
        return Err(format!(
            "the receipt in the directory of `{}` is of the artifact `{}`",
            want.0, got.0
        ));
    }
    if want.1 != got.1 {
        return Err(format!(
            "the receipt of `{}` is of version {}, not of the version {} it was read from",
            want.0, got.1, want.1
        ));
    }
    if want.2 != got.2 {
        return Err(format!(
            "the receipt of `{}` is of another run (plan {}, this run's is {})",
            want.0, got.2, want.2
        ));
    }
    Ok(())
}

/// A stored file against its receipt entry: `(size, sha256)`.
pub fn check_digest(
    file: &str,
    artifact: &str,
    want: (u64, &str),
    got: (u64, &str),
) -> Result<(), String> {
    if want == got {
        return Ok(());
    }
    Err(format!(
        "`{file}` of artifact `{artifact}` does not match its receipt (size {} against {}, sha256 {} against {})",
        got.0, want.0, got.1, want.1
    ))
}

/// Whether the paths a download would write can all be written: no path
/// twice, none that differs from another only in case (the same file on a
/// case-insensitive file system, the default of macOS), none that is a file
/// where another is under it as a directory.
pub fn find_collision(dests: &[String]) -> Result<(), String> {
    let mut seen = std::collections::BTreeMap::new();
    for d in dests {
        if let Some(prev) = seen.insert(d.to_lowercase(), d) {
            return Err(if prev == d {
                format!("two files would both write `{d}`")
            } else {
                format!("`{prev}` and `{d}` differ only in case, which is one file on a case-insensitive file system")
            });
        }
    }
    for d in dests {
        let folded = d.to_lowercase();
        for (i, _) in folded.match_indices('/') {
            if let Some(file) = seen.get(&folded[..i]) {
                return Err(format!(
                    "`{file}` is a file, and `{d}` needs it as a directory"
                ));
            }
        }
    }
    Ok(())
}

/// What is at one component of a path a download writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    Missing,
    Dir,
    File,
    /// A symlink, a device, a pipe...
    Other,
}

/// Whether the file `dest` can be written given what is there: `found` has
/// one entry per component, from the first, up to the first that is missing
/// (what is under a missing one is missing too). Each component that exists
/// must be a directory, and the last a regular file or nothing.
pub fn check_path_kinds(dest: &str, found: &[Found]) -> Result<(), String> {
    let comps: Vec<&str> = dest.split('/').collect();
    for (i, f) in found.iter().enumerate() {
        match (f, i + 1 == comps.len()) {
            (Found::Missing, _) => return Ok(()),
            (Found::Dir, false) | (Found::File, true) => {}
            (_, false) => {
                return Err(format!(
                    "`{}` exists and is not a directory, and `{dest}` needs it as one",
                    comps[..=i].join("/")
                ))
            }
            (_, true) => {
                return Err(format!(
                    "`{dest}` exists and is not a file; it is not replaced"
                ))
            }
        }
    }
    Ok(())
}

/// Whether an upload of `name` takes the place of an artifact that is there
/// under a name that differs from it only in case.
pub fn name_collision(existing: &[String], name: &str) -> Result<(), String> {
    let folded = name.to_lowercase();
    match existing
        .iter()
        .find(|e| e.as_str() != name && e.to_lowercase() == folded)
    {
        Some(e) => Err(format!(
            "an artifact named `{e}` is there, and `{name}` differs from it only in case, which is one name on a case-insensitive file system"
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_two_actions_at_v4() {
        assert_eq!(
            parse_uses("actions/upload-artifact@v4"),
            Some(Ok(Kind::Upload))
        );
        assert_eq!(
            parse_uses("Actions/Download-Artifact@v4.1.8"),
            Some(Ok(Kind::Download))
        );
        assert!(
            matches!(parse_uses("actions/upload-artifact@v3"), Some(Err(e)) if e.contains("@v4"))
        );
        assert!(matches!(
            parse_uses("actions/upload-artifact@v40"),
            Some(Err(_))
        ));
        assert!(matches!(
            parse_uses("actions/upload-artifact"),
            Some(Err(_))
        ));
        assert!(matches!(
            parse_uses("actions/download-artifact@abc123"),
            Some(Err(_))
        ));
        assert!(
            matches!(parse_uses("actions/upload-artifact/merge@v4"), Some(Err(e)) if e.contains("sub-actions"))
        );
        assert!(matches!(
            parse_uses("actions/download-artifact/x@v4"),
            Some(Err(_))
        ));
        assert_eq!(parse_uses("someone/upload-artifact@v4"), None);
        assert_eq!(parse_uses("actions/checkout@v4"), None);
    }

    #[test]
    fn names() {
        assert!(validate_name("report-1.2_x y").is_ok());
        for bad in [
            "", "  ", ".", "..", "a/b", "a\\b", "a:b", "a*", "a?", "a<b", "a>b", "a|b", "a\"b",
            "a\nb",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_name(&"x".repeat(200)).is_ok());
        assert!(validate_name(&"x".repeat(201)).is_err());
        assert!(validate_name("a/b").unwrap_err().contains("not allowed"));
    }

    #[test]
    fn paths_are_plain_or_refused() {
        assert_eq!(normalize_rel("a/b").unwrap(), "a/b");
        assert_eq!(normalize_rel("./a//b/./c").unwrap(), "a/b/c");
        assert_eq!(normalize_rel("").unwrap(), "");
        assert_eq!(normalize_rel(".").unwrap(), "");
        assert_eq!(normalize_rel("a..b/..c").unwrap(), "a..b/..c");
        for bad in [
            "/etc/passwd",
            "..",
            "../x",
            "a/../b",
            "a/..",
            "a\\b",
            "a\0b",
            "C:/x",
            "c:x",
        ] {
            assert!(normalize_rel(bad).is_err(), "{bad:?}");
        }
        assert!(normalize_rel("/x").unwrap_err().contains("absolute"));
        assert!(normalize_rel("a/..").unwrap_err().contains("`..`"));
        assert!(normalize_rel("a\\b").unwrap_err().contains("character"));
        assert!(normalize_rel("ab:c").is_ok());
        assert!(normalize_rel("1:x").is_ok());
        assert_eq!(stored_rel("./a/b").unwrap(), "a/b");
        assert!(stored_rel("").is_err());
        assert!(stored_rel(".").unwrap_err().contains("no file"));
        assert!(stored_rel("../x").is_err());
    }

    #[test]
    fn path_lines_are_patterns_with_exclusions() {
        let l = path_lines("dist/**\n\n  !dist/tmp  \n./out/a.txt\n", "").unwrap();
        assert_eq!(
            l,
            vec![
                Line {
                    pattern: "dist/**".into(),
                    exclude: false
                },
                Line {
                    pattern: "dist/tmp".into(),
                    exclude: true
                },
                Line {
                    pattern: "out/a.txt".into(),
                    exclude: false
                },
            ]
        );
        // a path inside the workspace is taken relative to it
        let l = path_lines("/ws/dist\n/wsx/y", "/ws").unwrap_err();
        assert!(l.contains("absolute"));
        // the workspace itself, with no slash, is the root
        assert_eq!(path_lines("/ws", "/ws").unwrap()[0].pattern, "**");
        let l = path_lines("/ws/dist\n/ws/\n", "/ws").unwrap();
        assert_eq!(l[0].pattern, "dist");
        assert_eq!(l[1].pattern, "**");
        assert_eq!(path_lines(".", "").unwrap()[0].pattern, "**");
        // an absolute path elsewhere, `..` and nothing at all are refused
        assert!(path_lines("/etc", "/ws").is_err());
        assert!(path_lines("/ws/../etc", "/ws").is_err());
        assert!(path_lines("../x", "").is_err());
        assert!(path_lines("", "").unwrap_err().contains("nothing"));
        assert!(path_lines("!a", "").is_err());
        assert!(path_lines("a\n!b", "").is_ok());
    }

    #[test]
    fn globs() {
        assert!(glob_match("a/b", "a/b"));
        assert!(!glob_match("a/b", "a/c"));
        assert!(!glob_match("a/b", "a"));
        assert!(!glob_match("a", "a/b"));
        assert!(glob_match("*.js", "x.js"));
        assert!(!glob_match("*.js", "a/x.js"));
        assert!(glob_match("a/*", "a/x"));
        assert!(!glob_match("a/*", "a"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("**", "a/b/c"));
        assert!(glob_match("a/**", "a/b/c"));
        assert!(glob_match("a/**", "a"));
        assert!(glob_match("a/**/d", "a/d"));
        assert!(glob_match("a/**/d", "a/b/c/d"));
        assert!(!glob_match("a/**/d", "a/b/c/e"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
        assert!(!glob_match("abc", "abd"));
        assert!(!glob_match("abc", "ab"));
        assert!(has_glob("a/*"));
        assert!(has_glob("a?"));
        assert!(!has_glob("a/b"));
        assert_eq!(literal_prefix("dist/sub/*.js"), "dist/sub");
        assert_eq!(literal_prefix("dist/a.txt"), "dist/a.txt");
        assert_eq!(literal_prefix("**"), "");
        assert_eq!(literal_prefix("a/b?/c"), "a");
    }

    fn files() -> Vec<String> {
        s(&[
            "dist/a.js",
            "dist/sub/b.js",
            "dist/tmp/c.js",
            "dist/.hidden",
            ".env",
            "out/r.txt",
            "readme",
        ])
    }

    #[test]
    fn files_are_selected_by_the_lines() {
        let sel =
            |p: &str, hidden: bool| select_files(&path_lines(p, "").unwrap(), &files(), hidden);
        assert_eq!(sel("dist/*.js", false), s(&["dist/a.js"]));
        // a directory takes everything under it, hidden files only when asked
        assert_eq!(
            sel("dist", false),
            s(&["dist/a.js", "dist/sub/b.js", "dist/tmp/c.js"])
        );
        assert_eq!(
            sel("dist", true),
            s(&[
                "dist/.hidden",
                "dist/a.js",
                "dist/sub/b.js",
                "dist/tmp/c.js"
            ])
        );
        assert_eq!(
            sel("dist\n!dist/tmp", false),
            s(&["dist/a.js", "dist/sub/b.js"])
        );
        assert_eq!(
            sel("dist/a.js\nreadme\ndist/a.js", false),
            s(&["dist/a.js", "readme"])
        );
        assert_eq!(sel("nothing", false), Vec::<String>::new());
        assert_eq!(sel("**", false).len(), 5);
        assert_eq!(sel("**", true).len(), 7);
        // an excluding line alone does not select
        let only = [Line {
            pattern: "dist".into(),
            exclude: true,
        }];
        assert!(select_files(&only, &files(), true).is_empty());
    }

    #[test]
    fn stored_names_are_relative_to_the_common_ancestor() {
        let st = |p: &str| {
            let lines = path_lines(p, "").unwrap();
            let sel = select_files(&lines, &files(), false);
            stored_paths(&lines, &sel)
                .into_iter()
                .map(|(_, d)| d)
                .collect::<Vec<_>>()
        };
        // a single file is its own name
        assert_eq!(st("dist/a.js"), s(&["a.js"]));
        // a directory is its contents
        assert_eq!(st("dist"), s(&["a.js", "sub/b.js", "tmp/c.js"]));
        assert_eq!(st("dist/**"), s(&["a.js", "sub/b.js", "tmp/c.js"]));
        assert_eq!(st("dist/*.js"), s(&["a.js"]));
        // lines from two directories keep their paths
        assert_eq!(st("dist/a.js\nout/r.txt"), s(&["dist/a.js", "out/r.txt"]));
        // the whole workspace keeps all paths
        assert_eq!(st("**").len(), 5);
        assert!(st("**").contains(&"dist/a.js".to_string()));
        // two files under one directory
        assert_eq!(st("dist/a.js\ndist/sub/b.js"), s(&["a.js", "sub/b.js"]));
        // a line that selected nothing does not move the root
        assert_eq!(st("dist/a.js\nnothing/**"), s(&["a.js"]));
        // pairs hold the source too
        let lines = path_lines("dist/sub", "").unwrap();
        let sel = select_files(&lines, &files(), false);
        assert_eq!(
            stored_paths(&lines, &sel),
            vec![("dist/sub/b.js".to_string(), "b.js".to_string())]
        );
        // a literal file that is not alone in its hits is a directory
        assert_eq!(parent("a/b"), "a");
        assert_eq!(parent("a"), "");
        assert_eq!(common_dir(&[]), "");
        assert_eq!(common_dir(&s(&["a/b", "a/c"])), "a");
        assert_eq!(common_dir(&s(&["a/b", "a/b/c"])), "a/b");
        assert_eq!(common_dir(&s(&["a", "b"])), "");
        assert_eq!(common_dir(&s(&["a/b"])), "a/b");
    }

    #[test]
    fn a_file_named_like_its_directory_prefix_keeps_its_name() {
        // `a` is a file and `ab/x` another: a base that is a mere string
        // prefix of a path does not strip it
        let lines = path_lines("a\nab/x", "").unwrap();
        let sel = select_files(&lines, &s(&["a", "ab/x"]), false);
        assert_eq!(
            stored_paths(&lines, &sel),
            vec![
                ("a".to_string(), "a".to_string()),
                ("ab/x".to_string(), "ab/x".to_string())
            ]
        );
    }

    #[test]
    fn no_files_found() {
        assert_eq!(missing_policy(None), Ok(Missing::Warn));
        assert_eq!(missing_policy(Some("")), Ok(Missing::Warn));
        assert_eq!(missing_policy(Some("warn")), Ok(Missing::Warn));
        assert_eq!(missing_policy(Some(" error ")), Ok(Missing::Error));
        assert_eq!(missing_policy(Some("ignore")), Ok(Missing::Ignore));
        assert!(missing_policy(Some("abort")).unwrap_err().contains("abort"));
        for p in [Missing::Warn, Missing::Error, Missing::Ignore] {
            assert_eq!(when_no_files(p, 1), NoFiles::Upload);
        }
        assert_eq!(when_no_files(Missing::Warn, 0), NoFiles::Warn);
        assert_eq!(when_no_files(Missing::Error, 0), NoFiles::Fail);
        assert_eq!(when_no_files(Missing::Ignore, 0), NoFiles::Nothing);
    }

    #[test]
    fn versions_and_conflicts() {
        assert_eq!(decide_version("r", None, false), Ok(1));
        assert_eq!(decide_version("r", None, true), Ok(1));
        assert_eq!(decide_version("r", Some(1), true), Ok(2));
        assert_eq!(decide_version("r", Some(4), true), Ok(5));
        let e = decide_version("r", Some(1), false).unwrap_err();
        assert!(e.contains("`r`") && e.contains("overwrite"), "{e}");
    }

    #[test]
    fn booleans_and_retention() {
        assert_eq!(parse_bool("k", None), Ok(false));
        assert_eq!(parse_bool("k", Some("")), Ok(false));
        assert_eq!(parse_bool("k", Some("false")), Ok(false));
        assert_eq!(parse_bool("k", Some(" true ")), Ok(true));
        assert!(parse_bool("overwrite", Some("yes"))
            .unwrap_err()
            .contains("overwrite"));
        assert_eq!(parse_retention(None), Ok(None));
        assert_eq!(parse_retention(Some("")), Ok(None));
        assert_eq!(parse_retention(Some("1")), Ok(Some(1)));
        assert_eq!(parse_retention(Some("90")), Ok(Some(90)));
        assert!(parse_retention(Some("0")).is_err());
        assert!(parse_retention(Some("91")).is_err());
        assert!(parse_retention(Some("x")).is_err());
        assert!(parse_retention(Some("-1")).is_err());
    }

    #[test]
    fn options_that_are_not_supported_are_errors() {
        use Kind::*;
        let ok = |k, p: &[(&str, &str)], m| validate_with(k, &w(p), m);
        assert!(ok(Upload, &[("name", "r"), ("path", "dist")], Matrix::None).is_ok());
        assert!(ok(Upload, &[("path", "dist")], Matrix::None).is_ok());
        assert!(ok(
            Upload,
            &[
                ("name", "r"),
                ("path", "dist\n!dist/x"),
                ("if-no-files-found", "error"),
                ("overwrite", "true"),
                ("retention-days", "5"),
                ("include-hidden-files", "true"),
            ],
            Matrix::None
        )
        .is_ok());
        assert!(ok(Download, &[("name", "r"), ("path", "out")], Matrix::None).is_ok());
        assert!(ok(Download, &[], Matrix::None).is_ok());
        assert!(ok(
            Download,
            &[("pattern", "r-*"), ("merge-multiple", "true")],
            Matrix::None
        )
        .is_ok());
        for k in [
            "github-token",
            "run-id",
            "repository",
            "artifact-ids",
            "skip-decompress",
            "unknown",
        ] {
            let e = ok(Download, &[("name", "r"), (k, "x")], Matrix::None).unwrap_err();
            assert!(e.contains(k) && e.contains("download-artifact"), "{e}");
            let e = ok(Upload, &[("path", "d"), (k, "x")], Matrix::None).unwrap_err();
            assert!(e.contains(k) && e.contains("upload-artifact"), "{e}");
        }
        // values
        assert!(ok(Upload, &[], Matrix::None)
            .unwrap_err()
            .contains("required"));
        assert!(ok(Upload, &[("path", " ")], Matrix::None)
            .unwrap_err()
            .contains("required"));
        assert!(ok(Upload, &[("path", "../x")], Matrix::None).is_err());
        assert!(ok(Upload, &[("path", "/x")], Matrix::None).is_err());
        assert!(ok(Upload, &[("path", "d"), ("name", "a/b")], Matrix::None).is_err());
        assert!(ok(
            Upload,
            &[("path", "d"), ("if-no-files-found", "x")],
            Matrix::None
        )
        .is_err());
        assert!(ok(Upload, &[("path", "d"), ("overwrite", "x")], Matrix::None).is_err());
        assert!(ok(
            Upload,
            &[("path", "d"), ("include-hidden-files", "x")],
            Matrix::None
        )
        .is_err());
        assert!(ok(
            Upload,
            &[("path", "d"), ("retention-days", "x")],
            Matrix::None
        )
        .is_err());
        assert!(ok(Download, &[("name", "a"), ("pattern", "b")], Matrix::None).is_err());
        assert!(ok(Download, &[("merge-multiple", "x")], Matrix::None).is_err());
        assert!(ok(Download, &[("path", "/x")], Matrix::None).is_err());
        assert!(ok(Download, &[("path", "../x")], Matrix::None).is_err());
        assert!(ok(Download, &[("name", "a:b")], Matrix::None).is_err());
        // an expression is checked when its value is known
        assert!(ok(
            Upload,
            &[("path", "${{ x }}/y"), ("name", "${{ x }}:")],
            Matrix::None
        )
        .is_ok());
        assert!(ok(
            Download,
            &[("path", "${{ x }}/../y"), ("name", "${{ x }}:")],
            Matrix::None
        )
        .is_ok());
        // a matrix job needs the variant in the name
        let e = ok(Upload, &[("path", "d"), ("name", "r")], Matrix::Unknown).unwrap_err();
        assert!(e.contains("matrix"), "{e}");
        assert!(ok(Upload, &[("path", "d")], Matrix::Unknown).is_err());
        assert!(ok(
            Upload,
            &[("path", "d"), ("name", "r-${{ github.run_id }}")],
            Matrix::Unknown
        )
        .is_err());
        assert!(ok(
            Upload,
            &[("path", "d"), ("name", "matrix.x")],
            Matrix::Unknown
        )
        .is_err());
        assert!(ok(
            Upload,
            &[("path", "d"), ("name", "r-${{ matrix.os }}")],
            Matrix::Unknown
        )
        .is_ok());
        assert!(ok(Download, &[("name", "r")], Matrix::Unknown).is_ok());
        // a written-out matrix: every key, with or without `overwrite`
        let keys = s(&["node", "os"]);
        let axes = Matrix::Axes(&keys);
        for extra in [&[][..], &[("overwrite", "true")][..]] {
            let up = |name: &str| {
                let mut p = vec![("path", "d"), ("name", name)];
                p.extend_from_slice(extra);
                ok(Upload, &p, axes)
            };
            let e = up("r-${{ matrix.os }}").unwrap_err();
            assert!(e.contains("matrix.node") && e.contains("matrix.os"), "{e}");
            assert!(up("r-${{ matrix.node }}").is_err());
            assert!(up("r-${{ matrix.os }}-${{ matrix.node }}").is_ok());
            assert!(up("r-${{ matrix }}").is_err());
            assert!(up("r-${{ toJSON(matrix) }}").is_err());
            assert!(up("r-${{ strategy.job-index }}").is_ok());
        }
        // no matrix: nothing to tell apart
        assert!(ok(Upload, &[("path", "d"), ("name", "r")], Matrix::None).is_ok());
    }

    #[test]
    fn what_the_plan_requires() {
        let up = "actions/upload-artifact@v4";
        let err = [("name", "r"), ("path", "d"), ("if-no-files-found", "error")];
        assert_eq!(required_upload(up, &w(&err), false), Some("r".into()));
        assert_eq!(required_upload(up, &w(&err), true), None);
        assert_eq!(
            required_upload("actions/download-artifact@v4", &w(&err), false),
            None
        );
        assert_eq!(
            required_upload("actions/checkout@v4", &w(&err), false),
            None
        );
        assert_eq!(
            required_upload("actions/upload-artifact@v3", &w(&err), false),
            None
        );
        // warn (the default) and ignore do not require
        assert_eq!(
            required_upload(up, &w(&[("name", "r"), ("path", "d")]), false),
            None
        );
        let ig = [
            ("name", "r"),
            ("path", "d"),
            ("if-no-files-found", "ignore"),
        ];
        assert_eq!(required_upload(up, &w(&ig), false), None);
        // a name that is not known now cannot be required
        let ex = [
            ("name", "r-${{ matrix.a }}"),
            ("path", "d"),
            ("if-no-files-found", "error"),
        ];
        assert_eq!(required_upload(up, &w(&ex), false), None);
        // the default name
        let dn = [("path", "d"), ("if-no-files-found", "error")];
        assert_eq!(required_upload(up, &w(&dn), false), Some("artifact".into()));

        let down = "actions/download-artifact@v4";
        assert_eq!(
            needed_download(down, &w(&[("name", "r")])),
            Some("r".into())
        );
        assert_eq!(needed_download(down, &w(&[])), None);
        assert_eq!(needed_download(down, &w(&[("name", "${{ x }}")])), None);
        assert_eq!(needed_download(up, &w(&[("name", "r")])), None);
        assert_eq!(
            needed_download("actions/download-artifact@v3", &w(&[("name", "r")])),
            None
        );
    }

    #[test]
    fn a_download_picks_its_artifacts() {
        let have = s(&["r-1", "r-2", "other"]);
        assert_eq!(
            select_artifacts(&have, Some("other"), None),
            Ok(s(&["other"]))
        );
        assert!(select_artifacts(&have, Some("nope"), None)
            .unwrap_err()
            .contains("`nope`"));
        assert_eq!(
            select_artifacts(&have, None, Some("r-*")),
            Ok(s(&["r-1", "r-2"]))
        );
        assert!(select_artifacts(&have, None, Some("x*"))
            .unwrap_err()
            .contains("`x*`"));
        assert_eq!(select_artifacts(&have, None, None), Ok(have.clone()));
        assert!(select_artifacts(&[], None, None)
            .unwrap_err()
            .contains("no artifact was"));
        // a name wins over a pattern that is not there to be checked
        assert_eq!(
            select_artifacts(&have, Some("r-1"), Some("x")),
            Ok(s(&["r-1"]))
        );
    }

    #[test]
    fn where_files_go() {
        assert_eq!(place_dir("", "a", true), "");
        assert_eq!(place_dir("out", "a", true), "out");
        assert_eq!(place_dir("", "a", false), "a");
        assert_eq!(place_dir("out", "a", false), "out/a");
        assert_eq!(target_rel("", "x/y").unwrap(), "x/y");
        assert_eq!(target_rel("out", "./x/y").unwrap(), "out/x/y");
        assert!(target_rel("out", "../y").is_err());
        assert!(target_rel("out", "/y").is_err());
        assert!(target_rel("out", "").is_err());
    }
    #[test]
    fn a_path_is_read_the_same_for_upload_and_download() {
        assert_eq!(workspace_rel("/ws/x/y", "/ws").unwrap(), "x/y");
        assert_eq!(workspace_rel("/ws", "/ws").unwrap(), "");
        assert_eq!(workspace_rel("/ws/", "/ws").unwrap(), "");
        assert_eq!(workspace_rel("/ws/x", "/ws/").unwrap(), "x");
        assert_eq!(workspace_rel("x/./y", "/ws").unwrap(), "x/y");
        assert_eq!(workspace_rel("", "/ws").unwrap(), "");
        assert_eq!(workspace_rel("a/b", "").unwrap(), "a/b");
        // still outside after stripping, or only the same prefix
        assert!(workspace_rel("/ws/../x", "/ws")
            .unwrap_err()
            .contains("`..`"));
        assert!(workspace_rel("/wsx/y", "/ws")
            .unwrap_err()
            .contains("absolute"));
        assert!(workspace_rel("/etc/x", "/ws")
            .unwrap_err()
            .contains("absolute"));
        assert!(workspace_rel("/x", "").unwrap_err().contains("absolute"));
    }

    #[test]
    fn a_name_is_checked_after_expansion_even_with_an_expression_left_in() {
        assert_eq!(runtime_name("report-1").unwrap(), "report-1");
        for bad in [
            "",
            "..",
            "a/b",
            "../x",
            "${{ x }}/../..",
            "a\\b",
            "a:b",
            "x\0y",
        ] {
            assert!(runtime_name(bad).is_err(), "{bad:?}");
        }
        // the plan skips a name that holds an expression; the run does not
        assert!(validate_with(
            Kind::Upload,
            &w(&[("name", "${{ x }}/y"), ("path", "a")]),
            Matrix::None
        )
        .is_ok());
        assert!(runtime_name("${{ x }}/y").is_err());
        assert_eq!(runtime_name("${{ x }}").unwrap(), "${{ x }}");
    }

    #[test]
    fn a_receipt_must_be_of_what_was_asked_for() {
        let ok = ("n", 2, "sha256:p");
        assert!(check_receipt(ok, ok).is_ok());
        let e = check_receipt(ok, ("m", 2, "sha256:p")).unwrap_err();
        assert!(e.contains("`n`") && e.contains("artifact `m`"), "{e}");
        let e = check_receipt(ok, ("n", 3, "sha256:p")).unwrap_err();
        assert!(e.contains("version 3") && e.contains("version 2"), "{e}");
        let e = check_receipt(ok, ("n", 2, "sha256:q")).unwrap_err();
        let ok = e.contains("another run") && e.contains("sha256:q") && e.contains("sha256:p");
        assert!(ok, "{e}");
    }

    #[test]
    fn a_stored_file_must_match_its_entry() {
        assert!(check_digest("f", "n", (3, "aa"), (3, "aa")).is_ok());
        let e = check_digest("f", "n", (3, "aa"), (4, "aa")).unwrap_err();
        assert!(e.contains("size 4 against 3"), "{e}");
        let e = check_digest("f", "n", (3, "aa"), (3, "bb")).unwrap_err();
        let ok = e.contains("sha256 bb against aa") && e.contains("`f` of artifact `n`");
        assert!(ok, "{e}");
    }

    #[test]
    fn paths_that_would_be_one_file_are_a_collision() {
        assert!(find_collision(&[]).is_ok());
        assert!(find_collision(&s(&["a", "b/x", "c/y", "c/d/e", "ab"])).is_ok());
        let e = find_collision(&s(&["a", "b", "a"])).unwrap_err();
        assert_eq!(e, "two files would both write `a`");
        let e = find_collision(&s(&["Out/A.txt", "out/a.txt"])).unwrap_err();
        let ok = e.contains("`Out/A.txt`") && e.contains("`out/a.txt`") && e.contains("case");
        assert!(ok, "{e}");
        // a file where another needs a directory, in either order and in any case
        for order in [["a", "a/b"], ["a/b", "a"], ["A", "a/b"], ["a/b/c", "A/B"]] {
            let e = find_collision(&s(&order)).unwrap_err();
            assert!(e.contains("as a directory"), "{order:?}: {e}");
        }
        // a sibling with the same start is not a parent
        assert!(find_collision(&s(&["ab", "a/b", "a/b2"])).is_ok());
    }

    #[test]
    fn an_artifact_name_that_differs_only_in_case_is_a_collision() {
        let have = s(&["Report", "other"]);
        assert!(name_collision(&have, "Report").is_ok());
        assert!(name_collision(&have, "fresh").is_ok());
        assert!(name_collision(&[], "x").is_ok());
        let e = name_collision(&have, "report").unwrap_err();
        let ok = e.contains("`Report`") && e.contains("`report`") && e.contains("case");
        assert!(ok, "{e}");
        assert!(name_collision(&have, "OTHER").is_err());
    }

    #[test]
    fn a_race_for_a_version_overwrites_onto_the_next_or_conflicts() {
        assert_eq!(version_after_race("n", 3, true), Ok(4));
        let e = version_after_race("n", 3, false).unwrap_err();
        assert!(e.contains("already uploaded") && e.contains("`n`"), "{e}");
    }

    #[test]
    fn a_step_about_to_run_is_checked_by_its_values_not_by_the_plan() {
        let run = |k, p: &[(&str, &str)]| validate_run(k, &w(p));
        // an absolute path in the workspace is read against it later
        assert!(run(Kind::Upload, &[("path", "/ws/dist")]).is_ok());
        assert!(run(Kind::Download, &[("name", "n"), ("path", "/ws/x")]).is_ok());
        // an upload in a matrix whose name has no variant is the plan's to refuse
        assert!(validate_with(Kind::Upload, &w(&[("path", "a")]), Matrix::Unknown).is_err());
        assert!(run(Kind::Upload, &[("path", "a")]).is_ok());
        // the name is checked even holding an expression, and options are
        assert!(run(Kind::Upload, &[("name", "${{ x }}/../y"), ("path", "a")]).is_err());
        assert!(run(Kind::Download, &[("name", "a/b")]).is_err());
        assert!(run(Kind::Download, &[("name", "ok")]).is_ok());
        assert!(run(Kind::Upload, &[("path", "a"), ("run-id", "1")]).is_err());
        assert!(run(Kind::Upload, &[("path", "a"), ("overwrite", "x")]).is_err());
        assert!(run(Kind::Download, &[("name", "a"), ("pattern", "b")]).is_err());
    }

    #[test]
    fn a_commit_pin_runs_as_v4_with_a_warning() {
        let sha = "ea165f8d65b6e75b540449e92b4886f43607fa02";
        let uses = format!("actions/upload-artifact@{sha}");
        assert_eq!(parse_uses(&uses), Some(Ok(Kind::Upload)));
        assert_eq!(
            parse_uses(&format!("actions/download-artifact@{}", sha.to_uppercase())),
            Some(Ok(Kind::Download))
        );
        let w = pin_warning(&uses).unwrap();
        let said = w.starts_with("warning:") && w.contains(sha) && w.contains("v4");
        assert!(said, "{w}");
        // not 40 hex digits, or not a commit: still the old errors
        for bad in [&sha[..39], &format!("{sha}0"), &sha.replace('e', "g")] {
            let u = format!("actions/upload-artifact@{bad}");
            assert!(matches!(parse_uses(&u), Some(Err(_))), "{u}");
            assert_eq!(pin_warning(&u), None, "{u}");
        }
        for v in ["v3", "v5", "v5.0.0"] {
            let u = format!("actions/upload-artifact@{v}");
            let said = matches!(parse_uses(&u), Some(Err(e)) if e.contains("v4"));
            assert!(said, "{u}");
        }
        // a tag, a third party and no ref at all carry no warning
        assert_eq!(pin_warning("actions/upload-artifact@v4"), None);
        assert_eq!(pin_warning("actions/upload-artifact"), None);
        assert_eq!(pin_warning(&format!("someone/upload-artifact@{sha}")), None);
        assert_eq!(pin_warning(&format!("actions/checkout@{sha}")), None);
    }

    #[test]
    fn compression_level_is_a_number_from_0_to_9() {
        assert_eq!(parse_compression(None), Ok(None));
        assert_eq!(parse_compression(Some(" ")), Ok(None));
        assert_eq!(parse_compression(Some("0")), Ok(Some(0)));
        assert_eq!(parse_compression(Some(" 9 ")), Ok(Some(9)));
        for bad in ["10", "-1", "x", "1.5"] {
            let e = parse_compression(Some(bad)).unwrap_err();
            assert!(e.contains("compression-level") && e.contains(bad), "{e}");
        }
        let up = |v: &str| {
            validate_with(
                Kind::Upload,
                &w(&[("path", "d"), ("compression-level", v)]),
                Matrix::None,
            )
        };
        assert!(up("6").is_ok());
        assert!(up("10").is_err());
        // an upload only: a download has no such option
        assert!(validate_with(
            Kind::Download,
            &w(&[("name", "r"), ("compression-level", "1")]),
            Matrix::None
        )
        .is_err());
    }

    #[test]
    fn the_keys_of_a_matrix() {
        use serde_json::json;
        assert_eq!(
            matrix_axes(&json!({"os": ["a"], "node": [1], "exclude": [{"os": "a"}]})),
            Some(s(&["node", "os"]))
        );
        // include adds none: it adds keys to variants that are there
        assert_eq!(
            matrix_axes(
                &json!({"os": ["a"], "include": [{"os": "b", "extra": 1}, 3, {"more": 2}]})
            ),
            Some(s(&["os"]))
        );
        assert_eq!(
            matrix_axes(&json!({"os": ["a", "b"], "include": [{"os": "a", "experimental": true}]})),
            Some(s(&["os"]))
        );
        // only include: the keys the entries differ on
        assert_eq!(
            matrix_axes(&json!({"include": [{"name": "x", "shard": 1}]})),
            Some(s(&[]))
        );
        assert_eq!(
            matrix_axes(&json!({"include": [{"os": "a", "n": 1}, {"os": "b", "n": 1}]})),
            Some(s(&["os"]))
        );
        assert_eq!(
            matrix_axes(
                &json!({"include": [{"os": "a", "n": 1}, {"os": "a", "n": 2}, {"os": "a"}]})
            ),
            Some(s(&["n"]))
        );
        assert_eq!(
            matrix_axes(&json!({"include": [{"os": "a"}, {"n": 1}]})),
            Some(s(&["n", "os"]))
        );
        assert_eq!(matrix_axes(&json!({"include": []})), Some(s(&[])));
        assert_eq!(
            matrix_axes(&json!({"os": ["a"], "include": 7})),
            Some(s(&["os"]))
        );
        // an expression says nothing
        assert_eq!(matrix_axes(&json!("${{ fromJSON(x) }}")), None);
        assert_eq!(
            matrix_axes(&json!({"os": ["a"], "include": "${{ x }}"})),
            None
        );
    }

    #[test]
    fn a_name_that_tells_the_variants_apart() {
        let keys = s(&["node", "os"]);
        let k = Some(&keys[..]);
        let sep = |n: &str| name_separates_variants(n, k);
        assert!(sep("r-${{ matrix.os }}-${{matrix.node}}"));
        assert!(sep("r-${{ matrix.os }}-${{ matrix['node'] }}"));
        assert!(sep("r-${{ matrix.os }}-${{ matrix[\"node\"] }}"));
        assert!(sep("r-${{ format('{0}-{1}', matrix.os, matrix.node) }}"));
        assert!(!sep("r-${{ matrix.os }}"));
        assert!(!sep("r-${{ matrix.node }}"));
        assert!(!sep("r"));
        assert!(!sep("matrix.os matrix.node"));
        // a key that only starts like the real one, or belongs to another object
        assert!(!sep("${{ matrix.os }}-${{ matrix.nodes }}"));
        assert!(!sep("${{ matrix.os }}-${{ matrix.node-x }}"));
        assert!(!sep("${{ matrix.os }}-${{ inputs.matrix.node }}"));
        assert!(!sep("${{ matrix.os }}-${{ xmatrix.node }}"));
        assert!(sep("${{ matrix.os }}-${{ x.y == 1 && matrix.node }}"));
        assert!(!sep("${{ matrix.os }}-${{ matrix.node"));
        // the index of the variant
        for n in [
            "r-${{ hashFiles('x') }}-${{ strategy.job-index }}",
            "r-${{ Strategy.Job-Index }}",
        ] {
            assert!(sep(n), "{n}");
        }
        // the whole matrix tells nothing apart (it is refused on its own)
        assert!(!sep("r-${{ matrix }}"));
        assert!(!sep("r-${{ toJSON(matrix) }}"));
        assert!(!sep("r-${{ toJSON(matrix.os) }}"));
        assert!(!sep("r-${{ matrixx }}"));
        // no key: a literal name is fine for nothing to tell apart
        assert!(name_separates_variants("r", Some(&[])));
        // a matrix that is an expression: reading it somehow is enough
        assert!(name_separates_variants("r-${{ matrix.os }}", None));
        assert!(name_separates_variants("r-${{ matrix['os'] }}", None));
        assert!(!name_separates_variants("r-${{ github.run_id }}", None));
        assert!(!name_separates_variants("matrix.os", None));
        assert!(!name_separates_variants("r", None));
    }

    #[test]
    fn a_name_that_holds_the_whole_matrix() {
        for n in [
            "r-${{ matrix }}",
            "r-${{matrix}}",
            "r-${{ toJSON(matrix) }}",
            "r-${{ toJson( matrix ) }}",
        ] {
            assert!(names_whole_matrix(n), "{n}");
        }
        for n in [
            "r",
            "r-${{ matrix.os }}",
            "r-${{ toJSON(matrix.os) }}",
            "r-${{ matrixx }}",
            "matrix",
        ] {
            assert!(!names_whole_matrix(n), "{n}");
        }
        let with = |n: &str| {
            vec![
                ("name".to_string(), n.to_string()),
                ("path".to_string(), "p".to_string()),
            ]
        };
        let keys = s(&["os"]);
        for m in [Matrix::None, Matrix::Axes(&keys), Matrix::Unknown] {
            let e = validate_with(Kind::Upload, &with("r-${{ matrix }}"), m).unwrap_err();
            assert!(e.contains("holds the whole matrix"), "{e}");
        }
        // at run time the name is expanded, and said by the run
        assert!(validate_run(Kind::Upload, &with("r-${{ matrix }}")).is_ok());
    }

    #[test]
    fn a_path_is_written_only_over_directories_and_a_file() {
        use Found::*;
        let ok = |d: &str, f: &[Found]| check_path_kinds(d, f).unwrap();
        let err = |d: &str, f: &[Found]| check_path_kinds(d, f).unwrap_err();
        ok("a", &[Missing]);
        ok("a", &[File]);
        ok("out/sub/b", &[Dir, Dir, File]);
        ok("out/sub/b", &[Dir, Missing]);
        ok("out/sub/b", &[Missing]);
        ok("out/sub/b", &[Dir, Dir, Missing]);
        // a regular file partway along
        let e = err("out/sub/b", &[Dir, File]);
        assert!(e.contains("`out/sub` exists and is not a directory"), "{e}");
        let e = err("out/sub/b", &[File]);
        assert!(e.contains("`out` exists"), "{e}");
        // a link partway or at the end, a directory at the end
        assert!(err("out/sub/b", &[Dir, Other]).contains("`out/sub`"));
        assert!(err("a/b", &[Dir, Dir]).contains("`a/b` exists and is not a file"));
        assert!(err("a/b", &[Dir, Other]).contains("not replaced"));
        assert!(err("a", &[Dir]).contains("not replaced"));
        // nothing is looked at under a missing component
        ok("a/b/c", &[Dir, Missing, File]);
    }

    #[test]
    fn a_condition_that_is_always_true_is_no_condition() {
        for c in [
            "always()",
            "success()",
            "!cancelled()",
            "true",
            "  always()  ",
            "${{ always() }}",
            "${{always()}}",
            " ${{  success()  }} ",
            "${{ !cancelled() }}",
            "${{ true }}",
        ] {
            assert!(always_true(c), "{c:?}");
        }
        for c in [
            "",
            "false",
            "failure()",
            "cancelled()",
            "always() && github.ref == 'x'",
            "github.ref == 'x'",
            "${{ always() && x }}",
            "${{ failure() }}",
            "!always()",
            "success() || failure()",
            "${{ always()",
            "always() }}",
            "True",
        ] {
            assert!(!always_true(c), "{c:?}");
        }
    }

    #[test]
    fn what_an_artifact_holds_is_a_digest_of_its_files() {
        let f = |p: &str, n: u64, h: &str| (p.to_string(), n, h.to_string());
        let a = files_digest(&[f("a", 1, "x"), f("b", 2, "y")]);
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        // the order the files are listed in does not matter
        assert_eq!(a, files_digest(&[f("b", 2, "y"), f("a", 1, "x")]));
        // a path, a size, a hash, a file more or fewer does
        for other in [
            vec![f("c", 1, "x"), f("b", 2, "y")],
            vec![f("a", 3, "x"), f("b", 2, "y")],
            vec![f("a", 1, "z"), f("b", 2, "y")],
            vec![f("a", 1, "x")],
            vec![f("a", 1, "x"), f("b", 2, "y"), f("c", 0, "")],
        ] {
            assert_ne!(a, files_digest(&other));
        }
        // nothing is a digest too, and not the same as an empty file
        assert_ne!(files_digest(&[]), files_digest(&[f("a", 0, "")]));
        // fields do not run into each other
        assert_ne!(
            files_digest(&[f("a", 1, "2x")]),
            files_digest(&[f("a1", 2, "x")])
        );
    }

    #[test]
    fn the_expressions_of_a_text() {
        assert_eq!(expressions("a-${{ x }}-${{y.z}}b"), ["x", "y.z"]);
        assert_eq!(expressions("none"), Vec::<&str>::new());
        assert_eq!(expressions("a${{ x ${{ }}"), ["x ${{"]);
        assert_eq!(expressions("${{ open"), Vec::<&str>::new());
        assert_eq!(expressions("${{ a }}${{ b }}"), ["a", "b"]);
        assert_eq!(expressions("${{}}${{a}}"), ["", "a"]);
    }
}
