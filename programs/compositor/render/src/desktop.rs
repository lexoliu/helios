//! The desktop: what is on the screen, and how a change to it becomes
//! the fewest pixels that can carry the change.
//!
//! Everything drawn here is drawn into the scanout surface's own frame
//! buffer — the pages the display engine reads — with ordinary stores.
//! Nothing is composed into a scratch buffer first, because there is
//! nothing a scratch buffer would buy: the engine reads what is written
//! only when `present` names a rectangle, so a half-drawn frame is never
//! shown.
//!
//! # What is drawn, bottom to top
//!
//! 1. The wallpaper, which is a function of the pixel's position and
//!    nothing else, so any rectangle of it can be redrawn without
//!    keeping a copy anywhere.
//! 2. The terminal window: a panel, a border, and the rows of the
//!    character grid that the rectangle touches.
//! 3. Every client window that the rectangle touches, blitted from the
//!    client's own pages.
//!
//! The pointer is not in that list. It is the display engine's own
//! plane, moved with `move-cursor`, so pointer motion costs no pixels
//! and never waits behind a frame.

use std::vec::Vec;

use helios_api::display::{
    CURSOR_BYTES, CURSOR_HEIGHT, CURSOR_WIDTH, DisplayError, Mode, Point, Rect, Surface,
};

use crate::damage::{Damage, Region};
use crate::font::{CELL_HEIGHT, CELL_WIDTH};
use crate::paint::Colour;
use crate::term::Terminal;

/// Bytes one pixel of every surface on this desktop occupies.
const BYTES_PER_PIXEL: usize = 4;

/// How far the terminal window sits from the edges of the screen.
const DESKTOP_MARGIN: u32 = 32;

/// How wide the border drawn around a window is.
const BORDER: u32 = 2;

/// How far a client window is offset from the last one.
const CASCADE_STEP: u32 = 24;

/// The colours of the desktop.
///
/// The terminal's two are public because the host-side check renders the
/// strip it expects with them: a check that carried its own copy of a
/// colour could pass on a desktop that had changed.
pub const TERMINAL_BACKGROUND: Colour = Colour::new(0x10, 0x12, 0x18);
pub const TERMINAL_FOREGROUND: Colour = Colour::new(0xe8, 0xe8, 0xe8);
const TERMINAL_CURSOR: Colour = Colour::new(0x5a, 0xc8, 0xff);
const BORDER_FOCUSED: Colour = Colour::new(0x5a, 0xc8, 0xff);
const BORDER_UNFOCUSED: Colour = Colour::new(0x40, 0x46, 0x52);

/// What the keyboard is talking to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// The pointer is over the wallpaper, so nothing has the keyboard.
    Desktop,
    /// The pointer is over the terminal, so the shell has the keyboard.
    Terminal,
    /// The pointer is over a client window, which has the keyboard.
    Client(u64),
}

/// One window a client asked the desktop for.
pub struct ClientWindow {
    id: u64,
    region: Region,
    /// Where the client's frame buffer sits in *this* program's linear
    /// memory. The kernel mapped the client's pinned run a second time,
    /// so these bytes and the client's are the same bytes.
    offset: usize,
    length: usize,
}

impl ClientWindow {
    /// The client's pixels.
    ///
    /// # Panics
    ///
    /// Panics when the kernel placed the run somewhere this program
    /// cannot address, which would mean the two disagree about the size
    /// of a pointer.
    fn pixels(&self) -> &[u8] {
        // SAFETY: the kernel mapped `length` bytes of the client's
        // pinned run at `offset` in this program's linear memory and
        // keeps them mapped until it calls `destroy` for this window.
        // Nothing else in this program can reach them: the offset is
        // above everything the allocator can ever hand out, because the
        // kernel caps this program's memory growth below it.
        unsafe { core::slice::from_raw_parts(self.offset as *const u8, self.length) }
    }
}

/// Everything on the screen.
pub struct Desktop {
    surface: Surface,
    mode: Mode,
    damage: Damage,
    terminal: Terminal,
    terminal_region: Region,
    clients: Vec<ClientWindow>,
    pointer: Point,
    focus: Focus,
}

