//! TOMLX's built-in functions.
//!
//! Every built-in is one entry in [`BUILTINS`]: its name, whether it may
//! stand alone on a line as a statement (for functions called for their
//! effect, such as `git()`), and the function that implements it. The
//! scanner and the evaluator consult only this table, so a new built-in
//! is one entry plus its documentation in `docs/tomlx-spec.md`.
//!
//! Paths given to built-ins are relative to the project root, the
//! directory of the top-level configuration file.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use toml::Value;

use crate::error::TomlxError;
use crate::evaluator::{type_of, Scope};
use crate::lexer::Span;

/// A built-in function.
pub(crate) struct Builtin {
    /// The name it is called by.
    pub name: &'static str,
    /// Whether a bare call may stand on its own line.
    pub statement: bool,
    /// The implementation.
    pub call: fn(&Args<'_>) -> Result<Value, TomlxError>,
}

/// Every built-in, in the order the specification lists them.
pub(crate) static BUILTINS: &[Builtin] = &[
    Builtin {
        name: "env",
        statement: false,
        call: env,
    },
    Builtin {
        name: "glob",
        statement: false,
        call: glob,
    },
    Builtin {
        name: "exec",
        statement: false,
        call: exec,
    },
    Builtin {
        name: "exists",
        statement: false,
        call: exists,
    },
    Builtin {
        name: "which",
        statement: false,
        call: which,
    },
    Builtin {
        name: "basename",
        statement: false,
        call: basename,
    },
    Builtin {
        name: "dirname",
        statement: false,
        call: dirname,
    },
    Builtin {
        name: "join",
        statement: false,
        call: join,
    },
    Builtin {
        name: "replace",
        statement: false,
        call: replace,
    },
    Builtin {
        name: "git",
        statement: true,
        call: git,
    },
    Builtin {
        name: "fetch",
        statement: true,
        call: fetch,
    },
];

/// The built-in called `name`, if there is one.
pub(crate) fn lookup(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

/// The evaluated arguments of one call.
pub(crate) struct Args<'a> {
    pub name: &'a str,
    pub positional: Vec<Value>,
    pub kwargs: BTreeMap<String, Value>,
    pub span: &'a Span,
    pub scope: &'a Scope,
}

impl Args<'_> {
    fn err(&self, msg: impl Into<String>) -> TomlxError {
        self.span.error(format!("{}(): {}", self.name, msg.into()))
    }

    /// Checks the number of positional arguments and that every keyword
    /// argument is one of `keywords`.
    fn expect(&self, min: usize, max: usize, keywords: &[&str]) -> Result<(), TomlxError> {
        let n = self.positional.len();
        if n < min || n > max {
            let wanted = if min == max {
                format!("{min}")
            } else {
                format!("{min} to {max}")
            };
            return Err(self.err(format!("expects {wanted} argument(s), got {n}")));
        }
        if let Some(unknown) = self.kwargs.keys().find(|k| !keywords.contains(&k.as_str())) {
            return Err(if keywords.is_empty() {
                self.err("takes no keyword arguments")
            } else {
                self.err(format!(
                    "has no keyword argument `{unknown}` (it takes {})",
                    keywords.join(", ")
                ))
            });
        }
        Ok(())
    }

    fn string(&self, index: usize, label: &str) -> Result<String, TomlxError> {
        match &self.positional[index] {
            Value::String(s) => Ok(s.clone()),
            other => Err(self.err(format!("{label} must be a string, got {}", type_of(other)))),
        }
    }

    fn kw_string(&self, name: &str) -> Result<Option<String>, TomlxError> {
        match self.kwargs.get(name) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(other) => {
                Err(self.err(format!("{name} must be a string, got {}", type_of(other))))
            }
        }
    }

    fn kw_bool(&self, name: &str) -> Result<bool, TomlxError> {
        match self.kwargs.get(name) {
            None => Ok(false),
            Some(Value::Boolean(b)) => Ok(*b),
            Some(other) => {
                Err(self.err(format!("{name} must be a boolean, got {}", type_of(other))))
            }
        }
    }

    fn kw_int(&self, name: &str) -> Result<Option<i64>, TomlxError> {
        match self.kwargs.get(name) {
            None => Ok(None),
            Some(Value::Integer(i)) => Ok(Some(*i)),
            Some(other) => {
                Err(self.err(format!("{name} must be an integer, got {}", type_of(other))))
            }
        }
    }

    fn root(&self) -> &Path {
        &self.scope.root
    }
}

