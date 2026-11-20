//! The document scanner: turns a TOMLX document into a TOML document.
//!
//! The scanner walks the source statement by statement, the way a TOML
//! parser does, so it always knows whether it is at the start of a line
//! of the document or inside a value. A value's full extent is found
//! before anything is decided about it: a value continues over newlines
//! while it is inside a string (of any of TOML's four kinds), an array,
//! an inline table, parentheses or the braces of an `if`. Text inside a
//! value is therefore never mistaken for a declaration or a statement,
//! which matters for the shell scripts that tests hold in `'''…'''`.
//!
//! Each statement is one of:
//!
//! - a comment, a blank line or a table header, copied as written;
//! - `$name = expr`, a variable declaration, evaluated and replaced by
//!   blank lines;
//! - `name(...)` where `name` is a built-in usable as a statement (such
//!   as `git`), evaluated for its effect and replaced by blank lines;
//! - `include = [...]` before the first table header, recorded for the
//!   loader and replaced by blank lines;
//! - `key = value`. A value that is plain TOML, with no `$` outside a
//!   string and no `${` inside a basic string, is copied as written;
//!   anything else is evaluated as a TOMLX expression and rendered back
//!   as a TOML literal on the key's line.
//!
//! Replacements keep the number of lines, so the TOML parser's line
//! numbers, and those of every diagnostic after it, are the source's.

use std::path::{Path, PathBuf};

use toml::Value;

use crate::builtins;
use crate::error::TomlxError;
use crate::evaluator::{evaluate, render_toml_value, Scope};
use crate::lexer::{tokenize, Span};
use crate::parser::parse_expr_at;

/// The result of scanning one document.
pub(crate) struct Scanned {
    /// The document as plain TOML, line for line.
    pub text: String,
    /// The `include` statements found, in order.
    pub includes: Vec<Include>,
}

/// One `include = ...` statement.
pub(crate) struct Include {
    /// The patterns it names, relative to the project root.
    pub patterns: Vec<String>,
    /// Where it is written.
    pub span: Span,
    /// The variables in scope at that point, which the included files see.
    pub scope: Scope,
}

/// Scan `source`, read from `path`, updating `scope` with each variable
/// it declares.
pub(crate) fn scan(source: &str, path: &Path, scope: &mut Scope) -> Result<Scanned, TomlxError> {
    let mut scanner = Scanner {
        src: source,
        bytes: source.as_bytes(),
        file: path.to_path_buf(),
        line_starts: line_starts(source),
        out: String::with_capacity(source.len()),
        includes: Vec::new(),
    };
    scanner.run(scope)?;
    Ok(Scanned {
        text: scanner.out,
        includes: scanner.includes,
    })
}

/// What a skim of one value found.
struct Skim {
    /// Offset one past the value's last byte (at a newline, a comment or
    /// the end of the source).
    end: usize,
    /// A `$` outside any string.
    dollar: bool,
    /// A `${` inside a basic string.
    interpolates: bool,
}

struct Scanner<'a> {
    src: &'a str,
    bytes: &'a [u8],
    file: PathBuf,
    line_starts: Vec<usize>,
    out: String,
    includes: Vec<Include>,
}

