//! Loading TOMLX documents and evaluating their expressions.
//!
//! [`load_documents`] reads a configuration file and every file it
//! includes, turns each into plain TOML with the scanner (`scanner.rs`),
//! and returns them as [`Document`]s. [`load`] merges those into one
//! [`toml::Value`].
//!
//! # Includes
//!
//! `include = "file"` or `include = ["pattern", ...]`, written before
//! the first table header, loads further files. Paths and patterns are
//! relative to the project root (the directory of the top-level file),
//! whichever file includes them. An included file sees the variables
//! declared before its `include` line; the variables it declares stay
//! its own. A file is loaded once however many times it is included.
//! The documents' tables are merged, and a key that two documents both
//! define is an error naming both.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::builtins;
use crate::error::TomlxError;
use crate::lexer::Span;
use crate::parser::{BinOp, Expr, StringPart, UnaryOp};
use crate::scanner;

/// The variables in scope while a document is evaluated.
#[derive(Debug, Clone)]
pub(crate) struct Scope {
    pub vars: BTreeMap<String, toml::Value>,
    /// The project root, which relative paths are resolved against.
    pub root: PathBuf,
}

impl Scope {
    fn new(root: PathBuf) -> Self {
        Self {
            vars: BTreeMap::new(),
            root,
        }
    }
}

/// One configuration file, evaluated to plain TOML.
#[derive(Debug, Clone)]
pub struct Document {
    /// The file it was read from.
    pub path: PathBuf,
    /// Its content as TOML, with the same line numbering as the source.
    pub text: String,
}

/// A configuration file and everything it includes.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The project root: the directory of the top-level file.
    pub root: PathBuf,
    /// The top-level document first, then the included ones in the order
    /// they were included.
    pub documents: Vec<Document>,
}

impl Loaded {
    /// Every document parsed and merged into one value.
    pub fn merged(&self) -> Result<toml::Value, TomlxError> {
        let mut merged = toml::Table::new();
        let mut origins: HashMap<String, PathBuf> = HashMap::new();
        for doc in &self.documents {
            let table = parse_plain_toml(&doc.text, &doc.path)?;
            merge_into(&mut merged, table, "", &doc.path, &mut origins)?;
        }
        Ok(toml::Value::Table(merged))
    }
}

/// Load `path` and every file it includes.
pub fn load_documents(path: &Path) -> Result<Loaded, TomlxError> {
    let source = read(path)?;
    load_documents_from(&source, path)
}

/// Like [`load_documents`], with the top-level file's text given.
pub fn load_documents_from(source: &str, path: &Path) -> Result<Loaded, TomlxError> {
    let root = neomake_core::project_root_for(path);
    let mut loaded = Loaded {
        root: root.clone(),
        documents: Vec::new(),
    };
    let mut seen = HashSet::new();
    seen.insert(identity(path));
    load_one(
        source,
        path,
        Scope::new(root),
        &mut loaded.documents,
        &mut seen,
    )?;
    Ok(loaded)
}

/// Evaluate a TOMLX source string into a fully-materialized [`toml::Value`].
///
/// `path` locates the document for diagnostics and gives the project
/// root that relative paths and includes are resolved against.
pub fn load_str(source: &str, path: &Path) -> Result<toml::Value, TomlxError> {
    load_documents_from(source, path)?.merged()
}

/// Read a file from disk, with everything it includes, and evaluate it.
pub fn load(path: &Path) -> Result<toml::Value, TomlxError> {
    load_documents(path)?.merged()
}

fn read(path: &Path) -> Result<String, TomlxError> {
    std::fs::read_to_string(path)
        .map_err(|e| TomlxError::at_unknown(path, format!("cannot read `{}`: {e}", path.display())))
}

