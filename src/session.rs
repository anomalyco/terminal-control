use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::recording::{self, InputOrigin};
use crate::semantic;
use crate::shot::{self, Host, Options, Shot, respond_to_output};
use crate::terminal_core::{SCROLLBACK_ROWS, TerminalCore};
use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const OUTPUT_QUEUE: usize = 64;
const OUTPUT_BATCH: usize = OUTPUT_QUEUE;
const OUTPUT_CHUNK: usize = 1024;
const SEMANTIC_PROTOCOL_VERSION: u8 = 6;

struct Output {
    at_ms: u64,
    bytes: Vec<u8>,
}

/// One running terminal application controlled in-process by its caller.
///
/// `Session` is the embedded equivalent of the CLI `session` lifecycle. It owns a PTY and the
/// visible terminal state, so callers can send input, wait for content, take shots, and resize
/// without spawning a new `termctrl` command for each action. Ghostty terminal state is
/// thread-confined, so a session must remain on the thread where it was created.
pub struct Session {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send>,
    #[cfg(unix)]
    process_group: Option<i32>,
    terminal: TerminalCore,
    ansi: Vec<u8>,
    host: Host,
    receive: Receiver<Option<Output>>,
    max_bytes: usize,
    ansi_truncated: bool,
    output_closed: bool,
    stopped: bool,
    exit: Option<ProcessExit>,
    last_output: Option<Instant>,
    recording: Option<recording::Writer>,
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
    launch: SessionLaunch,
    mirror: Option<Box<dyn Write + Send>>,
    semantic: Option<semantic::Host>,
}

/// Lifecycle state of a running or completed session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Running,
    Exited,
}

/// Termination information observed for a completed terminal application.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProcessExit {
    pub code: u32,
    pub signal: Option<String>,
    pub success: bool,
}

impl From<ExitStatus> for ProcessExit {
    fn from(status: ExitStatus) -> Self {
        Self {
            code: status.exit_code(),
            signal: status.signal().map(str::to_owned),
            success: status.success(),
        }
    }
}

/// Reason a session capture returned its visible shot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureReason {
    Idle,
    Deadline,
    Exited,
    OutputClosed,
}

/// A visible shot together with the condition that made it observable.
#[derive(Deserialize, Serialize)]
pub struct CaptureResult {
    pub shot: Shot,
    pub reason: CaptureReason,
}

/// Observable state of one embedded or named terminal session.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionStatus {
    pub state: SessionState,
    pub exit: Option<ProcessExit>,
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
    pub idle_for_ms: Option<u64>,
    pub has_visible_content: bool,
    pub recording: bool,
    pub logs_truncated: bool,
    pub launch: SessionLaunch,
}

/// Non-secret launch settings retained for status display and named-session restart.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionLaunch {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub record: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
    pub max_bytes: usize,
    pub opentui_host: bool,
    pub color: shot::ColorMode,
}

/// One named daemon session discovered in the local runtime directory.
#[derive(Debug, Serialize)]
pub struct NamedSessionStatus {
    pub name: String,
    pub status: Option<SessionStatus>,
    pub error: Option<String>,
    pub unavailable: Option<UnavailableReason>,
}

impl NamedSessionStatus {
    /// Whether this discovered entry satisfies a session-list filter.
    pub fn matches(&self, filter: &SessionFilter) -> bool {
        if filter.state.is_some_and(|state| !state.matches(self)) {
            return false;
        }
        let Some(status) = &self.status else {
            return filter.command.is_none() && filter.cwd.is_none();
        };
        filter.command.as_ref().is_none_or(|needle| {
            status
                .launch
                .command
                .iter()
                .any(|argument| argument.contains(needle))
        }) && filter
            .cwd
            .as_ref()
            .is_none_or(|cwd| status.launch.cwd == *cwd)
    }
}

/// Optional selectors applied to discovered named sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFilter {
    pub state: Option<SessionListState>,
    /// Case-sensitive substring matched against each command argument.
    pub command: Option<String>,
    /// Exact launch working directory.
    pub cwd: Option<PathBuf>,
}

/// States addressable by session discovery filters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionListState {
    Running,
    Exited,
    Stale,
    Incompatible,
}

impl SessionListState {
    fn matches(self, entry: &NamedSessionStatus) -> bool {
        match self {
            Self::Running => entry
                .status
                .as_ref()
                .is_some_and(|status| status.state == SessionState::Running),
            Self::Exited => entry
                .status
                .as_ref()
                .is_some_and(|status| status.state == SessionState::Exited),
            Self::Stale => entry.unavailable == Some(UnavailableReason::Stale),
            Self::Incompatible => {
                entry.unavailable == Some(UnavailableReason::IncompatibleProtocol)
            }
        }
    }
}

/// Why a named session socket could not report normal status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    Stale,
    IncompatibleProtocol,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PruneKind {
    Exited,
    Stale,
}

impl Session {
    /// Start `command` inside a live PTY-backed session.
    pub fn start(
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
    ) -> Result<Self> {
        Self::start_with_host_responses(command, cwd, record, options, options.opentui_host)
    }

