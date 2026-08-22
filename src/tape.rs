use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use terminal_control::{session, shot};

use super::{mouse_click, mouse_drag, mouse_secondary_click, session_input};

const MAX_TAPE_BYTES: usize = 1024 * 1024;
const MAX_DURATION_MS: u64 = 10 * 60 * 1000;
const MAX_DIAGNOSTIC_CHARS: usize = 4000;
const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ACTION_OUTPUT_BYTES: usize = 64 * 1024;
const ACTION_DRAIN_BURST_BYTES: usize = 64 * 1024;
const ACTION_POLL: Duration = Duration::from_millis(10);
const ACTION_TERMINATION_GRACE: Duration = Duration::from_millis(250);
const ACTION_DRAIN_GRACE: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct Located<T> {
    line: usize,
    value: T,
}

#[derive(Debug)]
struct Tape {
    source: PathBuf,
    name: String,
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
    max_bytes: usize,
    cwd: PathBuf,
    cwd_line: Option<usize>,
    record: Option<PathBuf>,
    pointer_recording: bool,
    env: BTreeMap<String, String>,
    opentui_host: bool,
    color: shot::ColorMode,
    setup: Vec<Located<ActionSpec>>,
    launch: Located<Vec<String>>,
    steps: Vec<Step>,
    cleanup: Vec<Located<ActionSpec>>,
}

#[derive(Debug, PartialEq, Eq)]
struct ActionSpec {
    command: Vec<String>,
    timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitMatch {
    Substring,
    Line,
}

#[derive(Debug, Serialize)]
pub(super) struct PlayReceipt {
    pub status: &'static str,
    #[serde(rename = "tape")]
    pub source: PathBuf,
    pub session: String,
    pub recording: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Wait {
        line: usize,
        text: String,
        timeout: Duration,
        match_kind: WaitMatch,
    },
    Type {
        line: usize,
        text: String,
        pace: Duration,
    },
    Key {
        line: usize,
        keys: Vec<String>,
        pace: Duration,
    },
    Click {
        line: usize,
        x: u16,
        y: u16,
    },
    RightClick {
        line: usize,
        x: u16,
        y: u16,
    },
    Move {
        line: usize,
        to: (u16, u16),
        steps: u16,
        pace: Duration,
    },
    Drag {
        line: usize,
        from: (u16, u16),
        to: (u16, u16),
        steps: u16,
        pace: Duration,
    },
    Mark {
        line: usize,
        name: String,
    },
    Sleep {
        line: usize,
        duration: Duration,
    },
    Action {
        line: usize,
        action: ActionSpec,
    },
    Stop {
        line: usize,
    },
}

impl Step {
    fn line(&self) -> usize {
        match self {
            Self::Wait { line, .. }
            | Self::Type { line, .. }
            | Self::Key { line, .. }
            | Self::Click { line, .. }
            | Self::RightClick { line, .. }
            | Self::Move { line, .. }
            | Self::Drag { line, .. }
            | Self::Mark { line, .. }
            | Self::Sleep { line, .. }
            | Self::Action { line, .. }
            | Self::Stop { line } => *line,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Wait { .. } => "Wait",
            Self::Type { .. } => "Type",
            Self::Key { .. } => "Key",
            Self::Click { .. } => "Click",
            Self::RightClick { .. } => "RightClick",
            Self::Move { .. } => "Move",
            Self::Drag { .. } => "Drag",
            Self::Mark { .. } => "Mark",
            Self::Sleep { .. } => "Sleep",
            Self::Action { .. } => "Action",
            Self::Stop { .. } => "Stop",
        }
    }
}

pub(super) fn play(path: &Path, json_receipt: bool) -> Result<PlayReceipt> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("tape") {
        bail!(
            "tape source must use the .tape extension; .termctrl is reserved for recording output"
        );
    }
    let source = fs::canonicalize(path).with_context(|| format!("open tape {}", path.display()))?;
    let bytes = fs::read(&source).with_context(|| format!("read tape {}", source.display()))?;
    if bytes.len() > MAX_TAPE_BYTES {
        bail!("tape {} exceeds the 1 MiB source limit", source.display());
    }
    let text = String::from_utf8(bytes)
        .with_context(|| format!("tape {} is not valid UTF-8", source.display()))?;
    let tape = parse(&source, &text)?;
    tape.validate_paths()?;
    let receipt = PlayReceipt {
        status: "ok",
        source: tape.source.clone(),
        session: tape.name.clone(),
        recording: tape.record.clone(),
    };
    if json_receipt {
        receipt.validate_json_paths()?;
    }
    execute(tape)?;
    Ok(receipt)
}

impl PlayReceipt {
    fn validate_json_paths(&self) -> Result<()> {
        if self.source.to_str().is_none() {
            bail!(
                "play --json requires a UTF-8 tape path before lifecycle side effects: {}",
                self.source.display()
            );
        }
        if let Some(recording) = &self.recording
            && recording.to_str().is_none()
        {
            bail!(
                "play --json requires a UTF-8 recording path before lifecycle side effects: {}",
                recording.display()
            );
        }
        Ok(())
    }
}

impl Tape {
    fn validate_paths(&self) -> Result<()> {
        if !self.cwd.is_dir() {
            let line = self.cwd_line.unwrap_or(1);
            bail!(
                "{}:{line}: Cwd is not a directory: {}",
                self.source.display(),
                self.cwd.display()
            );
        }
        Ok(())
    }
}

