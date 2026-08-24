//! MCP tools for discovering and controlling named Terminal Control sessions.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use rmcp::{
    ServiceExt,
    handler::server::wrapper::{Json, Parameters},
    schemars, tool, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};

use crate::{render, session};

#[derive(Debug, Clone)]
pub struct TerminalControl;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionName {
    #[schemars(description = "Named Terminal Control session")]
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ScreenRequest {
    name: String,
    #[serde(default)]
    #[schemars(
        description = "Optional quiet period before returning; omit for an immediate snapshot"
    )]
    settle_ms: u64,
    #[serde(default)]
    #[schemars(description = "Maximum optional settling wait; omit for an immediate snapshot")]
    deadline_ms: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SaveScreenRequest {
    name: String,
    #[schemars(description = "PNG output path; parent directories are created")]
    path: PathBuf,
    #[serde(default = "default_pixel_ratio")]
    #[schemars(default = "default_pixel_ratio")]
    pixel_ratio: f32,
}

fn default_pixel_ratio() -> f32 {
    2.0
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SendRequest {
    name: String,
    #[schemars(description = "Ordered typed terminal input")]
    input: Vec<Input>,
    #[serde(default)]
    pace_ms: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct InteractRequest {
    name: String,
    #[schemars(description = "Ordered typed terminal input")]
    input: Vec<Input>,
    #[serde(default)]
    pace_ms: u64,
    #[serde(default)]
    #[schemars(description = "Optional visible text to await after sending input")]
    wait_for: Option<String>,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    #[schemars(
        description = "Optional quiet period before returning; omit for an immediate snapshot"
    )]
    settle_ms: u64,
    #[serde(default)]
    #[schemars(description = "Maximum optional settling wait; omit for an immediate snapshot")]
    deadline_ms: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Input {
    Text { text: String },
    Key { key: Key },
    Control { letter: char },
    Bytes { bytes: Vec<u8> },
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ResizeRequest {
    name: String,
    cols: u16,
    rows: u16,
    #[serde(default)]
    cell_width: Option<u16>,
    #[serde(default)]
    cell_height: Option<u16>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SessionSummary {
    name: String,
    state: Option<String>,
    command: Option<Vec<String>>,
    cwd: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    recording: Option<bool>,
    error: Option<String>,
    unavailable: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct SessionList {
    sessions: Vec<SessionSummary>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ProcessExit {
    code: u32,
    signal: Option<String>,
    success: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SessionDetails {
    name: String,
    state: String,
    exit: Option<ProcessExit>,
    command: Vec<String>,
    cwd: String,
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
    idle_for_ms: Option<u64>,
    has_visible_content: bool,
    recording: bool,
    recording_path: Option<String>,
    logs_truncated: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Screen {
    name: String,
    text: String,
    state: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ScreenArtifact {
    name: String,
    path: String,
    cols: u16,
    rows: u16,
}

#[tool_router(server_handler)]
impl TerminalControl {
    #[tool(description = "List running local Terminal Control sessions")]
    async fn list_sessions(&self) -> Result<Json<SessionList>, String> {
        blocking(|| {
            let sessions = session::list().map_err(format_error)?;
            Ok(Json(SessionList {
                sessions: sessions
                    .into_iter()
                    .filter(|entry| {
                        entry
                            .status
                            .as_ref()
                            .is_some_and(|status| status.state == session::SessionState::Running)
                    })
                    .map(|entry| {
                        let status = entry.status;
                        SessionSummary {
                            name: entry.name,
                            state: status.as_ref().map(|value| state(value.state).to_owned()),
                            command: status.as_ref().map(|value| value.launch.command.clone()),
                            cwd: status.as_ref().map(|value| path_string(&value.launch.cwd)),
                            cols: status.as_ref().map(|value| value.cols),
                            rows: status.as_ref().map(|value| value.rows),
                            recording: status.as_ref().map(|value| value.recording),
                            error: entry.error,
                            unavailable: entry.unavailable.map(|reason| format!("{reason:?}")),
                        }
                    })
                    .collect(),
            }))
        })
        .await
    }

    #[tool(description = "Get structured status and launch details for a named terminal session")]
    async fn get_session_status(
        &self,
        Parameters(SessionName { name }): Parameters<SessionName>,
    ) -> Result<Json<SessionDetails>, String> {
        blocking(move || {
            let status = session::status(&name).map_err(format_error)?;
            Ok(Json(session_details(name, status)))
        })
        .await
    }

    #[tool(description = "Read the current visible screen of a named terminal session")]
    async fn get_screen(
        &self,
        Parameters(request): Parameters<ScreenRequest>,
    ) -> Result<Json<Screen>, String> {
        blocking(move || capture(request)).await.map(Json)
    }

    #[tool(description = "Save a PNG screenshot of a named terminal session")]
    async fn save_screen(
        &self,
        Parameters(request): Parameters<SaveScreenRequest>,
    ) -> Result<Json<ScreenArtifact>, String> {
        blocking(move || save_screen(request)).await.map(Json)
    }

    #[tool(
        description = "Send typed text, keys, controls, or exact bytes to a named terminal session"
    )]
    async fn send_input(
        &self,
        Parameters(request): Parameters<SendRequest>,
    ) -> Result<String, String> {
        blocking(move || {
            session::send(
                &request.name,
                encode_input(request.input)?,
                Duration::from_millis(request.pace_ms),
            )
            .map_err(format_error)?;
            Ok(format!("sent input to {}", request.name))
        })
        .await
    }

    #[tool(
        description = "Send input, optionally wait for visible text, and return the resulting screen"
    )]
    async fn interact(
        &self,
        Parameters(request): Parameters<InteractRequest>,
    ) -> Result<Json<Screen>, String> {
        blocking(move || {
            session::send(
                &request.name,
                encode_input(request.input)?,
                Duration::from_millis(request.pace_ms),
            )
            .map_err(format_error)?;
            if let Some(text) = request.wait_for {
                session::wait(
                    &request.name,
                    text,
                    Duration::from_millis(request.timeout_ms),
                )
                .map_err(format_error)?;
            }
            capture(ScreenRequest {
                name: request.name,
                settle_ms: request.settle_ms,
                deadline_ms: request.deadline_ms,
            })
        })
        .await
        .map(Json)
    }

    #[tool(description = "Resize a named terminal session")]
    async fn resize_session(
        &self,
        Parameters(request): Parameters<ResizeRequest>,
    ) -> Result<String, String> {
        blocking(move || {
            session::resize(
                &request.name,
                request.cols,
                request.rows,
                request.cell_width,
                request.cell_height,
            )
            .map_err(format_error)?;
            Ok(format!(
                "resized {} to {}x{}",
                request.name, request.cols, request.rows
            ))
        })
        .await
    }

    #[tool(description = "Stop a named terminal session and its child process")]
    async fn stop_session(
        &self,
        Parameters(SessionName { name }): Parameters<SessionName>,
    ) -> Result<String, String> {
        blocking(move || {
            session::stop(&name).map_err(format_error)?;
            Ok(format!("stopped {name}"))
        })
        .await
    }
}

pub async fn serve() -> anyhow::Result<()> {
    TerminalControl.serve(stdio()).await?.waiting().await?;
    Ok(())
}

fn capture(request: ScreenRequest) -> Result<Screen, String> {
    let (shot, status) = session::show_with_status(
        &request.name,
        Duration::from_millis(request.settle_ms),
        Duration::from_millis(request.deadline_ms),
    )
    .map_err(format_error)?;
    Ok(Screen {
        name: request.name,
        text: shot.frame.text(),
        state: state(status.state).to_owned(),
        cols: status.cols,
        rows: status.rows,
    })
}

fn save_screen(request: SaveScreenRequest) -> Result<ScreenArtifact, String> {
    if !request.pixel_ratio.is_finite() || request.pixel_ratio <= 0.0 {
        return Err("pixelRatio must be greater than zero".to_owned());
    }
    let shot =
        session::show(&request.name, Duration::ZERO, Duration::ZERO).map_err(format_error)?;
    let path = request.path;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let svg = render::svg(&shot.frame, &render::Options::default());
    render::png(&svg, &path, request.pixel_ratio).map_err(format_error)?;
    Ok(ScreenArtifact {
        name: request.name,
        path: path.to_string_lossy().into_owned(),
        cols: shot.frame.cols,
        rows: shot.frame.rows,
    })
}

fn session_details(name: String, status: session::SessionStatus) -> SessionDetails {
    SessionDetails {
        name,
        state: state(status.state).to_owned(),
        exit: status.exit.map(|exit| ProcessExit {
            code: exit.code,
            signal: exit.signal,
            success: exit.success,
        }),
        command: status.launch.command,
        cwd: path_string(&status.launch.cwd),
        cols: status.cols,
        rows: status.rows,
        cell_width: status.cell_width,
        cell_height: status.cell_height,
        idle_for_ms: status.idle_for_ms,
        has_visible_content: status.has_visible_content,
        recording: status.recording,
        recording_path: status.launch.record.as_deref().map(path_string),
        logs_truncated: status.logs_truncated,
    }
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| format!("terminal operation task failed: {error}"))?
}

fn encode_input(input: Vec<Input>) -> Result<Vec<Vec<u8>>, String> {
    input.into_iter().map(encode_atom).collect()
}

fn encode_atom(input: Input) -> Result<Vec<u8>, String> {
    Ok(match input {
        Input::Text { text } => text.into_bytes(),
        Input::Key { key } => key_bytes(key).to_vec(),
        Input::Control { letter } if letter.is_ascii_alphabetic() => {
            vec![(letter.to_ascii_uppercase() as u8) - b'@']
        }
        Input::Control { .. } => return Err("control input must be one ASCII letter".to_owned()),
        Input::Bytes { bytes } => bytes,
    })
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

fn state(value: session::SessionState) -> &'static str {
    match value {
        session::SessionState::Running => "running",
        session::SessionState::Exited => "exited",
    }
}

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

const fn default_timeout_ms() -> u64 {
    5_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_typed_input_without_shell_escaping() {
        let input = encode_input(vec![
            Input::Text {
                text: ":edit src/App.tsx".to_owned(),
            },
            Input::Key { key: Key::Enter },
            Input::Control { letter: 'c' },
        ])
        .unwrap();

        assert_eq!(
            input,
            [b":edit src/App.tsx".to_vec(), b"\r".to_vec(), vec![3]]
        );
    }

    #[test]
    fn screen_and_interact_requests_default_to_an_immediate_snapshot() {
        let screen: ScreenRequest = serde_json::from_value(serde_json::json!({
            "name": "editor"
        }))
        .unwrap();
        let interact: InteractRequest = serde_json::from_value(serde_json::json!({
            "name": "editor",
            "input": []
        }))
        .unwrap();

        assert_eq!(screen.settle_ms, 0);
        assert_eq!(screen.deadline_ms, 0);
        assert_eq!(interact.settle_ms, 0);
        assert_eq!(interact.deadline_ms, 0);
    }

    #[test]
    fn publishes_object_shaped_tool_contracts() {
        let tools = TerminalControl::tool_router().list_all();
        let names = tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "get_screen",
                "get_session_status",
                "interact",
                "list_sessions",
                "resize_session",
                "save_screen",
                "send_input",
                "stop_session",
            ]
        );
        assert!(tools.iter().all(|tool| {
            tool.output_schema.as_ref().is_none_or(|schema| {
                schema.get("type").and_then(|value| value.as_str()) == Some("object")
            })
        }));
    }

    #[test]
    fn publishes_agent_discovery_fields_in_tool_schemas() {
        let tools = TerminalControl::tool_router().list_all();
        let list = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "list_sessions")
            .unwrap();
        let status = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "get_session_status")
            .unwrap();
        let list_schema = serde_json::to_value(list.output_schema.as_ref().unwrap()).unwrap();
        let status_schema = serde_json::to_value(status.output_schema.as_ref().unwrap()).unwrap();

        let summary_properties = &list_schema["$defs"]["SessionSummary"]["properties"];
        assert!(summary_properties.get("command").is_some());
        assert!(summary_properties.get("cwd").is_some());

        let properties = &status_schema["properties"];
        for field in [
            "name",
            "state",
            "exit",
            "command",
            "cwd",
            "cols",
            "rows",
            "cellWidth",
            "cellHeight",
            "idleForMs",
            "hasVisibleContent",
            "recording",
            "recordingPath",
            "logsTruncated",
        ] {
            assert!(properties.get(field).is_some(), "missing {field}");
        }
    }

    #[test]
    fn publishes_settling_as_optional_and_immediate_by_default() {
        let tools = TerminalControl::tool_router().list_all();

        for name in ["get_screen", "interact"] {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap();
            let schema = serde_json::to_value(&tool.input_schema).unwrap();
            let required = schema["required"].as_array().unwrap();

            for field in ["settleMs", "deadlineMs"] {
                let property = &schema["properties"][field];
                assert_eq!(property["default"], 0);
                assert!(property["description"].as_str().unwrap().contains("omit"));
                assert!(!required.iter().any(|value| value == field));
            }
        }
    }

    #[test]
    fn maps_full_session_status_to_the_mcp_contract() {
        let details = session_details(
            "editor".to_owned(),
            session::SessionStatus {
                state: session::SessionState::Exited,
                exit: Some(session::ProcessExit {
                    code: 7,
                    signal: Some("SIGTERM".to_owned()),
                    success: false,
                }),
                cols: 120,
                rows: 40,
                cell_width: 9,
                cell_height: 18,
                idle_for_ms: Some(250),
                has_visible_content: true,
                recording: true,
                logs_truncated: true,
                launch: session::SessionLaunch {
                    command: vec!["nvim".to_owned(), "README.md".to_owned()],
                    cwd: "/tmp/project".into(),
                    record: Some("/tmp/editor.termctrl".into()),
                    cols: 80,
                    rows: 24,
                    cell_width: 8,
                    cell_height: 16,
                    max_bytes: 1024,
                    opentui_host: false,
                    color: crate::shot::ColorMode::Auto,
                },
            },
        );
        let value = serde_json::to_value(details).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "name": "editor",
                "state": "exited",
                "exit": { "code": 7, "signal": "SIGTERM", "success": false },
                "command": ["nvim", "README.md"],
                "cwd": "/tmp/project",
                "cols": 120,
                "rows": 40,
                "cellWidth": 9,
                "cellHeight": 18,
                "idleForMs": 250,
                "hasVisibleContent": true,
                "recording": true,
                "recordingPath": "/tmp/editor.termctrl",
                "logsTruncated": true,
            })
        );
    }
}
