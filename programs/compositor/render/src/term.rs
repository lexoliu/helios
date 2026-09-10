//! The terminal the desktop boots into: a grid of character cells, a
//! scrollback behind it, and the shell whose bytes fill both.
//!
//! This is a character grid and nothing more. It knows how to place a
//! byte the shell printed, how to scroll when the last line is full, and
//! how to skip an escape sequence it does not implement so that a
//! colouring `ls` prints words rather than punctuation. It does not know
//! what a pixel is: [`Terminal::render_row`] is given a frame buffer and
//! writes the glyphs of one row into it, and everything about where that
//! row sits on the desktop belongs to the caller.
//!
//! # What scrolls and what is kept
//!
//! Lines are appended, never overwritten in place, so the screen is the
//! last [`Terminal::rows`] of them and everything before is scrollback.
//! [`SCROLLBACK_LINES`] of it is kept, and the oldest line is dropped
//! when a new one would take the count past that: a terminal that kept
//! everything would grow without bound in a program that is meant to run
//! for the life of the machine.

use std::collections::VecDeque;
use std::vec::Vec;

use crate::font::{self, CELL_HEIGHT, CELL_WIDTH};
use crate::paint::{Colour, blend};

/// Lines kept behind the screen.
pub const SCROLLBACK_LINES: usize = 512;

/// Where a horizontal tab lands.
const TAB_WIDTH: usize = 8;

/// What the terminal is in the middle of reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Escape {
    /// Ordinary text.
    None,
    /// An `ESC` was seen and the next byte says what kind.
    Introducer,
    /// A `CSI` sequence, which ends at a byte in `0x40..=0x7e`.
    ControlSequence,
    /// An `OSC` string, which ends at `BEL` or at `ESC \`.
    OperatingSystem,
    /// An `OSC` string that has seen its terminating `ESC`.
    OperatingSystemEnd,
    /// A two-byte sequence whose second byte carries no meaning here.
    Discard,
}

/// One line of the terminal, as characters.
type Line = Vec<char>;

/// A character grid with a scrollback and a cursor.
pub struct Terminal {
    columns: usize,
    rows: usize,
    /// Every line, oldest first. The last `rows` are on screen.
    lines: VecDeque<Line>,
    /// Where the next character goes on the last line.
    column: usize,
    escape: Escape,
    /// Bytes of a character whose encoding is not complete yet.
    pending: Vec<u8>,
    /// Which visible rows have changed since the last render.
    dirty: Vec<bool>,
}

impl Terminal {
    /// An empty terminal of `columns` by `rows` cells.
    ///
    /// # Panics
    ///
    /// Panics on a grid with no cells: a terminal of no columns could
    /// never place a character, and the caller sized it from a window
    /// that has to be at least one cell across to be worth drawing.
    pub fn new(columns: usize, rows: usize) -> Self {
        assert!(
            columns > 0 && rows > 0,
            "a terminal grid is at least one cell, not {columns}x{rows}"
        );
        let mut lines = VecDeque::with_capacity(rows);
        lines.push_back(Line::new());
        Self {
            columns,
            rows,
            lines,
            column: 0,
            escape: Escape::None,
            pending: Vec::new(),
            dirty: vec![true; rows],
        }
    }

    pub const fn columns(&self) -> usize {
        self.columns
    }

    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// How many lines are behind the screen.
    pub fn scrollback(&self) -> usize {
        self.lines.len().saturating_sub(self.rows)
    }

    /// Where the cursor is on screen, as a `(column, row)` cell.
    pub fn cursor(&self) -> (usize, usize) {
        let row = self.lines.len().min(self.rows) - 1;
        (self.column.min(self.columns - 1), row)
    }

    /// The characters of one visible row, top row first.
    ///
    /// Empty for a row the terminal has not reached yet, which is every
    /// row of a terminal that has just started.
    pub fn visible_row(&self, row: usize) -> &[char] {
        let first = self.lines.len().saturating_sub(self.rows);
        self.lines
            .get(first + row)
            .map_or(&[][..], |line| line.as_slice())
    }

