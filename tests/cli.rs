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
        let _ = self.command().args(["stop", self.name]).status();
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