impl Scanner<'_> {
    fn run(&mut self, scope: &mut Scope) -> Result<(), TomlxError> {
        let mut pos = 0;
        let mut at_root = true;
        while pos < self.bytes.len() {
            let start = pos;
            let first = skip_blanks(self.bytes, pos);
            match self.bytes.get(first) {
                None | Some(b'\n' | b'\r' | b'#') => {
                    pos = self.copy_line(start);
                }
                Some(b'[') => {
                    at_root = false;
                    pos = self.copy_line(start);
                }
                Some(b'$')
                    if self
                        .bytes
                        .get(first + 1)
                        .is_some_and(|&b| is_ident_start(b)) =>
                {
                    pos = self.declaration(start, first, scope)?;
                }
                Some(&b) if is_ident_start(b) && self.is_statement(first) => {
                    pos = self.statement(start, first, scope)?;
                }
                Some(_) => {
                    pos = self.key_value(start, first, at_root, scope)?;
                }
            }
        }
        Ok(())
    }

    /// `$name = expr`
    fn declaration(
        &mut self,
        start: usize,
        dollar: usize,
        scope: &mut Scope,
    ) -> Result<usize, TomlxError> {
        let name_end = ident_end(self.bytes, dollar + 1);
        let name = self.src[dollar + 1..name_end].to_string();
        let eq = skip_blanks(self.bytes, name_end);
        if self.bytes.get(eq) != Some(&b'=') {
            return Err(self
                .span(dollar)
                .error(format!("a variable is declared as `${name} = value`")));
        }
        let value_start = skip_blanks(self.bytes, eq + 1);
        let skim = self.skim(value_start)?;
        let value = self.value_of(value_start, &skim, scope)?;
        scope.vars.insert(name, value);
        Ok(self.blank_out(start, skim.end))
    }

    /// A bare call of a built-in such as `git(...)`, kept for its effect.
    fn statement(
        &mut self,
        start: usize,
        call: usize,
        scope: &mut Scope,
    ) -> Result<usize, TomlxError> {
        let skim = self.skim(call)?;
        let text = self.src[call..skim.end].trim_end();
        self.evaluate_text(text, call, scope)?;
        Ok(self.blank_out(start, skim.end))
    }

    fn key_value(
        &mut self,
        start: usize,
        key_start: usize,
        at_root: bool,
        scope: &mut Scope,
    ) -> Result<usize, TomlxError> {
        let Some(eq) = find_key_end(self.bytes, key_start) else {
            // Not a key/value line: leave it for the TOML parser to report.
            return Ok(self.copy_line(start));
        };
        let value_start = skip_blanks(self.bytes, eq + 1);
        if matches!(
            self.bytes.get(value_start),
            None | Some(b'\n' | b'\r' | b'#')
        ) {
            return Ok(self.copy_line(start));
        }
        let skim = self.skim(value_start)?;
        let key = self.src[key_start..eq].trim();

        if at_root && key == "include" {
            let value = self.value_of(value_start, &skim, scope)?;
            let patterns = match value {
                Value::String(s) => vec![s],
                Value::Array(items) => items
                    .into_iter()
                    .map(|v| match v {
                        Value::String(s) => Ok(s),
                        other => Err(self.span(value_start).error(format!(
                            "`include` lists file names, not {}",
                            crate::evaluator::type_of(&other)
                        ))),
                    })
                    .collect::<Result<_, _>>()?,
                other => {
                    return Err(self.span(value_start).error(format!(
                        "`include` is a file name or a list of them, not {}",
                        crate::evaluator::type_of(&other)
                    )))
                }
            };
            self.includes.push(Include {
                patterns,
                span: self.span(key_start),
                scope: scope.clone(),
            });
            return Ok(self.blank_out(start, skim.end));
        }

        let text = self.src[value_start..skim.end].trim_end();
        if !skim.dollar && !skim.interpolates && probe_toml(text).is_ok() {
            return Ok(self.copy_line_from(start, skim.end));
        }
        let value = self.value_of(value_start, &skim, scope)?;
        let line_end = line_end(self.bytes, skim.end);
        self.out.push_str(&self.src[start..value_start]);
        self.out.push_str(&render_toml_value(&value));
        // A comment after the value stays on the key's line.
        let rest = &self.src[skim.end..line_end];
        if !rest.trim().is_empty() {
            self.out.push(' ');
            self.out.push_str(rest.trim());
        }
        let newlines = self.src[value_start..skim.end].matches('\n').count();
        self.out.extend(std::iter::repeat('\n').take(newlines));
        Ok(self.finish_line(line_end))
    }

    /// The value written at `start`, plain TOML or a TOMLX expression.
    fn value_of(&self, start: usize, skim: &Skim, scope: &Scope) -> Result<Value, TomlxError> {
        let text = self.src[start..skim.end].trim_end();
        if !skim.dollar && !skim.interpolates {
            match probe_toml(text) {
                Ok(v) => return Ok(v),
                // Perhaps an expression without a `$`, such as
                // `env("X")`; if it does not parse as one either, the
                // TOML parser's message is the more useful.
                Err(toml_message) => {
                    let base = self.span(start);
                    let parsed =
                        tokenize(text, &base).and_then(|tokens| parse_expr_at(tokens, Some(&base)));
                    return match parsed {
                        Ok(expr) => evaluate(&expr, scope, &base),
                        Err(_) => Err(base.error(format!("invalid value: {toml_message}"))),
                    };
                }
            }
        }
        self.evaluate_text(text, start, scope)
    }

    fn evaluate_text(&self, text: &str, at: usize, scope: &Scope) -> Result<Value, TomlxError> {
        let base = self.span(at);
        let tokens = tokenize(text, &base)?;
        let expr = parse_expr_at(tokens, Some(&base))?;
        evaluate(&expr, scope, &base)
    }

    fn is_statement(&self, at: usize) -> bool {
        let end = ident_end(self.bytes, at);
        let open = skip_blanks(self.bytes, end);
        self.bytes.get(open) == Some(&b'(')
            && builtins::lookup(&self.src[at..end]).is_some_and(|b| b.statement)
    }

    /// Where the value starting at `start` ends.
    fn skim(&self, start: usize) -> Result<Skim, TomlxError> {
        let b = self.bytes;
        let mut skim = Skim {
            end: b.len(),
            dollar: false,
            interpolates: false,
        };
        let mut depth: i64 = 0;
        let mut i = start;
        while i < b.len() {
            match b[i] {
                b'\n' if depth <= 0 => {
                    skim.end = i;
                    return Ok(skim);
                }
                b'#' if depth <= 0 => {
                    skim.end = i;
                    return Ok(skim);
                }
                b'#' => {
                    // A comment inside a multi-line array or table.
                    i = line_end(b, i);
                    continue;
                }
                b'"' => {
                    i = self.skip_basic(i, &mut skim)?;
                    continue;
                }
                b'\'' => {
                    i = self.skip_literal(i)?;
                    continue;
                }
                b'[' | b'{' | b'(' => depth += 1,
                b']' | b'}' | b')' => depth -= 1,
                b'$' => skim.dollar = true,
                _ => {}
            }
            i += 1;
        }
        Ok(skim)
    }

    /// Past the basic string opening at `open`.
    fn skip_basic(&self, open: usize, skim: &mut Skim) -> Result<usize, TomlxError> {
        let b = self.bytes;
        let multiline = b[open..].starts_with(b"\"\"\"");
        let mut i = open + if multiline { 3 } else { 1 };
        while i < b.len() {
            match b[i] {
                b'\\' => {
                    i += 2;
                    continue;
                }
                b'$' if b.get(i + 1) == Some(&b'{') => skim.interpolates = true,
                b'"' if !multiline => return Ok(i + 1),
                b'"' if b[i..].starts_with(b"\"\"\"") => {
                    let mut end = i + 3;
                    while end < i + 5 && b.get(end) == Some(&b'"') {
                        end += 1;
                    }
                    return Ok(end);
                }
                b'\n' if !multiline => break,
                _ => {}
            }
            i += 1;
        }
        Err(self.span(open).error(if multiline {
            "unterminated string: no closing `\"\"\"`"
        } else {
            "unterminated string (a string that spans lines is written \"\"\"…\"\"\")"
        }))
    }

    /// Past the literal string opening at `open`.
    fn skip_literal(&self, open: usize) -> Result<usize, TomlxError> {
        let b = self.bytes;
        let multiline = b[open..].starts_with(b"'''");
        let mut i = open + if multiline { 3 } else { 1 };
        while i < b.len() {
            match b[i] {
                b'\'' if !multiline => return Ok(i + 1),
                b'\'' if b[i..].starts_with(b"'''") => {
                    let mut end = i + 3;
                    while end < i + 5 && b.get(end) == Some(&b'\'') {
                        end += 1;
                    }
                    return Ok(end);
                }
                b'\n' if !multiline => break,
                _ => {}
            }
            i += 1;
        }
        Err(self.span(open).error(if multiline {
            "unterminated literal string: no closing `'''`"
        } else {
            "unterminated literal string (a string that spans lines is written '''…''')"
        }))
    }

    /// Copy from `start` to the end of its line, newline included.
    fn copy_line(&mut self, start: usize) -> usize {
        self.copy_line_from(start, start)
    }

    /// Copy from `start` to the end of the line `from` is on.
    fn copy_line_from(&mut self, start: usize, from: usize) -> usize {
        let end = line_end(self.bytes, from);
        self.out.push_str(&self.src[start..end]);
        self.finish_line(end)
    }

    /// Replace `start` up to the end of the line holding `end` with as
    /// many newlines.
    fn blank_out(&mut self, start: usize, end: usize) -> usize {
        let end = line_end(self.bytes, end);
        let newlines = self.src[start..end].matches('\n').count();
        self.out.extend(std::iter::repeat('\n').take(newlines));
        self.finish_line(end)
    }

    /// Emit the newline at `line_end`, if any, and return the offset of
    /// the next line.
    fn finish_line(&mut self, line_end: usize) -> usize {
        if line_end < self.bytes.len() {
            self.out.push('\n');
            line_end + 1
        } else {
            line_end
        }
    }

    fn span(&self, offset: usize) -> Span {
        let line_idx = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let line_start = self.line_starts[line_idx];
        let col = self.src[line_start..offset].chars().count() + 1;
        Span::new(self.file.clone(), line_idx + 1, col)
    }
}

/// The value of `text` as plain TOML, or the TOML parser's message.
fn probe_toml(text: &str) -> Result<Value, String> {
    let doc = format!("v = {text}\n");
    match toml::from_str::<toml::Table>(&doc) {
        Ok(mut table) => Ok(table.remove("v").expect("the probe defines `v`")),
        Err(e) => Err(e.message().to_string()),
    }
}

fn line_starts(src: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
    starts
}

fn skip_blanks(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    i
}

/// Offset of the newline ending the line `i` is on (or the end).
fn line_end(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| i + p)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn ident_end(b: &[u8], mut i: usize) -> usize {
    while b
        .get(i)
        .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        i += 1;
    }
    i
}

/// The offset of the `=` ending the key that starts at `i`, if the line
/// has one outside quoted key parts.
fn find_key_end(b: &[u8], mut i: usize) -> Option<usize> {
    while i < b.len() {
        match b[i] {
            b'=' => return Some(i),
            b'\n' => return None,
            quote @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != quote && b[i] != b'\n' {
                    if quote == b'"' && b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}
