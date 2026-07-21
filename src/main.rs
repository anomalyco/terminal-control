use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use terminal_control::{driver, mcp, recording, render, session, shot as shot_engine};

const HELP: &str = "\
termctrl controls and captures terminal applications for agents and tests. Start a named live
application, read its visible screen with `show`, or retain selected artifacts with `save`.";

const ROOT_EXAMPLES: &str = "\
Examples:
  termctrl show -- my-terminal-app
  termctrl save --format png --out captures/app.png -- my-terminal-app
  termctrl start demo --host opentui -- opencode
  termctrl run
  termctrl attach workspace
  termctrl wait demo '/connect' && termctrl send demo text:/connect enter
  termctrl show demo
  termctrl save demo --format png --out captures/provider.png
  termctrl logs demo
  termctrl restart demo
  termctrl stop demo";

const SHOW_HELP: &str = "\
Show prints a settled visible terminal screen to standard output, as text by default.

Sources:
  termctrl show NAME                    Read a named live session.
  termctrl show -- COMMAND...           Run a disposable command in a PTY.
  termctrl show --pipe -- COMMAND...    Read piped stdout/stderr.
  termctrl show --input FILE            Read ANSI/VT bytes from FILE, or use - for stdin.
  termctrl show --recording FILE        Replay the final screen of a .termctrl recording.

Use --format json, --format ansi, or --format svg for another stdout-readable representation.
Use `--at-marker NAME` or `--at-ms MS` with --recording to inspect an exact moment. Use `save`
to write files.";

const SAVE_HELP: &str = "\
Save freezes a visible terminal screen and writes exactly the requested artifact formats.

Examples:
  termctrl save demo --format png --out captures/current.png
  termctrl save demo --format png --format txt --out captures/current
  termctrl save --input debug.ansi --format png --out captures/replay.png
  termctrl save --recording captures/demo.termctrl --at-marker done --format png --out captures/done.png
  termctrl save --format png --out captures/startup.png -- my-terminal-app";

const START_HELP: &str = "\
Start creates one background PTY session and returns once its local control socket is available.
The application stays alive until `termctrl stop NAME`, so later commands interact with the
same screen and application state. Persistent sessions currently require macOS or Linux. Session
sockets are local control endpoints protected for the current user; recordings contain terminal
output plus client and automatic host input, so treat them as sensitive artifacts.

Example:
  termctrl start demo --host opentui --cols 112 --rows 34 -- opencode
  termctrl status demo
  termctrl wait demo '/connect'
  termctrl send demo text:/connect enter
  termctrl resize demo --cols 132 --rows 38
  termctrl show demo
  termctrl save demo --format png --out captures/provider.png
  termctrl stop demo";

const RUN_HELP: &str = "\
Run creates or reattaches a visible Terminal Control workspace. With no arguments it uses the
default name `workspace`. Supply a command after -- to replace the first shell when creating, or
NAME to expose a stable explicit control socket. If NAME already exists, no command may be supplied.

Each workspace starts with a `main` window. Use ctrl-b p for the command palette, ctrl-b c/w to
create/list windows, ctrl-b l for the last window, ctrl-b n for next, and ctrl-b 0-9 to select by
index. Use ctrl-b </> or tab dragging to reorder, and ctrl-b t to move tabs. Use ctrl-b % to split
left/right, ctrl-b \" to split top/bottom, ctrl-b plus arrow keys or h/j/k to focus, ctrl-b H/J/K/L to resize, ctrl-b z to zoom,
ctrl-b q to show pane ids, ctrl-b d to detach, ctrl-b ? for help, and ctrl-b ctrl-b to send a literal ctrl-b. Destructive ctrl-b x (pane), ctrl-b
& (window), and ctrl-b Q (workspace) actions require y/n confirmation. Agents can target hidden
windows without changing human selection. Foreground workspaces inherit the outer terminal colors.

Examples:
  termctrl run
  termctrl new-window workspace editor -- nvim
  termctrl windows workspace
  termctrl current --json
  termctrl tab-position workspace top
  termctrl move-window workspace editor --index 0
  termctrl show workspace --window editor
  termctrl panes workspace
  termctrl layout workspace --grid 2x2
  termctrl send workspace --pane 1 text:opencode2 enter
  termctrl focus workspace --pane 1
  termctrl close-pane workspace --pane 1
  termctrl resize-pane workspace --pane 1 --direction left --cells 5
  termctrl zoom-pane workspace --pane 1
  termctrl show workspace --pane 1
  termctrl run editor --cwd ~/src/project -- nvim
  termctrl run -- /usr/bin/nvim";

const ATTACH_HELP: &str = "\
Attach connects the current terminal to an existing detached workspace. It adopts this terminal's
size and colors and repaints the complete workspace. A workspace accepts one human terminal at a
time; agent controls remain available independently.

Example:
  termctrl attach workspace";

const SEND_HELP: &str = "\
Send ordered input to a live session. Text uses `text:<value>`; named keys include `enter`,
`escape`, arrows, `tab`, `shift-tab`, `backspace`, `delete`, `home`, `end`, `page-up`, and
`page-down`. Use `ctrl-a` through `ctrl-z` for control input such as `ctrl-c` cancellation.
Add `--pace-ms 35` when producing a human-readable recording so typed text appears character by
character in the terminal instead of as one immediate paste. Use `--stdin` to send exact bytes
from standard input as one burst.

Examples:
  termctrl send demo ctrl-p text:model enter
  termctrl send demo ctrl-c
  printf '%s' 'a multiline prompt' | termctrl send demo --stdin
  termctrl send demo --pace-ms 35 'text:Write a terminal haiku.' enter";

const VIDEO_HELP: &str = "\
Replay a recording produced by `termctrl start --record` into a video artifact. Without `--edit`,
the video preserves observed timing. For a concise annotated demo, add named moments while recording
with `termctrl mark`, then pass an edit-plan JSON file with `--edit`. Each clip selects a marker range
and may set `speed`, optional visible `caption`, or optional `hold_ms`. Omit `hold_ms` for no artificial
pause between clips. Use `--tail-ms 0` if the final frame should not be held after the last clip.

`--fps` controls the maximum sampled frame rate; identical rendered screens are rasterized once and
reused. Pass `--include-startup` to retain blank startup or capability negotiation frames. The source
`.termctrl` file always retains the original timing, terminal bytes, client input, automatic host
input, and markers until the session is closed. Video export requires `ffmpeg` to be installed.
Pass `--footer` to add a bottom row with the clip caption, elapsed timecode, and TERMINAL CONTROL
branding; without it, edit-plan captions render as inline annotation rows.

Example:
  termctrl start demo --record captures/demo.termctrl -- opencode
  termctrl mark demo before-connect
  termctrl send demo text:/connect enter
  termctrl mark demo after-connect
  termctrl stop demo
  termctrl markers captures/demo.termctrl
  termctrl video captures/demo.termctrl --edit captures/demo.json --tail-ms 0 --out captures/demo.mp4";

const MARK_HELP: &str = "\
Add a named marker to the active `.termctrl` recording at the current session time. Markers do not
change the raw recording; they give later `show --recording --at-marker` and `video --edit` commands
stable names for important moments.

Example:
  termctrl start demo --record captures/demo.termctrl -- opencode
  termctrl wait demo \"Ask anything\"
  termctrl mark demo ready
  termctrl send demo text:/connect enter
  termctrl mark demo after-connect";

const MARKERS_HELP: &str = "\
List named markers from a .termctrl recording. Use the timestamps to audit an edit plan, or inspect
screens with `termctrl show --recording FILE --at-marker NAME` before exporting a demo video.";

const DRIVER_HELP: &str = "\
Driver mode serves isolated embedded sessions as newline-delimited JSON over standard input and
standard output. It is used by the `@kitlangton/terminal-control` package; standard output
contains protocol messages only. Driver sessions support isolated child environments, stable
captures, SVG evidence, recordings, resizing, and explicit exit waiting.

Example:
  termctrl driver";

