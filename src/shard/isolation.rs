//! Jobs whose tests cannot be given a database, port or container of their own.
//!
//! The shards of a split job run at the same time, on one host or on several,
//! and each starts the steps of the job. A job that starts a container, a
//! server on a port it chooses, or a service the workflow declares would have
//! its shards fight over the same one (or start it twice and test against the
//! wrong copy). This module reads the steps of a job and says what it can see.
//!
//! It sees only what the workflow says. A suite that reaches a shared
//! database through its own configuration (a `.env` file, a hard-coded URL, a
//! test helper that starts a server) is invisible here: the caller who knows
//! the fixtures are isolated says so with [`super::adapters::Safety`], and an
//! end-to-end adapter assumes they are not until it is told.

use crate::workflow::{Job, Step};

/// Programs that start containers or clusters.
const CONTAINER_TOOLS: &[&str] = &[
    "docker",
    "docker-compose",
    "podman",
    "podman-compose",
    "nerdctl",
    "kubectl",
    "minikube",
];

/// Programs that serve something on a port and keep running.
const SERVERS: &[&str] = &[
    "http-server",
    "live-server",
    "wait-on",
    "start-server-and-test",
    "webpack-dev-server",
    "uvicorn",
    "gunicorn",
];

/// A program and the word after it that starts a server (`next dev`).
const SERVER_COMMANDS: &[(&str, &str)] = &[
    ("next", "dev"),
    ("next", "start"),
    ("nuxt", "dev"),
    ("vite", "dev"),
    ("vite", "preview"),
    ("vite", "serve"),
    ("astro", "dev"),
    ("astro", "preview"),
    ("webpack", "serve"),
    ("ng", "serve"),
    ("flask", "run"),
    ("rails", "server"),
    ("rails", "s"),
    ("php", "-s"),
];

/// Package managers: `npm start`, `npm run dev`.
const PACKAGE_MANAGERS: &[&str] = &["npm", "yarn", "pnpm", "bun"];
const SERVER_SCRIPTS: &[&str] = &["start", "dev", "serve", "preview"];

/// Actions that start a database or a container.
const SERVICE_ACTIONS: &[&str] = &[
    "docker",
    "compose",
    "postgres",
    "mysql",
    "mongo",
    "redis",
    "rabbitmq",
    "localstack",
    "kafka",
    "elasticsearch",
];

/// Why the tests of `job` cannot be given resources of their own, or `None`
/// when the steps show nothing of the kind.
pub fn reason(job: &Job) -> Option<String> {
    if let Some(k) = env_reason(&job.env) {
        return Some(k);
    }
    job.steps.iter().find_map(step_reason)
}

fn env_reason(env: &[(String, String)]) -> Option<String> {
    env.iter()
        .find(|(k, _)| k == "PORT" || k.ends_with("_PORT"))
        .map(|(k, _)| format!("it sets the port `{k}`"))
}

fn step_reason(step: &Step) -> Option<String> {
    if let Some(r) = env_reason(&step.env) {
        return Some(r);
    }
    if let Some(u) = &step.uses {
        let u = u.to_lowercase();
        if let Some(w) = SERVICE_ACTIONS.iter().find(|w| u.contains(**w)) {
            return Some(format!("it uses `{u}`, which starts a `{w}`"));
        }
    }
    step.run.as_deref().and_then(run_reason)
}

