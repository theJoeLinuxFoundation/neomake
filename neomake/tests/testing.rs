//! End-to-end tests of tests: steps, their checks, captures, templates,
//! terminals, matrices, selection and the reports, through the binary.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

fn neomake(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_neomake"))
        .args(args)
        .current_dir(dir)
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("spawn neomake");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const TOOL: &str = r#"#!/bin/sh
case "$1" in
  version) echo "tool 1.2.3";;
  ask) printf "Continue? [Y/n] "; read a; if [ "$a" = y ]; then echo yes; else echo "Nothing was changed."; exit 1; fi;;
  *) echo "usage: tool version|ask" >&2; exit 2;;
esac
"#;

fn project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("tool"), TOOL).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("tool"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(tmp.path().join("tests")).unwrap();
    fs::write(
        tmp.path().join("neomake.tomlx"),
        r#"
$path = "bin:/usr/bin:/bin"
include = ["tests/*.tomlx"]

[settings]
state_dir = "build/neomake"

[templates.says]
params = ["what", "text"]
steps = [{ run = 'tool {{what}}', contains = "{{text}}" }]
"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("tests/cli.tomlx"),
        r#"
[tests.cli]
description = "the tool's command line"
tags = ["fast"]
env = { PATH = "${path}" }
prelude = 'quiet() { "$@" >/dev/null 2>&1; }'
steps = [
  { section = "version" },
  { run = 'tool version', capture = { version = 'tool (\S+)' } },
  { run = 'echo "v={{version}}" > "{{test.dir}}/version"; cat "$NEOMAKE_TEST_DIR/version"', equals = "v=1.2.3" },
  { use = "says", with = { what = "version", text = "1.2" } },
  { section = "mistakes" },
  { run = 'tool bogus', exit = 2, stream = "stderr", contains = "usage:" },
  { run = 'quiet tool bogus', exit = "nonzero" },
  { section = "questions" },
  { run = 'tool ask', tty = { respond = [{ on = '\[Y/n\]', send = "n\n" }] }, exit = 1, contains = "Nothing was changed." },
  { run = 'tool ask', tty = { respond = [{ on = '\[Y/n\]', send = "y\n" }], forbid = 'Nothing' }, contains = "yes" },
  { run = 'n=$(cat count 2>/dev/null || echo 0); echo $((n+1)) > count; cat count', equals = "3", wait = "10s", interval = "50ms" },
]

[tests.media]
tags = ["vm"]
matrix.media = [{ name = "cd", dev = "/dev/sr0" }, { name = "usb", dev = "/dev/sda" }]
steps = [{ run = 'echo {{matrix.media}} {{matrix.media.dev}}', matches = '^(cd /dev/sr0|usb /dev/sda)$' }]

[tests.broken]
env = { PATH = "${path}" }
steps = [
  { section = "versions" },
  { run = 'tool version', contains = "tool" },
  { run = 'tool version', contains = "2.0", name = "the version is 2.0" },
  { run = 'echo never' },
]

[tests.unmet]
requires = { commands = ["no-such-qemu-x9"] }
command = "true"
"#,
    )
    .unwrap();
    tmp
}