impl Desktop {
    /// Build the desktop on `surface`, sized to `mode`.
    pub fn new(surface: Surface, mode: Mode) -> Self {
        let bounds = Region::new(0, 0, mode.width, mode.height);
        let terminal_region = terminal_region(mode);
        let inner = inset(terminal_region, BORDER);
        let columns = (inner.width as usize / CELL_WIDTH).max(1);
        let rows = (inner.height as usize / CELL_HEIGHT).max(1);
        Self {
            surface,
            mode,
            damage: Damage::new(bounds),
            terminal: Terminal::new(columns, rows),
            terminal_region,
            clients: Vec::new(),
            pointer: initial_pointer(mode),
            focus: initial_focus(mode),
        }
    }

    pub const fn terminal(&mut self) -> &mut Terminal {
        &mut self.terminal
    }

    pub const fn focus(&self) -> Focus {
        self.focus
    }

    pub const fn pointer(&self) -> Point {
        self.pointer
    }

    /// Where the terminal's top-left character cell sits.
    pub const fn terminal_origin(&self) -> (usize, usize) {
        (
            (self.terminal_region.x + BORDER) as usize,
            (self.terminal_region.y + BORDER) as usize,
        )
    }

    /// The whole screen has to be drawn again.
    pub fn damage_all(&mut self) {
        self.damage.add_all();
        self.terminal.dirty_all();
    }

    /// The rows of the terminal that changed have to be drawn again.
    pub fn damage_terminal_rows(&mut self) {
        let origin = self.terminal_origin();
        let width = self.terminal.columns() * CELL_WIDTH;
        for row in self.terminal.take_dirty_rows() {
            self.damage.add(Region::new(
                origin.0 as u32,
                (origin.1 + row * CELL_HEIGHT) as u32,
                width as u32,
                CELL_HEIGHT as u32,
            ));
        }
    }

    /// A rectangle of one client's window changed.
    pub fn damage_client(&mut self, id: u64, region: Region) {
        let Some(client) = self.clients.iter().find(|client| client.id == id) else {
            return;
        };
        let placed = region.translated(client.region.x, client.region.y);
        self.damage.add(placed.intersection(client.region));
    }

    /// Take a client's window onto the desktop.
    pub fn add_client(&mut self, id: u64, width: u32, height: u32, offset: u64, length: u64) {
        let region = self.place_client(width, height);
        let offset = usize::try_from(offset)
            .expect("the kernel maps a window inside this program's address space");
        let length = usize::try_from(length)
            .expect("the kernel maps a window inside this program's address space");
        self.clients.push(ClientWindow {
            id,
            region,
            offset,
            length,
        });
        self.damage.add(bordered(region));
    }

    /// Take a client's window off the desktop.
    pub fn remove_client(&mut self, id: u64) {
        let Some(index) = self.clients.iter().position(|client| client.id == id) else {
            return;
        };
        let client = self.clients.remove(index);
        self.damage.add(bordered(client.region));
        if self.focus == Focus::Client(id) {
            self.focus = self.focus_at(self.pointer);
        }
    }

    /// Whether the desktop holds a window for `id`.
    pub fn holds_client(&self, id: u64) -> bool {
        self.clients.iter().any(|client| client.id == id)
    }

    /// Move the pointer, and let focus follow it.
    ///
    /// Returns the focus after the move, so the caller can say when it
    /// changed without asking twice.
    pub fn move_pointer(&mut self, position: Point) -> Focus {
        self.pointer = Point {
            x: position.x.min(self.mode.width.saturating_sub(1)),
            y: position.y.min(self.mode.height.saturating_sub(1)),
        };
        let focus = self.focus_at(self.pointer);
        if focus != self.focus {
            // The border of whatever gained and whatever lost the
            // keyboard is the only thing that changed on screen.
            self.damage_focus_border(self.focus);
            self.damage_focus_border(focus);
            self.focus = focus;
        }
        focus
    }

    /// Where the pointer is, as a pixel inside the focused client's own
    /// window.
    pub fn pointer_in_client(&self, id: u64) -> Option<(u32, u32)> {
        let client = self.clients.iter().find(|client| client.id == id)?;
        Some((
            self.pointer.x.saturating_sub(client.region.x),
            self.pointer.y.saturating_sub(client.region.y),
        ))
    }

