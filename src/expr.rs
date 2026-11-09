//! A small evaluator for GitHub Actions expressions (`${{ ... }}` and `if:`).
//!
//! Values are JSON values. Anything the evaluator does not know (an unknown
//! context, an unknown function) is an error, never a silent `false`, so the
//! caller can report the job as unsupported instead of quietly skipping it.

use anyhow::{bail, Result};
use serde_json::{Map, Value};
use std::cell::Cell;

/// Job status the status functions (`success()`, `failure()`, ...) answer from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Success,
    Failure,
    Cancelled,
}

/// What `success()`, `failure()` and `cancelled()` answer. A step is in one of
/// the three ([`Status`]); a job may be in none (a job it needs was skipped:
/// neither `success()` nor `failure()`), and `failure()` there also covers the
/// failure of any ancestor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusFns {
    pub success: bool,
    pub failure: bool,
    pub cancelled: bool,
}

impl From<Status> for StatusFns {
    fn from(s: Status) -> Self {
        StatusFns {
            success: s == Status::Success,
            failure: s == Status::Failure,
            cancelled: s == Status::Cancelled,
        }
    }
}

/// Stands for this machine's GitHub token inside an evaluated expression.
pub const TOKEN_MARK: &str = "\u{e000}local-ci-github-token\u{e001}";

/// The environment variable that carries the token to a step's process.
pub const TOKEN_ENV: &str = "LOCAL_CI_GITHUB_TOKEN";

pub struct Ctx<'a> {
    /// Top-level contexts: github, env, matrix, steps, needs, job, runner, inputs, vars, strategy.
    pub contexts: &'a Map<String, Value>,
    pub status: StatusFns,
    /// Set when the expression touched `secrets.*` (or `github.token`) and no
    /// value could be supplied.
    pub used_secrets: Cell<bool>,
    /// Where the value of `github.token` / `secrets.GITHUB_TOKEN` comes from,
    /// when this host can supply it.
    token_from: Option<&'a crate::context::ExecContext>,
    /// Set when the token's mark was put into the expression's result.
    pub used_token: Cell<bool>,
}

impl<'a> Ctx<'a> {
    pub fn new(contexts: &'a Map<String, Value>, status: Status) -> Self {
        Self::with_fns(contexts, status.into())
    }

    pub fn with_fns(contexts: &'a Map<String, Value>, status: StatusFns) -> Self {
        Ctx {
            contexts,
            status,
            used_secrets: Cell::new(false),
            token_from: None,
            used_token: Cell::new(false),
        }
    }

    /// Supply the value of `github.token` and `secrets.GITHUB_TOKEN` from
    /// `ctx` (this machine's `gh auth token`) instead of flagging them as
    /// secrets. Other secrets stay flagged. The value itself never enters an
    /// expression's result: it is [`TOKEN_MARK`], which the caller turns into
    /// a reference to an environment variable (scripts) or the value itself
    /// (the child's environment only), so the token is never written to disk.
    pub fn with_token(mut self, ctx: &'a crate::context::ExecContext) -> Self {
        self.token_from = Some(ctx);
        self
    }