#[derive(Parser)]
#[command(
    name = "termctrl",
    version,
    about = "Control and capture terminal applications",
    long_about = HELP,
    after_help = ROOT_EXAMPLES
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the visible screen of a session, command, or terminal stream.
    #[command(after_help = SHOW_HELP)]
    Show(ShowArgs),
    /// Save selected artifact formats from a session, command, or terminal stream.
    #[command(after_help = SAVE_HELP)]
    Save(SaveArgs),
    /// Start a named persistent terminal application.
    #[command(after_help = START_HELP)]
    Start(StartArgs),
    /// Enter a visible, agent-controllable terminal workspace.
    #[command(after_help = RUN_HELP)]
    Run(RunArgs),
    /// Attach this terminal to an existing workspace.
    #[command(after_help = ATTACH_HELP)]
    Attach(AttachArgs),
    /// Wait until a named session includes visible text.
    Wait(WaitArgs),
    /// Send ordered input to a named session.
    #[command(after_help = SEND_HELP)]
    Send(SendArgs),
    /// Inspect lifecycle state and launch settings of a named session.
    Status(StatusArgs),
    /// List named local sessions and their states.
    List(ListArgs),
    /// Remove retained exited sessions and stale sockets.
    Prune(PruneArgs),
    /// List panes in a running workspace.
    Panes(PanesArgs),
    /// List named windows in a running workspace.
    Windows(WindowsArgs),
    /// Resolve this pane's authoritative workspace, window, and pane identity.
    Current(CurrentArgs),
    /// Move a live workspace tab strip between the top and bottom.
    TabPosition(TabPositionArgs),
    /// Reorder one workspace window.
    MoveWindow(MoveWindowArgs),
    /// Create and select a named workspace window.
    NewWindow(NewWindowArgs),
    /// Select one named workspace window for the attached terminal.
    SelectWindow(WindowActionArgs),
    /// Rename one workspace window.
    RenameWindow(RenameWindowArgs),
    /// Terminate one workspace window and all of its panes.
    CloseWindow(WindowActionArgs),
    /// Declaratively arrange one, two, or four workspace panes.
    Layout(LayoutArgs),
    /// Intentionally move human focus to one workspace pane.
    Focus(PaneActionArgs),
    /// Terminate exactly one workspace pane.
    ClosePane(PaneActionArgs),
    /// Move one running pane into another named window without restarting it.
    MovePane(MovePaneArgs),
    /// Grow one pane toward a neighboring boundary.
    ResizePane(ResizePaneArgs),
    /// Toggle one pane between its split and full-window presentation.
    ZoomPane(PaneActionArgs),
    /// Resize a named live session.
    Resize(ResizeArgs),
    /// Add a named moment to an active recording for later editing.
    #[command(after_help = MARK_HELP)]
    Mark(MarkArgs),
    /// List named moments in a recording.
    #[command(after_help = MARKERS_HELP)]
    Markers(MarkersArgs),
    /// Print retained readable terminal output or exact ANSI/VT bytes.
    Logs(LogsArgs),
    /// Restart a named session, reusing launch settings by default.
    Restart(RestartArgs),
    /// Terminate a named session.
    Stop(SessionArgs),
    /// Export a video from a recorded persistent session.
    #[command(after_help = VIDEO_HELP)]
    Video(VideoArgs),
    /// Serve isolated sessions for external testing clients.
    #[command(after_help = DRIVER_HELP)]
    Driver,
    /// Serve named Terminal Control sessions as MCP tools over stdio.
    Mcp,
    #[command(name = "__serve", hide = true)]
    Serve(ServeArgs),
    #[command(name = "__serve-workspace", hide = true)]
    ServeWorkspace(ServeWorkspaceArgs),
}

#[derive(Args)]
struct RenderArgs {
    /// Cell width used for terminal geometry and rendering.
    #[arg(long, default_value_t = 9)]
    cell_width: u16,
    /// Cell height used for terminal geometry and rendering.
    #[arg(long, default_value_t = 18)]
    cell_height: u16,
    /// Outer padding around the rendered terminal in pixels.
    #[arg(long, default_value_t = 18.0)]
    padding: f32,
    /// Font family used in SVG/PNG output.
    #[arg(
        long,
        default_value = "JetBrains Mono, SFMono-Regular, Menlo, monospace"
    )]
    font_family: String,
    /// Scale PNG output for sharp HiDPI viewing; SVG output is unchanged.
    #[arg(long, default_value_t = 2.0)]
    pixel_ratio: f32,
    /// Hide the terminal cursor in rendered output.
    #[arg(long)]
    hide_cursor: bool,
}

#[derive(Args)]
struct SourceArgs {
    /// Existing named terminal session to read.
    #[arg(value_name = "NAME")]
    name: Option<String>,
    /// Read one workspace pane instead of the composed workspace.
    #[arg(long, requires = "name")]
    pane: Option<u32>,
    /// Read one named workspace window instead of the selected window.
    #[arg(long, requires = "name", conflicts_with = "pane")]
    window: Option<String>,
    /// Terminal width in cells for command or ANSI input (default: 80).
    #[arg(long)]
    cols: Option<u16>,
    /// Terminal height in cells for command or ANSI input (default: 24).
    #[arg(long)]
    rows: Option<u16>,
    /// Observe command stdout/stderr as pipes instead of launching it in a PTY.
    #[arg(long)]
    pipe: bool,
    /// Render ANSI/VT bytes from this file; use `-` for stdin.
    #[arg(long, value_name = "FILE")]
    input: Option<PathBuf>,
    /// Replay a .termctrl recording instead of reading a live session or command.
    #[arg(long, value_name = "FILE")]
    recording: Option<PathBuf>,
    /// Replay a recording up to this named marker.
    #[arg(long, requires = "recording", conflicts_with = "at_ms")]
    at_marker: Option<String>,
    /// Replay a recording up to this timestamp in milliseconds.
    #[arg(long, requires = "recording")]
    at_ms: Option<u64>,
    /// Color environment policy for a command source (default: auto for PTY, always for pipe).
    #[arg(long, value_enum)]
    color: Option<ColorMode>,
    /// Quiet period before capture (default: 0 for named sessions, 250 for commands).
    #[arg(long)]
    settle_ms: Option<u64>,
    /// Settling deadline (default: 0 for named sessions, 5000 for commands).
    #[arg(long)]
    deadline_ms: Option<u64>,
    /// Wait this long before allowing the initial screen to settle.
    #[arg(long)]
    initial_delay_ms: Option<u64>,
    /// Wait until the visible terminal includes this text before interacting or capturing.
    #[arg(long)]
    wait_for: Option<String>,
    /// Fail if command or ANSI input exceeds this many terminal bytes (default: 16777216).
    #[arg(long)]
    max_bytes: Option<usize>,
    /// Working directory for the terminal command.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Terminal-host compatibility response profile.
    #[arg(long, value_enum)]
    host: Option<HostProfile>,
    /// Ordered input after readiness: key name or `text:<value>` (repeatable/groupable).
    #[arg(short = 's', long, value_name = "INPUT", num_args = 1..)]
    send: Vec<String>,
    /// Command and arguments to launch, following `--`.
    #[arg(last = true, required = false, num_args = 1.., allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct ShowArgs {
    #[command(flatten)]
    render: RenderArgs,
    #[command(flatten)]
    source: SourceArgs,
    /// Standard-output representation of the visible screen.
    #[arg(long, value_enum, default_value = "txt")]
    format: ShotFormat,
}

#[derive(Args)]
struct SaveArgs {
    #[command(flatten)]
    render: RenderArgs,
    #[command(flatten)]
    source: SourceArgs,
    /// Output path for one format, or output stem for several formats.
    #[arg(short, long)]
    out: PathBuf,
    /// Artifact format to write; repeat to write several explicit formats.
    #[arg(long = "format", value_enum, required = true)]
    formats: Vec<ShotFormat>,
}

