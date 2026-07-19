use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context, Result};
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{PaletteIndex, RgbColor, Underline as GhosttyUnderline};
use libghostty_vt::{RenderState, Terminal, TerminalOptions, terminal::Mode};

use crate::frame::{
    Attributes, Cell, Color, Cursor, DEFAULT_BACKGROUND, DEFAULT_FOREGROUND, FORMAT_VERSION, Frame,
    Underline, indexed_color,
};

pub(crate) const SCROLLBACK_ROWS: usize = 10_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InputModes {
    pub cursor_keys: bool,
    pub keypad_keys: bool,
    pub focus_events: bool,
    pub bracketed_paste: bool,
}

pub(crate) struct TerminalCore {
    terminal: Terminal<'static, 'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    responses: Rc<RefCell<Vec<u8>>>,
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

        Ok(Self {
            terminal,
            rows: RowIterator::new().context("create Ghostty row iterator")?,
            cells: CellIterator::new().context("create Ghostty cell iterator")?,
            responses,
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
            .context("resize Ghostty terminal")
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

    pub(crate) fn input_modes(&self) -> Result<InputModes> {
        Ok(InputModes {
            cursor_keys: self.terminal.mode(Mode::DECCKM)?,
            keypad_keys: self.terminal.mode(Mode::KEYPAD_KEYS)?,
            focus_events: self.terminal.mode(Mode::FOCUS_EVENT)?,
            bracketed_paste: self.terminal.mode(Mode::BRACKETED_PASTE)?,
        })
    }

    pub(crate) fn frame(&mut self) -> Result<Frame> {
        // Frame v1 is a complete snapshot, while a reused Ghostty render state exposes dirty
        // updates. Start a fresh render state so repeated captures include unchanged rows.
        let mut render_state = RenderState::new().context("create Ghostty render state")?;
        let snapshot = render_state
            .update(&self.terminal)
            .context("update Ghostty render state")?;
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
                })
        } else {
            None
        };

        let mut frame_cells = Vec::new();
        let mut row_index = 0_u16;
        let mut row_iter = self
            .rows
            .update(&snapshot)
            .context("iterate Ghostty rows")?;
        while let Some(row) = row_iter.next() {
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
                    frame_cells.push(Cell {
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

        Ok(Frame {
            version: FORMAT_VERSION,
            cols,
            rows,
            foreground,
            background,
            cursor,
            cells: frame_cells,
        })
    }
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

#[cfg(test)]
mod tests {
    use super::TerminalCore;

    #[test]
    fn captures_terminal_responses() {
        let mut terminal = TerminalCore::new(1, 20, 0).unwrap();

        assert_eq!(terminal.apply_output(b"\x1b[5n"), b"\x1b[0n");
    }

    #[test]
    fn exposes_input_modes_requested_by_the_application() {
        let mut terminal = TerminalCore::new(1, 20, 0).unwrap();
        let _responses = terminal.apply_output(b"\x1b[?1h\x1b=\x1b[?1004h\x1b[?2004h");

        let modes = terminal.input_modes().unwrap();
        assert!(modes.cursor_keys);
        assert!(modes.keypad_keys);
        assert!(modes.focus_events);
        assert!(modes.bracketed_paste);
    }

    #[test]
    fn resize_reflows_existing_content() {
        let mut terminal = TerminalCore::new(3, 10, 100).unwrap();
        let _responses = terminal.apply_output(b"alpha beta");
        terminal.resize(5, 3, 9, 18).unwrap();

        assert_eq!(terminal.frame().unwrap().text(), "alpha\n beta");
    }

    #[test]
    fn preserves_wide_graphemes_in_frame_v1() {
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
}