    fn token(&self) -> Option<String> {
        self.token_from?.github_token()?;
        self.used_token.set(true);
        Some(TOKEN_MARK.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Op(&'static str),
}

fn lex(src: &str) -> Result<Vec<Tok>> {
    let c: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                if i >= c.len() {
                    bail!("unterminated string in `{src}`");
                }
                if c[i] == '\'' {
                    if i + 1 < c.len() && c[i + 1] == '\'' {
                        s.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                s.push(c[i]);
                i += 1;
            }
            out.push(Tok::Str(s));
            continue;
        }
        if ch.is_ascii_digit()
            || (ch == '-'
                && i + 1 < c.len()
                && c[i + 1].is_ascii_digit()
                && prev_allows_number(&out))
        {
            let start = i;
            i += 1;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '.') {
                i += 1;
            }
            let text: String = c[start..i].iter().collect();
            let n = if let Some(hex) = text.strip_prefix("0x") {
                i64::from_str_radix(hex, 16).map(|v| v as f64).ok()
            } else {
                text.parse::<f64>().ok()
            };
            match n {
                Some(n) => out.push(Tok::Num(n)),
                None => bail!("bad number `{text}` in `{src}`"),
            }
            continue;
        }
        if ch.is_alphabetic() || ch == '_' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '-') {
                i += 1;
            }
            out.push(Tok::Ident(c[start..i].iter().collect()));
            continue;
        }
        let two: String = c[i..(i + 2).min(c.len())].iter().collect();
        let op2 = ["==", "!=", "<=", ">=", "&&", "||"]
            .into_iter()
            .find(|o| *o == two);
        if let Some(o) = op2 {
            out.push(Tok::Op(o));
            i += 2;
            continue;
        }
        let op1 = ["!", "<", ">", "(", ")", "[", "]", ".", ",", "*"]
            .into_iter()
            .find(|o| o.starts_with(ch));
        match op1 {
            Some(o) => {
                out.push(Tok::Op(o));
                i += 1;
            }
            None => bail!("unexpected `{ch}` in `{src}`"),
        }
    }
    Ok(out)
}

fn prev_allows_number(out: &[Tok]) -> bool {
    !matches!(
        out.last(),
        Some(Tok::Num(_))
            | Some(Tok::Str(_))
            | Some(Tok::Ident(_))
            | Some(Tok::Op(")"))
            | Some(Tok::Op("]"))
    )
}

#[derive(Debug, Clone)]
enum Ast {
    Lit(Value),
    Ctx(String),
    Prop(Box<Ast>, Box<Ast>),
    Star(Box<Ast>),
    Call(String, Vec<Ast>),
    Not(Box<Ast>),
    Bin(&'static str, Box<Ast>, Box<Ast>),
}

struct Parser<'s> {
    toks: Vec<Tok>,
    pos: usize,
    src: &'s str,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn eat_op(&mut self, op: &str) -> bool {
        if let Some(Tok::Op(o)) = self.peek() {
            if *o == op {
                self.pos += 1;
                return true;
            }
        }
        false
    }
    fn expect_op(&mut self, op: &str) -> Result<()> {
        if !self.eat_op(op) {
            bail!("expected `{op}` in `{}`", self.src);
        }
        Ok(())
    }
    fn binary(&mut self, level: usize) -> Result<Ast> {
        const LEVELS: [&[&str]; 4] = [&["||"], &["&&"], &["==", "!="], &["<", "<=", ">", ">="]];
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut lhs = self.binary(level + 1)?;
        loop {
            let op = match self.peek() {
                Some(Tok::Op(o)) if LEVELS[level].contains(o) => *o,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.binary(level + 1)?;
            lhs = Ast::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn unary(&mut self) -> Result<Ast> {
        if self.eat_op("!") {
            return Ok(Ast::Not(Box::new(self.unary()?)));
        }
        self.postfix()
    }
    fn postfix(&mut self) -> Result<Ast> {
        let mut e = self.primary()?;
        loop {
            if self.eat_op(".") {
                match self.toks.get(self.pos).cloned() {
                    Some(Tok::Ident(name)) => {
                        self.pos += 1;
                        e = Ast::Prop(Box::new(e), Box::new(Ast::Lit(Value::String(name))));
                    }
                    Some(Tok::Op("*")) => {
                        self.pos += 1;
                        e = Ast::Star(Box::new(e));
                    }
                    _ => bail!("expected a property name after `.` in `{}`", self.src),
                }
            } else if self.eat_op("[") {
                if self.eat_op("*") {
                    self.expect_op("]")?;
                    e = Ast::Star(Box::new(e));
                } else {
                    let idx = self.binary(0)?;
                    self.expect_op("]")?;
                    e = Ast::Prop(Box::new(e), Box::new(idx));
                }
            } else {
                break;
            }
        }
        Ok(e)
    }
    fn primary(&mut self) -> Result<Ast> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        match t {
            Some(Tok::Num(n)) => Ok(Ast::Lit(num(n))),
            Some(Tok::Str(s)) => Ok(Ast::Lit(Value::String(s))),
            Some(Tok::Ident(id)) => match id.as_str() {
                "true" => Ok(Ast::Lit(Value::Bool(true))),
                "false" => Ok(Ast::Lit(Value::Bool(false))),
                "null" => Ok(Ast::Lit(Value::Null)),
                _ => {
                    if self.eat_op("(") {
                        let mut args = Vec::new();
                        if !self.eat_op(")") {
                            loop {
                                args.push(self.binary(0)?);
                                if self.eat_op(")") {
                                    break;
                                }
                                self.expect_op(",")?;
                            }
                        }
                        Ok(Ast::Call(id.to_ascii_lowercase(), args))
                    } else {
                        Ok(Ast::Ctx(id))
                    }
                }
            },
            Some(Tok::Op("(")) => {
                let e = self.binary(0)?;
                self.expect_op(")")?;
                Ok(e)
            }
            _ => bail!("unexpected end or token in `{}`", self.src),
        }
    }
}

fn parse(src: &str) -> Result<Ast> {
    let mut p = Parser {
        toks: lex(src)?,
        pos: 0,
        src,
    };
    let e = p.binary(0)?;
    if p.pos != p.toks.len() {
        bail!("trailing tokens in `{src}`");
    }
    Ok(e)
}

fn num(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn to_num(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => *b as i32 as f64,
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else if let Some(h) = t.strip_prefix("0x") {
                i64::from_str_radix(h, 16)
                    .map(|v| v as f64)
                    .unwrap_or(f64::NAN)
            } else {
                t.parse().unwrap_or(f64::NAN)
            }
        }
        _ => f64::NAN,
    }
}