#[derive(Args)]
struct StartArgs {
    /// Stable local name used by later session commands.
    name: String,
    /// Terminal width in cells.
    #[arg(long, default_value_t = 80)]
    cols: u16,
    /// Terminal height in cells.
    #[arg(long, default_value_t = 24)]
    rows: u16,
    /// Terminal cell width in pixels.
    #[arg(long, default_value_t = 9)]
    cell_width: u16,
    /// Terminal cell height in pixels.
    #[arg(long, default_value_t = 18)]
    cell_height: u16,
    /// Maximum raw terminal bytes retained by the live session.
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    max_bytes: usize,
    /// Working directory for the terminal command.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Write timestamped terminal output and client/host input to this private recording file.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Color environment policy for the terminal command.
    #[arg(long, value_enum, default_value = "auto")]
    color: ColorMode,
    /// Terminal-host compatibility response profile.
    #[arg(long, value_enum)]
    host: Option<HostProfile>,
    /// Command and arguments to launch, following `--`.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct RunArgs {
    /// Stable local name used by later commands; defaults to `workspace` or the executable name.
    #[arg(value_name = "NAME")]
    name: Option<String>,
    /// Terminal width in cells when terminal dimensions cannot be detected.
    #[arg(long, default_value_t = 80)]
    cols: u16,
    /// Terminal height in cells when terminal dimensions cannot be detected.
    #[arg(long, default_value_t = 24)]
    rows: u16,
    /// Terminal cell width in pixels.
    #[arg(long, default_value_t = 9)]
    cell_width: u16,
    /// Terminal cell height in pixels.
    #[arg(long, default_value_t = 18)]
    cell_height: u16,
    /// Maximum raw terminal bytes retained by the live session.
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    max_bytes: usize,
    /// Working directory for the terminal command.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Record the composed workspace, including tabs, splits, and window switches.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Place the persistent workspace tab strip at the top or bottom.
    #[arg(long, value_enum, default_value = "bottom")]
    tab_position: TabPositionArg,
    /// Color environment policy for the terminal command.
    #[arg(long, value_enum, default_value = "auto")]
    color: ColorMode,
    /// Terminal-host compatibility response profile.
    #[arg(long, value_enum)]
    host: Option<HostProfile>,
    /// Command and arguments to launch, following `--`.
    #[arg(last = true, num_args = 1.., allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct AttachArgs {
    /// Existing workspace name.
    name: String,
    /// Terminal cell width in pixels.
    #[arg(long, default_value_t = 9)]
    cell_width: u16,
    /// Terminal cell height in pixels.
    #[arg(long, default_value_t = 18)]
    cell_height: u16,
}

#[derive(Args)]
struct WaitArgs {
    /// Name of a running session.
    name: String,
    /// Visible text that must appear in the session screen.
    text: String,
    /// Target one workspace pane instead of the active pane.
    #[arg(long)]
    pane: Option<u32>,
    /// Target one named workspace window's active pane.
    #[arg(long, conflicts_with = "pane")]
    window: Option<String>,
    /// Maximum time to wait before returning an error.
    #[arg(long, default_value_t = 5000, value_name = "MS")]
    timeout: u64,
}

#[derive(Args)]
struct SendArgs {
    /// Name of a running session.
    name: String,
    /// Target one workspace pane instead of the active pane.
    #[arg(long)]
    pane: Option<u32>,
    /// Target one named workspace window's active pane.
    #[arg(long, conflicts_with = "pane")]
    window: Option<String>,
    /// Delay between input atoms; text is split into characters when set.
    #[arg(long, default_value_t = 0)]
    pace_ms: u64,
    /// Send bytes read from stdin as one burst; cannot be paced or combined with INPUT.
    #[arg(long, conflicts_with = "input")]
    stdin: bool,
    /// Ordered input: key name or `text:<value>`.
    #[arg(value_name = "INPUT")]
    input: Vec<String>,
}

#[derive(Args)]
struct StatusArgs {
    /// Name of a running or inspectable exited session.
    name: String,
    /// Write structured JSON status.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ListArgs {
    /// Include retained exited sessions and stale sockets.
    #[arg(long)]
    all: bool,
    /// Write structured JSON entries.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct PruneArgs {
    /// Show removable entries without deleting them.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct PanesArgs {
    /// Name of a running workspace.
    name: String,
    /// List one named window instead of the selected window.
    #[arg(long)]
    window: Option<String>,
    /// Write structured JSON pane entries.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct WindowsArgs {
    /// Name of a running workspace.
    name: String,
    /// Write structured JSON window entries.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CurrentArgs {
    /// Workspace name; defaults to TERMCTRL_WORKSPACE from the current pane.
    #[arg(value_name = "NAME")]
    name: Option<String>,
    /// Stable pane id; defaults to TERMCTRL_PANE_ID from the current pane.
    #[arg(long)]
    pane: Option<u32>,
    /// Write structured JSON context.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TabPositionArgs {
    /// Name of a running workspace.
    name: String,
    /// New live tab strip position.
    #[arg(value_enum)]
    position: TabPositionArg,
}

#[derive(Args)]
struct MoveWindowArgs {
    /// Name of a running workspace.
    name: String,
    /// Exact window name returned by `termctrl windows`.
    window: String,
    /// Final zero-based tab index.
    #[arg(long)]
    index: usize,
}

#[derive(Args)]
struct NewWindowArgs {
    /// Name of a running workspace.
    name: String,
    /// Unique window name; defaults to window-N.
    #[arg(value_name = "WINDOW")]
    window: Option<String>,
    /// Working directory for the new window's first pane.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Command and arguments for the first pane; defaults to $SHELL.
    #[arg(last = true, num_args = 1.., allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct WindowActionArgs {
    /// Name of a running workspace.
    name: String,
    /// Exact window name returned by `termctrl windows`.
    window: String,
}

#[derive(Args)]
struct RenameWindowArgs {
    /// Name of a running workspace.
    name: String,
    /// Existing window name.
    window: String,
    /// New unique window name.
    new_name: String,
}

#[derive(Args)]
struct LayoutArgs {
    /// Name of a running workspace.
    name: String,
    /// Arrange one named window without selecting it.
    #[arg(long)]
    window: Option<String>,
    /// Desired grid: 1x1, 2x1, 1x2, or 2x2.
    #[arg(long, value_parser = parse_grid)]
    grid: (u16, u16),
    /// Command for the first pane created while growing the layout; defaults to $SHELL.
    #[arg(last = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct PaneActionArgs {
    /// Name of a running workspace.
    name: String,
    /// Stable pane id returned by `termctrl panes`.
    #[arg(long)]
    pane: u32,
}

#[derive(Args)]
struct MovePaneArgs {
    /// Name of a running workspace.
    name: String,
    /// Globally stable pane id returned by `termctrl panes`.
    #[arg(long)]
    pane: u32,
    /// Exact destination window name.
    #[arg(long)]
    window: String,
    /// Stack below the destination's active pane instead of splitting to its right.
    #[arg(long)]
    vertical: bool,
}

#[derive(Args)]
struct ResizePaneArgs {
    /// Name of a running workspace.
    name: String,
    /// Globally stable pane id returned by `termctrl panes`.
    #[arg(long)]
    pane: u32,
    /// Boundary toward which the pane should grow.
    #[arg(long, value_enum)]
    direction: PaneResizeDirection,
    /// Number of terminal cells by which to move the boundary.
    #[arg(long, default_value_t = 1)]
    cells: u16,
}

#[derive(Clone, Copy, ValueEnum)]
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

#[derive(Args)]
struct ResizeArgs {
    /// Name of a running session.
    name: String,
    /// New terminal width in cells.
    #[arg(long)]
    cols: u16,
    /// New terminal height in cells.
    #[arg(long)]
    rows: u16,
    /// New terminal cell width in pixels; defaults to current geometry.
    #[arg(long)]
    cell_width: Option<u16>,
    /// New terminal cell height in pixels; defaults to current geometry.
    #[arg(long)]
    cell_height: Option<u16>,
}

#[derive(Args)]
struct MarkArgs {
    /// Name of a running session started with --record.
    name: String,
    /// Unique marker name referenced by video edit plans.
    marker: String,
}

#[derive(Args)]
struct MarkersArgs {
    /// Recording created by `termctrl start --record`.
    input: PathBuf,
    /// Write structured JSON marker entries.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct LogsArgs {
    /// Name of a running or inspectable exited session.
    name: String,
    /// Read the active pane logs from one named workspace window.
    #[arg(long)]
    window: Option<String>,
    /// Write exact retained ANSI/VT stream bytes instead of readable retained output.
    #[arg(long)]
    ansi: bool,
}

