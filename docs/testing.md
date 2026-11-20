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
  rather than failed. So is everything that depends on it. Programs are
  looked for on the `PATH` the test's `env` sets, if it sets one. What
  only skipped tests depend on is not run at all, so a host without KVM
  does not build images for machine tests it will skip.
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
variables the steps share; its placeholders are filled in as the steps'
are. Variables a step sets do not reach the next
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

A step with `soft = true` does not end the test when it fails: the
failure is recorded, the steps after it run, and the test fails when
they are done, listing every soft step that failed. It suits a row of
independent checks, such as each of a desktop's applications starting,
where knowing all that fail is worth more than stopping at the first.
On a `use` step, `soft = true` makes each of the template's steps soft.

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

## 6. Machines

A machine test runs its steps in a QEMU virtual machine, over its serial
console. The machine is described once:

```toml
[machines.scaffold]
disk = "build/images/scaffold.img"
memory = "2G"
cpus = 4
console = { prompt = "joe# ", banner = "BdsDxe: starting ", setup = "dmesg -n 1; stty cols 250 rows 50" }

[machines.live]
use = "scaffold"                      # everything not set here comes from scaffold
disks = [{ size = "40G" }]            # an empty disk to install onto
media = [{ file = "build/images/joe-live.iso", as = "cd" }]
memory = "6G"
display = true
```

and a test names it:

```toml
[tests.smoke]
deps = ["scaffold-image"]
machine = "scaffold"                  # or { use = "scaffold", memory = "4G" }
timeout = "10m"
steps = [
  { run = 'cat /proc/cmdline', matches = 'rootflags=subvol=joe/deploy/1\b', lacks = "initrd=" },
  { run = 'findmnt -rno FSTYPE,OPTIONS /', matches = '^btrfs \S*\brw\b' },
  { reboot = {} },
  { run = 'systemctl is-system-running --wait', exit = "any", timeout = "3m" },
]
```

The machine boots before the first step (unless the test says
`autoboot = false`): neomake waits for the console's `banner` (what the
firmware prints as it starts a boot), then for its `prompt`, then runs
its `setup`. The guest must log a shell in on its first serial port
(`ttyS0`); joe Linux's `--test` images do.

### What a machine is

| Field | Meaning |
|---|---|
| `disk` | The image it boots from. Each run gets its own copy (a reflink on btrfs, a sparse copy elsewhere); the image itself is never changed. |
| `disks = [{ file, size, name }]` | Further disks: a copy of `file` (grown to `size`), or an empty disk of `size`. `{{machine.disks.NAME}}` is a named disk's path. |
| `media = [{ file, as = "cd" or "usb", name }]` | Removable media, first in the boot order. |
| `memory`, `cpus` | `"2G"` and `2` by default. |
| `kvm` | `"require"` (the default: on a host without a usable `/dev/kvm`, the test is skipped with that reason), `"prefer"` (emulate without it, slowly) or `"off"`. |
| `firmware` | `"uefi"` (the default: OVMF, found where Arch, Debian, Ubuntu and Fedora install it), `{ code = "…", vars = "…" }`, or `"bios"`. Each run gets its own copy of the variable store. |
| `display` | A virtio GPU, keyboard and tablet, and screenshots. |
| `shares = [{ dir, tag, optional }]` | Host directories shared over virtiofs (mounted in the guest with `mount -t virtiofs TAG …`); `virtiofsd` runs as the user. Without `virtiofsd`, a test whose machine shares a directory is skipped, unless the share is `optional`: then the machine starts without it, and the test's log says so. |
| `net` | User-mode networking, on by default: the host is `10.0.2.2` to the guest. |
| `console` | `prompt` (required), `banner`, `setup`, and the `reboot` and `poweroff` commands (`systemctl reboot` and `systemctl poweroff` by default). |
| `boot_timeout`, `command_timeout` | How long a boot (3 minutes) and a command (2 minutes) may take, unless a step says otherwise. |
| `qemu`, `extra_args`, `env` | The QEMU program, further arguments, and its environment. |
| `use` | Another machine this one builds on. |

A test whose machine cannot run on the host, because QEMU, KVM, the
firmware or virtiofsd is missing, is skipped with the reason.
`neomake --no-kvm` treats KVM as missing. At most `max_machines`
machine tests run at once (2, set in `[settings]` or with `--machines`).

### Steps in a machine

`run` runs in the machine's shell; `host` runs on the host, as in any
test, so a test can change what the machine will download while the
machine runs. The rest drive the machine:

| Step | Does |
|---|---|
| `{ expect = 'pattern', times = 3, groups = ["joe/deploy/4", "fail"], capture = ["subvol", "result"] }` | Waits for the console to show the pattern (`times` times over), checks its groups and captures them. |
| `{ send = "text\n" }` | Types text on the console as it is. |
| `{ reboot = {} }`, `{ reboot = { wait = false } }` | Reboots cleanly, and waits for the next boot unless told not to (to `expect` what the shutdown and boot print). |
| `{ power_cut = {} }` | Pulls the plug and starts the machine again (and waits, unless `wait = false`). |
| `{ wait_boot = true }`, `{ wait_prompt = true }` | Waits for a boot, or for the prompt. |
| `{ kill = true }`, `{ start = true }`, `{ boot = true }`, `{ shutdown = true }` | Pulls the plug; starts without waiting; starts and waits for the boot; powers off cleanly. |
| `{ write = "/usr/local/bin/x", content = "…", mode = "755" }` | Writes a file in the machine; `from = "tests/vm/files/x"` takes it from a host file instead. |
| `{ screenshot = "gdm.png" }` | Saves what the display shows, in the test's directory. |
| `{ attach = { file, as, name } }`, `{ detach = "medium" }` | Changes the media, from the next start on. |

A command runs in the console's shell, which keeps what it defines until
the next boot. A command that spans lines, is long, or holds a `#` is
written to a file in the machine's `/tmp` and evaluated there as if it
had been typed (`eval "$(cat FILE)"`, which a system that refuses to
source unsealed scripts still allows), so a multi-line `run` can define
shell functions for later steps. A command must not `exit` the shell,
which would log the console out, nor `return` outside a function. Output on a serial
console is one stream, so a guest command's checks see stdout and stderr
together.

`on_boot` is shell text run after every boot (a good place for
functions the steps share), and `on_failure` lists steps run when a step
has failed, to record what shows why: `run`, `host` and `screenshot`
steps. Their output goes to the test's log, and their own failures are
only noted there:

```toml
on_boot = '''
expect_subvol() { grep -q "rootflags=subvol=joe/deploy/$1\b" /proc/cmdline; }
'''
on_failure = [
  { run = 'journalctl -b --no-pager | tail -n 50' },
  { screenshot = "failed.png" },
]
```

Placeholders of machine tests: `{{machine.name}}`, `{{machine.dir}}`
(where its disks and logs are), `{{machine.disk}}` (the copy of its first
disk) and `{{machine.console}}` (its console log).

### What a machine test leaves

In the test's directory: `console.log` (everything the machine printed),
`console.timed.log` (each line with the seconds since QEMU started),
`qemu.log` (QEMU's command line and messages), screenshots, and, if the
test failed, the copies of its disks, so that what the machine left can
be looked at. A failure caused by the machine quotes the console's last
output.

### On the terminal

```
neomake machine list
neomake machine run scaffold                    a fresh copy of its disks, console on this terminal (Ctrl-A X quits)
neomake machine run desktop --persist build/vm  a machine that keeps its disks from one run to the next
neomake machine run live --media build/images/joe-live.iso
```

---

## 7. Running tests

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

## 8. What a run leaves

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
