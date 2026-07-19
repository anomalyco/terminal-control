use std::io::{Stdout, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::frame::{
    Attributes, Cell, Cursor, DEFAULT_BACKGROUND, DEFAULT_FOREGROUND, FORMAT_VERSION, Frame,
};
use crate::session::{Session, SessionLaunch, SessionState, SessionStatus};
use crate::shot::{Options, Shot};
use crate::terminal_core::InputModes;

pub type PaneId = u32;

const DIVIDER: &str = "│";
const PREFIX: u8 = 0x02;

pub(crate) struct Workspace {
    panes: Vec<Pane>,
    active: usize,
    next_id: PaneId,
    cols: u16,
    rows: u16,
    cwd: PathBuf,
    shell: Vec<String>,
    options: Options,
    launch: SessionLaunch,
    stopped: bool,
}

struct Pane {
    id: PaneId,
    session: Session,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PaneStatus {
    pub id: PaneId,
    pub active: bool,
    pub state: SessionState,
    pub cols: u16,
    pub rows: u16,
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaneRect {
    x: u16,
    cols: u16,
    rows: u16,
}

impl Workspace {
    pub(crate) fn start(
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
    ) -> Result<Self> {
        let cwd = cwd
            .map(Path::to_owned)
            .unwrap_or(std::env::current_dir().context("resolve workspace directory")?);
        let shell = shell_command();
        let command = if command.is_empty() { &shell } else { command };
        let mut session = Session::start(command, Some(&cwd), record, options)?;
        let launch = session.status()?.launch;
        Ok(Self {
            panes: vec![Pane { id: 0, session }],
            active: 0,
            next_id: 1,
            cols: options.cols,
            rows: options.rows,
            cwd,
            shell,
            options: options.clone(),
            launch,
            stopped: false,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    pub(crate) fn pump(&mut self) -> Result<()> {
        for pane in &mut self.panes {
            pane.session.pump()?;
        }
        Ok(())
    }

    pub(crate) fn remove_exited(&mut self) -> Result<bool> {
        let mut removed = false;
        let mut index = 0;
        while index < self.panes.len() {
            if self.panes[index].session.status()?.state == SessionState::Exited {
                self.panes.remove(index);
                removed = true;
                if self.active >= self.panes.len() {
                    self.active = self.panes.len().saturating_sub(1);
                }
            } else {
                index += 1;
            }
        }
        if removed && !self.panes.is_empty() {
            self.apply_layout()?;
        }
        Ok(removed)
    }

    pub(crate) fn split_right(&mut self) -> Result<()> {
        if self.panes.len() >= 2 {
            bail!("the initial workspace supports at most two panes");
        }
        let rects = layout(self.cols, self.rows, 2)?;
        let mut options = self.options.clone();
        options.cols = rects[1].cols;
        options.rows = rects[1].rows;
        let session = Session::start(&self.shell, Some(&self.cwd), None, &options)?;
        self.panes[0].session.resize(
            rects[0].cols,
            rects[0].rows,
            self.options.cell_width,
            self.options.cell_height,
        )?;
        self.panes.push(Pane {
            id: self.next_id,
            session,
        });
        self.next_id += 1;
        self.active = 1;
        Ok(())
    }

    pub(crate) fn focus_left(&mut self) -> bool {
        if self.active == 0 {
            return false;
        }
        self.active = 0;
        true
    }

    pub(crate) fn focus_right(&mut self) -> bool {
        if self.panes.len() < 2 || self.active == 1 {
            return false;
        }
        self.active = 1;
        true
    }

    pub(crate) fn close_active(&mut self) -> Result<()> {
        if self.panes.is_empty() {
            return Ok(());
        }
        let mut pane = self.panes.remove(self.active);
        pane.session.stop()?;
        if self.panes.is_empty() {
            self.stopped = true;
        }
        if self.active >= self.panes.len() {
            self.active = self.panes.len().saturating_sub(1);
        }
        if !self.panes.is_empty() {
            self.apply_layout()?;
        }
        Ok(())
    }

    pub(crate) fn send(&mut self, pane: Option<PaneId>, input: &[u8]) -> Result<()> {
        let index = self.resolve_pane(pane)?;
        self.panes[index].session.send(input)
    }

    pub(crate) fn send_all(
        &mut self,
        pane: Option<PaneId>,
        input: &[Vec<u8>],
        pace: std::time::Duration,
        mut tick: impl FnMut(&mut Self) -> Result<bool>,
    ) -> Result<()> {
        let target = self.panes[self.resolve_pane(pane)?].id;
        for (index, bytes) in input.iter().enumerate() {
            self.send(Some(target), bytes)?;
            if index + 1 < input.len() {
                if pace.is_zero() {
                    if !tick(self)? {
                        bail!("workspace ended while sending input");
                    }
                } else {
                    self.wait_with_ticks(pace, &mut tick)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> Result<()> {
        if self.cols == cols
            && self.rows == rows
            && self.options.cell_width == cell_width
            && self.options.cell_height == cell_height
        {
            return Ok(());
        }
        if self.panes.len() == 2 && cols < 3 {
            return Ok(());
        }
        let rects = layout(cols, rows, self.panes.len())?;
        self.cols = cols;
        self.rows = rows;
        self.options.cols = cols;
        self.options.rows = rows;
        self.options.cell_width = cell_width;
        self.options.cell_height = cell_height;
        self.apply_rects(&rects)
    }

    pub(crate) fn frame(&mut self) -> Result<Frame> {
        let rects = layout(self.cols, self.rows, self.panes.len())?;
        let mut frames = Vec::with_capacity(self.panes.len());
        for pane in &mut self.panes {
            frames.push(pane.session.frame()?);
        }
        Ok(compose(self.cols, self.rows, &rects, &frames, self.active))
    }

    pub(crate) fn shot(&mut self, pane: Option<PaneId>) -> Result<Shot> {
        if let Some(pane) = pane {
            let index = self.resolve_pane(Some(pane))?;
            return self.panes[index].session.snapshot();
        }
        let frame = self.frame()?;
        let ansi = frame_ansi(&frame)?;
        Ok(Shot { frame, ansi })
    }

    pub(crate) fn capture(
        &mut self,
        pane: Option<PaneId>,
        settle: Duration,
        deadline: Duration,
        mut tick: impl FnMut(&mut Self) -> Result<bool>,
    ) -> Result<Shot> {
        let started = Instant::now();
        let deadline = started + deadline;
        loop {
            if !tick(self)? {
                bail!("workspace ended before capture completed");
            }
            let idle = match pane {
                Some(pane) => {
                    let index = self.resolve_pane(Some(pane))?;
                    self.panes[index]
                        .session
                        .status()?
                        .idle_for_ms
                        .map_or_else(|| started.elapsed(), Duration::from_millis)
                }
                None => {
                    let mut idle = started.elapsed();
                    for pane in &mut self.panes {
                        let pane_idle = pane
                            .session
                            .status()?
                            .idle_for_ms
                            .map_or_else(|| started.elapsed(), Duration::from_millis);
                        idle = idle.min(pane_idle);
                    }
                    idle
                }
            };
            if settle.is_zero() || idle >= settle || Instant::now() >= deadline {
                return self.shot(pane);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn panes(&mut self) -> Result<Vec<PaneStatus>> {
        let mut statuses = Vec::with_capacity(self.panes.len());
        for (index, pane) in self.panes.iter_mut().enumerate() {
            let status = pane.session.status()?;
            statuses.push(PaneStatus {
                id: pane.id,
                active: index == self.active,
                state: status.state,
                cols: status.cols,
                rows: status.rows,
                command: status.launch.command,
                cwd: status.launch.cwd,
            });
        }
        Ok(statuses)
    }

    pub(crate) fn status(&mut self) -> Result<SessionStatus> {
        let active = self.resolve_pane(None)?;
        let mut statuses = Vec::with_capacity(self.panes.len());
        for pane in &mut self.panes {
            statuses.push(pane.session.status()?);
        }
        let mut status = statuses[active].clone();
        status.cols = self.cols;
        status.rows = self.rows;
        status.idle_for_ms = statuses
            .iter()
            .filter_map(|status| status.idle_for_ms)
            .min();
        status.has_visible_content = statuses.iter().any(|status| status.has_visible_content);
        status.recording = statuses.iter().any(|status| status.recording);
        status.logs_truncated = statuses.iter().any(|status| status.logs_truncated);
        status.launch = self.launch.clone();
        Ok(status)
    }

    pub(crate) fn active_input_modes(&self) -> Result<InputModes> {
        let index = self.resolve_pane(None)?;
        self.panes[index].session.input_modes()
    }

    pub(crate) fn wait_for_text(
        &mut self,
        pane: Option<PaneId>,
        text: &str,
        timeout: std::time::Duration,
        mut tick: impl FnMut(&mut Self) -> Result<bool>,
    ) -> Result<()> {
        let target = self.panes[self.resolve_pane(pane)?].id;
        let deadline = Instant::now() + timeout;
        loop {
            if !tick(self)? {
                bail!("workspace ended before visible terminal included {text:?}");
            }
            let index = match self.resolve_pane(Some(target)) {
                Ok(index) => index,
                Err(_) => bail!("pane {target} ended before visible terminal included {text:?}"),
            };
            if self.panes[index].session.frame()?.text().contains(text) {
                return Ok(());
            }
            if self.panes[index].session.status()?.state == SessionState::Exited {
                bail!("pane ended before visible terminal included {text:?}");
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for visible terminal text {text:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_with_ticks(
        &mut self,
        duration: Duration,
        tick: &mut impl FnMut(&mut Self) -> Result<bool>,
    ) -> Result<()> {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            if !tick(self)? {
                bail!("workspace ended while sending input");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                std::thread::sleep(Duration::from_millis(10).min(remaining));
            }
        }
        Ok(())
    }

    pub(crate) fn active_logs(&mut self, ansi: bool) -> Result<Vec<u8>> {
        let index = self.resolve_pane(None)?;
        self.panes[index].session.logs(ansi)
    }

    pub(crate) fn mark_recording(&mut self, name: &str) -> Result<()> {
        for pane in &mut self.panes {
            if pane.session.status()?.recording {
                return pane.session.mark(name);
            }
        }
        bail!("workspace is not recording")
    }

    pub(crate) fn stop(&mut self) {
        self.stopped = true;
        for pane in &mut self.panes {
            let _ = pane.session.stop();
        }
        self.panes.clear();
    }

    pub(crate) fn was_stopped(&self) -> bool {
        self.stopped
    }

    fn apply_layout(&mut self) -> Result<()> {
        let rects = layout(self.cols, self.rows, self.panes.len())?;
        self.apply_rects(&rects)
    }

    fn apply_rects(&mut self, rects: &[PaneRect]) -> Result<()> {
        for (pane, rect) in self.panes.iter_mut().zip(rects) {
            pane.session.resize(
                rect.cols,
                rect.rows,
                self.options.cell_width,
                self.options.cell_height,
            )?;
        }
        Ok(())
    }

    fn resolve_pane(&self, pane: Option<PaneId>) -> Result<usize> {
        match pane {
            Some(id) => self
                .panes
                .iter()
                .position(|pane| pane.id == id)
                .ok_or_else(|| anyhow::anyhow!("workspace has no pane {id}")),
            None => self
                .panes
                .get(self.active)
                .map(|_| self.active)
                .ok_or_else(|| anyhow::anyhow!("workspace has no active pane")),
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.stop();
    }
}

fn shell_command() -> Vec<String> {
    vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned())]
}

fn layout(cols: u16, rows: u16, panes: usize) -> Result<Vec<PaneRect>> {
    match panes {
        0 => Ok(Vec::new()),
        1 => Ok(vec![PaneRect { x: 0, cols, rows }]),
        2 if cols >= 3 => {
            let left = (cols - 1) / 2;
            Ok(vec![
                PaneRect {
                    x: 0,
                    cols: left,
                    rows,
                },
                PaneRect {
                    x: left + 1,
                    cols: cols - left - 1,
                    rows,
                },
            ])
        }
        2 => bail!("workspace is too narrow to split"),
        _ => bail!("the initial workspace supports at most two panes"),
    }
}

fn compose(cols: u16, rows: u16, rects: &[PaneRect], frames: &[Frame], active: usize) -> Frame {
    let foreground = frames
        .get(active)
        .map_or(DEFAULT_FOREGROUND, |frame| frame.foreground);
    let background = frames
        .get(active)
        .map_or(DEFAULT_BACKGROUND, |frame| frame.background);
    let mut cells = Vec::new();
    for (rect, frame) in rects.iter().zip(frames) {
        if frame.background != background {
            for y in 0..rect.rows {
                cells.push(Cell {
                    x: rect.x,
                    y,
                    text: String::new(),
                    width: rect.cols,
                    foreground: frame.foreground,
                    background: frame.background,
                    attributes: Attributes::default(),
                });
            }
        }
        for cell in &frame.cells {
            if cell.x >= rect.cols
                || cell.y >= rect.rows
                || cell.x.saturating_add(cell.width) > rect.cols
            {
                continue;
            }
            let mut cell = cell.clone();
            cell.x += rect.x;
            cells.push(cell);
        }
    }
    if rects.len() == 2 {
        for y in 0..rows {
            cells.push(Cell {
                x: rects[0].cols,
                y,
                text: DIVIDER.to_owned(),
                width: 1,
                foreground,
                background,
                attributes: Attributes {
                    faint: true,
                    ..Attributes::default()
                },
            });
        }
    }
    let cursor = rects
        .get(active)
        .zip(frames.get(active))
        .and_then(|(rect, frame)| frame.cursor.as_ref().map(|cursor| (rect, cursor)))
        .and_then(|(rect, cursor)| {
            (cursor.x < rect.cols && cursor.y < rect.rows).then(|| Cursor {
                x: rect.x + cursor.x,
                y: cursor.y,
                color: cursor.color,
                blinking: cursor.blinking,
            })
        });
    Frame {
        version: FORMAT_VERSION,
        cols,
        rows,
        foreground,
        background,
        cursor,
        cells,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InputAction {
    Send(Vec<u8>),
    SplitRight,
    FocusLeft,
    FocusRight,
    CloseActive,
    Quit,
}

#[derive(Default)]
pub(crate) struct PrefixDecoder {
    waiting: bool,
}

impl PrefixDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<InputAction> {
        let mut actions = Vec::new();
        let mut plain = Vec::new();
        for &byte in bytes {
            if self.waiting {
                if !plain.is_empty() {
                    actions.push(InputAction::Send(std::mem::take(&mut plain)));
                }
                let action = match byte {
                    b'%' => InputAction::SplitRight,
                    b'h' => InputAction::FocusLeft,
                    b'l' => InputAction::FocusRight,
                    b'x' => InputAction::CloseActive,
                    b'q' => InputAction::Quit,
                    PREFIX => InputAction::Send(vec![PREFIX]),
                    _ => InputAction::Send(vec![PREFIX, byte]),
                };
                actions.push(action);
                self.waiting = false;
            } else if byte == PREFIX {
                if !plain.is_empty() {
                    actions.push(InputAction::Send(std::mem::take(&mut plain)));
                }
                self.waiting = true;
            } else {
                plain.push(byte);
            }
        }
        if !plain.is_empty() {
            actions.push(InputAction::Send(plain));
        }
        actions
    }
}

pub(crate) struct OuterScreen {
    stdout: Stdout,
    modes: InputModes,
}

impl OuterScreen {
    pub(crate) fn enter() -> Result<Self> {
        let mut stdout = std::io::stdout();
        stdout
            .write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H")
            .context("enter workspace screen")?;
        stdout.flush().context("flush workspace screen")?;
        Ok(Self {
            stdout,
            modes: InputModes::default(),
        })
    }

    pub(crate) fn sync_input_modes(&mut self, modes: InputModes) -> Result<()> {
        if modes == self.modes {
            return Ok(());
        }
        set_dec_mode(&mut self.stdout, 1, modes.cursor_keys)?;
        self.stdout
            .write_all(if modes.keypad_keys {
                b"\x1b="
            } else {
                b"\x1b>"
            })
            .context("set workspace keypad mode")?;
        set_dec_mode(&mut self.stdout, 1004, modes.focus_events)?;
        set_dec_mode(&mut self.stdout, 2004, modes.bracketed_paste)?;
        self.stdout.flush().context("flush workspace input modes")?;
        self.modes = modes;
        Ok(())
    }

    pub(crate) fn paint(&mut self, frame: &Frame) -> Result<()> {
        write_frame(&mut self.stdout, frame)?;
        self.stdout.flush().context("flush workspace frame")
    }

    pub(crate) fn bell(&mut self) -> Result<()> {
        self.stdout
            .write_all(b"\x07")
            .context("ring workspace bell")?;
        self.stdout.flush().context("flush workspace bell")
    }
}

fn write_frame(mut writer: impl Write, frame: &Frame) -> Result<()> {
    write!(
        writer,
        "\x1b[0;48;2;{};{};{}m\x1b[?25l\x1b[2J\x1b[H",
        frame.background.r, frame.background.g, frame.background.b
    )
    .context("clear workspace screen")?;
    for cell in &frame.cells {
        if cell.x >= frame.cols || cell.y >= frame.rows {
            continue;
        }
        let text = if cell.text.is_empty() {
            " ".repeat(usize::from(cell.width))
        } else {
            cell.text.clone()
        };
        write!(
            writer,
            "\x1b[{};{}H\x1b[0;38;2;{};{};{};48;2;{};{};{}{}m{}",
            cell.y + 1,
            cell.x + 1,
            cell.foreground.r,
            cell.foreground.g,
            cell.foreground.b,
            cell.background.r,
            cell.background.g,
            cell.background.b,
            attributes(&cell.attributes),
            text,
        )
        .context("paint workspace cell")?;
    }
    if let Some(cursor) = &frame.cursor {
        write!(
            writer,
            "\x1b[0m\x1b[{};{}H\x1b[?25h",
            cursor.y + 1,
            cursor.x + 1
        )
        .context("place workspace cursor")?;
    }
    Ok(())
}

fn frame_ansi(frame: &Frame) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, frame)?;
    Ok(bytes)
}

impl Drop for OuterScreen {
    fn drop(&mut self) {
        let _ = self
            .stdout
            .write_all(b"\x1b[?1l\x1b>\x1b[?1004l\x1b[?2004l");
        let _ = self.stdout.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = self.stdout.flush();
    }
}

fn set_dec_mode(stdout: &mut Stdout, mode: u16, enabled: bool) -> Result<()> {
    write!(stdout, "\x1b[?{mode}{}", if enabled { 'h' } else { 'l' })
        .context("set workspace terminal mode")
}

fn attributes(attributes: &Attributes) -> String {
    let mut output = String::new();
    if attributes.bold {
        output.push_str(";1");
    }
    if attributes.faint {
        output.push_str(";2");
    }
    if attributes.italic {
        output.push_str(";3");
    }
    if attributes.underline.is_some() {
        output.push_str(";4");
    }
    if attributes.strikethrough {
        output.push_str(";9");
    }
    if attributes.overline {
        output.push_str(";53");
    }
    if attributes.invisible {
        output.push_str(";8");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn frame(cols: u16, rows: u16, text: &str, cursor: Option<(u16, u16)>) -> Frame {
        Frame {
            version: FORMAT_VERSION,
            cols,
            rows,
            foreground: DEFAULT_FOREGROUND,
            background: DEFAULT_BACKGROUND,
            cursor: cursor.map(|(x, y)| Cursor {
                x,
                y,
                color: DEFAULT_FOREGROUND,
                blinking: false,
            }),
            cells: vec![Cell {
                x: 0,
                y: 0,
                text: text.to_owned(),
                width: 1,
                foreground: DEFAULT_FOREGROUND,
                background: DEFAULT_BACKGROUND,
                attributes: Attributes::default(),
            }],
        }
    }

    #[test]
    fn two_pane_layout_reserves_one_divider_column() {
        assert_eq!(
            layout(80, 24, 2).unwrap(),
            [
                PaneRect {
                    x: 0,
                    cols: 39,
                    rows: 24
                },
                PaneRect {
                    x: 40,
                    cols: 40,
                    rows: 24
                }
            ]
        );
    }

    #[test]
    fn composition_offsets_right_cells_and_active_cursor() {
        let rects = layout(11, 3, 2).unwrap();
        let composed = compose(
            11,
            3,
            &rects,
            &[
                frame(5, 3, "L", Some((1, 1))),
                frame(5, 3, "R", Some((2, 2))),
            ],
            1,
        );

        assert!(
            composed
                .cells
                .iter()
                .any(|cell| cell.text == "R" && cell.x == 6)
        );
        assert_eq!(composed.cursor.as_ref().unwrap().x, 8);
        assert_eq!(composed.text(), "L    │R\n     │\n     │");
    }

    #[test]
    fn prefix_decoder_keeps_state_across_input_chunks() {
        let mut decoder = PrefixDecoder::default();

        assert!(decoder.push(&[PREFIX]).is_empty());
        assert_eq!(decoder.push(b"%"), [InputAction::SplitRight]);
        assert_eq!(
            decoder.push(&[PREFIX, b'z']),
            [InputAction::Send(vec![PREFIX, b'z'])]
        );
        assert_eq!(
            decoder.push(&[PREFIX, PREFIX]),
            [InputAction::Send(vec![PREFIX])]
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_workspace_splits_and_routes_agent_input() {
        let options = Options {
            cols: 21,
            rows: 4,
            ..Options::default()
        };
        let mut workspace = Workspace::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf LEFT; cat".to_owned(),
            ],
            None,
            None,
            &options,
        )
        .unwrap();
        workspace.shell = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf RIGHT; cat".to_owned(),
        ];
        workspace.split_right().unwrap();
        workspace.send(Some(1), b"AGENT\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = loop {
            workspace.pump().unwrap();
            let frame = workspace.frame().unwrap();
            let text = frame.text();
            if text.contains("LEFT") && text.contains("RIGHT") && text.contains("AGENT") {
                break frame;
            }
            assert!(Instant::now() < deadline, "workspace output did not arrive");
            std::thread::sleep(Duration::from_millis(10));
        };

        assert_eq!(frame.cols, 21);
        assert_eq!(workspace.panes().unwrap().len(), 2);
        assert!(!workspace.shot(None).unwrap().ansi.is_empty());
        workspace.resize(2, 4, 9, 18).unwrap();
        assert_eq!(workspace.status().unwrap().cols, 21);
        workspace.send(Some(1), &[0x04]).unwrap();
        let error = workspace
            .wait_for_text(Some(1), "never", Duration::from_secs(2), |workspace| {
                workspace.pump()?;
                workspace.remove_exited()?;
                Ok(!workspace.is_empty())
            })
            .unwrap_err();
        assert!(error.to_string().contains("pane 1 ended"));
        workspace.stop();
    }
}
