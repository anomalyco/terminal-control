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
        description = "Optional stable workspace window name; cannot be combined with pane"
    )]
    window: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional workspace pane id; omit for the composed workspace")]
    pane: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SaveScreenRequest {
    name: String,
    #[serde(default)]
    #[schemars(
        description = "Optional stable workspace window name; cannot be combined with pane"
    )]
    window: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional workspace pane id; omit for the composed workspace")]
    pane: Option<u32>,
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
    #[serde(default)]
    #[schemars(
        description = "Optional stable workspace window name; cannot be combined with pane"
    )]
    window: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional workspace pane id; omit for the active pane")]
    pane: Option<u32>,
    #[schemars(description = "Ordered typed terminal input")]
    input: Vec<Input>,
    #[serde(default)]
    pace_ms: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct InteractRequest {
    name: String,
    #[serde(default)]
    #[schemars(
        description = "Optional stable workspace window name; cannot be combined with pane"
    )]
    window: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional workspace pane id; omit to target the active pane and return the composed workspace"
    )]
    pane: Option<u32>,
    #[schemars(description = "Ordered typed terminal input")]
    input: Vec<Input>,
    #[serde(default)]
    pace_ms: u64,
    #[serde(default)]
    #[schemars(description = "Optional visible text to await after sending input")]
    wait_for: Option<String>,
    #[serde(default = "default_timeout_ms")]
    #[schemars(
        description = "Maximum wait for waitFor text. Defaults to 5000; omit unless intentionally overriding, and never send an explicit 5000"
    )]
    timeout_ms: u64,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct LayoutRequest {
    #[schemars(description = "Named Terminal Control workspace")]
    name: String,
    #[serde(default)]
    #[schemars(
        description = "Optional stable workspace window name; omit for the selected window"
    )]
    window: Option<String>,
    #[schemars(description = "Grid columns, either 1 or 2")]
    columns: u16,
    #[schemars(description = "Grid rows, either 1 or 2")]
    rows: u16,
    #[serde(default)]
    #[schemars(
        description = "Optional argv for the first pane created while growing the layout; omit to start the workspace shell"
    )]
    command: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PaneRequest {
    #[schemars(description = "Named Terminal Control workspace")]
    name: String,
    #[schemars(description = "Stable workspace pane id")]
    pane: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct WindowScopeRequest {
    name: String,
    #[serde(default)]
    #[schemars(
        description = "Optional stable workspace window name; omit for the selected window"
    )]
    window: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct WindowRequest {
    #[schemars(description = "Named Terminal Control workspace")]
    name: String,
    #[schemars(description = "Exact stable workspace window name")]
    window: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ContextRequest {
    #[schemars(description = "Named Terminal Control workspace")]
    name: String,
    #[serde(default)]
    #[schemars(description = "Optional stable pane id; omit for the selected pane")]
    pane: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum TabPosition {
    Top,
    Bottom,
}

impl From<TabPosition> for session::TabPosition {
    fn from(position: TabPosition) -> Self {
        match position {
            TabPosition::Top => Self::Top,
            TabPosition::Bottom => Self::Bottom,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct TabPositionRequest {
    name: String,
    position: TabPosition,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct MoveWindowRequest {
    name: String,
    window: String,
    #[schemars(description = "Final zero-based tab index")]
    index: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CreateWindowRequest {
    #[schemars(description = "Named Terminal Control workspace")]
    name: String,
    #[serde(default)]
    #[schemars(description = "Optional unique window name; defaults to window-N")]
    window: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional command for the first pane; defaults to $SHELL")]
    command: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Optional working directory; defaults to the workspace directory")]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RenameWindowRequest {
    name: String,
    window: String,
    new_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct MovePaneRequest {
    name: String,
    pane: u32,
    window: String,
    #[serde(default)]
    #[schemars(description = "Stack below the target pane instead of splitting to its right")]
    vertical: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum PaneResizeDirection {
    Left,
    Right,
    Up,
    Down,
}

impl From<PaneResizeDirection> for session::PaneDirection {
    fn from(direction: PaneResizeDirection) -> Self {
        match direction {
            PaneResizeDirection::Left => Self::Left,
            PaneResizeDirection::Right => Self::Right,
            PaneResizeDirection::Up => Self::Up,
            PaneResizeDirection::Down => Self::Down,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ResizePaneRequest {
    name: String,
    pane: u32,
    direction: PaneResizeDirection,
    #[serde(default = "default_resize_cells")]
    #[schemars(default = "default_resize_cells")]
    cells: u16,
}

fn default_resize_cells() -> u16 {
    1
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
struct PaneSummary {
    id: u32,
    active: bool,
    visible: bool,
    state: String,
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
    title: String,
    command: Vec<String>,
    cwd: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct PaneList {
    panes: Vec<PaneSummary>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct WindowSummary {
    index: usize,
    name: String,
    active: bool,
    pane_count: usize,
    active_pane: Option<u32>,
    zoomed_pane: Option<u32>,
    activity: bool,
    activity_kinds: Vec<String>,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct WindowList {
    windows: Vec<WindowSummary>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct WorkspaceContext {
    session: String,
    workspace: String,
    window_id: u32,
    window: String,
    window_index: usize,
    pane: u32,
    window_active: bool,
    pane_active: bool,
    tab_position: String,
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

    #[tool(description = "List stable panes in a named Terminal Control workspace")]
    async fn list_panes(
        &self,
        Parameters(request): Parameters<WindowScopeRequest>,
    ) -> Result<Json<PaneList>, String> {
        blocking(move || {
            let panes =
                session::panes_in_window(&request.name, request.window).map_err(format_error)?;
            Ok(Json(pane_list(panes)))
        })
        .await
    }

    #[tool(description = "List stable named windows in a Terminal Control workspace")]
    async fn list_windows(
        &self,
        Parameters(SessionName { name }): Parameters<SessionName>,
    ) -> Result<Json<WindowList>, String> {
        blocking(move || {
            session::windows(&name)
                .map(window_list)
                .map(Json)
                .map_err(format_error)
        })
        .await
    }

    #[tool(
        description = "Resolve a workspace pane to its authoritative current workspace, window, pane, and tab placement"
    )]
    async fn get_workspace_context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> Result<Json<WorkspaceContext>, String> {
        blocking(move || {
            session::workspace_context(&request.name, request.pane)
                .map(workspace_context)
                .map(Json)
                .map_err(format_error)
        })
        .await
    }

    #[tool(description = "Move a live workspace tab strip to the top or bottom")]
    async fn set_workspace_tab_position(
        &self,
        Parameters(request): Parameters<TabPositionRequest>,
    ) -> Result<Json<WindowList>, String> {
        blocking(move || {
            session::set_workspace_tab_position(&request.name, request.position.into())
                .map(window_list)
                .map(Json)
                .map_err(format_error)
        })
        .await
    }

    #[tool(description = "Reorder one workspace window by final zero-based tab index")]
    async fn move_workspace_window(
        &self,
        Parameters(request): Parameters<MoveWindowRequest>,
    ) -> Result<Json<WindowList>, String> {
        blocking(move || {
            session::move_workspace_window(&request.name, request.window, request.index)
                .map(window_list)
                .map(Json)
                .map_err(format_error)
        })
        .await
    }

    #[tool(description = "Create and select a workspace window with one shell or command pane")]
    async fn create_workspace_window(
        &self,
        Parameters(request): Parameters<CreateWindowRequest>,
    ) -> Result<Json<WindowList>, String> {
        blocking(move || {
            session::create_workspace_window(
                &request.name,
                request.window,
                request.command,
                request.cwd.map(Into::into),
            )
            .map(window_list)
            .map(Json)
            .map_err(format_error)
        })
        .await
    }

    #[tool(description = "Select one named workspace window for the attached terminal")]
    async fn select_workspace_window(
        &self,
        Parameters(request): Parameters<WindowRequest>,
    ) -> Result<Json<WindowList>, String> {
        blocking(move || {
            session::select_workspace_window(&request.name, request.window)
                .map(window_list)
                .map(Json)
                .map_err(format_error)
        })
        .await
    }

    #[tool(description = "Rename one stable workspace window without changing its panes")]
    async fn rename_workspace_window(
        &self,
        Parameters(request): Parameters<RenameWindowRequest>,
    ) -> Result<Json<WindowList>, String> {
        blocking(move || {
            session::rename_workspace_window(&request.name, request.window, request.new_name)
                .map(window_list)
                .map(Json)
                .map_err(format_error)
        })
        .await
    }

    #[tool(
        description = "Terminate one named workspace window and all its panes. Closing the final window ends the workspace"
    )]
    async fn close_workspace_window(
        &self,
        Parameters(request): Parameters<WindowRequest>,
    ) -> Result<Json<WindowList>, String> {
        blocking(move || {
            session::close_workspace_window(&request.name, request.window)
                .map(window_list)
                .map(Json)
                .map_err(format_error)
        })
        .await
    }

    #[tool(
        description = "Move one running pane into another named window without restarting its process"
    )]
    async fn move_workspace_pane(
        &self,
        Parameters(request): Parameters<MovePaneRequest>,
    ) -> Result<Json<WindowList>, String> {
        blocking(move || {
            session::move_workspace_pane(
                &request.name,
                request.pane,
                request.window,
                request.vertical,
            )
            .map(window_list)
            .map(Json)
            .map_err(format_error)
        })
        .await
    }

    #[tool(description = "Grow one workspace pane toward a neighboring boundary")]
    async fn resize_workspace_pane(
        &self,
        Parameters(request): Parameters<ResizePaneRequest>,
    ) -> Result<Json<PaneList>, String> {
        blocking(move || {
            session::resize_workspace_pane(
                &request.name,
                request.pane,
                request.direction.into(),
                request.cells,
            )
            .map(pane_list)
            .map(Json)
            .map_err(format_error)
        })
        .await
    }

    #[tool(description = "Toggle one workspace pane between split and full-window presentation")]
    async fn toggle_workspace_zoom(
        &self,
        Parameters(request): Parameters<PaneRequest>,
    ) -> Result<Json<PaneList>, String> {
        blocking(move || {
            session::toggle_workspace_zoom(&request.name, request.pane)
                .map(pane_list)
                .map(Json)
                .map_err(format_error)
        })
        .await
    }

    #[tool(
        description = "Set a named workspace to a 1x1, 2x1, 1x2, or 2x2 grid. Missing cells open shells; surplus panes must be closed explicitly"
    )]
    async fn set_workspace_layout(
        &self,
        Parameters(request): Parameters<LayoutRequest>,
    ) -> Result<Json<PaneList>, String> {
        blocking(move || {
            session::set_workspace_layout_in_window(
                &request.name,
                request.window,
                request.columns,
                request.rows,
                request.command,
            )
            .map(pane_list)
            .map(Json)
            .map_err(format_error)
        })
        .await
    }

    #[tool(description = "Intentionally move human focus to one stable workspace pane id")]
    async fn focus_workspace_pane(
        &self,
        Parameters(request): Parameters<PaneRequest>,
    ) -> Result<Json<PaneList>, String> {
        blocking(move || {
            session::focus_workspace_pane(&request.name, request.pane)
                .map(pane_list)
                .map(Json)
                .map_err(format_error)
        })
        .await
    }

    #[tool(
        description = "Terminate exactly one stable workspace pane id. Closing the final pane ends the workspace"
    )]
    async fn close_workspace_pane(
        &self,
        Parameters(request): Parameters<PaneRequest>,
    ) -> Result<Json<PaneList>, String> {
        blocking(move || {
            session::close_workspace_pane(&request.name, request.pane)
                .map(pane_list)
                .map(Json)
                .map_err(format_error)
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

    #[tool(
        description = "Save a PNG screenshot of a composed workspace, named window, or stable pane"
    )]
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
            let target =
                session::terminal_target(request.window, request.pane).map_err(format_error)?;
            let input = encode_input(request.input)?;
            session::send_to_target(
                &request.name,
                target,
                input,
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
            let target =
                session::terminal_target(request.window, request.pane).map_err(format_error)?;
            let input = encode_input(request.input)?;
            session::send_to_target(
                &request.name,
                target.clone(),
                input,
                Duration::from_millis(request.pace_ms),
            )
            .map_err(format_error)?;
            if let Some(text) = request.wait_for {
                session::wait_for_target(
                    &request.name,
                    target.clone(),
                    text,
                    Duration::from_millis(request.timeout_ms),
                )
                .map_err(format_error)?;
            }
            capture_target(request.name, target)
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Resize a named detached session; workspaces follow their current human attachment"
    )]
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
    let target = session::terminal_target(request.window, request.pane).map_err(format_error)?;
    capture_target(request.name, target)
}

fn save_screen(request: SaveScreenRequest) -> Result<ScreenArtifact, String> {
    if !request.pixel_ratio.is_finite() || request.pixel_ratio <= 0.0 {
        return Err("pixelRatio must be greater than zero".to_owned());
    }
    let target = session::terminal_target(request.window, request.pane).map_err(format_error)?;
    let shot = session::show_target(&request.name, target, Duration::ZERO, Duration::ZERO)
        .map_err(format_error)?;
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

fn capture_target(name: String, target: session::TerminalTarget) -> Result<Screen, String> {
    let (shot, status) =
        session::show_target_with_status(&name, target, Duration::ZERO, Duration::ZERO)
            .map_err(format_error)?;
    Ok(Screen {
        name,
        text: shot.frame.text(),
        state: state(status.state).to_owned(),
        cols: shot.frame.cols,
        rows: shot.frame.rows,
    })
}

fn pane_list(panes: Vec<session::PaneStatus>) -> PaneList {
    PaneList {
        panes: panes
            .into_iter()
            .map(|pane| PaneSummary {
                id: pane.id,
                active: pane.active,
                visible: pane.visible,
                state: state(pane.state).to_owned(),
                x: pane.x,
                y: pane.y,
                cols: pane.cols,
                rows: pane.rows,
                title: pane.title,
                command: pane.command,
                cwd: path_string(&pane.cwd),
            })
            .collect(),
    }
}

fn window_list(windows: Vec<session::WindowStatus>) -> WindowList {
    WindowList {
        windows: windows
            .into_iter()
            .map(|window| WindowSummary {
                index: window.index,
                name: window.name,
                active: window.active,
                pane_count: window.pane_count,
                active_pane: window.active_pane,
                zoomed_pane: window.zoomed_pane,
                activity: window.activity,
                activity_kinds: window
                    .activity_kinds
                    .into_iter()
                    .map(|kind| kind.as_str().to_owned())
                    .collect(),
                cols: window.cols,
                rows: window.rows,
            })
            .collect(),
    }
}

fn workspace_context(context: session::WorkspaceContext) -> WorkspaceContext {
    WorkspaceContext {
        session: context.session,
        workspace: context.workspace,
        window_id: context.window_id,
        window: context.window,
        window_index: context.window_index,
        pane: context.pane,
        window_active: context.window_active,
        pane_active: context.pane_active,
        tab_position: context.tab_position.as_str().to_owned(),
    }
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
    fn screen_and_interact_requests_use_explicit_targets() {
        let screen: ScreenRequest = serde_json::from_value(serde_json::json!({
            "name": "editor"
        }))
        .unwrap();
        let interact: InteractRequest = serde_json::from_value(serde_json::json!({
            "name": "editor",
            "window": "tests",
            "input": []
        }))
        .unwrap();

        assert!(screen.window.is_none());
        assert_eq!(interact.window.as_deref(), Some("tests"));
        assert!(session::terminal_target(Some("tests".to_owned()), Some(3)).is_err());

        let save: SaveScreenRequest = serde_json::from_value(serde_json::json!({
            "name": "editor",
            "pane": 3,
            "path": "/tmp/editor-pane.png"
        }))
        .unwrap();
        assert_eq!(save.pane, Some(3));
        assert_eq!(save.pixel_ratio, 2.0);
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
                "close_workspace_pane",
                "close_workspace_window",
                "create_workspace_window",
                "focus_workspace_pane",
                "get_screen",
                "get_session_status",
                "get_workspace_context",
                "interact",
                "list_panes",
                "list_sessions",
                "list_windows",
                "move_workspace_pane",
                "move_workspace_window",
                "rename_workspace_window",
                "resize_session",
                "resize_workspace_pane",
                "save_screen",
                "select_workspace_window",
                "send_input",
                "set_workspace_layout",
                "set_workspace_tab_position",
                "stop_session",
                "toggle_workspace_zoom",
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
        let panes = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "list_panes")
            .unwrap();
        let windows = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "list_windows")
            .unwrap();
        let context = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "get_workspace_context")
            .unwrap();
        let list_schema = serde_json::to_value(list.output_schema.as_ref().unwrap()).unwrap();
        let status_schema = serde_json::to_value(status.output_schema.as_ref().unwrap()).unwrap();
        let pane_schema = serde_json::to_value(panes.output_schema.as_ref().unwrap()).unwrap();
        let window_schema = serde_json::to_value(windows.output_schema.as_ref().unwrap()).unwrap();
        let context_schema = serde_json::to_value(context.output_schema.as_ref().unwrap()).unwrap();

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
        let properties = &pane_schema["$defs"]["PaneSummary"]["properties"];
        for field in [
            "id", "active", "visible", "state", "x", "y", "cols", "rows", "title", "command", "cwd",
        ] {
            assert!(properties.get(field).is_some(), "missing pane {field}");
        }
        let properties = &window_schema["$defs"]["WindowSummary"]["properties"];
        for field in [
            "index",
            "name",
            "active",
            "paneCount",
            "activePane",
            "zoomedPane",
            "activity",
            "activityKinds",
            "cols",
            "rows",
        ] {
            assert!(properties.get(field).is_some(), "missing window {field}");
        }
        let properties = &context_schema["properties"];
        for field in [
            "session",
            "workspace",
            "windowId",
            "window",
            "windowIndex",
            "pane",
            "windowActive",
            "paneActive",
            "tabPosition",
        ] {
            assert!(properties.get(field).is_some(), "missing context {field}");
        }
    }

    #[test]
    fn publishes_immediate_screen_reads_without_settling_controls() {
        let tools = TerminalControl::tool_router().list_all();

        for name in ["get_screen", "interact"] {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap();
            let schema = serde_json::to_value(&tool.input_schema).unwrap();
            for field in ["settleMs", "deadlineMs"] {
                assert!(schema["properties"].get(field).is_none());
            }
        }

        let save = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "save_screen")
            .unwrap();
        let schema = serde_json::to_value(&save.input_schema).unwrap();
        assert_eq!(schema["properties"]["pixelRatio"]["default"], 2.0);
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&"path".into())
        );

        let interact = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "interact")
            .unwrap();
        let schema = serde_json::to_value(&interact.input_schema).unwrap();
        let timeout = &schema["properties"]["timeoutMs"];
        assert_eq!(timeout["default"], 5_000);
        let description = timeout["description"].as_str().unwrap();
        assert!(description.contains("omit"));
        assert!(description.contains("never send an explicit 5000"));
        assert!(
            !schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "timeoutMs")
        );
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