    /// Which rows have changed since the last time this was asked.
    pub fn take_dirty_rows(&mut self) -> Vec<usize> {
        let rows = self
            .dirty
            .iter()
            .enumerate()
            .filter_map(|(row, dirty)| dirty.then_some(row))
            .collect();
        self.dirty.iter_mut().for_each(|dirty| *dirty = false);
        rows
    }

    /// Mark every row as needing a redraw.
    pub fn dirty_all(&mut self) {
        self.dirty.iter_mut().for_each(|dirty| *dirty = true);
    }

    /// Take everything the shell printed.
    ///
    /// The bytes are a stream, so a character split across two reads is
    /// held until the rest of it arrives rather than drawn as the
    /// substitute.
    pub fn write(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        loop {
            match core::str::from_utf8(&self.pending) {
                Ok(text) => {
                    let characters: Vec<char> = text.chars().collect();
                    self.pending.clear();
                    for character in characters {
                        self.put(character);
                    }
                    return;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    let characters: Vec<char> = core::str::from_utf8(&self.pending[..valid])
                        .expect("the prefix the decoder reported as valid is valid")
                        .chars()
                        .collect();
                    for character in characters {
                        self.put(character);
                    }
                    match error.error_len() {
                        // The rest of this character has not arrived yet.
                        None => {
                            self.pending.drain(..valid);
                            return;
                        }
                        // A byte that cannot start or continue a
                        // character. The terminal shows the shell's
                        // output, so it shows that something was there.
                        Some(length) => {
                            self.pending.drain(..valid + length);
                            self.put(char::REPLACEMENT_CHARACTER);
                        }
                    }
                }
            }
        }
    }

    /// Place one character, or act on it when it is a control.
    fn put(&mut self, character: char) {
        match self.escape {
            Escape::None => {}
            Escape::Introducer => {
                self.escape = match character {
                    '[' => Escape::ControlSequence,
                    ']' => Escape::OperatingSystem,
                    // A designator: the byte after it names a character
                    // set this terminal has exactly one of.
                    '(' | ')' | '*' | '+' | '#' | '%' => Escape::Discard,
                    _ => Escape::None,
                };
                return;
            }
            Escape::ControlSequence => {
                if matches!(character, '\u{40}'..='\u{7e}') {
                    self.escape = Escape::None;
                }
                return;
            }
            Escape::OperatingSystem => {
                self.escape = match character {
                    '\u{07}' => Escape::None,
                    '\u{1b}' => Escape::OperatingSystemEnd,
                    _ => Escape::OperatingSystem,
                };
                return;
            }
            Escape::OperatingSystemEnd => {
                self.escape = if character == '\\' {
                    Escape::None
                } else {
                    Escape::OperatingSystem
                };
                return;
            }
            Escape::Discard => {
                self.escape = Escape::None;
                return;
            }
        }

        match character {
            '\u{1b}' => self.escape = Escape::Introducer,
            '\n' => self.newline(),
            '\r' => self.column = 0,
            '\u{08}' => self.backspace(),
            '\t' => {
                let target = ((self.column / TAB_WIDTH) + 1) * TAB_WIDTH;
                for _ in self.column..target.min(self.columns) {
                    self.place(' ');
                }
            }
            character if font::is_printable(character) => self.place(character),
            // Every other control byte — a bell, a shift-out — is
            // something this terminal has no cell for. Dropping it is
            // what it means for a grid to show text.
            _ => {}
        }
    }

    /// Put a printable character in the current cell and advance.
    fn place(&mut self, character: char) {
        if self.column >= self.columns {
            self.newline();
        }
        let column = self.column;
        let line = self
            .lines
            .back_mut()
            .expect("a terminal always has a line to write on");
        if line.len() <= column {
            line.resize(column + 1, ' ');
        }
        line[column] = character;
        self.column += 1;
        self.mark_last_row();
    }

    fn backspace(&mut self) {
        if self.column == 0 {
            return;
        }
        // A backspace moves the cursor and rubs out nothing. Erasing
        // here would rub out two cells for every one the shell means to
        // erase, because what a shell sends to erase a character is a
        // backspace, a space, and a backspace again.
        self.column -= 1;
        self.mark_last_row();
    }