fn parse(source: &Path, text: &str) -> Result<Tape> {
    if text.contains('\0') {
        bail!("{}:1: tape source contains a NUL byte", source.display());
    }
    let base = source.parent().unwrap_or_else(|| Path::new("."));
    let mut name: Option<Located<String>> = None;
    let mut viewport: Option<Located<(u16, u16)>> = None;
    let mut cell: Option<Located<(u16, u16)>> = None;
    let mut max_bytes: Option<Located<usize>> = None;
    let mut cwd: Option<Located<PathBuf>> = None;
    let mut record: Option<Located<PathBuf>> = None;
    let mut pointer: Option<Located<bool>> = None;
    let mut host: Option<Located<bool>> = None;
    let mut color: Option<Located<shot::ColorMode>> = None;
    let mut env = BTreeMap::new();
    let mut setup = Vec::new();
    let mut launch: Option<Located<Vec<String>>> = None;
    let mut steps = Vec::new();
    let mut cleanup = Vec::new();
    let mut stopped = false;
    let mut markers = HashSet::new();

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let tokens = lex_line(source, line, raw)?;
        if tokens.is_empty() {
            continue;
        }
        let command = tokens[0].as_str();
        if stopped {
            return line_error(source, line, "no commands are allowed after Stop");
        }
        let before_launch = launch.is_none();
        match command {
            "Session" => {
                require_header(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 1, "Session NAME")?;
                if !valid_session_name(&tokens[1]) {
                    return line_error(
                        source,
                        line,
                        "session names may contain only ASCII letters, digits, '.', '-', and '_'",
                    );
                }
                set_once(source, &mut name, line, "Session", tokens[1].clone())?;
            }
            "Viewport" => {
                require_header(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 2, "Viewport COLS ROWS")?;
                let cols = number::<u16>(source, line, &tokens[1], "viewport columns")?;
                let rows = number::<u16>(source, line, &tokens[2], "viewport rows")?;
                if cols == 0 || rows == 0 {
                    return line_error(
                        source,
                        line,
                        "viewport dimensions must be greater than zero",
                    );
                }
                set_once(source, &mut viewport, line, "Viewport", (cols, rows))?;
            }
            "Cell" => {
                require_header(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 2, "Cell WIDTH HEIGHT")?;
                let width = number::<u16>(source, line, &tokens[1], "cell width")?;
                let height = number::<u16>(source, line, &tokens[2], "cell height")?;
                if width == 0 || height == 0 {
                    return line_error(source, line, "cell dimensions must be greater than zero");
                }
                set_once(source, &mut cell, line, "Cell", (width, height))?;
            }
            "MaxBytes" => {
                require_header(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 1, "MaxBytes BYTES")?;
                let bytes = number::<usize>(source, line, &tokens[1], "MaxBytes")?;
                if bytes == 0 {
                    return line_error(source, line, "MaxBytes must be greater than zero");
                }
                set_once(source, &mut max_bytes, line, "MaxBytes", bytes)?;
            }
            "Cwd" => {
                require_header(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 1, "Cwd PATH")?;
                set_once(
                    source,
                    &mut cwd,
                    line,
                    "Cwd",
                    resolve_path(base, &tokens[1]),
                )?;
            }
            "Record" => {
                require_header(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 1, "Record FILE.termctrl")?;
                let path = PathBuf::from(&tokens[1]);
                if path.extension().and_then(|extension| extension.to_str()) != Some("termctrl") {
                    return line_error(
                        source,
                        line,
                        "Record output must use the .termctrl extension",
                    );
                }
                set_once(
                    source,
                    &mut record,
                    line,
                    "Record",
                    resolve_path(base, &tokens[1]),
                )?;
            }
            "Pointer" => {
                require_header(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 1, "Pointer on|off")?;
                let value = match tokens[1].as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return line_error(source, line, "Pointer must be 'on' or 'off'"),
                };
                set_once(source, &mut pointer, line, "Pointer", value)?;
            }
            "Env" => {
                require_header(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 2, "Env KEY VALUE")?;
                validate_env_key(source, line, &tokens[1])?;
                if env.insert(tokens[1].clone(), tokens[2].clone()).is_some() {
                    return line_error(source, line, format!("duplicate Env key {:?}", tokens[1]));
                }
            }
            "Host" => {
                require_header(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 1, "Host none|opentui")?;
                let value = match tokens[1].as_str() {
                    "none" => false,
                    "opentui" => true,
                    _ => return line_error(source, line, "Host must be 'none' or 'opentui'"),
                };
                set_once(source, &mut host, line, "Host", value)?;
            }
            "Color" => {
                require_header(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 1, "Color auto|always|never")?;
                let value = match tokens[1].as_str() {
                    "auto" => shot::ColorMode::Auto,
                    "always" => shot::ColorMode::Always,
                    "never" => shot::ColorMode::Never,
                    _ => {
                        return line_error(
                            source,
                            line,
                            "Color must be 'auto', 'always', or 'never'",
                        );
                    }
                };
                set_once(source, &mut color, line, "Color", value)?;
            }
            "Setup" => {
                require_header(source, line, command, before_launch)?;
                setup.push(Located {
                    line,
                    value: parse_action(source, line, &tokens, "Setup")?,
                });
            }
            "Cleanup" => {
                require_header(source, line, command, before_launch)?;
                cleanup.push(Located {
                    line,
                    value: parse_action(source, line, &tokens, "Cleanup")?,
                });
            }
            "Launch" => {
                require_header(source, line, command, before_launch)?;
                at_least_args(source, line, &tokens, 1, "Launch PROGRAM [ARG ...]")?;
                validate_argv(source, line, &tokens[1..])?;
                launch = Some(Located {
                    line,
                    value: tokens[1..].to_vec(),
                });
            }
            "Wait" => {
                require_step(source, line, command, before_launch)?;
                if tokens.len() < 2 {
                    return line_error(
                        source,
                        line,
                        "usage: Wait TEXT [Match substring|line] [Timeout DURATION]",
                    );
                }
                if tokens[1].is_empty() {
                    return line_error(source, line, "Wait text must not be empty");
                }
                let (match_kind, timeout) = wait_options(source, line, &tokens)?;
                steps.push(Step::Wait {
                    line,
                    text: tokens[1].clone(),
                    timeout,
                    match_kind,
                });
            }
            "Type" => {
                require_step(source, line, command, before_launch)?;
                let (values, pace) =
                    paced_values(source, line, &tokens, "Type TEXT [Pace DURATION]")?;
                if values.len() != 1 {
                    return line_error(source, line, "usage: Type TEXT [Pace DURATION]");
                }
                let text = values[0].clone();
                session_input(&[format!("text:{text}")], !pace.is_zero())
                    .map_err(|error| located_error(source, line, "invalid Type", error))?;
                steps.push(Step::Type { line, text, pace });
            }
            "Key" => {
                require_step(source, line, command, before_launch)?;
                let (keys, pace) =
                    paced_values(source, line, &tokens, "Key KEY [KEY ...] [Pace DURATION]")?;
                if keys.is_empty() {
                    return line_error(source, line, "usage: Key KEY [KEY ...] [Pace DURATION]");
                }
                session_input(&keys, !pace.is_zero())
                    .map_err(|error| located_error(source, line, "invalid Key", error))?;
                steps.push(Step::Key { line, keys, pace });
            }
            "Click" => {
                require_step(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 2, "Click X Y")?;
                let x = number::<u16>(source, line, &tokens[1], "click x")?;
                let y = number::<u16>(source, line, &tokens[2], "click y")?;
                mouse_click(x, y)
                    .map_err(|error| located_error(source, line, "invalid Click", error))?;
                steps.push(Step::Click { line, x, y });
            }
            "RightClick" => {
                require_step(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 2, "RightClick X Y")?;
                let x = number::<u16>(source, line, &tokens[1], "right-click x")?;
                let y = number::<u16>(source, line, &tokens[2], "right-click y")?;
                mouse_secondary_click(x, y)
                    .map_err(|error| located_error(source, line, "invalid RightClick", error))?;
                steps.push(Step::RightClick { line, x, y });
            }
            "Move" => {
                require_step(source, line, command, before_launch)?;
                if tokens.len() < 3 {
                    return line_error(source, line, "usage: Move X Y [Steps N] [Pace DURATION]");
                }
                let to = (
                    number::<u16>(source, line, &tokens[1], "move x")?,
                    number::<u16>(source, line, &tokens[2], "move y")?,
                );
                let (move_steps, pace) = motion_options(source, line, &tokens, 3, "Move")?;
                super::mouse_move(None, to, move_steps)
                    .map_err(|error| located_error(source, line, "invalid Move", error))?;
                steps.push(Step::Move {
                    line,
                    to,
                    steps: move_steps,
                    pace,
                });
            }
            "Drag" => {
                require_step(source, line, command, before_launch)?;
                if tokens.len() < 5 {
                    return line_error(
                        source,
                        line,
                        "usage: Drag FROM_X FROM_Y TO_X TO_Y [Steps N] [Pace DURATION]",
                    );
                }
                let from = (
                    number::<u16>(source, line, &tokens[1], "drag from x")?,
                    number::<u16>(source, line, &tokens[2], "drag from y")?,
                );
                let to = (
                    number::<u16>(source, line, &tokens[3], "drag to x")?,
                    number::<u16>(source, line, &tokens[4], "drag to y")?,
                );
                let (drag_steps, pace) = motion_options(source, line, &tokens, 5, "Drag")?;
                mouse_drag(from, to, drag_steps)
                    .map_err(|error| located_error(source, line, "invalid Drag", error))?;
                steps.push(Step::Drag {
                    line,
                    from,
                    to,
                    steps: drag_steps,
                    pace,
                });
            }
            "Mark" => {
                require_step(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 1, "Mark NAME")?;
                if tokens[1].is_empty() {
                    return line_error(source, line, "marker name must not be empty");
                }
                if !markers.insert(tokens[1].clone()) {
                    return line_error(source, line, format!("duplicate marker {:?}", tokens[1]));
                }
                steps.push(Step::Mark {
                    line,
                    name: tokens[1].clone(),
                });
            }
            "Sleep" => {
                require_step(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 1, "Sleep DURATION")?;
                steps.push(Step::Sleep {
                    line,
                    duration: duration(source, line, &tokens[1], false)?,
                });
            }
            "Action" => {
                require_step(source, line, command, before_launch)?;
                steps.push(Step::Action {
                    line,
                    action: parse_action(source, line, &tokens, "Action")?,
                });
            }
            "Stop" => {
                require_step(source, line, command, before_launch)?;
                exact_args(source, line, &tokens, 0, "Stop")?;
                steps.push(Step::Stop { line });
                stopped = true;
            }
            _ => {
                return line_error(source, line, format!("unknown tape command {command:?}"));
            }
        }
    }

    let name = required(source, name, "Session")?.value;
    let (cols, rows) = required(source, viewport, "Viewport")?.value;
    let launch = required(source, launch, "Launch")?;
    validate_mouse_coordinates(source, &steps, cols, rows)?;
    if !stopped {
        bail!("{}: missing required final Stop command", source.display());
    }
    if !markers.is_empty() && record.is_none() {
        bail!(
            "{}: Mark requires a Record FILE.termctrl directive before Launch",
            source.display()
        );
    }
    let pointer_recording = pointer.is_some_and(|value| value.value);
    if pointer_recording && record.is_none() {
        bail!(
            "{}: Pointer on requires a Record FILE.termctrl directive before Launch",
            source.display()
        );
    }
    let (cell_width, cell_height) = cell.map_or((9, 18), |value| value.value);
    let cwd_line = cwd.as_ref().map(|value| value.line);
    let cwd = cwd.map_or_else(|| base.to_path_buf(), |value| value.value);

    Ok(Tape {
        source: source.to_path_buf(),
        name,
        cols,
        rows,
        cell_width,
        cell_height,
        max_bytes: max_bytes.map_or(16 * 1024 * 1024, |value| value.value),
        cwd,
        cwd_line,
        record: record.map(|value| value.value),
        pointer_recording,
        env,
        opentui_host: host.is_some_and(|value| value.value),
        color: color.map_or(shot::ColorMode::Auto, |value| value.value),
        setup,
        launch,
        steps,
        cleanup,
    })
}

