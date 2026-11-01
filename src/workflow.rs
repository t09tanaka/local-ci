//! Workflow files, read into plain structs. Only the parts local-ci acts on.

use anyhow::{anyhow, Context, Result};
use serde_yaml::Value as Y;

#[derive(Debug, Clone, Default)]
pub struct Trigger {
    pub event: String,
    pub branches: Vec<String>,
    pub branches_ignore: Vec<String>,
    pub paths: Vec<String>,
    pub paths_ignore: Vec<String>,
    pub types: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Workflow {
    pub file: String,
    pub name: String,
    pub triggers: Vec<Trigger>,
    pub env: Vec<(String, String)>,
    pub working_directory: Option<String>,
    pub shell: Option<String>,
    pub jobs: Vec<Job>,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub name: Option<String>,
    pub needs: Vec<String>,
    pub cond: Option<String>,
    pub env: Vec<(String, String)>,
    pub working_directory: Option<String>,
    pub shell: Option<String>,
    pub services: Vec<Service>,
    pub container: bool,
    pub reusable: Option<String>,
    /// `strategy.matrix`, as YAML (a mapping, or an expression string).
    pub matrix: Option<Y>,
    pub fail_fast: bool,
    pub steps: Vec<Step>,
    pub timeout_minutes: Option<f64>,
    pub continue_on_error: bool,
    pub outputs: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Service {
    pub id: String,
    pub image: String,
    pub env: Vec<(String, String)>,
    /// (host port, container port); host is None when only the container port is given.
    pub ports: Vec<(Option<u16>, u16)>,
    pub options: String,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub id: Option<String>,
    pub name: Option<String>,
    pub uses: Option<String>,
    pub run: Option<String>,
    pub with: Vec<(String, String)>,
    pub env: Vec<(String, String)>,
    pub cond: Option<String>,
    pub working_directory: Option<String>,
    pub shell: Option<String>,
    pub continue_on_error: bool,
    pub timeout_minutes: Option<f64>,
}

impl Step {
    pub fn label(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        if let Some(u) = &self.uses {
            return u.clone();
        }
        let first = self
            .run
            .as_deref()
            .unwrap_or("")
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let mut s: String = first.chars().take(60).collect();
        if first.chars().count() > 60 {
            s.push('…');
        }
        s
    }
}

fn scalar(v: &Y) -> Option<String> {
    match v {
        Y::String(s) => Some(s.clone()),
        Y::Bool(b) => Some(b.to_string()),
        Y::Number(n) => Some(n.to_string()),
        Y::Null => Some(String::new()),
        _ => None,
    }
}

fn get<'a>(m: &'a Y, key: &str) -> Option<&'a Y> {
    m.as_mapping()
        .and_then(|m| m.get(Y::String(key.to_string())))
}

fn str_of(m: &Y, key: &str) -> Option<String> {
    get(m, key).and_then(scalar)
}

fn list_of(m: &Y, key: &str) -> Vec<String> {
    match get(m, key) {
        Some(Y::Sequence(s)) => s.iter().filter_map(scalar).collect(),
        Some(v) => scalar(v).into_iter().collect(),
        None => vec![],
    }
}

fn pairs(m: Option<&Y>) -> Vec<(String, String)> {
    match m.and_then(Y::as_mapping) {
        Some(map) => map
            .iter()
            .filter_map(|(k, v)| {
                Some((
                    scalar(k)?,
                    scalar(v).unwrap_or_else(|| serde_yaml::to_string(v).unwrap_or_default()),
                ))
            })
            .collect(),
        None => vec![],
    }
}

fn bool_of(m: &Y, key: &str) -> bool {
    matches!(get(m, key), Some(Y::Bool(true)))
        || matches!(get(m, key), Some(Y::String(s)) if s == "true")
}