#[derive(Args)]
struct RestartArgs {
    /// Name of a session to restart using its retained launch settings.
    name: String,
    #[arg(long)]
    cols: Option<u16>,
    #[arg(long)]
    rows: Option<u16>,
    #[arg(long)]
    cell_width: Option<u16>,
    #[arg(long)]
    cell_height: Option<u16>,
    #[arg(long)]
    max_bytes: Option<usize>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    record: Option<PathBuf>,
    #[arg(long, value_enum)]
    color: Option<ColorMode>,
    #[arg(long, value_enum)]
    host: Option<HostProfile>,
    /// Replacement command; when omitted the prior command is reused.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct SessionArgs {
    /// Name of a running session.
    name: String,
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long)]
    name: String,
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    record: Option<PathBuf>,
    #[arg(long)]
    opentui_host: bool,
    #[arg(long, value_enum, default_value = "auto")]
    color: ColorMode,
    #[arg(long)]
    cols: u16,
    #[arg(long)]
    rows: u16,
    #[arg(long)]
    cell_width: u16,
    #[arg(long)]
    cell_height: u16,
    #[arg(long)]
    max_bytes: usize,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct ServeWorkspaceArgs {
    #[arg(long)]
    name: String,
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    record: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "bottom")]
    tab_position: TabPositionArg,
    #[arg(long)]
    opentui_host: bool,
    #[arg(long, value_enum, default_value = "auto")]
    color: ColorMode,
    #[arg(long)]
    cols: u16,
    #[arg(long)]
    rows: u16,
    #[arg(long)]
    cell_width: u16,
    #[arg(long)]
    cell_height: u16,
    #[arg(long)]
    max_bytes: usize,
    #[arg(last = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct VideoArgs {
    /// Recording created by `termctrl start --record`.
    input: PathBuf,
    /// Override the recorded terminal cell width in rendered pixels.
    #[arg(long)]
    cell_width: Option<u16>,
    /// Override the recorded terminal cell height in rendered pixels.
    #[arg(long)]
    cell_height: Option<u16>,
    /// Outer padding around the rendered terminal in pixels.
    #[arg(long, default_value_t = 18.0)]
    padding: f32,
    /// Font family used in video output.
    #[arg(
        long,
        default_value = "JetBrains Mono, SFMono-Regular, Menlo, monospace"
    )]
    font_family: String,
    /// Scale video frames for sharp HiDPI viewing.
    #[arg(long, default_value_t = 2.0)]
    pixel_ratio: f32,
    /// Output video file path.
    #[arg(short, long, default_value = "video.mp4")]
    out: PathBuf,
    /// Hide the terminal cursor in rendered output.
    #[arg(long)]
    hide_cursor: bool,
    /// Add a bottom footer with clip caption, elapsed timecode, and TERMINAL CONTROL branding.
    #[arg(long)]
    footer: bool,
    /// Maximum sampled frames per second (1 to 1000).
    #[arg(long, default_value_t = 20)]
    fps: u32,
    /// Marker-based JSON edit plan with clips, captions, speeds, and holds.
    #[arg(long)]
    edit: Option<PathBuf>,
    /// Hold the final frame for this duration; use 0 for no artificial final pause.
    #[arg(long, default_value_t = 1000)]
    tail_ms: u64,
    /// Include leading contentless startup/terminal negotiation frames.
    #[arg(long)]
    include_startup: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum HostProfile {
    /// Respond to OpenTUI startup terminal capability queries.
    Opentui,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorMode {
    /// Preserve the current process color environment.
    Auto,
    /// Remove NO_COLOR and set common force-color environment variables.
    Always,
    /// Set common no-color environment variables.
    Never,
}

#[derive(Clone, Copy, ValueEnum)]
enum TabPositionArg {
    Top,
    Bottom,
}

impl From<TabPositionArg> for session::TabPosition {
    fn from(position: TabPositionArg) -> Self {
        match position {
            TabPositionArg::Top => Self::Top,
            TabPositionArg::Bottom => Self::Bottom,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ShotFormat {
    /// PNG image.
    Png,
    /// SVG image.
    Svg,
    /// Visible plain text.
    Txt,
    /// Structured terminal cells.
    Json,
    /// Original ANSI/VT terminal stream.
    Ansi,
}

impl From<ColorMode> for shot_engine::ColorMode {
    fn from(value: ColorMode) -> Self {
        match value {
            ColorMode::Auto => shot_engine::ColorMode::Auto,
            ColorMode::Always => shot_engine::ColorMode::Always,
            ColorMode::Never => shot_engine::ColorMode::Never,
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Show(args) => show(args)?,
        Command::Save(args) => save(args)?,
        Command::Start(args) => {
            start_session(&args)?;
            println!("{}", args.name);
        }
        Command::Run(args) => run_session(&args)?,
        Command::Attach(args) => attach_session(&args)?,
        Command::Wait(args) => {
            session::wait_for_in(
                &args.name,
                args.window,
                args.pane,
                args.text,
                Duration::from_millis(args.timeout),
            )?;
        }
        Command::Send(args) => send(args)?,
        Command::Status(args) => status(args)?,
        Command::List(args) => list(args)?,
        Command::Prune(args) => prune(args)?,
        Command::Panes(args) => panes(args)?,
        Command::Windows(args) => windows(args)?,
        Command::Current(args) => current(args)?,
        Command::TabPosition(args) => {
            let windows = session::set_workspace_tab_position(&args.name, args.position.into())?;
            print_windows(&windows, true)?;
        }
        Command::MoveWindow(args) => {
            let windows = session::move_workspace_window(&args.name, args.window, args.index)?;
            print_windows(&windows, true)?;
        }
        Command::NewWindow(args) => {
            let windows =
                session::create_workspace_window(&args.name, args.window, args.command, args.cwd)?;
            print_windows(&windows, true)?;
        }
        Command::SelectWindow(args) => {
            let windows = session::select_workspace_window(&args.name, args.window)?;
            print_windows(&windows, true)?;
        }
        Command::RenameWindow(args) => {
            let windows = session::rename_workspace_window(&args.name, args.window, args.new_name)?;
            print_windows(&windows, true)?;
        }
        Command::CloseWindow(args) => {
            let windows = session::close_workspace_window(&args.name, args.window)?;
            print_windows(&windows, true)?;
        }
        Command::Layout(args) => {
            let panes = session::set_workspace_layout_in_window(
                &args.name,
                args.window,
                args.grid.0,
                args.grid.1,
                args.command,
            )?;
            print_panes(&panes, true)?;
        }
        Command::Focus(args) => {
            let panes = session::focus_workspace_pane(&args.name, args.pane)?;
            print_panes(&panes, true)?;
        }
        Command::ClosePane(args) => {
            let panes = session::close_workspace_pane(&args.name, args.pane)?;
            print_panes(&panes, true)?;
        }
        Command::MovePane(args) => {
            let windows =
                session::move_workspace_pane(&args.name, args.pane, args.window, args.vertical)?;
            print_windows(&windows, true)?;
        }
        Command::ResizePane(args) => {
            let panes = session::resize_workspace_pane(
                &args.name,
                args.pane,
                args.direction.into(),
                args.cells,
            )?;
            print_panes(&panes, true)?;
        }
        Command::ZoomPane(args) => {
            let panes = session::toggle_workspace_zoom(&args.name, args.pane)?;
            print_panes(&panes, true)?;
        }
        Command::Resize(args) => {
            validate_terminal_size(args.cols, args.rows)?;
            session::resize(
                &args.name,
                args.cols,
                args.rows,
                args.cell_width,
                args.cell_height,
            )?;
        }
        Command::Mark(args) => session::mark(&args.name, args.marker)?,
        Command::Markers(args) => markers(args)?,
        Command::Logs(args) => logs(args)?,
        Command::Restart(args) => {
            restart_session(&args)?;
            println!("{}", args.name);
        }
        Command::Stop(args) => session::stop(&args.name)?,
        Command::Video(args) => {
            let out = args.out.clone();
            recording::video(
                &args.input,
                &recording::VideoOptions {
                    out: args.out,
                    cell_width: args.cell_width,
                    cell_height: args.cell_height,
                    padding: args.padding,
                    font_family: args.font_family,
                    pixel_ratio: args.pixel_ratio,
                    hide_cursor: args.hide_cursor,
                    footer: args.footer,
                    fps: args.fps,
                    tail: Duration::from_millis(args.tail_ms),
                    include_startup: args.include_startup,
                    edit: args.edit,
                },
            )?;
            println!("{}", out.display());
        }
        Command::Driver => {
            driver::serve(BufReader::new(io::stdin().lock()), io::stdout().lock())?;
        }
        Command::Mcp => {
            tokio::runtime::Runtime::new()?.block_on(mcp::serve())?;
        }
        Command::Serve(args) => {
            session::serve(
                args.name,
                args.socket,
                args.command,
                args.cwd,
                args.record,
                shot_engine::Options {
                    cols: args.cols,
                    rows: args.rows,
                    cell_width: args.cell_width,
                    cell_height: args.cell_height,
                    settle: Duration::ZERO,
                    deadline: Duration::ZERO,
                    input: Vec::new(),
                    initial_delay: Duration::ZERO,
                    wait_for: None,
                    max_bytes: args.max_bytes,
                    opentui_host: args.opentui_host,
                    color: args.color.into(),
                    env: Default::default(),
                    inherit_env: true,
                },
            )?;
        }
        Command::ServeWorkspace(args) => {
            session::serve_workspace(
                args.name,
                args.socket,
                args.command,
                args.cwd,
                args.record,
                shot_engine::Options {
                    cols: args.cols,
                    rows: args.rows,
                    cell_width: args.cell_width,
                    cell_height: args.cell_height,
                    settle: Duration::ZERO,
                    deadline: Duration::ZERO,
                    input: Vec::new(),
                    initial_delay: Duration::ZERO,
                    wait_for: None,
                    max_bytes: args.max_bytes,
                    opentui_host: args.opentui_host,
                    color: args.color.into(),
                    env: Default::default(),
                    inherit_env: true,
                },
                args.tab_position.into(),
            )?;
        }
    }
    Ok(())
}

fn show(args: ShowArgs) -> Result<()> {
    if args.format == ShotFormat::Png {
        bail!("show does not support PNG output; use save --format png --out PATH");
    }
    let captured = read_source(&args.source, &args.render)?;
    write_stdout(&captured, &args.render, args.format)
}

fn save(args: SaveArgs) -> Result<()> {
    let captured = read_source(&args.source, &args.render)?;
    write_outputs(&captured, &args.render, &args.out, &args.formats)
}

fn read_source(args: &SourceArgs, render: &RenderArgs) -> Result<shot_engine::Shot> {
    let defaults = shot_engine::Options::default();
    let (settle, deadline) = capture_timing(args, &defaults);
    if let Some(path) = args.recording.as_ref() {
        if args.name.is_some()
            || args.pipe
            || args.input.is_some()
            || !args.command.is_empty()
            || args.cols.is_some()
            || args.rows.is_some()
            || args.color.is_some()
            || args.settle_ms.is_some()
            || args.deadline_ms.is_some()
            || args.initial_delay_ms.is_some()
            || args.wait_for.is_some()
            || args.max_bytes.is_some()
            || args.cwd.is_some()
            || args.host.is_some()
            || !args.send.is_empty()
        {
            bail!(
                "--recording can only be combined with rendering options, --at-marker, or --at-ms"
            );
        }
        return recording::shot_at(path, args.at_ms, args.at_marker.as_deref());
    }
    if args.at_marker.is_some() || args.at_ms.is_some() {
        bail!("--at-marker and --at-ms require --recording");
    }
    if args.input.is_some() && (args.pipe || args.name.is_some() || !args.command.is_empty()) {
        bail!("--input cannot be combined with --pipe, NAME, or a command");
    }
    if args.name.is_some() && (args.pipe || !args.command.is_empty()) {
        bail!("NAME cannot be combined with --pipe or a command");
    }
    if let Some(name) = args.name.as_deref() {
        if args.cols.is_some()
            || args.rows.is_some()
            || args.color.is_some()
            || args.initial_delay_ms.is_some()
            || args.wait_for.is_some()
            || args.max_bytes.is_some()
            || args.cwd.is_some()
            || args.host.is_some()
            || !args.send.is_empty()
        {
            bail!("named-session reads support rendering, --settle-ms, and --deadline-ms only");
        }
        return session::show_in(name, args.window.clone(), args.pane, settle, deadline);
    }
    let cols = args.cols.unwrap_or(defaults.cols);
    let rows = args.rows.unwrap_or(defaults.rows);
    validate_terminal_size(cols, rows)?;
    let max_bytes = args.max_bytes.unwrap_or(defaults.max_bytes);
    if let Some(path) = args.input.as_ref() {
        if args.color.is_some()
            || args.settle_ms.is_some()
            || args.deadline_ms.is_some()
            || args.initial_delay_ms.is_some()
            || args.wait_for.is_some()
            || args.cwd.is_some()
            || args.host.is_some()
            || !args.send.is_empty()
        {
            bail!("--input reads support dimensions, rendering, and --max-bytes only");
        }
        let mut input = Vec::new();
        let limit = max_bytes.saturating_add(1) as u64;
        if path.as_os_str() == "-" {
            io::stdin()
                .take(limit)
                .read_to_end(&mut input)
                .context("read ANSI input")?;
        } else {
            fs::File::open(path)
                .with_context(|| format!("open {}", path.display()))?
                .take(limit)
                .read_to_end(&mut input)
                .with_context(|| format!("read {}", path.display()))?;
        }
        return shot_engine::from_ansi(input, rows, cols, max_bytes);
    }
    if args.command.is_empty() {
        bail!("provide NAME, a command after --, or --input FILE");
    }
    if args.pipe
        && (!args.send.is_empty()
            || args.host.is_some()
            || args.initial_delay_ms.is_some()
            || args.settle_ms.is_some())
    {
        bail!("--pipe reads do not support --send, --host, --initial-delay-ms, or --settle-ms");
    }
    let color = args.color.unwrap_or(if args.pipe {
        ColorMode::Always
    } else {
        ColorMode::Auto
    });
    let options = shot_engine::Options {
        cols,
        rows,
        cell_width: render.cell_width,
        cell_height: render.cell_height,
        settle,
        deadline,
        input: input_bytes(&args.send)?,
        initial_delay: Duration::from_millis(args.initial_delay_ms.unwrap_or(0)),
        wait_for: args.wait_for.clone(),
        max_bytes,
        opentui_host: matches!(args.host, Some(HostProfile::Opentui)),
        color: color.into(),
        env: Default::default(),
        inherit_env: true,
    };
    if args.pipe {
        shot_engine::from_pipe_command(&args.command, args.cwd.as_deref(), &options)
    } else {
        shot_engine::from_command(&args.command, args.cwd.as_deref(), &options)
    }
}

fn capture_timing(args: &SourceArgs, defaults: &shot_engine::Options) -> (Duration, Duration) {
    let default_settle = if args.name.is_some() {
        0
    } else {
        defaults.settle.as_millis() as u64
    };
    let default_deadline = if args.name.is_some() {
        0
    } else {
        defaults.deadline.as_millis() as u64
    };
    (
        Duration::from_millis(args.settle_ms.unwrap_or(default_settle)),
        Duration::from_millis(args.deadline_ms.unwrap_or(default_deadline)),
    )
}

fn send(args: SendArgs) -> Result<()> {
    if args.stdin && args.pace_ms > 0 {
        bail!("--stdin cannot be combined with --pace-ms");
    }
    let input = if args.stdin {
        let mut bytes = Vec::new();
        io::stdin()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .context("read session input")?;
        if bytes.len() > 1024 * 1024 {
            bail!("session input exceeds 1 MiB");
        }
        vec![bytes]
    } else {
        if args.input.is_empty() {
            bail!("provide INPUT events or --stdin");
        }
        session_input(&args.input, args.pace_ms > 0)?
    };
    session::send_to_in(
        &args.name,
        args.window,
        args.pane,
        input,
        Duration::from_millis(args.pace_ms),
    )?;
    Ok(())
}

fn status(args: StatusArgs) -> Result<()> {
    let status = session::status(&args.name)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("{} {}", args.name, session_state(status.state));
        println!("cwd: {}", status.launch.cwd.display());
        println!("command: {}", status.launch.command.join(" "));
        println!("viewport: {}x{}", status.cols, status.rows);
        println!(
            "recording: {}",
            status
                .launch
                .record
                .as_ref()
                .map_or_else(|| "none".to_owned(), |path| path.display().to_string())
        );
    }
    Ok(())
}

fn current(args: CurrentArgs) -> Result<()> {
    let workspace = std::env::var("TERMCTRL_WORKSPACE").ok();
    let session_name = std::env::var("TERMCTRL_SESSION").ok();
    if args.name.is_none() && workspace.is_none() {
        if args.pane.is_some() {
            bail!("--pane requires a workspace name or TERMCTRL_WORKSPACE");
        }
        let name = session_name.context(
            "not running inside a named Terminal Control session; provide a workspace NAME",
        )?;
        let status = session::status(&name)?;
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "session": name,
                    "workspace": null,
                    "pane": null,
                    "state": session_state(status.state),
                    "command": status.launch.command,
                    "cwd": status.launch.cwd,
                }))?
            );
        } else {
            println!("session: {name}");
            println!("state: {}", session_state(status.state));
            println!("cwd: {}", status.launch.cwd.display());
            println!("command: {}", status.launch.command.join(" "));
        }
        return Ok(());
    }
    let name = args.name.or_else(|| workspace.clone()).context(
        "not running inside a Terminal Control workspace; provide NAME or set TERMCTRL_WORKSPACE",
    )?;
    let inherited_pane = std::env::var("TERMCTRL_PANE_ID").ok();
    let pane = resolve_current_pane(
        args.pane,
        &name,
        workspace.as_deref(),
        inherited_pane.as_deref(),
    )?;
    let context = session::workspace_context(&name, pane)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&context)?);
    } else {
        println!("workspace: {}", context.workspace);
        println!(
            "window: {} ({}:{})",
            context.window_index, context.window_id, context.window
        );
        println!("pane: {}", context.pane);
        println!("tabs: {}", context.tab_position.as_str());
    }
    Ok(())
}