fn validate_mouse_coordinates(source: &Path, steps: &[Step], cols: u16, rows: u16) -> Result<()> {
    for step in steps {
        let coordinates: &[(&str, (u16, u16))] = match step {
            Step::Click { x, y, .. } => &[("Click", (*x, *y))],
            Step::RightClick { x, y, .. } => &[("RightClick", (*x, *y))],
            Step::Move { to, .. } => &[("Move", *to)],
            Step::Drag { from, to, .. } => &[("Drag start", *from), ("Drag end", *to)],
            _ => continue,
        };
        for (label, (x, y)) in coordinates {
            if *x >= cols || *y >= rows {
                return line_error(
                    source,
                    step.line(),
                    format!(
                        "{label} coordinate ({x}, {y}) is outside Viewport {cols}x{rows}; coordinates must satisfy X < {cols} and Y < {rows}"
                    ),
                );
            }
        }
    }
    Ok(())
}

fn execute(tape: Tape) -> Result<()> {
    let options = shot::Options {
        cols: tape.cols,
        rows: tape.rows,
        cell_width: tape.cell_width,
        cell_height: tape.cell_height,
        settle: Duration::ZERO,
        deadline: Duration::ZERO,
        input: Vec::new(),
        initial_delay: Duration::ZERO,
        wait_for: None,
        max_bytes: tape.max_bytes,
        opentui_host: tape.opentui_host,
        color: tape.color,
        env: tape.env.clone(),
        inherit_env: true,
        pointer_recording: tape.pointer_recording,
    };
    let mut primary = None;
    let mut additional = Vec::new();
    for setup in &tape.setup {
        if let Err(error) = run_action(&tape, &setup.value) {
            primary = Some(located_error(
                &tape.source,
                setup.line,
                "Setup failed",
                error,
            ));
            break;
        }
    }

    let mut owned = None;
    if primary.is_none() {
        match session::start(
            &tape.name,
            &tape.launch.value,
            Some(&tape.cwd),
            tape.record.as_deref(),
            &options,
        ) {
            Ok(()) => owned = Some(OwnedSession::new(tape.name.clone())),
            Err(error) => {
                primary = Some(located_error(
                    &tape.source,
                    tape.launch.line,
                    "Launch failed",
                    error,
                ));
            }
        }
    }

    if primary.is_none() {
        let owned = owned.as_mut().expect("successful launch owns a session");
        let mut pointer_position = None;
        for step in &tape.steps {
            if let Err(error) = execute_step(&tape, step, owned, &mut pointer_position) {
                primary = Some(located_error(
                    &tape.source,
                    step.line(),
                    format!("{} failed", step.name()),
                    error,
                ));
                break;
            }
        }
    }

    let mut cleanup_safe = true;
    if let Some(owned) = &mut owned
        && owned.active
        && let Err(error) = owned.stop()
    {
        additional.push(anyhow!(
            "failed to stop owned session {:?}: {error:#}",
            tape.name
        ));
        cleanup_safe = false;
    }
    if cleanup_safe {
        for cleanup in tape.cleanup.iter().rev() {
            if let Err(error) = run_action(&tape, &cleanup.value) {
                additional.push(located_error(
                    &tape.source,
                    cleanup.line,
                    "Cleanup failed",
                    error,
                ));
            }
        }
    } else if !tape.cleanup.is_empty() {
        additional.push(anyhow!(
            "skipped Cleanup because the owned session could not be confirmed stopped"
        ));
    }
    finish_lifecycle(primary, additional)
}