fn env(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(1, 2, &[])?;
    let key = a.string(0, "the variable name")?;
    match std::env::var(&key) {
        Ok(v) => Ok(Value::String(v)),
        Err(_) if a.positional.len() == 2 => Ok(a.positional[1].clone()),
        Err(_) => Err(a.err(format!(
            "environment variable `{key}` is not set (env(\"{key}\", \"default\") gives a fallback)"
        ))),
    }
}

fn glob(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(1, 1, &[])?;
    let pattern = a.string(0, "the pattern")?;
    let matches = neomake_core::glob::expand(a.root(), &pattern, &[]).map_err(|e| a.err(e))?;
    Ok(Value::Array(
        matches.into_iter().map(Value::String).collect(),
    ))
}

fn exec(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(1, 1, &[])?;
    let cmd = a.string(0, "the command")?;
    let invocation = neomake_core::shell::resolve();
    let output = Command::new(&invocation.program)
        .args(&invocation.args)
        .arg(&cmd)
        .current_dir(a.root())
        .output()
        .map_err(|e| a.err(format!("`{cmd}` failed to start: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(a.err(format!(
            "`{cmd}` exited with {}{}",
            output.status,
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        )));
    }
    Ok(Value::String(
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
    ))
}

fn exists(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(1, 1, &[])?;
    let path = a.string(0, "the path")?;
    Ok(Value::Boolean(a.root().join(path).exists()))
}

fn which(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(1, 1, &[])?;
    let program = a.string(0, "the program")?;
    let found = which::which(&program)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Value::String(found))
}

fn basename(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(1, 1, &[])?;
    let path = a.string(0, "the path")?;
    let trimmed = path.trim_end_matches('/');
    let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
    Ok(Value::String(base.to_string()))
}

fn dirname(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(1, 1, &[])?;
    let path = a.string(0, "the path")?;
    let trimmed = path.trim_end_matches('/');
    let dir = match trimmed.rfind('/') {
        Some(0) => "/",
        Some(i) => &trimmed[..i],
        None => ".",
    };
    Ok(Value::String(dir.to_string()))
}

fn join(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(1, usize::MAX, &[])?;
    let mut out = String::new();
    for i in 0..a.positional.len() {
        let part = a.string(i, "every part")?;
        if part.starts_with('/') || out.is_empty() {
            out = part;
        } else if !part.is_empty() {
            if !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(&part);
        }
    }
    Ok(Value::String(out))
}

fn replace(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(3, 3, &[])?;
    let text = a.string(0, "the text")?;
    let from = a.string(1, "the text to replace")?;
    let to = a.string(2, "the replacement")?;
    if from.is_empty() {
        return Err(a.err("the text to replace cannot be empty"));
    }
    Ok(Value::String(text.replace(&from, &to)))
}

fn git(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(3, 3, &["submodules", "depth"])?;
    let repo = a.string(0, "the repository")?;
    let rev = a.string(1, "the revision")?;
    let dest_template = a.string(2, "the destination")?;
    let submodules = a.kw_bool("submodules")?;
    let depth = a.kw_int("depth")?.map(|d| d as u32);

    let basename = crate::fetch::extract_basename(&repo);
    let dest = crate::fetch::resolve_wildcard(&dest_template, &basename, None);
    let (path, sha) =
        crate::fetch::git_clone(&repo, &rev, &dest, submodules, depth).map_err(|e| a.err(e))?;

    let mut manifest = neomake_core::asset::AssetManifest::load(a.root());
    manifest.add(
        path.clone(),
        neomake_core::asset::AssetRecord {
            kind: neomake_core::asset::AssetKind::Git { commit_sha: sha },
            source: repo,
        },
    );
    manifest.save(a.root());
    Ok(Value::String(path.to_string_lossy().to_string()))
}

fn fetch(a: &Args<'_>) -> Result<Value, TomlxError> {
    a.expect(2, 2, &["sha256", "alias", "extract"])?;
    let url = a.string(0, "the URL")?;
    let dest_template = a.string(1, "the destination")?;
    let sha256 = a.kw_string("sha256")?;
    let alias = a.kw_string("alias")?;
    let extract = a.kw_bool("extract")?;

    let basename = crate::fetch::extract_basename(&url);
    let dest = crate::fetch::resolve_wildcard(&dest_template, &basename, alias.as_deref());
    let (path, sha) =
        crate::fetch::http_fetch(&url, &dest, sha256.as_deref(), extract).map_err(|e| a.err(e))?;

    let mut manifest = neomake_core::asset::AssetManifest::load(a.root());
    manifest.add(
        path.clone(),
        neomake_core::asset::AssetRecord {
            kind: neomake_core::asset::AssetKind::Fetch {
                content_sha256: sha,
            },
            source: url,
        },
    );
    manifest.save(a.root());
    Ok(Value::String(path.to_string_lossy().to_string()))
}
