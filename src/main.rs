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
  termctrl run editor -- nvim
  termctrl wait demo '/connect' && termctrl send demo text:/connect enter
  termctrl show demo
  termctrl show demo --format semantic
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

Use --format json, --format ansi, or --format svg for another terminal representation. Use
`--format semantic` with a named target for optional application-provided UI semantics.
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
Run starts a PTY session in the foreground and mirrors it through the current terminal. Omit NAME
to use the executable basename as the session name. Inferred names must be valid session names and
must not collide with an existing session.
The application remains directly visible and interactive in this terminal pane while agents use
the same named-session commands to inspect its screen, send input, and resize it. The application
exits with this foreground command; no terminal multiplexer or terminal-specific API is required.

Examples:
  termctrl run editor --cwd ~/src/project -- nvim
  termctrl run -- /usr/bin/nvim";

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
    /// Run a named, agent-controllable application visibly in this terminal.
    #[command(after_help = RUN_HELP)]
    Run(RunArgs),
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
    /// Capture after this many milliseconds without output (default: 250).
    #[arg(long)]
    settle_ms: Option<u64>,
    /// Capture or return after this deadline even if output continues (default: 5000).
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
    /// Standard-output representation of the visible screen or application semantics.
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
    /// Stable local name used by later session commands; defaults to the executable basename.
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
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct WaitArgs {
    /// Name of a running session.
    name: String,
    /// Visible text that must appear in the session screen.
    text: String,
    /// Maximum time to wait before returning an error.
    #[arg(long, default_value_t = 5000, value_name = "MS")]
    timeout: u64,
}

#[derive(Args)]
struct SendArgs {
    /// Name of a running session.
    name: String,
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
    /// Application-provided semantic UI snapshot.
    Semantic,
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
        Command::Wait(args) => {
            session::wait(&args.name, args.text, Duration::from_millis(args.timeout))?;
        }
        Command::Send(args) => send(args)?,
        Command::Status(args) => status(args)?,
        Command::List(args) => list(args)?,
        Command::Prune(args) => prune(args)?,
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
    }
    Ok(())
}

fn show(args: ShowArgs) -> Result<()> {
    if args.format == ShotFormat::Semantic {
        return show_semantic(&args);
    }
    if args.format == ShotFormat::Png {
        bail!("show does not support PNG output; use save --format png --out PATH");
    }
    let captured = read_source(&args.source, &args.render)?;
    write_stdout(&captured, &args.render, args.format)
}

fn save(args: SaveArgs) -> Result<()> {
    if args.formats.contains(&ShotFormat::Semantic) {
        bail!("save does not support semantic output; use show NAME --format semantic");
    }
    let captured = read_source(&args.source, &args.render)?;
    write_outputs(&captured, &args.render, &args.out, &args.formats)
}

fn show_semantic(args: &ShowArgs) -> Result<()> {
    let source = &args.source;
    let render = &args.render;
    if source.recording.is_some()
        || source.at_marker.is_some()
        || source.at_ms.is_some()
        || source.cols.is_some()
        || source.rows.is_some()
        || source.pipe
        || source.input.is_some()
        || source.color.is_some()
        || source.settle_ms.is_some()
        || source.initial_delay_ms.is_some()
        || source.wait_for.is_some()
        || source.max_bytes.is_some()
        || source.cwd.is_some()
        || source.host.is_some()
        || !source.send.is_empty()
        || !source.command.is_empty()
        || render.cell_width != 9
        || render.cell_height != 18
        || render.padding != 18.0
        || render.font_family != "JetBrains Mono, SFMono-Regular, Menlo, monospace"
        || render.pixel_ratio != 2.0
        || render.hide_cursor
    {
        bail!("semantic output supports only NAME and --deadline-ms");
    }
    let name = source
        .name
        .as_deref()
        .context("semantic output requires a named session")?;
    let timeout = Duration::from_millis(source.deadline_ms.unwrap_or(1000));
    if timeout.is_zero() {
        bail!("--deadline-ms must be greater than zero for semantic output");
    }
    let snapshot = session::semantic_snapshot(name, timeout)?;
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    Ok(())
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
        return session::show(name, settle, deadline);
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
    let (default_settle, default_deadline) = if args.name.is_some() {
        (0, 0)
    } else {
        (
            defaults.settle.as_millis() as u64,
            defaults.deadline.as_millis() as u64,
        )
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
    session::send(&args.name, input, Duration::from_millis(args.pace_ms))?;
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
    let bytes = session::logs(&args.name, args.ansi)?;
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
        None => session::infer_name(&args.command)?,
    };
    let (cols, rows) = crossterm::terminal::size().unwrap_or((args.cols, args.rows));
    validate_terminal_size(cols, rows)?;
    let exit = session::run_foreground(
        &name,
        &args.command,
        args.cwd.as_deref(),
        args.record.as_deref(),
        &shot_engine::Options {
            cols,
            rows,
            cell_width: args.cell_width,
            cell_height: args.cell_height,
            max_bytes: args.max_bytes,
            opentui_host: matches!(args.host, Some(HostProfile::Opentui)),
            color: args.color.into(),
            ..shot_engine::Options::default()
        },
    )?;
    if exit.success {
        Ok(())
    } else {
        std::process::exit(i32::try_from(exit.code).unwrap_or(1).max(1));
    }
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
        ShotFormat::Semantic => unreachable!("show handles semantic output before reading source"),
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
    fn parses_flat_session_control_commands() {
        assert!(Cli::try_parse_from(["termctrl", "run", "editor", "--", "nvim", "."]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "status", "demo", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "list", "--all", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "prune", "--dry-run"]).is_ok());
        assert!(Cli::try_parse_from(["termctrl", "show", "demo", "--format", "semantic"]).is_ok());
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
    fn rejects_removed_workspace_commands() {
        assert!(Cli::try_parse_from(["termctrl", "attach", "demo"]).is_err());
        assert!(Cli::try_parse_from(["termctrl", "panes", "demo"]).is_err());
        assert!(Cli::try_parse_from(["termctrl", "run"]).is_err());
    }

    #[test]
    fn named_screen_reads_are_immediate_by_default() {
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
    }

    #[test]
    fn semantic_show_rejects_other_sources() {
        let cli = Cli::try_parse_from([
            "termctrl",
            "show",
            "demo",
            "--format",
            "semantic",
            "--",
            "ignored-command",
        ])
        .unwrap();
        let Command::Show(args) = cli.command else {
            panic!("expected show command");
        };

        assert_eq!(
            show_semantic(&args).unwrap_err().to_string(),
            "semantic output supports only NAME and --deadline-ms"
        );
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
    }

    #[test]
    fn paced_session_input_splits_text_without_splitting_keys() {
        assert_eq!(
            session_input(&["text:hi".to_owned(), "enter".to_owned()], true).unwrap(),
            vec![b"h".to_vec(), b"i".to_vec(), b"\r".to_vec()]
        );
    }
}