/// GitHub's loose equality: same type compares directly (strings ignore case),
/// different primitive types compare as numbers, objects/arrays by identity
/// (here: never equal unless both are the same JSON).
fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.eq_ignore_ascii_case(y),
        (Value::Array(_), _)
        | (Value::Object(_), _)
        | (_, Value::Array(_))
        | (_, Value::Object(_)) => a == b,
        _ if std::mem::discriminant(a) == std::mem::discriminant(b) => a == b,
        _ => to_num(a) == to_num(b),
    }
}

fn cmp(op: &str, a: &Value, b: &Value) -> bool {
    if let (Value::String(x), Value::String(y)) = (a, b) {
        let (x, y) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
        return match op {
            "<" => x < y,
            "<=" => x <= y,
            ">" => x > y,
            _ => x >= y,
        };
    }
    let (x, y) = (to_num(a), to_num(b));
    match op {
        "<" => x < y,
        "<=" => x <= y,
        ">" => x > y,
        _ => x >= y,
    }
}

pub fn to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn get_prop(base: &Value, key: &Value) -> Value {
    match base {
        Value::Object(m) => {
            let k = to_string(key);
            m.iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&k))
                .map(|(_, v)| v.clone())
                .unwrap_or(Value::Null)
        }
        Value::Array(a) => {
            if let Value::Number(_) = key {
                let i = to_num(key);
                if i >= 0.0 {
                    return a.get(i as usize).cloned().unwrap_or(Value::Null);
                }
                return Value::Null;
            }
            // property of a filtered array (`foo.*.bar`) maps over the elements
            Value::Array(
                a.iter()
                    .map(|e| get_prop(e, key))
                    .filter(|v| !v.is_null())
                    .collect(),
            )
        }
        _ => Value::Null,
    }
}

