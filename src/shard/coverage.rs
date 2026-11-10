//! Putting together the coverage reports of the shards of one job.
//!
//! Each shard measures only the tests it ran, so no report is the coverage of
//! the job: they have to be merged, and only in a way the format defines. Two
//! formats are merged, Istanbul's `coverage-final.json` (Vitest, Jest, nyc,
//! `@cypress/code-coverage`) and LCOV (`lcov.info`, also what `cargo llvm-cov
//! --lcov` writes). In both a hit count adds up across shards, but only for the
//! same code: reports whose statements, functions, branches or lines differ for
//! one file were not made from the same source, and are a [`CoverageError`],
//! not a sum. Anything else (`llvm-cov` profile data, Cobertura, a coverage
//! report that only one tool can read) is [`CoverageError::Unsupported`]: a job
//! that collects it is not split.
//!
//! The paths of files are made relative to the root of each shard's workspace,
//! since the shards ran in different directories. Both are resolved first
//! (symbolic links, `..`), and an absolute path that is not under the root is
//! a [`CoverageError::Malformed`] report: it names a file of some other place.
//!
//! Nothing in this crate collects the shards' reports or calls [`merge`]: it is
//! a library for the caller that does (`exec` does not collect coverage, so a
//! job that collects it is not split).
//!
//! Vitest's `v8` coverage provider cannot be merged: it builds the statement
//! map of a file from the ranges that ran, so two shards that ran different
//! tests map the same file differently, and the merge stops at
//! [`CoverageError::Mismatch`]. The `istanbul` provider (and Jest, nyc,
//! `@cypress/code-coverage`) map the whole source and merge.
//!
//! The input is bounded ([`MAX_SHARDS`], [`MAX_BYTES`], [`MAX_FILES`]), since
//! the reports come from the shards.

use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// The most reports one merge takes.
pub const MAX_SHARDS: usize = 1024;
/// The most bytes of reports one merge takes, all shards together.
pub const MAX_BYTES: usize = 256 * 1024 * 1024;
/// The most files one merged report may hold.
pub const MAX_FILES: usize = 100_000;

/// A coverage format that can be merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageFormat {
    /// `coverage-final.json`.
    Istanbul,
    /// `lcov.info`.
    Lcov,
}

/// Why reports cannot be merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageError {
    /// The runner's coverage is in a format that is not merged here.
    Unsupported(String),
    /// No report was given.
    Empty,
    /// A shard's report is not a report of its format, or holds no file.
    Malformed { shard: String, why: String },
    /// Two shards measured different code under the same file name.
    Mismatch { file: String, why: String },
    /// More reports, bytes or files than a merge takes.
    TooLarge(String),
}

impl std::fmt::Display for CoverageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoverageError::Unsupported(w) => write!(f, "coverage in {w} cannot be merged"),
            CoverageError::Empty => f.write_str("there is no coverage report to merge"),
            CoverageError::Malformed { shard, why } => {
                write!(f, "the coverage report of shard {shard} is unusable: {why}")
            }
            CoverageError::Mismatch { file, why } => {
                write!(f, "the shards measured different code in `{file}`: {why}")
            }
            CoverageError::TooLarge(why) => write!(f, "the coverage reports are too large: {why}"),
        }
    }
}

/// The coverage report one shard wrote.
#[derive(Debug, Clone, Copy)]
pub struct ShardCoverage<'a> {
    pub shard_id: &'a str,
    /// The root of the workspace the shard ran in; paths under it are made
    /// relative.
    pub root: &'a str,
    pub text: &'a str,
}

/// The formats a runner's coverage can be merged in.
pub fn supports(runner: &str, format: CoverageFormat) -> bool {
    match runner {
        // `cargo llvm-cov` keeps raw profile data that only `llvm-profdata`
        // merges; its lcov export is a report of lines and merges as one
        "cargo-test" => format == CoverageFormat::Lcov,
        "vitest" | "jest" | "playwright" | "cypress" => true,
        _ => false,
    }
}

/// The format a runner's coverage `name` is in, or why it cannot be merged.
pub fn format_named(runner: &str, name: &str) -> Result<CoverageFormat, CoverageError> {
    let format = match name {
        "istanbul" | "coverage-final.json" => CoverageFormat::Istanbul,
        "lcov" | "lcov.info" => CoverageFormat::Lcov,
        _ => return Err(CoverageError::Unsupported(format!("`{name}` ({runner})"))),
    };
    if supports(runner, format) {
        Ok(format)
    } else {
        Err(CoverageError::Unsupported(format!("`{name}` ({runner})")))
    }
}

