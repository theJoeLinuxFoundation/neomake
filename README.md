# neomake

A progressively enhanced build orchestrator in Rust. `neomake` reads a
TOML (or TOMLX) configuration file, builds a dependency DAG, runs tasks
concurrently on a `tokio` executor, and skips unchanged work via a
SHA-256 content-addressable cache.

## Highlights

- **TOML in, TOML out.** Your build file starts as plain TOML. When you
  need more, opt in to TOMLX — a strict superset adding variables,
  value-level conditionals, and a small library of built-ins
  (`env`, `glob`, `exec`).
- **Parallel by default.** Tasks with no dependency run concurrently.
  Concurrency is capped with `--concurrency N` (default: number of
  CPUs).
- **Content-addressable cache.** Each task's cache key is a SHA-256 of
  its command, env, sorted input file hashes, and the cache keys of its
  upstream dependencies, so a change anywhere in the transitive input
  closure invalidates every dependent.
- **Clear diagnostics.** Cycle detection reports the concrete cycle
  path; every config error carries a file name and line number.

## Workspace layout

```
neomake/             CLI binary
neomake-core/        task model, DAG builder, cache, tokio executor
neomake-tomlx/       TOMLX lexer, parser, evaluator
examples/            basic.toml, advanced.tomlx
docs/tomlx-spec.md   formal TOMLX spec
```

## Installation

Requires a recent stable Rust toolchain (1.75+).

```bash
cargo install --path neomake
```

A `cargo build --release` at the workspace root produces
`target/release/neomake`.

### Arch Linux (AUR)