fn execute_step(
    tape: &Tape,
    step: &Step,
    owned: &mut OwnedSession,
    pointer_position: &mut Option<(u16, u16)>,
) -> Result<()> {
    match step {
        Step::Wait {
            text,
            timeout,
            match_kind,
            ..
        } => {
            let wait = match match_kind {
                WaitMatch::Substring => session::wait(&tape.name, text.clone(), *timeout),
                WaitMatch::Line => wait_for_line(&tape.name, text, *timeout),
            };
            if let Err(error) = wait {
                let screen = session::show(&tape.name, Duration::ZERO, Duration::ZERO)
                    .map(|shot| diagnostic_text(&shot.frame.text()))
                    .unwrap_or_else(|screen_error| {
                        format!("<screen unavailable: {screen_error:#}>")
                    });
                bail!(
                    "{error:#}\nlast visible screen after {}:\n{screen}",
                    display_duration(*timeout)
                );
            }
        }
        Step::Type { text, pace, .. } => {
            let input = session_input(&[format!("text:{text}")], !pace.is_zero())?;
            session::send(&tape.name, input, *pace)?;
        }
        Step::Key { keys, pace, .. } => {
            session::send(&tape.name, session_input(keys, !pace.is_zero())?, *pace)?;
        }
        Step::Click { x, y, .. } => {
            session::mouse_to_in(
                &tape.name,
                None,
                None,
                super::mouse_click_events(*x, *y)?,
                Duration::ZERO,
            )?;
            *pointer_position = Some((*x, *y));
        }
        Step::RightClick { x, y, .. } => {
            session::mouse_to_in(
                &tape.name,
                None,
                None,
                super::mouse_click_events_with_button(
                    *x,
                    *y,
                    terminal_control::recording::PointerButton::Secondary,
                )?,
                Duration::ZERO,
            )?;
            *pointer_position = Some((*x, *y));
        }
        Step::Move {
            to, steps, pace, ..
        } => {
            session::mouse_to_in(
                &tape.name,
                None,
                None,
                super::mouse_move_events(*pointer_position, *to, *steps)?,
                *pace,
            )?;
            *pointer_position = Some(*to);
        }
        Step::Drag {
            from,
            to,
            steps,
            pace,
            ..
        } => {
            session::mouse_to_in(
                &tape.name,
                None,
                None,
                super::mouse_drag_events(*from, *to, *steps)?,
                *pace,
            )?;
            *pointer_position = Some(*to);
        }
        Step::Mark { name, .. } => session::mark(&tape.name, name.clone())?,
        Step::Sleep { duration, .. } => thread::sleep(*duration),
        Step::Action { action, .. } => run_action(tape, action)?,
        Step::Stop { .. } => owned.stop()?,
    }
    Ok(())
}

