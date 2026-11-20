//! Glob expansion relative to a project root.
//!
//! Patterns use [`globset`] syntax with `literal_separator` set, so `*`
//! stays within one path component and `**` crosses any number of them.
//! Matching is done on `/`-separated paths relative to the root on every
//! platform, and results come back in that form, sorted.
//!
//! The walk starts at the pattern's literal prefix (`build/repo` for
//! `build/repo/*.dave`) rather than at the root: projects such as joe
//! Linux keep millions of files under `build/`, and a pattern that names
//! one directory must not pay for all of them. `.git` and `.neomake`
//! directories are never entered.

use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use walkdir::WalkDir;

/// Directory names that are never walked into.
const ALWAYS_SKIPPED: &[&str] = &[".git", ".neomake"];

/// The leading path components of `pattern` that contain no glob
/// metacharacters. `src/**/*.rs` gives `src`, `*.rs` gives the empty
/// string, and a pattern without metacharacters is returned whole.
pub fn static_prefix(pattern: &str) -> &str {
    let mut end = 0;
    for (idx, component) in pattern.split('/').enumerate() {
        if component.contains(['*', '?', '[', ']', '{', '}', '\\']) {
            break;
        }
        end = if idx == 0 {
            component.len()
        } else {
            end + 1 + component.len()
        };
    }
    &pattern[..end]
}

/// `true` when `pattern` contains glob metacharacters.
pub fn is_pattern(pattern: &str) -> bool {
    static_prefix(pattern).len() != pattern.len()
}

/// Every file under `root` whose relative path matches `pattern`.
///
/// `skip` names further directories (relative to `root`) that are not
/// entered, such as a state directory under `build/`.
pub fn expand(root: &Path, pattern: &str, skip: &[PathBuf]) -> Result<Vec<String>, String> {
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    if !is_pattern(pattern) {
        let path = root.join(pattern);
        return Ok(if path.is_file() {
            vec![pattern.to_string()]
        } else {
            Vec::new()
        });
    }
    let matcher = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("invalid glob `{pattern}`: {e}"))?
        .compile_matcher();

    let prefix = static_prefix(pattern);
    let start = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix)
    };
    if !start.is_dir() {
        return Ok(Vec::new());
    }
    let skipped: Vec<PathBuf> = skip.iter().map(|s| root.join(s)).collect();

    let mut matches = Vec::new();
    let walker = WalkDir::new(&start)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 || !entry.file_type().is_dir() {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            if ALWAYS_SKIPPED.contains(&name.as_ref()) {
                return false;
            }
            !skipped.iter().any(|s| s == entry.path())
        });
    for entry in walker.flatten() {
        if !(entry.file_type().is_file() || entry.path().is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = to_slash(rel);
        if matcher.is_match(&rel) {
            matches.push(rel);
        }
    }
    matches.sort();
    Ok(matches)
}

/// A relative path with `/` separators, whatever the platform.
pub fn to_slash(path: &Path) -> String {
    let s = path.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        s.into_owned()
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "x").unwrap();
    }

    #[test]
    fn static_prefix_stops_at_the_first_pattern_component() {
        assert_eq!(static_prefix("src/**/*.rs"), "src");
        assert_eq!(static_prefix("build/repo/*.dave"), "build/repo");
        assert_eq!(static_prefix("*.rs"), "");
        assert_eq!(static_prefix("a/b/c.txt"), "a/b/c.txt");
        assert!(!is_pattern("a/b/c.txt"));
        assert!(is_pattern("a/*/c.txt"));
    }

    #[test]
    fn expands_from_the_prefix_and_skips_vcs_and_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "src/a.rs");
        touch(root, "src/sub/b.rs");
        touch(root, "src/c.txt");
        touch(root, ".git/x.rs");
        touch(root, "build/state/y.rs");
        touch(root, "build/z.rs");

        assert_eq!(
            expand(root, "src/**/*.rs", &[]).unwrap(),
            vec!["src/a.rs", "src/sub/b.rs"]
        );
        assert_eq!(expand(root, "src/*.rs", &[]).unwrap(), vec!["src/a.rs"]);
        assert_eq!(
            expand(root, "**/*.rs", &[PathBuf::from("build/state")]).unwrap(),
            vec!["build/z.rs", "src/a.rs", "src/sub/b.rs"]
        );
        assert_eq!(expand(root, "src/c.txt", &[]).unwrap(), vec!["src/c.txt"]);
        assert!(expand(root, "missing/*.rs", &[]).unwrap().is_empty());
        assert!(expand(root, "missing.txt", &[]).unwrap().is_empty());
    }
}
