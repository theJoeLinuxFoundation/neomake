//! The configuration schema and its validation.
//!
//! A configuration is one or more documents of plain TOML (TOMLX is
//! evaluated to TOML first, by `neomake-tomlx`), each parsed on its own
//! so that every task knows the file and line it is defined at. Their
//! tasks are then put together; a name defined in two documents is an
//! error naming both. `[settings]` may only appear in the first,
//! top-level document.
//!
//! Every table rejects keys it does not know, so a misspelt field is an
//! error rather than silently ignored.
//!
//! Tests (`[tests.*]`) are tasks that are not cached unless they ask to
//! be, and that may have `steps` instead of a `command` (see
//! [`crate::steps`]); templates of steps (`[templates.*]`) may be defined
//! in any document. Either may have a `matrix` ([`matrix`]).

mod machines;
mod matrix;
mod services;
mod steps;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use serde::Deserialize;
use toml::Spanned;

use crate::duration;
use crate::error::ConfigError;
use crate::task::{Location, Requirements, Task, TaskKind, TaskSet};

/// Settings for the whole project, from `[settings]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Where neomake keeps its cache, runs and logs, relative to the
    /// project root.
    pub state_dir: PathBuf,
    /// The timeout of a task that sets none.
    pub default_timeout: Option<Duration>,
    /// How many past runs' directories are kept.
    pub keep_runs: usize,
    /// The shell of a task that sets none.
    pub shell: Option<Vec<String>>,
    /// How many machine tests may run at once.
    pub max_machines: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::from(".neomake"),
            default_timeout: None,
            keep_runs: 20,
            shell: None,
            max_machines: 2,
        }
    }
}

/// A loaded, validated configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The project root: the directory of the top-level file.
    pub root: PathBuf,
    /// `[settings]`.
    pub settings: Settings,
    /// Every task and test, in declaration order.
    pub tasks: TaskSet,
    /// Every service, by name.
    pub services: BTreeMap<String, crate::services::Service>,
    /// Every machine, by name.
    pub machines: BTreeMap<String, crate::machine::MachineSpec>,
}

