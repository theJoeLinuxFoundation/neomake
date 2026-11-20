//! Reading steps and step templates.
//!
//! A step is an inline table with exactly one action key (`section`,
//! `run`, `host`, or `use` for a template) and, for steps that run a
//! command, the checks on its outcome. A template is a list of steps
//! with parameters, `[templates.NAME]`, which a step uses with
//! `{ use = "NAME", with = { param = value } }`; its steps are copied in
//! with `{{param}}` replaced, are reported at the `use` step's line, and
//! are named after the template.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use serde::Deserialize;
use toml::Spanned;

use super::{line_of, RawDuration};
use crate::error::ConfigError;
use crate::steps::{vars, Action, Capture, Check, ExitCheck, Respond, Step, StreamSel, Tty, Wait};

/// How deeply templates may use templates.
const MAX_TEMPLATE_DEPTH: usize = 8;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawStep {
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default, rename = "use")]
    use_template: Option<String>,
    #[serde(default)]
    with: BTreeMap<String, toml::Value>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    exit: Option<ExitValue>,
    #[serde(default)]
    stream: Option<String>,
    #[serde(default)]
    contains: Option<OneOrMany>,
    #[serde(default)]
    lacks: Option<OneOrMany>,
    #[serde(default)]
    matches: Option<OneOrMany>,
    #[serde(default)]
    equals: Option<String>,
    #[serde(default)]
    capture: Option<CaptureValue>,
    #[serde(default)]
    timeout: Option<RawDuration>,
    #[serde(default)]
    wait: Option<RawDuration>,
    #[serde(default)]
    interval: Option<RawDuration>,
    #[serde(default)]
    tty: Option<TtyValue>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ExitValue {
    Code(i64),
    Word(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub(super) fn into_vec(self) -> Vec<String> {
        match self {
            OneOrMany::One(s) => vec![s],
            OneOrMany::Many(v) => v,
        }
    }
}

/// `capture = "name"` (the whole output) or `capture = { name = 'regex' }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum CaptureValue {
    Whole(String),
    Patterns(IndexMap<String, String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum TtyValue {
    Flag(bool),
    Spec(RawTty),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTty {
    #[serde(default)]
    respond: Vec<RawRespond>,
    #[serde(default)]
    forbid: Option<OneOrMany>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRespond {
    on: String,
    send: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTemplate {
    #[serde(default)]
    params: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    steps: Vec<Spanned<RawStep>>,
}

/// A template, as read, with where it is defined.
pub(super) struct Template {
    params: Vec<String>,
    steps: Vec<Spanned<RawStep>>,
    file: PathBuf,
    text: String,
    line: usize,
}

/// Every template of the configuration.
#[derive(Default)]
pub(super) struct Templates {
    by_name: BTreeMap<String, Template>,
}

impl Templates {
    pub(super) fn add(
        &mut self,
        name: String,
        raw: Spanned<RawTemplate>,
        file: &Path,
        text: &str,
    ) -> Result<(), ConfigError> {
        let line = line_of(text, raw.span().start);
        if let Some(first) = self.by_name.get(&name) {
            return Err(ConfigError::new(
                file,
                Some(line),
                format!(
                    "template `{name}` is already defined at {}:{}",
                    first.file.display(),
                    first.line
                ),
            ));
        }
        let raw = raw.into_inner();
        self.by_name.insert(
            name,
            Template {
                params: raw.params,
                steps: raw.steps,
                file: file.to_path_buf(),
                text: text.to_string(),
                line,
            },
        );
        Ok(())
    }
}

/// Where steps are being read from, for errors.
pub(super) struct Origin<'a> {
    pub file: &'a Path,
    pub text: &'a str,
}

/// Read `raw` steps, expanding the templates they use.
pub(super) fn convert(
    raw: &[Spanned<RawStep>],
    origin: &Origin<'_>,
    templates: &Templates,
) -> Result<Vec<Step>, ConfigError> {
    let mut out = Vec::new();
    for step in raw {
        let line = line_of(origin.text, step.span().start);
        expand(step.get_ref(), line, origin, templates, 0, &mut out)?;
    }
    Ok(out)
}

fn expand(
    raw: &RawStep,
    line: usize,
    origin: &Origin<'_>,
    templates: &Templates,
    depth: usize,
    out: &mut Vec<Step>,
) -> Result<(), ConfigError> {
    let err = |msg: String| ConfigError::new(origin.file, Some(line), msg);
    let Some(name) = &raw.use_template else {
        out.push(convert_one(raw, line).map_err(err)?);
        return Ok(());
    };
    if let Some(key) = other_keys_than_use(raw) {
        return Err(err(format!(
            "a step that uses a template takes only `use` and `with`, not `{key}`"
        )));
    }
    let template = templates.by_name.get(name).ok_or_else(|| {
        let known: Vec<&str> = templates.by_name.keys().map(String::as_str).collect();
        err(match crate::executor::closest(name, known.into_iter()) {
            Some(s) => format!("no template named `{name}` (did you mean `{s}`?)"),
            None => format!("no template named `{name}`"),
        })
    })?;
    if depth >= MAX_TEMPLATE_DEPTH {
        return Err(err(format!(
            "templates use each other more than {MAX_TEMPLATE_DEPTH} deep (through `{name}`)"
        )));
    }
    let mut args = BTreeMap::new();
    for (key, value) in &raw.with {
        if !template.params.contains(key) {
            return Err(err(format!(
                "template `{name}` has no parameter `{key}` (it takes {})",
                if template.params.is_empty() {
                    "none".to_string()
                } else {
                    template.params.join(", ")
                }
            )));
        }
        args.insert(
            key.clone(),
            scalar_text(value).map_err(|m| err(format!("`with.{key}`: {m}")))?,
        );
    }
    if let Some(missing) = template.params.iter().find(|p| !args.contains_key(*p)) {
        return Err(err(format!(
            "template `{name}` needs `{missing}` in `with`"
        )));
    }

    let inner_origin = Origin {
        file: &template.file,
        text: &template.text,
    };
    let mut inner = Vec::new();
    for step in &template.steps {
        let inner_line = line_of(&template.text, step.span().start);
        expand(
            step.get_ref(),
            inner_line,
            &inner_origin,
            templates,
            depth + 1,
            &mut inner,
        )?;
    }
    for mut step in inner {
        for s in step.strings_mut() {
            *s = vars::substitute_known(s, &|p| args.get(p).cloned());
        }
        let label = format!("{name} › {}", step.describe());
        step.label = Some(label);
        // Reported where the template is used.
        step.line = Some(line);
        out.push(step);
    }
    Ok(())
}

fn other_keys_than_use(raw: &RawStep) -> Option<&'static str> {
    let set: &[(&str, bool)] = &[
        ("section", raw.section.is_some()),
        ("run", raw.run.is_some()),
        ("host", raw.host.is_some()),
        ("name", raw.name.is_some()),
        ("exit", raw.exit.is_some()),
        ("stream", raw.stream.is_some()),
        ("contains", raw.contains.is_some()),
        ("lacks", raw.lacks.is_some()),
        ("matches", raw.matches.is_some()),
        ("equals", raw.equals.is_some()),
        ("capture", raw.capture.is_some()),
        ("timeout", raw.timeout.is_some()),
        ("wait", raw.wait.is_some()),
        ("interval", raw.interval.is_some()),
        ("tty", raw.tty.is_some()),
    ];
    set.iter().find(|(_, present)| *present).map(|(k, _)| *k)
}

/// A value given to a template parameter, as text.
pub(super) fn scalar_text(value: &toml::Value) -> Result<String, String> {
    match value {
        toml::Value::String(s) => Ok(s.clone()),
        toml::Value::Integer(i) => Ok(i.to_string()),
        toml::Value::Float(f) => Ok(f.to_string()),
        toml::Value::Boolean(b) => Ok(b.to_string()),
        toml::Value::Datetime(d) => Ok(d.to_string()),
        _ => Err("must be a string, number or boolean".into()),
    }
}

fn convert_one(raw: &RawStep, line: usize) -> Result<Step, String> {
    let actions: Vec<&str> = [
        ("section", raw.section.is_some()),
        ("run", raw.run.is_some()),
        ("host", raw.host.is_some()),
    ]
    .iter()
    .filter(|(_, present)| *present)
    .map(|(k, _)| *k)
    .collect();
    let action_name = match actions.as_slice() {
        [one] => *one,
        [] => return Err("a step needs one of `run`, `host`, `section` or `use`".into()),
        many => {
            return Err(format!(
                "a step does one thing, but this one has {}",
                many.iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ))
        }
    };
    if !raw.with.is_empty() {
        return Err("`with` goes with `use`".into());
    }

    let tty = match &raw.tty {
        None | Some(TtyValue::Flag(false)) => None,
        Some(TtyValue::Flag(true)) => Some(Tty::default()),
        Some(TtyValue::Spec(spec)) => Some(Tty {
            respond: spec
                .respond
                .iter()
                .map(|r| Respond {
                    on: r.on.clone(),
                    send: r.send.clone(),
                })
                .collect(),
            forbid: spec
                .forbid
                .clone()
                .map(OneOrMany::into_vec)
                .unwrap_or_default(),
        }),
    };
    let action = match action_name {
        "section" => Action::Section(raw.section.clone().unwrap_or_default()),
        "run" => Action::Run {
            script: raw.run.clone().unwrap_or_default(),
            tty: tty.clone(),
        },
        _ => Action::Host {
            script: raw.host.clone().unwrap_or_default(),
            tty: tty.clone(),
        },
    };

    let check = Check {
        exit: match &raw.exit {
            None => ExitCheck::Zero,
            Some(ExitValue::Code(c)) => ExitCheck::Code(
                i32::try_from(*c).map_err(|_| format!("`exit = {c}` is not an exit status"))?,
            ),
            Some(ExitValue::Word(w)) => match w.as_str() {
                "nonzero" | "non-zero" => ExitCheck::Nonzero,
                "any" => ExitCheck::Any,
                other => {
                    return Err(format!(
                        "`exit` is a number, \"nonzero\" or \"any\", not {other:?}"
                    ))
                }
            },
        },
        stream: match raw.stream.as_deref() {
            None if tty.is_some() => StreamSel::Combined,
            None | Some("stdout") => StreamSel::Stdout,
            Some("stderr") => StreamSel::Stderr,
            Some("combined") | Some("both") => StreamSel::Combined,
            Some(other) => {
                return Err(format!(
                    "`stream` is \"stdout\", \"stderr\" or \"combined\", not {other:?}"
                ))
            }
        },
        contains: raw
            .contains
            .clone()
            .map(OneOrMany::into_vec)
            .unwrap_or_default(),
        lacks: raw
            .lacks
            .clone()
            .map(OneOrMany::into_vec)
            .unwrap_or_default(),
        matches: raw
            .matches
            .clone()
            .map(OneOrMany::into_vec)
            .unwrap_or_default(),
        equals: raw.equals.clone(),
        captures: match &raw.capture {
            None => Vec::new(),
            Some(CaptureValue::Whole(name)) => vec![Capture {
                name: name.clone(),
                pattern: None,
            }],
            Some(CaptureValue::Patterns(map)) => map
                .iter()
                .map(|(name, pattern)| Capture {
                    name: name.clone(),
                    pattern: Some(pattern.clone()),
                })
                .collect(),
        },
    };

    let runs = action.runs_a_command();
    if !runs
        && (!check.is_default() || tty.is_some() || raw.wait.is_some() || raw.timeout.is_some())
    {
        return Err(format!(
            "a `{action_name}` step runs nothing, so it has nothing to check"
        ));
    }
    if tty.is_some()
        && raw
            .stream
            .as_deref()
            .is_some_and(|s| s != "combined" && s != "both")
    {
        return Err("on a terminal, stdout and stderr are one stream".into());
    }
    for capture in &check.captures {
        if !valid_var_name(&capture.name) {
            return Err(format!(
                "`{}` cannot be captured into: a variable name is letters, digits, `_` and `-`",
                capture.name
            ));
        }
    }
    // Patterns are checked now, unless placeholders make them known only
    // when the step runs.
    let patterns = check
        .matches
        .iter()
        .chain(check.captures.iter().filter_map(|c| c.pattern.as_ref()))
        .chain(
            tty.iter()
                .flat_map(|t| t.respond.iter().map(|r| &r.on).chain(t.forbid.iter())),
        );
    for pattern in patterns {
        if vars::names(pattern).is_empty() {
            crate::steps::compile(pattern)?;
        }
    }

    let duration = |d: &Option<RawDuration>, what: &str| -> Result<Option<Duration>, String> {
        d.clone()
            .map(|d| d.parse())
            .transpose()
            .map_err(|e| format!("{what}: {e}"))
    };
    let wait = duration(&raw.wait, "wait")?;
    let interval = duration(&raw.interval, "interval")?;
    if interval.is_some() && wait.is_none() {
        return Err("`interval` goes with `wait`".into());
    }

    Ok(Step {
        line: Some(line),
        label: raw.name.clone(),
        action,
        check,
        timeout: duration(&raw.timeout, "timeout")?,
        wait: wait.map(|within| Wait {
            within,
            interval: interval.unwrap_or(Duration::from_secs(1)),
        }),
    })
}

/// A name a capture can set: letters, digits, `_` and `-`, starting with
/// a letter or `_`. (Names with dots belong to neomake: `test.dir`.)
pub(super) fn valid_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