    fn newline(&mut self) {
        self.column = 0;
        let was_full = self.lines.len() >= self.rows;
        self.lines.push_back(Line::new());
        if self.lines.len() > SCROLLBACK_LINES + self.rows {
            self.lines.pop_front();
        }
        if was_full {
            // Everything on screen moved up by a row, so everything on
            // screen has to be drawn again.
            self.dirty_all();
        } else {
            self.mark_last_row();
        }
    }

    fn mark_last_row(&mut self) {
        let row = self.lines.len().min(self.rows) - 1;
        self.dirty[row] = true;
    }

    /// Draw one visible row into `target`.
    ///
    /// `target` is a frame buffer of `stride` bytes per row in the
    /// display's four-byte pixels; `origin` is the pixel the terminal's
    /// top-left cell sits at. Only the row's own strip is touched, which
    /// is what makes a keystroke cost one glyph cell of traffic rather
    /// than a screen.
    pub fn render_row(
        &self,
        row: usize,
        target: &mut [u8],
        stride: usize,
        origin: (usize, usize),
        foreground: Colour,
        background: Colour,
    ) {
        let characters = self.visible_row(row);
        let top = origin.1 + row * CELL_HEIGHT;
        for column in 0..self.columns {
            let left = origin.0 + column * CELL_WIDTH;
            let character = characters.get(column).copied().unwrap_or(' ');
            draw_cell(target, stride, left, top, character, foreground, background);
        }
    }

    /// Draw the block the cursor sits under.
    pub fn render_cursor(
        &self,
        target: &mut [u8],
        stride: usize,
        origin: (usize, usize),
        colour: Colour,
    ) {
        let (column, row) = self.cursor();
        let left = origin.0 + column * CELL_WIDTH;
        let top = origin.1 + row * CELL_HEIGHT;
        for y in 0..CELL_HEIGHT {
            let start = (top + y) * stride + left * 4;
            let Some(strip) = target.get_mut(start..start + CELL_WIDTH * 4) else {
                return;
            };
            for pixel in strip.chunks_exact_mut(4) {
                colour.write(pixel);
            }
        }
    }
}