    /// Publish everything that changed, one rectangle at a time.
    ///
    /// Nothing is presented when nothing changed, which is what makes a
    /// pointer move cost no frame at all.
    pub async fn present(&mut self) -> Result<usize, DisplayError> {
        if self.damage.is_empty() {
            return Ok(0);
        }
        let regions = self.damage.take();
        for region in &regions {
            self.compose(*region);
        }
        for region in &regions {
            self.surface
                .present(Rect {
                    x: region.x,
                    y: region.y,
                    width: region.width,
                    height: region.height,
                })
                .await?;
        }
        Ok(regions.len())
    }

    /// Put the pointer image on the display engine's own plane.
    pub async fn set_cursor(&self) -> Result<(), DisplayError> {
        self.surface
            .set_cursor(cursor_image(), Point { x: 1, y: 1 })
            .await
    }

    /// Move the pointer on the display engine's plane.
    ///
    /// This costs no pixels and no frame-buffer traffic, which is the
    /// whole reason the plane exists.
    pub async fn present_cursor(&self) -> Result<(), DisplayError> {
        self.surface.move_cursor(self.pointer).await
    }

    /// What has the keyboard when the pointer is at `position`.
    fn focus_at(&self, position: Point) -> Focus {
        resolve_focus(
            self.clients.iter().map(|client| (client.id, client.region)),
            self.terminal_region,
            position,
        )
    }

    /// The border of whatever `focus` names has to be drawn again.
    fn damage_focus_border(&mut self, focus: Focus) {
        let region = match focus {
            Focus::Desktop => return,
            Focus::Terminal => self.terminal_region,
            Focus::Client(id) => match self.clients.iter().find(|client| client.id == id) {
                Some(client) => client.region,
                None => return,
            },
        };
        self.damage.add(bordered(region));
    }

    /// Where the next client window goes.
    fn place_client(&self, width: u32, height: u32) -> Region {
        let step = (self.clients.len() as u32) * CASCADE_STEP;
        let width = width.min(self.mode.width);
        let height = height.min(self.mode.height);
        let x = (self.mode.width / 2 + step).min(self.mode.width.saturating_sub(width));
        let y = (DESKTOP_MARGIN + step).min(self.mode.height.saturating_sub(height));
        Region::new(x, y, width, height)
    }

    /// Draw one rectangle of the desktop, bottom to top.
    fn compose(&mut self, region: Region) {
        let stride = self.surface.stride();
        let mode = self.mode;
        let terminal_region = self.terminal_region;
        let focus = self.focus;
        let origin = self.terminal_origin();

        {
            let pixels = self.surface.pixels();
            fill_wallpaper(pixels, stride, region, mode);
        }

        // The terminal panel and its border.
        if terminal_region.intersection(bordered(region)).is_empty() {
            // Nothing of the terminal is in this rectangle.
        } else {
            let inner = inset(terminal_region, BORDER);
            {
                let pixels = self.surface.pixels();
                fill(
                    pixels,
                    stride,
                    inner.intersection(region),
                    TERMINAL_BACKGROUND,
                );
                draw_border(
                    pixels,
                    stride,
                    terminal_region,
                    region,
                    if focus == Focus::Terminal {
                        BORDER_FOCUSED
                    } else {
                        BORDER_UNFOCUSED
                    },
                );
            }
            let first_row = row_of(region.y, origin.1);
            let last_row = row_of(region.bottom().saturating_sub(1), origin.1);
            let rows = self.terminal.rows();
            for row in first_row..=last_row.min(rows.saturating_sub(1)) {
                let pixels = self.surface.pixels();
                self.terminal.render_row(
                    row,
                    pixels,
                    stride,
                    origin,
                    TERMINAL_FOREGROUND,
                    TERMINAL_BACKGROUND,
                );
            }
            let pixels = self.surface.pixels();
            self.terminal
                .render_cursor(pixels, stride, origin, TERMINAL_CURSOR);
        }

        // The client windows, oldest first, so the newest is in front.
        for index in 0..self.clients.len() {
            let (client_region, offset, length, id) = {
                let client = &self.clients[index];
                (client.region, client.offset, client.length, client.id)
            };
            if client_region.intersection(bordered(region)).is_empty() {
                continue;
            }
            let source = ClientWindow {
                id,
                region: client_region,
                offset,
                length,
            };
            let visible = client_region.intersection(region);
            let pixels = self.surface.pixels();
            blit(pixels, stride, &source, visible);
            draw_border(
                pixels,
                stride,
                client_region,
                region,
                if focus == Focus::Client(id) {
                    BORDER_FOCUSED
                } else {
                    BORDER_UNFOCUSED
                },
            );
        }
    }
}

