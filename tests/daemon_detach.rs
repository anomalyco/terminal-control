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

    fn launch(&mut self, operation: &str, generation: &str) {
        let ready = self.runtime.join(format!("{generation}.ready"));
        let pids = self.runtime.join(format!("{generation}.pids"));
        let mut launcher = Command::new("/bin/sh");
        launcher
            .arg("-c")
            .arg(
                r#""$1" "$2" demo -- /bin/sh -c 'printf "%s %s\n" "$$" "$PPID" > "$1"; printf READY; read -r ignored' fixture "$3" && : > "$4" && exec sleep 30"#,
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
                fs::File::create(self.runtime.join(format!("{generation}.stderr"))).unwrap(),
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
                fs::read_to_string(self.runtime.join(format!("{generation}.stderr")))
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
            "{operation}/{generation}: launcher group {}, application/daemon {}",
            self.launchers.last().unwrap().id(),
            fs::read_to_string(&pids).unwrap().trim()
        );
        if operation == "restart" {
            self.assert_application_stopped("start");
        }
    }

    fn assert_application_stopped(&self, operation: &str) {
        // Keep the generation record for Drop: application exit does not prove daemon exit.
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
        // Snapshot every generation before shutdown can remove any runtime files.
        let mut records = Vec::new();
        let mut cleaned = true;
        match fs::read_dir(&self.runtime) {
            Ok(entries) => {
                for entry in entries {
                    let Ok(entry) = entry else {
                        cleaned = false;
                        continue;
                    };
                    let path = entry.path();
                    if path.extension().and_then(|value| value.to_str()) != Some("pids") {
                        continue;
                    }
                    match fs::read_to_string(&path) {
                        Ok(pids) => records.push((path, pids)),
                        Err(_) => cleaned = false,
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => cleaned = false,
        }
        // A successful stop only accounts for the current generation, not failed predecessors.
        let _ = self.output(&["stop", "demo"]);
        let owns_pid =
            |pid: libc::pid_t, path: &PathBuf, executable: &str| -> std::io::Result<bool> {
                if pid <= 1 || pid == std::process::id() as libc::pid_t {
                    return Ok(false);
                }
                if unsafe { libc::kill(pid, 0) } < 0 {
                    let error = std::io::Error::last_os_error();
                    return if error.raw_os_error() == Some(libc::ESRCH) {
                        Ok(false)
                    } else {
                        Err(error)
                    };
                }
                let output = Command::new("/bin/ps")
                    .args(["-ww", "-p", &pid.to_string(), "-o", "uid=,command="])
                    .output()?;
                if !output.status.success() {
                    return if unsafe { libc::kill(pid, 0) } < 0
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                    {
                        Ok(false)
                    } else {
                        Err(std::io::Error::other(
                            "could not verify fixture process identity",
                        ))
                    };
                }
                let line = String::from_utf8_lossy(&output.stdout);
                let Some((uid, command)) = line.trim().split_once(char::is_whitespace) else {
                    return Err(std::io::Error::other("invalid fixture process identity"));
                };
                // The application blocks in a shell builtin, retaining its unique PID-file
                // argument. Check it and the executable/UID rather than trusting a stale PID.
                Ok(
                    uid.parse::<libc::uid_t>().ok() == Some(unsafe { libc::geteuid() })
                        && command.trim_start().starts_with(executable)
                        && command.ends_with(&format!(" fixture {}", path.display())),
                )
            };
        for (path, record) in &records {
            let Ok(pids) = record
                .split_whitespace()
                .map(str::parse::<libc::pid_t>)
                .collect::<Result<Vec<_>, _>>()
            else {
                cleaned = false;
                continue;
            };
            if pids.len() != 2 {
                cleaned = false;
                continue;
            }
            for (pid, executable) in pids.into_iter().zip([
                "/bin/sh -c ".to_owned(),
                format!("{} __serve ", env!("CARGO_BIN_EXE_termctrl")),
            ]) {
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut signaled = false;
                loop {
                    match owns_pid(pid, path, &executable) {
                        Ok(false) => break,
                        Ok(true) if Instant::now() < deadline => {
                            if !signaled {
                                // Signal only a verified individual process, never its group.
                                if unsafe { libc::kill(pid, libc::SIGKILL) } < 0
                                    && std::io::Error::last_os_error().raw_os_error()
                                        != Some(libc::ESRCH)
                                {
                                    cleaned = false;
                                    break;
                                }
                                signaled = true;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        _ => {
                            cleaned = false;
                            break;
                        }
                    }
                }
            }
        }
        if cleaned {
            let _ = fs::remove_dir_all(&self.runtime);
        } else {
            eprintln!(
                "fixture cleanup could not be verified; retaining records at {}",
                self.runtime.display()
            );
        }
    }
}

fn survives_launcher_hangup(restart: bool) {
    let operation = if restart { "restart" } else { "start" };
    let mut session = NamedSession::new(operation);
    session.launch("start", "start");
    if restart {
        session.launch("restart", "restart");
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

#[test]
fn cleanup_stops_every_generation_after_a_failed_restart_assertion() {
    // Preserve an independently reachable endpoint so even the failing regression
    // run can stop the original daemon after the buggy destructor deletes its records.
    let backup = NamedSession::new("cleanup-backup");
    let mut unrelated = NamedSession::new("cleanup-unrelated");
    unrelated.launch("start", "start");
    let mut session = NamedSession::new("cleanup");
    session.launch("start", "start");
    let original = fs::read_to_string(session.runtime.join("start.pids")).unwrap();
    fs::rename(
        session.runtime.join("demo.sock"),
        backup.runtime.join("demo.sock"),
    )
    .unwrap();
    session.launch("start", "replacement");
    let replacement = fs::read_to_string(session.runtime.join("replacement.pids")).unwrap();
    // Simulate stale/reused PID records using another independently owned fixture,
    // never a shared process or the test runner itself.
    fs::write(
        session.runtime.join("stale.pids"),
        fs::read_to_string(unrelated.runtime.join("start.pids")).unwrap(),
    )
    .unwrap();
    let runtime = session.runtime.clone();

    let failure = std::panic::catch_unwind(move || session.assert_application_stopped("start"));
    assert!(
        failure.is_err(),
        "the original generation must still be running at the assertion"
    );
    let pids: Vec<_> = original
        .split_whitespace()
        .chain(replacement.split_whitespace())
        .map(|value| value.parse::<libc::pid_t>().unwrap())
        .collect();
    let deadline = Instant::now() + Duration::from_secs(2);
    let surviving = loop {
        let surviving: Vec<_> = pids
            .iter()
            .copied()
            .filter(|pid| unsafe { libc::kill(*pid, 0) } == 0)
            .collect();
        if surviving.is_empty() || Instant::now() >= deadline {
            break surviving;
        }
        thread::sleep(Duration::from_millis(10));
    };
    eprintln!("failed-generation cleanup: surviving fixture PIDs {surviving:?}");
    assert!(
        surviving.is_empty(),
        "cleanup left generation processes alive: {surviving:?}"
    );
    assert!(
        !runtime.exists(),
        "cleanup left its runtime directory behind"
    );
    let status: serde_json::Value = serde_json::from_slice(
        &assert_success(unrelated.output(&["status", "demo", "--json"]).unwrap()).stdout,
    )
    .unwrap();
    assert_eq!(status["state"], "running");
    eprintln!("stale PID records: unrelated fixture generation remains running");
}
