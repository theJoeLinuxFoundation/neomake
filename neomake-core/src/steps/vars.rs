//! `{{ name }}` placeholders.
//!
//! Strings in tests may refer to values only known later: a matrix
//! variant's values and a template's parameters (substituted when the
//! configuration is loaded), and the variables steps capture, the
//! test's directory and the ports of services (substituted as the test
//! runs). A placeholder is `{{`, optional spaces, a name made of
//! letters, digits, `_`, `-` and `.` that starts with a letter or `_`,
//! optional spaces and `}}`. Anything else between braces, such as Go's
//! `{{.State}}`, is left as written, and `\{{` is a literal `{{`.

use std::collections::BTreeMap;

/// A placeholder found in a string: its name and its byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Placeholder {
    name: String,
    start: usize,
    end: usize,
}

fn scan(text: &str) -> Vec<Placeholder> {
    let b = text.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'\\' && b[i + 1] == b'{' {
            i += 2;
            continue;
        }
        if b[i] == b'{' && b[i + 1] == b'{' {
            let mut j = i + 2;
            while j < b.len() && b[j] == b' ' {
                j += 1;
            }
            let name_start = j;
            if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
                while j < b.len()
                    && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'_' | b'-' | b'.'))
                {
                    j += 1;
                }
                let name_end = j;
                while j < b.len() && b[j] == b' ' {
                    j += 1;
                }
                if b[j..].starts_with(b"}}") {
                    found.push(Placeholder {
                        name: text[name_start..name_end].to_string(),
                        start: i,
                        end: j + 2,
                    });
                    i = j + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    found
}

/// The names of every placeholder in `text`, in order.
pub fn names(text: &str) -> Vec<String> {
    scan(text).into_iter().map(|p| p.name).collect()
}

/// Replace every placeholder whose name `lookup` knows, leaving the
/// others (and escapes) as they are. Used when the configuration is
/// loaded, for matrix values and template parameters.
pub fn substitute_known(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for p in scan(text) {
        if let Some(value) = lookup(&p.name) {
            out.push_str(&text[last..p.start]);
            out.push_str(&value);
            last = p.end;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Replace every placeholder, and turn `\{{` into `{{`. A placeholder
/// `lookup` does not know is an error naming it.
pub fn substitute(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for p in scan(text) {
        let value =
            lookup(&p.name).ok_or_else(|| format!("`{{{{{}}}}}` is not defined", p.name))?;
        out.push_str(&unescape(&text[last..p.start]));
        out.push_str(&value);
        last = p.end;
    }
    out.push_str(&unescape(&text[last..]));
    Ok(out)
}

fn unescape(text: &str) -> String {
    text.replace("\\{{", "{{")
}

/// The variables of a running test.
#[derive(Debug, Clone, Default)]
pub struct Vars {
    values: BTreeMap<String, String>,
}

impl Vars {
    /// Set `name` to `value`.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.values.insert(name.into(), value.into());
    }

    /// The value of `name`.
    pub fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    /// `text` with every placeholder replaced.
    pub fn apply(&self, text: &str) -> Result<String, String> {
        substitute(text, &|name| self.get(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(name: &str) -> Option<String> {
        match name {
            "n" => Some("3".into()),
            "matrix.base" => Some("joe".into()),
            _ => None,
        }
    }

    #[test]
    fn finds_placeholders_and_leaves_other_braces() {
        assert_eq!(
            names("a {{n}} b {{ matrix.base }} {{.State}} {{ 1x }} \\{{n}}"),
            vec!["n", "matrix.base"]
        );
    }

    #[test]
    fn substitutes_known_names_only() {
        assert_eq!(
            substitute_known("{{n}} {{kernel}} \\{{n}}", &lookup),
            "3 {{kernel}} \\{{n}}"
        );
    }

    #[test]
    fn substitutes_everything_or_says_what_is_missing() {
        assert_eq!(
            substitute("deploy/{{ n }} on {{matrix.base}} \\{{x}}", &lookup).unwrap(),
            "deploy/3 on joe {{x}}"
        );
        assert_eq!(
            substitute("{{kernel}}", &lookup).unwrap_err(),
            "`{{kernel}}` is not defined"
        );
        assert_eq!(
            substitute("go {{.State}}", &lookup).unwrap(),
            "go {{.State}}"
        );
    }
}