fn resolve_current_pane(
    explicit: Option<u32>,
    workspace: &str,
    inherited_workspace: Option<&str>,
    inherited_pane: Option<&str>,
) -> Result<Option<u32>> {
    if explicit.is_some() || inherited_workspace != Some(workspace) {
        return Ok(explicit);
    }
    inherited_pane
        .map(|value| {
            value
                .parse::<u32>()
                .with_context(|| format!("invalid TERMCTRL_PANE_ID {value:?}"))
        })
        .transpose()
}

fn list(args: ListArgs) -> Result<()> {
    let sessions = session::list()?
        .into_iter()
        .filter(|entry| {
            args.all
                || entry
                    .status
                    .as_ref()
                    .is_some_and(|status| status.state == session::SessionState::Running)
        })
        .collect::<Vec<_>>();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
    } else {
        for entry in sessions {
            if let Some(status) = entry.status {
                println!(
                    "{}\t{}\t{}x{}\t{}",
                    entry.name,
                    session_state(status.state),
                    status.cols,
                    status.rows,
                    if status.recording { "recording" } else { "-" }
                );
            } else {
                let reason = match entry.unavailable {
                    Some(session::UnavailableReason::IncompatibleProtocol) => "incompatible",
                    _ => "stale",
                };
                println!("{}\t{}\t-\t-", entry.name, reason);
            }
        }
    }
    Ok(())
}

