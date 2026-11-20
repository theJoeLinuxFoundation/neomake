# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

neomake is a parallel, cached task runner configured in TOML or TOMLX (a superset with variables, expressions and built-in functions). It is being grown into the test runner and VM test harness of joe Linux (`theJoeLinuxFoundation/joe-linux`), replacing that project's Makefile test targets, `tests/cli.sh` and its Python QEMU harness. Features are therefore judged by whether they let a joe Linux test be written declaratively, run unprivileged, and report precisely why it failed.

The repository lives at `theJoeLinuxFoundation/neomake` (`origin`); `upstream` is the original `sinisterMage/neomake`.

## Commands

```
cargo build --workspace
cargo test --workspace                                   # unit tests plus neomake/tests/end_to_end.rs
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings    # CI fails on any warning
cargo run -p neomake -- -c examples/basic.toml run
```

All four checks must pass before a commit is pushed; CI runs them on Linux, macOS and Windows.

## Layout

- `neomake-tomlx/`: the configuration language. `evaluator.rs` turns a TOMLX document into a `toml::Value`; `lexer.rs` and `parser.rs` handle the expression language; `fetch.rs` implements `git()` and `fetch()`. `docs/tomlx-spec.md` is the specification and must change with the code.
- `neomake-core/`: the task model (`task.rs`), the dependency graph (`dag.rs`), the cache (`cache.rs`) and the tokio executor (`executor.rs`).
- `neomake/`: the CLI binary (`cli.rs`, `main.rs`) and the end-to-end tests, which run the built binary in a temporary directory.

Every crate has `#![warn(missing_docs)]` and `#![deny(clippy::all)]`: public items need doc comments.

## Git

- Commit messages: `area: imperative summary`, lowercase, no trailing period. The area is the crate or part touched: `tomlx`, `core`, `executor`, `cache`, `cli`, `report`, `services`, `machine`, `docs`, `ci`, `examples`.
- The body is prose: what changed and why, in paragraphs wrapped at about 72 columns.
- Never add `Co-Authored-By`, `Generated-with`, `Assisted-by` or any similar
  trailer. Not once, not for a small change. Treat it as a rule violation.
- The author line is the human who committed it.
