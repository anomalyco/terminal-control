#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct NamedSession {
    binary: &'static str,
    runtime: PathBuf,
    name: &'static str,
}

impl NamedSession {
    fn new(label: &str, name: &'static str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let runtime =
            std::env::temp_dir().join(format!("termctrl-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir(&runtime).unwrap();
        Self {
            binary: env!("CARGO_BIN_EXE_termctrl"),
            runtime,
            name,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.binary);
        command.env("TERMCTRL_RUNTIME_DIR", &self.runtime);
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}

impl Drop for NamedSession {
    fn drop(&mut self) {
        let _ = self.command().args(["stop", self.name]).output();
        let _ = fs::remove_dir_all(&self.runtime);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn parsed_hex(output: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(output)
        .split_ascii_whitespace()
        .filter(|token| token.len() == 2 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(|token| u8::from_str_radix(token, 16).unwrap())
        .collect()
}

fn wait_for_logs(session: &NamedSession, expected_len: usize) -> Output {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let output = session.output(&["logs", session.name, "--ansi"]);
        if output.status.success() && parsed_hex(&output.stdout).len() == expected_len {
            return output;
        }
        if Instant::now() >= deadline {
            panic!(
                "session did not emit {expected_len} expected bytes; last output: {:?}",
                (
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                    parsed_hex(&output.stdout).len(),
                )
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn named_session_survives_launcher_process_group_hangup() {
    let session = NamedSession::new("daemon-detach", "detach-test");
    let ready = session.runtime.join("launcher-ready");
    let mut launcher = Command::new("/bin/sh");
    launcher
        .arg("-c")
        .arg("\"$1\" start \"$2\" -- /bin/sh -c 'printf READY; sleep 30' && : > \"$3\" && sleep 30")
        .arg("termctrl-launcher")
        .arg(session.binary)
        .arg(session.name)
        .arg(&ready)
        .env("TERMCTRL_RUNTIME_DIR", &session.runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        launcher.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut launcher = launcher.spawn().unwrap();
    let process_group = launcher.id() as libc::pid_t;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            launcher.try_wait().unwrap().is_none(),
            "launcher exited before starting the named session"
        );
        assert!(Instant::now() < deadline, "launcher did not become ready");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(unsafe { libc::killpg(process_group, libc::SIGHUP) }, 0);

    let status = launcher.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGHUP));

    let output = session.output(&["status", session.name, "--json"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"state\": \"running\""));
}

#[test]
fn click_and_drag_send_exact_sgr_mouse_events() {
    let session = NamedSession::new("mouse-input", "mouse-test");
    let output = session.output(&[
        "start",
        session.name,
        "--",
        "/bin/sh",
        "-c",
        "stty raw -echo; printf READY; od -An -tx1 -v -N 58",
    ]);
    assert_success(&output);
    assert_success(&session.output(&["wait", session.name, "READY"]));
    assert_success(&session.output(&["click", session.name, "12", "4"]));
    assert_success(&session.output(&[
        "drag",
        session.name,
        "0",
        "0",
        "2",
        "0",
        "--steps",
        "2",
        "--pace-ms",
        "0",
    ]));

    let output = wait_for_logs(&session, 58);
    assert_eq!(
        parsed_hex(&output.stdout),
        [
            b"\x1b[<0;13;5M".as_slice(),
            b"\x1b[<0;13;5m".as_slice(),
            b"\x1b[<0;1;1M".as_slice(),
            b"\x1b[<32;2;1M".as_slice(),
            b"\x1b[<32;3;1M".as_slice(),
            b"\x1b[<0;3;1m".as_slice(),
        ]
        .concat()
    );
}

#[test]
fn live_mouse_rejects_coordinates_outside_the_actual_viewport_before_recording() {
    let session = NamedSession::new("mouse-bounds", "mouse-bounds-test");
    let recording = session.runtime.join("mouse-bounds.termctrl");
    let output = session.output(&[
        "start",
        session.name,
        "--cols",
        "2",
        "--rows",
        "1",
        "--record",
        recording.to_str().unwrap(),
        "--record-pointer",
        "--",
        "/bin/sh",
        "-c",
        "stty raw -echo; cat >/dev/null",
    ]);
    assert_success(&output);
    thread::sleep(Duration::from_millis(50));

    for args in [
        vec!["click", session.name, "2", "0"],
        vec!["drag", session.name, "0", "0", "2", "0", "--pace-ms", "0"],
    ] {
        let output = session.output(&args);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("outside target viewport 2x1"),
            "{args:?}: {stderr}"
        );
    }

    assert_success(&session.output(&["stop", session.name]));
    let entries = fs::read_to_string(recording).unwrap();
    assert!(!entries.contains("\"type\":\"pointer\""), "{entries}");
    assert!(!entries.contains("\"origin\":\"client\""), "{entries}");
}

#[test]
fn live_send_accepts_shift_enter_modifier_chord() {
    let session = NamedSession::new("modifier-input", "modifier-input-test");
    assert_success(&session.output(&[
        "start",
        session.name,
        "--",
        "/bin/sh",
        "-c",
        "stty raw -echo; printf READY; od -An -tx1 -v -N 7",
    ]));
    assert_success(&session.output(&["wait", session.name, "READY"]));
    assert_success(&session.output(&["send", session.name, "shift+enter"]));

    let output = wait_for_logs(&session, 7);
    assert_eq!(parsed_hex(&output.stdout), b"\x1b[13;2u");
}

#[test]
fn pointer_enabled_session_records_structured_events_and_renders_live_and_replay() {
    let session = NamedSession::new("pointer-recording", "pointer-recording-test");
    let recording = session.runtime.join("pointer.termctrl");
    let before = session.runtime.join("before.svg");
    let live = session.runtime.join("live.svg");
    let replay = session.runtime.join("replay.svg");
    let live_faded = session.runtime.join("live-faded.svg");
    let live_persistent = session.runtime.join("live-persistent.svg");
    let replay_faded = session.runtime.join("replay-faded.svg");
    let replay_persistent = session.runtime.join("replay-persistent.svg");
    let output = session.output(&[
        "start",
        session.name,
        "--record",
        recording.to_str().unwrap(),
        "--record-pointer",
        "--",
        "/bin/sh",
        "-c",
        "stty raw -echo; printf READY; cat >/dev/null",
    ]);
    assert_success(&output);
    assert_success(&session.output(&["wait", session.name, "READY"]));
    assert_success(&session.output(&[
        "save",
        session.name,
        "--format",
        "svg",
        "--pointer=persistent",
        "--out",
        before.to_str().unwrap(),
    ]));
    assert!(
        !fs::read_to_string(before)
            .unwrap()
            .contains("data-termctrl-pointer=\"true\"")
    );
    assert_success(&session.output(&["click", session.name, "12", "4"]));
    assert_success(&session.output(&[
        "drag",
        session.name,
        "0",
        "0",
        "2",
        "0",
        "--steps",
        "2",
        "--pace-ms",
        "0",
    ]));
    assert_success(&session.output(&["mark", session.name, "pointer-final"]));
    assert_success(&session.output(&[
        "save",
        session.name,
        "--format",
        "svg",
        "--pointer",
        "--out",
        live.to_str().unwrap(),
    ]));
    thread::sleep(Duration::from_millis(1_300));
    assert_success(&session.output(&["mark", session.name, "pointer-idle"]));
    assert_success(&session.output(&[
        "save",
        session.name,
        "--format",
        "svg",
        "--pointer",
        "--out",
        live_faded.to_str().unwrap(),
    ]));
    assert_success(&session.output(&[
        "save",
        session.name,
        "--format",
        "svg",
        "--pointer=persistent",
        "--out",
        live_persistent.to_str().unwrap(),
    ]));
    assert_success(&session.output(&["stop", session.name]));
    assert_success(&session.output(&[
        "save",
        "--recording",
        recording.to_str().unwrap(),
        "--at-marker",
        "pointer-final",
        "--format",
        "svg",
        "--pointer",
        "--out",
        replay.to_str().unwrap(),
    ]));
    assert_success(&session.output(&[
        "save",
        "--recording",
        recording.to_str().unwrap(),
        "--at-marker",
        "pointer-idle",
        "--format",
        "svg",
        "--pointer",
        "--out",
        replay_faded.to_str().unwrap(),
    ]));
    assert_success(&session.output(&[
        "save",
        "--recording",
        recording.to_str().unwrap(),
        "--at-marker",
        "pointer-idle",
        "--format",
        "svg",
        "--pointer=persistent",
        "--out",
        replay_persistent.to_str().unwrap(),
    ]));

    let entries: Vec<serde_json::Value> = fs::read_to_string(&recording)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(entries[0]["version"], 2);
    let phases = entries
        .iter()
        .filter(|entry| entry["type"] == "pointer")
        .map(|entry| entry["phase"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        phases,
        ["press", "release", "press", "move", "move", "release"]
    );
    for path in [live, replay] {
        let svg = fs::read_to_string(path).unwrap();
        assert!(svg.contains("data-termctrl-pointer=\"true\""), "{svg}");
        assert!(
            svg.contains("data-termctrl-pointer-click=\"true\""),
            "{svg}"
        );
    }
    for path in [live_faded, replay_faded] {
        let svg = fs::read_to_string(path).unwrap();
        assert!(!svg.contains("data-termctrl-pointer=\"true\""), "{svg}");
    }
    for path in [live_persistent, replay_persistent] {
        let svg = fs::read_to_string(path).unwrap();
        assert!(svg.contains("data-termctrl-pointer=\"true\""), "{svg}");
        assert!(
            !svg.contains("data-termctrl-pointer-click=\"true\""),
            "{svg}"
        );
    }
}

#[test]
fn tape_play_runs_state_aware_demo_and_records_exact_input() {
    let session = NamedSession::new("tape-play", "tape-play-test");
    let tape = session.runtime.join("demo.tape");
    let recording = session.runtime.join("demo.termctrl");
    fs::write(
        &tape,
        r#"# comments and quoted # text are both supported
Session tape-play-test
Viewport 72 12
Cell 8 16
MaxBytes 1048576
Cwd "."
Env DEMO_ENV "quoted # value"
Record "demo.termctrl"
Pointer on
Color never
Launch "/bin/sh" "-c" "stty raw -echo; printf READY:%s:%s \"$DEMO_ENV\" \"$PWD\"; (while [ ! -f action-ready ]; do sleep 0.01; done; printf FIXTURE) & od -An -tx1 -v"
Wait "READY:quoted # value" Timeout 2s
Type "hello" Pace 1ms
Key enter shift+enter
Move 5 2 Steps 4 Pace 0ms
Move 9 2 Steps 2 Pace 0ms
Click 12 4
RightClick 10 4
Move 14 4 Steps 2 Pace 0ms
Drag 0 0 2 0 Steps 2 Pace 0ms
Move 4 0 Steps 2 Pace 0ms
Action "/usr/bin/touch" "action-ready"
Wait FIXTURE Timeout 2s
Mark complete
Sleep 10ms
Stop
"#,
    )
    .unwrap();

    let output = session.output(&["play", tape.to_str().unwrap()]);
    assert_success(&output);
    let receipt = String::from_utf8_lossy(&output.stdout);
    assert!(
        receipt.contains(&format!("played {}", tape.display())),
        "{receipt}"
    );
    assert!(
        receipt.contains(&format!("recording {}", recording.display())),
        "{receipt}"
    );
    assert!(session.runtime.join("action-ready").exists());
    assert!(recording.exists());
    assert!(!session.output(&["status", session.name]).status.success());

    let entries: Vec<serde_json::Value> = fs::read_to_string(&recording)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(entries[0]["type"], "header");
    assert_eq!(entries[0]["version"], 2);
    assert_eq!(entries[0]["cols"], 72);
    assert_eq!(entries[0]["rows"], 12);
    assert!(
        entries
            .iter()
            .any(|entry| { entry["type"] == "marker" && entry["name"] == "complete" })
    );
    let pointer_events = entries
        .iter()
        .filter(|entry| entry["type"] == "pointer")
        .map(|entry| {
            (
                entry["x"].as_u64().unwrap(),
                entry["y"].as_u64().unwrap(),
                entry["phase"].as_str().unwrap(),
                entry["button"].as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        pointer_events,
        [
            (5, 2, "move", None),
            (7, 2, "move", None),
            (9, 2, "move", None),
            (12, 4, "press", None),
            (12, 4, "release", None),
            (10, 4, "press", Some("secondary")),
            (10, 4, "release", Some("secondary")),
            (12, 4, "move", None),
            (14, 4, "move", None),
            (0, 0, "press", None),
            (1, 0, "move", None),
            (2, 0, "move", None),
            (2, 0, "release", None),
            (3, 0, "move", None),
            (4, 0, "move", None),
        ]
    );
    let client_input: Vec<u8> = entries
        .iter()
        .filter(|entry| entry["type"] == "input" && entry["origin"] == "client")
        .flat_map(|entry| {
            entry["bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|byte| byte.as_u64().unwrap() as u8)
        })
        .collect();
    assert_eq!(
        client_input,
        [
            b"hello\r\x1b[13;2u".as_slice(),
            b"\x1b[<35;6;3M".as_slice(),
            b"\x1b[<35;8;3M".as_slice(),
            b"\x1b[<35;10;3M".as_slice(),
            b"\x1b[<0;13;5M".as_slice(),
            b"\x1b[<0;13;5m".as_slice(),
            b"\x1b[<2;11;5M".as_slice(),
            b"\x1b[<2;11;5m".as_slice(),
            b"\x1b[<35;13;5M".as_slice(),
            b"\x1b[<35;15;5M".as_slice(),
            b"\x1b[<0;1;1M".as_slice(),
            b"\x1b[<32;2;1M".as_slice(),
            b"\x1b[<32;3;1M".as_slice(),
            b"\x1b[<0;3;1m".as_slice(),
            b"\x1b[<35;4;1M".as_slice(),
            b"\x1b[<35;5;1M".as_slice(),
        ]
        .concat()
    );
}

#[test]
fn tape_move_sends_exact_unpressed_motion_without_pointer_recording() {
    let session = NamedSession::new("tape-move", "tape-move-test");
    let tape = session.runtime.join("move.tape");
    let observed = session.runtime.join("observed.hex");
    let recording = session.runtime.join("move.termctrl");
    let expected = [
        b"\x1b[<35;2;2M".as_slice(),
        b"\x1b[<35;4;2M".as_slice(),
        b"\x1b[<35;6;2M".as_slice(),
        b"\x1b[<2;7;2M".as_slice(),
        b"\x1b[<2;7;2m".as_slice(),
        b"\x1b[<0;3;4M".as_slice(),
        b"\x1b[<0;3;4m".as_slice(),
        b"\x1b[<35;4;4M".as_slice(),
        b"\x1b[<35;5;4M".as_slice(),
        b"\x1b[<0;1;1M".as_slice(),
        b"\x1b[<32;2;1M".as_slice(),
        b"\x1b[<32;3;1M".as_slice(),
        b"\x1b[<0;3;1m".as_slice(),
        b"\x1b[<35;4;1M".as_slice(),
        b"\x1b[<35;5;1M".as_slice(),
    ]
    .concat();
    let source = format!(
        r#"Session tape-move-test
Viewport 80 24
Cwd "."
Record "move.termctrl"
Launch "/bin/sh" "-c" "stty raw -echo; printf READY; od -An -tx1 -v -N {} > observed.hex; printf DONE; sleep 30"
Wait READY Timeout 2s
Move 1 1 Steps 9 Pace 0ms
Move 5 1 Steps 2 Pace 0ms
RightClick 6 1
Click 2 3
Move 4 3 Steps 2 Pace 0ms
Drag 0 0 2 0 Steps 2 Pace 0ms
Move 4 0 Steps 2 Pace 0ms
Wait DONE Timeout 2s
Stop
"#,
        expected.len()
    );
    fs::write(&tape, source).unwrap();

    assert_success(&session.output(&["play", tape.to_str().unwrap()]));
    assert_eq!(parsed_hex(&fs::read(observed).unwrap()), expected);
    let entries = fs::read_to_string(recording).unwrap();
    assert!(entries.lines().next().unwrap().contains("\"version\":1"));
    assert!(!entries.contains("\"type\":\"pointer\""));
}

#[test]
fn tape_wait_line_match_disambiguates_substrings_and_json_receipt_is_stable() {
    let substring = NamedSession::new("tape-wait-substring", "tape-wait-substring-test");
    let substring_tape = substring.runtime.join("substring.tape");
    fs::write(
        &substring_tape,
        r#"Session tape-wait-substring-test
Viewport 80 8
Launch "/bin/sh" "-c" "printf 'history entry 10\r\n'; sleep 30"
Wait "history entry 1" Timeout 200ms
Stop
"#,
    )
    .unwrap();
    let output = substring.output(&["play", substring_tape.to_str().unwrap(), "--quiet"]);
    assert_success(&output);
    assert!(output.stdout.is_empty());

    let line = NamedSession::new("tape-wait-line", "tape-wait-line-test");
    let line_tape = line.runtime.join("line.tape");
    fs::write(
        &line_tape,
        r#"Session tape-wait-line-test
Viewport 80 8
Launch "/bin/sh" "-c" "printf 'history entry 10\r\n'; sleep 30"
Wait "history entry 1" Match line Timeout 30ms
Stop
"#,
    )
    .unwrap();
    let output = line.output(&["play", line_tape.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timed out waiting for a visible line exactly matching"),
        "{stderr}"
    );

    let exact = NamedSession::new("tape-wait-exact", "tape-wait-exact-test");
    let exact_tape = exact.runtime.join("exact.tape");
    fs::write(
        &exact_tape,
        r#"Session tape-wait-exact-test
Viewport 80 8
Launch "/bin/sh" "-c" "printf 'history entry 10\r\nhistory entry 1\r\n'; sleep 30"
Wait "history entry 1" Match line Timeout 200ms
Stop
"#,
    )
    .unwrap();
    let output = exact.output(&["play", exact_tape.to_str().unwrap(), "--json"]);
    assert_success(&output);
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["status"], "ok");
    assert_eq!(receipt["session"], "tape-wait-exact-test");
    assert_eq!(receipt["tape"], exact_tape.to_str().unwrap());
    assert!(receipt["recording"].is_null());
}

#[test]
fn tape_json_rejects_non_utf8_receipt_paths_before_lifecycle_side_effects() {
    let session = NamedSession::new("tape-json-path", "tape-json-path-test");
    let directory = session
        .runtime
        .join(OsString::from_vec(b"non-utf8-\xff".to_vec()));
    fs::create_dir(&directory).unwrap();
    let tape = directory.join("demo.tape");
    let setup_marker = directory.join("setup-ran");
    fs::write(
        &tape,
        r#"Session tape-json-path-test
Viewport 80 24
Cwd "."
Setup "/usr/bin/touch" "setup-ran"
Launch "/bin/sh" "-c" "printf READY; sleep 30"
Stop
"#,
    )
    .unwrap();

    let output = session
        .command()
        .arg("play")
        .arg(&tape)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_ne!(output.status.code(), Some(101), "process panicked");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("play --json requires a UTF-8 tape path before lifecycle side effects"),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!setup_marker.exists());
    assert!(!session.output(&["status", session.name]).status.success());
}

#[test]
fn tape_play_validates_before_launch_or_action_side_effects() {
    let session = NamedSession::new("tape-validate", "tape-validate-test");
    let tape = session.runtime.join("invalid.tape");
    fs::write(
        &tape,
        r#"Session tape-validate-test
Viewport 2 1
Cwd "."
Setup "/usr/bin/touch" "setup-ran"
Cleanup "/usr/bin/touch" "cleanup-ran"
Launch "/usr/bin/touch" "launched"
Action "/usr/bin/touch" "acted"
Click 65534 0
Stop
"#,
    )
    .unwrap();

    let output = session.output(&["play", tape.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("invalid.tape:8: Click coordinate (65534, 0) is outside Viewport 2x1")
    );
    assert!(!session.runtime.join("setup-ran").exists());
    assert!(!session.runtime.join("cleanup-ran").exists());
    assert!(!session.runtime.join("launched").exists());
    assert!(!session.runtime.join("acted").exists());
}

#[test]
fn tape_wait_failure_reports_screen_and_cleans_up_owned_session() {
    let session = NamedSession::new("tape-timeout", "tape-timeout-test");
    let tape = session.runtime.join("timeout.tape");
    fs::write(
        &tape,
        r#"Session tape-timeout-test
Viewport 80 24
Launch "/bin/sh" "-c" "printf ACTUAL; sleep 30"
Wait MISSING Timeout 20ms
Stop
"#,
    )
    .unwrap();

    let output = session.output(&["play", tape.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timeout.tape:4: Wait failed"), "{stderr}");
    assert!(stderr.contains("last visible screen"), "{stderr}");
    assert!(stderr.contains("ACTUAL"), "{stderr}");
    assert!(!session.output(&["status", session.name]).status.success());
}

#[test]
fn tape_setup_and_cleanup_make_repeat_runs_begin_clean() {
    let session = NamedSession::new("tape-repeat", "tape-repeat-test");
    let tape = session.runtime.join("repeat.tape");
    let fixture = session.runtime.join("repeat.fixture");
    fs::write(
        &tape,
        r#"Session tape-repeat-test
Viewport 80 24
Cwd "."
Setup "/usr/bin/test" "!" "-e" "repeat.fixture"
Setup "/usr/bin/touch" "repeat.fixture"
Cleanup "/usr/bin/rm" "-f" "repeat.fixture"
Launch "/bin/sh" "-c" "test -e repeat.fixture && printf READY; sleep 30"
Wait READY Timeout 2s
Stop
"#,
    )
    .unwrap();

    for _ in 0..2 {
        assert_success(&session.output(&["play", tape.to_str().unwrap()]));
        assert!(!fixture.exists(), "cleanup must restore the fixture");
    }
}

#[test]
fn tape_failure_stops_session_then_cleans_up_without_masking_primary_error() {
    let session = NamedSession::new("tape-cleanup", "tape-cleanup-test");
    let tape = session.runtime.join("cleanup.tape");
    let fixture = session.runtime.join("failure.fixture");
    fs::write(
        &tape,
        r#"Session tape-cleanup-test
Viewport 80 24
Cwd "."
Setup "/usr/bin/touch" "failure.fixture"
Cleanup "/bin/sh" "-c" "alive=0; kill -0 \"$(cat app.pid)\" 2>/dev/null && alive=1; rm -f failure.fixture app.pid; if [ \"$alive\" -eq 1 ]; then echo session-alive >&2; exit 8; fi; echo cleanup-deliberate >&2; exit 7"
Launch "/bin/sh" "-c" "echo $$ > app.pid; printf ACTUAL; sleep 30"
Wait MISSING Timeout 20ms
Stop
"#,
    )
    .unwrap();

    let output = session.output(&["play", tape.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let primary = stderr.find("cleanup.tape:7: Wait failed").unwrap();
    let additional = stderr.find("additional lifecycle failures").unwrap();
    assert!(primary < additional, "{stderr}");
    assert!(
        stderr.contains("cleanup.tape:5: Cleanup failed"),
        "{stderr}"
    );
    assert!(stderr.contains("cleanup-deliberate"), "{stderr}");
    assert!(!stderr.contains("stderr:\nsession-alive"), "{stderr}");
    assert!(!fixture.exists(), "cleanup must restore the fixture");
    assert!(!session.output(&["status", session.name]).status.success());
}

#[test]
fn tape_action_timeout_kills_process_group_and_bounds_noisy_diagnostics() {
    let session = NamedSession::new("tape-action-timeout", "tape-action-timeout-test");
    let tape = session.runtime.join("action-timeout.tape");
    let child_pid = session.runtime.join("action-child.pid");
    fs::write(
        &tape,
        r#"Session tape-action-timeout-test
Viewport 80 24
Cwd "."
Launch "/bin/sh" "-c" "printf READY; sleep 30"
Wait READY Timeout 2s
Action "/bin/sh" "-c" "sleep 30 & echo $! > action-child.pid; exec /usr/bin/yes noisy" Timeout 50ms
Stop
"#,
    )
    .unwrap();

    let output = session.output(&["play", tape.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Action failed"), "{stderr}");
    assert!(stderr.contains("timed out after 50ms"), "{stderr}");
    assert!(stderr.contains("<truncated>"), "{stderr}");
    assert!(
        stderr.len() < 10_000,
        "diagnostic was not bounded: {}",
        stderr.len()
    );
    let pid = fs::read_to_string(child_pid)
        .unwrap()
        .trim()
        .parse::<libc::pid_t>()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let result = unsafe { libc::kill(pid, 0) };
        if result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "action descendant {pid} survived timeout"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!session.output(&["status", session.name]).status.success());
}

#[test]
fn tape_action_bounds_drain_when_escaped_descendant_retains_output_pipes() {
    let session = NamedSession::new("tape-action-drain", "tape-action-drain-test");
    let helper = session.runtime.join("escape-action.sh");
    let escaped_pid = session.runtime.join("escaped.pid");
    let cleanup_marker = session.runtime.join("cleanup-ran");
    fs::write(
        &helper,
        "#!/bin/sh\nsetsid /bin/sh -c 'echo $$ > escaped.pid; sleep 30' &\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    let tape = session.runtime.join("action-drain.tape");
    fs::write(
        &tape,
        r#"Session tape-action-drain-test
Viewport 80 24
Cwd "."
Cleanup "/usr/bin/touch" "cleanup-ran"
Launch "/bin/sh" "-c" "printf READY; sleep 30"
Wait READY Timeout 2s
Action "./escape-action.sh" Timeout 1s
Stop
"#,
    )
    .unwrap();

    let started = Instant::now();
    let mut playback = session
        .command()
        .arg("play")
        .arg(&tape)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if playback.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = playback.kill();
            panic!("playback did not bound Action output drain within 3 seconds");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = playback.wait_with_output().unwrap();

    let pid = fs::read_to_string(&escaped_pid)
        .unwrap()
        .trim()
        .parse::<libc::pid_t>()
        .unwrap();
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }

    assert!(!output.status.success());
    assert!(started.elapsed() < Duration::from_secs(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Action failed"), "{stderr}");
    assert!(
        stderr.contains("left output pipes open after 250ms"),
        "{stderr}"
    );
    assert!(
        stderr.contains("detached descendants may still be running"),
        "{stderr}"
    );
    assert!(cleanup_marker.exists(), "cleanup did not proceed");
    assert!(!session.output(&["status", session.name]).status.success());
}
