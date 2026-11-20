//! End-to-end tests of machine tests, with a stand-in for QEMU
//! (`tests/fixtures/fake-qemu`): a shell behind a firmware banner and a
//! `joe# ` prompt, which `systemctl reboot` starts over.

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

fn project(tests: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    fs::create_dir(tmp.path().join("fake")).unwrap();
    for f in ["fake-qemu", "fake-qemu.rc"] {
        fs::copy(fixtures.join(f), tmp.path().join("fake").join(f)).unwrap();
    }
    fs::write(
        tmp.path().join("neomake.toml"),
        format!(
            r#"
[machines.fake]
qemu = "fake/fake-qemu"
kvm = "off"
firmware = "bios"
disk = "images/base.img"
console = {{ prompt = "joe# ", banner = "BdsDxe: starting ", setup = "dmesg -n 1; stty cols 250 rows 50" }}
boot_timeout = "20s"
command_timeout = "10s"

[tasks.image]
command = "mkdir -p images && head -c 65536 /dev/zero > images/base.img"
outputs = ["images/base.img"]

[templates.expect-x]
params = ["v"]
steps = [{{ run = 'echo "$X"', equals = "{{{{v}}}}" }}]

{tests}
"#
        ),
    )
    .unwrap();
    tmp
}

#[test]
fn a_machine_test_runs_commands_reboots_and_loses_power() {
    let tmp = project(
        r#"
[tests.console]
deps = ["image"]
machine = "fake"
on_boot = '''
greet() { echo "hello $1"; }
X=booted
'''
steps = [
  { section = "commands" },
  { run = 'greet joe', equals = "hello joe" },
  { run = 'echo one; echo two >&2; f() { return 3; }; f', exit = 3, contains = ["one", "two"] },
  { run = '''
for i in 1 2 3; do
  echo "line $i"   # a comment, which a one-line command could not hold
done
''', matches = '^line 3$' },
  { run = 'uname -s', capture = "os" },
  { host = 'echo "{{os}} {{machine.disk}}"', contains = "disk.img" },
  { section = "files" },
  { write = "written", content = "first line\nsecond: 'quoted' $HOME\n", mode = "600" },
  { run = 'cat written; stat -c %a written 2>/dev/null || stat -f %Lp written', contains = ["second: 'quoted' $HOME", "600"] },
  { section = "boots" },
  { run = 'X=changed; echo $X', equals = "changed" },
  { reboot = {} },
  { use = "expect-x", with = { v = "booted" } },
  { send = "echo JOE-HEALTH subvol=$((1+2)) result=ok\n" },
  { expect = 'JOE-HEALTH subvol=(\d+) result=(\w+)\r?\n', groups = ["3", "ok"], capture = ["n", "result"] },
  { wait_prompt = true },
  { run = 'echo {{n}}-{{result}}', equals = "3-ok" },
  { power_cut = {} },
  { use = "expect-x", with = { v = "booted" } },
  { run = 'n=0; [ -f count ] && n=$(cat count); echo $((n+1)) > count; cat count', equals = "3", wait = "10s", interval = "100ms" },
]
"#,
    );
    let (code, stdout, stderr) = neomake(tmp.path(), &["test", "-v"]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(
        stdout.contains("✓ step 12/17 expect /JOE-HEALTH"),
        "{stdout}"
    );
    let dir = tmp.path().join(".neomake/runs/latest/tests/console");
    let args = fs::read_to_string(dir.join("fake-qemu.args")).unwrap();
    assert_eq!(
        args.lines().count(),
        3,
        "booted, rebooted, power cut: {args}"
    );
    assert!(
        args.contains("-cpu max -machine q35 -m 2G -smp 2"),
        "{args}"
    );
    assert!(
        args.contains(",format=raw,if=virtio,cache=unsafe"),
        "{args}"
    );
    assert!(
        args.contains("-display none -monitor none -serial stdio"),
        "{args}"
    );
    let console = fs::read_to_string(dir.join("console.log")).unwrap();
    assert!(console.contains("BdsDxe: starting"), "{console}");
    let timed = fs::read_to_string(dir.join("console.timed.log")).unwrap();
    assert!(timed.lines().all(|l| l.starts_with('[')), "{timed}");
    assert!(
        !dir.join("disk.img").exists(),
        "a passing test's disk copy is deleted"
    );
    let log = fs::read_to_string(tmp.path().join(".neomake/runs/latest/logs/console.log")).unwrap();
    assert!(
        log.contains("matched: JOE-HEALTH subvol=3 result=ok"),
        "{log}"
    );
}

#[test]
fn a_machine_failure_quotes_the_console_and_keeps_the_disk() {
    let tmp = project(
        r#"
[tests.never]
deps = ["image"]
machine = { use = "fake", command_timeout = "1s" }
on_failure = [{ run = 'echo gathering evidence' }]
steps = [
  { run = 'echo booted-fine' },
  { expect = 'never-printed' },
  { run = 'echo not reached' },
]
"#,
    );
    let (code, stdout, _) = neomake(tmp.path(), &["test"]);
    assert_eq!(code, 1, "{stdout}");
    assert!(
        stdout.contains("step 2/3 (neomake.toml:26) `expect /never-printed/`: nothing matched /never-printed/ within 1.0s"),
        "{stdout}"
    );
    assert!(stdout.contains("the console's last output:"), "{stdout}");
    let dir = tmp.path().join(".neomake/runs/latest/tests/never");
    assert!(
        dir.join("disk.img").exists(),
        "a failed test's disk copy is kept"
    );
    let log = fs::read_to_string(tmp.path().join(".neomake/runs/latest/logs/never.log")).unwrap();
    assert!(log.contains("gathering evidence"), "on_failure ran: {log}");
}

#[test]
fn machines_that_cannot_run_are_skipped() {
    let tmp = project(
        r#"
[tests.needs-kvm]
deps = ["image"]
machine = { use = "fake", kvm = "require" }
steps = [{ run = 'true' }]

[tests.missing-qemu]
machine = { use = "fake", qemu = "no-such-qemu-x9" }
steps = [{ run = 'true' }]
"#,
    );
    let (code, stdout, _) = neomake(tmp.path(), &["--no-kvm", "test"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("needs-kvm  (requires: machine `fake` needs KVM, and /dev/kvm is switched off (--no-kvm))"),
        "{stdout}"
    );
    assert!(
        stdout.contains("missing-qemu  (requires: no-such-qemu-x9 not found)"),
        "{stdout}"
    );
    // The plan says so too, and leaves out the image only they need.
    let (code, stdout, _) = neomake(tmp.path(), &["--no-kvm", "test", "--dry-run"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("skip        needs-kvm     requires: machine `fake` needs KVM"),
        "{stdout}"
    );
    assert!(!stdout.contains("image"), "{stdout}");
}

#[test]
fn machine_steps_belong_to_machine_tests() {
    let tmp = project("[tests.t]\nsteps = [{ reboot = true }]\n");
    let (code, _, stderr) = neomake(tmp.path(), &["check"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("`reboot` drives a machine, and this test has none"),
        "{stderr}"
    );
}