fn identity(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn load_one(
    source: &str,
    path: &Path,
    mut scope: Scope,
    documents: &mut Vec<Document>,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), TomlxError> {
    let scanned = scanner::scan(source, path, &mut scope)?;
    documents.push(Document {
        path: path.to_path_buf(),
        text: scanned.text,
    });
    for include in scanned.includes {
        for pattern in &include.patterns {
            let root = &include.scope.root;
            let files = neomake_core::glob::expand(root, pattern, &[])
                .map_err(|e| include.span.error(e))?;
            if files.is_empty() && !neomake_core::glob::is_pattern(pattern) {
                return Err(include
                    .span
                    .error(format!("included file `{pattern}` does not exist")));
            }
            for file in files {
                let file_path = if root == Path::new(".") {
                    PathBuf::from(&file)
                } else {
                    root.join(&file)
                };
                if !seen.insert(identity(&file_path)) {
                    continue;
                }
                let text = read(&file_path)?;
                load_one(&text, &file_path, include.scope.clone(), documents, seen)?;
            }
        }
    }
    Ok(())
}

fn parse_plain_toml(source: &str, path: &Path) -> Result<toml::Table, TomlxError> {
    toml::from_str(source).map_err(|e| {
        let (line, col) = e
            .span()
            .map(|span| line_col(source, span.start))
            .unwrap_or((0, 0));
        TomlxError::new(path, line, col, e.message().to_string())
    })
}

fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[line_start..].chars().count() + 1)
}

/// Merge `src`, read from `doc`, into `dst`. `origins` remembers which
/// document first defined each key path, for the conflict message.
fn merge_into(
    dst: &mut toml::Table,
    src: toml::Table,
    prefix: &str,
    doc: &Path,
    origins: &mut HashMap<String, PathBuf>,
) -> Result<(), TomlxError> {
    for (key, value) in src {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match dst.get_mut(&key) {
            None => {
                origins.insert(path, doc.to_path_buf());
                dst.insert(key, value);
            }
            Some(toml::Value::Table(existing)) if value.is_table() => {
                let toml::Value::Table(incoming) = value else {
                    unreachable!("checked by the guard")
                };
                merge_into(existing, incoming, &path, doc, origins)?;
            }
            Some(_) => {
                let first = origin_of(&path, origins).unwrap_or_else(|| doc.to_path_buf());
                return Err(TomlxError::at_unknown(
                    doc,
                    format!("`{path}` is defined both here and in {}", first.display()),
                ));
            }
        }
    }
    Ok(())
}

fn origin_of(path: &str, origins: &HashMap<String, PathBuf>) -> Option<PathBuf> {
    let mut candidate = path;
    loop {
        if let Some(origin) = origins.get(candidate) {
            return Some(origin.clone());
        }
        candidate = &candidate[..candidate.rfind('.')?];
    }
}

// -----------------------------------------------------------------------
// Expression evaluation.
// -----------------------------------------------------------------------

/// Evaluate `expr`. `at` locates errors that have no closer location,
/// such as an operator applied to the wrong types.
pub(crate) fn evaluate(expr: &Expr, scope: &Scope, at: &Span) -> Result<toml::Value, TomlxError> {
    match expr {
        Expr::Int(n) => Ok(toml::Value::Integer(*n)),
        Expr::Float(f) => Ok(toml::Value::Float(*f)),
        Expr::Bool(b) => Ok(toml::Value::Boolean(*b)),
        Expr::LitStr(s) => Ok(toml::Value::String(s.clone())),
        Expr::TemplateStr(parts) => {
            let mut buf = String::new();
            for part in parts {
                match part {
                    StringPart::Lit(s) => buf.push_str(s),
                    StringPart::Interp(e) => {
                        let v = evaluate(e, scope, at)?;
                        buf.push_str(&stringify_value(&v));
                    }
                }
            }
            Ok(toml::Value::String(buf))
        }
        Expr::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(evaluate(item, scope, at)?);
            }
            Ok(toml::Value::Array(out))
        }
        Expr::Table(entries) => {
            let mut table = toml::Table::new();
            for (key, value) in entries {
                let value = evaluate(value, scope, at)?;
                insert_dotted(&mut table, key, value).map_err(|msg| at.error(msg))?;
            }
            Ok(toml::Value::Table(table))
        }
        Expr::Var { name, span } => scope
            .vars
            .get(name)
            .cloned()
            .ok_or_else(|| span.error(format!("undefined variable `{name}`"))),
        Expr::Call { name, args, span } => call_builtin(name, args, scope, span),
        Expr::Binary { op, lhs, rhs } => {
            let l = evaluate(lhs, scope, at)?;
            // `&&` and `||` short-circuit.
            match (op, &l) {
                (BinOp::And, toml::Value::Boolean(false)) => return Ok(l),
                (BinOp::Or, toml::Value::Boolean(true)) => return Ok(l),
                _ => {}
            }
            let r = evaluate(rhs, scope, at)?;
            apply_binop(*op, &l, &r).map_err(|msg| at.error(msg))
        }
        Expr::Unary { op, expr } => {
            let v = evaluate(expr, scope, at)?;
            apply_unary(*op, &v).map_err(|msg| at.error(msg))
        }
        Expr::KeywordArg { .. } => {
            Err(at.error("keyword arguments are only valid inside function calls"))
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => match evaluate(cond, scope, at)? {
            toml::Value::Boolean(true) => evaluate(then_branch, scope, at),
            toml::Value::Boolean(false) => evaluate(else_branch, scope, at),
            other => Err(at.error(format!(
                "an if condition must be a boolean, got {}",
                type_of(&other)
            ))),
        },
    }
}