impl Config {
    /// Build a configuration from `(path, toml_text)` documents, the
    /// top-level one first.
    pub fn from_sources(root: &Path, sources: &[(PathBuf, String)]) -> Result<Self, ConfigError> {
        let mut settings = Settings::default();
        let mut tasks: Vec<Task> = Vec::new();
        let mut services: BTreeMap<String, crate::services::Service> = BTreeMap::new();
        let mut defined_in: BTreeMap<String, Location> = BTreeMap::new();

        // Templates first, from every document, since any test may use them.
        let mut parsed = Vec::with_capacity(sources.len());
        let mut templates = steps::Templates::default();
        let mut machines = machines::Machines::default();
        for (path, text) in sources {
            let mut raw: RawConfig = toml::from_str(text).map_err(|e| {
                let line = e.span().map(|s| line_of(text, s.start));
                ConfigError::new(path, line, e.message().trim_end().to_string())
            })?;
            for (name, template) in std::mem::take(&mut raw.templates) {
                templates.add(name, template, path, text)?;
            }
            for (name, machine) in std::mem::take(&mut raw.machines) {
                let at = Location {
                    file: path.clone(),
                    line: line_of(text, machine.span().start),
                };
                machines
                    .add(name, machine.into_inner(), at.clone())
                    .map_err(|m| ConfigError::new(&at.file, Some(at.line), m))?;
            }
            parsed.push(raw);
        }
        let machine_specs = machines::check(&machines)
            .map_err(|(at, m)| ConfigError::new(&at.file, Some(at.line), m))?;

        for (index, ((path, text), raw)) in sources.iter().zip(parsed).enumerate() {
            if let Some(raw_settings) = raw.settings {
                if index != 0 {
                    return Err(ConfigError::new(
                        path,
                        Some(line_of(text, raw_settings.span().start)),
                        "[settings] belongs in the top-level configuration file",
                    ));
                }
                settings = convert_settings(raw_settings.into_inner(), path)?;
            }

            for (name, raw_service) in raw.services {
                let location = Location {
                    file: path.clone(),
                    line: line_of(text, raw_service.span().start),
                };
                if let Some(first) = services.get(&name) {
                    return Err(ConfigError::new(
                        path,
                        Some(location.line),
                        format!(
                            "service `{name}` is already defined at {}",
                            first
                                .source
                                .as_ref()
                                .map(|l| l.to_string())
                                .unwrap_or_default()
                        ),
                    ));
                }
                let service = services::convert(&name, raw_service.into_inner(), &location)
                    .map_err(|m| {
                        ConfigError::new(
                            path,
                            Some(location.line),
                            format!("service `{name}`: {m}"),
                        )
                    })?;
                services.insert(name, service);
            }

            let entries = raw
                .tasks
                .into_iter()
                .map(|(name, raw)| (name, raw, TaskKind::Task))
                .chain(
                    raw.tests
                        .into_iter()
                        .map(|(name, raw)| (name, raw, TaskKind::Test)),
                );
            for (name, raw_task, kind) in entries {
                let location = Location {
                    file: path.clone(),
                    line: line_of(text, raw_task.span().start),
                };
                if let Some(first) = defined_in.get(&name) {
                    return Err(ConfigError::new(
                        path,
                        Some(location.line),
                        format!("`{name}` is already defined at {first}"),
                    ));
                }
                let origin = steps::Origin { file: path, text };
                let mut raw_task = raw_task.into_inner();
                let matrix_values = std::mem::take(&mut raw_task.matrix);
                let machine_value = raw_task.machine.take();
                if machine_value.is_some() && kind != TaskKind::Test {
                    return Err(ConfigError::new(
                        path,
                        Some(location.line),
                        format!("task `{name}` has a `machine`, which only tests have"),
                    ));
                }
                let task = convert_task(
                    name.clone(),
                    kind,
                    raw_task,
                    &location,
                    &origin,
                    &templates,
                    machine_value.is_some(),
                )?;
                let variants = if matrix_values.is_empty() {
                    vec![(task, BTreeMap::new())]
                } else {
                    matrix::expand(&task, &matrix_values).map_err(|m| {
                        ConfigError::new(path, Some(location.line), format!("`{name}`: {m}"))
                    })?
                };
                defined_in.insert(name, location.clone());
                for (mut variant, fields) in variants {
                    if let Some(value) = &machine_value {
                        let mut value = value.clone();
                        machines::map_strings(&mut value, &mut |s| {
                            *s = crate::steps::vars::substitute_known(s, &|n| {
                                fields.get(n).cloned()
                            });
                        });
                        let spec = machines.resolve(&value, &location).map_err(|m| {
                            ConfigError::new(
                                path,
                                Some(location.line),
                                format!("`{}`: {m}", variant.name),
                            )
                        })?;
                        variant.machine = Some(spec);
                    }
                    if variant.variant.is_some() {
                        if let Some(first) = defined_in.get(&variant.name) {
                            return Err(ConfigError::new(
                                path,
                                Some(location.line),
                                format!("`{}` is already defined at {first}", variant.name),
                            ));
                        }
                        defined_in.insert(variant.name.clone(), location.clone());
                    }
                    tasks.push(variant);
                }
            }
        }

        // A dependency on a matrix's name is one on all of its variants.
        let mut variants_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for t in &tasks {
            if let Some(v) = &t.variant {
                variants_of
                    .entry(v.base.clone())
                    .or_default()
                    .push(t.name.clone());
            }
        }
        for t in &mut tasks {
            let deps = std::mem::take(&mut t.deps);
            for dep in deps {
                match variants_of.get(&dep) {
                    Some(all) => t.deps.extend(all.iter().cloned()),
                    None => t.deps.push(dep),
                }
            }
            t.deps.dedup();
        }

        // A task that uses a service depends on what the service needs.
        for t in &mut tasks {
            for name in t.services.clone() {
                let Some(service) = services.get(&name) else {
                    let at = t.source.clone().expect("loaded tasks have a source");
                    let known: Vec<&str> = services.keys().map(String::as_str).collect();
                    return Err(ConfigError::new(
                        &at.file,
                        Some(at.line),
                        match crate::executor::closest(&name, known.into_iter()) {
                            Some(s) => format!(
                                "`{}` uses no service `{name}` (did you mean `{s}`?)",
                                t.name
                            ),
                            None => format!("`{}` uses no service `{name}`", t.name),
                        },
                    ));
                };
                for dep in &service.deps {
                    let expanded = variants_of
                        .get(dep)
                        .cloned()
                        .unwrap_or_else(|| vec![dep.clone()]);
                    for d in expanded {
                        if !t.deps.contains(&d) && d != t.name {
                            t.deps.push(d);
                        }
                    }
                }
            }
        }

        Ok(Self {
            root: root.to_path_buf(),
            settings,
            tasks: TaskSet::new(tasks),
            services,
            machines: machine_specs,
        })
    }
}