fn wait_for_line(name: &str, text: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let screen = session::show(name, Duration::ZERO, Duration::ZERO)?;
        if screen.frame.text().lines().any(|line| line == text) {
            return Ok(());
        }
        if session::status(name)?.state == session::SessionState::Exited {
            bail!("session ended before a visible line exactly matched {text:?}");
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for a visible line exactly matching {text:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn finish_lifecycle(
    primary: Option<anyhow::Error>,
    mut additional: Vec<anyhow::Error>,
) -> Result<()> {
    let primary = match primary {
        Some(error) => error,
        None if additional.is_empty() => return Ok(()),
        None => additional.remove(0),
    };
    if additional.is_empty() {
        return Err(primary);
    }
    let detail = additional
        .iter()
        .map(|error| format!("- {error:#}"))
        .collect::<Vec<_>>()
        .join("\n");
    Err(anyhow!(
        "{primary:#}\nadditional lifecycle failures:\n{detail}"
    ))
}

fn run_action(tape: &Tape, action: &ActionSpec) -> Result<()> {
    #[cfg(not(unix))]
    bail!("tape actions require Unix process-group control");

    let command = &action.command;
    let mut process = Command::new(&command[0]);
    process
        .args(&command[1..])
        .current_dir(&tape.cwd)
        .envs(&tape.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        process.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = process
        .spawn()
        .with_context(|| format!("run host action argv {command:?}"))?;
    let mut stdout = child.stdout.take().context("capture host action stdout")?;
    let mut stderr = child.stderr.take().context("capture host action stderr")?;
    let nonblocking = set_nonblocking(&stdout)
        .context("bound host action stdout drain")
        .and_then(|()| set_nonblocking(&stderr).context("bound host action stderr drain"));
    if let Err(error) = nonblocking {
        let _ = terminate_action_group(child.id());
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let mut stdout_output = ActionOutput::default();
    let mut stderr_output = ActionOutput::default();
    let outcome = wait_for_action(
        &mut child,
        action.timeout,
        &mut stdout,
        &mut stderr,
        &mut stdout_output,
        &mut stderr_output,
    );
    if outcome.is_err() {
        let _ = terminate_action_group(child.id());
        let _ = child.kill();
        let _ = child.wait();
    }
    let (status, timed_out, drain) = outcome?;
    if timed_out {
        bail!(
            "host action argv {command:?} timed out after {}; output:\n{}",
            display_duration(action.timeout),
            action_output(&stdout_output, &stderr_output, drain)
        );
    }
    if drain.incomplete() {
        bail!(
            "host action argv {command:?} left output pipes open after {}; detached descendants may still be running; output:\n{}",
            display_duration(ACTION_DRAIN_GRACE),
            action_output(&stdout_output, &stderr_output, drain)
        );
    }
    if status.success() {
        return Ok(());
    }
    bail!(
        "host action argv {command:?} exited with {status}; output:\n{}",
        action_output(&stdout_output, &stderr_output, drain)
    )
}

#[derive(Default)]
struct ActionOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Clone, Copy)]
struct ActionDrain {
    stdout_open: bool,
    stderr_open: bool,
}

impl ActionDrain {
    fn incomplete(self) -> bool {
        self.stdout_open || self.stderr_open
    }
}

fn drain_action_output(reader: &mut impl Read, output: &mut ActionOutput) -> std::io::Result<bool> {
    let mut buffer = [0_u8; 8192];
    let mut drained = 0;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(false),
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let remaining = MAX_ACTION_OUTPUT_BYTES.saturating_sub(output.bytes.len());
        output
            .bytes
            .extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            output.truncated = true;
        }
        drained += read;
        if drained >= ACTION_DRAIN_BURST_BYTES {
            return Ok(true);
        }
    }
}

fn action_output(stdout: &ActionOutput, stderr: &ActionOutput, drain: ActionDrain) -> String {
    let mut sections = Vec::new();
    for (name, output) in [("stderr", stderr), ("stdout", stdout)] {
        if output.bytes.is_empty() && !output.truncated {
            continue;
        }
        let mut text = diagnostic_text(&String::from_utf8_lossy(&output.bytes));
        if output.truncated && !text.ends_with("<truncated>") {
            text.push_str("\n<truncated>");
        }
        sections.push(format!("{name}:\n{text}"));
    }
    if drain.incomplete() {
        let pipes = match (drain.stdout_open, drain.stderr_open) {
            (true, true) => "stdout and stderr",
            (true, false) => "stdout",
            (false, true) => "stderr",
            (false, false) => unreachable!(),
        };
        sections.push(format!(
            "drain:\n<{pipes} remained open after {}>",
            display_duration(ACTION_DRAIN_GRACE)
        ));
    }
    if sections.is_empty() {
        "<no output>".to_owned()
    } else {
        sections.join("\n")
    }
}

fn wait_for_action(
    child: &mut Child,
    timeout: Duration,
    stdout: &mut impl Read,
    stderr: &mut impl Read,
    stdout_output: &mut ActionOutput,
    stderr_output: &mut ActionOutput,
) -> Result<(ExitStatus, bool, ActionDrain)> {
    let deadline = Instant::now() + timeout;
    loop {
        let stdout_open =
            drain_action_output(stdout, stdout_output).context("drain host action stdout")?;
        let stderr_open =
            drain_action_output(stderr, stderr_output).context("drain host action stderr")?;
        if let Some(status) = child.try_wait().context("wait for host action")? {
            terminate_action_group(child.id())?;
            let drain = finish_action_drain(
                stdout,
                stderr,
                stdout_output,
                stderr_output,
                stdout_open,
                stderr_open,
            )?;
            return Ok((status, false, drain));
        }
        if Instant::now() >= deadline {
            terminate_action_group(child.id())?;
            let status = child.wait().context("reap timed-out host action")?;
            let drain = finish_action_drain(
                stdout,
                stderr,
                stdout_output,
                stderr_output,
                stdout_open,
                stderr_open,
            )?;
            return Ok((status, true, drain));
        }
        thread::sleep(ACTION_POLL);
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_action_drain(
    stdout: &mut impl Read,
    stderr: &mut impl Read,
    stdout_output: &mut ActionOutput,
    stderr_output: &mut ActionOutput,
    mut stdout_open: bool,
    mut stderr_open: bool,
) -> Result<ActionDrain> {
    let deadline = Instant::now() + ACTION_DRAIN_GRACE;
    while stdout_open || stderr_open {
        if stdout_open {
            stdout_open = drain_action_output(stdout, stdout_output)
                .context("finish host action stdout drain")?;
        }
        if stderr_open {
            stderr_open = drain_action_output(stderr, stderr_output)
                .context("finish host action stderr drain")?;
        }
        if !stdout_open && !stderr_open || Instant::now() >= deadline {
            break;
        }
        thread::sleep(ACTION_POLL);
    }
    Ok(ActionDrain {
        stdout_open,
        stderr_open,
    })
}

#[cfg(unix)]
fn set_nonblocking(file: &impl AsRawFd) -> std::io::Result<()> {
    let descriptor = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_nonblocking(_file: &impl Read) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "tape actions require Unix process-group control",
    ))
}

#[cfg(unix)]
fn terminate_action_group(id: u32) -> Result<()> {
    let group = id as libc::pid_t;
    if !process_group_exists(group)? {
        return Ok(());
    }
    signal_process_group(group, libc::SIGTERM)?;
    let deadline = Instant::now() + ACTION_TERMINATION_GRACE;
    while Instant::now() < deadline {
        if !process_group_exists(group)? {
            return Ok(());
        }
        thread::sleep(ACTION_POLL);
    }
    signal_process_group(group, libc::SIGKILL)?;
    Ok(())
}

#[cfg(unix)]
fn process_group_exists(group: libc::pid_t) -> Result<bool> {
    if unsafe { libc::killpg(group, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error).context("inspect host action process group")
    }
}

#[cfg(unix)]
fn signal_process_group(group: libc::pid_t, signal: libc::c_int) -> Result<()> {
    if unsafe { libc::killpg(group, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error).context("terminate host action process group")
    }
}

#[cfg(not(unix))]
fn terminate_action_group(_id: u32) -> Result<()> {
    bail!("tape actions require Unix process-group control")
}

struct OwnedSession {
    name: String,
    active: bool,
}

impl OwnedSession {
    fn new(name: String) -> Self {
        Self { name, active: true }
    }

    fn stop(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        session::stop(&self.name)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for OwnedSession {
    fn drop(&mut self) {
        if self.active {
            let _ = session::stop(&self.name);
        }
    }
}

fn lex_line(source: &Path, line: usize, input: &str) -> Result<Vec<String>> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        if index == chars.len() || chars[index] == '#' {
            break;
        }
        let mut token = String::new();
        let mut started = false;
        let mut comment = false;
        while index < chars.len() {
            match chars[index] {
                value if value.is_whitespace() => break,
                '#' => {
                    comment = true;
                    break;
                }
                '\'' | '"' => {
                    let quote = chars[index];
                    let column = index + 1;
                    started = true;
                    index += 1;
                    let mut closed = false;
                    while index < chars.len() {
                        let value = chars[index];
                        if value == quote {
                            closed = true;
                            index += 1;
                            break;
                        }
                        if value == '\\' && quote == '"' {
                            index += 1;
                            if index == chars.len() {
                                return line_error(
                                    source,
                                    line,
                                    "trailing escape in double-quoted text",
                                );
                            }
                            token.push(escaped(source, line, chars[index])?);
                            index += 1;
                            continue;
                        }
                        token.push(value);
                        index += 1;
                    }
                    if !closed {
                        return line_error(
                            source,
                            line,
                            format!("unterminated {quote} quote beginning at column {column}"),
                        );
                    }
                }
                '\\' => {
                    started = true;
                    index += 1;
                    if index == chars.len() {
                        return line_error(source, line, "trailing escape in unquoted text");
                    }
                    token.push(chars[index]);
                    index += 1;
                }
                value => {
                    started = true;
                    token.push(value);
                    index += 1;
                }
            }
        }
        if started {
            tokens.push(token);
        }
        if comment {
            break;
        }
    }
    Ok(tokens)
}

fn escaped(source: &Path, line: usize, value: char) -> Result<char> {
    match value {
        '\\' => Ok('\\'),
        '"' => Ok('"'),
        '\'' => Ok('\''),
        'n' => Ok('\n'),
        'r' => Ok('\r'),
        't' => Ok('\t'),
        other => line_error(
            source,
            line,
            format!("unsupported escape \\{other}; use \\\\, \\\", \\n, \\r, or \\t"),
        ),
    }
}

fn paced_values(
    source: &Path,
    line: usize,
    tokens: &[String],
    usage: &str,
) -> Result<(Vec<String>, Duration)> {
    if tokens.len() < 2 {
        return line_error(source, line, format!("usage: {usage}"));
    }
    if tokens.len() >= 3 && tokens[tokens.len() - 2] == "Pace" {
        let pace = duration(source, line, &tokens[tokens.len() - 1], true)?;
        return Ok((tokens[1..tokens.len() - 2].to_vec(), pace));
    }
    Ok((tokens[1..].to_vec(), Duration::ZERO))
}

fn wait_options(source: &Path, line: usize, tokens: &[String]) -> Result<(WaitMatch, Duration)> {
    let mut match_kind = WaitMatch::Substring;
    let mut timeout = Duration::from_secs(5);
    let mut saw_match = false;
    let mut saw_timeout = false;
    let mut option = 2;
    while option < tokens.len() {
        if option + 1 >= tokens.len() {
            return line_error(source, line, "Wait options require a value");
        }
        match tokens[option].as_str() {
            "Match" if !saw_match => {
                match_kind = match tokens[option + 1].as_str() {
                    "substring" => WaitMatch::Substring,
                    "line" => WaitMatch::Line,
                    _ => {
                        return line_error(
                            source,
                            line,
                            "Wait Match must be 'substring' or 'line'",
                        );
                    }
                };
                saw_match = true;
            }
            "Timeout" if !saw_timeout => {
                timeout = duration(source, line, &tokens[option + 1], false)?;
                saw_timeout = true;
            }
            "Match" => return line_error(source, line, "duplicate Wait Match option"),
            "Timeout" => return line_error(source, line, "duplicate Wait Timeout option"),
            other => return line_error(source, line, format!("unknown Wait option {other:?}")),
        }
        option += 2;
    }
    Ok((match_kind, timeout))
}

fn motion_options(
    source: &Path,
    line: usize,
    tokens: &[String],
    mut option: usize,
    command: &str,
) -> Result<(u16, Duration)> {
    let mut steps = 10;
    let mut pace = Duration::from_millis(8);
    let mut saw_steps = false;
    let mut saw_pace = false;
    while option < tokens.len() {
        if option + 1 >= tokens.len() {
            return line_error(source, line, format!("{command} options require a value"));
        }
        match tokens[option].as_str() {
            "Steps" if !saw_steps => {
                steps = number::<u16>(
                    source,
                    line,
                    &tokens[option + 1],
                    &format!("{} steps", command.to_ascii_lowercase()),
                )?;
                if !(1..=1000).contains(&steps) {
                    return line_error(
                        source,
                        line,
                        format!(
                            "{} Steps must be between 1 and 1000",
                            command.to_ascii_lowercase()
                        ),
                    );
                }
                saw_steps = true;
            }
            "Pace" if !saw_pace => {
                pace = duration(source, line, &tokens[option + 1], true)?;
                saw_pace = true;
            }
            "Steps" => {
                return line_error(source, line, format!("duplicate {command} Steps option"));
            }
            "Pace" => {
                return line_error(source, line, format!("duplicate {command} Pace option"));
            }
            other => {
                return line_error(source, line, format!("unknown {command} option {other:?}"));
            }
        }
        option += 2;
    }
    Ok((steps, pace))
}

fn parse_action(source: &Path, line: usize, tokens: &[String], name: &str) -> Result<ActionSpec> {
    at_least_args(
        source,
        line,
        tokens,
        1,
        &format!("{name} PROGRAM [ARG ...] [Timeout DURATION]"),
    )?;
    if tokens.last().is_some_and(|token| token == "Timeout") {
        return line_error(
            source,
            line,
            format!("usage: {name} PROGRAM [ARG ...] [Timeout DURATION]"),
        );
    }
    let (command, timeout) = if tokens.len() >= 4 && tokens[tokens.len() - 2] == "Timeout" {
        (
            tokens[1..tokens.len() - 2].to_vec(),
            duration(source, line, &tokens[tokens.len() - 1], false)?,
        )
    } else {
        (tokens[1..].to_vec(), DEFAULT_ACTION_TIMEOUT)
    };
    validate_argv(source, line, &command)?;
    Ok(ActionSpec { command, timeout })
}

fn duration(source: &Path, line: usize, value: &str, allow_zero: bool) -> Result<Duration> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    if number.is_empty() || unit.is_empty() {
        return line_error(
            source,
            line,
            format!("invalid duration {value:?}; use 250ms, 2s, or 1m"),
        );
    }
    let number = number
        .parse::<u64>()
        .map_err(|_| anyhow!("invalid duration number"))
        .map_err(|error| {
            located_error(source, line, format!("invalid duration {value:?}"), error)
        })?;
    let multiplier = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        _ => {
            return line_error(
                source,
                line,
                format!("invalid duration unit in {value:?}; use ms, s, or m"),
            );
        }
    };
    let milliseconds = number
        .checked_mul(multiplier)
        .filter(|value| *value <= MAX_DURATION_MS)
        .ok_or_else(|| anyhow!("{}:{line}: duration exceeds 10 minutes", source.display()))?;
    if milliseconds == 0 && !allow_zero {
        return line_error(source, line, "duration must be greater than zero");
    }
    Ok(Duration::from_millis(milliseconds))
}

