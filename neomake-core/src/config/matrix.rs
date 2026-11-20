//! Matrices: one definition, several variants.
//!
//! `matrix = { base = ["scaffold", "joe"], media = ["cd", "usb"] }`
//! turns a task or test into one variant per combination of values,
//! named `NAME[base=scaffold,media=cd]` and so on. In every string of
//! the definition, `{{matrix.base}}` is the variant's value. A value may
//! instead be a table with a `name` (which the variant is named by) and
//! further fields, reached as `{{matrix.media.dev}}`:
//!
//! ```toml
//! matrix.media = [
//!   { name = "cd", device = "/dev/sr0" },
//!   { name = "usb", device = "/dev/sda" },
//! ]
//! ```

use std::collections::BTreeMap;

use indexmap::IndexMap;

use super::steps::scalar_text;
use crate::steps::vars;
use crate::task::{Task, Variant};

/// One value of one matrix key.
#[derive(Debug, Clone)]
struct Value {
    /// How the variant's name shows it.
    name: String,
    /// `matrix.KEY` and `matrix.KEY.FIELD` and their values.
    fields: BTreeMap<String, String>,
}

/// The variants of `task` for `matrix`, or an error message.
pub(super) fn expand(
    task: &Task,
    matrix: &IndexMap<String, Vec<toml::Value>>,
) -> Result<Vec<Task>, String> {
    let mut keys: Vec<(String, Vec<Value>)> = Vec::new();
    for (key, values) in matrix {
        if !super::steps::valid_var_name(key) {
            return Err(format!(
                "`{key}` cannot be a matrix key: a key is letters, digits, `_` and `-`"
            ));
        }
        if values.is_empty() {
            return Err(format!("matrix key `{key}` has no values"));
        }
        let mut parsed = Vec::with_capacity(values.len());
        for value in values {
            parsed.push(read_value(key, value)?);
        }
        let mut seen = std::collections::BTreeSet::new();
        for v in &parsed {
            if !seen.insert(v.name.clone()) {
                return Err(format!("matrix key `{key}` lists `{}` twice", v.name));
            }
        }
        keys.push((key.clone(), parsed));
    }
    if keys.is_empty() {
        return Err("a matrix needs at least one key".into());
    }

    let mut combinations: Vec<Vec<(String, Value)>> = vec![Vec::new()];
    for (key, values) in &keys {
        combinations = combinations
            .into_iter()
            .flat_map(|combo| {
                values.iter().map(move |v| {
                    let mut c = combo.clone();
                    c.push((key.clone(), v.clone()));
                    c
                })
            })
            .collect();
    }

    let mut out = Vec::with_capacity(combinations.len());
    for combo in combinations {
        let fields: BTreeMap<String, String> =
            combo.iter().flat_map(|(_, v)| v.fields.clone()).collect();
        let mut variant = task.clone();
        let label: Vec<String> = combo
            .iter()
            .map(|(k, v)| format!("{k}={}", v.name))
            .collect();
        variant.name = format!("{}[{}]", task.name, label.join(","));
        let mut unknown = None;
        for_each_string(&mut variant, &mut |s| {
            *s = vars::substitute_known(s, &|name| fields.get(name).cloned());
            if unknown.is_none() {
                unknown = vars::names(s)
                    .into_iter()
                    .find(|n| n.starts_with("matrix."));
            }
        });
        if let Some(name) = unknown {
            return Err(format!(
                "`{{{{{name}}}}}` is not one of this matrix's values ({})",
                fields.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        variant.variant = Some(Variant {
            base: task.name.clone(),
            values: combo
                .iter()
                .map(|(k, v)| (k.clone(), v.name.clone()))
                .collect(),
        });
        out.push(variant);
    }
    Ok(out)
}

fn read_value(key: &str, value: &toml::Value) -> Result<Value, String> {
    let mut fields = BTreeMap::new();
    let name = match value {
        toml::Value::Table(table) => {
            let name = match table.get("name") {
                Some(toml::Value::String(n)) => n.clone(),
                _ => {
                    return Err(format!(
                        "a table among the values of matrix key `{key}` needs a `name`"
                    ))
                }
            };
            for (field, v) in table {
                let text = scalar_text(v).map_err(|m| format!("`matrix.{key}.{field}` {m}"))?;
                fields.insert(format!("matrix.{key}.{field}"), text);
            }
            name
        }
        other => scalar_text(other).map_err(|m| format!("a value of matrix key `{key}` {m}"))?,
    };
    if name.is_empty() || name.contains([',', '[', ']', '=']) {
        return Err(format!(
            "`{name}` cannot name a variant of matrix key `{key}`: it is empty or has `,`, `=`, `[` or `]`"
        ));
    }
    fields.insert(format!("matrix.{key}"), name.clone());
    Ok(Value { name, fields })
}

/// Call `f` on every string of `task` that a matrix value can appear in.
pub(super) fn for_each_string(task: &mut Task, f: &mut dyn FnMut(&mut String)) {
    f(&mut task.command);
    task.deps.iter_mut().for_each(&mut *f);
    task.inputs.iter_mut().for_each(&mut *f);
    task.outputs.iter_mut().for_each(&mut *f);
    let env = std::mem::take(&mut task.env);
    task.env = env
        .into_iter()
        .map(|(k, mut v)| {
            f(&mut v);
            (k, v)
        })
        .collect();
    if let Some(d) = &mut task.description {
        f(d);
    }
    task.tags.iter_mut().for_each(&mut *f);
    if let Some(c) = &mut task.cwd {
        f(c);
    }
    task.requires.commands.iter_mut().for_each(&mut *f);
    task.requires.files.iter_mut().for_each(&mut *f);
    task.requires.env.iter_mut().for_each(&mut *f);
    if let Some(s) = &mut task.skip {
        f(s);
    }
    if let Some(shell) = &mut task.shell {
        shell.iter_mut().for_each(&mut *f);
    }
    task.exclusive.iter_mut().for_each(&mut *f);
    if let Some(p) = &mut task.prelude {
        f(p);
    }
    for step in &mut task.steps {
        for s in step.strings_mut() {
            f(s);
        }
    }
}
