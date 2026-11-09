//! What a `run:` script does, read from its simple commands.
//!
//! A step that would push, publish or deploy, or that installs system
//! packages, is not run here (it is left out and the run is `incomplete`).
//! That is decided from the commands the script runs, not from words it
//! happens to contain: the script is cut into simple commands (at `;`, `&&`,
//! `||`, `|`, `&` and newlines; comments and heredoc bodies are dropped), the
//! command word is found past `sudo`, `env`, `command` and the like, and its
//! subcommand decides. `echo gh pr list`, `grep -q x vercel.json` and
//! `git push --dry-run` run nothing outside this machine.

type Cmd = Vec<String>;

/// Cut `script` into simple commands, each a list of words with the quotes
/// removed. Comments, heredoc bodies and redirections are left out.
pub(crate) fn commands(script: &str) -> Vec<Cmd> {
    let mut out = Vec::new();
    collect(script, 0, &mut out);
    out
}

fn collect(script: &str, depth: usize, out: &mut Vec<Cmd>) {
    for cmd in split(script) {
        // `bash -c '...'` runs a script of its own
        if depth < 4 {
            if let Some(inner) = shell_c(&cmd) {
                collect(&inner, depth + 1, out);
            }
        }
        out.push(cmd);
    }
}

fn shell_c(cmd: &Cmd) -> Option<String> {
    let (_, rest) = strip(cmd)?;
    let (head, args) = rest.split_first()?;
    if !matches!(base(head), "bash" | "sh" | "zsh" | "dash") {
        return None;
    }
    let i = args
        .iter()
        .position(|a| a.starts_with('-') && !a.starts_with("--") && a.ends_with('c'))?;
    args.get(i + 1).cloned()
}

struct Words {
    words: Vec<(String, bool)>,
    cur: String,
    in_word: bool,
    quoted: bool,
    out: Vec<Cmd>,
}

impl Words {
    fn end_word(&mut self) {
        if self.in_word {
            self.words
                .push((std::mem::take(&mut self.cur), self.quoted));
        }
        self.in_word = false;
        self.quoted = false;
    }

    fn end_command(&mut self) {
        self.end_word();
        let words = std::mem::take(&mut self.words);
        let mut cmd = Vec::new();
        let mut it = words.into_iter();
        while let Some((w, quoted)) = it.next() {
            if !quoted && is_redirect(&w) {
                // `2>&1`, `>file` carry their target; a bare `>` takes the next word
                if w.trim_start_matches(|c: char| c.is_ascii_digit()).len() <= 2
                    && !w.contains('&')
                    && w.trim_start_matches(|c: char| c.is_ascii_digit())
                        .chars()
                        .all(|c| c == '<' || c == '>')
                {
                    it.next();
                }
                continue;
            }
            cmd.push(w);
        }
        if !cmd.is_empty() {
            self.out.push(cmd);
        }
    }
}

fn is_redirect(w: &str) -> bool {
    let t = w.trim_start_matches(|c: char| c.is_ascii_digit());
    t.starts_with('<') || t.starts_with('>')
}

fn split(script: &str) -> Vec<Cmd> {
    let chars: Vec<char> = script.chars().collect();
    let mut w = Words {
        words: vec![],
        cur: String::new(),
        in_word: false,
        quoted: false,
        out: vec![],
    };
    // delimiters of heredocs whose bodies start after the current line
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                match chars.get(i + 1) {
                    // a line continuation
                    Some('\n') => {}
                    Some(&n) => {
                        w.cur.push(n);
                        w.in_word = true;
                    }
                    None => {}
                }
                i += 2;
                continue;
            }
            '\'' => {
                w.in_word = true;
                w.quoted = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    w.cur.push(chars[i]);
                    i += 1;
                }
                i += 1;
                continue;
            }
            '"' => {
                w.in_word = true;
                w.quoted = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    w.cur.push(chars[i]);
                    i += 1;
                }
                i += 1;
                continue;
            }
            '#' if !w.in_word => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '\n' => {
                w.end_command();
                i += 1;
                // heredoc bodies start on the next line
                for (delim, strip_tabs) in std::mem::take(&mut heredocs) {
                    while i < chars.len() {
                        let end = chars[i..]
                            .iter()
                            .position(|&c| c == '\n')
                            .map_or(chars.len(), |p| i + p);
                        let line: String = chars[i..end].iter().collect();
                        i = (end + 1).min(chars.len());
                        let line = if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            line.as_str()
                        };
                        if line.trim_end() == delim {
                            break;
                        }
                    }
                }
                continue;
            }
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                // `<<EOF`, `<<-EOF`, `<<'EOF'`
                let mut j = i + 2;
                let strip_tabs = chars.get(j) == Some(&'-');
                if strip_tabs {
                    j += 1;
                }
                while chars.get(j).is_some_and(|c| *c == ' ' || *c == '\t') {
                    j += 1;
                }
                let mut delim = String::new();
                while let Some(&d) = chars.get(j) {
                    if d.is_whitespace() || matches!(d, ';' | '&' | '|' | '(' | ')') {
                        break;
                    }
                    if !matches!(d, '\'' | '"' | '\\') {
                        delim.push(d);
                    }
                    j += 1;
                }
                if !delim.is_empty() {
                    heredocs.push((delim, strip_tabs));
                }
                w.end_word();
                i = j;
                continue;
            }
            ';' | '(' | ')' | '`' => {
                w.end_command();
            }
            '|' => {
                w.end_command();
                if matches!(chars.get(i + 1), Some('|') | Some('&')) {
                    i += 1;
                }
            }
            '&' => {
                // `2>&1`, `>&2`, `&>file` are redirections, not a separator
                let redirect =
                    (w.in_word && w.cur.ends_with(['>', '<'])) || chars.get(i + 1) == Some(&'>');
                if redirect {
                    w.cur.push('&');
                    w.in_word = true;
                } else {
                    w.end_command();
                    if chars.get(i + 1) == Some(&'&') {
                        i += 1;
                    }
                }
            }
            c if c.is_whitespace() => w.end_word(),
            c => {
                w.cur.push(c);
                w.in_word = true;
            }
        }
        i += 1;
    }
    w.end_command();
    w.out
}

fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((k, _)) => {
            !k.is_empty()
                && !k.starts_with(|c: char| c.is_ascii_digit())
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "do", "while", "until", "!", "{", "}", "time",
];

/// Past keywords, assignments and wrappers (`env`, `command`, `sudo`, ...):
/// whether `sudo` was among them and the command that is run. `None` for a
/// command that only looks something up (`command -v x`) or has nothing to run.
fn strip(cmd: &[String]) -> Option<(bool, &[String])> {
    let mut sudo = false;
    let mut i = 0;
    loop {
        let w = cmd.get(i)?.as_str();
        if KEYWORDS.contains(&w) || is_assignment(w) {
            i += 1;
            continue;
        }
        match base(w) {
            "sudo" => {
                sudo = true;
                i += 1;
                while let Some(a) = cmd.get(i) {
                    if matches!(
                        a.as_str(),
                        "-u" | "-g" | "-C" | "-p" | "-h" | "-D" | "-r" | "-t" | "-U"
                    ) {
                        i += 2;
                    } else if a.starts_with('-') || is_assignment(a) {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            "env" => {
                i += 1;
                while let Some(a) = cmd.get(i) {
                    if matches!(a.as_str(), "-u" | "-C" | "-S") {
                        i += 2;
                    } else if a.starts_with('-') || is_assignment(a) {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            "command" => {
                i += 1;
                while let Some(a) = cmd.get(i) {
                    match a.as_str() {
                        "-v" | "-V" => return None,
                        "-p" | "--" => i += 1,
                        _ => break,
                    }
                }
            }
            "exec" | "nohup" | "builtin" | "setsid" | "xargs" | "stdbuf" => {
                i += 1;
                while cmd.get(i).is_some_and(|a| a.starts_with('-')) {
                    i += 1;
                }
            }
            "nice" => {
                i += 1;
                while let Some(a) = cmd.get(i) {
                    if a == "-n" {
                        i += 2;
                    } else if a.starts_with('-') {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            "timeout" => {
                i += 1;
                while cmd.get(i).is_some_and(|a| a.starts_with('-')) {
                    i += 1;
                }
                i += 1; // the duration
            }
            "npx" | "bunx" => i = skip_runner_options(cmd, i + 1),
            tool @ ("npm" | "pnpm" | "yarn" | "bun") if runner_sub(cmd, i, tool).is_some() => {
                i = skip_runner_options(cmd, runner_sub(cmd, i, tool).unwrap_or(i + 1));
            }
            _ => return Some((sudo, &cmd[i..])),
        }
    }
}

/// For `pnpm dlx x`, `pnpm exec x`, `yarn dlx x`, `yarn exec x`, `npm exec x`
/// and `bun x x`: the index after the runner subcommand, past options before it
/// (`pnpm --filter pkg exec x`).
fn runner_sub(cmd: &[String], i: usize, tool: &str) -> Option<usize> {
    let subs: &[&str] = match tool {
        "npm" => &["exec", "x"],
        "pnpm" | "yarn" => &["dlx", "exec"],
        "bun" => &["x"],
        _ => return None,
    };
    let mut j = i + 1;
    loop {
        let a = cmd.get(j)?.as_str();
        if a.starts_with('-') {
            j += if matches!(
                a,
                "--filter" | "-F" | "--prefix" | "-C" | "--cwd" | "--dir" | "--workspace" | "-w"
            ) {
                2
            } else {
                1
            };
        } else {
            return subs.contains(&a).then_some(j + 1);
        }
    }
}

/// Past the options of a package runner (`-y`, `-p pkg`, `--`) to the command.
fn skip_runner_options(cmd: &[String], mut j: usize) -> usize {
    while let Some(a) = cmd.get(j).map(String::as_str) {
        if !a.starts_with('-') {
            break;
        }
        j += if matches!(
            a,
            "-p" | "--package" | "--registry" | "--cache" | "--node-options" | "-c"
        ) {
            2
        } else {
            1
        };
        if a == "--" {
            break;
        }
    }
    j
}

/// The words of `args` that are not options, skipping the values of the
/// options named in `valued`.
fn positionals<'a>(args: &'a [String], valued: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if valued.contains(&a) {
            i += 2;
            continue;
        }
        if !a.starts_with('-') || a == "-" {
            out.push(a);
        }
        i += 1;
    }
    out
}

fn has_flag(args: &[String], names: &[&str]) -> bool {
    args.iter().any(|a| {
        names
            .iter()
            .any(|n| a == n || a.starts_with(&format!("{n}=")))
    })
}

/// A `-n` among short options (`-n`, `-nu`).
fn short_n(args: &[String]) -> bool {
    args.iter()
        .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('n'))
}

const AWS_VALUED: &[&str] = &[
    "--profile",
    "--region",
    "--endpoint-url",
    "--output",
    "--query",
    "--cli-read-timeout",
    "--cli-connect-timeout",
    "--ca-bundle",
    "--color",
];

fn aws_remote(args: &[String]) -> bool {
    let pos = positionals(args, AWS_VALUED);
    let (Some(service), op) = (pos.first().copied(), pos.get(1).copied().unwrap_or("")) else {
        return false;
    };
    if has_flag(args, &["--dryrun", "--dry-run"]) {
        return false;
    }
    match service {
        "s3" => match op {
            "cp" | "sync" => {
                // writing to a bucket; a download is a read
                pos.get(3).is_some_and(|d| d.starts_with("s3://"))
            }
            "mv" | "rm" | "mb" | "rb" => true,
            _ => false,
        },
        "deploy" => op != "help",
        "configure" | "help" | "sts" => false,
        _ => {
            const READ: &[&str] = &[
                "describe-",
                "get-",
                "list-",
                "head-",
                "batch-get-",
                "lookup-",
                "search-",
                "scan",
                "query",
                "filter-",
                "validate-",
                "estimate-",
                "check-",
                "preview-",
                "select-",
            ];
            if op.is_empty()
                || op == "help"
                || READ.iter().any(|p| op.starts_with(p))
                || args.iter().any(|a| a == "--version")
            {
                return false;
            }
            const WRITE: &[&str] = &[
                "create",
                "delete",
                "update",
                "put",
                "start",
                "stop",
                "run",
                "invoke",
                "terminate",
                "modify",
                "attach",
                "detach",
                "publish",
                "send",
                "tag",
                "untag",
                "register",
                "deregister",
                "upload",
                "deploy",
                "remove",
                "add",
                "set",
                "reboot",
                "restore",
                "release",
                "associate",
                "disassociate",
                "enable",
                "disable",
                "execute",
                "import",
                "copy",
                "purge",
                "cancel",
                "reset",
                "rotate",
                "revoke",
                "authorize",
                "grant",
                "apply",
                "replace",
                "change",
                "write",
                "post",
                "commit",
                "promote",
                "rollback",
            ];
            WRITE.iter().any(|p| op.starts_with(p))
        }
    }
}

fn gh_remote(args: &[String]) -> bool {
    let pos = positionals(args, &["-R", "--repo", "--hostname"]);
    let (Some(group), sub) = (pos.first().copied(), pos.get(1).copied().unwrap_or("")) else {
        return false;
    };
    match group {
        "api" => {
            let method = args
                .iter()
                .enumerate()
                .find_map(|(i, a)| {
                    if a == "-X" || a == "--method" {
                        args.get(i + 1).cloned()
                    } else if let Some(m) = a.strip_prefix("--method=") {
                        Some(m.to_string())
                    } else {
                        a.strip_prefix("-X")
                            .filter(|m| !m.is_empty())
                            .map(String::from)
                    }
                })
                .map(|m| m.to_ascii_uppercase());
            match method {
                Some(m) => matches!(m.as_str(), "POST" | "PATCH" | "PUT" | "DELETE"),
                // fields and an input make it a POST
                None => has_flag(args, &["-f", "-F", "--field", "--raw-field", "--input"]),
            }
        }
        "pr" => matches!(
            sub,
            "create" | "merge" | "comment" | "edit" | "close" | "reopen" | "review" | "ready"
        ),
        "issue" => matches!(
            sub,
            "create" | "comment" | "edit" | "close" | "reopen" | "delete" | "transfer" | "lock"
        ),
        "release" => matches!(
            sub,
            "create" | "upload" | "edit" | "delete" | "delete-asset"
        ),
        "workflow" => matches!(sub, "run" | "enable" | "disable"),
        "run" => matches!(sub, "rerun" | "cancel" | "delete"),
        "repo" => matches!(
            sub,
            "create" | "delete" | "edit" | "fork" | "rename" | "archive" | "unarchive"
        ),
        "secret" | "variable" => matches!(sub, "set" | "delete" | "remove"),
        "label" => matches!(sub, "create" | "edit" | "delete" | "clone"),
        "gist" => matches!(sub, "create" | "edit" | "delete" | "rename"),
        _ => false,
    }
}

fn kubectl_remote(args: &[String]) -> bool {
    if has_flag(args, &["--dry-run"]) {
        return false;
    }
    let pos = positionals(
        args,
        &[
            "-n",
            "--namespace",
            "--context",
            "--kubeconfig",
            "-s",
            "--server",
        ],
    );
    match pos.first().copied() {
        Some("rollout") => !matches!(pos.get(1).copied(), Some("status" | "history")),
        Some(
            "apply" | "delete" | "create" | "replace" | "patch" | "scale" | "set" | "annotate"
            | "label" | "edit" | "drain" | "cordon" | "uncordon" | "taint" | "run" | "expose"
            | "autoscale" | "cp" | "exec",
        ) => true,
        _ => false,
    }
}

fn git_remote(args: &[String]) -> bool {
    let pos = positionals(args, &["-C", "-c", "--git-dir", "--work-tree"]);
    if pos.first().copied() != Some("push") {
        return false;
    }
    let after: &[String] = match args.iter().position(|a| a == "push") {
        Some(i) => &args[i + 1..],
        None => &[],
    };
    !(has_flag(after, &["--dry-run"]) || short_n(after))
}

const DOCKER_VALUED: &[&str] = &[
    "--context",
    "-c",
    "-H",
    "--host",
    "--config",
    "-l",
    "--log-level",
    "-f",
    "--file",
    "-p",
    "--project-name",
    "--profile",
    "--env-file",
    "--project-directory",
];

fn docker_remote(head: &str, args: &[String]) -> bool {
    let pos = positionals(args, DOCKER_VALUED);
    let (first, second) = (
        pos.first().copied().unwrap_or(""),
        pos.get(1).copied().unwrap_or(""),
    );
    let pushes = has_flag(args, &["--push"])
        || args
            .iter()
            .any(|a| a.contains("type=registry") || a.contains("push=true"));
    // `docker-compose push` has no `compose` word
    let (first, second) = if base(head) == "docker-compose" {
        ("compose", first)
    } else {
        (first, second)
    };
    match first {
        "push" | "login" => true,
        "image" | "manifest" => second == "push",
        "compose" => second == "push",
        "buildx" => {
            matches!(second, "build" | "bake" | "b") && pushes
                || (second == "imagetools" && pos.get(2).copied() == Some("create"))
        }
        "build" | "bake" => pushes,
        _ => false,
    }
}

/// Short options of `curl` that take a value, so the rest of the cluster (or
/// the next word) is theirs.
const CURL_SHORT_VALUED: &str = "oHuAebcDEKwxyYzUrCmQ";

fn write_method(m: &str) -> bool {
    matches!(
        m.to_ascii_uppercase().as_str(),
        "POST" | "PUT" | "PATCH" | "DELETE"
    )
}

fn curl_remote(args: &[String]) -> bool {
    let get = has_flag(args, &["-G", "--get"]);
    let mut body = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            match name {
                "request" => {
                    let v = inline.or_else(|| args.get(i + 1).map(String::as_str));
                    if v.is_some_and(write_method) {
                        return true;
                    }
                }
                "data" | "data-raw" | "data-binary" | "data-urlencode" | "data-ascii" | "form"
                | "form-string" | "upload-file" | "json" => body = true,
                _ => {}
            }
        } else if a.starts_with('-') && a.len() > 1 {
            let cluster: Vec<char> = a[1..].chars().collect();
            let mut k = 0;
            while k < cluster.len() {
                match cluster[k] {
                    'X' => {
                        let rest: String = cluster[k + 1..].iter().collect();
                        let v = if rest.is_empty() {
                            args.get(i + 1).cloned().unwrap_or_default()
                        } else {
                            rest
                        };
                        if write_method(&v) {
                            return true;
                        }
                        break;
                    }
                    'd' | 'F' | 'T' => {
                        body = true;
                        break;
                    }
                    c if CURL_SHORT_VALUED.contains(c) => break,
                    _ => {}
                }
                k += 1;
            }
        }
        i += 1;
    }
    // `-G` turns `-d` into a query string
    body && !get
}

fn wget_remote(args: &[String]) -> bool {
    has_flag(
        args,
        &["--post-data", "--post-file", "--body-data", "--body-file"],
    ) || args.iter().enumerate().any(|(i, a)| {
        let v = if a == "--method" {
            args.get(i + 1).map(String::as_str)
        } else {
            a.strip_prefix("--method=")
        };
        v.is_some_and(write_method)
    })
}

fn is_remote(sudo_less: &[String]) -> bool {
    let Some((head, args)) = sudo_less.split_first() else {
        return false;
    };
    let pos = positionals(args, &[]);
    let first = pos.first().copied().unwrap_or("");
    let second = pos.get(1).copied().unwrap_or("");
    let dry = has_flag(args, &["--dry-run", "--dryrun", "-n"]);
    match base(head) {
        "git" => git_remote(args),
        "gh" => gh_remote(args),
        "aws" => aws_remote(args),
        "kubectl" | "oc" => kubectl_remote(args),
        // options take values (`--filter pkg`, `--prefix a`, `yarn npm`,
        // `yarn workspace x`): look for the word anywhere, not at one place
        "npm" | "pnpm" | "yarn" | "bun" => {
            args.iter()
                .any(|a| matches!(a.as_str(), "publish" | "unpublish" | "deprecate"))
                && !dry
        }
        "cargo" => first == "publish" && !dry,
        "dart" | "flutter" => first == "pub" && second == "publish" && !dry,
        "twine" => first == "upload",
        "docker" | "docker-compose" | "podman" | "buildah" => docker_remote(head, args),
        "curl" => curl_remote(args),
        "wget" => wget_remote(args),
        "helm" => matches!(first, "install" | "upgrade" | "uninstall" | "rollback") && !dry,
        "terraform" | "tofu" => matches!(first, "apply" | "destroy"),
        "serverless" | "sls" => matches!(first, "deploy" | "remove"),
        "firebase" => first == "deploy",
        "wrangler" => {
            (matches!(
                first,
                "deploy" | "publish" | "secret" | "delete" | "rollback"
            ) || (first == "pages" && matches!(second, "deploy" | "publish")))
                && !has_flag(args, &["--dry-run"])
        }
        "vercel" => {
            if has_flag(args, &["--version", "-v", "--help", "-h"]) {
                return false;
            }
            first.is_empty()
                || first.starts_with(['.', '/', '~'])
                || matches!(
                    first,
                    "deploy" | "promote" | "rollback" | "remove" | "rm" | "alias"
                )
        }
        "netlify" => first == "deploy",
        "gcloud" => pos.iter().any(|w| {
            matches!(
                *w,
                "deploy"
                    | "create"
                    | "delete"
                    | "update"
                    | "submit"
                    | "set"
                    | "add"
                    | "remove"
                    | "cp"
                    | "rsync"
                    | "mv"
                    | "rm"
                    | "import"
                    | "export"
            )
        }),
        _ => false,
    }
}

/// Whether the script runs something that changes the world outside this
/// machine: a push, a publish, a deploy, a write through `gh`, `aws` or
/// `kubectl`. Reading is not: `git push --dry-run`, `aws sts get-caller-identity`,
/// `gh pr list` and `kubectl get` run here.
pub(crate) fn remote_effect(script: &str) -> bool {
    commands(script)
        .iter()
        .any(|c| strip(c).is_some_and(|(_, rest)| is_remote(rest)))
}

/// Whether a reference to the GitHub token, which becomes `${LOCAL_CI_GITHUB_TOKEN}`
/// in the script, could end up as that literal text instead of the value: in
/// single quotes, in `$'...'`, after a backslash, or in the body of a quoted
/// heredoc, the shell does not expand it. Also true when a part of a mark is
/// left (an expression such as `substring()` cut one), and when the quoting
/// cannot be followed to its end. Such a step is not run.
pub(crate) fn token_may_be_literal(script: &str, mark: &str) -> bool {
    if script.replace(mark, "").contains(['\u{e000}', '\u{e001}']) {
        return true;
    }
    if !script.contains(mark) {
        return false;
    }
    let chars: Vec<char> = script.chars().collect();
    let m: Vec<char> = mark.chars().collect();
    let n = chars.len();
    let at_mark = |i: usize| chars[i..].starts_with(&m);
    enum Ctx {
        Dq,
        Sub(usize),
    }
    let mut stack: Vec<Ctx> = Vec::new();
    // heredocs whose bodies start on the next line: (delimiter, `<<-`, quoted)
    let mut heredocs: Vec<(String, bool, bool)> = Vec::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if at_mark(i) {
            i += m.len();
            continue;
        }
        if matches!(stack.last(), Some(Ctx::Dq)) {
            match c {
                '\\' => {
                    if i + 1 < n && at_mark(i + 1) {
                        return true;
                    }
                    i += 2;
                }
                '"' => {
                    stack.pop();
                    i += 1;
                }
                '$' if chars.get(i + 1) == Some(&'(') => {
                    stack.push(Ctx::Sub(1));
                    i += 2;
                }
                _ => i += 1,
            }
            continue;
        }
        let ansi = c == '$' && chars.get(i + 1) == Some(&'\'');
        match c {
            '\\' => {
                if i + 1 < n && at_mark(i + 1) {
                    return true;
                }
                i += 2;
            }
            '\'' | '$' if c == '\'' || ansi => {
                let mut j = if ansi { i + 2 } else { i + 1 };
                loop {
                    if j >= n {
                        return true;
                    }
                    if at_mark(j) {
                        return true;
                    }
                    if ansi && chars[j] == '\\' {
                        j += 2;
                        continue;
                    }
                    if chars[j] == '\'' {
                        break;
                    }
                    j += 1;
                }
                i = j + 1;
            }
            '"' => {
                stack.push(Ctx::Dq);
                i += 1;
            }
            '#' if i == 0 || chars[i - 1].is_whitespace() || ";&|(".contains(chars[i - 1]) => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '(' => {
                if let Some(Ctx::Sub(d)) = stack.last_mut() {
                    *d += 1;
                }
                i += 1;
            }
            ')' => {
                if let Some(Ctx::Sub(d)) = stack.last_mut() {
                    *d -= 1;
                    if *d == 0 {
                        stack.pop();
                    }
                }
                i += 1;
            }
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                let mut j = i + 2;
                let strip_tabs = chars.get(j) == Some(&'-');
                if strip_tabs {
                    j += 1;
                }
                while chars.get(j).is_some_and(|c| *c == ' ' || *c == '\t') {
                    j += 1;
                }
                let (mut delim, mut quoted) = (String::new(), false);
                while let Some(&d) = chars.get(j) {
                    if d.is_whitespace() || matches!(d, ';' | '&' | '|' | '(' | ')') {
                        break;
                    }
                    if matches!(d, '\'' | '"' | '\\') {
                        quoted = true;
                    } else {
                        delim.push(d);
                    }
                    j += 1;
                }
                if !delim.is_empty() {
                    heredocs.push((delim, strip_tabs, quoted));
                }
                i = j;
            }
            '\n' => {
                i += 1;
                for (delim, strip_tabs, quoted) in std::mem::take(&mut heredocs) {
                    while i < n {
                        let end = chars[i..]
                            .iter()
                            .position(|&c| c == '\n')
                            .map_or(n, |p| i + p);
                        let line: String = chars[i..end].iter().collect();
                        i = (end + 1).min(n);
                        if quoted && line.contains(mark) {
                            return true;
                        }
                        let t = if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            line.as_str()
                        };
                        if t.trim_end() == delim {
                            break;
                        }
                    }
                }
            }
            _ => i += 1,
        }
    }
    // a quote that never closes: the reading above cannot be trusted
    !stack.is_empty()
}

/// What a script wants of the system's packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Install {
    /// Nothing.
    No,
    /// `brew install` of formulae already on the path: running it changes
    /// nothing, so the step runs.
    Present,
    /// Installs or changes system packages; not done here.
    Needed,
}

