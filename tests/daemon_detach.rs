#![cfg(unix)]

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct NamedSession {
    runtime: PathBuf,
    launchers: Vec<Child>,
}

impl NamedSession {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Keep the canonical socket path below the portable 100-byte limit on macOS.
        let runtime = PathBuf::from(format!(
            "/tmp/termctrl-detach-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
        Self {
            runtime,
            launchers: Vec::new(),
        }
    }

    fn output(&self, args: &[&str]) -> std::io::Result<Output> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_termctrl"));
        command
            .args(args)
            .env("TERMCTRL_RUNTIME_DIR", &self.runtime)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("command timed out: {args:?}"),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output()
    }

    fn launch(&mut self, operation: &str) {
        let ready = self.runtime.join(format!("{operation}.ready"));
        let pids = self.runtime.join(format!("{operation}.pids"));
        let mut launcher = Command::new("/bin/sh");
        launcher
            .arg("-c")
            .arg(
                r#""$1" "$2" demo -- /bin/sh -c 'printf "%s %s\n" "$$" "$PPID" > "$1"; printf READY; exec sleep 30' fixture "$3" && : > "$4" && exec sleep 30"#,
            )
            .arg("termctrl-launcher")
            .arg(env!("CARGO_BIN_EXE_termctrl"))
            .arg(operation)
            .arg(&pids)
            .arg(&ready)
            .env("TERMCTRL_RUNTIME_DIR", &self.runtime)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::File::create(self.runtime.join(format!("{operation}.stderr"))).unwrap(),
            ));
        unsafe {
            launcher.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self.launchers.push(launcher.spawn().unwrap());
        let launcher = self.launchers.last_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() || !pids.exists() {
            assert!(
                launcher.try_wait().unwrap().is_none(),
                "{operation} launcher exited early: {}",
                fs::read_to_string(self.runtime.join(format!("{operation}.stderr")))
                    .unwrap_or_default()
            );
            assert!(
                Instant::now() < deadline,
                "{operation} launcher was not ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_success(
            self.output(&["wait", "demo", "READY", "--timeout", "2000"])
                .unwrap(),
        );
        eprintln!(
            "{operation}: launcher group {}, application/daemon {}",
            self.launchers.last().unwrap().id(),
            fs::read_to_string(&pids).unwrap().trim()
        );
        if operation == "restart" {
            self.assert_application_stopped("start");
        }
    }

    fn assert_application_stopped(&self, operation: &str) {
        let path = self.runtime.join(format!("{operation}.pids"));
        let pids = fs::read_to_string(&path).unwrap();
        let application = pids
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        assert!(application > 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(application, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "{operation} application was not stopped"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        fs::remove_file(path).unwrap();
    }
}

impl Drop for NamedSession {
    fn drop(&mut self) {
        // Only these successfully setsid-isolated launchers receive group signals.
        // Kill them before reading PID files so no launcher can start another daemon.
        for launcher in &mut self.launchers {
            // Do not signal a reaped child's ID, which could have been reused.
            if launcher.try_wait().is_ok_and(|status| status.is_none()) {
                unsafe {
                    libc::killpg(launcher.id() as libc::pid_t, libc::SIGKILL);
                }
            }
            let _ = launcher.wait();
        }
        // Prefer normal shutdown so a live daemon reaps its application. This is bounded
        // even on assertion failures; the PID fallback also covers a daemon killed by SIGHUP.
        if self
            .output(&["stop", "demo"])
            .is_ok_and(|output| output.status.success())
        {
            let _ = fs::remove_dir_all(&self.runtime);
            return;
        }
        if let Ok(entries) = fs::read_dir(&self.runtime) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|value| value.to_str()) != Some("pids") {
                    continue;
                }
                if let Ok(pids) = fs::read_to_string(entry.path()) {
                    for pid in pids
                        .split_whitespace()
                        .filter_map(|value| value.parse::<libc::pid_t>().ok())
                    {
                        if pid > 1 && pid != std::process::id() as libc::pid_t {
                            // The application execs sleep instead of spawning descendants.
                            // Use individual PIDs: a broken daemon may share the runner's group.
                            unsafe {
                                libc::kill(pid, libc::SIGKILL);
                            }
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            }
        }
        let _ = fs::remove_dir_all(&self.runtime);
    }
}

fn survives_launcher_hangup(restart: bool) {
    let operation = if restart { "restart" } else { "start" };
    let mut session = NamedSession::new(operation);
    session.launch("start");
    if restart {
        session.launch("restart");
    }
    let pids = fs::read_to_string(session.runtime.join(format!("{operation}.pids"))).unwrap();
    let daemon = pids
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    let launcher = session.launchers.last_mut().unwrap();
    let group = launcher.id() as libc::pid_t;
    assert!(group > 1);
    assert_ne!(group, unsafe { libc::getpgrp() });
    assert_eq!(unsafe { libc::getpgid(group) }, group);
    assert_eq!(unsafe { libc::killpg(group, libc::SIGHUP) }, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "launcher survived SIGHUP");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.signal(), Some(libc::SIGHUP));

    let status: serde_json::Value = serde_json::from_slice(
        &assert_success(session.output(&["status", "demo", "--json"]).unwrap()).stdout,
    )
    .unwrap();
    assert_eq!(status["state"], "running");
    assert_eq!(unsafe { libc::getsid(daemon) }, daemon);
    assert_eq!(
        String::from_utf8(assert_success(session.output(&["show", "demo"]).unwrap()).stdout)
            .unwrap()
            .trim(),
        "READY"
    );
    assert_success(session.output(&["stop", "demo"]).unwrap());
    let deadline = Instant::now() + Duration::from_secs(2);
    while session.runtime.join("demo.sock").exists() {
        assert!(
            Instant::now() < deadline,
            "stop left the session socket behind"
        );
        thread::sleep(Duration::from_millis(10));
    }
    session.assert_application_stopped(operation);
    assert!(
        !session
            .output(&["status", "demo"])
            .unwrap()
            .status
            .success()
    );
    eprintln!("launcher SIGHUP: status running, show READY, stop removed socket and application");
}

fn assert_success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn named_session_survives_launcher_process_group_hangup() {
    survives_launcher_hangup(false);
}

#[test]
fn restarted_session_survives_launcher_process_group_hangup() {
    survives_launcher_hangup(true);
}