/// Draw one character cell, glyph over background.
fn draw_cell(
    target: &mut [u8],
    stride: usize,
    left: usize,
    top: usize,
    character: char,
    foreground: Colour,
    background: Colour,
) {
    let raster = font::glyph(character).raster();
    for y in 0..CELL_HEIGHT {
        let start = (top + y) * stride + left * 4;
        let Some(strip) = target.get_mut(start..start + CELL_WIDTH * 4) else {
            return;
        };
        let coverage = raster.get(y).copied().unwrap_or(&[]);
        for (x, pixel) in strip.chunks_exact_mut(4).enumerate() {
            let alpha = coverage.get(x).copied().unwrap_or(0);
            blend(background, foreground, alpha).write(pixel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(terminal: &Terminal, row: usize) -> String {
        terminal.visible_row(row).iter().collect()
    }

    #[test]
    fn printed_text_lands_on_the_first_row() {
        let mut terminal = Terminal::new(20, 4);
        terminal.write(b"hello");
        assert_eq!(row_text(&terminal, 0), "hello");
        assert_eq!(terminal.cursor(), (5, 0));
    }

    #[test]
    fn a_newline_starts_the_next_row_and_a_return_rewrites_this_one() {
        let mut terminal = Terminal::new(20, 4);
        terminal.write(b"one\ntwo");
        assert_eq!(row_text(&terminal, 0), "one");
        assert_eq!(row_text(&terminal, 1), "two");
        terminal.write(b"\rTWO");
        assert_eq!(row_text(&terminal, 1), "TWO");
    }

    #[test]
    fn a_backspace_moves_the_cursor_back_over_what_is_overwritten_next() {
        let mut terminal = Terminal::new(20, 4);
        terminal.write(b"helo\x08lo");
        assert_eq!(row_text(&terminal, 0), "hello");
        assert_eq!(terminal.cursor(), (5, 0));
    }

    #[test]
    fn the_sequence_a_shell_sends_to_erase_a_character_erases_one() {
        let mut terminal = Terminal::new(20, 4);
        terminal.write(b"hello\x08 \x08");
        // The cell the character was in is a blank the row still holds:
        // erasing a character is writing a space over it.
        assert_eq!(row_text(&terminal, 0), "hell ");
        assert_eq!(terminal.cursor(), (4, 0));
    }

    #[test]
    fn text_wider_than_the_grid_wraps_onto_the_next_row() {
        let mut terminal = Terminal::new(4, 4);
        terminal.write(b"abcdef");
        assert_eq!(row_text(&terminal, 0), "abcd");
        assert_eq!(row_text(&terminal, 1), "ef");
    }

    #[test]
    fn the_screen_scrolls_and_what_left_it_is_scrollback() {
        let mut terminal = Terminal::new(20, 3);
        terminal.write(b"one\ntwo\nthree\nfour");
        // The screen holds the last three lines...
        assert_eq!(row_text(&terminal, 0), "two");
        assert_eq!(row_text(&terminal, 1), "three");
        assert_eq!(row_text(&terminal, 2), "four");
        // ...and the first is behind it rather than gone.
        assert_eq!(terminal.scrollback(), 1);
    }

    #[test]
    fn the_scrollback_stops_growing_once_it_is_full() {
        let mut terminal = Terminal::new(20, 2);
        for index in 0..(SCROLLBACK_LINES + 32) {
            terminal.write(format!("line {index}\n").as_bytes());
        }
        assert_eq!(terminal.scrollback(), SCROLLBACK_LINES);
        // The newest lines are still the ones on screen.
        assert_eq!(
            row_text(&terminal, 0),
            format!("line {}", SCROLLBACK_LINES + 31)
        );
    }

    #[test]
    fn a_scroll_marks_every_row_for_redraw_and_a_keystroke_marks_one() {
        let mut terminal = Terminal::new(20, 3);
        let _ = terminal.take_dirty_rows();
        terminal.write(b"a");
        assert_eq!(terminal.take_dirty_rows(), vec![0]);
        terminal.write(b"\nb\nc\n");
        // The fourth line scrolled the screen, so everything moved.
        assert_eq!(terminal.take_dirty_rows(), vec![0, 1, 2]);
    }

    #[test]
    fn a_colour_escape_is_skipped_rather_than_printed() {
        let mut terminal = Terminal::new(20, 3);
        terminal.write(b"\x1b[1;32mgreen\x1b[0m done");
        assert_eq!(row_text(&terminal, 0), "green done");
    }

    #[test]
    fn a_window_title_sequence_is_skipped_whichever_way_it_ends() {
        let mut terminal = Terminal::new(20, 3);
        terminal.write(b"\x1b]0;a title\x07x");
        assert_eq!(row_text(&terminal, 0), "x");
        terminal.write(b"\r\x1b]0;another\x1b\\y");
        assert_eq!(row_text(&terminal, 0), "y");
    }

    #[test]
    fn a_character_split_across_two_reads_is_drawn_once_it_is_whole() {
        let mut terminal = Terminal::new(20, 3);
        let bytes = "é".as_bytes();
        terminal.write(&bytes[..1]);
        assert_eq!(row_text(&terminal, 0), "");
        terminal.write(&bytes[1..]);
        assert_eq!(row_text(&terminal, 0), "é");
    }

    #[test]
    fn a_byte_that_can_never_be_a_character_is_shown_as_the_replacement() {
        let mut terminal = Terminal::new(20, 3);
        terminal.write(&[b'a', 0xff, b'b']);
        assert_eq!(row_text(&terminal, 0), "a\u{fffd}b");
    }

    #[test]
    fn a_tab_advances_to_the_next_stop() {
        let mut terminal = Terminal::new(20, 3);
        terminal.write(b"ab\tc");
        assert_eq!(row_text(&terminal, 0), "ab      c");
    }
}