#[test]
fn steps_check_capture_and_answer() {
    let tmp = project();
    let (code, stdout, stderr) = neomake(tmp.path(), &["-v", "test", "cli"]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(stdout.contains("cli == questions"), "{stdout}");
    assert!(
        stdout.contains("✓ step 3/8 says › run: tool version"),
        "{stdout}"
    );
    assert!(stdout.contains("[OK     ] cli"), "{stdout}");
    let runs = tmp.path().join("build/neomake/runs/latest");
    assert_eq!(
        fs::read_to_string(runs.join("tests/cli/version")).unwrap(),
        "v=1.2.3\n",
        "the test's directory is {{test.dir}} and $NEOMAKE_TEST_DIR"
    );
}

#[test]
fn a_failing_step_is_named_with_its_line() {
    let tmp = project();
    let (code, stdout, _) = neomake(
        tmp.path(),
        &[
            "test",
            "broken",
            "--junit",
            "junit.xml",
            "--json",
            "report.json",
        ],
    );
    assert_eq!(code, 1, "{stdout}");
    assert!(
        stdout.contains(
            "broken: step 2/3 (tests/cli.tomlx:31) `the version is 2.0`: expected stdout to contain \"2.0\""
        ),
        "{stdout}"
    );
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(tmp.path().join("report.json")).unwrap()).unwrap();
    let steps = report["tasks"][0]["steps"].as_array().unwrap();
    let statuses: Vec<&str> = steps
        .iter()
        .map(|s| s["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, vec!["passed", "failed", "not_run"]);
    assert_eq!(steps[2]["section"], "versions");
    let junit = fs::read_to_string(tmp.path().join("junit.xml")).unwrap();
    assert!(junit.contains("<failure message=\"step 2/3"), "{junit}");
    assert!(junit.contains("classname=\"tests.cli\""), "{junit}");
    let (_, log, _) = neomake(tmp.path(), &["log", "broken"]);
    assert!(
        log.contains("--- step 2 failed: expected stdout to contain \"2.0\""),
        "{log}"
    );
}

#[test]
fn selection_by_pattern_matrix_and_tag() {
    let tmp = project();
    let (code, stdout, _) = neomake(tmp.path(), &["test", "--dry-run"]);
    assert_eq!(code, 0);
    let has = |words: &[&str]| {
        stdout.lines().any(|l| {
            words.iter().all(|w| {
                l.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .contains(w)
            })
        })
    };
    assert!(has(&["would run cli 8 steps"]), "{stdout}");
    assert!(has(&["would run media[media=cd] 1 step"]), "{stdout}");
    assert!(
        has(&["skip unmet requires: no-such-qemu-x9 not found"]),
        "{stdout}"
    );
    let (_, stdout, _) = neomake(tmp.path(), &["test", "--dry-run", "media"]);
    assert_eq!(stdout.lines().count(), 2, "{stdout}");
    let (_, stdout, _) = neomake(tmp.path(), &["test", "--dry-run", "--tag", "fast"]);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let (_, stdout, _) = neomake(
        tmp.path(),
        &["test", "--dry-run", "--exclude-tag", "vm", "*"],
    );
    assert!(!stdout.contains("media"), "{stdout}");
    let (code, stdout, _) = neomake(tmp.path(), &["test", "media[media=usb]"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("usb /dev/sda"), "{stdout}");
    let (code, _, stderr) = neomake(tmp.path(), &["test", "medai"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("did you mean `media`?"), "{stderr}");
}

#[test]
fn check_and_list() {
    let tmp = project();
    let (code, stdout, _) = neomake(tmp.path(), &["check"]);
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(stdout.trim(), "ok: 0 tasks and 5 tests in 2 files");
    let (_, stdout, _) = neomake(tmp.path(), &["-v", "list", "--tests"]);
    assert!(
        stdout.contains("the tool's command line  [fast]"),
        "{stdout}"
    );
    assert!(
        stdout.contains("requires, and is missing: no-such-qemu-x9 not found"),
        "{stdout}"
    );

    fs::write(
        tmp.path().join("tests/bad.tomlx"),
        "[tests.bad]\nsteps = [{ run = 'echo {{never_captured}}' }]\n",
    )
    .unwrap();
    let (code, stdout, _) = neomake(tmp.path(), &["check"]);
    assert_eq!(code, 1);
    assert!(
        stdout.contains("tests/bad.tomlx:2: `bad`: a step uses `{{never_captured}}`"),
        "{stdout}"
    );
}

#[test]
fn grouped_output_on_github_actions() {
    let tmp = project();
    let summary = tmp.path().join("summary.md");
    let out = Command::new(env!("CARGO_BIN_EXE_neomake"))
        .args(["test", "broken", "media"])
        .current_dir(tmp.path())
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("::group::media[media=cd] output"),
        "{stdout}"
    );
    assert!(
        stdout.contains("::error file=tests/cli.tomlx,line=31,title=broken::step 2/3"),
        "{stdout}"
    );
    let md = fs::read_to_string(&summary).unwrap();
    assert!(md.contains("| ❌ | `broken` |"), "{md}");
}
