//! The wire form of a plan: types that belong to the plan's schema, not to the
//! engine.
//!
//! The planner and the executor work on the types of [`crate::workflow`] and
//! [`crate::plan`]. A [`crate::FrozenPlan`] does not hold those: it holds the
//! types here, and every crossing is an explicit function (`from_*` into the
//! wire form, `to_*` back). Adding a field to an engine type therefore changes
//! no digest until someone decides to carry it over, and the wire form can be
//! read by a program that does not link the engine.
//!
//! Matrix values are [`MatrixValue`], not YAML or JSON values: keys are always
//! strings (see [`MatrixValue::from_yaml`]) so a workflow with a number or
//! `null` as a mapping key still gives a plan.

use crate::plan::{Selected, SetupStep, StepKey, Unit};
use crate::workflow::{Job, Service, Step, Trigger, Workflow};
use serde::{Deserialize, Serialize};
use serde_yaml::Value as Y;

/// A matrix (or any part of one) as data with a fixed order.
///
/// Mapping keys are strings. A key that is not a string in the YAML is written
/// the way it prints: `true` / `false`, a number as its shortest decimal form
/// (`1.5`, `10`), `null` as `null`. Two keys that print alike (`1` and `"1"`)
/// stay two entries, in file order. Numbers are kept as text, so a float is the
/// same on every host. Values keep their order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MatrixValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Seq(Vec<MatrixValue>),
    Map(Vec<(String, MatrixValue)>),
}

impl MatrixValue {
    pub fn from_yaml(v: &Y) -> Self {
        match v {
            Y::Null => MatrixValue::Null,
            Y::Bool(b) => MatrixValue::Bool(*b),
            Y::Number(n) => MatrixValue::Number(n.to_string()),
            Y::String(s) => MatrixValue::String(s.clone()),
            Y::Sequence(s) => MatrixValue::Seq(s.iter().map(Self::from_yaml).collect()),
            Y::Mapping(m) => MatrixValue::Map(
                m.iter()
                    .map(|(k, v)| (key_text(k), Self::from_yaml(v)))
                    .collect(),
            ),
            Y::Tagged(t) => Self::from_yaml(&t.value),
        }
    }

    pub fn to_yaml(&self) -> Y {
        match self {
            MatrixValue::Null => Y::Null,
            MatrixValue::Bool(b) => Y::Bool(*b),
            MatrixValue::Number(n) => serde_yaml::from_str::<Y>(n)
                .ok()
                .filter(|v| v.is_number())
                .unwrap_or_else(|| Y::String(n.clone())),
            MatrixValue::String(s) => Y::String(s.clone()),
            MatrixValue::Seq(s) => Y::Sequence(s.iter().map(Self::to_yaml).collect()),
            MatrixValue::Map(m) => {
                let mut out = serde_yaml::Mapping::new();
                for (k, v) in m {
                    out.insert(Y::String(k.clone()), v.to_yaml());
                }
                Y::Mapping(out)
            }
        }
    }

    /// As JSON, for expanding the matrix.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            MatrixValue::Null => J::Null,
            MatrixValue::Bool(b) => J::Bool(*b),
            MatrixValue::Number(n) => n
                .parse::<serde_json::Number>()
                .map(J::Number)
                .unwrap_or_else(|_| J::String(n.clone())),
            MatrixValue::String(s) => J::String(s.clone()),
            MatrixValue::Seq(s) => J::Array(s.iter().map(Self::to_json).collect()),
            MatrixValue::Map(m) => {
                J::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
            }
        }
    }

    /// Whether any string in the value holds a `${{ }}` expression.
    pub fn has_expression(&self) -> bool {
        match self {
            MatrixValue::String(s) => s.contains("${{"),
            MatrixValue::Seq(s) => s.iter().any(Self::has_expression),
            MatrixValue::Map(m) => m
                .iter()
                .any(|(k, v)| k.contains("${{") || v.has_expression()),
            _ => false,
        }
    }
}

