use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context, Result};
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::render::{CellIterator, CursorVisualStyle, Dirty, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{PaletteIndex, RgbColor, Underline as GhosttyUnderline};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};

use crate::frame::{
    Attributes, Cell, Color, Cursor, CursorStyle, DEFAULT_BACKGROUND, DEFAULT_FOREGROUND,
    FORMAT_VERSION, Frame, Underline, indexed_color,
};

pub(crate) const SCROLLBACK_ROWS: usize = 10_000;

pub(crate) struct TerminalCore {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    responses: Rc<RefCell<Vec<u8>>>,
    cursor_style: CursorVisualStyle,
    cached_frame: Option<Frame>,
}

impl TerminalCore {
    pub(crate) fn new(rows: u16, cols: u16, max_scrollback: usize) -> Result<Self> {
        let responses = Rc::new(RefCell::new(Vec::new()));
        let mut terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback,
        })
        .context("create Ghostty terminal")?;
        terminal
            .on_pty_write({
                let responses = Rc::clone(&responses);
                move |_terminal, bytes| responses.borrow_mut().extend_from_slice(bytes)
            })
            .context("configure Ghostty PTY responses")?;
        apply_theme(&mut terminal)?;

        Ok(Self {
            terminal,
            render_state: RenderState::new().context("create Ghostty render state")?,
            rows: RowIterator::new().context("create Ghostty row iterator")?,
            cells: CellIterator::new().context("create Ghostty cell iterator")?,
            responses,
            cursor_style: CursorVisualStyle::Block,
            cached_frame: None,
        })
    }

    pub(crate) fn apply_output(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.terminal.vt_write(bytes);
        std::mem::take(&mut self.responses.borrow_mut())
    }

    pub(crate) fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> Result<()> {
        self.terminal
            .resize(cols, rows, u32::from(cell_width), u32::from(cell_height))
            .context("resize Ghostty terminal")?;
        self.cached_frame = None;
        Ok(())
    }

    pub(crate) fn text(&mut self) -> Result<String> {
        Ok(self.frame()?.text())
    }

    pub(crate) fn scrollback_text(&self) -> Result<Vec<u8>> {
        let options = FormatterOptions::new()
            .with_format(Format::Plain)
            .with_trim(true);
        let mut formatter = Formatter::new(&self.terminal, options)
            .context("create Ghostty plain-text formatter")?;
        let bytes = formatter
            .format_alloc(None)
            .context("format Ghostty scrollback")?;
        Ok(bytes.as_ref().to_vec())
    }

    pub(crate) fn frame(&mut self) -> Result<Frame> {
        let snapshot = self
            .render_state
            .update(&self.terminal)
            .context("update Ghostty render state")?;
        let dirty = snapshot.dirty().context("read Ghostty dirty state")?;
        let next_cursor_style = snapshot
            .cursor_visual_style()
            .context("read Ghostty cursor style")?;
        let cursor_style_changed = self.cursor_style != next_cursor_style;
        self.cursor_style = next_cursor_style;
        if dirty == Dirty::Clean
            && !cursor_style_changed
            && let Some(frame) = &self.cached_frame
        {
            return Ok(frame.clone());
        }
        let cols = snapshot.cols().context("read Ghostty columns")?;
        let rows = snapshot.rows().context("read Ghostty rows")?;
        let colors = snapshot.colors().context("read Ghostty colors")?;
        let foreground = from_ghostty_color(colors.foreground);
        let background = from_ghostty_color(colors.background);
        let cursor = if snapshot
            .cursor_visible()
            .context("read Ghostty cursor visibility")?
        {
            snapshot
                .cursor_viewport()
                .context("read Ghostty cursor position")?
                .map(|cursor| Cursor {
                    x: cursor.x,
                    y: cursor.y,
                    color: from_ghostty_color(colors.cursor.unwrap_or(colors.foreground)),
                    blinking: snapshot.cursor_blinking().unwrap_or(false),
                    style: frame_cursor_style(self.cursor_style),
                })
        } else {
            None
        };

        let full = dirty == Dirty::Full
            || self
                .cached_frame
                .as_ref()
                .is_none_or(|frame| frame.cols != cols || frame.rows != rows);
        let mut changed_rows = vec![false; usize::from(rows)];
        let mut changed_cells = Vec::new();
        let mut row_index = 0_u16;
        let mut row_iter = self
            .rows
            .update(&snapshot)
            .context("iterate Ghostty rows")?;
        while let Some(row) = row_iter.next() {
            let row_dirty = full || row.dirty().context("read Ghostty row dirty state")?;
            row.set_dirty(false)
                .context("clear Ghostty row dirty state")?;
            if !row_dirty {
                row_index += 1;
                continue;
            }
            if row_index < rows {
                changed_rows[usize::from(row_index)] = true;
            }
            let mut column = 0_u16;
            let mut cell_iter = self.cells.update(row).context("iterate Ghostty cells")?;
            while let Some(cell) = cell_iter.next() {
                let raw = cell.raw_cell().context("read Ghostty cell")?;
                let wide = raw.wide().context("read Ghostty cell width")?;
                if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                    column += 1;
                    continue;
                }
                let style = cell.style().context("read Ghostty cell style")?;
                let mut cell_foreground = cell
                    .fg_color()
                    .context("read Ghostty cell foreground")?
                    .map_or(foreground, from_ghostty_color);
                let mut cell_background = cell
                    .bg_color()
                    .context("read Ghostty cell background")?
                    .map_or(background, from_ghostty_color);
                if style.inverse {
                    std::mem::swap(&mut cell_foreground, &mut cell_background);
                }
                let attributes = Attributes {
                    bold: style.bold,
                    italic: style.italic,
                    faint: style.faint,
                    invisible: style.invisible,
                    strikethrough: style.strikethrough,
                    overline: style.overline,
                    underline: (style.underline != GhosttyUnderline::None)
                        .then_some(Underline::Single),
                };
                let mut text = String::new();
                cell.graphemes_utf8(&mut text)
                    .context("read Ghostty cell text")?;
                if !text.is_empty() || cell_background != background || has_attributes(&attributes)
                {
                    changed_cells.push(Cell {
                        x: column,
                        y: row_index,
                        text,
                        width: if wide == CellWide::Wide { 2 } else { 1 },
                        foreground: cell_foreground,
                        background: cell_background,
                        attributes,
                    });
                }
                column += 1;
            }
            row_index += 1;
        }
        snapshot
            .set_dirty(Dirty::Clean)
            .context("clear Ghostty dirty state")?;

        let frame = if full {
            Frame {
                version: FORMAT_VERSION,
                cols,
                rows,
                foreground,
                background,
                cursor,
                cells: changed_cells,
            }
        } else {
            let mut frame = self.cached_frame.take().expect("partial frame has cache");
            frame.cols = cols;
            frame.rows = rows;
            frame.foreground = foreground;
            frame.background = background;
            frame.cursor = cursor;
            frame
                .cells
                .retain(|cell| !changed_rows[usize::from(cell.y)]);
            frame.cells.extend(changed_cells);
            frame.cells.sort_unstable_by_key(|cell| (cell.y, cell.x));
            frame
        };
        self.cached_frame = Some(frame.clone());
        Ok(frame)
    }
}