/// The reports of the shards as one report of `format`. A report is merged
/// only with reports of the same code; the result is the same whatever the
/// order of the shards.
pub fn merge(
    format: CoverageFormat,
    reports: &[ShardCoverage<'_>],
) -> Result<String, CoverageError> {
    if reports.is_empty() {
        return Err(CoverageError::Empty);
    }
    if reports.len() > MAX_SHARDS {
        return Err(CoverageError::TooLarge(format!(
            "{} reports (at most {MAX_SHARDS})",
            reports.len()
        )));
    }
    let bytes: usize = reports.iter().map(|r| r.text.len()).sum();
    if bytes > MAX_BYTES {
        return Err(CoverageError::TooLarge(format!(
            "{bytes} bytes (at most {MAX_BYTES})"
        )));
    }
    match format {
        CoverageFormat::Istanbul => merge_istanbul(reports),
        CoverageFormat::Lcov => merge_lcov(reports),
    }
}

fn malformed(r: &ShardCoverage<'_>, why: impl Into<String>) -> CoverageError {
    CoverageError::Malformed {
        shard: r.shard_id.into(),
        why: why.into(),
    }
}

fn mismatch(file: &str, why: impl Into<String>) -> CoverageError {
    CoverageError::Mismatch {
        file: file.into(),
        why: why.into(),
    }
}

/// `path` with `.` and `..` resolved by its text alone; `None` when a `..`
/// leaves the start of a relative path.
fn lexical(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// `path` with its links resolved as far as it exists (the rest by its text),
/// so that a root and a file are spelled alike (`/var` is `/private/var` on
/// macOS).
fn resolved(path: &Path) -> Option<PathBuf> {
    let path = lexical(path)?;
    let mut rest = PathBuf::new();
    let mut at = path.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            return Some(real.join(rest));
        }
        let (parent, name) = (at.parent()?, at.file_name()?);
        rest = Path::new(name).join(rest);
        at = parent;
    }
}

/// `path` relative to the root of the shard's workspace. A relative path must
/// stay inside (no `..` out of it); an absolute one must be under the root.
fn relative(r: &ShardCoverage<'_>, path: &str) -> Result<String, CoverageError> {
    let p = Path::new(path);
    let rel = if p.is_absolute() {
        let root = resolved(Path::new(r.root)).filter(|_| !r.root.is_empty());
        root.zip(resolved(p))
            .and_then(|(root, p)| p.strip_prefix(root).ok().map(Path::to_path_buf))
    } else {
        lexical(p)
    };
    rel.filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| {
            malformed(
                r,
                format!("`{path}` is not a file under the root `{}`", r.root),
            )
        })
}

// ---- Istanbul ----

/// The hit counts of one file: statements, functions, branches (a list of
/// counts for each branch).
type Counts = [BTreeMap<String, Vec<u64>>; 4];

struct IstanbulFile {
    /// The entry of the first shard that has it, with its counts replaced at
    /// the end.
    entry: Map<String, Value>,
    counts: Counts,
}

const MAPS: [(&str, &str); 3] = [("statementMap", "s"), ("fnMap", "f"), ("branchMap", "b")];

/// The truthiness counts of branches (`bT`), which only some entries have: a
/// list for each branch, like `b`.
const TRUTHY: &str = "bT";

fn count_of(v: &Value) -> Option<u64> {
    v.as_u64()
}

/// The counts of an entry, checked against its maps: every key of a map has a
/// count and the other way round.
fn istanbul_counts(entry: &Map<String, Value>) -> Result<Counts, String> {
    let mut out: Counts = Default::default();
    for (i, (map, counts)) in MAPS.iter().enumerate() {
        let m = entry
            .get(*map)
            .and_then(Value::as_object)
            .ok_or_else(|| format!("no `{map}`"))?;
        let c = entry
            .get(*counts)
            .and_then(Value::as_object)
            .ok_or_else(|| format!("no `{counts}`"))?;
        if m.keys().ne(c.keys()) {
            return Err(format!("`{map}` and `{counts}` list different keys"));
        }
        for (k, v) in c {
            let list = match v {
                Value::Array(a) => a.iter().map(count_of).collect::<Option<Vec<_>>>(),
                n => count_of(n).map(|n| vec![n]),
            };
            let list =
                list.ok_or_else(|| format!("`{counts}` has a count that is not a number"))?;
            // a statement or a function has one count
            if i < 2 && list.len() != 1 {
                return Err(format!("`{counts}` has a count that is not one number"));
            }
            out[i].insert(k.clone(), list);
        }
    }
    if let Some(t) = entry.get(TRUTHY) {
        let t = t
            .as_object()
            .ok_or_else(|| format!("`{TRUTHY}` is not an object"))?;
        if t.keys().ne(out[2].keys()) {
            return Err(format!("`{TRUTHY}` and `b` list different keys"));
        }
        for (k, v) in t {
            let list = v
                .as_array()
                .and_then(|a| a.iter().map(count_of).collect::<Option<Vec<_>>>())
                .ok_or_else(|| format!("`{TRUTHY}` has a count that is not a list of numbers"))?;
            out[3].insert(k.clone(), list);
        }
    }
    Ok(out)
}