fn prune(args: PruneArgs) -> Result<()> {
    let candidates = session::list()?
        .into_iter()
        .filter(|entry| {
            entry
                .status
                .as_ref()
                .is_some_and(|status| status.state == session::SessionState::Exited)
                || entry.unavailable == Some(session::UnavailableReason::Stale)
        })
        .collect::<Vec<_>>();
    let mut removed = 0;
    for entry in candidates {
        if let Some(kind) = session::prune(&entry.name, args.dry_run)? {
            let kind = match kind {
                session::PruneKind::Exited => "exited",
                session::PruneKind::Stale => "stale",
            };
            println!("{}\t{kind}", entry.name);
            removed += 1;
        }
    }
    eprintln!(
        "{} {}",
        if args.dry_run {
            "would remove"
        } else {
            "removed"
        },
        removed
    );
    Ok(())
}

fn logs(args: LogsArgs) -> Result<()> {
    let bytes = match args.window {
        Some(window) => session::logs_window(&args.name, window, args.ansi)?,
        None => session::logs(&args.name, args.ansi)?,
    };
    io::stdout()
        .write_all(&bytes)
        .context("write session logs")?;
    if !args.ansi && !bytes.ends_with(b"\n") {
        io::stdout()
            .write_all(b"\n")
            .context("write session logs newline")?;
    }
    Ok(())
}

fn markers(args: MarkersArgs) -> Result<()> {
    let markers = recording::markers(&recording::read(&args.input)?);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&markers)?);
        return Ok(());
    }
    for marker in markers {
        println!("{}\t{}", marker.at_ms, marker.name);
    }
    Ok(())
}

fn start_session(args: &StartArgs) -> Result<()> {
    validate_terminal_size(args.cols, args.rows)?;
    let options = shot_engine::Options {
        cols: args.cols,
        rows: args.rows,
        cell_width: args.cell_width,
        cell_height: args.cell_height,
        settle: Duration::ZERO,
        deadline: Duration::ZERO,
        input: Vec::new(),
        initial_delay: Duration::ZERO,
        wait_for: None,
        max_bytes: args.max_bytes,
        opentui_host: matches!(args.host, Some(HostProfile::Opentui)),
        color: args.color.into(),
        env: Default::default(),
        inherit_env: true,
    };
    session::start(
        &args.name,
        &args.command,
        args.cwd.as_deref(),
        args.record.as_deref(),
        &options,
    )
}

fn run_session(args: &RunArgs) -> Result<()> {
    let name = match args.name.as_deref() {
        Some(name) => name.to_owned(),
        None if args.command.is_empty() => "workspace".to_owned(),
        None => session::infer_name(&args.command)?,
    };
    let (cols, rows) = crossterm::terminal::size().unwrap_or((args.cols, args.rows));
    validate_workspace_size(cols, rows)?;
    let options = shot_engine::Options {
        cols,
        rows,
        cell_width: args.cell_width,
        cell_height: args.cell_height,
        max_bytes: args.max_bytes,
        opentui_host: matches!(args.host, Some(HostProfile::Opentui)),
        color: args.color.into(),
        ..shot_engine::Options::default()
    };
    session::run_foreground(
        &name,
        &args.command,
        args.cwd.as_deref(),
        args.record.as_deref(),
        &options,
        args.tab_position.into(),
    )
}

fn attach_session(args: &AttachArgs) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size()
        .context("read current terminal size for workspace attachment")?;
    validate_workspace_size(cols, rows)?;
    session::attach(
        &args.name,
        &shot_engine::Options {
            cols,
            rows,
            cell_width: args.cell_width,
            cell_height: args.cell_height,
            ..shot_engine::Options::default()
        },
    )
}

fn panes(args: PanesArgs) -> Result<()> {
    let panes = session::panes_in_window(&args.name, args.window)?;
    print_panes(&panes, args.json)
}

fn windows(args: WindowsArgs) -> Result<()> {
    let windows = session::windows(&args.name)?;
    print_windows(&windows, args.json)
}

fn print_windows(windows: &[session::WindowStatus], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(windows)?);
        return Ok(());
    }
    for window in windows {
        let activity = window
            .activity_kinds
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{}\t{}\t{}\t{} panes\t{}x{}\t{}",
            window.index,
            if window.active { "active" } else { "" },
            window.name,
            window.pane_count,
            window.cols,
            window.rows,
            activity,
        );
    }
    Ok(())
}

fn print_panes(panes: &[session::PaneStatus], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&panes)?);
        return Ok(());
    }
    for pane in panes {
        let position = format!("{},{}", pane.x, pane.y);
        let title = pane
            .title
            .chars()
            .filter(|character| !character.is_control())
            .collect::<String>();
        println!(
            "{}\t{}\t{}\t{}x{}\t{}\t{}",
            pane.id,
            if pane.active { "active" } else { "" },
            position,
            pane.cols,
            pane.rows,
            title,
            pane.command.join(" ")
        );
    }
    Ok(())
}