fn eval(ast: &Ast, ctx: &Ctx) -> Result<Value> {
    Ok(match ast {
        Ast::Lit(v) => v.clone(),
        Ast::Ctx(name) => {
            let lower = name.to_ascii_lowercase();
            if lower == "secrets" {
                ctx.used_secrets.set(true);
                return Ok(Value::Object(Map::new()));
            }
            match ctx.contexts.get(&lower) {
                Some(v) => v.clone(),
                None => bail!("unsupported context `{name}`"),
            }
        }
        Ast::Prop(b, k) => {
            let key = eval(k, ctx)?;
            let name = to_string(&key);
            // `secrets.GITHUB_TOKEN` and `github.token` are the same credential
            let secrets = matches!(b.as_ref(), Ast::Ctx(n) if n.eq_ignore_ascii_case("secrets"));
            let github = matches!(b.as_ref(), Ast::Ctx(n) if n.eq_ignore_ascii_case("github"));
            if secrets && name.eq_ignore_ascii_case("GITHUB_TOKEN") {
                if let Some(t) = ctx.token() {
                    return Ok(Value::String(t));
                }
            }
            let base = eval(b, ctx)?;
            if github && name.eq_ignore_ascii_case("token") {
                match ctx.token() {
                    Some(t) => return Ok(Value::String(t)),
                    None => ctx.used_secrets.set(true),
                }
            }
            get_prop(&base, &key)
        }
        Ast::Star(b) => match eval(b, ctx)? {
            Value::Object(m) => Value::Array(m.values().cloned().collect()),
            Value::Array(a) => Value::Array(a),
            _ => Value::Array(vec![]),
        },
        Ast::Not(e) => Value::Bool(!truthy(&eval(e, ctx)?)),
        Ast::Bin(op, l, r) => {
            let lv = eval(l, ctx)?;
            match *op {
                "&&" => {
                    if !truthy(&lv) {
                        lv
                    } else {
                        eval(r, ctx)?
                    }
                }
                "||" => {
                    if truthy(&lv) {
                        lv
                    } else {
                        eval(r, ctx)?
                    }
                }
                "==" => Value::Bool(loose_eq(&lv, &eval(r, ctx)?)),
                "!=" => Value::Bool(!loose_eq(&lv, &eval(r, ctx)?)),
                o => Value::Bool(cmp(o, &lv, &eval(r, ctx)?)),
            }
        }
        Ast::Call(name, args) => call(name, args, ctx)?,
    })
}

fn call(name: &str, args: &[Ast], ctx: &Ctx) -> Result<Value> {
    let a: Vec<Value> = args.iter().map(|e| eval(e, ctx)).collect::<Result<_>>()?;
    let s = |i: usize| a.get(i).map(to_string).unwrap_or_default();
    Ok(match name {
        "success" => Value::Bool(ctx.status.success),
        "failure" => Value::Bool(ctx.status.failure),
        "cancelled" => Value::Bool(ctx.status.cancelled),
        "always" => Value::Bool(true),
        // Cache keys only; every cache step is skipped locally.
        "hashfiles" => Value::String(String::new()),
        "contains" => match a.first() {
            Some(Value::Array(items)) => Value::Bool(
                items
                    .iter()
                    .any(|i| loose_eq(i, a.get(1).unwrap_or(&Value::Null))),
            ),
            _ => Value::Bool(s(0).to_lowercase().contains(&s(1).to_lowercase())),
        },
        "startswith" => Value::Bool(s(0).to_lowercase().starts_with(&s(1).to_lowercase())),
        "endswith" => Value::Bool(s(0).to_lowercase().ends_with(&s(1).to_lowercase())),
        "format" => {
            let mut out = s(0);
            for i in 1..a.len() {
                out = out.replace(&format!("{{{}}}", i - 1), &s(i));
            }
            Value::String(out.replace("{{", "{").replace("}}", "}"))
        }
        "join" => {
            let sep = if a.len() > 1 { s(1) } else { ",".into() };
            match a.first() {
                Some(Value::Array(items)) => {
                    Value::String(items.iter().map(to_string).collect::<Vec<_>>().join(&sep))
                }
                _ => Value::String(s(0)),
            }
        }
        "tojson" => Value::String(serde_json::to_string_pretty(
            a.first().unwrap_or(&Value::Null),
        )?),
        "fromjson" => serde_json::from_str(&s(0)).map_err(|e| anyhow::anyhow!("fromJSON: {e}"))?,
        other => bail!("unsupported function `{other}()`"),
    })
}