/// Where the pointer is before the first report moves it: the mode's
/// centre, which is where the display's cursor plane starts it.
const fn initial_pointer(mode: Mode) -> Point {
    Point {
        x: mode.width / 2,
        y: mode.height / 2,
    }
}

/// What has the keyboard before the first pointer report: whatever the
/// pointer's starting position lies over, resolved the same way
/// `move_pointer` resolves a reported one. A terminal under the cursor
/// is typed into from the first frame rather than the first mouse move.
fn initial_focus(mode: Mode) -> Focus {
    resolve_focus(
        core::iter::empty(),
        terminal_region(mode),
        initial_pointer(mode),
    )
}

/// What has the keyboard when the pointer is at `position`.
///
/// Focus follows the pointer and nothing else moves it: there is no
/// click to raise and no shortcut to cycle, so what lies under the
/// pointer is what is typed into. `windows` comes in the order the
/// windows were added, and the last one is the one in front.
fn resolve_focus<I>(windows: I, terminal: Region, position: Point) -> Focus
where
    I: DoubleEndedIterator<Item = (u64, Region)>,
{
    for (id, region) in windows.rev() {
        if contains(region, position) {
            return Focus::Client(id);
        }
    }
    if contains(terminal, position) {
        return Focus::Terminal;
    }
    Focus::Desktop
}

/// Which terminal row the scanout row `y` falls in.
const fn row_of(y: u32, top: usize) -> usize {
    let y = y as usize;
    if y <= top {
        return 0;
    }
    (y - top) / CELL_HEIGHT
}

/// Where the terminal window sits on a screen of `mode`.
const fn terminal_region(mode: Mode) -> Region {
    let width = mode.width.saturating_sub(DESKTOP_MARGIN * 2);
    let height = mode.height.saturating_sub(DESKTOP_MARGIN * 2);
    Region::new(DESKTOP_MARGIN, DESKTOP_MARGIN, width, height)
}

/// The rectangle `region` grown by the border drawn around it.
const fn bordered(region: Region) -> Region {
    Region::new(
        region.x.saturating_sub(BORDER),
        region.y.saturating_sub(BORDER),
        region.width + BORDER * 2,
        region.height + BORDER * 2,
    )
}

/// The rectangle inside `region`'s own border.
const fn inset(region: Region, by: u32) -> Region {
    Region::new(
        region.x + by,
        region.y + by,
        region.width.saturating_sub(by * 2),
        region.height.saturating_sub(by * 2),
    )
}

const fn contains(region: Region, point: Point) -> bool {
    point.x >= region.x
        && point.x < region.right()
        && point.y >= region.y
        && point.y < region.bottom()
}

/// The wallpaper's colour at one pixel.
///
/// A function of the position and nothing else, computed in integers, so
/// any rectangle of it can be redrawn without a copy of it anywhere and
/// a host reading a capture back can work out what a pixel should be.
/// It darkens downwards and warms to the right: dusk over a horizon.
pub const fn wallpaper_colour(x: u32, y: u32, mode: Mode) -> Colour {
    let width = if mode.width > 1 { mode.width - 1 } else { 1 };
    let height = if mode.height > 1 { mode.height - 1 } else { 1 };
    let across = (x * 48 / width) as u8;
    let down = (y * 64 / height) as u8;
    Colour::new(0x18 + across, 0x1c + down / 2, 0x50 + down)
}

