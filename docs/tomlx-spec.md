# TOMLX Specification (v0.2)

TOMLX is a progressively enhanced superset of TOML used by `neomake`.
It adds **variables**, **expressions** (operators, conditionals and
built-in functions) and **includes**, while guaranteeing that any valid
[TOML](https://toml.io/) document is a valid TOMLX document with
identical semantics.

This specification describes TOMLX as implemented in `neomake-tomlx`.
Items marked *"Future"* are reserved for later extensions.

---

## 1. Relationship to TOML

TOMLX is a **strict superset** of TOML 1.0:

- Every valid `.toml` file is also a valid `.tomlx` file.
- Every value that is plain TOML is passed to the TOML parser exactly as
  written (§6), so the result is the one a TOML parser would produce.
  The only TOML text TOMLX reads differently is `${` inside a basic
  string, which is interpolation (§3).

---

## 2. Variables

A **variable declaration** is a statement of the form:

```
$NAME = EXPR
```

where `NAME` is an identifier matching `[A-Za-z_][A-Za-z0-9_-]*` and
`EXPR` is a TOMLX expression (§4). Like any value, the expression may
continue over several lines while it is inside brackets, braces or a
string.

- Declarations are evaluated top-to-bottom; later declarations may
  reference earlier ones.
- A declaration may appear anywhere a key/value pair may; variables are
  global to the file regardless of the table they are written in.
- The declaration is removed from the document before it is handed to
  the TOML parser.
- Redeclaration is allowed and shadows the previous value.

### References

Inside an expression, `$NAME` or `${NAME}` resolves to the value of the
variable `NAME`. Inside an expression the `$` is redundant: bare `NAME`
means the same thing.

Referencing an undefined variable is an error, reported at the
reference:

```
neomake.tomlx:12:11: undefined variable `profile`
```

---

## 3. Strings and Interpolation

TOMLX has TOML's four kinds of string:

| Form | Kind | Escapes | `${…}` |
|------|------|---------|--------|
| `"…"` | basic | yes | interpolated |
| `"""…"""` | multi-line basic | yes | interpolated |
| `'…'` | literal | no | as written |
| `'''…'''` | multi-line literal | no | as written |

In basic strings, `${EXPR}` is replaced by the result of evaluating
`EXPR`, stringified (strings as themselves, numbers and booleans in
their usual form, arrays joined with `,`).

```
msg = "hello ${user_name}!"
```

- The escapes are TOML's (`\b \t \n \f \r \" \\ \uXXXX \UXXXXXXXX`),
  plus `\e`, `\xHH`, `\0` and `\$`. `"\${literal}"` is the literal text
  `${literal}`.
- A `$` not followed by `{` is ordinary text: `"cost: $5"`.
- Literal strings perform no interpolation and no escaping:
  `'hello ${user_name}'` is the literal 19-character string.

**Shell commands belong in literal strings.** A command such as
`'echo "${HOME}" $?'` or a script in `'''…'''` reaches the shell exactly
as written. In a basic string, `${HOME}` would be read as a reference to
a TOMLX variable named `HOME`.

---

## 4. Expressions

TOMLX expressions extend TOML values with operators, conditionals,
function calls and variable references. The full grammar is given in §9.

### Operators (precedence lowest → highest)

| Operator | Meaning                       | Operands              |
|----------|-------------------------------|-----------------------|
| `\|\|`   | short-circuit logical OR      | bool, bool            |
| `&&`     | short-circuit logical AND     | bool, bool            |
| `==`     | equality                      | any pair              |
| `!=`     | inequality                    | any pair              |
| `+`      | addition / concat / extend    | int+int, float+float, string+string, array+array |
| `!`      | logical NOT (unary)           | bool                  |
| `-`      | negation (unary)              | int, float            |

Parentheses `(` `)` override precedence. Values of different types are
never equal (`1 == 1.0` is false); arrays and tables are equal when
their elements are.

### Conditionals

```
if COND { THEN_EXPR } else { ELSE_EXPR }
if COND { A } else if COND2 { B } else { C }
```

`COND` must evaluate to a boolean; the branches are expressions and may
have different types. Exactly one branch is evaluated. A conditional
may span several lines.

*Future*: a block-level form, `if COND { [table.path] ... }`, that
conditionally includes whole TOML tables.

### Arrays and inline tables

`[EXPR, EXPR, ...]` and `{ KEY = EXPR, a.b = EXPR, ... }`. Every element
is evaluated; the result is a plain TOML array or table. Unlike TOML
1.0, an inline table holding expressions may span lines and end with a
trailing comma.

### Numbers

Decimal integers and floats as in TOML, with `_` separators and
exponents, and `inf` and `nan`. Hexadecimal, octal and binary integers
and dates are plain TOML: they may be written as values, but not inside
an expression.

---

## 5. Built-in Functions

Built-ins are evaluated while the document is loaded; their results
become part of the TOML document. Paths are relative to the **project
root**, the directory of the top-level configuration file. A built-in
given a keyword argument it does not take is an error.

### `env(name)` / `env(name, default)`

The value of the environment variable `name` (a string). If it is not
set, `default` is returned; without a default, loading fails.

### `glob(pattern)`

A sorted array of the files under the project root whose path (relative
to the root, with `/` separators) matches `pattern`, in
[`globset`](https://docs.rs/globset) syntax: `*` matches within one
path component, `**` across any number. The walk starts at the
pattern's literal prefix (`build/repo` for `build/repo/*.dave`), and
`.git` and `.neomake` directories are never entered.

### `exec(command)`

Runs `command` with the platform's shell, with the project root as the
working directory, and returns its standard output with trailing
whitespace removed. A non-zero exit status is an error, which includes
what the command wrote to standard error.

The shell is `sh -c` on Unix-likes; on Windows it is `pwsh`, then
`powershell.exe`, then `cmd.exe /S /C`. `NEOMAKE_SHELL` overrides it
(whitespace-separated argv; the command is appended as the last
argument).

> **Cache caveat.** `exec()` runs once per load and is not part of any
> task's cache key. To make a command's output invalidate the cache,
> write it to a file and list that file under the task's `inputs`.

### `exists(path)`

`true` if `path` exists.

### `which(program)`

The full path of `program` found on `PATH`, or `""` if there is none:
`which("qemu-system-x86_64") != ""`.

### `basename(path)`, `dirname(path)`

The last component of `path`, and everything before it (`"."` when
there is nothing before it). Trailing slashes are ignored.

### `join(part, ...)`

The parts joined with `/`. A part that starts with `/` replaces what
came before it.

### `replace(text, from, to)`

`text` with every occurrence of `from` replaced by `to`.

### `git(repo, rev, dest, submodules = false, depth = N)`

Clones `repo` into `dest` and checks out `rev`, unless `dest` already
holds a repository. `repo` is a URL, `git@…`, `owner/repo` (GitHub), or
`gh:`, `gl:`, `cb:` or `bb:` followed by `owner/repo` (also spelled
`github:`, `gitlab:`, `codeberg:`, `bitbucket:`). A `%` in `dest` is
replaced by the repository's name. Returns the destination path. `depth`
is accepted but clones are always full.

### `fetch(url, dest, sha256 = "…", alias = "…", extract = false)`

Downloads `url` to `dest`, checking its SHA-256 if one is given. With
`extract = true`, a `.tar.gz`, `.tgz` or `.tar.xz` archive is unpacked
next to `dest`. A `%` in `dest` is replaced by `alias`, or else by the
URL's file name. Returns the destination path.

`git` and `fetch` may also be written as statements, alone on their
line, when only their effect is wanted:

```
[tasks.vendor]
git('theJoeLinuxFoundation/neomake', 'main', 'vendor/%')
command = "ls vendor/neomake"
```

### Type coercion

TOMLX performs no implicit conversion between numbers and strings.
Interpolation (§3) is the way to put a number into a string.

---

## 6. How Values Are Recognised

The scanner reads the document statement by statement, as a TOML
parser does. For every `key = value` it first finds the value's full
extent: a value ends at the newline (or comment) that is outside every
string, bracket, brace and parenthesis. Then:

- If the value contains no `$` outside a string, no `${` inside a basic
  string, and is valid TOML on its own, it is **copied as written**.
- Otherwise it is evaluated as a TOMLX expression and rendered back as
  a TOML literal on the key's line.

So `deps = ["a"]` and a `'''…'''` script are untouched, while
`env("X")`, `"a" + "b"`, `${v}` and `"x-${v}"` are evaluated. Because
the extent is found first, nothing inside a value is ever read as a
declaration, a statement or an `include`, however its lines look.

Every replacement keeps the number of lines it replaces, so line
numbers in TOML parse errors and in later diagnostics are the source
file's.

---

## 7. Includes

```
include = "tests/cli.tomlx"
include = ["tests/*.tomlx", "tests/vm/*.tomlx"]
```

An `include` key before the first table header names files to load as
well. Its value is a path or a list of paths and glob patterns (§5),
and may be an expression.

- Paths are relative to the project root, whichever file includes them.
- A path without glob characters must exist; a pattern may match
  nothing.
- An included file sees the variables declared before the `include`
  line. The variables it declares are its own.
- A file is loaded once, however many times it is included, so two
  files may include each other.
- The documents' tables are merged. A key defined in two documents is
  an error that names both files.

---

## 8. Evaluation Pipeline

```
source
  │
  ▼  scanner (statements, value extents, includes)
plain TOML text, line for line
  │
  ▼  toml parser
toml::Value      (one per document, then merged)
```

Each value that is not plain TOML goes through the expression pipeline:
lexer → parser → evaluator (variable scope, built-ins) → `toml::Value`
→ rendered TOML literal.

The pipeline is deterministic: given the same sources, environment and
working directory, it produces the same output. `exec()`, `git()` and
`fetch()` are the only built-ins that look outside the configuration.

---

## 9. Grammar (EBNF)

```ebnf
tomlx_document  = { statement } ;
statement       = var_decl | call_statement | key_value | toml_line ;

var_decl        = [ ws ] "$" ident ws "=" ws value [ comment ] newline ;
call_statement  = [ ws ] ident "(" args ")" [ comment ] newline ;   (* ident: git, fetch *)
key_value       = [ ws ] toml_key ws "=" ws value [ comment ] newline ;
value           = toml_value | expr ;      (* see §6 *)

expr            = or_expr ;
or_expr         = and_expr { "||" and_expr } ;
and_expr        = eq_expr  { "&&" eq_expr } ;
eq_expr         = add_expr { ( "==" | "!=" ) add_expr } ;
add_expr        = unary    { "+" unary } ;
unary           = "!" unary | "-" unary | primary ;
primary         = literal | var_ref | call | array | table | if_expr
                | "(" expr ")" ;

var_ref         = "$" ident | "${" ident "}" | ident ;
call            = ident "(" [ args ] ")" ;
args            = arg { "," arg } [ "," ] ;
arg             = expr | ident "=" expr ;
array           = "[" [ expr { "," expr } [ "," ] ] "]" ;
table           = "{" [ entry { "," entry } [ "," ] ] "}" ;
entry           = key { "." key } "=" expr ;
key             = ident | string ;
if_expr         = "if" expr "{" expr "}" "else" ( "{" expr "}" | if_expr ) ;

literal         = int | float | bool | string ;
string          = basic_string | ml_basic_string | literal_string | ml_literal_string ;
                  (* basic strings may hold "${" expr "}" *)

ident           = ( letter | "_" ) { letter | digit | "_" | "-" } ;
```

Whitespace, newlines and `#` comments may appear between the tokens of
an expression. `toml_line`, `toml_key` and `toml_value` are the
corresponding productions of [TOML 1.0](https://toml.io/en/v1.0.0).

---

## 10. Limits

- `exec()` runs once per configuration load; `git()` and `fetch()` run
  when the configuration is loaded, not when a task runs.
- Block-level `if` (conditionally including whole tables) is not yet
  supported; use a value-level conditional or `include`.

---

## 11. Versioning

This document describes TOMLX v0.2. v0.1 documents remain valid, with
one difference: a keyword argument a built-in does not take is now an
error, where v0.1 ignored it.
The on-disk cache format is versioned independently; see
`neomake-core::cache::CACHE_FORMAT_VERSION`.