fn display_duration(duration: Duration) -> String {
    format!("{}ms", duration.as_millis())
}

fn diagnostic_text(value: &str) -> String {
    let mut output: String = value.chars().take(MAX_DIAGNOSTIC_CHARS).collect();
    if value.chars().count() > MAX_DIAGNOSTIC_CHARS {
        output.push_str("\n<truncated>");
    }
    if output.trim().is_empty() {
        "<empty>".to_owned()
    } else {
        output
    }
}

fn resolve_path(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn validate_env_key(source: &Path, line: usize, key: &str) -> Result<()> {
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        return line_error(
            source,
            line,
            "Env keys must be non-empty and cannot contain '=' or NUL",
        );
    }
    Ok(())
}

fn validate_argv(source: &Path, line: usize, command: &[String]) -> Result<()> {
    if command[0].is_empty() {
        return line_error(source, line, "program name must not be empty");
    }
    if command.iter().any(|value| value.contains('\0')) {
        return line_error(source, line, "argv values cannot contain NUL");
    }
    Ok(())
}

fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn number<T>(source: &Path, line: usize, value: &str, label: &str) -> Result<T>
where
    T: std::str::FromStr,
{
    value
        .parse()
        .map_err(|_| anyhow!("{}:{line}: invalid {label} {value:?}", source.display()))
}