    fn start_with_host_responses(
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
        host_responses: bool,
    ) -> Result<Self> {
        if command.is_empty() {
            bail!("provide a command after --");
        }
        if options.cols == 0 || options.rows == 0 {
            bail!("terminal dimensions must be greater than zero");
        }
        let cwd = match cwd {
            Some(cwd) if cwd.is_absolute() => cwd.to_owned(),
            Some(cwd) => std::env::current_dir()
                .context("resolve session working directory")?
                .join(cwd),
            None => std::env::current_dir().context("resolve session working directory")?,
        };
        let cwd = fs::canonicalize(&cwd).context("canonicalize session working directory")?;
        let terminal = TerminalCore::new(options.rows, options.cols, SCROLLBACK_ROWS)?;
        let started = Instant::now();
        let recording = record
            .map(|path| {
                recording::Writer::new(
                    path,
                    started,
                    options.cols,
                    options.rows,
                    options.cell_width,
                    options.cell_height,
                )
            })
            .transpose()?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: options.rows,
                cols: options.cols,
                pixel_width: options.cell_width,
                pixel_height: options.cell_height,
            })
            .context("open session pseudo-terminal")?;
        let semantic = options
            .opentui_host
            .then(semantic::Host::bind)
            .transpose()?;
        let mut builder = CommandBuilder::new(&command[0]);
        builder.args(&command[1..]);
        shot::configure_pty_environment(&mut builder, options);
        if let Some(path) = semantic.as_ref().and_then(semantic::Host::path) {
            builder.env(semantic::SOCKET_ENV, path);
        }
        builder.cwd(&cwd);
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("open session PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("open session PTY writer")?;
        let child = pair
            .slave
            .spawn_command(builder)
            .context("spawn session command")?;
        drop(pair.slave);
        #[cfg(unix)]
        let process_group = child.process_id().and_then(|pid| i32::try_from(pid).ok());
        let (send, receive) = mpsc::sync_channel(OUTPUT_QUEUE);
        thread::spawn(move || {
            let mut buffer = [0_u8; OUTPUT_CHUNK];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(len) => {
                        if send
                            .send(Some(Output {
                                at_ms: started.elapsed().as_millis() as u64,
                                bytes: buffer[..len].to_vec(),
                            }))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = send.send(None);
        });
        let mut host_options = options.clone();
        host_options.opentui_host = host_responses;
        Ok(Self {
            master: pair.master,
            child,
            #[cfg(unix)]
            process_group,
            terminal,
            ansi: Vec::new(),
            host: Host::new(writer, &host_options),
            receive,
            max_bytes: options.max_bytes,
            ansi_truncated: false,
            output_closed: false,
            stopped: false,
            exit: None,
            last_output: None,
            recording,
            cols: options.cols,
            rows: options.rows,
            cell_width: options.cell_width,
            cell_height: options.cell_height,
            launch: SessionLaunch {
                command: command.to_vec(),
                cwd,
                record: record.map(Path::to_owned),
                cols: options.cols,
                rows: options.rows,
                cell_width: options.cell_width,
                cell_height: options.cell_height,
                max_bytes: options.max_bytes,
                opentui_host: options.opentui_host,
                color: options.color,
            },
            mirror: None,
            semantic,
        })
    }

    /// Mirror subsequent PTY output to another terminal-facing writer.
    pub fn mirror_to(&mut self, writer: impl Write + Send + 'static) {
        self.mirror = Some(Box::new(writer));
    }

    /// Send one input burst to the terminal application.
    pub fn send(&mut self, input: &[u8]) -> Result<()> {
        self.send_all(&[input.to_vec()], Duration::ZERO)
    }

    /// Send ordered input bursts, optionally pacing them for recorded interactions.
    pub fn send_all(&mut self, input: &[Vec<u8>], pace: Duration) -> Result<()> {
        self.consume_batch()?;
        if self.has_exited()? || self.stopped {
            bail!("session command has exited");
        }
        let last = input.len().saturating_sub(1);
        for (index, bytes) in input.iter().enumerate() {
            self.host.send(bytes)?;
            if let Some(recording) = &mut self.recording {
                recording.input(InputOrigin::Client, bytes)?;
            }
            if !pace.is_zero() && index < last {
                thread::sleep(pace);
                self.consume_batch()?;
            }
        }
        Ok(())
    }

    /// Wait until visible terminal text contains `text`.
    pub fn wait_for_text(&mut self, text: &str, timeout: Duration) -> Result<()> {
        self.wait_for_text_with(text, timeout, |_| Ok(()))
    }

    fn wait_for_text_with(
        &mut self,
        text: &str,
        timeout: Duration,
        mut on_tick: impl FnMut(&mut Self) -> Result<()>,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            self.consume_batch()?;
            on_tick(self)?;
            if self.terminal.text()?.contains(text) {
                return Ok(());
            }
            if self.has_exited()? || self.stopped {
                bail!("session ended before visible terminal included {text:?}");
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for visible terminal text {text:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until no terminal output has arrived for `settle`.
    pub fn wait_for_idle(&mut self, settle: Duration, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        let deadline = started + timeout;
        loop {
            self.consume_batch()?;
            if self.output_closed || self.last_output.unwrap_or(started).elapsed() >= settle {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for terminal output to settle");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait for the terminal application to exit, returning `None` on timeout.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Result<Option<ProcessExit>> {
        let deadline = Instant::now() + timeout;
        loop {
            self.consume_batch()?;
            if self.has_exited()? || self.stopped {
                return Ok(self.exit.clone());
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Capture visible terminal state and report whether it settled, exited, or reached a limit.
    pub fn capture(&mut self, settle: Duration, deadline: Duration) -> Result<CaptureResult> {
        self.capture_with(settle, deadline, |_| Ok(()))
    }

    fn capture_with(
        &mut self,
        settle: Duration,
        deadline: Duration,
        mut on_tick: impl FnMut(&mut Self) -> Result<()>,
    ) -> Result<CaptureResult> {
        let started = Instant::now();
        let deadline = started + deadline;
        loop {
            self.consume_batch()?;
            on_tick(self)?;
            let reason = if self.has_exited()? || self.stopped {
                Some(CaptureReason::Exited)
            } else if self.output_closed {
                Some(CaptureReason::OutputClosed)
            } else if self.last_output.unwrap_or(started).elapsed() >= settle {
                Some(CaptureReason::Idle)
            } else if Instant::now() >= deadline {
                Some(CaptureReason::Deadline)
            } else {
                None
            };
            if let Some(reason) = reason {
                return Ok(CaptureResult {
                    shot: Shot {
                        frame: self.terminal.frame()?,
                        ansi: self.ansi.clone(),
                    },
                    reason,
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Inspect session lifecycle, geometry, and whether a visible frame is available.
    pub fn status(&mut self) -> Result<SessionStatus> {
        self.consume_batch()?;
        Ok(SessionStatus {
            state: if self.has_exited()? || self.stopped {
                SessionState::Exited
            } else {
                SessionState::Running
            },
            exit: self.exit.clone(),
            cols: self.cols,
            rows: self.rows,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
            idle_for_ms: self
                .last_output
                .map(|last| last.elapsed().as_millis() as u64),
            has_visible_content: self.terminal.frame()?.has_visible_content(),
            recording: self.recording.is_some(),
            logs_truncated: self.ansi_truncated,
            launch: self.launch.clone(),
        })
    }

    /// Return readable normal-screen scrollback, or the exact retained ANSI/VT stream.
    pub fn logs(&mut self, ansi: bool) -> Result<Vec<u8>> {
        self.consume_batch()?;
        if ansi {
            return Ok(self.ansi.clone());
        }
        self.terminal.scrollback_text()
    }

    /// Return the application's semantic UI snapshot, or an empty snapshot when unavailable.
    pub fn semantic_snapshot(&mut self, timeout: Duration) -> Result<Value> {
        self.semantic_snapshot_with(timeout, |_| Ok(()))
    }

    fn semantic_snapshot_with(
        &mut self,
        timeout: Duration,
        mut on_tick: impl FnMut(&mut Self) -> Result<()>,
    ) -> Result<Value> {
        let Some(mut semantic) = self.semantic.take() else {
            return Ok(semantic::empty_semantic_snapshot());
        };
        let result = semantic
            .snapshot(timeout, || {
                self.consume_output_batch()?;
                on_tick(self)?;
                Ok(self.has_exited()? || self.stopped)
            })
            .map(|snapshot| snapshot.unwrap_or_else(semantic::empty_semantic_snapshot));
        if self.exit.is_none() && !self.stopped {
            self.semantic = Some(semantic);
        }
        result
    }

    /// Resize the PTY and reflow subsequent terminal parsing at the new dimensions.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> Result<()> {
        if cols == 0 || rows == 0 {
            bail!("terminal dimensions must be greater than zero");
        }
        if (cols, rows, cell_width, cell_height)
            == (self.cols, self.rows, self.cell_width, self.cell_height)
        {
            return Ok(());
        }
        self.consume_batch()?;
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: cell_width,
                pixel_height: cell_height,
            })
            .context("resize session pseudo-terminal")?;
        self.host.resize(cols, rows, cell_width, cell_height);
        self.terminal.resize(cols, rows, cell_width, cell_height)?;
        self.cols = cols;
        self.rows = rows;
        self.cell_width = cell_width;
        self.cell_height = cell_height;
        if let Some(recording) = &mut self.recording {
            recording.resize(cols, rows, cell_width, cell_height)?;
        }
        Ok(())
    }

    /// Add a named moment to the active recording timeline.
    pub fn mark(&mut self, name: &str) -> Result<()> {
        self.consume_batch()?;
        let recording = self
            .recording
            .as_mut()
            .context("session was not started with --record")?;
        recording.marker(name)
    }

    /// Terminate the application owned by this session.
    pub fn stop(&mut self) -> Result<()> {
        self.terminate();
        Ok(())
    }

    pub(crate) fn pump(&mut self) -> Result<()> {
        self.consume_batch()
    }

    fn consume_batch(&mut self) -> Result<()> {
        if let Some(semantic) = &mut self.semantic {
            semantic.pump();
        }
        self.consume_output_batch()
    }

    fn consume_output_batch(&mut self) -> Result<()> {
        for _ in 0..OUTPUT_BATCH {
            if !self.consume_one()? {
                break;
            }
        }
        Ok(())
    }

    fn consume_one(&mut self) -> Result<bool> {
        match self.receive.try_recv() {
            Ok(Some(output)) => {
                self.apply_output(output)?;
                Ok(true)
            }
            Ok(None) | Err(TryRecvError::Disconnected) => {
                self.output_closed = true;
                Ok(false)
            }
            Err(TryRecvError::Empty) => Ok(false),
        }
    }

    fn has_exited(&mut self) -> Result<bool> {
        if self.exit.is_some() {
            return Ok(true);
        }
        if let Some(status) = self.child.try_wait().context("poll session command")? {
            self.exit = Some(status.into());
            self.finish_exited_output()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn terminate(&mut self) {
        if self.stopped {
            return;
        }
        #[cfg(unix)]
        if let Some(process_group) = self.process_group.take() {
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.exit.is_none() && Instant::now() < deadline {
            // The PTY reader may be blocked by the bounded queue while the child exits.
            // Keep draining one chunk at a time so forced shutdown cannot deadlock on backpressure.
            let _ = self.consume_one();
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exit = Some(status.into());
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.output_closed = true;
        self.stopped = true;
        self.semantic.take();
    }

    fn finish_exited_output(&mut self) -> Result<()> {
        let kill_after = Instant::now() + Duration::from_millis(50);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !self.output_closed && Instant::now() < deadline {
            // A cleanly exited application should close the PTY promptly. Only signal its
            // saved group if output remains open long enough to indicate a live descendant.
            #[cfg(unix)]
            if Instant::now() >= kill_after
                && let Some(process_group) = self.process_group.take()
            {
                unsafe {
                    libc::kill(-process_group, libc::SIGKILL);
                }
            }
            match self.receive.recv_timeout(Duration::from_millis(10)) {
                Ok(Some(output)) => self.apply_output(output)?,
                Ok(None) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.output_closed = true;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        #[cfg(unix)]
        if self.output_closed {
            self.process_group.take();
        }
        self.semantic.take();
        Ok(())
    }

    fn apply_output(&mut self, output: Output) -> Result<()> {
        if let Some(mirror) = &mut self.mirror {
            mirror
                .write_all(&output.bytes)
                .context("mirror PTY output")?;
            mirror.flush().context("flush mirrored PTY output")?;
        }
        if let Some(recording) = &mut self.recording {
            recording.output(output.at_ms, &output.bytes)?;
        }
        let response = respond_to_output(&mut self.terminal, &mut self.host, &output.bytes)?;
        if !response.is_empty()
            && let Some(recording) = &mut self.recording
        {
            recording.input(InputOrigin::Host, &response)?;
        }
        retain_recent(
            &mut self.ansi,
            &output.bytes,
            self.max_bytes,
            &mut self.ansi_truncated,
        );
        self.last_output = Some(Instant::now());
        Ok(())
    }
}

fn retain_recent(ansi: &mut Vec<u8>, bytes: &[u8], max_bytes: usize, truncated: &mut bool) {
    if max_bytes == 0 {
        *truncated |= !bytes.is_empty();
        ansi.clear();
        return;
    }
    if bytes.len() >= max_bytes {
        *truncated |= !ansi.is_empty() || bytes.len() > max_bytes;
        ansi.clear();
        ansi.extend_from_slice(&bytes[bytes.len() - max_bytes..]);
        return;
    }
    let excess = ansi
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(max_bytes);
    if excess > 0 {
        ansi.drain(..excess);
        *truncated = true;
    }
    ansi.extend_from_slice(bytes);
}

impl Drop for Session {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Serialize, Deserialize)]
enum Request {
    Ping,
    Status,
    Wait {
        text: String,
        timeout_ms: u64,
    },
    Send {
        input: Vec<Vec<u8>>,
        pace_ms: u64,
    },
    Show {
        settle_ms: u64,
        deadline_ms: u64,
    },
    Logs {
        ansi: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
        cell_width: Option<u16>,
        cell_height: Option<u16>,
    },
    Mark {
        name: String,
    },
    SemanticSnapshot {
        deadline_unix_ms: u64,
    },
    Stop,
}

#[derive(Serialize, Deserialize)]
struct Response {
    #[serde(default)]
    protocol_version: u8,
    error: Option<String>,
    captured: Option<Shot>,
    status: Option<SessionStatus>,
    logs: Option<Vec<u8>>,
    #[serde(default)]
    semantic_snapshot: Option<SemanticSnapshotResult>,
}

impl Default for Response {
    fn default() -> Self {
        Self {
            protocol_version: SEMANTIC_PROTOCOL_VERSION,
            error: None,
            captured: None,
            status: None,
            logs: None,
            semantic_snapshot: None,
        }
    }
}

impl Response {
    fn error(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::default()
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SemanticSnapshotResult {
    value: Value,
}

#[doc(hidden)]
pub fn start(
    name: &str,
    command: &[String],
    cwd: Option<&Path>,
    record: Option<&Path>,
    options: &Options,
) -> Result<()> {
    validate_name(name)?;
    implementation::start(name, command, cwd, record, options)
}

#[doc(hidden)]
pub fn restart(
    name: &str,
    command: &[String],
    cwd: Option<&Path>,
    record: Option<&Path>,
    options: &Options,
) -> Result<()> {
    validate_name(name)?;
    implementation::restart(name, command, cwd, record, options)
}

#[doc(hidden)]
pub fn wait(name: &str, text: String, timeout: Duration) -> Result<()> {
    request(
        name,
        Request::Wait {
            text,
            timeout_ms: timeout.as_millis() as u64,
        },
    )?;
    Ok(())
}

#[doc(hidden)]
pub fn status(name: &str) -> Result<SessionStatus> {
    request(name, Request::Status)?
        .status
        .ok_or_else(|| anyhow::anyhow!("session did not return status"))
}

#[doc(hidden)]
pub fn send(name: &str, input: Vec<Vec<u8>>, pace: Duration) -> Result<()> {
    request(
        name,
        Request::Send {
            input,
            pace_ms: pace.as_millis() as u64,
        },
    )?;
    Ok(())
}

#[doc(hidden)]
pub fn show(name: &str, settle: Duration, deadline: Duration) -> Result<Shot> {
    request(
        name,
        Request::Show {
            settle_ms: settle.as_millis() as u64,
            deadline_ms: deadline.as_millis() as u64,
        },
    )?
    .captured
    .ok_or_else(|| anyhow::anyhow!("session did not return a visible screen"))
}

pub(crate) fn show_with_status(
    name: &str,
    settle: Duration,
    deadline: Duration,
) -> Result<(Shot, SessionStatus)> {
    let response = request(
        name,
        Request::Show {
            settle_ms: settle.as_millis() as u64,
            deadline_ms: deadline.as_millis() as u64,
        },
    )?;
    Ok((
        response
            .captured
            .context("session did not return a visible screen")?,
        response.status.context("session did not return status")?,
    ))
}

#[doc(hidden)]
pub fn semantic_snapshot(name: &str, timeout: Duration) -> Result<Value> {
    validate_name(name)?;
    let response: Response = semantic::request_snapshot(
        &socket_path(name)?,
        &Request::SemanticSnapshot {
            deadline_unix_ms: semantic::deadline_unix_ms(timeout)?,
        },
        timeout,
    )?;
    if response.protocol_version < SEMANTIC_PROTOCOL_VERSION {
        bail!("running session predates semantic snapshots; restart it with the current termctrl");
    }
    if let Some(error) = response.error {
        bail!(error);
    }
    response
        .semantic_snapshot
        .map(|snapshot| snapshot.value)
        .context("session did not return a semantic snapshot")
}

#[doc(hidden)]
pub fn resize(
    name: &str,
    cols: u16,
    rows: u16,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
) -> Result<()> {
    request(
        name,
        Request::Resize {
            cols,
            rows,
            cell_width,
            cell_height,
        },
    )?;
    Ok(())
}

#[doc(hidden)]
pub fn mark(name: &str, marker: String) -> Result<()> {
    request(name, Request::Mark { name: marker })?;
    Ok(())
}

#[doc(hidden)]
pub fn logs(name: &str, ansi: bool) -> Result<Vec<u8>> {
    request(name, Request::Logs { ansi })?
        .logs
        .ok_or_else(|| anyhow::anyhow!("session did not return logs"))
}

#[doc(hidden)]
pub fn list() -> Result<Vec<NamedSessionStatus>> {
    implementation::list()
}

#[doc(hidden)]
pub fn prune(name: &str, dry_run: bool) -> Result<Option<PruneKind>> {
    validate_name(name)?;
    implementation::prune(name, dry_run)
}

#[doc(hidden)]
pub fn stop(name: &str) -> Result<()> {
    request(name, Request::Stop)?;
    Ok(())
}

#[doc(hidden)]
pub fn serve(
    name: String,
    socket: PathBuf,
    command: Vec<String>,
    cwd: Option<PathBuf>,
    record: Option<PathBuf>,
    options: Options,
) -> Result<()> {
    implementation::serve(name, socket, command, cwd, record, options)
}

/// Run a named session in the foreground, mirrored through the current terminal.
pub fn run_foreground(
    name: &str,
    command: &[String],
    cwd: Option<&Path>,
    record: Option<&Path>,
    options: &Options,
) -> Result<ProcessExit> {
    validate_name(name)?;
    implementation::run_foreground(name, command, cwd, record, options)
}

#[doc(hidden)]
pub fn infer_name(command: &[String]) -> Result<String> {
    let executable = command.first().ok_or_else(|| {
        anyhow::anyhow!("cannot infer a session name: provide a command after --")
    })?;
    let name = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot infer a session name from executable {executable:?}: basename is empty or invalid"
            )
        })?;
    validate_name(name).map_err(|error| {
        anyhow::anyhow!(
            "cannot infer a session name from executable {executable:?}: basename {name:?} is invalid: {error}"
        )
    })?;
    Ok(name.to_owned())
}

fn request(name: &str, request: Request) -> Result<Response> {
    validate_name(name)?;
    let response = implementation::request(socket_path(name)?, &request)?;
    if let Some(error) = response.error {
        bail!(error);
    }
    Ok(response)
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '-' | '_' | '.'))
    {
        bail!("session names may contain only ASCII letters, digits, '.', '-', and '_'");
    }
    Ok(())
}

fn socket_path(name: &str) -> Result<PathBuf> {
    Ok(implementation::runtime_dir()?.join(format!("{name}.sock")))
}

#[cfg(unix)]
mod implementation {
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::{ErrorKind, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};

    use super::{
        NamedSessionStatus, ProcessExit, PruneKind, Request, Response, SemanticSnapshotResult,
        Session, SessionState, UnavailableReason,
    };
    use crate::shot::{self, Options};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
    const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

    struct StartLock(fs::File);

    impl StartLock {
        fn acquire(path: &Path) -> Result<Self> {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(path)
                .with_context(|| format!("open {}", path.display()))?;
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                bail!("another session operation is already starting this name");
            }
            Ok(Self(file))
        }
    }

    impl Drop for StartLock {
        fn drop(&mut self) {
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
    pub fn runtime_dir() -> Result<PathBuf> {
        crate::runtime::directory()
    }

    pub fn start(
        name: &str,
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
    ) -> Result<()> {
        if command.is_empty() {
            bail!("provide a command after --");
        }
        let runtime = runtime_dir()?;
        ensure_socket_path(&runtime.join(format!("{name}.sock")))?;
        let _lock = StartLock::acquire(&runtime.join(format!("{name}.lock")))?;
        start_locked(name, command, cwd, record, options, &runtime)
    }

    pub fn restart(
        name: &str,
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
    ) -> Result<()> {
        if command.is_empty() {
            bail!("provide a command after --");
        }
        let runtime = runtime_dir()?;
        ensure_socket_path(&runtime.join(format!("{name}.sock")))?;
        let _lock = StartLock::acquire(&runtime.join(format!("{name}.lock")))?;
        let socket = runtime.join(format!("{name}.sock"));
        if let Ok(response) = request(socket.clone(), &Request::Stop) {
            if let Some(error) = response.error {
                bail!(error);
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while request(socket.clone(), &Request::Ping).is_ok() {
                if Instant::now() >= deadline {
                    bail!("timed out stopping session {name:?} before restart");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        start_locked(name, command, cwd, record, options, &runtime)
    }

    fn start_locked(
        name: &str,
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
        runtime: &Path,
    ) -> Result<()> {
        let socket = runtime.join(format!("{name}.sock"));
        remove_stale_socket(name, &socket)?;
        let mut daemon =
            Command::new(std::env::current_exe().context("locate termctrl executable")?);
        daemon
            .arg("__serve")
            .arg("--name")
            .arg(name)
            .arg("--socket")
            .arg(&socket)
            .arg("--cols")
            .arg(options.cols.to_string())
            .arg("--rows")
            .arg(options.rows.to_string())
            .arg("--cell-width")
            .arg(options.cell_width.to_string())
            .arg("--cell-height")
            .arg(options.cell_height.to_string())
            .arg("--max-bytes")
            .arg(options.max_bytes.to_string());
        if options.opentui_host {
            daemon.arg("--opentui-host");
        }
        match options.color {
            shot::ColorMode::Auto => {}
            shot::ColorMode::Always => {
                daemon.arg("--color").arg("always");
            }
            shot::ColorMode::Never => {
                daemon.arg("--color").arg("never");
            }
        }
        if let Some(cwd) = cwd {
            daemon.arg("--cwd").arg(cwd);
        }
        if let Some(record) = record {
            let record = if record.is_absolute() {
                record.to_owned()
            } else {
                std::env::current_dir()
                    .context("resolve recording output directory")?
                    .join(record)
            };
            daemon.arg("--record").arg(record);
        }
        daemon
            .arg("--")
            .args(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut daemon = daemon.spawn().context("start session daemon")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if request(socket.clone(), &Request::Ping).is_ok() {
                return Ok(());
            }
            if let Some(status) = daemon.try_wait().context("poll session daemon")? {
                bail!("session daemon exited before becoming ready: {status}");
            }
            if Instant::now() >= deadline {
                let _ = daemon.kill();
                bail!("timed out starting session {name:?}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn request(socket: PathBuf, request: &Request) -> Result<Response> {
        ensure_socket_path(&socket)?;
        let mut stream = UnixStream::connect(&socket).with_context(|| {
            format!("connect to session at {}; is it running?", socket.display())
        })?;
        let mut writer = std::io::BufWriter::with_capacity(64 * 1024, &mut stream);
        serde_json::to_writer(&mut writer, request).context("write session request")?;
        writer.flush().context("flush session request")?;
        drop(writer);
        stream
            .shutdown(std::net::Shutdown::Write)
            .context("finish session request")?;
        serde_json::from_reader(std::io::BufReader::with_capacity(64 * 1024, stream))
            .context("read session response")
    }

    pub fn list() -> Result<Vec<NamedSessionStatus>> {
        let mut sessions = Vec::new();
        for entry in fs::read_dir(runtime_dir()?).context("read session runtime directory")? {
            let path = entry.context("read session runtime entry")?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("sock") {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let (status, error, unavailable) = match request(path, &Request::Status) {
                Ok(response) => (response.status, response.error, None),
                Err(error) => {
                    let reason = if stale_socket_error(&error) {
                        UnavailableReason::Stale
                    } else {
                        UnavailableReason::IncompatibleProtocol
                    };
                    (None, Some(format!("{error:#}")), Some(reason))
                }
            };
            sessions.push(NamedSessionStatus {
                name,
                status,
                error,
                unavailable,
            });
        }
        sessions.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(sessions)
    }

    pub fn prune(name: &str, dry_run: bool) -> Result<Option<PruneKind>> {
        let runtime = runtime_dir()?;
        let _lock = StartLock::acquire(&runtime.join(format!("{name}.lock")))?;
        let socket = runtime.join(format!("{name}.sock"));
        ensure_socket_path(&socket)?;
        let metadata = match fs::symlink_metadata(&socket) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect session socket {}", socket.display()));
            }
        };
        if !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            bail!(
                "refusing to prune untrusted session socket {}",
                socket.display()
            );
        }
        match request(socket.clone(), &Request::Status) {
            Ok(response) => {
                if let Some(error) = response.error {
                    bail!(error);
                }
                let status = response
                    .status
                    .context("session did not return status while pruning")?;
                if status.state != SessionState::Exited {
                    return Ok(None);
                }
                if !dry_run {
                    let response = request(socket, &Request::Stop)?;
                    if let Some(error) = response.error {
                        bail!(error);
                    }
                }
                Ok(Some(PruneKind::Exited))
            }
            Err(error) if stale_socket_error(&error) => {
                if !dry_run {
                    fs::remove_file(&socket).with_context(|| {
                        format!("remove stale session socket {}", socket.display())
                    })?;
                }
                Ok(Some(PruneKind::Stale))
            }
            Err(error) => Err(error).context("refusing to prune an unresponsive session"),
        }
    }

    fn stale_socket_error(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    ErrorKind::ConnectionRefused | ErrorKind::NotFound
                )
            })
        })
    }

    pub fn serve(
        name: String,
        socket: PathBuf,
        command: Vec<String>,
        cwd: Option<PathBuf>,
        record: Option<PathBuf>,
        mut options: Options,
    ) -> Result<()> {
        ensure_socket_path(&socket)?;
        if command.is_empty() {
            bail!("provide a command after --");
        }
        options.env.insert("TERMCTRL_SESSION".to_owned(), name);
        let result = (|| {
            let listener = bind_listener(&socket)?;
            let mut session =
                Session::start(&command, cwd.as_deref(), record.as_deref(), &options)?;
            let result = run(&listener, &mut session);
            let _ = session.stop();
            result
        })();
        let _ = fs::remove_file(&socket);
        result
    }

    struct RawMode;

    impl RawMode {
        fn enter() -> Result<Self> {
            enable_raw_mode().context("enable terminal raw mode")?;
            Ok(Self)
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(
                b"\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?2004l\x1b[?1049l",
            );
            let _ = stdout.flush();
            let _ = disable_raw_mode();
        }
    }

    pub fn run_foreground(
        name: &str,
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
    ) -> Result<ProcessExit> {
        if command.is_empty() {
            bail!("provide a command after --");
        }
        let runtime = runtime_dir()?;
        let socket = runtime.join(format!("{name}.sock"));
        ensure_socket_path(&socket)?;
        let _lock = StartLock::acquire(&runtime.join(format!("{name}.lock")))?;
        remove_stale_socket(name, &socket)?;
        let result = (|| {
            let listener = bind_listener(&socket)?;
            let _raw = RawMode::enter()?;
            let mut options = options.clone();
            options
                .env
                .insert("TERMCTRL_SESSION".to_owned(), name.to_owned());
            let mut session =
                Session::start_with_host_responses(command, cwd, record, &options, false)?;
            session.mirror_to(std::io::stdout());
            let (input_send, input_receive) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);
            thread::spawn(move || {
                let mut stdin = std::io::stdin().lock();
                let mut bytes = [0_u8; 1024];
                while let Ok(length) = stdin.read(&mut bytes) {
                    if length == 0 || input_send.send(bytes[..length].to_vec()).is_err() {
                        break;
                    }
                }
            });
            let exit = loop {
                session.consume_batch()?;
                drain_input(&mut session, &input_receive)?;
                match listener.accept() {
                    Ok((stream, _)) => {
                        if handle(stream, &mut session, |session| {
                            drain_input(session, &input_receive)
                        })? {
                            break session
                                .exit
                                .clone()
                                .context("stopped foreground session without an exit status")?;
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error).context("accept session request"),
                }
                if session.has_exited()? || session.stopped {
                    break session
                        .exit
                        .clone()
                        .context("foreground session ended without an exit status")?;
                }
                if let Ok((cols, rows)) = crossterm::terminal::size()
                    && cols > 0
                    && rows > 0
                    && (cols != session.cols || rows != session.rows)
                {
                    session.resize(cols, rows, session.cell_width, session.cell_height)?;
                }
                thread::sleep(Duration::from_millis(5));
            };
            Ok(exit)
        })();
        let _ = fs::remove_file(&socket);
        result
    }

    fn ensure_socket_path(path: &Path) -> Result<()> {
        if path.as_os_str().as_encoded_bytes().len() >= 100 {
            bail!(
                "session socket path is too long for portable Unix sockets: {}; set TERMCTRL_RUNTIME_DIR to a shorter directory",
                path.display()
            );
        }
        Ok(())
    }

    fn remove_stale_socket(name: &str, socket: &Path) -> Result<()> {
        if request(socket.to_owned(), &Request::Ping).is_ok() {
            bail!("session {name:?} is already running");
        }
        match fs::remove_file(socket) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove stale {}", socket.display())),
        }
    }

    fn bind_listener(socket: &Path) -> Result<UnixListener> {
        let listener =
            UnixListener::bind(socket).with_context(|| format!("bind {}", socket.display()))?;
        fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("secure {}", socket.display()))?;
        listener
            .set_nonblocking(true)
            .context("set session socket nonblocking")?;
        Ok(listener)
    }

    fn drain_input(
        session: &mut Session,
        input: &std::sync::mpsc::Receiver<Vec<u8>>,
    ) -> Result<()> {
        while let Ok(input) = input.try_recv() {
            session.send(&input)?;
        }
        Ok(())
    }

    fn run(listener: &UnixListener, session: &mut Session) -> Result<()> {
        loop {
            // Keep parsing and recording output even when no control request is in flight.
            session.consume_batch()?;
            match listener.accept() {
                Ok((stream, _)) => {
                    if handle(stream, session, |_| Ok(()))? {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error).context("accept session request"),
            }
        }
    }

    fn handle(
        mut stream: UnixStream,
        session: &mut Session,
        mut on_tick: impl FnMut(&mut Session) -> Result<()>,
    ) -> Result<bool> {
        stream
            .set_nonblocking(false)
            .context("set session connection blocking")?;
        stream
            .set_read_timeout(Some(CONTROL_TIMEOUT))
            .context("set session request timeout")?;
        stream
            .set_write_timeout(Some(CONTROL_TIMEOUT))
            .context("set session response timeout")?;
        let mut bytes = Vec::new();
        let response = match Read::by_ref(&mut stream)
            .take(MAX_REQUEST_BYTES + 1)
            .read_to_end(&mut bytes)
        {
            Ok(_) if bytes.len() as u64 > MAX_REQUEST_BYTES => {
                Response::error("session request exceeds 1 MiB")
            }
            Ok(_) => match serde_json::from_slice::<Request>(&bytes) {
                Ok(request) => {
                    let stop = matches!(request, Request::Stop);
                    let response = match respond(session, request, &mut on_tick) {
                        Ok(response) => response,
                        Err(error) => Response::error(format!("{error:#}")),
                    };
                    if write_response(&mut stream, &response).is_ok() && stop {
                        return Ok(true);
                    }
                    return Ok(false);
                }
                Err(error) => Response::error(format!("invalid session request: {error}")),
            },
            Err(error) => Response::error(format!("failed to read session request: {error}")),
        };
        let _ = write_response(&mut stream, &response);
        Ok(false)
    }

    fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
        let mut writer = std::io::BufWriter::with_capacity(64 * 1024, stream);
        serde_json::to_writer(&mut writer, response).context("write session response")?;
        writer.flush().context("flush session response")
    }

    fn respond(
        session: &mut Session,
        request: Request,
        on_tick: &mut impl FnMut(&mut Session) -> Result<()>,
    ) -> Result<Response> {
        let mut response = Response::default();
        match request {
            Request::Ping => {}
            Request::Status => response.status = Some(session.status()?),
            Request::Send { input, pace_ms } => {
                session.send_all(&input, Duration::from_millis(pace_ms))?;
            }
            Request::Wait { text, timeout_ms } => {
                session.wait_for_text_with(
                    &text,
                    Duration::from_millis(timeout_ms),
                    |session| on_tick(session),
                )?;
            }
            Request::Show {
                settle_ms,
                deadline_ms,
            } => {
                response.captured = Some(
                    session
                        .capture_with(
                            Duration::from_millis(settle_ms),
                            Duration::from_millis(deadline_ms),
                            |session| on_tick(session),
                        )?
                        .shot,
                );
                response.status = Some(session.status()?);
            }
            Request::Logs { ansi } => response.logs = Some(session.logs(ansi)?),
            Request::Resize {
                cols,
                rows,
                cell_width,
                cell_height,
            } => {
                let status = session.status()?;
                session.resize(
                    cols,
                    rows,
                    cell_width.unwrap_or(status.cell_width),
                    cell_height.unwrap_or(status.cell_height),
                )?;
            }
            Request::Mark { name } => session.mark(&name)?,
            Request::SemanticSnapshot { deadline_unix_ms } => {
                response.semantic_snapshot = Some(SemanticSnapshotResult {
                    value: session.semantic_snapshot_with(
                        crate::semantic::remaining(deadline_unix_ms)?,
                        |session| on_tick(session),
                    )?,
                });
            }
            Request::Stop => session.stop()?,
        }
        Ok(response)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn name_start_lock_rejects_a_concurrent_owner() {
            let path = std::env::temp_dir().join(format!(
                "termctrl-start-lock-test-{}.lock",
                std::process::id()
            ));
            let held = StartLock::acquire(&path).unwrap();

            assert!(StartLock::acquire(&path).is_err());
            drop(held);
            assert!(StartLock::acquire(&path).is_ok());
            let _ = fs::remove_file(path);
        }

        #[test]
        fn named_daemon_exposes_its_session_identity() {
            let name = format!("identity-{}", std::process::id());
            let socket = std::env::temp_dir().join(format!("termctrl-{name}.sock"));
            let server_name = name.clone();
            let server_socket = socket.clone();
            let server = thread::spawn(move || {
                serve(
                    server_name,
                    server_socket,
                    vec![
                        "sh".to_owned(),
                        "-c".to_owned(),
                        "printf '%s' \"$TERMCTRL_SESSION\"; sleep 10".to_owned(),
                    ],
                    None,
                    None,
                    Options::default(),
                )
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            while request(socket.clone(), &Request::Ping).is_err() {
                assert!(Instant::now() < deadline, "session daemon did not start");
                thread::sleep(Duration::from_millis(10));
            }
            request(
                socket.clone(),
                &Request::Wait {
                    text: name.clone(),
                    timeout_ms: 2_000,
                },
            )
            .unwrap();

            let response = request(
                socket.clone(),
                &Request::Show {
                    settle_ms: 0,
                    deadline_ms: 0,
                },
            )
            .unwrap();
            assert_eq!(response.captured.unwrap().frame.text(), name);
            request(socket, &Request::Stop).unwrap();
            server.join().unwrap().unwrap();
        }
    }
}

#[cfg(not(unix))]
mod implementation {
    use super::{NamedSessionStatus, Options, ProcessExit, PruneKind, Request, Response};
    use anyhow::{Result, bail};
    use std::path::{Path, PathBuf};

    pub fn runtime_dir() -> Result<PathBuf> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn start(
        _: &str,
        _: &[String],
        _: Option<&Path>,
        _: Option<&Path>,
        _: &Options,
    ) -> Result<()> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn restart(
        _: &str,
        _: &[String],
        _: Option<&Path>,
        _: Option<&Path>,
        _: &Options,
    ) -> Result<()> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn request(_: PathBuf, _: &Request) -> Result<Response> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn list() -> Result<Vec<NamedSessionStatus>> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn prune(_: &str, _: bool) -> Result<Option<PruneKind>> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn serve(
        _: String,
        _: PathBuf,
        _: Vec<String>,
        _: Option<PathBuf>,
        _: Option<PathBuf>,
        _: Options,
    ) -> Result<()> {
        bail!("persistent sessions require Unix sockets")
    }
    pub fn run_foreground(
        _: &str,
        _: &[String],
        _: Option<&Path>,
        _: Option<&Path>,
        _: &Options,
    ) -> Result<ProcessExit> {
        bail!("persistent sessions require Unix sockets")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed_session(name: &str, state: SessionState, cwd: &str) -> NamedSessionStatus {
        NamedSessionStatus {
            name: name.to_owned(),
            status: Some(SessionStatus {
                state,
                exit: None,
                cols: 80,
                rows: 24,
                cell_width: 9,
                cell_height: 18,
                idle_for_ms: None,
                has_visible_content: false,
                recording: false,
                logs_truncated: false,
                launch: SessionLaunch {
                    command: vec!["nvim".to_owned(), "README.md".to_owned()],
                    cwd: PathBuf::from(cwd),
                    record: None,
                    cols: 80,
                    rows: 24,
                    cell_width: 9,
                    cell_height: 18,
                    max_bytes: 1024,
                    opentui_host: false,
                    color: shot::ColorMode::Auto,
                },
            }),
            error: None,
            unavailable: None,
        }
    }

    #[test]
    fn filters_discovered_sessions_by_state_command_and_cwd() {
        let running = listed_session("editor", SessionState::Running, "/repo");
        assert!(running.matches(&SessionFilter {
            state: Some(SessionListState::Running),
            command: Some("README".to_owned()),
            cwd: Some(PathBuf::from("/repo")),
        }));
        assert!(!running.matches(&SessionFilter {
            state: Some(SessionListState::Exited),
            ..SessionFilter::default()
        }));
        assert!(!running.matches(&SessionFilter {
            command: Some("cargo".to_owned()),
            ..SessionFilter::default()
        }));

        let stale = NamedSessionStatus {
            name: "old".to_owned(),
            status: None,
            error: Some("connection refused".to_owned()),
            unavailable: Some(UnavailableReason::Stale),
        };
        assert!(stale.matches(&SessionFilter {
            state: Some(SessionListState::Stale),
            ..SessionFilter::default()
        }));
        assert!(!stale.matches(&SessionFilter {
            command: Some("nvim".to_owned()),
            ..SessionFilter::default()
        }));
    }

    #[test]
    fn semantic_request_and_response_preserve_the_session_protocol() {
        let request = Request::SemanticSnapshot {
            deadline_unix_ms: 1,
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        assert!(matches!(
            serde_json::from_slice(&encoded).unwrap(),
            Request::SemanticSnapshot {
                deadline_unix_ms: 1
            }
        ));
        assert_eq!(
            Response::default().protocol_version,
            SEMANTIC_PROTOCOL_VERSION
        );
    }

    #[cfg(unix)]
    #[test]
    fn embedded_session_waits_sends_resizes_and_captures_the_screen() {
        let mut session = Session::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf ready; IFS= read -r line; printf '\\r\\ngot:%s' \"$line\"; sleep 1"
                    .to_owned(),
            ],
            None,
            None,
            &Options {
                cols: 20,
                rows: 4,
                settle: Duration::from_millis(10),
                deadline: Duration::from_secs(2),
                ..Options::default()
            },
        )
        .unwrap();

        session
            .wait_for_text("ready", Duration::from_secs(2))
            .unwrap();
        session.send(b"hello\r").unwrap();
        session
            .wait_for_text("got:hello", Duration::from_secs(2))
            .unwrap();
        assert_eq!(session.status().unwrap().state, SessionState::Running);
        session
            .wait_for_idle(Duration::from_millis(10), Duration::from_secs(2))
            .unwrap();
        session.resize(30, 5, 9, 18).unwrap();
        let shot = session
            .capture(Duration::from_millis(10), Duration::from_secs(2))
            .unwrap();

        assert_eq!((shot.shot.frame.cols, shot.shot.frame.rows), (30, 5));
        assert!(shot.shot.frame.text().contains("got:hello"));
        session.stop().unwrap();
        assert_eq!(session.status().unwrap().state, SessionState::Exited);
        assert!(session.capture(Duration::ZERO, Duration::ZERO).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn waits_can_cooperatively_process_input() {
        let mut session = Session::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "IFS= read -r line; printf 'got:%s' \"$line\"; sleep 1".to_owned(),
            ],
            None,
            None,
            &Options::default(),
        )
        .unwrap();
        let mut sent = false;

        session
            .wait_for_text_with("got:hello", Duration::from_secs(2), |session| {
                if !sent {
                    session.send(b"hello\r")?;
                    sent = true;
                }
                Ok(())
            })
            .unwrap();

        session.stop().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn embedded_session_advertises_and_cleans_up_the_application_semantic_socket() {
        let mut session = Session::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                format!(
                    "if [ -S \"${}\" ]; then printf semantic-ready; else printf semantic-missing; fi; sleep 10",
                    semantic::SOCKET_ENV
                ),
            ],
            None,
            None,
            &Options {
                opentui_host: true,
                ..Options::default()
            },
        )
        .unwrap();
        let path = session
            .semantic
            .as_ref()
            .unwrap()
            .path()
            .unwrap()
            .to_owned();

        session
            .wait_for_text("semantic-ready", Duration::from_secs(2))
            .unwrap();
        assert!(path.exists());
        session.stop().unwrap();
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn embedded_session_reads_its_application_semantic_snapshot() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixStream;

        let mut session = Session::start(
            &["sh".to_owned(), "-c".to_owned(), "sleep 10".to_owned()],
            None,
            None,
            &Options {
                opentui_host: true,
                ..Options::default()
            },
        )
        .unwrap();
        let path = session
            .semantic
            .as_ref()
            .unwrap()
            .path()
            .unwrap()
            .to_owned();
        let (connected, ready) = mpsc::channel();
        let application = thread::spawn(move || {
            let mut stream = UnixStream::connect(path).unwrap();
            stream
                .write_all(
                    b"{\"type\":\"hello\",\"protocolVersion\":1,\"application\":{\"name\":\"fixture\"},\"capabilities\":[\"semantic.snapshot\"]}\n",
                )
                .unwrap();
            connected.send(()).unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).unwrap();
            assert!(line.contains("\"type\":\"ready\""));
            line.clear();
            stream.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["type"], "semantic.snapshot");
            let response = serde_json::json!({
                "type": "result",
                "id": request["id"],
                "value": { "format": "termctrl-semantic-snapshot-v1", "nodes": [] }
            });
            serde_json::to_writer(stream.get_mut(), &response).unwrap();
            stream.get_mut().write_all(b"\n").unwrap();
            stream.get_mut().flush().unwrap();
        });
        ready.recv().unwrap();

        let result = session.semantic_snapshot(Duration::from_secs(2)).unwrap();

        assert_eq!(result["format"], "termctrl-semantic-snapshot-v1");
        session.stop().unwrap();
        application.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn capture_reports_a_deadline_instead_of_implying_idle() {
        let mut session = Session::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; sleep 0.001; done"
                    .to_owned(),
            ],
            None,
            None,
            &Options::default(),
        )
        .unwrap();

        let capture = session
            .capture(Duration::from_secs(1), Duration::from_millis(50))
            .unwrap();

        assert_eq!(capture.reason, CaptureReason::Deadline);
        session.stop().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn session_resizes_after_retained_output_is_truncated() {
        let mut session = Session::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf '123456789'; sleep 1".to_owned(),
            ],
            None,
            None,
            &Options {
                max_bytes: 4,
                ..Options::default()
            },
        )
        .unwrap();
        session
            .wait_for_text("123456789", Duration::from_secs(2))
            .unwrap();

        assert_eq!(session.logs(true).unwrap(), b"6789");
        assert!(session.status().unwrap().logs_truncated);
        session.resize(4, 3, 9, 18).unwrap();
        let capture = session.capture(Duration::ZERO, Duration::ZERO).unwrap();
        assert_eq!((capture.shot.frame.cols, capture.shot.frame.rows), (4, 3));
        assert_eq!(capture.shot.frame.text(), "1234\n5678\n9");
        session.stop().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn status_preserves_the_observed_process_exit() {
        let mut session = Session::start(
            &["sh".to_owned(), "-c".to_owned(), "exit 7".to_owned()],
            None,
            None,
            &Options::default(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            let status = session.status().unwrap();
            if status.state == SessionState::Exited {
                break status;
            }
            assert!(Instant::now() < deadline, "child did not exit");
            thread::sleep(Duration::from_millis(10));
        };

        assert_eq!(status.exit.unwrap().code, 7);
    }

    #[cfg(unix)]
    #[test]
    fn status_retains_canonical_launch_details() {
        let mut session = Session::start(
            &["/bin/sh".to_owned(), "-c".to_owned(), "sleep 1".to_owned()],
            Some(Path::new("/tmp")),
            None,
            &Options::default(),
        )
        .unwrap();

        let status = session.status().unwrap();
        assert_eq!(status.launch.command, ["/bin/sh", "-c", "sleep 1"]);
        assert_eq!(status.launch.cwd, std::fs::canonicalize("/tmp").unwrap());
        session.stop().unwrap();
    }

    #[test]
    fn infers_session_name_from_executable_basename() {
        assert_eq!(infer_name(&["nvim".to_owned()]).unwrap(), "nvim");
        assert_eq!(infer_name(&["/usr/bin/nvim".to_owned()]).unwrap(), "nvim");
    }

    #[test]
    fn rejects_empty_or_invalid_inferred_session_names() {
        assert!(
            infer_name(&["/".to_owned()])
                .unwrap_err()
                .to_string()
                .contains("basename is empty or invalid")
        );
        assert!(
            infer_name(&["/usr/bin/my editor".to_owned()])
                .unwrap_err()
                .to_string()
                .contains("basename \"my editor\" is invalid")
        );
        assert!(
            infer_name(&[])
                .unwrap_err()
                .to_string()
                .contains("provide a command after --")
        );
    }

    #[cfg(unix)]
    #[test]
    fn recorded_session_encodes_resize_in_its_timeline() {
        let record = std::env::temp_dir().join(format!(
            "termctrl-recorded-resize-test-{}.termctrl",
            std::process::id()
        ));
        let mut session = Session::start(
            &["sh".to_owned(), "-c".to_owned(), "sleep 1".to_owned()],
            None,
            Some(&record),
            &Options::default(),
        )
        .unwrap();

        session.resize(100, 32, 9, 18).unwrap();
        session.stop().unwrap();
        let recording = recording::read(&record).unwrap();
        assert!(matches!(
            recording.events.last(),
            Some(recording::Entry::Resize {
                cols: 100,
                rows: 32,
                ..
            })
        ));
        let _ = std::fs::remove_file(record);
    }

    #[cfg(unix)]
    #[test]
    fn waits_for_exit_without_polling_status() {
        let mut session = Session::start(
            &["sh".to_owned(), "-c".to_owned(), "exit 3".to_owned()],
            None,
            None,
            &Options::default(),
        )
        .unwrap();

        assert_eq!(
            session
                .wait_for_exit(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .code,
            3
        );
    }

    #[cfg(unix)]
    #[test]
    fn logs_expose_normal_screen_scrollback_and_raw_stream() {
        let mut session = Session::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf 'one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n'; sleep 1".to_owned(),
            ],
            None,
            None,
            &Options {
                cols: 20,
                rows: 2,
                ..Options::default()
            },
        )
        .unwrap();
        session
            .wait_for_text("five", Duration::from_secs(2))
            .unwrap();

        let logs = String::from_utf8(session.logs(false).unwrap()).unwrap();
        assert!(logs.contains("one"));
        assert!(logs.contains("five"));
        assert!(
            session
                .logs(true)
                .unwrap()
                .windows(3)
                .any(|bytes| bytes == b"one")
        );
        session.stop().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stopping_after_pty_eof_terminates_still_running_process() {
        let pid_path = std::env::temp_dir().join(format!(
            "termctrl-pty-eof-owner-test-{}.pid",
            std::process::id()
        ));
        let script = format!(
            "printf '%s' $$ > '{}'; exec >/dev/null 2>&1; sleep 30",
            pid_path.display()
        );
        let mut session = Session::start(
            &["sh".to_owned(), "-c".to_owned(), script],
            None,
            None,
            &Options::default(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_path) {
                break pid.parse::<i32>().unwrap();
            }
            assert!(Instant::now() < deadline, "child did not write its pid");
            thread::sleep(Duration::from_millis(10));
        };
        thread::sleep(Duration::from_millis(20));

        assert_eq!(session.status().unwrap().state, SessionState::Running);
        session.stop().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        let _ = std::fs::remove_file(pid_path);
    }

    #[cfg(unix)]
    #[test]
    fn natural_parent_exit_terminates_pty_holding_descendants() {
        let pid_path = std::env::temp_dir().join(format!(
            "termctrl-exited-owner-test-{}.pid",
            std::process::id()
        ));
        let script = format!(
            "sleep 30 & printf '%s' $! > '{}'; exit 0",
            pid_path.display()
        );
        let mut session = Session::start(
            &["sh".to_owned(), "-c".to_owned(), script],
            None,
            None,
            &Options::default(),
        )
        .unwrap();

        session
            .wait_for_exit(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let pid = std::fs::read_to_string(&pid_path)
            .unwrap()
            .parse::<i32>()
            .unwrap();

        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        let _ = std::fs::remove_file(pid_path);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_start_failure_removes_bound_socket() {
        let socket = std::env::temp_dir().join(format!(
            "termctrl-failed-daemon-start-{}.sock",
            std::process::id()
        ));
        let result = serve(
            "failed-start".to_owned(),
            socket.clone(),
            vec!["/definitely/not/a/termctrl-command".to_owned()],
            None,
            None,
            Options::default(),
        );

        assert!(result.is_err());
        assert!(!socket.exists());
    }
}