fn insert_dotted(
    table: &mut toml::Table,
    key: &[String],
    value: toml::Value,
) -> Result<(), String> {
    let (last, parents) = key.split_last().expect("a key has at least one part");
    let mut current = table;
    for part in parents {
        let entry = current
            .entry(part.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        current = match entry {
            toml::Value::Table(t) => t,
            _ => return Err(format!("`{}` is not a table", key.join("."))),
        };
    }
    if current.insert(last.clone(), value).is_some() {
        return Err(format!("`{}` is given twice", key.join(".")));
    }
    Ok(())
}

fn call_builtin(
    name: &str,
    args: &[Expr],
    scope: &Scope,
    span: &Span,
) -> Result<toml::Value, TomlxError> {
    let builtin = builtins::lookup(name).ok_or_else(|| {
        let known: Vec<&str> = builtins::BUILTINS.iter().map(|b| b.name).collect();
        span.error(format!(
            "unknown function `{name}` (the built-ins are {})",
            known.join(", ")
        ))
    })?;
    let mut positional = Vec::new();
    let mut kwargs = BTreeMap::new();
    for arg in args {
        match arg {
            Expr::KeywordArg { name, value } => {
                kwargs.insert(name.clone(), evaluate(value, scope, span)?);
            }
            other => positional.push(evaluate(other, scope, span)?),
        }
    }
    (builtin.call)(&builtins::Args {
        name,
        positional,
        kwargs,
        span,
        scope,
    })
}

fn apply_binop(op: BinOp, l: &toml::Value, r: &toml::Value) -> Result<toml::Value, String> {
    use toml::Value as V;
    match op {
        BinOp::Eq => Ok(V::Boolean(values_equal(l, r))),
        BinOp::Ne => Ok(V::Boolean(!values_equal(l, r))),
        BinOp::And => match (l, r) {
            (V::Boolean(a), V::Boolean(b)) => Ok(V::Boolean(*a && *b)),
            _ => Err(format!(
                "&& requires booleans, got {} and {}",
                type_of(l),
                type_of(r)
            )),
        },
        BinOp::Or => match (l, r) {
            (V::Boolean(a), V::Boolean(b)) => Ok(V::Boolean(*a || *b)),
            _ => Err(format!(
                "|| requires booleans, got {} and {}",
                type_of(l),
                type_of(r)
            )),
        },
        BinOp::Plus => match (l, r) {
            (V::Integer(a), V::Integer(b)) => a
                .checked_add(*b)
                .map(V::Integer)
                .ok_or_else(|| "integer overflow in `+`".to_string()),
            (V::Float(a), V::Float(b)) => Ok(V::Float(a + b)),
            (V::String(a), V::String(b)) => Ok(V::String(format!("{a}{b}"))),
            (V::Array(a), V::Array(b)) => {
                let mut out = a.clone();
                out.extend(b.iter().cloned());
                Ok(V::Array(out))
            }
            _ => Err(format!(
                "cannot apply `+` to {} and {}",
                type_of(l),
                type_of(r)
            )),
        },
    }
}

fn apply_unary(op: UnaryOp, v: &toml::Value) -> Result<toml::Value, String> {
    use toml::Value as V;
    match op {
        UnaryOp::Not => match v {
            V::Boolean(b) => Ok(V::Boolean(!*b)),
            _ => Err(format!("`!` requires a boolean, got {}", type_of(v))),
        },
        UnaryOp::Neg => match v {
            V::Integer(n) => Ok(V::Integer(-*n)),
            V::Float(f) => Ok(V::Float(-*f)),
            _ => Err(format!("unary `-` requires a number, got {}", type_of(v))),
        },
    }
}

fn values_equal(a: &toml::Value, b: &toml::Value) -> bool {
    use toml::Value as V;
    match (a, b) {
        (V::String(x), V::String(y)) => x == y,
        (V::Integer(x), V::Integer(y)) => x == y,
        (V::Float(x), V::Float(y)) => x == y,
        (V::Boolean(x), V::Boolean(y)) => x == y,
        (V::Datetime(x), V::Datetime(y)) => x == y,
        (V::Array(x), V::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| values_equal(p, q))
        }
        (V::Table(x), V::Table(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| values_equal(v, w)))
        }
        _ => false,
    }
}