`neomake` is available from the [AUR](https://wiki.archlinux.org/title/Arch_User_Repository). With [yay](https://github.com/Jguer/yay) or [paru](https://github.com/Morganamilo/paru):

```bash
yay -S neomake
# or
paru -S neomake
```

### GitHub Actions (reusable workflow)

Other repositories can call the reusable workflow to install `neomake`
from this repo and run it against the caller’s checkout:

```yaml
jobs:
  ci:
    uses: theJoeLinuxFoundation/neomake/.github/workflows/reusable-neomake.yml@main
    with:
      neomake-ref: main
      config-path: neomake.toml
```

See [`.github/workflows/reusable-neomake.yml`](.github/workflows/reusable-neomake.yml)
for all inputs (`working-directory`, `tasks`, `concurrency`, `extra-args`, …).

### Platform support

`neomake` runs on Linux, macOS, and Windows. Task commands are passed
to a shell whose choice depends on the host:

- **Unix-likes** — `sh -c <command>`.
- **Windows** — `pwsh -NoProfile -NoLogo -Command <command>` if
  PowerShell 7+ is on `PATH`, otherwise `powershell.exe` (Windows
  PowerShell 5.1), otherwise `cmd.exe /S /C` as a last-resort fallback.

Set `NEOMAKE_SHELL` to override; its value is whitespace-tokenized as
argv and the user's command is appended as the final argument:

```bash
NEOMAKE_SHELL="bash -c"          neomake run     # force bash
NEOMAKE_SHELL="cmd /S /C"        neomake run     # force cmd.exe on Windows
```

> **Cache compatibility.** The cache format is `neomake-cache-v3`: the
> key now covers a task's name, shell, working directory and output
> patterns as well. Entries written by an older neomake miss once after
> upgrading; `neomake cache clean` reclaims their space.

> **Windows PowerShell 5.1 redirection.** `>` writes UTF-16 + BOM by
> default in 5.1 (this is fixed in pwsh / PowerShell 7+). For
> deterministic byte-level output, prefer `pwsh` or use
> `Set-Content -Encoding ascii` / `-Encoding utf8NoBOM` instead.

## Quickstart

Create `neomake.toml`:

```toml
[tasks.build]
command = "cargo build --release"
inputs  = ["src/**/*.rs", "Cargo.toml"]
outputs = ["target/release/myapp"]

[tasks.test]
command = "cargo test"
deps    = ["build"]
```

Then:

```bash
neomake run               # runs every task (test → build)
neomake run build         # builds just `build` and its deps
neomake list              # shows the DAG in topological order
neomake cache status      # prints cache entry count and size
neomake cache clean       # empties the cache directory
```

### Task fields

| Field | Meaning |
|---|---|
| `command` | The shell command (required). |
| `deps` | Tasks that must succeed first. |
| `inputs`, `outputs` | Glob patterns; inputs are hashed into the cache key, outputs checked on a hit. |
| `env`, `env_clear` | Variables to set; start from an empty environment. |
| `cwd` | Working directory, relative to the project root. |
| `shell` | `"bash -euo pipefail -c"` or an argv array; the command is appended. |
| `timeout`, `retries` | `"10m"`, `"1h30m"`, `"500ms"` or seconds; repeat a failed attempt this often. |
| `cache` | `false` to always run (the default for tests). |
| `allow_failure` | A failure does not fail the run. |
| `requires` | `{ commands = [...], files = [...], env = [...] }`: when unmet, the task is skipped with the reason instead of failing. |
| `skip` | A reason to skip the task (or `true`). |
| `exclusive` | Named locks: tasks naming the same one never overlap. |
| `interactive` | Run alone, attached to the terminal. |
| `description`, `tags` | For people, and for selecting tasks. |

`[settings]` in the top-level file sets `state_dir` (where the cache,
runs and logs go; `.neomake` by default), `default_timeout`, `shell` and
`keep_runs`.

Every run gets a directory `<state_dir>/runs/<time>/` holding one log
per task under `logs/` and `report.json`, a machine-readable account of
the run; `runs/latest` names the newest. Every command runs in its own
process group, which is stopped as a whole at a timeout or on Ctrl-C.

### Tests

`[tests.*]` are tasks that check something: they always run (unless
`cache = true`), and may be written as steps that check each command's
exit status and output, capture values for later steps, answer a
program's questions on a pseudo-terminal, and come in matrix variants.
A failure names the step and the line it is written on. See
[docs/testing.md](docs/testing.md).

```toml
[tests.cli]
deps = ["dave"]
steps = [
  { section = "help" },
  { run = 'dave --help', contains = "Usage:" },
  { run = 'dave isntall', exit = "nonzero", stream = "combined", contains = "did you mean 'install'?" },
]
```

### CLI reference

```
neomake [OPTIONS] <SUBCOMMAND>

Subcommands:
  run [PATTERN]...         Run tasks and tests (defaults to everything)
  test [PATTERN]...        Run tests (defaults to every test)
  list [--tests]           Print tasks and tests in the order they run
  check                    Check the configuration without running anything
  log [TASK] [--run ID]    Print a task's log from the last run
  cache clean              Remove every cache entry
  cache status             Print cache size and entry count

Options:
  -c, --config <FILE>      Path to config; auto-detects neomake.toml or neomake.tomlx
      --concurrency <N>    Max parallel commands (default: num_cpus)
      --no-cache           Treat every task as a cache miss
  -q, --quiet              Show only the output of what fails
  -v, --verbose            Show every step as it passes

run and test options:
      --tag <TAG>          Only what has this tag (repeatable)
      --exclude-tag <TAG>  Leave out what has this tag (repeatable)
      --dry-run            Show what would run, be up to date or be skipped
      --fail-fast          Stop at the first failure
      --timeout-scale <F>  Multiply every timeout by F
      --output <MODE>      stream, grouped or failures
      --junit <FILE>       Write JUnit XML of the tests to FILE
      --json <FILE>        Also write the run's report to FILE
      --tail <N>           Lines of a failed task's output to show (default 20)
```

Exit status: 0 when every task succeeded, was cached or was skipped; 1
when one failed or timed out; 130 when interrupted.

Set `NEOMAKE_LOG=debug` to turn on `tracing` diagnostics.

## TOMLX at a glance

| Feature               | Syntax                                                      |
|-----------------------|-------------------------------------------------------------|
| Variable declaration  | `$profile = "release"`                                      |
| Variable reference    | `${profile}` (in strings) / `$profile` (in expressions)     |
| String interpolation  | `"target/${profile}/app"`                                   |
| String concatenation  | `"cargo build" + " --release"`                              |
| Conditional value     | `if ${ci} { "make ci" } else { "make" }`                    |
| Environment access    | `env("PROFILE", "debug")`                                   |
| File globbing         | `glob("src/**/*.rs")`                                       |
| Command execution     | `exec("git rev-parse HEAD")`                                |
| Git clone             | `git("github:owner/repo", "v1.0.0", "dest/%")`               |
| File download         | `fetch("https://url.com/file", "dest/%")`                    |
| Path helpers          | `exists("x")`, `which("qemu-system-x86_64")`, `join(a, b)`  |
| Split configurations  | `include = ["tests/*.tomlx"]`                               |
| Shell left untouched  | `command = 'echo "${HOME}" $?'` (literal strings)           |

See [`docs/tomlx-spec.md`](docs/tomlx-spec.md) for the full grammar and
semantics, and [`examples/advanced.tomlx`](examples/advanced.tomlx) for
a complete example.

## Architecture

```
┌───────────────────┐        ┌──────────────────────────┐
│  *.toml / *.tomlx │──────▶│   neomake-tomlx          │
└───────────────────┘        │  (lexer / parser / eval) │
                             └───────────┬──────────────┘
                                         │ toml::Value
                                         ▼
                             ┌──────────────────────────┐
                             │   neomake-core           │
                             │   TaskSet → DAG          │
                             │         │                │
                             │         ▼                │
                             │   Executor (tokio)       │
                             │         │                │
                             │         ▼                │
                             │   Cache (.neomake/cache) │
                             └──────────────────────────┘
```

- **DAG construction** uses Kahn's algorithm for topological sorting; if
  a cycle is detected, an iterative DFS recovers the concrete cycle
  path (e.g. `a -> b -> c -> a`) and reports it.
- **Cache keys** are SHA-256 over a versioned canonical byte layout
  (`neomake-cache-v2`) built from the command, env, sorted input file
  hashes, and upstream dep keys. See `neomake-core/src/cache.rs` for the
  exact format.
- **Parallel execution** uses one tokio task per DAG node, a per-task
  `watch` channel to propagate outcomes to dependents, and a
  `Semaphore` to cap concurrent shell commands. Cache hits bypass the
  semaphore entirely.


## License

MIT