fn parse_grid(value: &str) -> std::result::Result<(u16, u16), String> {
    let Some((columns, rows)) = value.split_once('x') else {
        return Err("grid must be 1x1, 2x1, 1x2, or 2x2".to_owned());
    };
    let columns = columns
        .parse::<u16>()
        .map_err(|_| "grid columns must be 1 or 2".to_owned())?;
    let rows = rows
        .parse::<u16>()
        .map_err(|_| "grid rows must be 1 or 2".to_owned())?;
    if !(1..=2).contains(&columns) || !(1..=2).contains(&rows) {
        return Err("grid must be 1x1, 2x1, 1x2, or 2x2".to_owned());
    }
    Ok((columns, rows))
}

fn restart_session(args: &RestartArgs) -> Result<()> {
    let previous = session::status(&args.name)?.launch;
    let cols = args.cols.unwrap_or(previous.cols);
    let rows = args.rows.unwrap_or(previous.rows);
    validate_terminal_size(cols, rows)?;
    let command = if args.command.is_empty() {
        previous.command
    } else {
        args.command.clone()
    };
    let cwd = args.cwd.clone().unwrap_or(previous.cwd);
    let record = args.record.clone().or(previous.record);
    session::restart(
        &args.name,
        &command,
        Some(&cwd),
        record.as_deref(),
        &shot_engine::Options {
            cols,
            rows,
            cell_width: args.cell_width.unwrap_or(previous.cell_width),
            cell_height: args.cell_height.unwrap_or(previous.cell_height),
            max_bytes: args.max_bytes.unwrap_or(previous.max_bytes),
            opentui_host: args.host.map_or(previous.opentui_host, |host| {
                matches!(host, HostProfile::Opentui)
            }),
            color: args.color.map_or(previous.color, Into::into),
            ..shot_engine::Options::default()
        },
    )
}

fn session_state(state: session::SessionState) -> &'static str {
    match state {
        session::SessionState::Running => "running",
        session::SessionState::Exited => "exited",
    }
}

fn input_bytes(events: &[String]) -> Result<Vec<u8>> {
    let mut input = Vec::new();
    for event in events {
        input.extend(input_event(event)?);
    }
    Ok(input)
}

