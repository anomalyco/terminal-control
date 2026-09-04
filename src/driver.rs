//! Versioned stdio protocol for TypeScript and other external session clients.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::render;
use crate::session::Session;
use crate::shot::{ColorMode, Options};

/// Current JSON Lines protocol version spoken by `termctrl driver`.
pub const PROTOCOL_VERSION: u8 = 2;

/// Serve isolated embedded sessions over newline-delimited JSON requests and responses.
///
/// A hello message is written before any requests are read. Standard output is reserved for
/// protocol messages; callers should send diagnostic output elsewhere.
pub fn serve(reader: impl BufRead, mut writer: impl Write) -> Result<()> {
    write_message(
        &mut writer,
        &json!({
            "type": "hello",
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": ["mouse"],
            "terminalControlVersion": env!("CARGO_PKG_VERSION")
        }),
    )?;
    let mut sessions = HashMap::<String, ManagedSession>::new();
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                write_error(&mut writer, None, "READ_ERROR", &error.to_string())?;
                break;
            }
        };
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                write_error(&mut writer, None, "INVALID_REQUEST", &error.to_string())?;
                continue;
            }
        };
        let request_id = value.get("id").and_then(Value::as_u64);
        let request = match serde_json::from_value::<Request>(value) {
            Ok(request) => request,
            Err(error) => {
                write_error(
                    &mut writer,
                    request_id,
                    "INVALID_REQUEST",
                    &error.to_string(),
                )?;
                continue;
            }
        };
        let id = request.id;
        let shutdown = matches!(&request.method, Method::Shutdown);
        match dispatch(&mut sessions, request) {
            Ok(result) => write_message(
                &mut writer,
                &json!({ "type": "response", "id": id, "result": result }),
            )?,
            Err(error) => write_error(
                &mut writer,
                Some(id),
                "REQUEST_FAILED",
                &format!("{error:#}"),
            )?,
        }
        if shutdown {
            break;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Request {
    id: u64,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(flatten)]
    method: Method,
}