fn fill_wallpaper(pixels: &mut [u8], stride: usize, region: Region, mode: Mode) {
    for y in region.y..region.bottom() {
        let row = (y as usize) * stride;
        for x in region.x..region.right() {
            let offset = row + (x as usize) * BYTES_PER_PIXEL;
            let Some(pixel) = pixels.get_mut(offset..offset + BYTES_PER_PIXEL) else {
                return;
            };
            wallpaper_colour(x, y, mode).write(pixel);
        }
    }
}

fn fill(pixels: &mut [u8], stride: usize, region: Region, colour: Colour) {
    for y in region.y..region.bottom() {
        let start = (y as usize) * stride + (region.x as usize) * BYTES_PER_PIXEL;
        let width = (region.width as usize) * BYTES_PER_PIXEL;
        let Some(strip) = pixels.get_mut(start..start + width) else {
            return;
        };
        for pixel in strip.chunks_exact_mut(BYTES_PER_PIXEL) {
            colour.write(pixel);
        }
    }
}

/// Draw the border of `window`, clipped to `region`.
fn draw_border(pixels: &mut [u8], stride: usize, window: Region, region: Region, colour: Colour) {
    let outer = bordered(window);
    let edges = [
        Region::new(outer.x, outer.y, outer.width, BORDER),
        Region::new(outer.x, window.bottom(), outer.width, BORDER),
        Region::new(outer.x, window.y, BORDER, window.height),
        Region::new(window.right(), window.y, BORDER, window.height),
    ];
    for edge in edges {
        fill(pixels, stride, edge.intersection(region), colour);
    }
}

/// Copy the part of one client's window that lies in `visible`.
fn blit(pixels: &mut [u8], stride: usize, client: &ClientWindow, visible: Region) {
    if visible.is_empty() {
        return;
    }
    let source = client.pixels();
    let source_stride = (client.region.width as usize) * BYTES_PER_PIXEL;
    for y in visible.y..visible.bottom() {
        let source_row = ((y - client.region.y) as usize) * source_stride
            + ((visible.x - client.region.x) as usize) * BYTES_PER_PIXEL;
        let target_row = (y as usize) * stride + (visible.x as usize) * BYTES_PER_PIXEL;
        let width = (visible.width as usize) * BYTES_PER_PIXEL;
        let (Some(from), Some(into)) = (
            source.get(source_row..source_row + width),
            pixels.get_mut(target_row..target_row + width),
        ) else {
            return;
        };
        into.copy_from_slice(from);
    }
}