fn key_text(k: &Y) -> String {
    match k {
        Y::String(s) => s.clone(),
        Y::Bool(b) => b.to_string(),
        Y::Number(n) => n.to_string(),
        Y::Null => "null".into(),
        Y::Tagged(t) => key_text(&t.value),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

/// `paths` / `paths-ignore` of the trigger the workflow was selected by.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PathFilter {
    pub paths: Vec<String>,
    pub paths_ignore: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSpec {
    pub id: String,
    pub image: String,
    pub env: Vec<(String, String)>,
    /// (host port, container port); the host port is absent when only the
    /// container's is given.
    pub ports: Vec<(Option<u16>, u16)>,
    pub options: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepSpec {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub id: String,
    pub name: Option<String>,
    pub needs: Vec<String>,
    pub cond: Option<String>,
    pub env: Vec<(String, String)>,
    pub working_directory: Option<String>,
    pub shell: Option<String>,
    pub services: Vec<ServiceSpec>,
    pub container: bool,
    pub reusable: Option<String>,
    /// `strategy.matrix`: a mapping, or an expression string.
    pub matrix: Option<MatrixValue>,
    pub fail_fast: bool,
    pub max_parallel: Option<usize>,
    pub steps: Vec<StepSpec>,
    pub timeout_minutes: Option<f64>,
    pub continue_on_error: bool,
    pub outputs: Vec<(String, String)>,
}

/// A selected workflow with the event it runs as.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSpec {
    /// Path relative to the repository root.
    pub file: String,
    pub name: String,
    pub event: String,
    pub path_filter: Option<PathFilter>,
    pub env: Vec<(String, String)>,
    pub working_directory: Option<String>,
    pub shell: Option<String>,
    pub jobs: Vec<JobSpec>,
}

/// Jobs that run in order on one thread of a local run (the engine's unit).
/// Tasks and the job graph are the distributable view; this is how the local
/// run schedules, reports and reuses results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnitSpec {
    /// Index into the plan's workflows.
    pub workflow: usize,
    /// Job ids in dependency order.
    pub jobs: Vec<String>,
    pub exclusive: bool,
}

/// An install that runs once before the units (replaced by setup recipes later).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupSpec {
    pub dir: String,
    pub run: String,
    pub env: Vec<(String, String)>,
    pub label: String,
    pub shell: Option<String>,
    pub workflow: usize,
    pub job: String,
    pub execute: bool,
}

impl ServiceSpec {
    pub fn from_service(s: &Service) -> Self {
        ServiceSpec {
            id: s.id.clone(),
            image: s.image.clone(),
            env: s.env.clone(),
            ports: s.ports.clone(),
            options: s.options.clone(),
        }
    }
    pub fn to_service(&self) -> Service {
        Service {
            id: self.id.clone(),
            image: self.image.clone(),
            env: self.env.clone(),
            ports: self.ports.clone(),
            options: self.options.clone(),
        }
    }
}

impl StepSpec {
    pub fn from_step(s: &Step) -> Self {
        StepSpec {
            id: s.id.clone(),
            name: s.name.clone(),
            uses: s.uses.clone(),
            run: s.run.clone(),
            with: s.with.clone(),
            env: s.env.clone(),
            cond: s.cond.clone(),
            working_directory: s.working_directory.clone(),
            shell: s.shell.clone(),
            continue_on_error: s.continue_on_error,
            timeout_minutes: s.timeout_minutes,
        }
    }
    pub fn to_step(&self) -> Step {
        Step {
            id: self.id.clone(),
            name: self.name.clone(),
            uses: self.uses.clone(),
            run: self.run.clone(),
            with: self.with.clone(),
            env: self.env.clone(),
            cond: self.cond.clone(),
            working_directory: self.working_directory.clone(),
            shell: self.shell.clone(),
            continue_on_error: self.continue_on_error,
            timeout_minutes: self.timeout_minutes,
        }
    }
}

impl JobSpec {
    pub fn from_job(j: &Job) -> Self {
        JobSpec {
            id: j.id.clone(),
            name: j.name.clone(),
            needs: j.needs.clone(),
            cond: j.cond.clone(),
            env: j.env.clone(),
            working_directory: j.working_directory.clone(),
            shell: j.shell.clone(),
            services: j.services.iter().map(ServiceSpec::from_service).collect(),
            container: j.container,
            reusable: j.reusable.clone(),
            matrix: j.matrix.as_ref().map(MatrixValue::from_yaml),
            fail_fast: j.fail_fast,
            max_parallel: j.max_parallel,
            steps: j.steps.iter().map(StepSpec::from_step).collect(),
            timeout_minutes: j.timeout_minutes,
            continue_on_error: j.continue_on_error,
            outputs: j.outputs.clone(),
        }
    }
    pub fn to_job(&self) -> Job {
        Job {
            id: self.id.clone(),
            name: self.name.clone(),
            needs: self.needs.clone(),
            cond: self.cond.clone(),
            env: self.env.clone(),
            working_directory: self.working_directory.clone(),
            shell: self.shell.clone(),
            services: self.services.iter().map(ServiceSpec::to_service).collect(),
            container: self.container,
            reusable: self.reusable.clone(),
            matrix: self.matrix.as_ref().map(MatrixValue::to_yaml),
            fail_fast: self.fail_fast,
            max_parallel: self.max_parallel,
            steps: self.steps.iter().map(StepSpec::to_step).collect(),
            timeout_minutes: self.timeout_minutes,
            continue_on_error: self.continue_on_error,
            outputs: self.outputs.clone(),
        }
    }
}