fn merge_istanbul(reports: &[ShardCoverage<'_>]) -> Result<String, CoverageError> {
    let mut files: BTreeMap<String, IstanbulFile> = BTreeMap::new();
    for r in reports {
        let v: Value = serde_json::from_str(r.text)
            .map_err(|e| malformed(r, format!("it is not JSON: {e}")))?;
        let obj = v
            .as_object()
            .filter(|o| !o.is_empty())
            .ok_or_else(|| malformed(r, "it holds no file"))?;
        for (key, entry) in obj {
            let name = relative(r, key)?;
            let entry = entry
                .as_object()
                .ok_or_else(|| malformed(r, format!("the entry of `{name}` is not an object")))?;
            let counts =
                istanbul_counts(entry).map_err(|why| malformed(r, format!("`{name}`: {why}")))?;
            match files.get_mut(&name) {
                None => {
                    files.insert(
                        name,
                        IstanbulFile {
                            entry: entry.clone(),
                            counts,
                        },
                    );
                }
                Some(have) => {
                    for (map, _) in MAPS {
                        if have.entry.get(map) != entry.get(map) {
                            return Err(mismatch(&name, format!("`{map}` differs")));
                        }
                    }
                    if have.entry.contains_key(TRUTHY) != entry.contains_key(TRUTHY) {
                        return Err(mismatch(&name, format!("`{TRUTHY}` differs")));
                    }
                    for (into, from) in have.counts.iter_mut().zip(counts) {
                        for (k, add) in from {
                            let sum = into.get_mut(&k).expect("the maps are equal");
                            if sum.len() != add.len() {
                                return Err(mismatch(&name, "a branch has another number of arms"));
                            }
                            for (a, b) in sum.iter_mut().zip(add) {
                                *a = a.saturating_add(b);
                            }
                        }
                    }
                }
            }
        }
    }
    if files.len() > MAX_FILES {
        return Err(CoverageError::TooLarge(format!(
            "{} files (at most {MAX_FILES})",
            files.len()
        )));
    }
    let mut out = Map::new();
    for (name, mut f) in files {
        let truthy = f.entry.contains_key(TRUTHY);
        let all = MAPS.iter().map(|(_, c)| *c).chain(truthy.then_some(TRUTHY));
        for (i, counts) in all.enumerate() {
            let merged = std::mem::take(&mut f.counts[i]);
            let m: Map<String, Value> = merged
                .into_iter()
                .map(|(k, v)| {
                    let v = if i >= 2 {
                        Value::Array(
                            v.into_iter()
                                .map(|n| Value::Number(Number::from(n)))
                                .collect(),
                        )
                    } else {
                        Value::Number(Number::from(v[0]))
                    };
                    (k, v)
                })
                .collect();
            f.entry.insert(counts.to_string(), Value::Object(m));
        }
        if f.entry.contains_key("path") {
            f.entry.insert("path".into(), Value::String(name.clone()));
        }
        out.insert(name, Value::Object(f.entry));
    }
    Ok(Value::Object(out).to_string())
}

// ---- LCOV ----

#[derive(Default)]
struct Record {
    /// function name to its first line
    fns: BTreeMap<String, u64>,
    /// function name to its hits
    fnda: BTreeMap<String, u64>,
    /// line to its hits
    da: BTreeMap<u64, u64>,
    /// (line, block, branch) to its hits, `None` when it was never reached
    br: BTreeMap<(u64, String, String), Option<u64>>,
}

fn number(s: &str) -> Result<u64, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("`{s}` is not a number"))
}

fn pair(rest: &str) -> Result<(&str, &str), String> {
    rest.split_once(',')
        .ok_or_else(|| format!("`{rest}` is not a pair"))
}

/// The records of one lcov report, by file.
fn lcov_records(r: &ShardCoverage<'_>) -> Result<BTreeMap<String, Record>, CoverageError> {
    let mut out: BTreeMap<String, Record> = BTreeMap::new();
    let mut open: Option<(String, Record)> = None;
    for (n, line) in r.text.lines().enumerate() {
        let line = line.trim();
        let at = |why: String| malformed(r, format!("line {}: {why}", n + 1));
        let (tag, rest) = line.split_once(':').unwrap_or((line, ""));
        match (tag, open.as_mut()) {
            ("", _) | ("TN" | "FNF" | "FNH" | "LF" | "LH" | "BRF" | "BRH", _) => {}
            ("SF", None) => open = Some((relative(r, rest)?, Record::default())),
            ("SF", Some(_)) => return Err(at("a file starts inside another".into())),
            ("end_of_record", Some(_)) => {
                let (name, rec) = open.take().expect("matched as open");
                if out.insert(name.clone(), rec).is_some() {
                    return Err(at(format!("`{name}` is in the report twice")));
                }
            }
            (_, None) => return Err(at(format!("`{tag}` is outside a file"))),
            ("FN", Some((_, rec))) => {
                let (start, rest) = pair(rest).map_err(at)?;
                // newer lcov: `FN:start,end,name`
                let name = match rest.split_once(',') {
                    Some((end, name)) if end.trim().parse::<u64>().is_ok() => name,
                    _ => rest,
                };
                let line = number(start).map_err(at)?;
                // a function the compiler emitted twice (a generic one) is one
                // function; the same name on two lines is not
                if rec
                    .fns
                    .insert(name.to_string(), line)
                    .is_some_and(|had| had != line)
                {
                    return Err(at(format!("`{name}` is on two lines")));
                }
            }
            ("FNDA", Some((_, rec))) => {
                let (hits, name) = pair(rest).map_err(at)?;
                let hits = number(hits).map_err(at)?;
                let sum = rec.fnda.entry(name.to_string()).or_default();
                *sum = sum.saturating_add(hits);
            }
            ("DA", Some((_, rec))) => {
                let mut p = rest.split(',');
                let (line, hits) = (p.next().unwrap_or(""), p.next().unwrap_or(""));
                let hits = number(hits).map_err(at)?;
                let sum = rec.da.entry(number(line).map_err(at)?).or_default();
                *sum = sum.saturating_add(hits);
            }
            ("BRDA", Some((_, rec))) => {
                let p: Vec<&str> = rest.split(',').collect();
                let [line, block, branch, taken] = p.as_slice() else {
                    return Err(at("a branch has not four fields".into()));
                };
                let taken = match *taken {
                    "-" => None,
                    t => Some(number(t).map_err(at)?),
                };
                let key = (
                    number(line).map_err(at)?,
                    block.to_string(),
                    branch.to_string(),
                );
                let sum = rec.br.entry(key).or_insert(None);
                *sum = add_taken(*sum, taken);
            }
            _ => return Err(at(format!("`{tag}` is not a line of an lcov report"))),
        }
    }
    if open.is_some() {
        return Err(malformed(r, "a file does not end"));
    }
    if out.is_empty() {
        return Err(malformed(r, "it holds no file"));
    }
    for (name, rec) in &out {
        if let Some(f) = rec.fnda.keys().find(|f| !rec.fns.contains_key(*f)) {
            return Err(malformed(
                r,
                format!("`{name}`: `{f}` has hits but no line"),
            ));
        }
    }
    Ok(out)
}