/// Commands run as root that change the system.
const SUDO_MUTATES: &[&str] = &[
    "apt",
    "apt-get",
    "aptitude",
    "yum",
    "dnf",
    "apk",
    "snap",
    "brew",
    "choco",
    "pacman",
    "zypper",
    "dpkg",
    "rpm",
    "pip",
    "pip3",
    "npm",
    "gem",
    "cargo",
    "make",
    "cp",
    "mv",
    "install",
    "ln",
    "tee",
    "rm",
    "chmod",
    "chown",
    "mkdir",
    "systemctl",
    "service",
    "sysctl",
    "ufw",
    "tar",
    "curl",
    "wget",
    "sh",
    "bash",
    "useradd",
    "usermod",
    "mount",
    "umount",
    "update-alternatives",
];

fn pkg_installs(head: &str, args: &[String]) -> bool {
    let first = positionals(args, &[]).first().copied().unwrap_or("");
    match head {
        "apt" | "apt-get" | "aptitude" | "yum" | "dnf" | "zypper" => matches!(
            first,
            "install"
                | "update"
                | "upgrade"
                | "dist-upgrade"
                | "full-upgrade"
                | "remove"
                | "purge"
                | "autoremove"
                | "reinstall"
                | "localinstall"
                | "groupinstall"
                | "downgrade"
        ),
        "apk" => matches!(first, "add" | "del" | "upgrade" | "update"),
        "snap" | "choco" => matches!(first, "install" | "upgrade" | "remove" | "refresh"),
        "pacman" => args
            .iter()
            .any(|a| a.starts_with("-S") || a.starts_with("-R")),
        "dpkg" | "rpm" => args
            .iter()
            .any(|a| matches!(a.as_str(), "-i" | "-U" | "-e" | "-ivh")),
        _ => false,
    }
}

