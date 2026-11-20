# Testing with neomake

neomake runs tests the way it runs tasks: in dependency order, in
parallel, each with its own log. A test differs from a task in three
ways. It is not cached unless it asks to be. It may be written as
**steps** that check what each command did. And the run reports it to
people and to CI as a test: passed, failed at a named step, or skipped
for a stated reason.

This document is the reference for tests. The configuration language is
described in [tomlx-spec.md](tomlx-spec.md), and tasks' own fields in the
[README](../README.md#task-fields).

---

## 1. Tests

```toml
[tests.unit]
description = "dave's unit tests"
tags = ["fast", "ci"]
deps = ["dave"]
cwd = "dave"
command = "zig build test --summary all"
timeout = "10m"
```

A test takes every field a task does (`deps`, `env`, `cwd`, `timeout`,
`retries`, `requires`, `skip`, `tags`, …). It has either a `command` or
`steps`, not both. Tests and tasks share one namespace and may depend on
each other.

- `cache = true` lets a test be skipped when nothing it depends on has
  changed, like a task with `inputs`. By default a test always runs.
- `requires = { commands = [...], files = [...], env = [...] }`: when a
  program is not on `PATH`, a path does not exist (such as `/dev/kvm`),
  or a variable is unset, the test is **skipped**, with that reason,
  rather than failed. So is everything that depends on it.
- `skip = "reason"` skips it unconditionally, which with TOMLX can be
  conditional: `skip = if env("DAVE", "") != "" { "DAVE overrides the binary" } else { false }`.
- `allow_failure = true` reports a failure without failing the run.

Each test gets a directory for files it wants to keep, in the run's
directory: `<state_dir>/runs/<id>/tests/<name>/`. Steps see it as
`{{test.dir}}` and as `$NEOMAKE_TEST_DIR`.

---

## 2. Steps

```toml
[tests.cli-install]
deps = ["cli-repo"]
cwd = "build/cli-test"
env = { TERM = "dumb" }
prelude = '''
D=(dave --root "$PWD/root")
'''
steps = [
  { section = "install pulls in dependencies" },
  { run = '"${D[@]}" install hello', matches = ['^  hello +1\.0-r1$', '^  libgreet +1\.0-r1 +\(dependency\)$'], contains = "2 to install." },
  { run = 'cat root/usr/bin/hello', equals = "hello v1" },
  { run = '"${D[@]}" install hello', contains = "Already installed." },
  { run = 'dave isntall hello', exit = "nonzero", stream = "combined", contains = "did you mean 'install'?" },
]
```

Each step is an inline table with one action:

| Action | Does |
|---|---|
| `run = '…'` | Runs a shell script (on the host; in a machine test, in the machine). |
| `host = '…'` | Runs a shell script on the host. |
| `section = "…"` | Starts a section: a heading for the steps after it, in progress output and reports. |
| `use = "template"` | Inserts a template's steps (§3). |

Every step that runs a command is its own shell process, started in the
test's `cwd` with its `env` and `shell`. The test's `prelude`, if any, is
put in front of each one, so it is the place for shell functions and
variables the steps share. Variables a step sets do not reach the next
step; files do.

**Write scripts in literal strings** (`'…'` or `'''…'''`). In basic
strings (`"…"`), TOMLX would read `${NAME}` as one of its own variables.

### Checks

A step passes when its command exits with status 0, unless it says
otherwise:

| Key | Passes when |
|---|---|
| `exit = 2` / `"nonzero"` / `"any"` | the exit status is that. |
| `contains = "…"` or `["…", …]` | the output contains each. |
| `lacks = "…"` or `[…]` | the output contains none of them. |
| `matches = '…'` or `[…]` | the output matches each regular expression. Patterns are in multi-line mode: `^` and `$` match at every line, as `grep -E` does. |
| `equals = "…"` | the output is exactly that, leading and trailing whitespace aside. |
| `stream = "stdout"` / `"stderr"` / `"combined"` | (which output the checks look at; `stdout` by default.) |

The first step that fails ends the test. The failure names the step, the
file and line it is written on, and what was expected:

```
[FAILED ] cli-install  step 4/5 (tests/cli.tomlx:23) `run: dave isntall hello`: expected the output to contain "did you mean 'install'?"
```

A step can be named with `name = "…"`, which is then used instead of its
command in progress and reports.

### Captures and placeholders

`capture` sets variables from a step's output, for later steps:

```toml
{ run = 'cat /efi/loader/entries/joe-1.conf', capture = { kernel = '^linux (\S+)$', options = '^options (.+)$' } },
{ run = 'dave version', capture = "version" },   # the whole output, trimmed
{ run = 'test "$(uname -r)" = "{{version}}"' },
```

With a pattern, the variable is the first group of the first match (or
the whole match, if the pattern has no group); a step whose pattern does
not match fails.

`{{ name }}` placeholders may appear in any string of a step. Besides
captured variables, every test has:

| Placeholder | Value |
|---|---|
| `{{test.dir}}` | the test's directory of files (absolute) |
| `{{test.name}}` | the test's name |
| `{{run.dir}}` | the run's directory (absolute) |
| `{{project.root}}` | the project root (absolute) |

A placeholder nothing defines makes its step fail; `neomake check` finds
such placeholders without running anything. Text between braces that is
not a placeholder, such as Go's `{{.State}}`, is left as written, and
`\{{` is a literal `{{`.

### Waiting

`wait = "30s"` repeats a step until its checks pass, pausing `interval`
(one second by default) between attempts, and fails with the last
attempt's reason when the time is up:

```toml
{ run = 'systemctl is-active systemd-bless-boot.service', equals = "active", wait = "30s" },
```

### Timeouts

The test's `timeout` covers all of its steps together. A step's own
`timeout = "5m"` can only shorten that.

### Terminals

Some programs behave differently on a terminal: they ask questions, or
use colour. `tty` runs a step on a pseudo-terminal (80×24, Unix hosts
only), where stdout and stderr are one stream:

```toml
{ run = 'dave --root tty install hello', tty = { respond = [{ on = '\[Y/n\]', send = "n\n" }] }, exit = "nonzero", contains = "Nothing was changed." },
{ run = 'dave --root tty install tree', tty = { forbid = '\[Y/n\]' } },
{ run = 'dave --root tty info hello', tty = true, contains = "\u001b[1m" },
```

`respond` types `send` once, the first time `on` matches what the command
has printed; `forbid` fails the step if a pattern ever appears.

---

## 3. Templates

A template is a list of steps with parameters, defined in any file of
the configuration:

```toml
[templates.expect-subvol]
params = ["n"]
steps = [
  { run = 'cat /proc/cmdline', matches = 'rootflags=subvol=joe/deploy/{{n}}\b' },
]
```

A step uses it with `use` and gives every parameter in `with`:

```toml
{ use = "expect-subvol", with = { n = 3 } },
```

Its steps are copied in with `{{n}}` replaced. They are reported at the
line of the `use` step and named after the template
(`expect-subvol › run: cat /proc/cmdline`). Templates may use templates.

---

## 4. Matrices

A matrix turns one definition into a variant per combination of values:

```toml
[tests.deployments]
deps = ["{{matrix.base}}-image"]
matrix = { base = ["scaffold", "joe"] }
steps = [...]
```

defines `deployments[base=scaffold]` and `deployments[base=joe]`. In every
string of the definition, `{{matrix.base}}` is the variant's value. With
several keys, every combination is a variant
(`live[base=joe,media=cd]`). A value may be a table with a `name`, which
names the variant, and further fields:

```toml
matrix.media = [
  { name = "cd", device = "/dev/sr0" },
  { name = "usb", device = "/dev/sda" },
]
```

`{{matrix.media}}` is then `cd` or `usb`, and `{{matrix.media.device}}`
the device. Depending on a matrix's name (`deps = ["deployments"]`)
depends on all of its variants. Tasks may have matrices too.

---

## 5. Services

A service is something tests need running while they run: a repository
served over HTTP, a daemon. It is declared once and used by name:

```toml
[services.joe-repo]
description = "build/repo, as guests see it"
serve = { dir = "build/repo", listen = [{ addr = "127.0.0.1:8643" }] }

[services.test-repo]
deps = ["deployments-repo"]
command = "some-server --port 8642 build/deployments-repo"
ready = { tcp = "127.0.0.1:8642" }
ready_timeout = "30s"

[tests.deployments]
services = ["joe-repo", "test-repo"]
```

A service starts when the first task or test that uses it is about to
run (after the tasks in the service's own `deps`, which every user
therefore depends on), and stops when the last one has ended. Before it
starts, the address it will listen on is checked: if something already
answers there, every user fails with a message naming the address,
instead of running against whatever that is. A service that exits, or is
not ready within `ready_timeout` (30 seconds by default), fails its users
with the last lines it printed. Each service writes its own log,
`logs/service-NAME.log`.

### The file server

`serve` is a static file server built into neomake: `GET` and `HEAD` of
the files in `dir`, with `404 Not Found` for anything else, as Python's
`http.server` answers. It listens on every address in `listen` (by
default one free port on `127.0.0.1`). Port 0 picks a free port. A
listener with `tls = { cert = "…", key = "…" }` (PEM files) speaks HTTPS:

```toml
[services.cli-net]
deps = ["cli-certs"]
serve = { dir = "build/cli-test/repo", listen = [
  { addr = "127.0.0.1:0" },
  { name = "here", addr = "127.0.0.1:0", tls = { cert = "build/cli-test/here.pem", key = "build/cli-test/here.key" } },
] }
```

Its log has one line per request.

### Commands

A `command` service is a command that keeps running, with one way to
tell it is ready: `ready = { tcp = "host:port" }` (it accepts
connections), `{ file = "path" }` (the file exists, relative to its
`cwd`), `{ log = 'pattern' }` (a line of its output matches) or
`{ http = "http://…" }` (a request gets an answer). Without `ready`, it
is ready as soon as it starts. It runs in a process group of its own,
which is stopped with SIGTERM, then SIGKILL, when the service stops.

### Where services listen

Commands and steps of the tasks that use a service find it through
placeholders:

| Placeholder | Value |
|---|---|
| `{{services.NAME.port}}` | the port of its first address |
| `{{services.NAME.addr}}` | `127.0.0.1:8643` |
| `{{services.NAME.url}}` | `http://127.0.0.1:8643` (or `https://`) |
| `{{services.NAME.LISTENER.port}}`, `.addr`, `.url` | the same, for a named listener |

A command service provides these when it is ready by `tcp` or `http`.

---

## 6. Running tests

```
neomake test                     every test, and what they depend on
neomake test cli-* smoke         tests named so, or matching * and ?
neomake test deployments         every variant of a matrix
neomake test 'deployments[base=joe]'
neomake test --tag vm --exclude-tag slow
neomake test --dry-run           what would run, be up to date, or be skipped, and why
neomake run                      tasks and tests alike
```

Patterns are names in which `*` matches any run of characters and `?` any
one; brackets are part of names, not character classes. Tags narrow what
the patterns chose; what the chosen tests depend on always runs.

| Option | Effect |
|---|---|
| `--fail-fast` | stop everything at the first failure |
| `--timeout-scale 2` | multiply every timeout (for a slow host) |
| `--output stream` | every line as it comes, prefixed with its task (the default) |
| `--output grouped` | each task's output together, when it ends (the default on GitHub Actions) |
| `--output failures`, `-q` | only the last lines of what fails, at the end |
| `-v` | every step as it passes, not only failing ones |
| `--junit FILE` | JUnit XML for CI |
| `--json FILE` | the run's report |

Other commands:

```
neomake list --tests -v          tests, with where they are defined and whether their requirements are met
neomake check                    the configuration's mistakes, without running anything
neomake log cli-install          a test's log from the last run (--run ID for another)
```

---

## 7. What a run leaves

Every run has a directory, `<state_dir>/runs/<id>/` (`runs/latest` names
the newest), which keeps:

- `logs/<name>.log`: everything each task and test printed, with a line
  before each step saying which it is and where it is written, and why it
  failed; `logs/service-<name>.log` for each service;
- `tests/<name>/`: each test's own files;
- `report.json`: every task and test with its status, reason, duration,
  attempts, log, the last lines of each failure, and each test's steps.

The newest `keep_runs` runs are kept (20 by default, set in
`[settings]`).

### JUnit

`--junit FILE` writes one `<testcase>` per test, with `classname` taken
from the file the test is defined in (`tests/cli.tomlx` gives
`tests.cli`), `file` and `line` attributes, its steps in `<system-out>`,
and `<skipped>` with the reason for skipped tests. A task that failed is
included as an `<error>`, since the tests after it could not run.

### GitHub Actions

When `GITHUB_ACTIONS` is `true`, output is grouped by task into
collapsible groups, every failure is annotated at the failing step's file
and line, and a table of what did not pass is appended to the job's
summary.