fn stringify_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(dt) => dt.to_string(),
        toml::Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(stringify_value).collect();
            parts.join(",")
        }
        toml::Value::Table(tbl) => render_toml_value(&toml::Value::Table(tbl.clone())),
    }
}

/// The name of a value's type, as diagnostics give it.
pub(crate) fn type_of(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "bool",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

// -----------------------------------------------------------------------
// TOML value renderer (value → syntax).
// -----------------------------------------------------------------------

/// Render a [`toml::Value`] as an inline TOML literal on one line.
///
/// Used to substitute evaluated TOMLX expressions back into the
/// scanned document. Strings are rendered as basic strings with every
/// control character escaped, tables as inline tables.
pub fn render_toml_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => render_string(s),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => {
            if f.is_nan() {
                "nan".into()
            } else if f.is_infinite() {
                if *f > 0.0 { "inf" } else { "-inf" }.into()
            } else if f.fract() == 0.0 {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(dt) => dt.to_string(),
        toml::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(render_toml_value).collect();
            format!("[{}]", parts.join(", "))
        }
        toml::Value::Table(tbl) => {
            if tbl.is_empty() {
                return "{}".into();
            }
            let parts: Vec<String> = tbl
                .iter()
                .map(|(k, v)| format!("{} = {}", render_key(k), render_toml_value(v)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
    }
}

fn render_key(k: &str) -> String {
    if !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        k.to_string()
    } else {
        render_string(k)
    }
}

fn render_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn load_s(src: &str) -> Result<toml::Value, TomlxError> {
        load_str(src, &PathBuf::from("test.tomlx"))
    }

    fn get<'a>(v: &'a toml::Value, path: &str) -> &'a toml::Value {
        path.split('.').fold(v, |v, k| {
            v.get(k).unwrap_or_else(|| panic!("no `{k}` in {v:?}"))
        })
    }

    fn get_str<'a>(v: &'a toml::Value, path: &str) -> &'a str {
        get(v, path).as_str().unwrap()
    }

    #[test]
    fn plain_toml_degrades_cleanly() {
        let src = r#"
[tasks.build]
command = "cargo build"
deps = ["codegen"]
"#;
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "tasks.build.command"), "cargo build");
    }

    #[test]
    fn plain_toml_passes_through_verbatim() {
        let src = "# a comment\n[a]\nx = 1 # trailing\ny = [\n  \"p\", # one\n  'q',\n]\nz = { k = 'v' }\nw = 1979-05-27T07:32:00Z\n";
        let loaded = load_documents_from(src, Path::new("t.toml")).unwrap();
        assert_eq!(loaded.documents[0].text, src);
    }

    #[test]
    fn variable_substitution() {
        let src = r#"
$profile = "release"

[tasks.build]
command = "cargo build --${profile}"
"#;
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "tasks.build.command"), "cargo build --release");
    }

    #[test]
    fn conditional_value() {
        let src = r#"
$ci = true

[tasks.test]
command = if ${ci} { "make ci-test" } else { "make test" }
"#;
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "tasks.test.command"), "make ci-test");
    }

    #[test]
    fn conditional_else_branch() {
        let src = r#"
$ci = false

[tasks.test]
command = if ${ci} { "make ci-test" } else { "make test" }
"#;
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "tasks.test.command"), "make test");
    }

    #[test]
    fn conditionals_may_span_lines_and_chain() {
        let src = r#"
$level = "b"
[t]
v = if level == "a" {
  1
} else if level == "b" {
  2
} else {
  3
}
after = 4
"#;
        let v = load_s(src).unwrap();
        assert_eq!(get(&v, "t.v").as_integer(), Some(2));
        assert_eq!(get(&v, "t.after").as_integer(), Some(4));
    }

    #[test]
    fn env_function_with_default() {
        let src = r#"
$who = env("NEOMAKE_TEST_UNLIKELY_VAR_X9", "nobody")

[who]
name = ${who}
"#;
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "who.name"), "nobody");
    }

    #[test]
    fn calls_without_a_dollar_are_evaluated() {
        let src = "[w]\nname = env(\"NEOMAKE_TEST_UNLIKELY_VAR_X9\", \"nobody\")\n";
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "w.name"), "nobody");
    }

    #[test]
    fn string_concatenation() {
        let src = r#"
$suffix = "world"

[m]
msg = "hello " + ${suffix}
"#;
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "m.msg"), "hello world");
    }

    #[test]
    fn undefined_variable_errors_with_its_location() {
        let src = "[x]\ny = ${nope}\n";
        let err = load_s(src).unwrap_err();
        assert!(
            err.message.contains("undefined variable"),
            "{}",
            err.message
        );
        assert_eq!((err.line, err.col), (2, 5));
    }

    #[test]
    fn array_with_interpolation() {
        let src = r#"
$name = "foo"

[t]
files = ["a", "${name}.rs", "b"]
"#;
        let v = load_s(src).unwrap();
        assert_eq!(
            get(&v, "t.files").as_array().unwrap()[1].as_str(),
            Some("foo.rs")
        );
    }

    #[test]
    fn interpolates_inside_multiline_arrays() {
        let src =
            "$name = \"foo\"\n[t]\nfiles = [\n  \"a\", # first\n  \"${name}.rs\",\n]\nnext = 1\n";
        let loaded = load_documents_from(src, Path::new("t.tomlx")).unwrap();
        // Line numbers are kept: `next` is still on line 7.
        assert_eq!(loaded.documents[0].text.lines().nth(6), Some("next = 1"));
        let v = loaded.merged().unwrap();
        let files: Vec<&str> = get(&v, "t.files")
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        assert_eq!(files, vec!["a", "foo.rs"]);
    }

    #[test]
    fn inline_tables_hold_expressions() {
        let src = "$x = \"1\"\n[t]\nenv = { A = \"${x}\", B.C = 'lit' }\n";
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "t.env.A"), "1");
        assert_eq!(get_str(&v, "t.env.B.C"), "lit");
    }

    #[test]
    fn multiline_literal_strings_are_left_alone() {
        // A shell script whose lines look like declarations, statements
        // and interpolations must reach TOML exactly as written.
        let script = "$x = 1\ngit(this is not a call)\necho \"${HOME}\" $?\ninclude = nothing\n";
        let src = format!("$v = 2\n[t]\nrun = '''\n{script}'''\nafter = ${{v}}\n");
        let v = load_s(&src).unwrap();
        assert_eq!(get_str(&v, "t.run"), script);
        assert_eq!(get(&v, "t.after").as_integer(), Some(2));
    }

    #[test]
    fn multiline_basic_strings_interpolate() {
        let src = "$who = \"joe\"\n[t]\nmsg = \"\"\"\nhello ${who}\n$HOME stays\n\"\"\"\n";
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "t.msg"), "hello joe\n$HOME stays\n");
    }

    #[test]
    fn arrays_of_inline_tables_pass_through() {
        let src = "[t]\nsteps = [\n  { run = 'echo ${X}', contains = \"a\" },\n  { run = '''\nmulti\n''' },\n]\n";
        let v = load_s(src).unwrap();
        let steps = get(&v, "t.steps").as_array().unwrap();
        assert_eq!(steps[0].get("run").unwrap().as_str(), Some("echo ${X}"));
        assert_eq!(steps[1].get("run").unwrap().as_str(), Some("multi\n"));
    }

    #[test]
    fn keeps_non_ascii_text() {
        let src = "$x = \"é\"\n[t]\ns = \"naïve ${x} — ✓\"\n";
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "t.s"), "naïve é — ✓");
    }

    #[test]
    fn statements_run_for_their_effect_only() {
        // `fetch` of an unreachable URL fails: proof the statement ran.
        let src = "[t]\nfetch('http://127.0.0.1:1/nothing', 'x')\nk = 1\n";
        let err = load_s(src).unwrap_err();
        assert!(err.message.contains("fetch()"), "{}", err.message);
        assert_eq!(err.line, 2);
    }

    #[test]
    fn rejects_unknown_keyword_arguments() {
        let src = "[t]\nk = glob('*', recursive = true)\n";
        let err = load_s(src).unwrap_err();
        assert!(err.message.contains("keyword"), "{}", err.message);
    }

    #[test]
    fn reports_invalid_plain_toml_at_its_line() {
        let src = "[t]\na = 1\nb = \"x\" \"y\"\n";
        let err = load_s(src).unwrap_err();
        assert_eq!(err.line, 3, "{err}");
    }

    #[test]
    fn reports_unterminated_strings_where_they_start() {
        let err = load_s("[t]\na = '''\nnever closed\n").unwrap_err();
        assert_eq!((err.line, err.col), (2, 5), "{err}");
    }

    #[test]
    fn path_builtins() {
        let src = r#"
[p]
base = basename("a/b/c.txt")
dir = dirname("a/b/c.txt")
top = dirname("c.txt")
joined = join("a", "b/", "c")
absolute = join("a", "/b")
replaced = replace("a-b-c", "-", "/")
here = exists("does-not-exist-anywhere")
sh = which("definitely-not-a-program-x9") == ""
"#;
        let v = load_s(src).unwrap();
        assert_eq!(get_str(&v, "p.base"), "c.txt");
        assert_eq!(get_str(&v, "p.dir"), "a/b");
        assert_eq!(get_str(&v, "p.top"), ".");
        assert_eq!(get_str(&v, "p.joined"), "a/b/c");
        assert_eq!(get_str(&v, "p.absolute"), "/b");
        assert_eq!(get_str(&v, "p.replaced"), "a/b/c");
        assert_eq!(get(&v, "p.here").as_bool(), Some(false));
        assert_eq!(get(&v, "p.sh").as_bool(), Some(true));
    }

    #[test]
    fn includes_merge_and_see_earlier_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(
            root.join("neomake.tomlx"),
            "$dave = \"bin/dave\"\ninclude = [\"tests/*.tomlx\"]\n[tasks.a]\ncommand = \"a\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tests/one.tomlx"),
            "[tasks.b]\ncommand = \"${dave} b\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tests/two.tomlx"),
            "include = 'tests/one.tomlx'\n[tasks.c]\ncommand = 'c'\n",
        )
        .unwrap();
        let loaded = load_documents(&root.join("neomake.tomlx")).unwrap();
        assert_eq!(loaded.documents.len(), 3, "one.tomlx is loaded once");
        let v = loaded.merged().unwrap();
        assert_eq!(get_str(&v, "tasks.a.command"), "a");
        assert_eq!(get_str(&v, "tasks.b.command"), "bin/dave b");
        assert_eq!(get_str(&v, "tasks.c.command"), "c");
        assert!(v.get("include").is_none());
    }

    #[test]
    fn include_conflicts_name_both_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("neomake.toml"),
            "include = 'more.toml'\n[tasks.a]\ncommand = 'a'\n",
        )
        .unwrap();
        std::fs::write(root.join("more.toml"), "[tasks.a]\ncommand = 'b'\n").unwrap();
        let err = load(&root.join("neomake.toml")).unwrap_err();
        assert!(err.message.contains("tasks.a.command"), "{}", err.message);
        assert!(err.message.contains("neomake.toml"), "{}", err.message);
        assert!(err.file.ends_with("more.toml"));
    }

    #[test]
    fn missing_include_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("neomake.toml");
        std::fs::write(&path, "include = 'nope.toml'\n").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.message.contains("does not exist"), "{}", err.message);
        assert_eq!(err.line, 1);
    }

    #[test]
    fn bare_config_paths_have_the_current_directory_as_root() {
        let scope = Scope::new(neomake_core::project_root_for(Path::new("neomake.tomlx")));
        assert_eq!(scope.root, PathBuf::from("."));
        let v = load_s("[t]\nout = exec('echo hi')\n").unwrap();
        assert_eq!(get_str(&v, "t.out"), "hi");
    }

    #[test]
    fn render_toml_value_roundtrips() {
        let v = toml::Value::Array(vec![
            toml::Value::String("a".into()),
            toml::Value::Integer(42),
            toml::Value::Boolean(true),
        ]);
        let rendered = render_toml_value(&v);
        assert_eq!(rendered, r#"["a", 42, true]"#);
    }
}
