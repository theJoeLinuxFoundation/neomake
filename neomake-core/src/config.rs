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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::from(".neomake"),
            default_timeout: None,
            keep_runs: 20,
            shell: None,
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
}

impl Config {
    /// Build a configuration from `(path, toml_text)` documents, the
    /// top-level one first.
    pub fn from_sources(root: &Path, sources: &[(PathBuf, String)]) -> Result<Self, ConfigError> {
        let mut settings = Settings::default();
        let mut tasks: Vec<Task> = Vec::new();
        let mut defined_in: BTreeMap<String, Location> = BTreeMap::new();

        for (index, (path, text)) in sources.iter().enumerate() {
            let raw: RawConfig = toml::from_str(text).map_err(|e| {
                let line = e.span().map(|s| line_of(text, s.start));
                ConfigError::new(path, line, e.message().trim_end().to_string())
            })?;

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
                let task = convert_task(name.clone(), kind, raw_task.into_inner(), &location)?;
                defined_in.insert(name, location);
                tasks.push(task);
            }
        }

        Ok(Self {
            root: root.to_path_buf(),
            settings,
            tasks: TaskSet::new(tasks),
        })
    }
}

fn line_of(text: &str, offset: usize) -> usize {
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
    })
}

fn convert_task(
    name: String,
    kind: TaskKind,
    raw: RawTask,
    at: &Location,
) -> Result<Task, ConfigError> {
    let err = |msg: String| ConfigError::new(&at.file, Some(at.line), msg);
    if name.is_empty() {
        return Err(err("a task name cannot be empty".into()));
    }
    if raw.command.trim().is_empty() {
        return Err(err(format!("task `{name}` has an empty `command`")));
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
        command: raw.command,
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTask {
    command: String,
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
#[derive(Debug, Deserialize)]
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
enum ShellValue {
    Line(String),
    Argv(Vec<String>),
}

impl ShellValue {
    fn into_argv(self) -> Vec<String> {
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
    fn rejects_self_dependencies() {
        let err = load(&[("n.toml", "[tasks.a]\ncommand='a'\ndeps=['a']\n")]).unwrap_err();
        assert!(err.message.contains("itself"), "{}", err.message);
    }
}