/// Evaluate a bare expression (the inside of `${{ }}`).
pub fn evaluate(src: &str, ctx: &Ctx) -> Result<Value> {
    eval(&parse(src)?, ctx)
}

/// Replace every `${{ expr }}` in `s`.
pub fn interpolate(s: &str, ctx: &Ctx) -> Result<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        let end =
            find_close(after).ok_or_else(|| anyhow::anyhow!("unterminated `${{{{` in `{s}`"))?;
        out.push_str(&to_string(&evaluate(&after[..end], ctx)?));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Like `interpolate`, but a string that is exactly one `${{ expr }}` keeps the
/// expression's type (needed for `strategy.matrix: ${{ fromJSON(...) }}`).
pub fn interpolate_value(s: &str, ctx: &Ctx) -> Result<Value> {
    let t = s.trim();
    if let Some(inner) = t.strip_prefix("${{").and_then(|r| r.strip_suffix("}}")) {
        if !inner.contains("${{") && find_close(inner).is_none() {
            return evaluate(inner, ctx);
        }
    }
    Ok(Value::String(interpolate(s, ctx)?))
}

/// Find the `}}` that closes an expression, skipping over quoted strings.
fn find_close(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        if in_str {
            if b[i] == b'\'' {
                if i + 1 < b.len() && b[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_str = false;
            }
        } else if b[i] == b'\'' {
            in_str = true;
        } else if b[i] == b'}' && i + 1 < b.len() && b[i + 1] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// What an expression takes from the `needs` context.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum NeedsRef {
    /// `needs.<job>.outputs.<name>`, however it is spelled.
    Output { job: String, name: String },
    /// Outputs of one job taken without naming one: `needs.<job>`,
    /// `needs.<job>.outputs`, `needs.<job>.outputs.*`, `needs.<job>.outputs[x]`.
    JobOutputs(String),
    /// `needs` taken whole or through a computed job: `toJSON(needs)`,
    /// `needs[matrix.job]`.
    All,
}

enum Seg {
    Name(String),
    Star,
    Dyn,
}

fn collect_needs(ast: &Ast, out: &mut Vec<NeedsRef>) {
    let mut segs: Vec<Seg> = Vec::new();
    let mut computed: Vec<&Ast> = Vec::new();
    let mut cur = ast;
    loop {
        match cur {
            Ast::Prop(base, key) => {
                match &**key {
                    Ast::Lit(Value::String(s)) => segs.push(Seg::Name(s.clone())),
                    other => {
                        segs.push(Seg::Dyn);
                        computed.push(other);
                    }
                }
                cur = base;
            }
            Ast::Star(base) => {
                segs.push(Seg::Star);
                cur = base;
            }
            _ => break,
        }
    }
    segs.reverse();
    match cur {
        Ast::Ctx(name) if name.eq_ignore_ascii_case("needs") => {
            let is = |s: Option<&Seg>, w: &str| matches!(s, Some(Seg::Name(n)) if n.eq_ignore_ascii_case(w));
            match segs.first() {
                None | Some(Seg::Star) | Some(Seg::Dyn) => out.push(NeedsRef::All),
                Some(Seg::Name(job)) => {
                    let job = job.clone();
                    match segs.get(1) {
                        None => out.push(NeedsRef::JobOutputs(job)),
                        Some(_) if is(segs.get(1), "outputs") => match segs.get(2) {
                            Some(Seg::Name(name)) => out.push(NeedsRef::Output {
                                job,
                                name: name.clone(),
                            }),
                            _ => out.push(NeedsRef::JobOutputs(job)),
                        },
                        // `result`, or a name computed: it could be `outputs`
                        Some(Seg::Dyn) => out.push(NeedsRef::JobOutputs(job)),
                        Some(_) => {}
                    }
                }
            }
        }
        Ast::Call(_, args) => args.iter().for_each(|a| collect_needs(a, out)),
        Ast::Not(a) => collect_needs(a, out),
        Ast::Bin(_, a, b) => {
            collect_needs(a, out);
            collect_needs(b, out);
        }
        _ => {}
    }
    for k in computed {
        collect_needs(k, out);
    }
}

/// What `text` takes from `needs`. `${{ }}` expressions in it are read; with
/// `bare`, the whole text is an expression too (an `if:`). Text that mentions
/// `needs` but cannot be read counts as taking all of it.
pub fn needs_refs(text: &str, bare: bool) -> Vec<NeedsRef> {
    let mut exprs: Vec<&str> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        let after = &rest[start + 3..];
        match find_close(after) {
            Some(end) => {
                exprs.push(&after[..end]);
                rest = &after[end + 2..];
            }
            None => {
                exprs.push(after);
                break;
            }
        }
    }
    if bare && exprs.is_empty() {
        exprs.push(text);
    }
    let mut out = Vec::new();
    for e in exprs {
        match parse(e) {
            Ok(ast) => collect_needs(&ast, &mut out),
            Err(_) if e.to_ascii_lowercase().contains("needs") => out.push(NeedsRef::All),
            Err(_) => {}
        }
    }
    out
}