fn apply_theme(terminal: &mut Terminal<'static, 'static>) -> Result<()> {
    terminal
        .set_default_fg_color(Some(to_ghostty_color(DEFAULT_FOREGROUND)))
        .context("configure Ghostty foreground")?
        .set_default_bg_color(Some(to_ghostty_color(DEFAULT_BACKGROUND)))
        .context("configure Ghostty background")?
        .set_default_cursor_color(Some(to_ghostty_color(DEFAULT_FOREGROUND)))
        .context("configure Ghostty cursor")?;
    let mut palette = terminal
        .default_color_palette()
        .context("read Ghostty color palette")?;
    for index in 0..=u8::MAX {
        palette.set(PaletteIndex(index), to_ghostty_color(indexed_color(index)));
    }
    terminal
        .set_default_color_palette(Some(palette))
        .context("configure Ghostty color palette")?;
    Ok(())
}

fn has_attributes(attributes: &Attributes) -> bool {
    attributes.bold
        || attributes.italic
        || attributes.faint
        || attributes.invisible
        || attributes.strikethrough
        || attributes.overline
        || attributes.underline.is_some()
}

fn to_ghostty_color(color: Color) -> RgbColor {
    RgbColor {
        r: color.r,
        g: color.g,
        b: color.b,
    }
}

fn from_ghostty_color(color: RgbColor) -> Color {
    Color {
        r: color.r,
        g: color.g,
        b: color.b,
    }
}