#[derive(Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
enum Method {
    Launch(LaunchParams),
    Status,
    Send(SendParams),
    Mouse(crate::mouse::MouseEvent),
    WaitForText(WaitForTextParams),
    WaitForIdle(WaitForIdleParams),
    WaitForExit(WaitForExitParams),
    Capture(CaptureParams),
    Logs(LogsParams),
    Recording,
    Resize(ResizeParams),
    Stop,
    Shutdown,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaunchParams {
    command: Vec<String>,
    cwd: Option<PathBuf>,
    record: Option<PathBuf>,
    cols: Option<u16>,
    rows: Option<u16>,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
    max_bytes: Option<usize>,
    host: Option<HostProfile>,
    color: Option<DriverColorMode>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default = "default_true")]
    inherit_env: bool,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum HostProfile {
    Opentui,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DriverColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendParams {
    input: Vec<InputAtom>,
    #[serde(default)]
    pace_ms: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum InputAtom {
    Text { value: String },
    Key { value: Key },
    Control { value: String },
    Bytes { value: Vec<u8> },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum Key {
    Enter,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Tab,
    ShiftTab,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaitForTextParams {
    text: String,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaitForIdleParams {
    #[serde(default = "default_settle_ms")]
    quiet_for_ms: u64,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaitForExitParams {
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaptureParams {
    #[serde(default = "default_settle_ms")]
    settle_ms: u64,
    #[serde(default = "default_timeout_ms")]
    deadline_ms: u64,
    #[serde(default)]
    include_ansi: bool,
    #[serde(default)]
    include_svg: bool,
}

#[derive(Default, Deserialize)]
struct LogsParams {
    #[serde(default)]
    ansi: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResizeParams {
    cols: u16,
    rows: u16,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
}

type SessionOperation = Box<dyn FnOnce(&mut Session) -> Result<Value> + Send>;

enum WorkerRequest {
    Call {
        operation: SessionOperation,
        reply: SyncSender<Result<Value>>,
    },
    Stop {
        reply: SyncSender<Result<Value>>,
    },
}

struct ManagedSession {
    send: SyncSender<WorkerRequest>,
    recording: Option<PathBuf>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ManagedSession {
    fn start(
        command: Vec<String>,
        cwd: Option<PathBuf>,
        recording: Option<PathBuf>,
        options: Options,
    ) -> Result<Self> {
        let (send, receive) = mpsc::sync_channel(1);
        let (started_send, started_receive) = mpsc::sync_channel(1);
        let session_recording = recording.clone();
        let worker = thread::spawn(move || {
            let mut session = match Session::start(
                &command,
                cwd.as_deref(),
                session_recording.as_deref(),
                &options,
            ) {
                Ok(session) => session,
                Err(error) => {
                    let _ = started_send.send(Err(error));
                    return;
                }
            };
            if started_send.send(Ok(())).is_err() {
                return;
            }
            let mut pump_error = None;
            loop {
                if pump_error.is_none()
                    && let Err(error) = session.pump()
                {
                    pump_error = Some(format!("{error:#}"));
                }
                match receive.recv_timeout(Duration::from_millis(10)) {
                    Ok(WorkerRequest::Call { operation, reply }) => {
                        let result = match &pump_error {
                            Some(error) => Err(anyhow!("session output pump failed: {error}")),
                            None => operation(&mut session),
                        };
                        let _ = reply.send(result);
                    }
                    Ok(WorkerRequest::Stop { reply }) => {
                        let result = session.stop().map(|()| Value::Null);
                        let _ = reply.send(result);
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        started_receive
            .recv()
            .context("session worker exited during startup")??;
        Ok(Self {
            send,
            recording,
            worker: Some(worker),
        })
    }

    fn call(
        &self,
        operation: impl FnOnce(&mut Session) -> Result<Value> + Send + 'static,
    ) -> Result<Value> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.send
            .send(WorkerRequest::Call {
                operation: Box::new(operation),
                reply,
            })
            .map_err(|_| anyhow!("session worker has exited"))?;
        receive.recv().context("session worker has exited")?
    }

    fn stop(mut self) -> Result<()> {
        self.stop_worker()
    }

    fn stop_worker(&mut self) -> Result<()> {
        let (reply, receive) = mpsc::sync_channel(1);
        let result = self
            .send
            .send(WorkerRequest::Stop { reply })
            .map_err(|_| anyhow!("session worker has exited"))
            .and_then(|()| receive.recv().context("session worker has exited"))
            .and_then(|result| result.map(|_| ()));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        result
    }

    fn recording(&self) -> Result<Vec<u8>> {
        let path = self
            .recording
            .as_ref()
            .ok_or_else(|| anyhow!("session was not launched with recording enabled"))?;
        fs::read(path).map_err(Into::into)
    }
}

impl Drop for ManagedSession {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.stop_worker();
        }
    }
}

fn dispatch(sessions: &mut HashMap<String, ManagedSession>, request: Request) -> Result<Value> {
    match request.method {
        Method::Launch(params) => {
            let session_id = required_session_id(&request.session_id)?;
            if sessions.contains_key(session_id) {
                bail!("driver session {session_id:?} already exists");
            }
            let options = options(&params);
            sessions.insert(
                session_id.to_owned(),
                ManagedSession::start(params.command, params.cwd, params.record, options)?,
            );
            Ok(json!({ "sessionId": session_id }))
        }
        Method::Status => session(sessions, &request.session_id)?
            .call(|session| Ok(serde_json::to_value(session.status()?)?)),
        Method::Send(params) => {
            let input = input_bytes(params.input, params.pace_ms > 0)?;
            session(sessions, &request.session_id)?.call(move |session| {
                session.send_all(&input, Duration::from_millis(params.pace_ms))?;
                Ok(Value::Null)
            })
        }
        Method::Mouse(event) => session(sessions, &request.session_id)?.call(move |session| {
            session.mouse(event)?;
            Ok(Value::Null)
        }),
        Method::WaitForText(params) => {
            session(sessions, &request.session_id)?.call(move |session| {
                session.wait_for_text(&params.text, Duration::from_millis(params.timeout_ms))?;
                Ok(Value::Null)
            })
        }
        Method::WaitForIdle(params) => {
            session(sessions, &request.session_id)?.call(move |session| {
                session.wait_for_idle(
                    Duration::from_millis(params.quiet_for_ms),
                    Duration::from_millis(params.timeout_ms),
                )?;
                Ok(Value::Null)
            })
        }
        Method::WaitForExit(params) => {
            session(sessions, &request.session_id)?.call(move |session| {
                let exit = session.wait_for_exit(Duration::from_millis(params.timeout_ms))?;
                Ok(match exit {
                    Some(exit) => json!({ "reason": "exited", "exit": exit }),
                    None => json!({ "reason": "deadline" }),
                })
            })
        }
        Method::Capture(params) => session(sessions, &request.session_id)?.call(move |session| {
            let mut capture = session.capture(
                Duration::from_millis(params.settle_ms),
                Duration::from_millis(params.deadline_ms),
            )?;
            let text = capture.shot.frame.text();
            let svg = params
                .include_svg
                .then(|| -> Result<String> {
                    let status = session.status()?;
                    let mut options = render::Options::default();
                    options.cell_width = f32::from(status.cell_width);
                    options.cell_height = f32::from(status.cell_height);
                    options.font_size = options.cell_height * 0.78;
                    Ok(render::svg(&capture.shot.frame, &options))
                })
                .transpose()?;
            if !params.include_ansi {
                capture.shot.ansi.clear();
            }
            let mut result = serde_json::to_value(capture)?;
            result["shot"]["text"] = json!(text);
            if let Some(svg) = svg {
                result["shot"]["svg"] = json!(svg);
            }
            Ok(result)
        }),
        Method::Logs(params) => session(sessions, &request.session_id)?.call(move |session| {
            Ok(json!({
                "ansi": params.ansi,
                "bytes": session.logs(params.ansi)?
            }))
        }),
        Method::Recording => {
            let session_id = required_session_id(&request.session_id)?;
            Ok(json!({
                "bytes": sessions
                    .get(session_id)
                    .ok_or_else(|| anyhow!("driver session {session_id:?} does not exist"))?
                    .recording()?
            }))
        }
        Method::Resize(params) => session(sessions, &request.session_id)?.call(move |session| {
            let status = session.status()?;
            session.resize(
                params.cols,
                params.rows,
                params.cell_width.unwrap_or(status.cell_width),
                params.cell_height.unwrap_or(status.cell_height),
            )?;
            Ok(Value::Null)
        }),
        Method::Stop => {
            let session_id = required_session_id(&request.session_id)?;
            let session = sessions
                .remove(session_id)
                .ok_or_else(|| anyhow!("driver session {session_id:?} does not exist"))?;
            session.stop()?;
            Ok(Value::Null)
        }
        Method::Shutdown => {
            for (_, session) in sessions.drain() {
                session.stop()?;
            }
            Ok(Value::Null)
        }
    }
}

fn required_session_id(session_id: &Option<String>) -> Result<&str> {
    session_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("request requires a non-empty sessionId"))
}

fn session<'a>(
    sessions: &'a mut HashMap<String, ManagedSession>,
    session_id: &Option<String>,
) -> Result<&'a ManagedSession> {
    let session_id = required_session_id(session_id)?;
    sessions
        .get(session_id)
        .ok_or_else(|| anyhow!("driver session {session_id:?} does not exist"))
}

fn options(params: &LaunchParams) -> Options {
    let mut options = Options::default();
    options.cols = params.cols.unwrap_or(options.cols);
    options.rows = params.rows.unwrap_or(options.rows);
    options.cell_width = params.cell_width.unwrap_or(options.cell_width);
    options.cell_height = params.cell_height.unwrap_or(options.cell_height);
    options.max_bytes = params.max_bytes.unwrap_or(options.max_bytes);
    options.opentui_host = matches!(params.host, Some(HostProfile::Opentui));
    options.color = match params.color {
        Some(DriverColorMode::Always) => ColorMode::Always,
        Some(DriverColorMode::Never) => ColorMode::Never,
        Some(DriverColorMode::Auto) | None => ColorMode::Auto,
    };
    options.env.extend(params.env.clone());
    options.inherit_env = params.inherit_env;
    options
}

fn input_bytes(input: Vec<InputAtom>, paced: bool) -> Result<Vec<Vec<u8>>> {
    let mut output = Vec::new();
    for atom in input {
        match atom {
            InputAtom::Text { value } if paced => {
                output.extend(value.chars().map(|char| char.to_string().into_bytes()));
            }
            InputAtom::Text { value } => output.push(value.into_bytes()),
            InputAtom::Key { value } => output.push(key_bytes(value).to_vec()),
            InputAtom::Control { value } => output.push(control_bytes(&value)?),
            InputAtom::Bytes { value } => output.push(value),
        }
    }
    Ok(output)
}

fn key_bytes(key: Key) -> &'static [u8] {
    match key {
        Key::Enter => b"\r",
        Key::Escape => b"\x1b",
        Key::ArrowUp => b"\x1b[A",
        Key::ArrowDown => b"\x1b[B",
        Key::ArrowLeft => b"\x1b[D",
        Key::ArrowRight => b"\x1b[C",
        Key::Tab => b"\t",
        Key::ShiftTab => b"\x1b[Z",
        Key::Backspace => b"\x7f",
        Key::Delete => b"\x1b[3~",
        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::PageUp => b"\x1b[5~",
        Key::PageDown => b"\x1b[6~",
    }
}

fn control_bytes(value: &str) -> Result<Vec<u8>> {
    if value.len() != 1 {
        bail!("control input value must be one ASCII letter");
    }
    let value = value.as_bytes()[0].to_ascii_lowercase();
    if !value.is_ascii_lowercase() {
        bail!("control input value must be one ASCII letter");
    }
    Ok(vec![value - b'a' + 1])
}

fn default_settle_ms() -> u64 {
    250
}

fn default_timeout_ms() -> u64 {
    5_000
}

fn default_true() -> bool {
    true
}

fn write_error(writer: &mut impl Write, id: Option<u64>, code: &str, message: &str) -> Result<()> {
    write_message(
        writer,
        &json!({ "type": "error", "id": id, "error": { "code": code, "message": message } }),
    )
}

fn write_message(writer: &mut impl Write, message: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, message)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor, Write};
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::*;

    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn jsonl_driver_operates_an_isolated_session() {
        let requests = concat!(
            r#"{"id":1,"method":"launch","sessionId":"app","params":{"command":["sh","-c","printf ready; IFS= read -r line; printf '\r\ngot:%s' \"$line\"; sleep 1"],"cols":20,"rows":4}}"#,
            "\n",
            r#"{"id":2,"method":"waitForText","sessionId":"app","params":{"text":"ready","timeoutMs":2000}}"#,
            "\n",
            r#"{"id":3,"method":"send","sessionId":"app","params":{"input":[{"type":"text","value":"hello"},{"type":"key","value":"enter"}]}}"#,
            "\n",
            r#"{"id":4,"method":"waitForText","sessionId":"app","params":{"text":"got:hello","timeoutMs":2000}}"#,
            "\n",
            r#"{"id":5,"method":"capture","sessionId":"app","params":{"settleMs":10,"deadlineMs":2000,"includeAnsi":true}}"#,
            "\n",
            r#"{"id":6,"method":"stop","sessionId":"app"}"#,
            "\n",
            r#"{"id":7,"method":"shutdown"}"#,
            "\n"
        );
        let mut output = Vec::new();

        serve(
            BufReader::new(Cursor::new(requests.as_bytes())),
            &mut output,
        )
        .unwrap();

        let messages = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(messages[0]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(messages[5]["result"]["reason"], "idle");
        assert!(messages[5]["result"]["shot"]["frame"]["cells"].is_array());
        assert_eq!(
            messages[5]["result"]["shot"]["text"],
            "readyhello\n\ngot:hello"
        );
        assert!(messages[5]["result"]["shot"]["ansi"].is_array());
        assert_eq!(messages[7]["result"], Value::Null);
    }

    #[cfg(unix)]
    #[test]
    fn driver_can_omit_ansi_when_a_client_only_needs_the_screen() {
        let requests = concat!(
            r#"{"id":1,"method":"launch","sessionId":"app","params":{"command":["sh","-c","printf ready; sleep 1"]}}"#,
            "\n",
            r#"{"id":2,"method":"waitForText","sessionId":"app","params":{"text":"ready","timeoutMs":2000}}"#,
            "\n",
            r#"{"id":3,"method":"capture","sessionId":"app","params":{"settleMs":10,"deadlineMs":2000,"includeAnsi":false}}"#,
            "\n",
            r#"{"id":4,"method":"shutdown"}"#,
            "\n"
        );
        let mut output = Vec::new();

        serve(
            BufReader::new(Cursor::new(requests.as_bytes())),
            &mut output,
        )
        .unwrap();

        let messages = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(messages[3]["result"]["shot"]["text"], "ready");
        assert_eq!(messages[3]["result"]["shot"]["ansi"], json!([]));
    }

    #[cfg(unix)]
    #[test]
    fn driver_launch_can_clear_and_supply_environment() {
        unsafe { std::env::set_var("TERMCTRL_PARENT_ONLY", "leak") };
        let requests = concat!(
            r#"{"id":1,"method":"launch","sessionId":"app","params":{"command":["/bin/sh","-c","printf '%s:%s:%s' \"${TERMCTRL_PARENT_ONLY-unset}\" \"$VISIBLE\" \"${TERMCTRL_SEMANTIC_SOCKET-unset}\""],"inheritEnv":false,"env":{"VISIBLE":"set"}}}"#,
            "\n",
            r#"{"id":2,"method":"capture","sessionId":"app","params":{"settleMs":10,"deadlineMs":2000}}"#,
            "\n",
            r#"{"id":3,"method":"shutdown"}"#,
            "\n"
        );
        let mut output = Vec::new();
        serve(
            BufReader::new(Cursor::new(requests.as_bytes())),
            &mut output,
        )
        .unwrap();
        unsafe { std::env::remove_var("TERMCTRL_PARENT_ONLY") };

        let messages = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(messages[2]["result"]["shot"]["text"], "unset:set:unset");
    }

    #[cfg(unix)]
    #[test]
    fn driver_waits_for_exit_and_renders_svg_evidence() {
        let requests = concat!(
            r#"{"id":1,"method":"launch","sessionId":"app","params":{"command":["sh","-c","printf done; exit 4"]}}"#,
            "\n",
            r#"{"id":2,"method":"waitForExit","sessionId":"app","params":{"timeoutMs":2000}}"#,
            "\n",
            r#"{"id":3,"method":"capture","sessionId":"app","params":{"includeSvg":true}}"#,
            "\n",
            r#"{"id":4,"method":"shutdown"}"#,
            "\n"
        );
        let mut output = Vec::new();
        serve(
            BufReader::new(Cursor::new(requests.as_bytes())),
            &mut output,
        )
        .unwrap();
        let messages = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(messages[2]["result"]["reason"], "exited");
        assert_eq!(messages[2]["result"]["exit"]["code"], 4);
        assert!(
            messages[3]["result"]["shot"]["svg"]
                .as_str()
                .unwrap()
                .starts_with("<svg")
        );
    }

    #[test]
    fn rejects_unsupported_control_input_in_protocol() {
        assert!(control_bytes("meta").is_err());
        assert!(control_bytes("1").is_err());
        assert_eq!(control_bytes("C").unwrap(), b"\x03");
    }

    #[test]
    fn invalid_typed_request_preserves_its_id() {
        let requests = "{\"id\":41,\"method\":\"noSuchMethod\",\"sessionId\":\"app\"}\n";
        let mut output = Vec::new();
        serve(
            BufReader::new(Cursor::new(requests.as_bytes())),
            &mut output,
        )
        .unwrap();
        let messages = String::from_utf8(output).unwrap();
        let error = serde_json::from_str::<Value>(messages.lines().nth(1).unwrap()).unwrap();

        assert_eq!(error["id"], 41);
    }

    #[cfg(unix)]
    #[test]
    fn idle_driver_continues_pumping_verbose_sessions() {
        let marker =
            std::env::temp_dir().join(format!("termctrl-driver-pump-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let (mut requests, input) = UnixStream::pair().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = SharedWriter(Arc::clone(&output));
        let marker_command = format!(
            "i=0; while [ $i -lt 100 ]; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; i=$((i+1)); done; : > '{}'; sleep 1",
            marker.display()
        );
        let launch = serde_json::to_string(&json!({
            "id": 1,
            "method": "launch",
            "sessionId": "noisy",
            "params": { "command": ["sh", "-c", marker_command] }
        }))
        .unwrap();
        let handle = thread::spawn(move || serve(BufReader::new(input), writer).unwrap());
        writeln!(requests, "{launch}").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        writeln!(requests, "{}", json!({"id":2,"method":"shutdown"})).unwrap();
        drop(requests);
        handle.join().unwrap();

        assert!(marker.exists(), "driver did not pump output while idle");
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[test]
    fn blocking_request_does_not_stop_other_session_output() {
        let marker = std::env::temp_dir().join(format!(
            "termctrl-driver-concurrent-pump-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let marker_command = format!(
            "i=0; while [ $i -lt 100 ]; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; i=$((i+1)); done; : > '{}'; sleep 1",
            marker.display()
        );
        let requests = [
            json!({
                "id": 1,
                "method": "launch",
                "sessionId": "noisy",
                "params": { "command": ["sh", "-c", marker_command] }
            }),
            json!({
                "id": 2,
                "method": "launch",
                "sessionId": "waiting",
                "params": { "command": ["sh", "-c", "sleep 1"] }
            }),
            json!({
                "id": 3,
                "method": "waitForText",
                "sessionId": "waiting",
                "params": { "text": "never", "timeoutMs": 500 }
            }),
            json!({"id": 4, "method": "shutdown"}),
        ]
        .into_iter()
        .map(|request| format!("{request}\n"))
        .collect::<String>();
        let mut output = Vec::new();

        serve(
            BufReader::new(Cursor::new(requests.into_bytes())),
            &mut output,
        )
        .unwrap();

        assert!(
            marker.exists(),
            "blocking one session stopped another session's PTY reader"
        );
        let _ = std::fs::remove_file(marker);
    }
}