impl WorkflowSpec {
    pub fn from_selected(s: &Selected) -> Self {
        let w = &s.wf;
        // the trigger the cache looks at: the first one for this event
        let path_filter = w
            .triggers
            .iter()
            .find(|t| {
                if s.event == "push" {
                    t.event == "push"
                } else {
                    t.event.starts_with("pull_request")
                }
            })
            .map(|t| PathFilter {
                paths: t.paths.clone(),
                paths_ignore: t.paths_ignore.clone(),
            });
        WorkflowSpec {
            file: w.file.clone(),
            name: w.name.clone(),
            event: s.event.clone(),
            path_filter,
            env: w.env.clone(),
            working_directory: w.working_directory.clone(),
            shell: w.shell.clone(),
            jobs: w.jobs.iter().map(JobSpec::from_job).collect(),
        }
    }

    /// The engine's view. Triggers other than the path filter of the one that
    /// selected the workflow are not carried: they were decided at planning.
    pub fn to_selected(&self) -> Selected {
        let triggers = self
            .path_filter
            .iter()
            .map(|f| Trigger {
                event: if self.event == "push" {
                    "push".into()
                } else {
                    "pull_request".into()
                },
                paths: f.paths.clone(),
                paths_ignore: f.paths_ignore.clone(),
                ..Trigger::default()
            })
            .collect();
        Selected {
            wf: Workflow {
                file: self.file.clone(),
                name: self.name.clone(),
                triggers,
                env: self.env.clone(),
                working_directory: self.working_directory.clone(),
                shell: self.shell.clone(),
                jobs: self.jobs.iter().map(JobSpec::to_job).collect(),
            },
            event: self.event.clone(),
        }
    }

    pub fn job(&self, id: &str) -> Option<&JobSpec> {
        self.jobs.iter().find(|j| j.id == id)
    }
}

impl UnitSpec {
    pub fn from_unit(u: &Unit) -> Self {
        UnitSpec {
            workflow: u.wf,
            jobs: u.jobs.clone(),
            exclusive: u.exclusive,
        }
    }
    pub fn to_unit(&self) -> Unit {
        Unit {
            wf: self.workflow,
            jobs: self.jobs.clone(),
            exclusive: self.exclusive,
        }
    }
}

impl SetupSpec {
    pub fn from_setup(s: &SetupStep) -> Self {
        SetupSpec {
            dir: s.key.wd.clone(),
            run: s.key.run.clone(),
            env: s.key.env.clone(),
            label: s.label.clone(),
            shell: s.shell.clone(),
            workflow: s.wf,
            job: s.job.clone(),
            execute: s.execute,
        }
    }
    pub fn to_setup(&self) -> SetupStep {
        SetupStep {
            key: StepKey {
                wd: self.dir.clone(),
                run: self.run.clone(),
                env: self.env.clone(),
            },
            label: self.label.clone(),
            shell: self.shell.clone(),
            wf: self.workflow,
            job: self.job.clone(),
            execute: self.execute,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_yaml_keys_become_strings_and_survive_a_round_trip() {
        let y: Y = serde_yaml::from_str(
            "os: [a, b]\n1.5: [x]\nnull: [y]\ntrue: [z]\n10: [w]\ninclude:\n  - os: a\n    n: 1e3\n",
        )
        .unwrap();
        let m = MatrixValue::from_yaml(&y);
        let MatrixValue::Map(entries) = &m else {
            panic!()
        };
        let keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["os", "1.5", "null", "true", "10", "include"]);
        // spec -> engine value -> spec is the identity
        assert_eq!(MatrixValue::from_yaml(&m.to_yaml()), m);
        // and through the wire
        let text = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<MatrixValue>(&text).unwrap(), m);
        // the JSON the matrix is expanded from has the same keys
        let j = m.to_json();
        assert!(j.as_object().unwrap().contains_key("1.5"));
        assert_eq!(j["include"][0]["n"], serde_json::json!(1000.0));
    }

    #[test]
    fn expressions_are_found_anywhere() {
        let y: Y =
            serde_yaml::from_str("a: [1]\ninclude:\n  - x: '${{ needs.a.outputs.b }}'\n").unwrap();
        assert!(MatrixValue::from_yaml(&y).has_expression());
        let y: Y = serde_yaml::from_str("a: [1, 2]").unwrap();
        assert!(!MatrixValue::from_yaml(&y).has_expression());
    }
}
