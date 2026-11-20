//! The task model.
//!
//! A [`Task`] is the atomic unit scheduled by the DAG executor. A
//! [`TaskSet`] is the collection of tasks loaded from a configuration,
//! in declaration order. The raw schema the configuration is read with,
//! and its validation, live in [`crate::config`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::ConfigError;
use crate::steps::Step;

/// Where something is defined in the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The configuration file.
    pub file: PathBuf,
    /// 1-indexed line.
    pub line: usize,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file.display(), self.line)
    }
}

/// What must be true of the host for a task to run. A task whose
/// requirements are not met is skipped, with the reason, rather than
/// failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requirements {
    /// Programs that must be found on `PATH`.
    pub commands: Vec<String>,
    /// Paths that must exist, relative to the project root unless
    /// absolute (such as `/dev/kvm`).
    pub files: Vec<String>,
    /// Environment variables that must be set and not empty.
    pub env: Vec<String>,
}

impl Requirements {
    /// `true` when nothing is required.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty() && self.files.is_empty() && self.env.is_empty()
    }

    /// Every requirement that is not met, described for a person, or
    /// `None` when all are.
    pub fn unmet(&self, root: &Path) -> Option<String> {
        let mut missing = Vec::new();
        let commands: Vec<&str> = self
            .commands
            .iter()
            .filter(|c| which::which(c.as_str()).is_err())
            .map(String::as_str)
            .collect();
        if !commands.is_empty() {
            missing.push(format!("{} not found", commands.join(", ")));
        }
        let files: Vec<&str> = self
            .files
            .iter()
            .filter(|f| !root.join(f.as_str()).exists())
            .map(String::as_str)
            .collect();
        if !files.is_empty() {
            missing.push(format!("{} missing", files.join(", ")));
        }
        let env: Vec<&str> = self
            .env
            .iter()
            .filter(|v| std::env::var_os(v.as_str()).map_or(true, |s| s.is_empty()))
            .map(String::as_str)
            .collect();
        if !env.is_empty() {
            missing.push(format!("{} not set", env.join(", ")));
        }
        if missing.is_empty() {
            None
        } else {
            Some(missing.join("; "))
        }
    }
}

/// Whether a task builds something or checks something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskKind {
    /// A `[tasks.*]` entry.
    #[default]
    Task,
    /// A `[tests.*]` entry.
    Test,
}

/// A single task.
///
/// `inputs` and `outputs` are glob patterns, resolved relative to the
/// project root. `env` uses [`BTreeMap`] so its iteration order is
/// deterministic, which matters for cache-key stability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// Unique task name (the table key in `[tasks.<name>]`).
    pub name: String,
    /// Task or test.
    pub kind: TaskKind,
    /// Shell command to execute.
    pub command: String,
    /// Names of tasks this task depends on.
    pub deps: Vec<String>,
    /// Glob patterns selecting files that are inputs to the task.
    pub inputs: Vec<String>,
    /// Glob patterns selecting files produced by the task.
    pub outputs: Vec<String>,
    /// Extra environment variables set when running the task.
    pub env: BTreeMap<String, String>,
    /// One line saying what the task is for.
    pub description: Option<String>,
    /// Labels a run can select tasks by.
    pub tags: Vec<String>,
    /// Working directory relative to the project root (the root when unset).
    pub cwd: Option<String>,
    /// How long one attempt may take before it is stopped.
    pub timeout: Option<Duration>,
    /// How many times a failed attempt is repeated.
    pub retries: u32,
    /// Whether results are looked up in and stored to the cache.
    pub cache: bool,
    /// Whether a failure leaves the run's result unaffected.
    pub allow_failure: bool,
    /// What the host must provide.
    pub requires: Requirements,
    /// When set, the task is skipped for this reason.
    pub skip: Option<String>,
    /// The shell's argv; the command is appended to it.
    pub shell: Option<Vec<String>>,
    /// Start from an empty environment instead of neomake's own.
    pub env_clear: bool,
    /// Named locks: tasks naming the same one never run at once.
    pub exclusive: Vec<String>,
    /// Run attached to the terminal, alone, with its output not captured.
    pub interactive: bool,
    /// Where the task is defined.
    pub source: Option<Location>,
    /// A test's steps (empty for a task, and for a test with a `command`).
    pub steps: Vec<Step>,
    /// Shell text put in front of every step that runs on the host.
    pub prelude: Option<String>,
    /// Which variant of a matrix this is, if it is one.
    pub variant: Option<Variant>,
    /// Services it uses (see [`crate::services`]).
    pub services: Vec<String>,
    /// The machine a test's steps run in (see [`crate::machine`]).
    pub machine: Option<crate::machine::MachineSpec>,
    /// Start the machine and wait for it to boot before the first step.
    pub autoboot: bool,
    /// Commands run in the machine after every boot.
    pub on_boot: Option<String>,
    /// Steps run in the machine when a step fails, to gather what shows
    /// why; their failures are only logged.
    pub on_failure: Vec<Step>,
}

/// One combination of a matrix's values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    /// The name the matrix is written under (`deployments` for
    /// `deployments[base=joe]`).
    pub base: String,
    /// The value of each matrix key, as its name shows it.
    pub values: Vec<(String, String)>,
}