/// The words of a shell command: split at blanks and the characters a shell
/// reads as syntax, quotes dropped, in lower case.
fn words(run: &str) -> Vec<String> {
    run.split(|c: char| c.is_whitespace() || ";|&()".contains(c))
        .map(|w| w.trim_matches(|c| c == '"' || c == '\'').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// A `&` that sends a command to the background (not `&&`, `2>&1`, `&>`).
fn backgrounds(run: &str) -> bool {
    let c: Vec<char> = run.chars().collect();
    (0..c.len()).any(|i| {
        c[i] == '&'
            && i.checked_sub(1)
                .is_none_or(|p| !matches!(c[p], '&' | '>' | '<'))
            && c.get(i + 1).is_none_or(|n| !matches!(n, '&' | '>'))
    })
}

/// A host and a port in a command (`localhost:3000`).
fn has_local_port(run: &str) -> bool {
    ["localhost:", "127.0.0.1:", "0.0.0.0:", "[::1]:"]
        .iter()
        .any(|h| {
            run.match_indices(h)
                .any(|(i, _)| run[i + h.len()..].starts_with(|c: char| c.is_ascii_digit()))
        })
}

fn run_reason(run: &str) -> Option<String> {
    let w = words(run);
    if let Some(t) = w.iter().find(|w| CONTAINER_TOOLS.contains(&w.as_str())) {
        return Some(format!("it runs `{t}`"));
    }
    if let Some(t) = w.iter().find(|w| SERVERS.contains(&w.as_str())) {
        return Some(format!("it runs `{t}`, which serves on a port"));
    }
    for pair in w.windows(2) {
        let (a, b) = (pair[0].as_str(), pair[1].as_str());
        if SERVER_COMMANDS.contains(&(a, b)) || (PACKAGE_MANAGERS.contains(&a) && b == "start") {
            return Some(format!("it runs `{a} {b}`, which serves on a port"));
        }
    }
    for t in w.windows(3) {
        if PACKAGE_MANAGERS.contains(&t[0].as_str())
            && t[1] == "run"
            && SERVER_SCRIPTS.contains(&t[2].as_str())
        {
            return Some(format!(
                "it runs `{} run {}`, which serves on a port",
                t[0], t[2]
            ));
        }
    }
    if w.iter().any(|x| x == "python" || x == "python3") && w.iter().any(|x| x == "http.server") {
        return Some("it runs a Python web server".into());
    }
    if has_local_port(run) || w.iter().any(|w| w == "--port" || w.starts_with("--port=")) {
        return Some("it names a port".into());
    }
    if run.contains("PORT=") {
        return Some("it sets a port".into());
    }
    backgrounds(run).then(|| "it starts a process in the background".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(steps: &[(&str, Option<&str>)]) -> Job {
        let steps = steps
            .iter()
            .map(|(run, uses)| Step {
                id: None,
                name: None,
                uses: uses.map(str::to_string),
                run: (!run.is_empty()).then(|| run.to_string()),
                with: vec![],
                env: vec![],
                cond: None,
                working_directory: None,
                shell: None,
                continue_on_error: false,
                timeout_minutes: None,
            })
            .collect();
        Job {
            id: "t".into(),
            name: None,
            needs: vec![],
            cond: None,
            env: vec![],
            working_directory: None,
            shell: None,
            services: vec![],
            container: false,
            reusable: None,
            matrix: None,
            fail_fast: false,
            max_parallel: None,
            steps,
            timeout_minutes: None,
            continue_on_error: false,
            outputs: vec![],
        }
    }

    fn why(run: &str) -> Option<String> {
        reason(&job(&[(run, None)]))
    }

    #[test]
    fn plain_test_commands_show_nothing() {
        for run in [
            "npm ci",
            "npx playwright test",
            "cargo test --workspace 2>&1 | tee log",
            "npx vitest run && echo done",
            "npm run build",
            "echo a > f &> g",
            "curl https://example.com/path",
            "python -m pytest",
            "npm test",
        ] {
            assert_eq!(why(run), None, "{run}");
        }
        assert_eq!(reason(&job(&[])), None);
    }

    #[test]
    fn containers_servers_and_ports_are_seen() {
        for (run, what) in [
            ("docker compose up -d", "`docker`"),
            ("sudo docker-compose up", "`docker-compose`"),
            ("kubectl apply -f x", "`kubectl`"),
            ("npx wait-on http://x", "`wait-on`"),
            (
                "npx start-server-and-test start http://x test",
                "serves on a port",
            ),
            ("npx next dev", "`next dev`"),
            ("npx vite preview", "`vite preview`"),
            ("npm start", "`npm start`"),
            ("yarn start", "`yarn start`"),
            ("pnpm run dev", "`pnpm run dev`"),
            ("php -S 0.0.0.0:80", "`php -s`"),
            ("python3 -m http.server", "Python web server"),
            ("python -m http.server 8000", "Python web server"),
            ("curl localhost:3000/health", "names a port"),
            ("curl http://127.0.0.1:8080", "names a port"),
            ("serve --port 3000", "names a port"),
            ("serve --port=3000", "names a port"),
            ("PORT=3000 node server.js", "sets a port"),
            ("node server.js &", "background"),
            ("node server.js & sleep 1", "background"),
            ("node server.js &\nsleep 1", "background"),
            ("echo 'a' &", "background"),
        ] {
            let got = why(run).unwrap_or_default();
            assert!(got.contains(what), "{run}: {got}");
        }
    }

    #[test]
    fn a_host_without_a_port_and_redirections_are_not_ports_or_background() {
        for run in [
            "curl localhost/x",
            "curl http://localhost:/x",
            "cmd 2>&1",
            "cmd >&2",
            "cmd &>/dev/null",
            "a && b",
            "&& a",
        ] {
            assert_eq!(why(run), None, "{run}");
        }
        assert!(backgrounds("&"));
        assert!(backgrounds("a &"));
        assert!(!backgrounds("a &&"));
        assert!(!backgrounds(""));
    }

    #[test]
    fn actions_and_environment_are_read_too() {
        let a = reason(&job(&[("", Some("hoverkraft-tech/compose-action@v2"))])).unwrap();
        assert!(a.contains("compose"), "{a}");
        assert_eq!(reason(&job(&[("", Some("actions/checkout@v4"))])), None);
        let mut j = job(&[("npx playwright test", None)]);
        j.env = vec![("API_PORT".into(), "3000".into())];
        assert!(reason(&j).unwrap().contains("API_PORT"));
        let mut j = job(&[("npx playwright test", None)]);
        j.steps[0].env = vec![("PORT".into(), "3000".into())];
        assert!(reason(&j).unwrap().contains("`PORT`"));
        j.steps[0].env = vec![("PORTAL".into(), "x".into())];
        assert_eq!(reason(&j), None);
        // a step further on is read too
        let j = job(&[
            ("npm ci", None),
            ("docker run x", None),
            ("npx playwright test", None),
        ]);
        assert!(reason(&j).unwrap().contains("docker"));
    }
}