fn require_header(source: &Path, line: usize, command: &str, before_launch: bool) -> Result<()> {
    if !before_launch {
        return line_error(source, line, format!("{command} must appear before Launch"));
    }
    Ok(())
}

fn require_step(source: &Path, line: usize, command: &str, before_launch: bool) -> Result<()> {
    if before_launch {
        return line_error(
            source,
            line,
            format!("{command} requires a preceding Launch"),
        );
    }
    Ok(())
}

fn exact_args(
    source: &Path,
    line: usize,
    tokens: &[String],
    count: usize,
    usage: &str,
) -> Result<()> {
    if tokens.len() != count + 1 {
        return line_error(source, line, format!("usage: {usage}"));
    }
    Ok(())
}

fn at_least_args(
    source: &Path,
    line: usize,
    tokens: &[String],
    count: usize,
    usage: &str,
) -> Result<()> {
    if tokens.len() < count + 1 {
        return line_error(source, line, format!("usage: {usage}"));
    }
    Ok(())
}

fn set_once<T>(
    source: &Path,
    target: &mut Option<Located<T>>,
    line: usize,
    name: &str,
    value: T,
) -> Result<()> {
    if let Some(previous) = target {
        return line_error(
            source,
            line,
            format!("duplicate {name}; first declared on line {}", previous.line),
        );
    }
    *target = Some(Located { line, value });
    Ok(())
}

fn required<T>(source: &Path, value: Option<Located<T>>, name: &str) -> Result<Located<T>> {
    value.ok_or_else(|| anyhow!("{}: missing required {name} directive", source.display()))
}

fn located_error(
    source: &Path,
    line: usize,
    message: impl std::fmt::Display,
    error: anyhow::Error,
) -> anyhow::Error {
    error.context(format!("{}:{line}: {message}", source.display()))
}

