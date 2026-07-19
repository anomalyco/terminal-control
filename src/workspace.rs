use std::io::{Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
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
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
const PASTE_CHUNK_BYTES: usize = 64 * 1024;

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
    paste: Option<(PaneId, bool)>,
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
            paste: None,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    fn all_exits_observed(&self) -> bool {
        !self.panes.is_empty() && self.panes.iter().all(|pane| pane.session.exit_observed())
    }

    pub(crate) fn active_id(&self) -> Option<PaneId> {
        self.panes.get(self.active).map(|pane| pane.id)
    }

    pub(crate) fn pump(&mut self) -> Result<()> {
        for pane in &mut self.panes {
            pane.session.pump()?;
        }
        Ok(())
    }

    pub(crate) fn observe_exits(&mut self) -> Result<bool> {
        let mut exited = false;
        for pane in &mut self.panes {
            exited |= pane.session.is_exited()?;
        }
        Ok(exited)
    }

    pub(crate) fn remove_observed_exits(&mut self) -> Result<bool> {
        let mut removed = false;
        let mut active_removed = false;
        let mut index = 0;
        while index < self.panes.len() {
            if self.panes[index].session.exit_observed() {
                let id = self.panes[index].id;
                if self.paste.is_some_and(|(target, _)| target == id) {
                    self.paste = None;
                }
                if index == self.active {
                    active_removed = true;
                } else if index < self.active {
                    self.active -= 1;
                }
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
            if active_removed && self.panes[self.active].session.input_modes()?.focus_events {
                self.panes[self.active]
                    .session
                    .send_current_if_open(b"\x1b[I")?;
            }
            self.apply_layout()?;
        }
        Ok(removed)
    }

    pub(crate) fn split_right(&mut self) -> Result<()> {
        if self.panes.is_empty() {
            bail!("workspace has no pane to split");
        }
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
        self.focus(1)?;
        Ok(())
    }

    pub(crate) fn focus_left(&mut self) -> Result<bool> {
        self.focus(0)
    }

    pub(crate) fn focus_right(&mut self) -> Result<bool> {
        if self.panes.len() < 2 {
            return Ok(false);
        }
        self.focus(1)
    }

    pub(crate) fn close_active(&mut self) -> Result<()> {
        if self.panes.is_empty() {
            return Ok(());
        }
        if self.panes[self.active].session.input_modes()?.focus_events {
            self.panes[self.active]
                .session
                .send_current_if_open(b"\x1b[O")?;
        }
        let id = self.panes[self.active].id;
        if self.paste.is_some_and(|(target, _)| target == id) {
            self.paste = None;
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
            if self.panes[self.active].session.input_modes()?.focus_events {
                self.panes[self.active]
                    .session
                    .send_current_if_open(b"\x1b[I")?;
            }
            self.apply_layout()?;
        }
        Ok(())
    }

    pub(crate) fn send(&mut self, pane: Option<PaneId>, input: &[u8]) -> Result<()> {
        let index = self.resolve_pane(pane)?;
        self.panes[index].session.send_current(input)
    }

    pub(crate) fn send_active_if_open(&mut self, input: &[u8]) -> Result<bool> {
        let index = self.resolve_pane(None)?;
        self.panes[index].session.send_current_if_open(input)
    }

    pub(crate) fn begin_paste(&mut self) -> Result<bool> {
        let index = self.resolve_pane(None)?;
        let target = self.panes[index].id;
        let bracketed = self.panes[index].session.input_modes()?.bracketed_paste;
        if bracketed
            && !self.panes[index]
                .session
                .send_current_if_open(PASTE_START)?
        {
            return Ok(false);
        }
        self.paste = Some((target, bracketed));
        Ok(true)
    }

    pub(crate) fn send_paste(&mut self, input: &[u8]) -> Result<bool> {
        let (target, _) = self
            .paste
            .ok_or_else(|| anyhow::anyhow!("workspace paste has not started"))?;
        let index = self.resolve_pane(Some(target))?;
        self.panes[index].session.send_current_if_open(input)
    }

    pub(crate) fn end_paste(&mut self) -> Result<bool> {
        let Some((target, bracketed)) = self.paste.take() else {
            bail!("workspace paste has not started");
        };
        if bracketed {
            let index = self.resolve_pane(Some(target))?;
            return self.panes[index].session.send_current_if_open(PASTE_END);
        }
        Ok(true)
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
            frames.push(pane.session.current_frame()?);
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
            let running = tick(self)?;
            if let Some(pane) = pane {
                let index = self.resolve_pane(Some(pane))?;
                if self.panes[index].session.exit_observed() {
                    return self.panes[index].session.snapshot();
                }
            }
            if !running {
                if self.is_empty() {
                    bail!("workspace ended before capture completed");
                }
                return self.shot(pane);
            }
            let idle = match pane {
                Some(pane) => {
                    let index = self.resolve_pane(Some(pane))?;
                    self.panes[index].session.idle_for(started)
                }
                None => {
                    let mut idle = started.elapsed();
                    for pane in &mut self.panes {
                        let pane_idle = pane.session.idle_for(started);
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

    pub(crate) fn active_title(&self) -> Result<String> {
        let index = self.resolve_pane(None)?;
        self.panes[index].session.title()
    }

    pub(crate) fn take_bells(&self) -> u64 {
        self.panes
            .iter()
            .map(|pane| pane.session.take_bells())
            .sum()
    }

    pub(crate) fn active_cursor_style(&self) -> libghostty_vt::render::CursorVisualStyle {
        self.panes[self.active].session.cursor_style()
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
            let running = tick(self)?;
            let index = match self.resolve_pane(Some(target)) {
                Ok(index) => index,
                Err(_) => bail!("pane {target} ended before visible terminal included {text:?}"),
            };
            if self.panes[index]
                .session
                .current_frame()?
                .text()
                .contains(text)
            {
                return Ok(());
            }
            if !running || self.panes[index].session.exit_observed() {
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
        self.paste = None;
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

    fn focus(&mut self, next: usize) -> Result<bool> {
        if self.active == next {
            return Ok(false);
        }
        if self.panes[self.active].session.input_modes()?.focus_events {
            self.panes[self.active]
                .session
                .send_current_if_open(b"\x1b[O")?;
        }
        self.active = next;
        if self.panes[self.active].session.input_modes()?.focus_events {
            self.panes[self.active]
                .session
                .send_current_if_open(b"\x1b[I")?;
        }
        Ok(true)
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
                text: if y == 0 {
                    if active == 0 { "◀" } else { "▶" }.to_owned()
                } else {
                    DIVIDER.to_owned()
                },
                width: 1,
                foreground,
                background,
                attributes: if y == 0 {
                    Attributes {
                        bold: true,
                        ..Attributes::default()
                    }
                } else {
                    Attributes {
                        faint: true,
                        ..Attributes::default()
                    }
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
enum InputAction {
    Send(Vec<u8>),
    PasteStart,
    PasteData(Vec<u8>),
    PasteEnd,
    SplitRight,
    FocusLeft,
    FocusRight,
    CloseActive,
    Quit,
    Help,
    Cancel,
    Unknown(u8),
}

struct PrefixDecoder {
    waiting: bool,
    pasting: bool,
    pending: Vec<u8>,
    pending_since: Option<Instant>,
    paste: Vec<u8>,
}

impl Default for PrefixDecoder {
    fn default() -> Self {
        Self {
            waiting: false,
            pasting: false,
            pending: Vec::with_capacity(PASTE_START.len()),
            pending_since: None,
            paste: Vec::new(),
        }
    }
}

impl PrefixDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<InputAction> {
        let mut actions = Vec::new();
        let mut plain = Vec::new();
        let mut input = std::mem::take(&mut self.pending);
        self.pending_since = None;
        input.extend_from_slice(bytes);
        let mut index = 0;
        while index < input.len() {
            if self.pasting {
                let remaining = &input[index..];
                if let Some(end) = remaining
                    .windows(PASTE_END.len())
                    .position(|window| window == PASTE_END)
                {
                    self.paste.extend_from_slice(&remaining[..end]);
                    if !self.paste.is_empty() {
                        actions.push(InputAction::PasteData(std::mem::take(&mut self.paste)));
                    }
                    actions.push(InputAction::PasteEnd);
                    index += end + PASTE_END.len();
                    self.pasting = false;
                    continue;
                }
                let keep = partial_marker_len(remaining, PASTE_END);
                self.paste
                    .extend_from_slice(&remaining[..remaining.len() - keep]);
                if self.paste.len() >= PASTE_CHUNK_BYTES {
                    actions.push(InputAction::PasteData(std::mem::take(&mut self.paste)));
                }
                self.pending
                    .extend_from_slice(&remaining[remaining.len() - keep..]);
                self.pending_since = Some(Instant::now());
                break;
            }
            let remaining = &input[index..];
            if remaining.starts_with(PASTE_START) {
                if self.waiting {
                    flush_plain(&mut actions, &mut plain);
                    actions.push(InputAction::Send(vec![PREFIX]));
                    self.waiting = false;
                }
                index += PASTE_START.len();
                self.pasting = true;
                actions.push(InputAction::PasteStart);
                continue;
            }
            if PASTE_START.starts_with(remaining) {
                self.pending.extend_from_slice(remaining);
                self.pending_since = Some(Instant::now());
                break;
            }
            let byte = input[index];
            index += 1;
            if self.waiting {
                flush_plain(&mut actions, &mut plain);
                let action = match byte {
                    b'%' => InputAction::SplitRight,
                    b'h' => InputAction::FocusLeft,
                    b'l' => InputAction::FocusRight,
                    b'x' => InputAction::CloseActive,
                    b'q' => InputAction::Quit,
                    b'?' => InputAction::Help,
                    0x1b => InputAction::Cancel,
                    PREFIX => InputAction::Send(vec![PREFIX]),
                    _ => InputAction::Unknown(byte),
                };
                actions.push(action);
                self.waiting = false;
            } else if byte == PREFIX {
                flush_plain(&mut actions, &mut plain);
                self.waiting = true;
            } else {
                plain.push(byte);
            }
        }
        flush_plain(&mut actions, &mut plain);
        actions
    }

    fn waiting(&self) -> bool {
        self.waiting
    }

    fn flush_ambiguous(&mut self, after: Duration) -> Vec<InputAction> {
        if self.pending.is_empty()
            || self
                .pending_since
                .is_none_or(|started| started.elapsed() < after)
        {
            return Vec::new();
        }
        self.pending_since = None;
        if self.pasting {
            return Vec::new();
        }
        let pending = std::mem::take(&mut self.pending);
        if self.waiting && pending.first() == Some(&0x1b) {
            self.waiting = false;
            let mut actions = vec![InputAction::Cancel];
            if pending.len() > 1 {
                actions.push(InputAction::Send(pending[1..].to_vec()));
            }
            return actions;
        }
        vec![InputAction::Send(pending)]
    }
}

fn flush_plain(actions: &mut Vec<InputAction>, plain: &mut Vec<u8>) {
    if !plain.is_empty() {
        actions.push(InputAction::Send(std::mem::take(plain)));
    }
}

fn partial_marker_len(bytes: &[u8], marker: &[u8]) -> usize {
    (1..marker.len().min(bytes.len() + 1))
        .rev()
        .find(|&length| bytes.ends_with(&marker[..length]))
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArmedAction {
    Close(PaneId),
    Quit,
}

struct WorkspaceUi {
    notice: Option<(String, Instant)>,
    armed: Option<(ArmedAction, Instant)>,
}

impl WorkspaceUi {
    fn new() -> Self {
        let mut ui = Self {
            notice: None,
            armed: None,
        };
        ui.notice("^B ? workspace keys", Duration::from_secs(2));
        ui
    }

    fn notice(&mut self, text: impl Into<String>, duration: Duration) {
        self.notice = Some((text.into(), Instant::now() + duration));
    }

    fn clear_armed(&mut self) {
        if self.armed.take().is_some() {
            self.notice = None;
        }
    }

    fn confirm(&mut self, action: ArmedAction, prompt: &str) -> bool {
        let now = Instant::now();
        if self
            .armed
            .is_some_and(|(armed, expires)| armed == action && expires >= now)
        {
            self.armed = None;
            return true;
        }
        self.armed = Some((action, now + Duration::from_millis(1_500)));
        self.notice(prompt, Duration::from_millis(1_500));
        false
    }

    fn overlay(&mut self, prefix: bool) -> Option<String> {
        if prefix {
            return Some("^B".to_owned());
        }
        let now = Instant::now();
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, expires)| *expires < now)
        {
            self.notice = None;
        }
        if self
            .armed
            .as_ref()
            .is_some_and(|(_, expires)| *expires < now)
        {
            self.armed = None;
        }
        self.notice.as_ref().map(|(text, _)| text.clone())
    }
}

#[derive(Default)]
struct WorkspaceFrames {
    content: Option<Frame>,
}

pub(crate) struct WorkspaceTerminal {
    input: Receiver<Vec<u8>>,
    decoder: PrefixDecoder,
    ui: WorkspaceUi,
    screen: OuterScreen,
    frames: WorkspaceFrames,
    cell_width: u16,
    cell_height: u16,
    pending_removal: bool,
    finished: bool,
}

impl WorkspaceTerminal {
    pub(crate) fn enter(input: Receiver<Vec<u8>>, options: &Options) -> Result<Self> {
        Ok(Self {
            input,
            decoder: PrefixDecoder::default(),
            ui: WorkspaceUi::new(),
            screen: OuterScreen::enter()?,
            frames: WorkspaceFrames::default(),
            cell_width: options.cell_width,
            cell_height: options.cell_height,
            pending_removal: false,
            finished: false,
        })
    }

    pub(crate) fn content(&self) -> Option<&Frame> {
        self.frames.content.as_ref()
    }

    pub(crate) fn finished(&self) -> bool {
        self.finished
    }

    pub(crate) fn tick(&mut self, workspace: &mut Workspace) -> Result<bool> {
        if self.finished {
            return Ok(false);
        }
        if self.pending_removal {
            workspace.remove_observed_exits()?;
            self.pending_removal = false;
            if workspace.is_empty() {
                self.finished = true;
                return Ok(false);
            }
        }
        workspace.pump()?;
        let exited = workspace.observe_exits()?;
        if workspace.take_bells() > 0 {
            self.screen.bell()?;
        }
        self.screen.sync_title(&workspace.active_title()?)?;
        if !exited {
            let mut actions = Vec::new();
            while let Ok(input) = self.input.try_recv() {
                actions.extend(self.decoder.push(&input));
            }
            actions.extend(self.decoder.flush_ambiguous(Duration::from_millis(25)));
            for action in actions {
                match action {
                    InputAction::Send(input) => {
                        self.ui.clear_armed();
                        if !workspace.send_active_if_open(&input)? {
                            workspace.observe_exits()?;
                            break;
                        }
                    }
                    InputAction::PasteStart => {
                        self.ui.clear_armed();
                        if !workspace.begin_paste()? {
                            workspace.observe_exits()?;
                            break;
                        }
                    }
                    InputAction::PasteData(input) => {
                        if !workspace.send_paste(&input)? {
                            workspace.observe_exits()?;
                            break;
                        }
                    }
                    InputAction::PasteEnd => {
                        if !workspace.end_paste()? {
                            workspace.observe_exits()?;
                            break;
                        }
                    }
                    InputAction::SplitRight => {
                        self.ui.clear_armed();
                        match workspace.split_right() {
                            Ok(()) => self.ui.notice(
                                format!("pane {} active", workspace.active_id().unwrap_or(0)),
                                Duration::from_millis(1_200),
                            ),
                            Err(error) => {
                                self.screen.bell()?;
                                self.ui
                                    .notice(error.to_string(), Duration::from_millis(1_500));
                            }
                        }
                    }
                    InputAction::FocusLeft => {
                        self.ui.clear_armed();
                        if workspace.focus_left()? {
                            self.ui.notice(
                                format!("pane {} active", workspace.active_id().unwrap_or(0)),
                                Duration::from_millis(1_000),
                            );
                        } else {
                            self.ui
                                .notice("no pane to the left", Duration::from_millis(1_000));
                        }
                    }
                    InputAction::FocusRight => {
                        self.ui.clear_armed();
                        if workspace.focus_right()? {
                            self.ui.notice(
                                format!("pane {} active", workspace.active_id().unwrap_or(0)),
                                Duration::from_millis(1_000),
                            );
                        } else {
                            self.ui
                                .notice("no pane to the right", Duration::from_millis(1_000));
                        }
                    }
                    InputAction::CloseActive => {
                        let pane = workspace.active_id().unwrap_or(0);
                        if self.ui.confirm(
                            ArmedAction::Close(pane),
                            &format!("^B x again to kill pane {pane}"),
                        ) {
                            workspace.close_active()?;
                            self.ui.notice(
                                format!("pane {pane} killed"),
                                Duration::from_millis(1_200),
                            );
                        }
                    }
                    InputAction::Quit => {
                        if self
                            .ui
                            .confirm(ArmedAction::Quit, "^B q again to kill all panes")
                        {
                            workspace.stop();
                        }
                    }
                    InputAction::Help => {
                        self.ui.clear_armed();
                        self.ui.notice(
                            "^B % split  h/l focus  x kill pane  q kill all  ^B send prefix",
                            Duration::from_secs(4),
                        );
                    }
                    InputAction::Cancel => {
                        self.ui.clear_armed();
                        self.ui
                            .notice("workspace prefix canceled", Duration::from_millis(1_000));
                    }
                    InputAction::Unknown(byte) => {
                        self.ui.clear_armed();
                        self.screen.bell()?;
                        let key = if byte.is_ascii_graphic() {
                            char::from(byte).to_string()
                        } else {
                            format!("0x{byte:02x}")
                        };
                        self.ui.notice(
                            format!("unknown workspace key: {key}  ^B ? for help"),
                            Duration::from_millis(1_500),
                        );
                    }
                }
                if workspace.is_empty() {
                    break;
                }
            }
        }
        if workspace.is_empty() {
            self.finished = true;
            return Ok(false);
        }
        if let Ok((cols, rows)) = crossterm::terminal::size()
            && cols > 0
            && rows > 0
        {
            workspace.resize(cols, rows, self.cell_width, self.cell_height)?;
        }
        self.screen
            .sync_input_modes(workspace.active_input_modes()?)?;
        let mut frame = workspace.frame()?;
        if self.frames.content.as_ref() != Some(&frame) {
            self.frames.content = Some(frame.clone());
        }
        self.screen.sync_cursor_style(
            workspace.active_cursor_style(),
            frame.cursor.as_ref().is_some_and(|cursor| cursor.blinking),
        )?;
        if let Some(overlay) = self.ui.overlay(self.decoder.waiting()) {
            add_overlay(&mut frame, &overlay);
        }
        self.screen.paint(&frame)?;
        if exited {
            if workspace.all_exits_observed() {
                self.finished = true;
                return Ok(false);
            }
            self.pending_removal = true;
        }
        Ok(true)
    }
}

fn add_overlay(frame: &mut Frame, text: &str) {
    if frame.cols == 0 || frame.rows == 0 || text.is_empty() {
        return;
    }
    let text = text
        .chars()
        .rev()
        .take(usize::from(frame.cols))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    let width = u16::try_from(text.chars().count()).unwrap_or(frame.cols);
    let start = frame.cols - width;
    let y = frame.rows - 1;
    let mut replacements = Vec::new();
    for cell in &frame.cells {
        if cell.y == y && cell.x < start && cell.x.saturating_add(cell.width) > start {
            replacements.push(Cell {
                x: cell.x,
                y,
                text: String::new(),
                width: start - cell.x,
                foreground: cell.foreground,
                background: cell.background,
                attributes: cell.attributes.clone(),
            });
        }
    }
    frame.cells.retain(|cell| {
        cell.y != y || cell.x.saturating_add(cell.width) <= start || cell.x >= frame.cols
    });
    frame.cells.extend(replacements);
    for (offset, character) in text.chars().enumerate() {
        frame.cells.push(Cell {
            x: start + u16::try_from(offset).unwrap_or(0),
            y,
            text: character.to_string(),
            width: 1,
            foreground: frame.background,
            background: frame.foreground,
            attributes: Attributes {
                bold: true,
                ..Attributes::default()
            },
        });
    }
}

struct OuterScreen {
    stdout: Stdout,
    modes: InputModes,
    previous: Option<Frame>,
    title: String,
    cursor_style: Option<(libghostty_vt::render::CursorVisualStyle, bool)>,
    output: Vec<u8>,
}

impl OuterScreen {
    pub(crate) fn enter() -> Result<Self> {
        let mut stdout = std::io::stdout();
        stdout
            .write_all(b"\x1b[22;0t\x1b[?1049h\x1b[?2004h\x1b[?25l\x1b[2J\x1b[H")
            .context("enter workspace screen")?;
        stdout.flush().context("flush workspace screen")?;
        Ok(Self {
            stdout,
            modes: InputModes::default(),
            previous: None,
            title: String::new(),
            cursor_style: None,
            output: Vec::with_capacity(16 * 1024),
        })
    }

    pub(crate) fn sync_input_modes(&mut self, modes: InputModes) -> Result<()> {
        if modes == self.modes {
            return Ok(());
        }
        set_dec_mode(&mut self.output, 1, modes.cursor_keys)?;
        self.output
            .write_all(if modes.keypad_keys {
                b"\x1b="
            } else {
                b"\x1b>"
            })
            .context("set workspace keypad mode")?;
        set_dec_mode(&mut self.output, 1004, modes.focus_events)?;
        self.modes = modes;
        Ok(())
    }

    pub(crate) fn paint(&mut self, frame: &Frame) -> Result<()> {
        let changed = self.previous.as_ref() != Some(frame);
        if changed {
            write_frame_update(&mut self.output, self.previous.as_ref(), frame)?;
            self.previous = Some(frame.clone());
        }
        if self.output.is_empty() {
            return Ok(());
        }
        self.stdout
            .write_all(&self.output)
            .context("write workspace update")?;
        self.stdout.flush().context("flush workspace frame")?;
        self.output.clear();
        Ok(())
    }

    pub(crate) fn sync_title(&mut self, title: &str) -> Result<()> {
        let title = title
            .chars()
            .filter(|character| !character.is_control())
            .collect::<String>();
        if title == self.title {
            return Ok(());
        }
        write!(self.output, "\x1b]2;{title}\x07").context("set workspace title")?;
        self.title = title;
        Ok(())
    }

    pub(crate) fn sync_cursor_style(
        &mut self,
        style: libghostty_vt::render::CursorVisualStyle,
        blinking: bool,
    ) -> Result<()> {
        if self.cursor_style == Some((style, blinking)) {
            return Ok(());
        }
        use libghostty_vt::render::CursorVisualStyle;
        let code = match (style, blinking) {
            (CursorVisualStyle::Block, true) => 1,
            (CursorVisualStyle::Block | CursorVisualStyle::BlockHollow, false) => 2,
            (CursorVisualStyle::Underline, true) => 3,
            (CursorVisualStyle::Underline, false) => 4,
            (CursorVisualStyle::Bar, true) => 5,
            (CursorVisualStyle::Bar, false) => 6,
            _ => 2,
        };
        write!(self.output, "\x1b[{code} q").context("set workspace cursor style")?;
        self.cursor_style = Some((style, blinking));
        Ok(())
    }

    pub(crate) fn bell(&mut self) -> Result<()> {
        self.output
            .write_all(b"\x07")
            .context("ring workspace bell")
    }
}

fn write_frame(mut writer: impl Write, frame: &Frame) -> Result<()> {
    write_frame_update(&mut writer, None, frame)
}

fn write_frame_update(
    mut writer: impl Write,
    previous: Option<&Frame>,
    frame: &Frame,
) -> Result<()> {
    let full = previous.is_none_or(|previous| {
        previous.cols != frame.cols
            || previous.rows != frame.rows
            || previous.foreground != frame.foreground
            || previous.background != frame.background
    });
    writer
        .write_all(b"\x1b[?2026h\x1b[?25l")
        .context("begin workspace frame")?;
    if full {
        write!(
            writer,
            "\x1b[0;48;2;{};{};{}m\x1b[2J\x1b[H",
            frame.background.r, frame.background.g, frame.background.b
        )
        .context("clear workspace screen")?;
    }
    let rows = cells_by_row(frame);
    let previous_rows = previous.map(cells_by_row);
    for y in 0..frame.rows {
        let cells = &rows[usize::from(y)];
        if !full
            && previous_rows
                .as_ref()
                .is_some_and(|previous| previous[usize::from(y)].as_slice() == cells.as_slice())
        {
            continue;
        }
        let row = dense_row(frame, cells);
        write!(writer, "\x1b[{};1H", y + 1).context("place workspace row")?;
        let mut style = None;
        let mut x = 0_usize;
        while x < row.len() {
            let cell = &row[x];
            if cell.continuation {
                x += 1;
                continue;
            }
            let next_style = (&cell.foreground, &cell.background, &cell.attributes);
            if style != Some(next_style) {
                write!(
                    writer,
                    "\x1b[0;38;2;{};{};{};48;2;{};{};{}{}m",
                    cell.foreground.r,
                    cell.foreground.g,
                    cell.foreground.b,
                    cell.background.r,
                    cell.background.g,
                    cell.background.b,
                    attributes(&cell.attributes),
                )
                .context("paint workspace style")?;
                style = Some(next_style);
            }
            writer
                .write_all(cell.text.unwrap_or(" ").as_bytes())
                .context("paint workspace text")?;
            x += usize::from(cell.width.max(1));
        }
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
    writer
        .write_all(b"\x1b[?2026l")
        .context("finish workspace frame")?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PaintCell<'a> {
    text: Option<&'a str>,
    width: u16,
    foreground: crate::frame::Color,
    background: crate::frame::Color,
    attributes: Attributes,
    continuation: bool,
}

fn cells_by_row(frame: &Frame) -> Vec<Vec<&Cell>> {
    let mut rows = vec![Vec::new(); usize::from(frame.rows)];
    for cell in &frame.cells {
        if cell.y < frame.rows {
            rows[usize::from(cell.y)].push(cell);
        }
    }
    rows
}

fn dense_row<'a>(frame: &Frame, cells: &[&'a Cell]) -> Vec<PaintCell<'a>> {
    let mut row = vec![
        PaintCell {
            text: None,
            width: 1,
            foreground: frame.foreground,
            background: frame.background,
            attributes: Attributes::default(),
            continuation: false,
        };
        usize::from(frame.cols)
    ];
    for &cell in cells {
        if cell.x >= frame.cols || cell.width == 0 {
            continue;
        }
        let end = cell.x.saturating_add(cell.width).min(frame.cols);
        if cell.text.is_empty() {
            for x in cell.x..end {
                row[usize::from(x)] = PaintCell {
                    text: None,
                    width: 1,
                    foreground: cell.foreground,
                    background: cell.background,
                    attributes: cell.attributes.clone(),
                    continuation: false,
                };
            }
            continue;
        }
        row[usize::from(cell.x)] = PaintCell {
            text: Some(&cell.text),
            width: cell.width,
            foreground: cell.foreground,
            background: cell.background,
            attributes: cell.attributes.clone(),
            continuation: false,
        };
        for x in cell.x + 1..end {
            row[usize::from(x)].continuation = true;
        }
    }
    row
}

fn frame_ansi(frame: &Frame) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, frame)?;
    Ok(bytes)
}

impl Drop for OuterScreen {
    fn drop(&mut self) {
        let _ = self.stdout.write_all(&self.output);
        let _ = self
            .stdout
            .write_all(b"\x1b[?2026l\x1b[?1l\x1b>\x1b[?1004l\x1b[?2004l");
        let _ = self
            .stdout
            .write_all(b"\x1b[0 q\x1b[0m\x1b[?25h\x1b[?1049l\x1b[23;0t");
        let _ = self.stdout.flush();
    }
}

fn set_dec_mode(writer: &mut impl Write, mode: u16, enabled: bool) -> Result<()> {
    write!(writer, "\x1b[?{mode}{}", if enabled { 'h' } else { 'l' })
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
        assert_eq!(composed.text(), "L    ▶R\n     │\n     │");
    }

    #[test]
    fn compositor_batches_full_rows_and_diffs_incremental_updates() {
        let mut first = frame(80, 24, "x", Some((0, 0)));
        first.cells = (0..24)
            .flat_map(|y| {
                (0..80).map(move |x| Cell {
                    x,
                    y,
                    text: "x".to_owned(),
                    width: 1,
                    foreground: DEFAULT_FOREGROUND,
                    background: DEFAULT_BACKGROUND,
                    attributes: Attributes::default(),
                })
            })
            .collect();
        let mut full = Vec::new();
        write_frame_update(&mut full, None, &first).unwrap();

        let mut second = first.clone();
        second.cells[0].text = "y".to_owned();
        second.cursor.as_mut().unwrap().x = 1;
        let mut incremental = Vec::new();
        write_frame_update(&mut incremental, Some(&first), &second).unwrap();

        assert!(full.len() < 5_000, "full frame was {} bytes", full.len());
        assert!(
            incremental.len() < 500,
            "incremental frame was {} bytes",
            incremental.len()
        );
        assert!(!incremental.windows(3).any(|bytes| bytes == b"[2J"));
    }

    #[test]
    fn batched_compositor_round_trips_wide_and_styled_cells() {
        let mut source = frame(8, 2, "界", Some((2, 0)));
        source.cells[0].width = 2;
        source.cells.push(Cell {
            x: 3,
            y: 1,
            text: "X".to_owned(),
            width: 1,
            foreground: crate::frame::Color { r: 1, g: 2, b: 3 },
            background: crate::frame::Color { r: 4, g: 5, b: 6 },
            attributes: Attributes {
                bold: true,
                underline: Some(crate::frame::Underline::Single),
                ..Attributes::default()
            },
        });

        let replayed = crate::shot::from_ansi(frame_ansi(&source).unwrap(), 2, 8, 100_000)
            .unwrap()
            .frame;

        assert_eq!(replayed.text(), source.text());
        let styled = replayed.cells.iter().find(|cell| cell.text == "X").unwrap();
        assert!(styled.attributes.bold);
        assert!(styled.attributes.underline.is_some());
        assert_eq!(styled.foreground, crate::frame::Color { r: 1, g: 2, b: 3 });
        assert_eq!(styled.background, crate::frame::Color { r: 4, g: 5, b: 6 });
    }

    #[test]
    fn prefix_decoder_keeps_state_across_input_chunks() {
        let mut decoder = PrefixDecoder::default();

        assert!(decoder.push(&[PREFIX]).is_empty());
        assert_eq!(decoder.push(b"%"), [InputAction::SplitRight]);
        assert_eq!(decoder.push(&[PREFIX, b'z']), [InputAction::Unknown(b'z')]);
        assert_eq!(
            decoder.push(&[PREFIX, PREFIX]),
            [InputAction::Send(vec![PREFIX])]
        );
    }

    #[test]
    fn prefix_decoder_never_interprets_commands_inside_bracketed_paste() {
        let mut decoder = PrefixDecoder::default();

        assert!(decoder.push(b"\x1b[20").is_empty());
        assert_eq!(decoder.push(b"0~A\x02%B\x1b[20"), [InputAction::PasteStart]);
        assert!(decoder.flush_ambiguous(Duration::ZERO).is_empty());
        assert_eq!(
            decoder.push(b"1~"),
            [
                InputAction::PasteData(b"A\x02%B".to_vec()),
                InputAction::PasteEnd
            ]
        );
        assert_eq!(decoder.push(b"\x02%"), [InputAction::SplitRight]);
    }

    #[test]
    fn prefix_decoder_flushes_ambiguous_escape_input() {
        let mut decoder = PrefixDecoder::default();

        assert!(decoder.push(b"\x1b").is_empty());
        assert_eq!(
            decoder.flush_ambiguous(Duration::ZERO),
            [InputAction::Send(vec![0x1b])]
        );
        assert!(decoder.push(b"\x02\x1b").is_empty());
        assert_eq!(
            decoder.flush_ambiguous(Duration::ZERO),
            [InputAction::Cancel]
        );
    }

    #[test]
    fn large_paste_streams_data_inside_one_transaction() {
        let mut decoder = PrefixDecoder::default();
        let mut first = PASTE_START.to_vec();
        first.extend(std::iter::repeat_n(b'a', PASTE_CHUNK_BYTES));
        let mut actions = decoder.push(&first);
        let mut last = vec![b'b'; 70_000 - PASTE_CHUNK_BYTES];
        last.extend_from_slice(PASTE_END);
        actions.extend(decoder.push(&last));

        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, InputAction::PasteStart))
                .count(),
            1
        );
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, InputAction::PasteEnd))
                .count(),
            1
        );
        assert_eq!(
            actions
                .iter()
                .map(|action| match action {
                    InputAction::PasteData(bytes) => bytes.len(),
                    _ => 0,
                })
                .sum::<usize>(),
            70_000
        );
    }

    #[test]
    fn ordinary_input_clears_destructive_confirmation_and_notice() {
        let mut ui = WorkspaceUi::new();

        assert!(!ui.confirm(ArmedAction::Quit, "confirm quit"));
        assert_eq!(ui.overlay(false).as_deref(), Some("confirm quit"));
        ui.clear_armed();

        assert_eq!(ui.overlay(false), None);
        assert!(!ui.confirm(ArmedAction::Quit, "confirm quit"));
    }

    #[test]
    fn overlay_preserves_uncovered_background_span() {
        let mut source = frame(8, 1, "", None);
        source.cells = vec![Cell {
            x: 0,
            y: 0,
            text: String::new(),
            width: 8,
            foreground: DEFAULT_FOREGROUND,
            background: crate::frame::Color { r: 1, g: 2, b: 3 },
            attributes: Attributes::default(),
        }];

        add_overlay(&mut source, "OK");

        assert!(source.cells.iter().any(|cell| {
            cell.x == 0
                && cell.width == 6
                && cell.background == crate::frame::Color { r: 1, g: 2, b: 3 }
        }));
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
                workspace.observe_exits()?;
                workspace.remove_observed_exits()?;
                Ok(!workspace.is_empty())
            })
            .unwrap_err();
        assert!(error.to_string().contains("pane 1 ended"));
        workspace.stop();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_retains_final_output_drained_during_exit_observation() {
        let mut workspace = Workspace::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf '\\033[5n'; awk 'BEGIN { for (i = 0; i < 500; i++) print \"line-\" i; print \"END-MARKER\" }'"
                    .to_owned(),
            ],
            None,
            None,
            &Options {
                cols: 40,
                rows: 8,
                ..Options::default()
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !workspace.observe_exits().unwrap() {
            workspace.pump().unwrap();
            assert!(Instant::now() < deadline, "workspace command did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(workspace.frame().unwrap().text().contains("END-MARKER"));
        workspace.stop();
    }

    #[cfg(unix)]
    #[test]
    fn removing_panes_never_observes_a_late_exit_after_composition() {
        let mut workspace = Workspace::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "read line; printf LATE-MARKER".to_owned(),
            ],
            None,
            None,
            &Options {
                cols: 40,
                rows: 8,
                ..Options::default()
            },
        )
        .unwrap();
        assert!(!workspace.observe_exits().unwrap());
        workspace.send(None, b"\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            workspace.pump().unwrap();
            if workspace.frame().unwrap().text().contains("LATE-MARKER") {
                break;
            }
            assert!(Instant::now() < deadline, "workspace output did not arrive");
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(!workspace.remove_observed_exits().unwrap());
        while !workspace.observe_exits().unwrap() {
            assert!(Instant::now() < deadline, "workspace command did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(workspace.frame().unwrap().text().contains("LATE-MARKER"));
        assert!(workspace.remove_observed_exits().unwrap());
        assert!(workspace.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn targeted_capture_returns_a_panes_final_frame_without_waiting_to_settle() {
        let options = Options {
            cols: 40,
            rows: 8,
            ..Options::default()
        };
        let mut workspace = Workspace::start(
            &["sh".to_owned(), "-c".to_owned(), "cat".to_owned()],
            None,
            None,
            &options,
        )
        .unwrap();
        workspace.shell = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf TARGET-FINAL".to_owned(),
        ];
        workspace.split_right().unwrap();

        let shot = workspace
            .capture(
                Some(1),
                Duration::from_secs(600),
                Duration::from_secs(2),
                |workspace| {
                    workspace.pump()?;
                    workspace.observe_exits()?;
                    Ok(true)
                },
            )
            .unwrap();

        assert!(shot.frame.text().contains("TARGET-FINAL"));
        workspace.stop();
    }
}
