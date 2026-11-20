//! TOMLX: a progressively enhanced superset of TOML for `neomake`.
//!
//! TOMLX adds, on top of TOML:
//!
//! 1. Variable declarations: `$name = <expr>`.
//! 2. String interpolation and bare-value references: `${name}`.
//! 3. Expressions: operators, `if cond { a } else { b }`, and built-in
//!    functions (`env`, `glob`, `exec`, `exists`, `which`, path helpers,
//!    `git`, `fetch`).
//! 4. `include`, which splits a configuration over several files.
//!
//! # Graceful degradation
//!
//! Any valid `.toml` file is a valid `.tomlx` file with identical
//! semantics: values that are plain TOML are copied through as written.
//!
//! # Pipeline
//!
//! ```text
//! source ──► [scanner] ──► plain TOML text ──► [toml] ──► toml::Value
//!               │
//!               └─ each TOMLX value: [lexer] ──► [parser] ──► [evaluator]
//! ```
//!
//! The scanner (`scanner.rs`) walks the document statement by statement,
//! finds the full extent of every value, and evaluates the ones that are
//! not plain TOML; the result keeps the source's line numbers. See
//! `docs/tomlx-spec.md` for the language.

#![warn(missing_docs)]
#![deny(clippy::all)]

mod builtins;
pub mod error;
pub mod evaluator;
pub mod fetch;
pub mod lexer;
pub mod parser;
mod scanner;

pub use error::TomlxError;
pub use evaluator::{
    load, load_documents, load_documents_from, load_str, render_toml_value, Document, Loaded,
};
pub use lexer::{Span, SpannedToken, StrSegment, Token};
pub use parser::{parse_expr, parse_expr_at, BinOp, Expr, StringPart, UnaryOp};