fn line_error<T>(source: &Path, line: usize, message: impl std::fmt::Display) -> Result<T> {
    Err(anyhow!("{}:{line}: {message}", source.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_TAPE: &str = r#"
# source comments stay out of quoted text
Session "demo-session"
Viewport 112 34
Cell 9 18
MaxBytes 4096
Cwd "."
Env DEMO_VALUE "quoted # value\nnext"
Record "artifacts/demo.termctrl"
Host opentui
Color always
Setup "/usr/bin/touch" "fixture ready" Timeout 2s
Cleanup "/usr/bin/rm" "-f" "fixture ready"
Launch "demo-app" "--mode" 'literal # argument'

Wait "Ready #1" Match line Timeout 2s
Type "hello \"tape\"" Pace 35ms
Key ctrl-p down shift+enter Pace 10ms
Move 4 2 Steps 8 Pace 0ms
Move 8 2 Steps 2 Pace 4ms
Click 12 4
RightClick 13 4
Drag 12 4 30 4 Pace 0ms Steps 6
Move 34 4 Steps 2 Pace 0ms
Mark "after-input"
Sleep 250ms
Action "/usr/bin/touch" "fixture ready" Timeout 3s
Stop
"#;

    #[test]
    fn parses_complete_state_aware_tape() {
        let tape = parse(Path::new("/tmp/demo.tape"), FULL_TAPE).unwrap();

        assert_eq!(tape.name, "demo-session");
        assert_eq!((tape.cols, tape.rows), (112, 34));
        assert_eq!((tape.cell_width, tape.cell_height), (9, 18));
        assert_eq!(tape.max_bytes, 4096);
        assert_eq!(tape.env["DEMO_VALUE"], "quoted # value\nnext");
        assert!(tape.opentui_host);
        assert_eq!(tape.color, shot::ColorMode::Always);
        assert_eq!(
            tape.launch.value,
            ["demo-app", "--mode", "literal # argument"]
        );
        assert_eq!(tape.setup.len(), 1);
        assert_eq!(tape.setup[0].value.timeout, Duration::from_secs(2));
        assert_eq!(tape.cleanup.len(), 1);
        assert_eq!(tape.cleanup[0].value.timeout, DEFAULT_ACTION_TIMEOUT);
        assert_eq!(tape.steps.len(), 13);
        assert!(matches!(
            &tape.steps[0],
            Step::Wait {
                match_kind: WaitMatch::Line,
                timeout,
                ..
            } if *timeout == Duration::from_secs(2)
        ));
        assert!(matches!(
            &tape.steps[1],
            Step::Type { text, pace, .. }
                if text == "hello \"tape\"" && *pace == Duration::from_millis(35)
        ));
        assert!(matches!(
            &tape.steps[4],
            Step::Move { to, steps, pace, .. }
                if *to == (8, 2) && *steps == 2 && *pace == Duration::from_millis(4)
        ));
        assert!(matches!(
            &tape.steps[7],
            Step::Drag { steps, pace, .. }
                if *steps == 6 && pace.is_zero()
        ));
        assert!(matches!(
            &tape.steps[11],
            Step::Action { action, .. } if action.timeout == Duration::from_secs(3)
        ));
    }

    #[test]
    fn reports_quote_and_validation_errors_at_the_source_line() {
        let error = parse(
            Path::new("broken.tape"),
            "Session demo\nViewport 80 24\nLaunch \"unterminated\nStop\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("broken.tape:3: unterminated"));

        let error = parse(
            Path::new("broken.tape"),
            "Session demo\nViewport 80 24\nLaunch app\nMark done\nStop\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("Mark requires a Record"));
    }

    #[test]
    fn validates_every_command_before_execution() {
        let error = parse(
            Path::new("broken.tape"),
            "Session demo\nViewport 80 24\nLaunch app\nAction touch changed\nStop extra\n",
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "broken.tape:5: usage: Stop");
    }

    #[test]
    fn wait_defaults_to_substring_and_validates_match_modifiers() {
        let tape = parse(
            Path::new("demo.tape"),
            "Session demo\nViewport 80 24\nLaunch app\nWait ready Timeout 2s\nStop\n",
        )
        .unwrap();
        assert!(matches!(
            &tape.steps[0],
            Step::Wait {
                match_kind: WaitMatch::Substring,
                timeout,
                ..
            } if *timeout == Duration::from_secs(2)
        ));

        for invalid in [
            "Wait ready Match exact",
            "Wait ready Match line Match substring",
            "Wait ready Timeout 1s Timeout 2s",
            "Wait ready Unknown line",
        ] {
            let source = format!("Session demo\nViewport 80 24\nLaunch app\n{invalid}\nStop\n");
            assert!(parse(Path::new("demo.tape"), &source).is_err(), "{invalid}");
        }
    }

    #[test]
    fn rejects_invalid_durations_keys_and_post_stop_commands() {
        assert!(duration(Path::new("demo.tape"), 2, "1.5s", false).is_err());
        assert!(duration(Path::new("demo.tape"), 2, "11m", false).is_err());
        assert!(
            parse(
                Path::new("demo.tape"),
                "Session demo\nViewport 80 24\nLaunch app\nKey unsupported\nStop\n"
            )
            .is_err()
        );
        assert!(
            parse(
                Path::new("demo.tape"),
                "Session demo\nViewport 80 24\nLaunch app\nMove 1 2 Steps 0\nStop\n"
            )
            .is_err()
        );
        assert!(
            parse(
                Path::new("demo.tape"),
                "Session demo\nViewport 80 24\nLaunch app\nMove 1 2 Pace 1ms Pace 2ms\nStop\n"
            )
            .is_err()
        );
        assert!(
            parse(
                Path::new("demo.tape"),
                "Session demo\nViewport 80 24\nLaunch app\nStop\nSleep 1s\n"
            )
            .is_err()
        );
        assert!(
            parse(
                Path::new("demo.tape"),
                "Session demo\nViewport 80 24\nSetup touch Timeout nope\nLaunch app\nStop\n"
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_every_mouse_endpoint_outside_the_declared_viewport() {
        for (line, label) in [
            ("Click 2 0", "Click coordinate (2, 0)"),
            ("RightClick 0 1", "RightClick coordinate (0, 1)"),
            ("Move 2 0", "Move coordinate (2, 0)"),
            ("Drag 0 0 2 0", "Drag end coordinate (2, 0)"),
            ("Drag 0 1 1 0", "Drag start coordinate (0, 1)"),
        ] {
            let source = format!("Session demo\nViewport 2 1\nLaunch app\n{line}\nStop\n");
            let error = parse(Path::new("bounds.tape"), &source).unwrap_err();
            let message = error.to_string();
            assert!(message.contains(label), "{line}: {message}");
            assert!(
                message.contains("outside Viewport 2x1"),
                "{line}: {message}"
            );
        }
    }
}
