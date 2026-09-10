//! What the desktop draws with, as a library both sides of the picture
//! link: the compositor plugin that paints the display and the host
//! check that reads the capture back.
//!
//! The glyph raster, the cell blend, the terminal grid, the damage
//! tracker and the key translation carry no plugin in them — no world,
//! no export, no kernel call — and that is what makes them linkable from
//! a host binary. The compositor's component exports stay in the plugin
//! crate, whose only artifact is the `cdylib` the kernel loads; a host
//! build never sees a component-model export name, and the check cannot
//! disagree with the desktop about a glyph, a colour or a cell because
//! it renders the expected pixels with these very modules.

pub mod damage;
pub mod desktop;
pub mod font;
pub mod keys;
pub mod paint;
pub mod term;