fn input_event(event: &str) -> Result<Vec<u8>> {
    if let Some(text) = event.strip_prefix("text:") {
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(key) = event
        .strip_prefix("ctrl-")
        .or_else(|| event.strip_prefix("ctrl:"))
        && key.len() == 1
    {
        let key = key.as_bytes()[0].to_ascii_lowercase();
        if key.is_ascii_lowercase() {
            return Ok(vec![key - b'a' + 1]);
        }
    }
    Ok(match event {
        "enter" => b"\r".to_vec(),
        "escape" | "esc" => b"\x1b".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "tab" => b"\t".to_vec(),
        "shift-tab" => b"\x1b[Z".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "page-up" => b"\x1b[5~".to_vec(),
        "page-down" => b"\x1b[6~".to_vec(),
        _ => anyhow::bail!(
            "unsupported input event {event:?}; use text:<value>, ctrl-a through ctrl-z, enter, escape, arrows, tab, shift-tab, backspace, delete, home, end, page-up, or page-down"
        ),
    })
}

fn session_input(events: &[String], paced: bool) -> Result<Vec<Vec<u8>>> {
    if !paced {
        return Ok(vec![input_bytes(events)?]);
    }
    let mut input = Vec::new();
    for event in events {
        if let Some(text) = event.strip_prefix("text:") {
            input.extend(text.chars().map(|char| char.to_string().into_bytes()));
            continue;
        }
        input.push(input_event(event)?);
    }
    Ok(input)
}

fn validate_terminal_size(cols: u16, rows: u16) -> Result<()> {
    if cols == 0 || rows == 0 {
        bail!("terminal dimensions must be greater than zero");
    }
    Ok(())
}

fn validate_workspace_size(cols: u16, rows: u16) -> Result<()> {
    validate_terminal_size(cols, rows)?;
    if rows < 2 {
        bail!("workspace needs at least two rows for content and tabs");
    }
    Ok(())
}

fn write_outputs(
    captured: &shot_engine::Shot,
    args: &RenderArgs,
    out: &Path,
    formats: &[ShotFormat],
) -> Result<()> {
    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let enabled = |format| formats.contains(&format);
    let svg = (enabled(ShotFormat::Svg) || enabled(ShotFormat::Png))
        .then(|| rendered_svg(captured, args));
    if let Some(svg) = svg.as_ref().filter(|_| enabled(ShotFormat::Svg)) {
        let path = out.with_extension("svg");
        fs::write(&path, svg).with_context(|| format!("write {}", path.display()))?;
        println!("{}", path.display());
    }
    if let Some(svg) = svg.as_ref().filter(|_| enabled(ShotFormat::Png)) {
        let path = out.with_extension("png");
        render::png(svg, &path, args.pixel_ratio)?;
        println!("{}", path.display());
    }
    if enabled(ShotFormat::Json) {
        let path = out.with_extension("json");
        fs::write(&path, serde_json::to_vec_pretty(&captured.frame)?)
            .with_context(|| format!("write {}", path.display()))?;
        println!("{}", path.display());
    }
    if enabled(ShotFormat::Txt) {
        let path = out.with_extension("txt");
        fs::write(&path, captured.frame.text())
            .with_context(|| format!("write {}", path.display()))?;
        println!("{}", path.display());
    }
    if enabled(ShotFormat::Ansi) {
        let path = out.with_extension("ansi");
        fs::write(&path, &captured.ansi).with_context(|| format!("write {}", path.display()))?;
        println!("{}", path.display());
    }
    Ok(())
}

fn write_stdout(captured: &shot_engine::Shot, args: &RenderArgs, format: ShotFormat) -> Result<()> {
    let bytes = match format {
        ShotFormat::Txt => captured.frame.text().into_bytes(),
        ShotFormat::Json => serde_json::to_vec_pretty(&captured.frame)?,
        ShotFormat::Ansi => captured.ansi.clone(),
        ShotFormat::Svg => rendered_svg(captured, args).into_bytes(),
        ShotFormat::Png => unreachable!("show validates PNG before reading source"),
    };
    io::stdout()
        .write_all(&bytes)
        .context("write visible screen")?;
    if format != ShotFormat::Ansi && !bytes.ends_with(b"\n") {
        io::stdout()
            .write_all(b"\n")
            .context("write visible screen newline")?;
    }
    Ok(())
}

fn rendered_svg(captured: &shot_engine::Shot, args: &RenderArgs) -> String {
    render::svg(
        &captured.frame,
        &render::Options {
            cell_width: f32::from(args.cell_width),
            cell_height: f32::from(args.cell_height),
            font_size: f32::from(args.cell_height) * 0.78,
            padding: args.padding,
            font_family: args.font_family.clone(),
            show_cursor: !args.hide_cursor,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_ordered_input_events() {
        assert_eq!(
            input_bytes(&[
                "ctrl-p".to_owned(),
                "text:model".to_owned(),
                "enter".to_owned()
            ])
            .unwrap(),
            b"\x10model\r"
        );
    }

    #[test]
    fn rejects_unsupported_input_events() {
        assert!(input_bytes(&["space".to_owned()]).is_err());
    }

    #[test]
    fn encodes_control_and_navigation_input_events() {
        assert_eq!(
            input_bytes(&[
                "ctrl-c".to_owned(),
                "shift-tab".to_owned(),
                "delete".to_owned()
            ])
            .unwrap(),
            b"\x03\x1b[Z\x1b[3~"
        );
    }

    #[test]
    fn parses_one_off_show_input_sequence() {
        let cli = Cli::try_parse_from([
            "termctrl",
            "show",
            "--wait-for",
            "ready",
            "-s",
            "ctrl-p",
            "text:model",
            "enter",
            "--",
            "app",
        ])
        .unwrap();
        let Command::Show(args) = cli.command else {
            panic!("expected show command");
        };
        assert!(args.source.name.is_none());
        assert_eq!(args.source.command, ["app"]);
        assert_eq!(args.source.send, ["ctrl-p", "text:model", "enter"]);
    }

    #[test]
    fn parses_explicit_saved_formats_and_named_source() {
        let cli = Cli::try_parse_from([
            "termctrl", "save", "demo", "--out", "capture", "--format", "png", "--format", "txt",
        ])
        .unwrap();
        let Command::Save(args) = cli.command else {
            panic!("expected save command");
        };
        assert_eq!(args.source.name.as_deref(), Some("demo"));
        assert_eq!(args.formats, [ShotFormat::Png, ShotFormat::Txt]);
    }

    #[test]
    fn named_screen_reads_are_immediate_without_explicit_zero_flags() {
        let defaults = shot_engine::Options::default();
        let cli = Cli::try_parse_from(["termctrl", "show", "demo"]).unwrap();
        let Command::Show(args) = cli.command else {
            panic!("expected show command");
        };
        assert_eq!(
            capture_timing(&args.source, &defaults),
            (Duration::ZERO, Duration::ZERO)
        );

        let cli = Cli::try_parse_from(["termctrl", "show", "--", "app"]).unwrap();
        let Command::Show(args) = cli.command else {
            panic!("expected show command");
        };
        assert_eq!(
            capture_timing(&args.source, &defaults),
            (defaults.settle, defaults.deadline)
        );

        let cli = Cli::try_parse_from([
            "termctrl",
            "show",
            "demo",
            "--settle-ms",
            "25",
            "--deadline-ms",
            "500",
        ])
        .unwrap();
        let Command::Show(args) = cli.command else {
            panic!("expected show command");
        };
        assert_eq!(
            capture_timing(&args.source, &defaults),
            (Duration::from_millis(25), Duration::from_millis(500))
        );
    }

    #[test]
    fn parses_flat_session_control_commands() {
        assert!(Cli::try_parse_from(["termctrl", "run"]).is_ok());
        let cli =
            Cli::try_parse_from(["termctrl", "run", "workspace", "--tab-position", "top"]).unwrap();
        let Command::Run(run) = cli.command else {
            panic!("expected run command");
        };
        assert!(matches!(run.tab_position, TabPositionArg::Top));
        assert!(Cli::try_parse_from(["termctrl", "attach", "workspace"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "run", "editor", "--", "nvim", "."]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "panes", "workspace", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "windows", "workspace", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "current", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "current", "workspace", "--pane", "3"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "tab-position", "workspace", "top"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "termctrl",
                "move-window",
                "workspace",
                "editor",
                "--index",
                "0"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "termctrl",
                "new-window",
                "workspace",
                "editor",
                "--cwd",
                "/tmp",
                "--",
                "nvim",
                ".",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["termctrl", "select-window", "workspace", "editor"]).is_ok());
        assert!(
            Cli::try_parse_from(["termctrl", "rename-window", "workspace", "editor", "code",])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["termctrl", "close-window", "workspace", "code"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "termctrl",
                "move-pane",
                "workspace",
                "--pane",
                "3",
                "--window",
                "code",
                "--vertical",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "termctrl",
                "panes",
                "workspace",
                "--window",
                "editor",
                "--json"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["termctrl", "layout", "workspace", "--grid", "2x2"]).is_ok());
        let cli = Cli::try_parse_from([
            "termctrl",
            "layout",
            "workspace",
            "--grid",
            "2x1",
            "--",
            "nvim",
            "--clean",
        ])
        .unwrap();
        let Command::Layout(layout) = cli.command else {
            panic!("expected layout command");
        };
        assert_eq!(layout.command, ["nvim", "--clean"]);
        assert!(
            Cli::try_parse_from([
                "termctrl",
                "layout",
                "workspace",
                "--window",
                "editor",
                "--grid",
                "2x2"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["termctrl", "focus", "workspace", "--pane", "2"]).is_ok());
        assert!(
            Cli::try_parse_from(["termctrl", "close-pane", "workspace", "--pane", "2"]).is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "termctrl",
                "resize-pane",
                "workspace",
                "--pane",
                "2",
                "--direction",
                "left",
                "--cells",
                "5",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["termctrl", "zoom-pane", "workspace", "--pane", "2"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "layout", "workspace", "--grid", "3x2"]).is_err());
        assert!(
            Cli::try_parse_from(["termctrl", "send", "workspace", "--pane", "1", "enter"]).is_ok()
        );
        assert!(Cli::try_parse_from(["termctrl", "show", "workspace", "--pane", "1"]).is_ok());
        assert!(
            Cli::try_parse_from(["termctrl", "show", "workspace", "--window", "editor"]).is_ok()
        );
        assert!(Cli::try_parse_from(["termctrl", "status", "demo", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "list"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "list", "--all", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "prune", "--dry-run"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "termctrl", "resize", "demo", "--cols", "120", "--rows", "40"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["termctrl", "send", "demo", "--stdin"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "mark", "demo", "before-send"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "markers", "captures/demo.termctrl"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "logs", "demo", "--ansi"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "restart", "demo"]).is_ok());
        assert!(
            Cli::try_parse_from(["termctrl", "wait", "demo", "ready", "--timeout", "5"]).is_ok()
        );
    }

    #[test]
    fn current_context_uses_inherited_pane_only_for_the_same_workspace() {
        assert_eq!(
            resolve_current_pane(None, "workspace", Some("workspace"), Some("7")).unwrap(),
            Some(7)
        );
        assert_eq!(
            resolve_current_pane(None, "other", Some("workspace"), Some("7")).unwrap(),
            None
        );
        assert_eq!(
            resolve_current_pane(Some(3), "other", Some("workspace"), Some("7")).unwrap(),
            Some(3)
        );
        assert!(resolve_current_pane(None, "workspace", Some("workspace"), Some("bad")).is_err());
    }

    #[test]
    fn parses_default_shell_workspace() {
        let cli = Cli::try_parse_from(["termctrl", "run"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };

        assert_eq!(args.name, None);
        assert!(args.command.is_empty());
    }

    #[test]
    fn parses_foreground_run_with_an_inferred_name() {
        let cli = Cli::try_parse_from([
            "termctrl",
            "run",
            "--cwd",
            "/tmp",
            "--",
            "/usr/bin/nvim",
            "file.txt",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };

        assert_eq!(args.name, None);
        assert_eq!(args.command, ["/usr/bin/nvim", "file.txt"]);
        assert_eq!(args.cwd.as_deref(), Some(Path::new("/tmp")));
    }

    #[test]
    fn preserves_explicit_foreground_run_names() {
        let cli = Cli::try_parse_from(["termctrl", "run", "editor", "--", "nvim"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };

        assert_eq!(args.name.as_deref(), Some("editor"));
        assert_eq!(args.command, ["nvim"]);
    }

    #[test]
    fn show_rejects_png_before_starting_a_source() {
        let cli =
            Cli::try_parse_from(["termctrl", "show", "--format", "png", "--", "app"]).unwrap();
        let Command::Show(args) = cli.command else {
            panic!("expected show command");
        };

        assert_eq!(
            show(args).unwrap_err().to_string(),
            "show does not support PNG output; use save --format png --out PATH"
        );
    }

    #[test]
    fn parses_recording_source_seek_options() {
        let cli = Cli::try_parse_from([
            "termctrl",
            "show",
            "--recording",
            "captures/demo.termctrl",
            "--at-marker",
            "done",
        ])
        .unwrap();
        let Command::Show(args) = cli.command else {
            panic!("expected show command");
        };

        assert_eq!(
            args.source.recording.as_deref(),
            Some(Path::new("captures/demo.termctrl"))
        );
        assert_eq!(args.source.at_marker.as_deref(), Some("done"));
    }

    #[test]
    fn rejects_settling_options_for_pipe_reads() {
        let cli = Cli::try_parse_from([
            "termctrl",
            "show",
            "--pipe",
            "--settle-ms",
            "100",
            "--",
            "true",
        ])
        .unwrap();
        let Command::Show(args) = cli.command else {
            panic!("expected show command");
        };

        assert!(show(args).is_err());
    }

    #[test]
    fn rejects_zero_terminal_dimensions() {
        assert!(validate_terminal_size(0, 24).is_err());
        assert!(validate_terminal_size(80, 0).is_err());
        assert!(validate_workspace_size(80, 1).is_err());
        assert!(validate_workspace_size(80, 2).is_ok());
    }

    #[test]
    fn paced_session_input_splits_text_without_splitting_keys() {
        assert_eq!(
            session_input(&["text:hi".to_owned(), "enter".to_owned()], true).unwrap(),
            vec![b"h".to_vec(), b"i".to_vec(), b"\r".to_vec()]
        );
    }
}