fn frame_cursor_style(style: CursorVisualStyle) -> CursorStyle {
    match style {
        CursorVisualStyle::Block => CursorStyle::Block,
        CursorVisualStyle::BlockHollow => CursorStyle::Hollow,
        CursorVisualStyle::Underline => CursorStyle::Underline,
        CursorVisualStyle::Bar => CursorStyle::Bar,
        _ => CursorStyle::Block,
    }
}

#[cfg(test)]
mod tests {
    use super::TerminalCore;
    use crate::frame::{CursorStyle, DEFAULT_BACKGROUND, DEFAULT_FOREGROUND, indexed_color};

    #[test]
    fn captures_terminal_responses() {
        let mut terminal = TerminalCore::new(1, 20, 0).unwrap();

        assert_eq!(terminal.apply_output(b"\x1b[5n"), b"\x1b[0n");
    }

    #[test]
    fn updates_cursor_style_without_cell_changes() {
        let mut terminal = TerminalCore::new(1, 20, 0).unwrap();
        terminal.frame().unwrap();

        terminal.apply_output(b"\x1b[6 q");
        let frame = terminal.frame().unwrap();

        assert_eq!(frame.cursor.unwrap().style, CursorStyle::Bar);
    }

    #[test]
    fn deterministic_theme_configures_defaults_and_ansi_palette() {
        let mut terminal = TerminalCore::new(1, 4, 0).unwrap();
        let _responses = terminal.apply_output(b"A\x1b[31mB\x1b[91mC");
        let frame = terminal.frame().unwrap();

        assert_eq!(frame.foreground, DEFAULT_FOREGROUND);
        assert_eq!(frame.background, DEFAULT_BACKGROUND);
        assert_eq!(frame.cells[0].foreground, DEFAULT_FOREGROUND);
        assert_eq!(frame.cells[1].foreground, indexed_color(1));
        assert_eq!(frame.cells[2].foreground, indexed_color(9));
    }

    #[test]
    fn resize_reflows_existing_content() {
        let mut terminal = TerminalCore::new(3, 10, 100).unwrap();
        let _responses = terminal.apply_output(b"alpha beta");
        terminal.resize(5, 3, 9, 18).unwrap();

        assert_eq!(terminal.frame().unwrap().text(), "alpha\n beta");
    }

    #[test]
    fn preserves_wide_graphemes_in_structured_frames() {
        let mut terminal = TerminalCore::new(1, 10, 0).unwrap();
        let _responses = terminal.apply_output("A界e\u{301}".as_bytes());
        let frame = terminal.frame().unwrap();

        assert_eq!(frame.text(), "A界e\u{301}");
        assert_eq!(frame.cells[1].text, "界");
        assert_eq!(frame.cells[1].width, 2);
        assert_eq!(frame.cells[2].text, "e\u{301}");
    }

    #[test]
    fn repeated_frames_are_complete_snapshots() {
        let mut terminal = TerminalCore::new(1, 10, 0).unwrap();
        let _responses = terminal.apply_output(b"ready");

        assert_eq!(terminal.frame().unwrap().text(), "ready");
        assert_eq!(terminal.frame().unwrap().text(), "ready");
    }

    #[test]
    fn retained_render_state_merges_dirty_rows_into_complete_frames() {
        let mut terminal = TerminalCore::new(3, 10, 0).unwrap();
        let _responses = terminal.apply_output(b"alpha\r\nbeta");
        assert_eq!(terminal.frame().unwrap().text(), "alpha\nbeta");

        let _responses = terminal.apply_output(b"\rB");
        let updated = terminal.frame().unwrap();
        assert_eq!(updated.text(), "alpha\nBeta");
        assert_eq!(terminal.frame().unwrap(), updated);

        let _responses = terminal.apply_output(b"\r\x1b[2K");
        assert_eq!(terminal.frame().unwrap().text(), "alpha");
    }

    #[test]
    #[ignore = "manual autoresearch benchmark"]
    fn benchmark_repeated_complete_frames() {
        let mut terminal = TerminalCore::new(44, 160, 0).unwrap();
        let line = format!("{}\r\n", "x".repeat(159));
        for _ in 0..43 {
            let _responses = terminal.apply_output(line.as_bytes());
        }
        let _warmup = terminal.frame().unwrap();
        let started = std::time::Instant::now();
        for _ in 0..1_000 {
            std::hint::black_box(terminal.frame().unwrap());
        }
        println!(
            "METRIC repeated_frame_median_proxy_us={:.1}",
            started.elapsed().as_secs_f64() * 1_000.0
        );
    }
}