impl Task {
    /// A task with the defaults every unset field gets.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: TaskKind::Task,
            command: command.into(),
            deps: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            env: BTreeMap::new(),
            description: None,
            tags: Vec::new(),
            cwd: None,
            timeout: None,
            retries: 0,
            cache: true,
            allow_failure: false,
            requires: Requirements::default(),
            skip: None,
            shell: None,
            env_clear: false,
            exclusive: Vec::new(),
            interactive: false,
            source: None,
            steps: Vec::new(),
            prelude: None,
            variant: None,
            services: Vec::new(),
            machine: None,
            autoboot: true,
            on_boot: None,
            on_failure: Vec::new(),
        }
    }

    /// Whether this is a test.
    pub fn is_test(&self) -> bool {
        self.kind == TaskKind::Test
    }

    /// The same task depending on `deps`.
    pub fn with_deps(mut self, deps: &[&str]) -> Self {
        self.deps = deps.iter().map(|d| d.to_string()).collect();
        self
    }
}

/// A collection of tasks loaded from a configuration file.
#[derive(Debug, Clone, Default)]
pub struct TaskSet {
    tasks: Vec<Task>,
}

impl TaskSet {
    /// A set holding `tasks`, which must have distinct names.
    pub fn new(tasks: Vec<Task>) -> Self {
        Self { tasks }
    }

    /// Access all tasks in configuration order.
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// Look up a task by name.
    pub fn get(&self, name: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.name == name)
    }

    /// Load a [`TaskSet`] from a pre-parsed [`toml::Value`].
    ///
    /// `source_path` is used for error reporting and to find the project
    /// root. Prefer [`crate::config::Config::from_sources`], which also
    /// knows where each task is defined.
    pub fn from_value(value: toml::Value, source_path: &Path) -> Result<Self, ConfigError> {
        let text = toml::to_string(&value).map_err(|e| {
            ConfigError::new(
                source_path,
                None,
                format!("cannot re-serialize configuration: {e}"),
            )
        })?;
        let config = crate::config::Config::from_sources(
            &project_root_for(source_path),
            &[(source_path.to_path_buf(), text)],
        )?;
        let mut tasks = config.tasks;
        for task in &mut tasks.tasks {
            // Lines refer to the re-serialized text, not to any file.
            task.source = None;
        }
        Ok(tasks)
    }

    /// Load a [`TaskSet`] directly from a plain `.toml` file on disk.
    ///
    /// This is a convenience wrapper that does not understand TOMLX.
    /// Binaries should prefer going through `neomake-tomlx`, which
    /// gracefully degrades to plain TOML when no extended syntax is used.
    pub fn from_toml_file(path: &Path) -> Result<Self, ConfigError> {
        let source = std::fs::read_to_string(path).map_err(|e| ConfigError {
            path: path.to_path_buf(),
            line: None,
            message: format!("cannot read config: {e}"),
        })?;
        let config = crate::config::Config::from_sources(
            &project_root_for(path),
            &[(path.to_path_buf(), source)],
        )?;
        Ok(config.tasks)
    }
}

/// Locate the project root for a given configuration file path.
///
/// The project root is simply the directory containing the config; this
/// is where `.neomake/cache/` and all glob patterns are resolved.
pub fn project_root_for(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(source: &str) -> Result<TaskSet, ConfigError> {
        let value: toml::Value = toml::from_str(source).unwrap();
        TaskSet::from_value(value, &PathBuf::from("test.toml"))
    }

    #[test]
    fn parses_minimal_task() {
        let set = parse(
            r#"
            [tasks.build]
            command = "cargo build"
            "#,
        )
        .unwrap();
        assert_eq!(set.tasks().len(), 1);
        let t = set.get("build").unwrap();
        assert_eq!(t.command, "cargo build");
        assert!(t.deps.is_empty());
        assert!(t.cache);
    }

    #[test]
    fn rejects_unknown_task_field() {
        let err = parse(
            r#"
            [tasks.build]
            command = "x"
            mystery = "?"
            "#,
        )
        .unwrap_err();
        assert!(
            err.message.contains("unknown field"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_empty_command() {
        let err = parse(
            r#"
            [tasks.build]
            command = ""
            "#,
        )
        .unwrap_err();
        assert!(err.message.contains("empty `command`"), "{}", err.message);
    }

    #[test]
    fn preserves_env_and_deps() {
        let set = parse(
            r#"
            [tasks.a]
            command = "echo a"

            [tasks.b]
            command = "echo b"
            deps = ["a"]
            inputs = ["src/**/*.rs"]
            outputs = ["out.txt"]
            env = { FOO = "bar", BAZ = "qux" }
            "#,
        )
        .unwrap();
        let b = set.get("b").unwrap();
        assert_eq!(b.deps, vec!["a".to_string()]);
        assert_eq!(b.env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(b.env.get("BAZ"), Some(&"qux".to_string()));
    }

    #[test]
    fn keeps_declaration_order() {
        let set = parse(
            r#"
            [tasks.zebra]
            command = "z"
            [tasks.apple]
            command = "a"
            "#,
        )
        .unwrap();
        let names: Vec<&str> = set.tasks().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["zebra", "apple"]);
    }

    #[test]
    fn requirements_describe_what_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("here"), "").unwrap();
        let req = Requirements {
            commands: vec!["cargo".into(), "no-such-program-x9".into()],
            files: vec!["here".into(), "not-here".into()],
            env: vec!["NEOMAKE_SURELY_UNSET_X9".into()],
        };
        let reason = req.unmet(tmp.path()).unwrap();
        assert_eq!(
            reason,
            "no-such-program-x9 not found; not-here missing; NEOMAKE_SURELY_UNSET_X9 not set"
        );
        let met = Requirements {
            files: vec!["here".into()],
            ..Requirements::default()
        };
        assert_eq!(met.unmet(tmp.path()), None);
    }
}