/// The pointer image: an arrow, opaque white with a black edge, so it
/// reads against both ends of the wallpaper.
fn cursor_image() -> Vec<u8> {
    let mut image = vec![0_u8; CURSOR_BYTES];
    // A classic pointer: a triangle whose left edge is vertical and
    // whose hypotenuse runs down and to the right, drawn from its own
    // tip so that the hotspot is the pixel at (1, 1).
    let extent = 22_u32;
    for y in 0..CURSOR_HEIGHT.min(extent) {
        for x in 0..CURSOR_WIDTH.min(extent) {
            if x > y || y >= extent {
                continue;
            }
            let edge = x == 0 || x == y || y == extent - 1;
            let level = if edge { 0x00 } else { 0xff };
            let offset = ((y * CURSOR_WIDTH + x) as usize) * BYTES_PER_PIXEL;
            image[offset] = level;
            image[offset + 1] = level;
            image[offset + 2] = level;
            image[offset + 3] = 0xff;
        }
    }
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODE: Mode = Mode {
        width: 640,
        height: 480,
    };

    #[test]
    fn the_terminal_sits_inside_the_screens_margins() {
        let region = terminal_region(MODE);
        assert_eq!(region.x, DESKTOP_MARGIN);
        assert_eq!(region.y, DESKTOP_MARGIN);
        assert_eq!(region.right(), MODE.width - DESKTOP_MARGIN);
        assert_eq!(region.bottom(), MODE.height - DESKTOP_MARGIN);
    }

    #[test]
    fn a_border_grows_a_window_by_the_border_on_every_side() {
        let region = Region::new(100, 100, 50, 50);
        let outer = bordered(region);
        assert_eq!(outer.x, 100 - BORDER);
        assert_eq!(outer.width, 50 + BORDER * 2);
        assert_eq!(inset(outer, BORDER), region);
    }

    #[test]
    fn the_wallpaper_is_the_same_colour_for_the_same_pixel_every_time() {
        let first = wallpaper_colour(123, 45, MODE);
        let second = wallpaper_colour(123, 45, MODE);
        assert_eq!(first, second);
        // And it does change across the screen, so a capture of it is
        // evidence of where a pixel is rather than of one flat fill.
        assert_ne!(
            wallpaper_colour(0, 0, MODE),
            wallpaper_colour(639, 479, MODE)
        );
    }

    #[test]
    fn a_scanout_row_maps_to_the_terminal_row_it_falls_in() {
        assert_eq!(row_of(34, 34), 0);
        assert_eq!(row_of(34 + CELL_HEIGHT as u32, 34), 1);
        assert_eq!(row_of(34 + CELL_HEIGHT as u32 * 3 + 1, 34), 3);
        // Above the terminal is the first row rather than an underflow.
        assert_eq!(row_of(0, 34), 0);
    }

    /// Focus follows the pointer from before its first report: the
    /// keyboard starts on whatever the cursor plane starts over, so a
    /// terminal there takes keys without the mouse ever moving.
    #[test]
    fn the_keyboard_starts_where_the_pointer_starts() {
        // The terminal fills this mode to within its margins, so the
        // centred starting pointer is over it.
        assert_eq!(initial_focus(MODE), Focus::Terminal);
        // A mode too small for the margins leaves nothing under the
        // pointer, and the wallpaper holds no keyboard.
        assert_eq!(
            initial_focus(Mode {
                width: 48,
                height: 48
            }),
            Focus::Desktop
        );
    }

    #[test]
    fn focus_follows_the_pointer_onto_the_terminal_and_off_it_again() {
        let terminal = Region::new(100, 100, 200, 200);
        let none: [(u64, Region); 0] = [];
        assert_eq!(
            resolve_focus(none.into_iter(), terminal, Point { x: 150, y: 150 }),
            Focus::Terminal
        );
        assert_eq!(
            resolve_focus(none.into_iter(), terminal, Point { x: 10, y: 10 }),
            Focus::Desktop
        );
    }

    #[test]
    fn a_client_window_under_the_pointer_takes_the_keyboard_from_the_terminal() {
        let terminal = Region::new(100, 100, 200, 200);
        let windows = [(7_u64, Region::new(150, 150, 50, 50))];
        assert_eq!(
            resolve_focus(windows.into_iter(), terminal, Point { x: 160, y: 160 }),
            Focus::Client(7)
        );
        // A pixel of the terminal the client does not cover is still
        // the terminal's.
        assert_eq!(
            resolve_focus(windows.into_iter(), terminal, Point { x: 120, y: 120 }),
            Focus::Terminal
        );
    }

    #[test]
    fn where_two_client_windows_overlap_the_later_one_has_the_keyboard() {
        let terminal = Region::new(0, 0, 10, 10);
        let windows = [
            (1_u64, Region::new(100, 100, 100, 100)),
            (2_u64, Region::new(150, 150, 100, 100)),
        ];
        assert_eq!(
            resolve_focus(windows.into_iter(), terminal, Point { x: 160, y: 160 }),
            Focus::Client(2)
        );
        assert_eq!(
            resolve_focus(windows.into_iter(), terminal, Point { x: 110, y: 110 }),
            Focus::Client(1)
        );
    }

    #[test]
    fn a_point_is_inside_a_window_up_to_but_not_past_its_edge() {
        let region = Region::new(10, 10, 5, 5);
        assert!(contains(region, Point { x: 10, y: 10 }));
        assert!(contains(region, Point { x: 14, y: 14 }));
        assert!(!contains(region, Point { x: 15, y: 14 }));
        assert!(!contains(region, Point { x: 9, y: 10 }));
    }

    #[test]
    fn the_pointer_image_is_the_size_the_display_engine_takes() {
        assert_eq!(cursor_image().len(), CURSOR_BYTES);
    }
}
