#![cfg(unix)]

use std::fs;
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
fn pointer_enabled_session_records_structured_events_and_renders_live_and_replay() {
    let session = NamedSession::new("pointer-recording", "pointer-recording-test");
    let recording = session.runtime.join("pointer.termctrl");
    let live = session.runtime.join("live.svg");
    let replay = session.runtime.join("replay.svg");
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
        assert!(svg.contains("#f9fafb"), "{svg}");
        assert!(svg.contains("#111827"), "{svg}");
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
Launch "/bin/sh" "-c" "stty raw -echo; printf READY:%s:%s \"$DEMO_ENV\" \"$PWD\"; od -An -tx1 -v -N 64; while [ ! -f action-ready ]; do sleep 0.01; done; printf FIXTURE; sleep 30"
Wait "READY:quoted # value" Timeout 2s
Type "hello" Pace 1ms
Key enter
Click 12 4
Drag 0 0 2 0 Steps 2 Pace 0ms
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
    assert!(entries.iter().any(|entry| entry["type"] == "pointer"));
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
            b"hello\r".as_slice(),
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
fn tape_play_validates_before_launch_or_action_side_effects() {
    let session = NamedSession::new("tape-validate", "tape-validate-test");
    let tape = session.runtime.join("invalid.tape");
    fs::write(
        &tape,
        r#"Session tape-validate-test
Viewport 80 24
Cwd "."
Setup "/usr/bin/touch" "setup-ran"
Cleanup "/usr/bin/touch" "cleanup-ran"
Launch "/usr/bin/touch" "launched"
Action "/usr/bin/touch" "acted"
Stop extra
"#,
    )
    .unwrap();

    let output = session.output(&["play", tape.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid.tape:8: usage: Stop"));
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