fn num_of(m: &Y, key: &str) -> Option<f64> {
    match get(m, key) {
        Some(Y::Number(n)) => n.as_f64(),
        Some(Y::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn defaults_run(m: &Y) -> (Option<String>, Option<String>) {
    let run = get(m, "defaults").and_then(|d| get(d, "run"));
    (
        run.and_then(|r| str_of(r, "working-directory")),
        run.and_then(|r| str_of(r, "shell")),
    )
}

fn parse_triggers(on: Option<&Y>) -> Vec<Trigger> {
    let ev = |name: &str| Trigger {
        event: name.to_string(),
        ..Default::default()
    };
    match on {
        Some(Y::String(s)) => vec![ev(s)],
        Some(Y::Sequence(seq)) => seq.iter().filter_map(scalar).map(|s| ev(&s)).collect(),
        Some(Y::Mapping(map)) => map
            .iter()
            .filter_map(|(k, v)| {
                let mut t = ev(&scalar(k)?);
                t.branches = list_of(v, "branches");
                t.branches_ignore = list_of(v, "branches-ignore");
                t.paths = list_of(v, "paths");
                t.paths_ignore = list_of(v, "paths-ignore");
                t.types = list_of(v, "types");
                Some(t)
            })
            .collect(),
        _ => vec![],
    }
}

fn parse_ports(v: Option<&Y>) -> Vec<(Option<u16>, u16)> {
    let items: Vec<String> = match v {
        Some(Y::Sequence(s)) => s.iter().filter_map(scalar).collect(),
        _ => vec![],
    };
    items
        .iter()
        .filter_map(|p| {
            let p = p.split('/').next().unwrap_or(p);
            let parts: Vec<&str> = p.split(':').collect();
            match parts.as_slice() {
                [c] => Some((None, c.parse().ok()?)),
                [.., h, c] => Some((h.parse().ok(), c.parse().ok()?)),
                _ => None,
            }
        })
        .collect()
}

fn parse_step(s: &Y) -> Step {
    Step {
        id: str_of(s, "id"),
        name: str_of(s, "name"),
        uses: str_of(s, "uses"),
        run: str_of(s, "run"),
        with: pairs(get(s, "with")),
        env: pairs(get(s, "env")),
        cond: str_of(s, "if"),
        working_directory: str_of(s, "working-directory"),
        shell: str_of(s, "shell"),
        continue_on_error: bool_of(s, "continue-on-error"),
        timeout_minutes: num_of(s, "timeout-minutes"),
    }
}

fn parse_job(id: &str, j: &Y) -> Job {
    let (wd, shell) = defaults_run(j);
    let services = get(j, "services")
        .and_then(Y::as_mapping)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    Some(Service {
                        id: scalar(k)?,
                        image: str_of(v, "image").unwrap_or_default(),
                        env: pairs(get(v, "env")),
                        ports: parse_ports(get(v, "ports")),
                        options: str_of(v, "options").unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Job {
        id: id.to_string(),
        name: str_of(j, "name"),
        needs: list_of(j, "needs"),
        cond: str_of(j, "if"),
        env: pairs(get(j, "env")),
        working_directory: wd,
        shell,
        services,
        container: get(j, "container").is_some(),
        reusable: str_of(j, "uses"),
        matrix: get(j, "strategy").and_then(|s| get(s, "matrix")).cloned(),
        fail_fast: !matches!(
            get(j, "strategy").and_then(|s| get(s, "fail-fast")),
            Some(Y::Bool(false))
        ),
        steps: match get(j, "steps") {
            Some(Y::Sequence(s)) => s.iter().map(parse_step).collect(),
            _ => vec![],
        },
        timeout_minutes: num_of(j, "timeout-minutes"),
        continue_on_error: bool_of(j, "continue-on-error"),
        outputs: pairs(get(j, "outputs")),
    }
}

pub fn parse(file: &str, text: &str) -> Result<Workflow> {
    let doc: Y = serde_yaml::from_str(text).with_context(|| format!("{file}: invalid YAML"))?;
    if !doc.is_mapping() {
        return Err(anyhow!("{file}: not a mapping"));
    }
    let (wd, shell) = defaults_run(&doc);
    let jobs = get(&doc, "jobs")
        .and_then(Y::as_mapping)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some(parse_job(&scalar(k)?, v)))
                .collect()
        })
        .unwrap_or_default();
    Ok(Workflow {
        file: file.to_string(),
        name: str_of(&doc, "name")
            .unwrap_or_else(|| file.rsplit('/').next().unwrap_or(file).to_string()),
        // `on` may be read as the boolean true by YAML 1.1 parsers; accept both keys.
        triggers: parse_triggers(
            get(&doc, "on").or_else(|| doc.as_mapping().and_then(|m| m.get(Y::Bool(true)))),
        ),
        env: pairs(get(&doc, "env")),
        working_directory: wd,
        shell,
        jobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WF: &str = r#"
name: CI
on:
  pull_request:
    types: [opened, synchronize]
    paths: ['app/**', '!app/docs/**']
  push:
    branches: [main]
defaults:
  run:
    working-directory: ./app
env:
  NODE_VERSION: 22
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: npm ci
      - name: Lint
        run: npm run lint
        env:
          CI: true
  test:
    needs: lint
    if: github.event.pull_request.draft == false
    services:
      db:
        image: postgres:16
        ports: ["5433:5432", "6379"]
        options: --health-cmd pg_isready
    strategy:
      matrix:
        shard: [1, 2]
    steps:
      - run: npm test -- --shard=${{ matrix.shard }}/2
        timeout-minutes: 5
        continue-on-error: true
"#;

    #[test]
    fn parses_the_parts_we_use() {
        let w = parse(".github/workflows/ci.yml", WF).unwrap();
        assert_eq!(w.name, "CI");
        assert_eq!(w.triggers.len(), 2);
        assert_eq!(w.triggers[0].paths, vec!["app/**", "!app/docs/**"]);
        assert_eq!(w.triggers[1].branches, vec!["main"]);
        assert_eq!(w.working_directory.as_deref(), Some("./app"));
        assert_eq!(w.env, vec![("NODE_VERSION".into(), "22".into())]);
        let test = &w.jobs[1];
        assert_eq!(test.needs, vec!["lint"]);
        assert_eq!(
            test.services[0].ports,
            vec![(Some(5433), 5432), (None, 6379)]
        );
        assert!(test.matrix.is_some());
        assert!(test.steps[0].continue_on_error);
        assert_eq!(test.steps[0].timeout_minutes, Some(5.0));
        assert_eq!(w.jobs[0].steps[2].env, vec![("CI".into(), "true".into())]);
        assert_eq!(w.jobs[0].steps[2].label(), "Lint");
        assert_eq!(w.jobs[0].steps[1].label(), "npm ci");
    }

    #[test]
    fn short_trigger_forms() {
        let w = parse("a.yml", "on: push\njobs: {}\n").unwrap();
        assert_eq!(w.triggers[0].event, "push");
        let w = parse("a.yml", "on: [push, pull_request]\njobs: {}\n").unwrap();
        assert_eq!(w.triggers.len(), 2);
    }
}