/// Whether the script installs system packages. `have` says whether a command
/// is on the path, for `brew install` of something that is there already.
pub(crate) fn global_install(script: &str, have: &dyn Fn(&str) -> bool) -> Install {
    let mut found = Install::No;
    for c in commands(script) {
        let Some((sudo, rest)) = strip(&c) else {
            continue;
        };
        let Some((head, args)) = rest.split_first() else {
            continue;
        };
        let head = base(head);
        if head == "brew" {
            let first = positionals(args, &[]).first().copied().unwrap_or("");
            match first {
                "install" | "reinstall" => {
                    let names: Vec<&str> = positionals(args, &[]).into_iter().skip(1).collect();
                    let cask = has_flag(args, &["--cask", "--casks"]);
                    let present = !cask
                        && !names.is_empty()
                        && names
                            .iter()
                            .all(|n| have(n.rsplit('/').next().unwrap_or(n)))
                        && first == "install";
                    if present {
                        found = Install::Present;
                    } else {
                        return Install::Needed;
                    }
                }
                "upgrade" | "tap" | "untap" | "uninstall" | "update" => return Install::Needed,
                _ => {}
            }
        } else if pkg_installs(head, args) || (sudo && SUDO_MUTATES.contains(&head)) {
            return Install::Needed;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn false_positives_are_not_remote_effects() {
        for s in [
            "git push --dry-run origin HEAD",
            "git push -n origin HEAD",
            "echo using aws sdk",
            "grep -q x vercel.json",
            "kubectl kustomize .",
            "echo gh pr list",
            "sudo -n true",
            "# git push origin main\necho hi",
            "cat <<EOF\ngit push origin main\nEOF\necho done",
            "git diff --exit-code",
            "npm run lint",
            "echo pushing",
            "npm publish --dry-run",
            "gh pr list --state open",
            "gh pr view 1",
            "gh api repos/x/y",
            "gh api -X GET repos/x/y",
            "aws --version",
            "aws sts get-caller-identity",
            "aws ec2 describe-instances --region us-east-1",
            "aws s3 ls s3://b",
            "aws s3 cp s3://b/f .",
            "aws s3 sync s3://b/dir out",
            "kubectl get pods",
            "kubectl rollout status deploy/x",
            "kubectl apply -f x.yaml --dry-run=client",
            "vercel --version",
            "vercel build",
            "command -v kubectl",
            "FOO=1 echo vercel",
            "git status && git log -1",
            "git push -n",
            "aws s3 ls",
            "curl https://example.test",
            "curl -I https://example.test",
            "curl -sSL -o out.tgz https://example.test/x.tgz",
            "curl -G -d q=1 https://example.test",
            "curl -X GET https://example.test",
            "wget https://example.test/x",
            "wget -q -O - https://example.test",
            "npx --yes prettier --check .",
            "pnpm exec eslint .",
            "pnpm dlx create-thing",
            "yarn dlx tsc",
            "npm exec -- tsc -p .",
            "npm --prefix a run build",
            "pnpm --filter pkg test",
            "yarn workspace x build",
            "docker build -t x .",
            "docker image ls",
            "docker compose up -d",
            "docker buildx build -t x .",
            "wrangler deploy --dry-run",
            "vercel pull --yes",
            "vercel ls",
            "npx publish-check",
        ] {
            assert!(!remote_effect(s), "{s:?}");
        }
    }

    #[test]
    fn remote_effects_are_found() {
        for s in [
            "git push origin main",
            "git -C sub push",
            "npm publish",
            "pnpm publish --access public",
            "gh pr merge 3 --squash",
            "gh pr create --fill",
            "gh pr comment 3 -b hi",
            "gh release create v1",
            "gh api -X POST repos/x/y/issues",
            "gh api --method=DELETE repos/x/y",
            "gh api repos/x/y -f a=b",
            "aws s3 sync dist s3://bucket",
            "aws s3 cp a.txt s3://bucket/a.txt",
            "aws s3 rm s3://bucket/a --recursive",
            "aws deploy create-deployment --application-name a",
            "aws cloudformation deploy --template-file t.yml --stack-name s",
            "aws lambda update-function-code --function-name f",
            "kubectl apply -f x.yaml",
            "kubectl delete pod x",
            "kubectl rollout restart deploy/x",
            "vercel --prod",
            "vercel deploy",
            "netlify deploy --prod",
            "echo hi\ncd out && git push origin HEAD:main",
            "if true; then git push; fi",
            "sudo git push",
            "env GIT_TERMINAL_PROMPT=0 git push origin x",
            "bash -c 'git push origin main'",
            "terraform apply -auto-approve",
            "cargo publish",
            "docker push img:1",
            "true | gh pr merge 1",
            "npx wrangler deploy",
            "npx --yes vercel --prod",
            "bunx netlify deploy --prod",
            "pnpm dlx wrangler pages deploy dist",
            "pnpm exec npm publish",
            "yarn dlx vercel deploy",
            "yarn exec -- npm publish",
            "npm exec -y -- wrangler deploy",
            "npm exec --package=x -- git push",
            "yarn npm publish",
            "yarn workspace x publish",
            "pnpm --filter pkg publish",
            "pnpm -r publish --no-git-checks",
            "npm --prefix a publish",
            "bun publish",
            "docker image push img:1",
            "docker buildx build --push -t x .",
            "docker buildx bake --push",
            "docker compose push",
            "docker compose -f ci.yml push",
            "docker-compose push",
            "docker manifest push img:1",
            "docker buildx imagetools create -t a b",
            "wrangler deploy",
            "wrangler pages deploy dist",
            "vercel",
            "vercel ./app",
            "netlify deploy",
            "curl -X POST https://example.test",
            "curl -XPUT https://example.test",
            "curl --request DELETE https://example.test",
            "curl --request=patch https://example.test",
            "curl -d a=b https://example.test",
            "curl --data-binary @f https://example.test",
            "curl -sSd a=b https://example.test",
            "curl -F f=@x https://example.test",
            "curl --form f=@x https://example.test",
            "curl -T f https://example.test/f",
            "curl --upload-file f https://example.test/f",
            "curl --json '{}' https://example.test",
            "wget --post-data=a=b https://example.test",
            "wget --post-file f https://example.test",
            "wget --method=PUT https://example.test",
            "wget --method DELETE https://example.test",
        ] {
            assert!(remote_effect(s), "{s:?}");
        }
    }

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn global_installs() {
        for s in [
            "sudo apt-get install -y jq",
            "sudo apt-get update && echo x",
            "apt-get install jq",
            "apt install jq",
            "brew install jq",
            "brew upgrade",
            "yum install -y x",
            "sudo make install",
            "sudo cp a /usr/local/bin/a",
            "sudo pip install x",
            "apk add curl",
        ] {
            assert_eq!(global_install(s, &none), Install::Needed, "{s:?}");
        }
        for s in [
            "npm ci",
            "echo sudo",
            "echo brew install x",
            "sudo -n true",
            "sudo -v",
            "apt list --installed",
            "apt-cache policy jq",
            "command -v brew",
            "# sudo apt-get install x",
        ] {
            assert_eq!(global_install(s, &none), Install::No, "{s:?}");
        }
    }

    #[test]
    fn brew_install_of_what_is_there_runs() {
        let have = |c: &str| c == "jq" || c == "yq";
        assert_eq!(global_install("brew install jq", &have), Install::Present);
        assert_eq!(
            global_install("brew install jq yq", &have),
            Install::Present
        );
        assert_eq!(
            global_install("brew install --quiet mikefarah/yq/yq", &have),
            Install::Present
        );
        assert_eq!(
            global_install("brew install jq ripgrep", &have),
            Install::Needed
        );
        assert_eq!(
            global_install("brew install --cask jq", &have),
            Install::Needed
        );
        assert_eq!(global_install("brew install", &have), Install::Needed);
        assert_eq!(
            global_install("brew install jq\nsudo apt-get install x", &have),
            Install::Needed
        );
    }

    #[test]
    fn heredoc_bodies_and_continuations() {
        let s = "cat > f <<-'EOF'\n\tgit push\n\tEOF\ngit \\\n  push origin x";
        assert!(remote_effect(s));
        let s = "cat > f <<EOF\ngit push\naws s3 rm x\nEOF\n";
        assert!(!remote_effect(s));
    }

    const M: &str = "\u{e000}local-ci-github-token\u{e001}";

    fn with_mark(t: &str) -> String {
        t.replace("@T@", M)
    }

    #[test]
    fn a_token_that_would_stay_literal_is_found() {
        for t in [
            "cat <<'EOF2'\n@T@\nEOF2\n",
            "cat <<\"EOF\"\nx @T@\nEOF\n",
            "cat <<\\EOF\n@T@\nEOF\n",
            "cat <<-'EOF'\n\t@T@\n\tEOF\n",
            "curl -H 'Authorization: token @T@' https://x.test",
            "jq -n '{t: \"@T@\"}'",
            "python -c 'print(\"@T@\")'",
            "node -e 'console.log(\"@T@\")'",
            "echo $'a @T@'",
            "echo \\@T@",
            "echo \"\\@T@\"",
            "echo \"x $(echo '@T@')\"",
            // an unterminated quote cannot be followed
            "echo \"@T@",
            "echo '@T@",
            // a part of a mark, as `substring()` leaves it
            "echo \u{e000}local-ci-github",
            "echo @T@ \u{e001}",
        ] {
            assert!(token_may_be_literal(&with_mark(t), M), "{t:?}");
        }
    }

    #[test]
    fn a_token_the_shell_expands_is_not() {
        for t in [
            "echo @T@",
            "echo \"token @T@\"",
            "curl -H \"Authorization: token @T@\" https://x.test",
            "git clone https://x:@T@@host/r.git",
            "cat <<EOF\nx @T@\nEOF\n",
            "cat <<EOF2\n@T@\nEOF2\necho @T@",
            // the quotes close before the token
            "echo 'a' @T@ 'b'",
            "echo 'it' \"@T@\"",
            "cat <<'EOF'\nplain\nEOF\necho @T@",
            "# don't @T@\necho @T@",
            "echo \"$(echo @T@)\"",
            "echo \"a 'b' @T@\"",
            "export T=@T@",
            "echo ${X:-@T@}",
        ] {
            assert!(!token_may_be_literal(&with_mark(t), M), "{t:?}");
        }
        // no mark at all
        assert!(!token_may_be_literal("echo 'a'", M));
    }
}