impl Config {
    /// What is wrong with the configuration that loading it does not
    /// catch: placeholders that nothing defines by the step that uses
    /// them. Each problem starts with where it is (`file:line: `).
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for task in self.tasks.tasks() {
            let file = task
                .source
                .as_ref()
                .map(|l| {
                    l.file
                        .strip_prefix(&self.root)
                        .unwrap_or(&l.file)
                        .display()
                        .to_string()
                })
                .unwrap_or_default();
            let mut known: Vec<String> = BUILTIN_VARS.iter().map(|v| v.to_string()).collect();
            for name in &task.services {
                if let Some(service) = self.services.get(name) {
                    known.extend(service.var_names());
                }
            }
            if let Some(machine) = &task.machine {
                known.extend(crate::machine::MACHINE_VARS.iter().map(|v| v.to_string()));
                known.extend(
                    machine
                        .disks
                        .iter()
                        .filter_map(|d| d.name.as_ref())
                        .map(|n| format!("machine.disks.{n}")),
                );
            }
            let mut report = |line: Option<usize>, what: &str, name: &str| {
                let at = match line.or(task.source.as_ref().map(|l| l.line)) {
                    Some(l) => format!("{file}:{l}"),
                    None => file.clone(),
                };
                problems.push(format!(
                    "{at}: `{}`: {what} uses `{{{{{name}}}}}`, which nothing defines there",
                    task.name
                ));
            };
            for name in steps_vars::names(&task.command) {
                if !known.contains(&name) {
                    report(None, "its command", &name);
                }
            }
            for text in task.prelude.iter().chain(task.on_boot.iter()) {
                for name in steps_vars::names(text) {
                    if !known.contains(&name) {
                        report(None, "its prelude or on_boot", &name);
                    }
                }
            }
            // Placeholders are filled in only in commands, preludes and steps.
            let mut rest = task.clone();
            rest.steps.clear();
            rest.command.clear();
            rest.prelude = None;
            rest.on_boot = None;
            let mut strings = Vec::new();
            matrix::for_each_string(&mut rest, &mut |s| strings.push(s.clone()));
            for s in &strings {
                for name in steps_vars::names(s) {
                    report(None, "a field other than its command or steps", &name);
                }
            }
            for step in task.steps.iter().chain(task.on_failure.iter()) {
                let line = step.line;
                let mut copy = step.clone();
                for s in copy.strings_mut() {
                    for name in steps_vars::names(s) {
                        if !known.contains(&name) {
                            report(line, "a step", &name);
                        }
                    }
                }
                known.extend(step.check.captures.iter().map(|c| c.name.clone()));
            }
        }
        problems
    }
}

/// The variables every test's steps can use.
pub const BUILTIN_VARS: &[&str] = &["test.name", "test.dir", "run.dir", "project.root"];

use crate::steps::vars as steps_vars;

pub(crate) fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

