//! Choosing what a run runs.
//!
//! A run names what it wants with patterns. A pattern is a task's exact
//! name (`deployments[base=joe]`), the name of a matrix (meaning all its
//! variants: `deployments`), or a name with `*` (any run of characters)
//! and `?` (any one character) in it: `cli-*`, `deployments[*]`. Brackets
//! are not character classes, so matrix names never need quoting beyond
//! the shell's. Tags then narrow the choice: `--tag` keeps what has any
//! of the tags given, `--exclude-tag` drops what has any of those. What
//! the chosen tasks depend on always comes along, whatever its tags.

use std::collections::BTreeSet;

use crate::dag::Dag;
use crate::error::ExecError;
use crate::task::Task;

/// What a run asked for.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// Patterns; none means everything.
    pub patterns: Vec<String>,
    /// Keep only what has one of these tags (when not empty).
    pub tags: Vec<String>,
    /// Drop what has one of these tags.
    pub exclude_tags: Vec<String>,
    /// Without patterns, choose the tests rather than everything, and
    /// let wildcards match only tests.
    pub tests_only: bool,
}

/// `true` when `name` matches `pattern`, where `*` matches any run of
/// characters and `?` any one character.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The tasks `selection` chooses, with everything they depend on, in the
/// DAG's order.
pub fn select(dag: &Dag, selection: &Selection) -> Result<Vec<Task>, ExecError> {
    let tasks = dag.tasks();
    let eligible = |t: &Task| !selection.tests_only || t.is_test();

    let mut roots: BTreeSet<String> = BTreeSet::new();
    if selection.patterns.is_empty() {
        roots.extend(tasks.iter().filter(|t| eligible(t)).map(|t| t.name.clone()));
    }
    for pattern in &selection.patterns {
        let exact: Vec<&Task> = tasks.iter().filter(|t| t.name == *pattern).collect();
        let variants: Vec<&Task> = tasks
            .iter()
            .filter(|t| t.variant.as_ref().is_some_and(|v| v.base == *pattern))
            .collect();
        let matched: Vec<&Task> = if !exact.is_empty() {
            exact
        } else if !variants.is_empty() {
            variants
        } else if pattern.contains(['*', '?']) {
            tasks
                .iter()
                .filter(|t| eligible(t) && wildcard_match(pattern, &t.name))
                .collect()
        } else {
            let mut names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
            names.extend(
                tasks
                    .iter()
                    .filter_map(|t| t.variant.as_ref().map(|v| v.base.as_str())),
            );
            return Err(ExecError::UnknownTask {
                name: pattern.clone(),
                suggestion: crate::executor::closest(pattern, names.into_iter()),
            });
        };
        roots.extend(matched.into_iter().map(|t| t.name.clone()));
    }

    roots.retain(|name| {
        let t = tasks
            .iter()
            .find(|t| t.name == *name)
            .expect("a root is a task");
        (selection.tags.is_empty() || t.tags.iter().any(|tag| selection.tags.contains(tag)))
            && !t
                .tags
                .iter()
                .any(|tag| selection.exclude_tags.contains(tag))
    });

    let mut selected: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = roots.into_iter().collect();
    while let Some(n) = stack.pop() {
        if !selected.insert(n.clone()) {
            continue;
        }
        for d in dag.deps_of(&n) {
            stack.push(d.clone());
        }
    }
    Ok(tasks
        .iter()
        .filter(|t| selected.contains(&t.name))
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{TaskKind, Variant};

    #[test]
    fn wildcards() {
        assert!(wildcard_match("cli-*", "cli-install"));
        assert!(wildcard_match("deployments[*]", "deployments[base=joe]"));
        assert!(wildcard_match("?mok?", "smoke"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("cli-*", "unit"));
        assert!(!wildcard_match("a*b", "ac"));
    }

    fn dag() -> Dag {
        let mut build = Task::new("build", "b");
        build.tags = vec!["build".into()];
        let mut unit = Task::new("unit", "u").with_deps(&["build"]);
        unit.kind = TaskKind::Test;
        unit.tags = vec!["fast".into()];
        let mut variants = Vec::new();
        for base in ["scaffold", "joe"] {
            let mut t = Task::new(format!("deploy[base={base}]"), "d").with_deps(&["build"]);
            t.kind = TaskKind::Test;
            t.tags = vec!["vm".into()];
            t.variant = Some(Variant {
                base: "deploy".into(),
                values: vec![("base".into(), base.into())],
            });
            variants.push(t);
        }
        let mut tasks = vec![build, unit];
        tasks.extend(variants);
        Dag::build(&tasks).unwrap()
    }

    fn names(sel: Selection) -> Vec<String> {
        select(&dag(), &sel)
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect()
    }

    #[test]
    fn selects_by_name_matrix_wildcard_and_tag() {
        let sel = |patterns: &[&str]| Selection {
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
            ..Selection::default()
        };
        assert_eq!(names(sel(&["unit"])), vec!["build", "unit"]);
        assert_eq!(
            names(sel(&["deploy"])),
            vec!["build", "deploy[base=scaffold]", "deploy[base=joe]"]
        );
        assert_eq!(
            names(sel(&["deploy[base=joe]"])),
            vec!["build", "deploy[base=joe]"]
        );
        assert_eq!(names(sel(&["*[base=*]"])).len(), 3);
        assert_eq!(
            names(Selection {
                tests_only: true,
                exclude_tags: vec!["vm".into()],
                ..Selection::default()
            }),
            vec!["build", "unit"]
        );
        assert_eq!(
            names(Selection {
                tags: vec!["vm".into()],
                tests_only: true,
                ..Selection::default()
            })
            .len(),
            3
        );
    }

    #[test]
    fn unknown_names_get_suggestions() {
        let err = select(
            &dag(),
            &Selection {
                patterns: vec!["deplyo".into()],
                ..Selection::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "no task named `deplyo` (did you mean `deploy`?)"
        );
    }
}
