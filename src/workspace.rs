use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::frame::{
    Attributes, Cell, Cursor, DEFAULT_BACKGROUND, DEFAULT_FOREGROUND, FORMAT_VERSION, Frame,
};
use crate::session::{Session, SessionLaunch, SessionState, SessionStatus};
use crate::shot::{Options, Shot};
use crate::terminal_core::InputModes;
use crate::terminal_theme::TerminalTheme;

pub type PaneId = u32;

const VERTICAL_DIVIDER: &str = "│";
const HORIZONTAL_DIVIDER: &str = "─";
const PREFIX: u8 = 0x02;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
const SGR_MOUSE_PREFIX: &[u8] = b"\x1b[<";
const PASTE_CHUNK_BYTES: usize = 64 * 1024;

pub(crate) struct Workspace {
    panes: Vec<Pane>,
    active: Option<PaneId>,
    layout: Option<LayoutNode>,
    applied: AppliedLayout,
    next_id: PaneId,
    cols: u16,
    rows: u16,
    cwd: PathBuf,
    shell: Vec<String>,
    options: Options,
    theme: TerminalTheme,
    launch: SessionLaunch,
    paste: Option<(PaneId, bool)>,
}

struct Pane {
    id: PaneId,
    session: Session,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SplitAxis {
    Columns,
    Rows,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LayoutNode {
    Leaf(PaneId),
    Split {
        axis: SplitAxis,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    fn contains(&self, pane: PaneId) -> bool {
        match self {
            Self::Leaf(id) => *id == pane,
            Self::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    fn first_leaf(&self) -> PaneId {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }

    fn split_leaf(&mut self, target: PaneId, axis: SplitAxis, pane: PaneId) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    axis,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(pane)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split_leaf(target, axis, pane) || second.split_leaf(target, axis, pane)
            }
        }
    }

    fn remove_leaf(self, target: PaneId) -> (Option<Self>, bool) {
        match self {
            Self::Leaf(id) if id == target => (None, true),
            Self::Leaf(_) => (Some(self), false),
            Self::Split {
                axis,
                first,
                second,
            } => {
                if first.contains(target) {
                    let (first, removed) = first.remove_leaf(target);
                    let tree = match first {
                        Some(first) => Some(Self::Split {
                            axis,
                            first: Box::new(first),
                            second,
                        }),
                        None => Some(*second),
                    };
                    (tree, removed)
                } else if second.contains(target) {
                    let (second, removed) = second.remove_leaf(target);
                    let tree = match second {
                        Some(second) => Some(Self::Split {
                            axis,
                            first,
                            second: Box::new(second),
                        }),
                        None => Some(*first),
                    };
                    (tree, removed)
                } else {
                    (
                        Some(Self::Split {
                            axis,
                            first,
                            second,
                        }),
                        false,
                    )
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    fn unavailable(self) -> &'static str {
        match self {
            Self::Left => "no pane to the left",
            Self::Right => "no pane to the right",
            Self::Up => "no pane above",
            Self::Down => "no pane below",
        }
    }
}

fn directional_score(
    current: PaneRect,
    candidate: PaneRect,
    pane: PaneId,
    direction: Direction,
) -> Option<(bool, u16, u16, PaneId)> {
    let current_right = current.x.saturating_add(current.cols);
    let candidate_right = candidate.x.saturating_add(candidate.cols);
    let current_bottom = current.y.saturating_add(current.rows);
    let candidate_bottom = candidate.y.saturating_add(candidate.rows);
    let current_x = current.x.saturating_mul(2).saturating_add(current.cols);
    let candidate_x = candidate.x.saturating_mul(2).saturating_add(candidate.cols);
    let current_y = current.y.saturating_mul(2).saturating_add(current.rows);
    let candidate_y = candidate.y.saturating_mul(2).saturating_add(candidate.rows);
    let horizontal_overlap = current.x < candidate_right && candidate.x < current_right;
    let vertical_overlap = current.y < candidate_bottom && candidate.y < current_bottom;
    match direction {
        Direction::Left if candidate_right <= current.x => Some((
            !vertical_overlap,
            current.x - candidate_right,
            current_y.abs_diff(candidate_y),
            pane,
        )),
        Direction::Right if candidate.x >= current_right => Some((
            !vertical_overlap,
            candidate.x - current_right,
            current_y.abs_diff(candidate_y),
            pane,
        )),
        Direction::Up if candidate_bottom <= current.y => Some((
            !horizontal_overlap,
            current.y - candidate_bottom,
            current_x.abs_diff(candidate_x),
            pane,
        )),
        Direction::Down if candidate.y >= current_bottom => Some((
            !horizontal_overlap,
            candidate.y - current_bottom,
            current_x.abs_diff(candidate_x),
            pane,
        )),
        _ => None,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PaneStatus {
    pub id: PaneId,
    pub active: bool,
    pub state: SessionState,
    #[serde(default)]
    pub x: u16,
    #[serde(default)]
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub title: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaneRect {
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlacedPane {
    id: PaneId,
    rect: PaneRect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Divider {
    axis: SplitAxis,
    x: u16,
    y: u16,
    len: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceGeometry {
    panes: Vec<PlacedPane>,
    dividers: Vec<Divider>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AppliedLayout {
    Ready(WorkspaceGeometry),
    Constrained(WorkspaceGeometry),
}

impl AppliedLayout {
    fn geometry(&self) -> &WorkspaceGeometry {
        match self {
            Self::Ready(geometry) | Self::Constrained(geometry) => geometry,
        }
    }

    fn geometry_mut(&mut self) -> &mut WorkspaceGeometry {
        match self {
            Self::Ready(geometry) | Self::Constrained(geometry) => geometry,
        }
    }

    fn is_constrained(&self) -> bool {
        matches!(self, Self::Constrained(_))
    }
}

impl Workspace {
    #[cfg(test)]
    pub(crate) fn start(
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
    ) -> Result<Self> {
        Self::start_with_theme(command, cwd, record, options, TerminalTheme::default())
    }

    pub(crate) fn start_with_theme(
        command: &[String],
        cwd: Option<&Path>,
        record: Option<&Path>,
        options: &Options,
        theme: TerminalTheme,
    ) -> Result<Self> {
        let cwd = cwd
            .map(Path::to_owned)
            .unwrap_or(std::env::current_dir().context("resolve workspace directory")?);
        let shell = shell_command();
        let command = if command.is_empty() { &shell } else { command };
        let mut session = Session::start_with_theme(command, Some(&cwd), record, options, theme)?;
        let launch = session.status()?.launch;
        Ok(Self {
            panes: vec![Pane { id: 0, session }],
            active: Some(0),
            layout: Some(LayoutNode::Leaf(0)),
            applied: AppliedLayout::Ready(WorkspaceGeometry {
                panes: vec![PlacedPane {
                    id: 0,
                    rect: PaneRect {
                        x: 0,
                        y: 0,
                        cols: options.cols,
                        rows: options.rows,
                    },
                }],
                dividers: Vec::new(),
            }),
            next_id: 1,
            cols: options.cols,
            rows: options.rows,
            cwd,
            shell,
            options: options.clone(),
            theme,
            launch,
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
        self.active
    }

    fn is_multi_pane(&self) -> bool {
        self.panes.len() > 1
    }

    pub(crate) fn set_theme(&mut self, theme: TerminalTheme) -> Result<()> {
        for pane in &mut self.panes {
            pane.session.set_theme(theme)?;
        }
        self.theme = theme;
        Ok(())
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
        let exited = self
            .panes
            .iter()
            .filter(|pane| pane.session.exit_observed())
            .map(|pane| pane.id)
            .collect::<Vec<_>>();
        if exited.is_empty() {
            return Ok(false);
        }
        let active_removed = self.active.is_some_and(|active| exited.contains(&active));
        for id in &exited {
            self.remove_layout_pane(*id);
            self.panes.retain(|pane| pane.id != *id);
            if self.paste.is_some_and(|(target, _)| target == *id) {
                self.paste = None;
            }
        }
        if self.panes.is_empty() {
            self.active = None;
            self.layout = None;
            self.applied = AppliedLayout::Ready(WorkspaceGeometry {
                panes: Vec::new(),
                dividers: Vec::new(),
            });
            return Ok(true);
        }
        if active_removed {
            self.active = self.first_layout_pane();
            self.send_focus(self.active, true)?;
        }
        self.refresh_layout()?;
        Ok(true)
    }

    fn split(&mut self, axis: SplitAxis) -> Result<()> {
        let active = self
            .active
            .ok_or_else(|| anyhow::anyhow!("workspace has no pane to split"))?;
        if self.panes.len() >= 4 {
            bail!("workspace supports at most four panes");
        }
        let new_id = self.next_id;
        let mut layout = self
            .layout
            .clone()
            .ok_or_else(|| anyhow::anyhow!("workspace has no layout"))?;
        if !layout.split_leaf(active, axis, new_id) {
            bail!("workspace layout has no pane {active}");
        }
        let geometry = geometry(&layout, self.cols, self.rows)?;
        let rect = geometry
            .panes
            .iter()
            .find(|pane| pane.id == new_id)
            .map(|pane| pane.rect)
            .context("new pane has no layout rectangle")?;
        let pane = self.spawn_pane(new_id, rect)?;
        self.panes.push(pane);
        if let Err(error) = self.apply_geometry(geometry) {
            if let Some(mut pane) = self.panes.pop() {
                let _ = pane.session.stop();
            }
            return Err(error);
        }
        self.next_id += 1;
        self.layout = Some(layout);
        self.focus_pane(new_id)
    }

    pub(crate) fn set_grid(&mut self, columns: u16, rows: u16) -> Result<()> {
        if !(1..=2).contains(&columns) || !(1..=2).contains(&rows) {
            bail!("workspace grids support one or two columns and rows");
        }
        let desired = usize::from(columns) * usize::from(rows);
        if desired < self.panes.len() {
            bail!(
                "grid {columns}x{rows} has {desired} cells but workspace has {} panes; close panes explicitly first",
                self.panes.len()
            );
        }
        let mut ids = self.panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
        ids.sort_unstable();
        while ids.len() < desired {
            ids.push(self.next_id + u32::try_from(ids.len() - self.panes.len()).unwrap_or(0));
        }
        let layout = grid_layout(&ids, columns, rows)?;
        if self.layout.as_ref() == Some(&layout) {
            return Ok(());
        }
        let geometry = geometry(&layout, self.cols, self.rows)?;
        let mut added = Vec::new();
        for id in ids
            .iter()
            .copied()
            .filter(|id| self.pane_index(*id).is_none())
        {
            let rect = geometry
                .panes
                .iter()
                .find(|pane| pane.id == id)
                .map(|pane| pane.rect)
                .context("new pane has no layout rectangle")?;
            added.push(self.spawn_pane(id, rect)?);
        }
        let added_len = added.len();
        self.panes.extend(added);
        if let Err(error) = self.apply_geometry(geometry) {
            for _ in 0..added_len {
                if let Some(mut pane) = self.panes.pop() {
                    let _ = pane.session.stop();
                }
            }
            return Err(error);
        }
        self.next_id += u32::try_from(added_len).unwrap_or(0);
        self.layout = Some(layout);
        Ok(())
    }

    fn focus_direction(&mut self, direction: Direction) -> Result<bool> {
        let active = match self.active {
            Some(active) => active,
            None => return Ok(false),
        };
        let panes = &self.applied.geometry().panes;
        let Some(current) = panes.iter().find(|pane| pane.id == active) else {
            return Ok(false);
        };
        let target = panes
            .iter()
            .filter(|pane| pane.id != active)
            .filter_map(|pane| directional_score(current.rect, pane.rect, pane.id, direction))
            .min_by_key(|score| *score)
            .map(|(_, _, _, pane)| pane);
        match target {
            Some(target) => {
                self.focus_pane(target)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub(crate) fn focus_pane(&mut self, pane: PaneId) -> Result<()> {
        self.resolve_pane(Some(pane))?;
        if self.active == Some(pane) {
            return Ok(());
        }
        self.send_focus(self.active, false)?;
        self.active = Some(pane);
        self.send_focus(self.active, true)
    }

    fn pane_at(&self, x: u16, y: u16) -> Option<(PaneId, u16, u16)> {
        self.applied.geometry().panes.iter().find_map(|pane| {
            let local_x = x.checked_sub(pane.rect.x)?;
            let local_y = y.checked_sub(pane.rect.y)?;
            (local_x < pane.rect.cols && local_y < pane.rect.rows)
                .then_some((pane.id, local_x, local_y))
        })
    }

    fn pane_position(&self, pane: PaneId, x: u16, y: u16) -> Option<(PaneId, u16, u16)> {
        let rect = self
            .applied
            .geometry()
            .panes
            .iter()
            .find(|placed| placed.id == pane)?
            .rect;
        let local_x = x.saturating_sub(rect.x).min(rect.cols.checked_sub(1)?);
        let local_y = y.saturating_sub(rect.y).min(rect.rows.checked_sub(1)?);
        Some((pane, local_x, local_y))
    }

    pub(crate) fn close_pane(&mut self, pane: PaneId) -> Result<()> {
        let index = self.resolve_pane(Some(pane))?;
        let closing_active = self.active == Some(pane);
        if closing_active {
            self.send_focus(Some(pane), false)?;
        }
        self.remove_layout_pane(pane);
        if self.paste.is_some_and(|(target, _)| target == pane) {
            self.paste = None;
        }
        let mut pane = self.panes.remove(index);
        pane.session.stop()?;
        if self.panes.is_empty() {
            self.active = None;
            self.layout = None;
            self.applied = AppliedLayout::Ready(WorkspaceGeometry {
                panes: Vec::new(),
                dividers: Vec::new(),
            });
            return Ok(());
        }
        if closing_active {
            self.active = self.first_layout_pane();
        }
        self.refresh_layout()?;
        if closing_active {
            self.send_focus(self.active, true)?;
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

    fn send_to_if_open(&mut self, pane: PaneId, input: &[u8]) -> Result<bool> {
        let index = self.resolve_pane(Some(pane))?;
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
        let Some((target, _)) = self.paste else {
            return Ok(true);
        };
        let index = self.resolve_pane(Some(target))?;
        self.panes[index].session.send_current_if_open(input)
    }

    pub(crate) fn end_paste(&mut self) -> Result<bool> {
        let Some((target, bracketed)) = self.paste.take() else {
            return Ok(true);
        };
        if bracketed {
            let index = self.resolve_pane(Some(target))?;
            return self.panes[index].session.send_current_if_open(PASTE_END);
        }
        Ok(true)
    }

    fn cancel_paste(&mut self) {
        let Some((target, bracketed)) = self.paste.take() else {
            return;
        };
        if bracketed && let Some(index) = self.pane_index(target) {
            let _ = self.panes[index].session.send_current_if_open(PASTE_END);
        }
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
        self.cols = cols;
        self.rows = rows;
        self.options.cols = cols;
        self.options.rows = rows;
        self.options.cell_width = cell_width;
        self.options.cell_height = cell_height;
        self.refresh_layout()
    }

    pub(crate) fn frame(&mut self) -> Result<Frame> {
        if self.applied.is_constrained() {
            return self.constrained_frame();
        }
        let mut frames = Vec::with_capacity(self.applied.geometry().panes.len());
        for placed in &self.applied.geometry().panes {
            let index = self
                .pane_index(placed.id)
                .ok_or_else(|| anyhow::anyhow!("layout references missing pane {}", placed.id))?;
            frames.push((placed.id, self.panes[index].session.current_frame()?));
        }
        Ok(compose_workspace(
            self.cols,
            self.rows,
            self.applied.geometry(),
            &frames,
            self.active,
        ))
    }

    fn constrained_frame(&mut self) -> Result<Frame> {
        let index = self.resolve_pane(None)?;
        let mut frame = self.panes[index].session.current_frame()?;
        frame.cols = self.cols;
        frame.rows = self.rows;
        frame.cells.retain(|cell| {
            cell.x < frame.cols
                && cell.y < frame.rows
                && cell.x.saturating_add(cell.width) <= frame.cols
        });
        frame.cursor = frame
            .cursor
            .filter(|cursor| cursor.x < frame.cols && cursor.y < frame.rows);
        add_overlay(&mut frame, "layout too small");
        Ok(frame)
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
        for pane in &mut self.panes {
            let status = pane.session.status()?;
            let rect = self
                .applied
                .geometry()
                .panes
                .iter()
                .find(|placed| placed.id == pane.id)
                .map(|placed| placed.rect)
                .context("pane has no applied layout rectangle")?;
            statuses.push(PaneStatus {
                id: pane.id,
                active: self.active == Some(pane.id),
                state: status.state,
                x: rect.x,
                y: rect.y,
                cols: status.cols,
                rows: status.rows,
                title: pane.session.title()?,
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
        let modes = self.panes[index].session.input_modes()?;
        Ok(outer_input_modes(modes, self.panes.len()))
    }

    fn pane_input_modes(&self, pane: PaneId) -> Result<InputModes> {
        let index = self.resolve_pane(Some(pane))?;
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
        self.active
            .and_then(|active| self.pane_index(active))
            .map_or(libghostty_vt::render::CursorVisualStyle::Block, |index| {
                self.panes[index].session.cursor_style()
            })
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
        self.paste = None;
        for pane in &mut self.panes {
            let _ = pane.session.stop();
        }
        self.panes.clear();
        self.active = None;
        self.layout = None;
        self.applied = AppliedLayout::Ready(WorkspaceGeometry {
            panes: Vec::new(),
            dividers: Vec::new(),
        });
    }

    fn refresh_layout(&mut self) -> Result<()> {
        let Some(layout) = &self.layout else {
            self.applied = AppliedLayout::Ready(WorkspaceGeometry {
                panes: Vec::new(),
                dividers: Vec::new(),
            });
            return Ok(());
        };
        match geometry(layout, self.cols, self.rows) {
            Ok(geometry) => self.apply_geometry(geometry),
            Err(_) => {
                self.applied = AppliedLayout::Constrained(self.applied.geometry().clone());
                Ok(())
            }
        }
    }

    fn apply_geometry(&mut self, geometry: WorkspaceGeometry) -> Result<()> {
        let previous = self.applied.geometry().clone();
        for placed in &geometry.panes {
            let index = self
                .pane_index(placed.id)
                .ok_or_else(|| anyhow::anyhow!("layout references missing pane {}", placed.id))?;
            if let Err(error) = self.panes[index].session.resize(
                placed.rect.cols,
                placed.rect.rows,
                self.options.cell_width,
                self.options.cell_height,
            ) {
                for previous in &previous.panes {
                    if let Some(index) = self.pane_index(previous.id) {
                        let _ = self.panes[index].session.resize(
                            previous.rect.cols,
                            previous.rect.rows,
                            self.options.cell_width,
                            self.options.cell_height,
                        );
                    }
                }
                return Err(error);
            }
        }
        self.applied = AppliedLayout::Ready(geometry);
        Ok(())
    }

    fn spawn_pane(&self, id: PaneId, rect: PaneRect) -> Result<Pane> {
        let mut options = self.options.clone();
        options.cols = rect.cols;
        options.rows = rect.rows;
        Ok(Pane {
            id,
            session: Session::start_with_theme(
                &self.shell,
                Some(&self.cwd),
                None,
                &options,
                self.theme,
            )?,
        })
    }

    fn remove_layout_pane(&mut self, pane: PaneId) {
        if let Some(layout) = self.layout.take() {
            let (layout, removed) = layout.remove_leaf(pane);
            debug_assert!(removed, "pane collection and layout tree diverged");
            self.layout = layout;
        }
        self.applied
            .geometry_mut()
            .panes
            .retain(|placed| placed.id != pane);
    }

    fn first_layout_pane(&self) -> Option<PaneId> {
        self.layout.as_ref().map(LayoutNode::first_leaf)
    }

    fn pane_index(&self, pane: PaneId) -> Option<usize> {
        self.panes.iter().position(|candidate| candidate.id == pane)
    }

    fn send_focus(&mut self, pane: Option<PaneId>, focused: bool) -> Result<()> {
        let Some(index) = pane.and_then(|pane| self.pane_index(pane)) else {
            return Ok(());
        };
        if self.panes[index].session.input_modes()?.focus_events {
            self.panes[index].session.send_current_if_open(if focused {
                b"\x1b[I"
            } else {
                b"\x1b[O"
            })?;
        }
        Ok(())
    }

    fn resolve_pane(&self, pane: Option<PaneId>) -> Result<usize> {
        match pane {
            Some(id) => self
                .pane_index(id)
                .ok_or_else(|| anyhow::anyhow!("workspace has no pane {id}")),
            None => self
                .active
                .and_then(|active| self.pane_index(active))
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

fn geometry(layout: &LayoutNode, cols: u16, rows: u16) -> Result<WorkspaceGeometry> {
    let mut geometry = WorkspaceGeometry {
        panes: Vec::new(),
        dividers: Vec::new(),
    };
    place_layout(
        layout,
        PaneRect {
            x: 0,
            y: 0,
            cols,
            rows,
        },
        &mut geometry,
    )?;
    Ok(geometry)
}

fn grid_layout(panes: &[PaneId], columns: u16, rows: u16) -> Result<LayoutNode> {
    match (columns, rows, panes) {
        (1, 1, [pane]) => Ok(LayoutNode::Leaf(*pane)),
        (2, 1, [left, right]) => Ok(LayoutNode::Split {
            axis: SplitAxis::Columns,
            first: Box::new(LayoutNode::Leaf(*left)),
            second: Box::new(LayoutNode::Leaf(*right)),
        }),
        (1, 2, [top, bottom]) => Ok(LayoutNode::Split {
            axis: SplitAxis::Rows,
            first: Box::new(LayoutNode::Leaf(*top)),
            second: Box::new(LayoutNode::Leaf(*bottom)),
        }),
        (2, 2, [top_left, top_right, bottom_left, bottom_right]) => Ok(LayoutNode::Split {
            axis: SplitAxis::Rows,
            first: Box::new(LayoutNode::Split {
                axis: SplitAxis::Columns,
                first: Box::new(LayoutNode::Leaf(*top_left)),
                second: Box::new(LayoutNode::Leaf(*top_right)),
            }),
            second: Box::new(LayoutNode::Split {
                axis: SplitAxis::Columns,
                first: Box::new(LayoutNode::Leaf(*bottom_left)),
                second: Box::new(LayoutNode::Leaf(*bottom_right)),
            }),
        }),
        _ => bail!("workspace grids support 1x1, 2x1, 1x2, or 2x2"),
    }
}

fn place_layout(
    layout: &LayoutNode,
    rect: PaneRect,
    geometry: &mut WorkspaceGeometry,
) -> Result<()> {
    match layout {
        LayoutNode::Leaf(id) => geometry.panes.push(PlacedPane { id: *id, rect }),
        LayoutNode::Split {
            axis,
            first,
            second,
        } => {
            let (first_rect, second_rect, divider) = match axis {
                SplitAxis::Columns if rect.cols >= 3 => {
                    let first_cols = (rect.cols - 1) / 2;
                    (
                        PaneRect {
                            cols: first_cols,
                            ..rect
                        },
                        PaneRect {
                            x: rect.x + first_cols + 1,
                            cols: rect.cols - first_cols - 1,
                            ..rect
                        },
                        Divider {
                            axis: *axis,
                            x: rect.x + first_cols,
                            y: rect.y,
                            len: rect.rows,
                        },
                    )
                }
                SplitAxis::Rows if rect.rows >= 3 => {
                    let first_rows = (rect.rows - 1) / 2;
                    (
                        PaneRect {
                            rows: first_rows,
                            ..rect
                        },
                        PaneRect {
                            y: rect.y + first_rows + 1,
                            rows: rect.rows - first_rows - 1,
                            ..rect
                        },
                        Divider {
                            axis: *axis,
                            x: rect.x,
                            y: rect.y + first_rows,
                            len: rect.cols,
                        },
                    )
                }
                SplitAxis::Columns => bail!("layout needs more columns"),
                SplitAxis::Rows => bail!("layout needs more rows"),
            };
            geometry.dividers.push(divider);
            place_layout(first, first_rect, geometry)?;
            place_layout(second, second_rect, geometry)?;
        }
    }
    Ok(())
}

fn compose_workspace(
    cols: u16,
    rows: u16,
    geometry: &WorkspaceGeometry,
    frames: &[(PaneId, Frame)],
    active: Option<PaneId>,
) -> Frame {
    let active_frame = active.and_then(|active| {
        frames
            .iter()
            .find(|(pane, _)| *pane == active)
            .map(|(_, frame)| frame)
    });
    let foreground = active_frame.map_or(DEFAULT_FOREGROUND, |frame| frame.foreground);
    let background = active_frame.map_or(DEFAULT_BACKGROUND, |frame| frame.background);
    let divider_cells = geometry
        .dividers
        .iter()
        .map(|divider| usize::from(divider.len))
        .sum::<usize>();
    let mut cells = Vec::with_capacity(
        frames
            .iter()
            .map(|(_, frame)| frame.cells.len())
            .sum::<usize>()
            + divider_cells,
    );
    for placed in &geometry.panes {
        let Some((_, frame)) = frames.iter().find(|(pane, _)| *pane == placed.id) else {
            continue;
        };
        if frame.background != background {
            for y in 0..placed.rect.rows {
                cells.push(Cell {
                    x: placed.rect.x,
                    y: placed.rect.y + y,
                    text: String::new(),
                    width: placed.rect.cols,
                    foreground: frame.foreground,
                    background: frame.background,
                    attributes: Attributes::default(),
                });
            }
        }
        for cell in &frame.cells {
            if cell.x >= placed.rect.cols
                || cell.y >= placed.rect.rows
                || cell.x.saturating_add(cell.width) > placed.rect.cols
            {
                continue;
            }
            let mut cell = cell.clone();
            cell.x += placed.rect.x;
            cell.y += placed.rect.y;
            cells.push(cell);
        }
    }
    let mut divider_cells = BTreeMap::new();
    for divider in &geometry.dividers {
        for offset in 0..divider.len {
            let x = divider.x
                + if divider.axis == SplitAxis::Rows {
                    offset
                } else {
                    0
                };
            let y = divider.y
                + if divider.axis == SplitAxis::Columns {
                    offset
                } else {
                    0
                };
            divider_cells
                .entry((x, y))
                .and_modify(|axes| *axes |= divider_axis(divider.axis))
                .or_insert_with(|| divider_axis(divider.axis));
        }
    }
    for (&(x, y), &axes) in &divider_cells {
        let left = x > 0 && divider_cells.contains_key(&(x - 1, y));
        let right = x + 1 < cols && divider_cells.contains_key(&(x + 1, y));
        let up = y > 0 && divider_cells.contains_key(&(x, y - 1));
        let down = y + 1 < rows && divider_cells.contains_key(&(x, y + 1));
        cells.push(Cell {
            x,
            y,
            text: divider_glyph(axes, left, right, up, down).to_owned(),
            width: 1,
            foreground,
            background,
            attributes: divider_attributes(),
        });
    }
    let cursor = active
        .and_then(|active| {
            geometry.panes.iter().find(|pane| pane.id == active).zip(
                frames
                    .iter()
                    .find(|(pane, _)| *pane == active)
                    .map(|(_, frame)| frame),
            )
        })
        .and_then(|(placed, frame)| {
            frame.cursor.as_ref().and_then(|cursor| {
                (cursor.x < placed.rect.cols && cursor.y < placed.rect.rows).then(|| Cursor {
                    x: placed.rect.x + cursor.x,
                    y: placed.rect.y + cursor.y,
                    color: cursor.color,
                    blinking: cursor.blinking,
                })
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

fn divider_axis(axis: SplitAxis) -> u8 {
    match axis {
        SplitAxis::Columns => 1,
        SplitAxis::Rows => 2,
    }
}

fn divider_glyph(axes: u8, left: bool, right: bool, up: bool, down: bool) -> &'static str {
    match (left, right, up, down) {
        (true, true, true, true) => "┼",
        (true, true, false, true) => "┬",
        (true, true, true, false) => "┴",
        (false, true, true, true) => "├",
        (true, false, true, true) => "┤",
        (false, true, false, true) => "┌",
        (true, false, false, true) => "┐",
        (false, true, true, false) => "└",
        (true, false, true, false) => "┘",
        _ if axes & 1 != 0 => VERTICAL_DIVIDER,
        _ => HORIZONTAL_DIVIDER,
    }
}

fn divider_attributes() -> Attributes {
    Attributes {
        faint: true,
        ..Attributes::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InputAction {
    Send(Vec<u8>),
    PasteStart,
    PasteData(Vec<u8>),
    PasteEnd,
    Split(SplitAxis),
    Focus(Direction),
    Mouse {
        input: Vec<u8>,
        position: Option<(u16, u16)>,
        primary_press: bool,
        capture_start: bool,
        captured_event: bool,
        capture_end: bool,
    },
    CloseActive,
    Detach,
    PaneNumbers,
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
            if self.waiting {
                if let Some((direction, length)) = prefix_arrow(remaining) {
                    flush_plain(&mut actions, &mut plain);
                    actions.push(InputAction::Focus(direction));
                    self.waiting = false;
                    index += length;
                    continue;
                }
                if is_prefix_arrow_start(remaining) {
                    self.pending.extend_from_slice(remaining);
                    self.pending_since = Some(Instant::now());
                    break;
                }
            }
            if remaining.starts_with(SGR_MOUSE_PREFIX) {
                if let Some(end) = remaining
                    .iter()
                    .position(|byte| matches!(byte, b'M' | b'm'))
                {
                    flush_plain(&mut actions, &mut plain);
                    actions.push(sgr_mouse_action(&remaining[..=end]));
                    index += end + 1;
                    continue;
                }
                self.pending.extend_from_slice(remaining);
                self.pending_since = Some(Instant::now());
                break;
            }
            if SGR_MOUSE_PREFIX.starts_with(remaining) {
                self.pending.extend_from_slice(remaining);
                self.pending_since = Some(Instant::now());
                break;
            }
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
                    b'%' => InputAction::Split(SplitAxis::Columns),
                    b'"' => InputAction::Split(SplitAxis::Rows),
                    b'h' => InputAction::Focus(Direction::Left),
                    b'l' => InputAction::Focus(Direction::Right),
                    b'k' => InputAction::Focus(Direction::Up),
                    b'j' => InputAction::Focus(Direction::Down),
                    b'x' => InputAction::CloseActive,
                    b'd' => InputAction::Detach,
                    b'q' => InputAction::PaneNumbers,
                    b'&' => InputAction::Quit,
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
        if pending.starts_with(SGR_MOUSE_PREFIX) {
            return Vec::new();
        }
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

fn prefix_arrow(bytes: &[u8]) -> Option<(Direction, usize)> {
    let direction = match bytes.get(..3)? {
        b"\x1b[D" | b"\x1bOD" => Direction::Left,
        b"\x1b[C" | b"\x1bOC" => Direction::Right,
        b"\x1b[A" | b"\x1bOA" => Direction::Up,
        b"\x1b[B" | b"\x1bOB" => Direction::Down,
        _ => return None,
    };
    Some((direction, 3))
}

fn is_prefix_arrow_start(bytes: &[u8]) -> bool {
    const ARROWS: [&[u8]; 8] = [
        b"\x1b[D", b"\x1bOD", b"\x1b[C", b"\x1bOC", b"\x1b[A", b"\x1bOA", b"\x1b[B", b"\x1bOB",
    ];
    ARROWS.iter().any(|arrow| arrow.starts_with(bytes))
}

fn sgr_mouse_action(bytes: &[u8]) -> InputAction {
    let parsed = (|| {
        let final_byte = *bytes.last()?;
        let body = std::str::from_utf8(&bytes[SGR_MOUSE_PREFIX.len()..bytes.len() - 1]).ok()?;
        let mut fields = body.split(';');
        let button = fields.next()?.parse::<u16>().ok()?;
        let x = fields.next()?.parse::<u16>().ok()?.checked_sub(1)?;
        let y = fields.next()?.parse::<u16>().ok()?.checked_sub(1)?;
        if fields.next().is_some() {
            return None;
        }
        let wheel = button & 0b1100_0000 != 0;
        let motion = button & 0b0010_0000 != 0;
        let press = final_byte == b'M' && !wheel && !motion && button & 0b11 != 3;
        let captured_event = (motion && button & 0b11 != 3) || final_byte == b'm';
        Some((
            (x, y),
            press && button & 0b11 == 0,
            press,
            captured_event,
            final_byte == b'm',
        ))
    })();
    InputAction::Mouse {
        input: bytes.to_vec(),
        position: parsed.map(|(position, _, _, _, _)| position),
        primary_press: parsed.is_some_and(|(_, primary_press, _, _, _)| primary_press),
        capture_start: parsed.is_some_and(|(_, _, capture_start, _, _)| capture_start),
        captured_event: parsed
            .is_some_and(|(_, _, _, captured_event, capture_end)| captured_event || capture_end),
        capture_end: parsed.is_some_and(|(_, _, _, _, capture_end)| capture_end),
    }
}

fn translate_mouse(input: &[u8], x: u16, y: u16, sgr: bool) -> Option<Vec<u8>> {
    if !input.starts_with(SGR_MOUSE_PREFIX) || input.len() <= SGR_MOUSE_PREFIX.len() + 1 {
        return None;
    }
    let final_byte = *input.last()?;
    let body = std::str::from_utf8(&input[SGR_MOUSE_PREFIX.len()..input.len() - 1]).ok()?;
    let mut button = body.split(';').next()?.parse::<u16>().ok()?;
    if sgr {
        return Some(
            format!(
                "\x1b[<{button};{};{}{}",
                x + 1,
                y + 1,
                char::from(final_byte)
            )
            .into_bytes(),
        );
    }
    if final_byte == b'm' {
        button = (button & !0b11) | 0b11;
    }
    Some(vec![
        0x1b,
        b'[',
        b'M',
        u8::try_from(button).ok()?.checked_add(32)?,
        u8::try_from(x).ok()?.checked_add(33)?,
        u8::try_from(y).ok()?.checked_add(33)?,
    ])
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

    fn arm(&mut self, action: ArmedAction, prompt: &str) {
        self.armed = Some((action, Instant::now() + Duration::from_secs(5)));
        self.notice(prompt, Duration::from_secs(5));
    }

    fn confirmation(&mut self, input: &[u8]) -> Option<Option<ArmedAction>> {
        let (action, expires) = self.armed?;
        if expires < Instant::now() {
            self.clear_armed();
            return None;
        }
        match input.first() {
            Some(b'y' | b'Y') => {
                self.armed = None;
                self.notice = None;
                Some(Some(action))
            }
            Some(b'n' | b'N' | 0x1b) => {
                self.clear_armed();
                Some(None)
            }
            _ => None,
        }
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

pub(crate) struct WorkspaceTerminal {
    attachment: Option<WorkspaceAttachment>,
    decoder: PrefixDecoder,
    ui: WorkspaceUi,
    mouse_target: Option<PaneId>,
    pending_removal: bool,
    finished: bool,
}

struct WorkspaceAttachment {
    id: u64,
    input: Receiver<Vec<u8>>,
    screen: OuterScreen,
}

pub(crate) struct WorkspaceAttachmentOptions {
    pub(crate) id: u64,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) cell_width: u16,
    pub(crate) cell_height: u16,
    pub(crate) theme: TerminalTheme,
}

impl WorkspaceTerminal {
    pub(crate) fn detached() -> Self {
        Self {
            attachment: None,
            decoder: PrefixDecoder::default(),
            ui: WorkspaceUi::new(),
            mouse_target: None,
            pending_removal: false,
            finished: false,
        }
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.attachment.is_some()
    }

    pub(crate) fn attach(
        &mut self,
        workspace: &mut Workspace,
        input: Receiver<Vec<u8>>,
        writer: Box<dyn Write + Send>,
        options: WorkspaceAttachmentOptions,
    ) -> Result<()> {
        if self.is_attached() {
            bail!("workspace already has an attached terminal");
        }
        workspace.set_theme(options.theme)?;
        workspace.resize(
            options.cols,
            options.rows,
            options.cell_width,
            options.cell_height,
        )?;
        self.decoder = PrefixDecoder::default();
        self.ui = WorkspaceUi::new();
        self.mouse_target = None;
        self.attachment = Some(WorkspaceAttachment {
            id: options.id,
            input,
            screen: OuterScreen::enter(writer)?,
        });
        Ok(())
    }

    pub(crate) fn resize_attachment(
        &mut self,
        workspace: &mut Workspace,
        id: u64,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> Result<()> {
        if self.attachment.as_ref().map(|attachment| attachment.id) != Some(id) {
            bail!("attachment is no longer active");
        }
        workspace.resize(cols, rows, cell_width, cell_height)?;
        Ok(())
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
        let bells = workspace.take_bells();
        let mut attachment = self.attachment.take();
        if let Some(attached) = attachment.as_mut() {
            let result = self.tick_attachment(workspace, attached, exited, bells);
            match result {
                Ok(true) => {}
                Ok(false) => {
                    workspace.cancel_paste();
                    attachment = None;
                }
                Err(error) if attachment_closed(&error) => {
                    workspace.cancel_paste();
                    attachment = None;
                }
                Err(error) => return Err(error),
            }
        }
        self.attachment = attachment;
        if workspace.is_empty() {
            self.finished = true;
            return Ok(false);
        }
        if exited && self.attachment.is_none() {
            let _final_frame = workspace.frame()?;
        }
        if exited {
            if workspace.all_exits_observed() {
                self.finished = true;
                return Ok(false);
            }
            self.pending_removal = true;
        }
        Ok(true)
    }

    fn tick_attachment(
        &mut self,
        workspace: &mut Workspace,
        attachment: &mut WorkspaceAttachment,
        exited: bool,
        bells: u64,
    ) -> Result<bool> {
        if bells > 0 {
            attachment.screen.bell()?;
        }
        attachment.screen.sync_title(&workspace.active_title()?)?;
        if !exited {
            let mut actions = Vec::new();
            loop {
                match attachment.input.try_recv() {
                    Ok(input) => actions.extend(self.decoder.push(&input)),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(false),
                }
            }
            actions.extend(self.decoder.flush_ambiguous(Duration::from_millis(25)));
            for action in actions {
                match action {
                    InputAction::Send(input) => {
                        if let Some(confirmation) = self.ui.confirmation(&input) {
                            match confirmation {
                                Some(ArmedAction::Close(pane)) => {
                                    match workspace.close_pane(pane) {
                                        Ok(()) => self.ui.notice(
                                            format!("pane {pane} killed"),
                                            Duration::from_millis(1_200),
                                        ),
                                        Err(error) => {
                                            attachment.screen.bell()?;
                                            self.ui.notice(
                                                error.to_string(),
                                                Duration::from_millis(1_500),
                                            );
                                        }
                                    }
                                }
                                Some(ArmedAction::Quit) => workspace.stop(),
                                None => self.ui.notice("canceled", Duration::from_millis(1_000)),
                            }
                            if workspace.is_empty() {
                                return Ok(true);
                            }
                            continue;
                        }
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
                    InputAction::Split(split) => {
                        self.ui.clear_armed();
                        match workspace.split(split) {
                            Ok(()) => self.ui.notice(
                                format!("pane {} active", workspace.active_id().unwrap_or(0)),
                                Duration::from_millis(1_200),
                            ),
                            Err(error) => {
                                attachment.screen.bell()?;
                                self.ui
                                    .notice(error.to_string(), Duration::from_millis(1_500));
                            }
                        }
                    }
                    InputAction::Focus(direction) => {
                        self.ui.clear_armed();
                        if workspace.focus_direction(direction)? {
                            self.ui.notice(
                                format!("pane {} active", workspace.active_id().unwrap_or(0)),
                                Duration::from_millis(1_000),
                            );
                        } else {
                            self.ui
                                .notice(direction.unavailable(), Duration::from_millis(1_000));
                        }
                    }
                    InputAction::Mouse {
                        input,
                        position,
                        primary_press,
                        capture_start,
                        captured_event,
                        capture_end,
                    } => {
                        if workspace.is_multi_pane() || self.mouse_target.is_some() {
                            let target = position.and_then(|(x, y)| {
                                if captured_event {
                                    self.mouse_target
                                        .and_then(|pane| workspace.pane_position(pane, x, y))
                                } else {
                                    workspace.pane_at(x, y)
                                }
                            });
                            if capture_end {
                                self.mouse_target = None;
                            }
                            if let Some((pane, local_x, local_y)) = target {
                                if capture_start {
                                    self.mouse_target = Some(pane);
                                }
                                if primary_press {
                                    self.ui.clear_armed();
                                    workspace.focus_pane(pane)?;
                                }
                                let modes = workspace.pane_input_modes(pane)?;
                                if (modes.normal_mouse || modes.button_mouse || modes.any_mouse)
                                    && let Some(input) =
                                        translate_mouse(&input, local_x, local_y, modes.sgr_mouse)
                                    && !workspace.send_to_if_open(pane, &input)?
                                {
                                    workspace.observe_exits()?;
                                    break;
                                }
                            }
                        } else if !workspace.send_active_if_open(&input)? {
                            workspace.observe_exits()?;
                            break;
                        }
                    }
                    InputAction::CloseActive => {
                        let pane = workspace.active_id().unwrap_or(0);
                        let prompt = if workspace.panes.len() == 1 {
                            format!("kill final pane {pane} and end workspace? (y/n)")
                        } else {
                            format!("kill pane {pane}? (y/n)")
                        };
                        self.ui.arm(ArmedAction::Close(pane), &prompt);
                    }
                    InputAction::Detach => return Ok(false),
                    InputAction::PaneNumbers => {
                        self.ui.clear_armed();
                        let mut panes = workspace
                            .panes
                            .iter()
                            .map(|pane| pane.id)
                            .collect::<Vec<_>>();
                        panes.sort_unstable();
                        let active = workspace.active_id();
                        let panes = panes
                            .into_iter()
                            .map(|pane| {
                                if active == Some(pane) {
                                    format!("[{pane}]")
                                } else {
                                    pane.to_string()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("  ");
                        self.ui.notice(
                            format!("panes: {panes}  active pane in brackets"),
                            Duration::from_secs(4),
                        );
                    }
                    InputAction::Quit => {
                        self.ui
                            .arm(ArmedAction::Quit, "kill workspace and all panes? (y/n)");
                    }
                    InputAction::Help => {
                        self.ui.clear_armed();
                        self.ui.notice(
                            "^B % side  \" stack  h/j/k/l focus  q panes  x pane  d detach  & all",
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
                        attachment.screen.bell()?;
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
                    return Ok(true);
                }
            }
        }
        attachment
            .screen
            .sync_input_modes(workspace.active_input_modes()?)?;
        let mut frame = workspace.frame()?;
        attachment.screen.sync_cursor_style(
            workspace.active_cursor_style(),
            frame.cursor.as_ref().is_some_and(|cursor| cursor.blinking),
        )?;
        if let Some(overlay) = self.ui.overlay(self.decoder.waiting()) {
            add_overlay(&mut frame, &overlay);
        }
        attachment.screen.paint(&frame)?;
        Ok(true)
    }
}

fn attachment_closed(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let Some(error) = cause.downcast_ref::<std::io::Error>() else {
            return false;
        };
        matches!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::UnexpectedEof
        ) || error.raw_os_error() == Some(libc::EIO)
    })
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
    writer: Box<dyn Write + Send>,
    modes: InputModes,
    previous: Option<Frame>,
    title: String,
    cursor_style: Option<(libghostty_vt::render::CursorVisualStyle, bool)>,
    output: Vec<u8>,
}

impl OuterScreen {
    pub(crate) fn enter(mut writer: Box<dyn Write + Send>) -> Result<Self> {
        writer
            .write_all(b"\x1b[22;0t\x1b[?1049h\x1b[?2004h\x1b[?25l\x1b[2J\x1b[H")
            .context("enter workspace screen")?;
        writer.flush().context("flush workspace screen")?;
        Ok(Self {
            writer,
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
        write_input_modes(&mut self.output, modes)?;
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
        self.writer
            .write_all(&self.output)
            .context("write workspace update")?;
        self.writer.flush().context("flush workspace frame")?;
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
        let _ = self.writer.write_all(&self.output);
        let _ = self
            .writer
            .write_all(
                b"\x1b[?2026l\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l",
            );
        let _ = self
            .writer
            .write_all(b"\x1b[0 q\x1b[0m\x1b[?25h\x1b[?1049l\x1b[23;0t");
        let _ = self.writer.flush();
    }
}

fn set_dec_mode(writer: &mut impl Write, mode: u16, enabled: bool) -> Result<()> {
    write!(writer, "\x1b[?{mode}{}", if enabled { 'h' } else { 'l' })
        .context("set workspace terminal mode")
}

fn write_input_modes(writer: &mut impl Write, modes: InputModes) -> Result<()> {
    set_dec_mode(writer, 1, modes.cursor_keys)?;
    writer
        .write_all(if modes.keypad_keys {
            b"\x1b="
        } else {
            b"\x1b>"
        })
        .context("set workspace keypad mode")?;
    set_dec_mode(writer, 1000, modes.normal_mouse)?;
    set_dec_mode(writer, 1002, modes.button_mouse)?;
    set_dec_mode(writer, 1003, modes.any_mouse)?;
    set_dec_mode(writer, 1006, modes.sgr_mouse)?;
    set_dec_mode(writer, 1004, modes.focus_events)?;
    Ok(())
}

fn outer_input_modes(modes: InputModes, pane_count: usize) -> InputModes {
    if pane_count == 1 {
        modes
    } else {
        let mut modes = modes;
        modes.normal_mouse = true;
        modes.sgr_mouse = true;
        modes
    }
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
    fn single_pane_workspace_mirrors_child_mouse_modes() {
        let modes = InputModes {
            normal_mouse: true,
            button_mouse: true,
            any_mouse: true,
            sgr_mouse: true,
            ..InputModes::default()
        };
        let mut output = Vec::new();

        write_input_modes(&mut output, outer_input_modes(modes, 1)).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[?1000h"));
        assert!(output.contains("\x1b[?1002h"));
        assert!(output.contains("\x1b[?1003h"));
        assert!(output.contains("\x1b[?1006h"));
    }

    #[test]
    fn split_workspace_adds_click_focus_without_disabling_child_mouse_modes() {
        let modes = InputModes {
            normal_mouse: true,
            button_mouse: true,
            any_mouse: true,
            sgr_mouse: true,
            ..InputModes::default()
        };

        let outer = outer_input_modes(modes, 2);

        assert!(outer.normal_mouse);
        assert!(outer.button_mouse);
        assert!(outer.any_mouse);
        assert!(outer.sgr_mouse);
    }

    #[test]
    fn two_pane_layout_reserves_one_divider_column() {
        let layout = grid_layout(&[0, 1], 2, 1).unwrap();
        assert_eq!(
            geometry(&layout, 80, 24).unwrap().panes,
            [
                PlacedPane {
                    id: 0,
                    rect: PaneRect {
                        x: 0,
                        y: 0,
                        cols: 39,
                        rows: 24
                    },
                },
                PlacedPane {
                    id: 1,
                    rect: PaneRect {
                        x: 40,
                        y: 0,
                        cols: 40,
                        rows: 24
                    },
                }
            ]
        );
    }

    #[test]
    fn composition_offsets_right_cells_and_active_cursor() {
        let layout = grid_layout(&[0, 1], 2, 1).unwrap();
        let geometry = geometry(&layout, 11, 3).unwrap();
        let composed = compose_workspace(
            11,
            3,
            &geometry,
            &[
                (0, frame(5, 3, "L", Some((1, 1)))),
                (1, frame(5, 3, "R", Some((2, 2)))),
            ],
            Some(1),
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
    fn stacked_layout_reserves_one_divider_row_and_offsets_the_bottom_pane() {
        let layout = grid_layout(&[0, 1], 1, 2).unwrap();
        let geometry = geometry(&layout, 8, 5).unwrap();
        assert_eq!(
            geometry.panes,
            [
                PlacedPane {
                    id: 0,
                    rect: PaneRect {
                        x: 0,
                        y: 0,
                        cols: 8,
                        rows: 2,
                    },
                },
                PlacedPane {
                    id: 1,
                    rect: PaneRect {
                        x: 0,
                        y: 3,
                        cols: 8,
                        rows: 2,
                    },
                },
            ]
        );
        let composed = compose_workspace(
            8,
            5,
            &geometry,
            &[
                (0, frame(8, 2, "T", Some((1, 1)))),
                (1, frame(8, 2, "B", Some((2, 1)))),
            ],
            Some(1),
        );

        assert_eq!(composed.cursor.as_ref().unwrap().y, 4);
        assert_eq!(composed.text(), "T\n\n────────\nB");
    }

    #[test]
    fn stacked_layout_offsets_bottom_background_spans_and_rejects_short_screens() {
        assert_eq!(
            geometry(&grid_layout(&[0, 1], 1, 2).unwrap(), 8, 2)
                .unwrap_err()
                .to_string(),
            "layout needs more rows"
        );
        assert_eq!(
            geometry(&grid_layout(&[0, 1], 2, 1).unwrap(), 2, 8)
                .unwrap_err()
                .to_string(),
            "layout needs more columns"
        );

        let layout = grid_layout(&[0, 1], 1, 2).unwrap();
        let geometry = geometry(&layout, 8, 5).unwrap();
        let top = frame(8, 2, "T", None);
        let mut bottom = frame(8, 2, "B", None);
        bottom.background = crate::frame::Color { r: 1, g: 2, b: 3 };
        let composed = compose_workspace(8, 5, &geometry, &[(0, top), (1, bottom)], Some(0));

        assert!(composed.cells.iter().any(|cell| {
            cell.y == 3
                && cell.width == 8
                && cell.background == crate::frame::Color { r: 1, g: 2, b: 3 }
        }));
        assert!(!composed.cells.iter().any(|cell| {
            cell.y < 3 && cell.background == crate::frame::Color { r: 1, g: 2, b: 3 }
        }));
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
        assert_eq!(decoder.push(b"%"), [InputAction::Split(SplitAxis::Columns)]);
        assert_eq!(
            decoder.push(&[PREFIX, b'"']),
            [InputAction::Split(SplitAxis::Rows)]
        );
        assert_eq!(
            decoder.push(&[PREFIX, b'j']),
            [InputAction::Focus(Direction::Down)]
        );
        assert_eq!(
            decoder.push(&[PREFIX, b'k']),
            [InputAction::Focus(Direction::Up)]
        );
        assert_eq!(decoder.push(&[PREFIX, b'd']), [InputAction::Detach]);
        assert_eq!(decoder.push(&[PREFIX, b'q']), [InputAction::PaneNumbers]);
        assert_eq!(decoder.push(&[PREFIX, b'&']), [InputAction::Quit]);
        assert!(decoder.push(&[PREFIX, 0x1b]).is_empty());
        assert!(decoder.push(b"[").is_empty());
        assert_eq!(decoder.push(b"A"), [InputAction::Focus(Direction::Up)]);
        assert_eq!(
            decoder.push(&[PREFIX, 0x1b, b'O', b'D']),
            [InputAction::Focus(Direction::Left)]
        );
        assert_eq!(decoder.push(&[PREFIX, b'z']), [InputAction::Unknown(b'z')]);
        assert_eq!(
            decoder.push(&[PREFIX, PREFIX]),
            [InputAction::Send(vec![PREFIX])]
        );
    }

    #[test]
    fn prefix_decoder_turns_chunked_left_clicks_into_workspace_focus() {
        let mut decoder = PrefixDecoder::default();

        assert!(decoder.push(b"\x1b[<0;42").is_empty());
        assert_eq!(
            decoder.push(b";13M"),
            [InputAction::Mouse {
                input: b"\x1b[<0;42;13M".to_vec(),
                position: Some((41, 12)),
                primary_press: true,
                capture_start: true,
                captured_event: false,
                capture_end: false,
            }]
        );
        assert_eq!(
            decoder.push(b"\x1b[<0;42;13m"),
            [InputAction::Mouse {
                input: b"\x1b[<0;42;13m".to_vec(),
                position: Some((41, 12)),
                primary_press: false,
                capture_start: false,
                captured_event: true,
                capture_end: true,
            }]
        );
        assert_eq!(
            decoder.push(b"\x1b[<64;42;13M"),
            [InputAction::Mouse {
                input: b"\x1b[<64;42;13M".to_vec(),
                position: Some((41, 12)),
                primary_press: false,
                capture_start: false,
                captured_event: false,
                capture_end: false,
            }]
        );
        assert_eq!(
            decoder.push(b"\x1b[<32;42;13M"),
            [InputAction::Mouse {
                input: b"\x1b[<32;42;13M".to_vec(),
                position: Some((41, 12)),
                primary_press: false,
                capture_start: false,
                captured_event: true,
                capture_end: false,
            }]
        );
        assert_eq!(
            translate_mouse(b"\x1b[<64;42;13M", 2, 3, true).as_deref(),
            Some(b"\x1b[<64;3;4M".as_slice())
        );
        assert_eq!(
            translate_mouse(b"\x1b[<0;42;13M", 2, 3, false).as_deref(),
            Some(b"\x1b[M #$".as_slice())
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
        assert_eq!(
            decoder.push(b"\x02%"),
            [InputAction::Split(SplitAxis::Columns)]
        );
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
    fn destructive_confirmation_accepts_yes_and_rejects_no() {
        let mut ui = WorkspaceUi::new();

        ui.arm(ArmedAction::Quit, "confirm quit");
        assert_eq!(ui.overlay(false).as_deref(), Some("confirm quit"));
        assert_eq!(ui.confirmation(b"n\r"), Some(None));
        assert_eq!(ui.overlay(false), None);
        ui.arm(ArmedAction::Close(3), "confirm close");
        assert_eq!(ui.confirmation(b"y\r"), Some(Some(ArmedAction::Close(3))));
        assert_eq!(ui.overlay(false), None);
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
        workspace.split(SplitAxis::Columns).unwrap();
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
        assert_eq!(workspace.status().unwrap().cols, 2);
        assert_eq!(workspace.frame().unwrap().cols, 2);
        workspace.resize(21, 4, 9, 18).unwrap();
        assert!(workspace.frame().unwrap().text().contains("LEFT"));
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
    fn real_workspace_stacks_panes_and_focuses_vertically() {
        let options = Options {
            cols: 21,
            rows: 7,
            ..Options::default()
        };
        let mut workspace = Workspace::start(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf TOP; cat".to_owned(),
            ],
            None,
            None,
            &options,
        )
        .unwrap();
        workspace.shell = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf BOTTOM; cat".to_owned(),
        ];
        workspace.split(SplitAxis::Rows).unwrap();
        workspace.send(Some(1), b"AGENT\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = loop {
            workspace.pump().unwrap();
            let frame = workspace.frame().unwrap();
            let text = frame.text();
            if text.contains("TOP") && text.contains("BOTTOM") && text.contains("AGENT") {
                break frame;
            }
            assert!(Instant::now() < deadline, "workspace output did not arrive");
            std::thread::sleep(Duration::from_millis(10));
        };

        assert!(frame.text().contains("─────────────────────"));
        assert_eq!(workspace.active_id(), Some(1));
        assert!(workspace.focus_direction(Direction::Up).unwrap());
        assert_eq!(workspace.active_id(), Some(0));
        assert!(workspace.focus_direction(Direction::Down).unwrap());
        assert_eq!(workspace.active_id(), Some(1));
        assert_eq!(
            workspace
                .panes()
                .unwrap()
                .into_iter()
                .map(|pane| (pane.cols, pane.rows))
                .collect::<Vec<_>>(),
            [(21, 3), (21, 3)]
        );
        workspace.resize(21, 9, 9, 18).unwrap();
        assert_eq!(
            workspace
                .panes()
                .unwrap()
                .into_iter()
                .map(|pane| (pane.cols, pane.rows))
                .collect::<Vec<_>>(),
            [(21, 4), (21, 4)]
        );
        workspace.resize(21, 2, 9, 18).unwrap();
        assert_eq!(workspace.status().unwrap().rows, 2);
        workspace.set_grid(1, 2).unwrap();
        assert_eq!(workspace.panes().unwrap()[1].y, 5);
        let constrained = workspace.frame().unwrap();
        assert_eq!(constrained.rows, 2);
        assert!(
            constrained.text().contains("layout too small"),
            "constrained frame: {:?}",
            constrained.text()
        );
        workspace.resize(21, 9, 9, 18).unwrap();
        let restored = workspace.frame().unwrap().text();
        assert!(restored.contains("TOP"));
        assert!(restored.contains("BOTTOM"));
        workspace
            .close_pane(workspace.active_id().unwrap())
            .unwrap();
        let panes = workspace.panes().unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!((panes[0].cols, panes[0].rows), (21, 9));
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
        workspace.split(SplitAxis::Columns).unwrap();

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

    #[test]
    fn grid_geometry_places_four_stable_pane_ids_and_recursive_dividers() {
        let layout = grid_layout(&[0, 1, 2, 3], 2, 2).unwrap();
        let geometry = geometry(&layout, 11, 7).unwrap();

        assert_eq!(
            geometry.panes,
            [
                PlacedPane {
                    id: 0,
                    rect: PaneRect {
                        x: 0,
                        y: 0,
                        cols: 5,
                        rows: 3,
                    },
                },
                PlacedPane {
                    id: 1,
                    rect: PaneRect {
                        x: 6,
                        y: 0,
                        cols: 5,
                        rows: 3,
                    },
                },
                PlacedPane {
                    id: 2,
                    rect: PaneRect {
                        x: 0,
                        y: 4,
                        cols: 5,
                        rows: 3,
                    },
                },
                PlacedPane {
                    id: 3,
                    rect: PaneRect {
                        x: 6,
                        y: 4,
                        cols: 5,
                        rows: 3,
                    },
                },
            ]
        );
        assert_eq!(geometry.dividers.len(), 3);
        let composed = compose_workspace(
            11,
            7,
            &geometry,
            &[
                (0, frame(5, 3, "0", Some((0, 0)))),
                (1, frame(5, 3, "1", Some((0, 0)))),
                (2, frame(5, 3, "2", Some((0, 0)))),
                (3, frame(5, 3, "3", Some((2, 1)))),
            ],
            Some(3),
        );
        assert_eq!(
            composed.cursor.as_ref().map(|cursor| (cursor.x, cursor.y)),
            Some((8, 5))
        );
        assert!(composed.text().contains('0'));
        assert!(composed.text().contains('3'));
        let junction = composed
            .cells
            .iter()
            .find(|cell| (cell.x, cell.y) == (5, 3))
            .unwrap();
        assert_eq!(junction.text, "┼");
        assert!(junction.attributes.faint);
    }

    #[cfg(unix)]
    #[test]
    fn semantic_grid_focus_and_close_share_one_layout_tree() {
        let options = Options {
            cols: 41,
            rows: 13,
            ..Options::default()
        };
        let mut workspace = Workspace::start(
            &["sh".to_owned(), "-c".to_owned(), "cat".to_owned()],
            None,
            None,
            &options,
        )
        .unwrap();
        workspace.shell = vec!["sh".to_owned(), "-c".to_owned(), "cat".to_owned()];

        workspace.set_grid(2, 2).unwrap();
        let panes = workspace.panes().unwrap();
        assert_eq!(
            panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert_eq!(workspace.active_id(), Some(0));
        assert_eq!(
            panes
                .iter()
                .map(|pane| (pane.x, pane.y, pane.cols, pane.rows))
                .collect::<Vec<_>>(),
            [(0, 0, 20, 6), (21, 0, 20, 6), (0, 7, 20, 6), (21, 7, 20, 6),]
        );
        assert!(workspace.set_grid(2, 1).is_err());
        workspace.focus_pane(3).unwrap();
        assert_eq!(workspace.active_id(), Some(3));
        workspace.focus_pane(0).unwrap();
        assert_eq!(workspace.active_id(), Some(0));
        workspace.focus_pane(3).unwrap();
        assert_eq!(workspace.active_id(), Some(3));
        assert_eq!(workspace.pane_at(22, 8), Some((3, 1, 1)));
        assert_eq!(workspace.pane_at(20, 6), None);
        assert_eq!(workspace.pane_position(0, 22, 8), Some((0, 19, 5)));
        workspace.close_pane(1).unwrap();
        assert_eq!(workspace.active_id(), Some(3));
        assert_eq!(workspace.panes().unwrap().len(), 3);
        assert!(workspace.focus_direction(Direction::Up).unwrap());
        assert_eq!(workspace.active_id(), Some(0));
        workspace.begin_paste().unwrap();
        workspace.close_pane(0).unwrap();
        assert!(workspace.send_paste(b"ignored after close").unwrap());
        assert!(workspace.end_paste().unwrap());
        workspace.stop();
    }
}