fn convert_settings(raw: RawSettings, path: &Path) -> Result<Settings, ConfigError> {
    let defaults = Settings::default();
    let state_dir = match raw.state_dir {
        Some(dir) if dir.trim().is_empty() => {
            return Err(ConfigError::new(
                path,
                None,
                "`settings.state_dir` cannot be empty",
            ))
        }
        Some(dir) => PathBuf::from(dir),
        None => defaults.state_dir,
    };
    Ok(Settings {
        state_dir,
        default_timeout: raw
            .default_timeout
            .map(|d| d.parse())
            .transpose()
            .map_err(|e| ConfigError::new(path, None, format!("settings.default_timeout: {e}")))?,
        keep_runs: raw.keep_runs.unwrap_or(defaults.keep_runs),
        shell: raw.shell.map(ShellValue::into_argv),
        max_machines: raw.max_machines.unwrap_or(defaults.max_machines).max(1),
    })
}

fn convert_task(
    name: String,
    kind: TaskKind,
    raw: RawTask,
    at: &Location,
    origin: &steps::Origin<'_>,
    templates: &steps::Templates,
    has_machine: bool,
) -> Result<Task, ConfigError> {
    let err = |msg: String| ConfigError::new(&at.file, Some(at.line), msg);
    if name.is_empty() {
        return Err(err("a task name cannot be empty".into()));
    }
    if name.contains(['[', ']', ',', '*', '?']) {
        return Err(err(format!(
            "`{name}` cannot be a name: `[`, `]`, `,`, `*` and `?` are kept for matrices and patterns"
        )));
    }
    let what = if kind == TaskKind::Test {
        "test"
    } else {
        "task"
    };
    let steps = steps::convert(&raw.steps, origin, templates, has_machine)?;
    let on_failure = steps::convert(&raw.on_failure, origin, templates, has_machine)?;
    if !has_machine && (raw.on_boot.is_some() || !on_failure.is_empty() || raw.autoboot.is_some()) {
        return Err(err(format!(
            "`{name}`: `on_boot`, `on_failure` and `autoboot` are for tests with a `machine`"
        )));
    }
    if has_machine && steps.is_empty() {
        return Err(err(format!(
            "test `{name}` has a machine but no steps to run in it"
        )));
    }
    let command = match (raw.command, steps.is_empty()) {
        (Some(c), true) if c.trim().is_empty() => {
            return Err(err(format!("{what} `{name}` has an empty `command`")))
        }
        (Some(c), true) => c,
        (None, false) if kind == TaskKind::Test => String::new(),
        (None, false) => {
            return Err(err(format!(
                "task `{name}` has `steps`, which only tests have (write it under [tests])"
            )))
        }
        (Some(_), false) => {
            return Err(err(format!(
                "test `{name}` has both a `command` and `steps`; it has one or the other"
            )))
        }
        (None, true) => {
            return Err(err(match kind {
                TaskKind::Test => format!("test `{name}` needs a `command` or `steps`"),
                TaskKind::Task => format!("task `{name}` needs a `command`"),
            }))
        }
    };
    if raw.prelude.is_some() && steps.is_empty() {
        return Err(err(format!(
            "{what} `{name}` has a `prelude` but no steps to put it in front of"
        )));
    }
    if raw.deps.contains(&name) {
        return Err(err(format!("task `{name}` depends on itself")));
    }
    let timeout = raw
        .timeout
        .map(|d| d.parse())
        .transpose()
        .map_err(|e| err(format!("task `{name}`: timeout: {e}")))?;
    let shell = raw.shell.map(ShellValue::into_argv);
    if shell.as_ref().is_some_and(|s| s.is_empty()) {
        return Err(err(format!("task `{name}`: `shell` cannot be empty")));
    }
    Ok(Task {
        kind,
        command,
        steps,
        prelude: raw.prelude,
        variant: None,
        deps: raw.deps,
        inputs: raw.inputs,
        outputs: raw.outputs,
        env: raw.env,
        description: raw.description,
        tags: raw.tags,
        cwd: raw.cwd,
        timeout,
        retries: raw.retries,
        cache: raw.cache.unwrap_or(kind == TaskKind::Task),
        allow_failure: raw.allow_failure,
        requires: Requirements {
            commands: raw.requires.commands,
            files: raw.requires.files,
            env: raw.requires.env,
        },
        skip: raw.skip.and_then(SkipValue::reason),
        shell,
        env_clear: raw.env_clear,
        exclusive: raw.exclusive,
        interactive: raw.interactive,
        services: raw.services,
        machine: None,
        autoboot: raw.autoboot.unwrap_or(true),
        on_boot: raw.on_boot,
        on_failure,
        source: Some(at.clone()),
        name,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    settings: Option<Spanned<RawSettings>>,
    #[serde(default)]
    tasks: IndexMap<String, Spanned<RawTask>>,
    #[serde(default)]
    tests: IndexMap<String, Spanned<RawTask>>,
    #[serde(default)]
    templates: IndexMap<String, Spanned<steps::RawTemplate>>,
    #[serde(default)]
    services: IndexMap<String, Spanned<services::RawService>>,
    #[serde(default)]
    machines: IndexMap<String, Spanned<machines::RawMachine>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    #[serde(default)]
    state_dir: Option<String>,
    #[serde(default)]
    default_timeout: Option<RawDuration>,
    #[serde(default)]
    keep_runs: Option<usize>,
    #[serde(default)]
    shell: Option<ShellValue>,
    #[serde(default)]
    max_machines: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTask {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    steps: Vec<Spanned<steps::RawStep>>,
    #[serde(default)]
    prelude: Option<String>,
    #[serde(default)]
    matrix: IndexMap<String, Vec<toml::Value>>,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout: Option<RawDuration>,
    #[serde(default)]
    retries: u32,
    #[serde(default)]
    cache: Option<bool>,
    #[serde(default)]
    allow_failure: bool,
    #[serde(default)]
    requires: RawRequirements,
    #[serde(default)]
    skip: Option<SkipValue>,
    #[serde(default)]
    shell: Option<ShellValue>,
    #[serde(default)]
    env_clear: bool,
    #[serde(default)]
    exclusive: Vec<String>,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    services: Vec<String>,
    #[serde(default)]
    machine: Option<toml::Value>,
    #[serde(default)]
    autoboot: Option<bool>,
    #[serde(default)]
    on_boot: Option<String>,
    #[serde(default)]
    on_failure: Vec<Spanned<steps::RawStep>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequirements {
    #[serde(default)]
    commands: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
}

/// `"10m"` or a number of seconds.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawDuration {
    Seconds(u64),
    Text(String),
}

impl RawDuration {
    fn parse(self) -> Result<Duration, String> {
        match self {
            RawDuration::Seconds(s) => Ok(Duration::from_secs(s)),
            RawDuration::Text(t) => duration::parse(&t),
        }
    }
}

/// `"bash -eu -c"` or `["bash", "-eu", "-c"]`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ShellValue {
    Line(String),
    Argv(Vec<String>),
}

impl ShellValue {
    pub(crate) fn into_argv(self) -> Vec<String> {
        match self {
            ShellValue::Line(line) => line.split_whitespace().map(str::to_string).collect(),
            ShellValue::Argv(argv) => argv,
        }
    }
}

/// `skip = "why"`, or `skip = true` / `false`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SkipValue {
    Flag(bool),
    Reason(String),
}

impl SkipValue {
    fn reason(self) -> Option<String> {
        match self {
            SkipValue::Flag(false) => None,
            SkipValue::Flag(true) => Some("skipped by its configuration".into()),
            SkipValue::Reason(r) if r.trim().is_empty() => None,
            SkipValue::Reason(r) => Some(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(docs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let sources: Vec<(PathBuf, String)> = docs
            .iter()
            .map(|(p, t)| (PathBuf::from(p), t.to_string()))
            .collect();
        Config::from_sources(Path::new("."), &sources)
    }

    #[test]
    fn reads_every_task_field() {
        let config = load(&[(
            "neomake.toml",
            r#"
[settings]
state_dir = "build/neomake"
default_timeout = "30m"
keep_runs = 3
shell = "bash -euo pipefail -c"

[tasks.image]
command = "make image"
description = "the disk image"
tags = ["image", "slow"]
cwd = "image"
timeout = "1h30m"
retries = 2
allow_failure = true
requires = { commands = ["mkfs.btrfs"], files = ["/dev/kvm"], env = ["HOME"] }
skip = "not today"
shell = ["bash", "-c"]
env_clear = true
exclusive = ["port-8643"]

[tests.smoke]
command = "true"
timeout = 90
"#,
        )])
        .unwrap();
        let s = &config.settings;
        assert_eq!(s.state_dir, PathBuf::from("build/neomake"));
        assert_eq!(s.default_timeout, Some(Duration::from_secs(1800)));
        assert_eq!(s.keep_runs, 3);
        assert_eq!(
            s.shell.as_deref(),
            Some(
                &[
                    "bash".to_string(),
                    "-euo".into(),
                    "pipefail".into(),
                    "-c".into()
                ][..]
            )
        );

        let t = config.tasks.get("image").unwrap();
        assert_eq!(t.kind, TaskKind::Task);
        assert_eq!(t.description.as_deref(), Some("the disk image"));
        assert_eq!(t.tags, vec!["image", "slow"]);
        assert_eq!(t.cwd.as_deref(), Some("image"));
        assert_eq!(t.timeout, Some(Duration::from_secs(5400)));
        assert_eq!(t.retries, 2);
        assert!(t.cache && t.allow_failure && t.env_clear);
        assert_eq!(t.requires.files, vec!["/dev/kvm"]);
        assert_eq!(t.skip.as_deref(), Some("not today"));
        assert_eq!(
            t.shell.as_deref(),
            Some(&["bash".to_string(), "-c".into()][..])
        );
        assert_eq!(t.exclusive, vec!["port-8643"]);
        assert_eq!(t.source.as_ref().unwrap().line, 8);

        let test = config.tasks.get("smoke").unwrap();
        assert_eq!(test.kind, TaskKind::Test);
        assert!(!test.cache, "tests are not cached unless they ask to be");
        assert_eq!(test.timeout, Some(Duration::from_secs(90)));
    }

    #[test]
    fn errors_carry_the_line() {
        let err = load(&[("a.toml", "[tasks.x]\ncommand = 'x'\ntimeout = 'soon'\n")]).unwrap_err();
        assert_eq!(err.line, Some(1));
        assert!(err.message.contains("timeout"), "{}", err.message);

        let err = load(&[("a.toml", "[tasks.x]\ncommand = 'x'\nretries = 'two'\n")]).unwrap_err();
        assert_eq!(err.line, Some(3), "{}", err.message);
    }

    #[test]
    fn names_are_unique_across_documents() {
        let err = load(&[
            ("neomake.toml", "[tasks.a]\ncommand = 'a'\n"),
            ("more.toml", "\n\n[tests.a]\ncommand = 'b'\n"),
        ])
        .unwrap_err();
        assert_eq!(err.path, PathBuf::from("more.toml"));
        assert_eq!(err.line, Some(3));
        assert!(err.message.contains("neomake.toml:1"), "{}", err.message);
    }

    #[test]
    fn settings_only_at_the_top() {
        let err = load(&[
            ("neomake.toml", ""),
            ("more.toml", "[settings]\nkeep_runs = 1\n"),
        ])
        .unwrap_err();
        assert!(err.message.contains("top-level"), "{}", err.message);
    }

    #[test]
    fn skip_accepts_flags_and_reasons() {
        let config = load(&[(
            "n.toml",
            "[tasks.a]\ncommand='a'\nskip=false\n[tasks.b]\ncommand='b'\nskip=true\n[tasks.c]\ncommand='c'\nskip=''\n",
        )])
        .unwrap();
        assert_eq!(config.tasks.get("a").unwrap().skip, None);
        assert!(config.tasks.get("b").unwrap().skip.is_some());
        assert_eq!(config.tasks.get("c").unwrap().skip, None);
    }

    #[test]
    fn reads_steps_templates_and_their_lines() {
        use crate::steps::{Action, ExitCheck, StreamSel};
        let config = load(&[
            (
                "neomake.toml",
                r#"
[tests.cli]
prelude = "dave() { echo dave; }"
steps = [
  { section = "help" },
  { run = 'dave --help', contains = ["Usage", "help"], lacks = "panic" },
  { run = 'dave isntall', exit = "nonzero", stream = "combined", matches = 'did you mean' },
  { use = "installs", with = { pkg = "hello", n = 2 } },
  { run = 'dave version', capture = { version = 'dave (\S+)' } },
  { run = 'dave remove', tty = { respond = [{ on = '\[Y/n\]', send = "y\n" }] } },
]
"#,
            ),
            (
                "templates.toml",
                r#"
[templates.installs]
params = ["pkg", "n"]
steps = [{ run = 'dave install {{pkg}}', contains = "{{n}} to install." }]
"#,
            ),
        ])
        .unwrap();
        let t = config.tasks.get("cli").unwrap();
        assert!(t.is_test() && t.command.is_empty());
        assert_eq!(t.steps.len(), 6);
        assert_eq!(t.steps[0].action, Action::Section("help".into()));
        assert_eq!(t.steps[1].line, Some(6));
        assert_eq!(t.steps[1].check.contains, vec!["Usage", "help"]);
        assert_eq!(t.steps[2].check.exit, ExitCheck::Nonzero);
        assert_eq!(t.steps[2].check.stream, StreamSel::Combined);
        let used = &t.steps[3];
        assert_eq!(
            used.line,
            Some(8),
            "template steps are reported where they are used"
        );
        assert_eq!(used.describe(), "installs › run: dave install hello");
        assert_eq!(used.check.contains, vec!["2 to install."]);
        assert_eq!(t.steps[4].check.captures[0].name, "version");
        assert_eq!(t.steps[5].check.stream, StreamSel::Combined);
    }

    #[test]
    fn step_mistakes_are_reported_at_their_line() {
        let cases = [
            ("{ run = 'a', host = 'b' }", "does one thing"),
            ("{ contains = 'x' }", "needs one of"),
            ("{ section = 's', contains = 'x' }", "nothing to check"),
            ("{ run = 'a', exit = 'sometimes' }", "\"nonzero\""),
            ("{ run = 'a', matches = '(' }", "invalid pattern"),
            ("{ run = 'a', interval = '1s' }", "goes with `wait`"),
            ("{ use = 'nope' }", "no template named `nope`"),
            ("{ run = 'a', bogus = 1 }", "unknown field"),
        ];
        for (step, wanted) in cases {
            let text = format!("[tests.t]\nsteps = [\n  {step},\n]\n");
            let err = load(&[("n.toml", &text)]).unwrap_err();
            assert!(err.message.contains(wanted), "{step}: {}", err.message);
            assert_eq!(err.line, Some(3), "{step}: {}", err.message);
        }
        let err = load(&[("n.toml", "[tasks.t]\nsteps = [{ run = 'a' }]\n")]).unwrap_err();
        assert!(err.message.contains("only tests"), "{}", err.message);
        let err = load(&[(
            "n.toml",
            "[tests.t]\ncommand = 'x'\nsteps = [{ run = 'a' }]\n",
        )])
        .unwrap_err();
        assert!(err.message.contains("one or the other"), "{}", err.message);
    }

    #[test]
    fn templates_check_their_parameters() {
        let template = "[templates.tpl]\nparams = ['a']\nsteps = [{ run = 'echo {{a}}' }]\n";
        for (step, wanted) in [
            ("{ use = 'tpl' }", "needs `a`"),
            (
                "{ use = 'tpl', with = { a = 1, b = 2 } }",
                "no parameter `b`",
            ),
            (
                "{ use = 'tpl', with = { a = 1 }, run = 'x' }",
                "only `use` and `with`",
            ),
        ] {
            let text = format!("{template}[tests.t]\nsteps = [{step}]\n");
            let err = load(&[("n.toml", &text)]).unwrap_err();
            assert!(err.message.contains(wanted), "{step}: {}", err.message);
        }
        let looping = "[templates.a]\nsteps = [{ use = 'b' }]\n[templates.b]\nsteps = [{ use = 'a' }]\n[tests.t]\nsteps = [{ use = 'a' }]\n";
        let err = load(&[("n.toml", looping)]).unwrap_err();
        assert!(err.message.contains("deep"), "{}", err.message);
    }

    #[test]
    fn matrices_expand_into_named_variants() {
        let config = load(&[(
            "n.toml",
            r#"
[tasks.image]
command = "make {{matrix.base}}.img"
matrix = { base = ["scaffold", "joe"] }

[tests.live]
deps = ["image"]
tags = ["vm-{{matrix.base}}"]
matrix.base = ["joe"]
matrix.media = [
  { name = "cd", device = "/dev/sr0" },
  { name = "usb", device = "/dev/sda" },
]
steps = [{ run = 'boot {{matrix.media}} from {{matrix.media.device}}', contains = "{{captured}}" }]
"#,
        )])
        .unwrap();
        let names: Vec<&str> = config
            .tasks
            .tasks()
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "image[base=scaffold]",
                "image[base=joe]",
                "live[base=joe,media=cd]",
                "live[base=joe,media=usb]"
            ]
        );
        assert_eq!(
            config.tasks.get("image[base=joe]").unwrap().command,
            "make joe.img"
        );
        let usb = config.tasks.get("live[base=joe,media=usb]").unwrap();
        assert_eq!(usb.deps, vec!["image[base=scaffold]", "image[base=joe]"]);
        assert_eq!(usb.tags, vec!["vm-joe"]);
        match &usb.steps[0].action {
            crate::steps::Action::Run { script, .. } => {
                assert_eq!(script, "boot usb from /dev/sda")
            }
            other => panic!("{other:?}"),
        }
        // Other placeholders wait for the run.
        assert_eq!(usb.steps[0].check.contains, vec!["{{captured}}"]);
        assert_eq!(usb.variant.as_ref().unwrap().base, "live");
    }

    #[test]
    fn matrix_mistakes() {
        for (matrix, wanted) in [
            ("matrix = { b = [] }", "no values"),
            ("matrix = { b = ['x', 'x'] }", "twice"),
            ("matrix = { b = [{ device = 'x' }] }", "needs a `name`"),
            ("matrix = { b = ['a,b'] }", "cannot name a variant"),
        ] {
            let text = format!("[tasks.t]\ncommand = 'x'\n{matrix}\n");
            let err = load(&[("n.toml", &text)]).unwrap_err();
            assert!(err.message.contains(wanted), "{matrix}: {}", err.message);
        }
        let err = load(&[(
            "n.toml",
            "[tasks.t]\ncommand = '{{matrix.nope}}'\nmatrix = { b = ['x'] }\n",
        )])
        .unwrap_err();
        assert!(
            err.message.contains("not one of this matrix's values"),
            "{}",
            err.message
        );
    }

    #[test]
    fn finds_placeholders_nothing_defines() {
        let config = load(&[(
            "n.toml",
            "[tests.t]\nsteps = [\n  { run = 'cat {{test.dir}}/x {{later}}' },\n  { run = 'v', capture = 'later' },\n  { run = 'echo {{later}}' },\n]\n[tasks.b]\ncommand = 'echo {{nope}}'\n",
        )])
        .unwrap();
        let problems = config.problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("n.toml:3: `t`: a step uses `{{later}}`")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("n.toml:7: `b`: its command uses `{{nope}}`")),
            "{problems:?}"
        );
    }

    #[test]
    fn rejects_self_dependencies() {
        let err = load(&[("n.toml", "[tasks.a]\ncommand='a'\ndeps=['a']\n")]).unwrap_err();
        assert!(err.message.contains("itself"), "{}", err.message);
    }
}