/// The hits of a branch, `None` for one that was never reached.
fn add_taken(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

fn merge_lcov(reports: &[ShardCoverage<'_>]) -> Result<String, CoverageError> {
    let mut files: BTreeMap<String, Record> = BTreeMap::new();
    for r in reports {
        for (name, rec) in lcov_records(r)? {
            let Some(have) = files.get_mut(&name) else {
                files.insert(name, rec);
                continue;
            };
            if have.fns != rec.fns {
                return Err(mismatch(&name, "the functions differ"));
            }
            if have.da.keys().ne(rec.da.keys()) {
                return Err(mismatch(&name, "the lines differ"));
            }
            if have.br.keys().ne(rec.br.keys()) {
                return Err(mismatch(&name, "the branches differ"));
            }
            for (k, v) in rec.fnda {
                let sum = have.fnda.entry(k).or_default();
                *sum = sum.saturating_add(v);
            }
            for (k, v) in rec.da {
                let sum = have.da.get_mut(&k).expect("the lines are equal");
                *sum = sum.saturating_add(v);
            }
            for (k, v) in rec.br {
                let sum = have.br.get_mut(&k).expect("the branches are equal");
                *sum = add_taken(*sum, v);
            }
        }
    }
    if files.len() > MAX_FILES {
        return Err(CoverageError::TooLarge(format!(
            "{} files (at most {MAX_FILES})",
            files.len()
        )));
    }
    let mut out = String::new();
    for (name, rec) in &files {
        out.push_str(&format!("TN:\nSF:{name}\n"));
        for (f, line) in &rec.fns {
            out.push_str(&format!("FN:{line},{f}\n"));
        }
        for f in rec.fns.keys() {
            out.push_str(&format!(
                "FNDA:{},{f}\n",
                rec.fnda.get(f).copied().unwrap_or(0)
            ));
        }
        let hit_fns = rec
            .fns
            .keys()
            .filter(|f| rec.fnda.get(*f).copied().unwrap_or(0) > 0)
            .count();
        out.push_str(&format!("FNF:{}\nFNH:{hit_fns}\n", rec.fns.len()));
        for ((line, block, branch), taken) in &rec.br {
            let taken = taken.map_or("-".to_string(), |t| t.to_string());
            out.push_str(&format!("BRDA:{line},{block},{branch},{taken}\n"));
        }
        let hit_br = rec.br.values().filter(|t| t.unwrap_or(0) > 0).count();
        out.push_str(&format!("BRF:{}\nBRH:{hit_br}\n", rec.br.len()));
        for (line, hits) in &rec.da {
            out.push_str(&format!("DA:{line},{hits}\n"));
        }
        let hit_lines = rec.da.values().filter(|h| **h > 0).count();
        out.push_str(&format!(
            "LF:{}\nLH:{hit_lines}\nend_of_record\n",
            rec.da.len()
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep<'a>(shard: &'a str, root: &'a str, text: &'a str) -> ShardCoverage<'a> {
        ShardCoverage {
            shard_id: shard,
            root,
            text,
        }
    }

    fn istanbul(path: &str, s: &[u64], b: &[u64]) -> String {
        let stmts: Map<String, Value> = (0..s.len())
            .map(|i| (i.to_string(), Value::String("loc".into())))
            .collect();
        let sc: Map<String, Value> = s
            .iter()
            .enumerate()
            .map(|(i, n)| (i.to_string(), Value::from(*n)))
            .collect();
        let json = serde_json::json!({
            path: {
                "path": path,
                "statementMap": stmts,
                "fnMap": {"0": {"name": "f"}},
                "branchMap": {"0": {"type": "if"}},
                "s": sc,
                "f": {"0": s.first().copied().unwrap_or(0)},
                "b": {"0": b},
            }
        });
        json.to_string()
    }

    #[rustfmt::skip]
    fn malformed_of(e: &CoverageError) -> (String, String) {
        if let CoverageError::Malformed { shard, why } = e { (shard.clone(), why.clone()) } else { (String::new(), String::new()) }
    }

    #[test]
    fn names_decide_what_a_runner_may_merge() {
        use CoverageFormat::*;
        assert_eq!(format_named("vitest", "istanbul"), Ok(Istanbul));
        assert_eq!(format_named("jest", "coverage-final.json"), Ok(Istanbul));
        assert_eq!(format_named("playwright", "lcov"), Ok(Lcov));
        assert_eq!(format_named("cargo-test", "lcov.info"), Ok(Lcov));
        for (runner, name) in [
            ("cargo-test", "istanbul"),
            ("cargo-test", "profraw"),
            ("vitest", "cobertura"),
            ("mystery", "lcov"),
        ] {
            let e = format_named(runner, name).unwrap_err();
            assert!(matches!(e, CoverageError::Unsupported(_)));
            assert!(e.to_string().contains("cannot be merged"), "{e}");
        }
        assert!(supports("cypress", Istanbul) && !supports("cargo-test", Istanbul));
    }

    #[test]
    fn counts_add_up_for_the_same_code_whatever_the_order_or_the_root() {
        let a = istanbul("/w/a/src/x.js", &[1, 0, 2], &[1, 0]);
        let b = istanbul("/w/b/src/x.js", &[0, 3, 2], &[0, 4]);
        let other = istanbul("/w/b/src/y.js", &[5], &[0]);
        let ab = merge(
            CoverageFormat::Istanbul,
            &[
                rep("a", "/w/a", &a),
                rep("b", "/w/b", &b),
                rep("c", "/w/b/", &other),
            ],
        )
        .unwrap();
        let ba = merge(
            CoverageFormat::Istanbul,
            &[
                rep("c", "/w/b/", &other),
                rep("b", "/w/b", &b),
                rep("a", "/w/a", &a),
            ],
        )
        .unwrap();
        assert_eq!(ab, ba);
        let v: Value = serde_json::from_str(&ab).unwrap();
        let x = &v["src/x.js"];
        assert_eq!(x["path"], "src/x.js");
        assert_eq!(x["s"], serde_json::json!({"0": 1, "1": 3, "2": 4}));
        assert_eq!(x["f"], serde_json::json!({"0": 1}));
        assert_eq!(x["b"], serde_json::json!({"0": [1, 4]}));
        assert_eq!(v["src/y.js"]["s"], serde_json::json!({"0": 5}));
        // an entry without `path` gets none
        let mut e: Value = serde_json::from_str(&istanbul("/w/a/z.js", &[1], &[1])).unwrap();
        e["/w/a/z.js"].as_object_mut().unwrap().remove("path");
        let out = merge(
            CoverageFormat::Istanbul,
            &[rep("a", "/w/a", &e.to_string())],
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["z.js"].get("path").is_none());
    }

    #[test]
    fn a_file_outside_the_root_is_a_report_of_some_other_place() {
        let m = |root: &str, path: &str| {
            let text = istanbul(path, &[1], &[1]);
            merge(CoverageFormat::Istanbul, &[rep("a", root, &text)])
        };
        assert!(m("/w/a", "/w/a/x.js").is_ok());
        assert!(m("/w/a", "x.js").is_ok());
        assert!(m("/w/a", "./src/../x.js").is_ok());
        for (root, path) in [
            ("/w/a", "/elsewhere/z.js"),
            ("/w/a", "/w/ab/x.js"),
            ("/w/a", "/w/a/../b/x.js"),
            ("/w/a", "../x.js"),
            ("/w/a", "src/../../x.js"),
            ("/w/a", "/w/a"),
            ("/w/a", "/.."),
            ("", "/w/a/x.js"),
        ] {
            let e = m(root, path).unwrap_err();
            let (shard, why) = malformed_of(&e);
            assert_eq!(shard, "a", "{root} {path}");
            let at = format!("{root} {path}: {why}");
            assert!(at.contains("not a file under the root"));
        }
        // the same for the lcov of a shard
        let lcov = "SF:/elsewhere/x.rs\nDA:1,1\nend_of_record\n";
        let e = merge(CoverageFormat::Lcov, &[rep("a", "/w/a", lcov)]).unwrap_err();
        assert!(malformed_of(&e).1.contains("not a file under the root"));
    }

    #[test]
    fn a_root_is_spelled_as_the_system_spells_it() {
        let tmp = std::env::temp_dir().join(format!("l3-cov-{}", std::process::id()));
        let real = tmp.join("real");
        std::fs::create_dir_all(real.join("src")).unwrap();
        let link = tmp.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (link, real_s) = (
            link.to_string_lossy().into_owned(),
            std::fs::canonicalize(&real)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        for (root, file) in [
            (&link, format!("{real_s}/src/x.js")),
            (&real_s, format!("{link}/src/x.js")),
            (&link, format!("{link}/src/not-there.js")),
            (&link, format!("{link}/missing/dir/x.js")),
        ] {
            let text = istanbul(&file, &[1], &[1]);
            let out = merge(CoverageFormat::Istanbul, &[rep("a", root, &text)]).unwrap();
            let v: Value = serde_json::from_str(&out).unwrap();
            let name = v.as_object().unwrap().keys().next().unwrap().clone();
            assert!(!name.starts_with('/') && name.ends_with(".js"), "{name}");
        }
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn a_function_or_a_statement_count_that_is_not_one_number_is_malformed_not_a_panic() {
        let with = |key: &str, value: Value| {
            let mut e: Value = serde_json::from_str(&istanbul("/w/x.js", &[1], &[1])).unwrap();
            e["/w/x.js"][key] = serde_json::json!({"0": value});
            merge(CoverageFormat::Istanbul, &[rep("a", "/w", &e.to_string())])
        };
        for key in ["s", "f"] {
            for bad in [serde_json::json!([]), serde_json::json!([1, 2])] {
                let e = with(key, bad).unwrap_err();
                assert!(malformed_of(&e).1.contains("not one number"), "{key}");
            }
            assert!(with(key, serde_json::json!([4])).is_ok());
        }
    }

    #[test]
    fn the_truthiness_counts_of_branches_add_up_too() {
        let with_truthy = |path: &str, b: &[u64], t: &[u64]| {
            let mut e: Value = serde_json::from_str(&istanbul(path, &[1], b)).unwrap();
            e[path]["bT"] = serde_json::json!({"0": t});
            e.to_string()
        };
        let a = with_truthy("/w/x.js", &[1, 0], &[1, 5]);
        let b = with_truthy("/w/x.js", &[0, 4], &[0, 6]);
        let out = merge(
            CoverageFormat::Istanbul,
            &[rep("a", "/w", &a), rep("b", "/w", &b)],
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["x.js"]["bT"], serde_json::json!({"0": [1, 11]}));
        assert_eq!(v["x.js"]["b"], serde_json::json!({"0": [1, 4]}));
        // one shard with them and one without is not the same report
        let plain = istanbul("/w/x.js", &[1], &[1, 0]);
        let e = merge(
            CoverageFormat::Istanbul,
            &[rep("a", "/w", &a), rep("b", "/w", &plain)],
        )
        .unwrap_err();
        assert!(e.to_string().contains("`bT` differs"), "{e}");
        // not an object, other keys than the branches, counts that are not lists
        for bad in [
            serde_json::json!(5),
            serde_json::json!({"9": [1]}),
            serde_json::json!({"0": 1}),
            serde_json::json!({"0": ["x"]}),
        ] {
            let mut e: Value = serde_json::from_str(&istanbul("/w/x.js", &[1], &[1])).unwrap();
            e["/w/x.js"]["bT"] = bad;
            let e = merge(CoverageFormat::Istanbul, &[rep("a", "/w", &e.to_string())]).unwrap_err();
            assert!(malformed_of(&e).1.contains("bT"), "{e}");
        }
        // arms of the truthiness counts of another number
        let longer = with_truthy("/w/x.js", &[1, 0], &[1, 5, 7]);
        let e = merge(
            CoverageFormat::Istanbul,
            &[rep("a", "/w", &a), rep("b", "/w", &longer)],
        )
        .unwrap_err();
        assert!(e.to_string().contains("arms"), "{e}");
    }

    #[test]
    fn a_merge_takes_a_bounded_input() {
        let text = istanbul("/w/x.js", &[1], &[1]);
        let many: Vec<ShardCoverage<'_>> =
            (0..=MAX_SHARDS).map(|_| rep("a", "/w", &text)).collect();
        let e = merge(CoverageFormat::Istanbul, &many).unwrap_err();
        assert!(matches!(e, CoverageError::TooLarge(_)));
        assert!(e.to_string().contains("too large") && e.to_string().contains("reports"));
        assert!(merge(CoverageFormat::Istanbul, &many[..MAX_SHARDS]).is_ok());
        let big = " ".repeat(MAX_BYTES / 2 + 1);
        let e = merge(
            CoverageFormat::Lcov,
            &[rep("a", "/w", &big), rep("b", "/w", &big)],
        )
        .unwrap_err();
        assert!(e.to_string().contains("bytes"), "{e}");
        // too many files in a merged report
        let files: String = (0..=MAX_FILES)
            .map(|i| format!("SF:f{i}\nDA:1,1\nend_of_record\n"))
            .collect();
        let e = merge(CoverageFormat::Lcov, &[rep("a", "/w", &files)]).unwrap_err();
        assert!(e.to_string().contains("files"), "{e}");
        let istanbul_files: String = {
            let mut m = Map::new();
            let entry: Value = serde_json::from_str(&istanbul("x", &[1], &[1])).unwrap();
            for i in 0..=MAX_FILES {
                m.insert(format!("f{i}"), entry["x"].clone());
            }
            Value::Object(m).to_string()
        };
        let e = merge(CoverageFormat::Istanbul, &[rep("a", "/w", &istanbul_files)]).unwrap_err();
        assert!(e.to_string().contains("files"), "{e}");
    }

    #[test]
    fn lcov_duplicates_within_a_record_add_up_and_contradictions_are_malformed() {
        // a generic function is emitted once per instantiation: its lines, its
        // function and its branches repeat
        let dup = "SF:/w/x.rs
FN:1,f
FN:1,f
FNDA:2,f
FNDA:3,f
DA:1,1
DA:1,2
DA:2,0
BRDA:1,0,0,1
BRDA:1,0,0,2
BRDA:1,0,1,-
BRDA:1,0,1,-
end_of_record
";
        let out = merge(CoverageFormat::Lcov, &[rep("a", "/w", dup)]).unwrap();
        assert!(out.contains("FN:1,f\nFNDA:5,f\nFNF:1\nFNH:1\n"), "{out}");
        assert!(out.contains("BRDA:1,0,0,3\nBRDA:1,0,1,-\n"), "{out}");
        assert!(out.contains("DA:1,3\nDA:2,0\n"), "{out}");
        // the same function on two lines is not one function
        let two = "SF:/w/x.rs\nFN:1,f\nFN:2,f\nend_of_record\n";
        let e = merge(CoverageFormat::Lcov, &[rep("a", "/w", two)]).unwrap_err();
        assert!(malformed_of(&e).1.contains("on two lines"));
        // sums do not overflow
        let max = u64::MAX;
        let big = format!(
            "SF:/w/x.rs\nFN:1,f\nFNDA:{max},f\nFNDA:{max},f\nDA:1,{max}\nDA:1,{max}\nBRDA:1,0,0,{max}\nBRDA:1,0,0,{max}\nend_of_record\n"
        );
        let other = big.clone();
        let out = merge(
            CoverageFormat::Lcov,
            &[rep("a", "/w", &big), rep("b", "/w", &other)],
        )
        .unwrap();
        assert!(out.contains(&format!("FNDA:{max},f")) && out.contains(&format!("DA:1,{max}")));
        assert!(out.contains(&format!("BRDA:1,0,0,{max}")));
    }

    #[test]
    fn istanbul_reports_that_cannot_be_trusted_are_errors() {
        let good = istanbul("/w/x.js", &[1], &[1]);
        let merge1 = |text: &str| merge(CoverageFormat::Istanbul, &[rep("a", "/w", text)]);
        assert_eq!(
            merge(CoverageFormat::Istanbul, &[]),
            Err(CoverageError::Empty)
        );
        assert!(CoverageError::Empty
            .to_string()
            .contains("no coverage report"));
        for (text, why) in [
            ("not json", "not JSON"),
            ("[]", "no file"),
            ("{}", "no file"),
            (r#"{"/w/x.js": 1}"#, "not an object"),
            (r#"{"/w/x.js": {}}"#, "no `statementMap`"),
            (
                r#"{"/w/x.js": {"statementMap": {}, "s": {}, "fnMap": {}, "f": {}, "branchMap": {}}}"#,
                "no `b`",
            ),
            (
                r#"{"/w/x.js": {"statementMap": {"0": 1}, "s": {}, "fnMap": {}, "f": {}, "branchMap": {}, "b": {}}}"#,
                "list different keys",
            ),
            (
                r#"{"/w/x.js": {"statementMap": {"0": 1}, "s": {"0": "x"}, "fnMap": {}, "f": {}, "branchMap": {}, "b": {}}}"#,
                "not a number",
            ),
            (
                r#"{"/w/x.js": {"statementMap": {"0": 1}, "s": {"0": 1}, "fnMap": {}, "f": {}, "branchMap": {"0": 1}, "b": {"0": [1, "x"]}}}"#,
                "not a number",
            ),
        ] {
            let e = merge1(text).unwrap_err();
            let (shard, w) = malformed_of(&e);
            assert_eq!(shard, "a");
            assert!(w.contains(why));
            assert!(e.to_string().contains("shard a is unusable"), "{e}");
        }
        assert!(merge1(&good).is_ok());
        // the same file, other code
        let other = istanbul("/w/x.js", &[1, 1], &[1]);
        let e = merge(
            CoverageFormat::Istanbul,
            &[rep("a", "/w", &good), rep("b", "/w", &other)],
        )
        .unwrap_err();
        assert!(
            matches!(&e, CoverageError::Mismatch { file, why } if file == "x.js" && why.contains("statementMap"))
        );
        assert!(e.to_string().contains("different code in `x.js`"), "{e}");
        // the same maps, a branch with another number of arms
        let arms = istanbul("/w/x.js", &[1], &[1, 1]);
        let e = merge(
            CoverageFormat::Istanbul,
            &[rep("a", "/w", &good), rep("b", "/w", &arms)],
        )
        .unwrap_err();
        assert!(matches!(&e, CoverageError::Mismatch { why, .. } if why.contains("arms")));
    }

    const LCOV_A: &str = "TN:
SF:/w/a/src/x.rs
FN:1,f
FN:5,10,g
FNDA:1,f
FNDA:0,g
FNF:2
FNH:1
BRDA:2,0,0,1
BRDA:2,0,1,-
BRDA:3,0,0,-
BRF:3
BRH:1
DA:1,1
DA:2,0
DA:5,0,abcd
LF:3
LH:1
end_of_record
";
    const LCOV_B: &str = "SF:/w/b/src/x.rs
FN:1,f
FN:5,10,g
FNDA:2,f
FNDA:3,g
BRDA:2,0,0,-
BRDA:2,0,1,-
BRDA:3,0,0,2
DA:1,0
DA:2,4
DA:5,1
end_of_record
SF:/w/b/src/y.rs
DA:1,1
end_of_record

";

    #[test]
    fn lcov_hits_add_up_and_the_totals_are_counted_again() {
        let ab = merge(
            CoverageFormat::Lcov,
            &[rep("a", "/w/a", LCOV_A), rep("b", "/w/b", LCOV_B)],
        )
        .unwrap();
        let ba = merge(
            CoverageFormat::Lcov,
            &[rep("b", "/w/b", LCOV_B), rep("a", "/w/a", LCOV_A)],
        )
        .unwrap();
        assert_eq!(ab, ba);
        let x = ab.split("end_of_record").next().unwrap();
        assert_eq!(
            x,
            "TN:
SF:src/x.rs
FN:1,f
FN:5,g
FNDA:3,f
FNDA:3,g
FNF:2
FNH:2
BRDA:2,0,0,1
BRDA:2,0,1,-
BRDA:3,0,0,2
BRF:3
BRH:2
DA:1,1
DA:2,4
DA:5,1
LF:3
LH:3
"
        );
        assert!(ab.contains(
            "SF:src/y.rs
FNF:0
FNH:0
BRF:0
BRH:0
DA:1,1
LF:1
LH:1
"
        ));
    }

    #[test]
    fn lcov_reports_that_cannot_be_trusted_are_errors() {
        let one = |text: &str| merge(CoverageFormat::Lcov, &[rep("a", "/w", text)]);
        for (text, why) in [
            ("", "holds no file"),
            (
                "TN:
",
                "holds no file",
            ),
            (
                "SF:a
SF:b
",
                "line 2: a file starts inside another",
            ),
            (
                "end_of_record
",
                "`end_of_record` is outside a file",
            ),
            (
                "DA:1,1
",
                "`DA` is outside a file",
            ),
            (
                "SF:a
DA:1,1
",
                "does not end",
            ),
            (
                "SF:a
XX:1
end_of_record
",
                "`XX` is not a line",
            ),
            (
                "SF:a
FN:oops
end_of_record
",
                "not a pair",
            ),
            (
                "SF:a
FN:x,f
end_of_record
",
                "`x` is not a number",
            ),
            ("SF:a\nFNDA:1\nend_of_record\n", "not a pair"),
            (
                "SF:a
FNDA:x,f
end_of_record
",
                "`x` is not a number",
            ),
            (
                "SF:a
FNDA:1,f
end_of_record
",
                "`f` has hits but no line",
            ),
            (
                "SF:a
DA:1
end_of_record
",
                "`` is not a number",
            ),
            (
                "SF:a
DA:x,1
end_of_record
",
                "`x` is not a number",
            ),
            (
                "SF:a
BRDA:1,0,0
end_of_record
",
                "not four fields",
            ),
            (
                "SF:a
BRDA:1,0,0,x
end_of_record
",
                "`x` is not a number",
            ),
            (
                "SF:a
BRDA:x,0,0,1
end_of_record
",
                "`x` is not a number",
            ),
            (
                "SF:a
end_of_record
SF:a
end_of_record
",
                "`a` is in the report twice",
            ),
        ] {
            let e = one(text).unwrap_err();
            let (shard, w) = malformed_of(&e);
            assert_eq!(shard, "a");
            assert!(w.contains(why));
        }
        let a = "SF:/w/x
FN:1,f
DA:1,1
BRDA:1,0,0,1
end_of_record
";
        for (b, why) in [
            (
                "SF:/w/x
FN:2,f
DA:1,1
BRDA:1,0,0,1
end_of_record
",
                "functions",
            ),
            (
                "SF:/w/x
FN:1,f
DA:1,1
DA:2,1
BRDA:1,0,0,1
end_of_record
",
                "lines",
            ),
            (
                "SF:/w/x
FN:1,f
DA:1,1
BRDA:1,0,1,1
end_of_record
",
                "branches",
            ),
        ] {
            let e = merge(
                CoverageFormat::Lcov,
                &[rep("a", "/w", a), rep("b", "/w", b)],
            )
            .unwrap_err();
            assert!(
                matches!(&e, CoverageError::Mismatch { file, why: w } if file == "x" && w.contains(why))
            );
        }
    }
}