/// Does an `if:` condition call a status function? If not, GitHub wraps it in `success() && (...)`.
fn has_status_fn(src: &str) -> bool {
    let l = src.to_ascii_lowercase();
    ["success(", "failure(", "always(", "cancelled("]
        .iter()
        .any(|f| l.contains(f))
}

/// Does an `if:` value leave a job to the default `success()` gate, so that it
/// is skipped when a job it needs did not succeed? False if it calls
/// `failure()`, `cancelled()` or `always()`.
pub(crate) fn success_gated(raw: Option<&str>) -> bool {
    let l = raw.unwrap_or("").to_ascii_lowercase();
    !["failure(", "cancelled(", "always("]
        .iter()
        .any(|f| l.contains(f))
}

/// Evaluate an `if:` value (with or without `${{ }}`); `None` means the default `success()`.
pub fn condition(raw: Option<&str>, ctx: &Ctx) -> Result<bool> {
    let raw = match raw {
        None => return Ok(ctx.status.success),
        Some(r) => r.trim(),
    };
    let inner = match raw.strip_prefix("${{").and_then(|r| r.strip_suffix("}}")) {
        Some(i) if find_close(i).is_none() => i.trim().to_string(),
        _ if raw.contains("${{") => interpolate(raw, ctx)?,
        _ => raw.to_string(),
    };
    if inner.is_empty() {
        return Ok(ctx.status.success);
    }
    let v = truthy(&evaluate(&inner, ctx)?);
    if has_status_fn(&inner) {
        Ok(v)
    } else {
        Ok(ctx.status.success && v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx_map() -> Map<String, Value> {
        let v = json!({
            "github": {"event_name": "pull_request", "event": {"pull_request": {"draft": false}}, "sha": "abc"},
            "matrix": {"type": "e2e", "port": 33306, "check_generated": true, "shard": "1/2"},
            "steps": {"prepare": {"outcome": "success"}},
            "env": {"node-version": "22.1.0"},
            "needs": {"list": {"outputs": {"shards": "{\"project\":[\"a\",\"b\"]}"}}}
        });
        v.as_object().unwrap().clone()
    }

    #[test]
    fn job_level_draft_check() {
        let m = ctx_map();
        let c = Ctx::new(&m, Status::Success);
        assert!(condition(Some("github.event.pull_request.draft == false"), &c).unwrap());
        assert!(condition(Some("${{ github.event_name == 'pull_request' }}"), &c).unwrap());
    }

    #[test]
    fn status_functions_and_steps() {
        let m = ctx_map();
        let failed = Ctx::new(&m, Status::Failure);
        assert!(condition(
            Some("${{ !cancelled() && steps.prepare.outcome == 'success' }}"),
            &failed
        )
        .unwrap());
        assert!(!condition(Some("matrix.type == 'e2e'"), &failed).unwrap());
        assert!(condition(Some("always()"), &failed).unwrap());
        assert!(!condition(None, &failed).unwrap());
    }

    #[test]
    fn matrix_bools_and_numbers() {
        let m = ctx_map();
        let c = Ctx::new(&m, Status::Success);
        assert!(condition(Some("${{ matrix.check_generated }}"), &c).unwrap());
        assert_eq!(
            interpolate("DB_PORT=${{ matrix.port }} x", &c).unwrap(),
            "DB_PORT=33306 x"
        );
        assert_eq!(
            interpolate("${{ env.node-version }}", &c).unwrap(),
            "22.1.0"
        );
        assert!(condition(Some("matrix.type == 'E2E'"), &c).unwrap());
    }

    #[test]
    fn from_json_keeps_type() {
        let m = ctx_map();
        let c = Ctx::new(&m, Status::Success);
        let v = interpolate_value("${{ fromJSON(needs.list.outputs.shards) }}", &c).unwrap();
        assert_eq!(v, json!({"project": ["a", "b"]}));
    }

    #[test]
    fn functions_and_operators() {
        let m = ctx_map();
        let c = Ctx::new(&m, Status::Success);
        let t = |s: &str| truthy(&evaluate(s, &c).unwrap());
        assert!(t("contains('Hello', 'ell')"));
        assert!(t("contains(fromJSON('[\"a\",\"b\"]'), 'b')"));
        assert!(t("startsWith('refs/heads/x', 'refs/')"));
        assert!(t("format('{0}-{1}', 'a', 1) == 'a-1'"));
        assert!(t("1 < 2 && '' == 0 && null == 0"));
        assert!(!t("'a' && ''"));
        assert_eq!(
            interpolate("${{ github.head_ref || 'main' }}", &c).unwrap(),
            "main"
        );
        assert_eq!(interpolate("k-${{ hashFiles('a/**') }}", &c).unwrap(), "k-");
    }

    #[test]
    fn unknown_things_are_errors() {
        let m = ctx_map();
        let c = Ctx::new(&m, Status::Success);
        assert!(evaluate("nope.x", &c).is_err());
        assert!(evaluate("frob(1)", &c).is_err());
        assert!(evaluate("1 +", &c).is_err());
    }

    #[test]
    fn secrets_are_flagged() {
        let m = ctx_map();
        let c = Ctx::new(&m, Status::Success);
        assert_eq!(interpolate("${{ secrets.TOKEN }}", &c).unwrap(), "");
        assert!(c.used_secrets.get());
        let c = Ctx::new(&m, Status::Success);
        interpolate("${{ github.token }}", &c).unwrap();
        assert!(c.used_secrets.get());
        let c = Ctx::new(&m, Status::Success);
        interpolate("${{ github.sha }}", &c).unwrap();
        assert!(!c.used_secrets.get());
    }

    #[test]
    fn strings_with_braces() {
        let m = ctx_map();
        let c = Ctx::new(&m, Status::Success);
        assert_eq!(interpolate("${{ format('{{x}}') }}", &c).unwrap(), "{x}");
    }
}
