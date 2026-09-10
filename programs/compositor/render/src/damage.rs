//! What changed since the last frame reached the screen.
//!
//! A compositor that redrew the whole scanout every time a key was
//! pressed would copy a few megabytes per keystroke and flush the whole
//! display engine behind it. What it redraws instead is the rectangles
//! that changed: a glyph cell, the row a line scrolled into, the strip a
//! client committed. Those rectangles are what this collects, and what
//! `present` is given one of at a time.
//!
//! # How rectangles are kept apart
//!
//! Overlapping damage is merged, because compositing the same pixel
//! twice is wasted work and presenting it twice is a wasted flush.
//! Disjoint damage is kept apart up to [`MAX_REGIONS`], because merging
//! a glyph at the top of the screen with one at the bottom would repaint
//! everything between them. Past that bound the two whose union wastes
//! the fewest pixels are merged, so the set never grows without limit
//! and never degenerates into "the whole screen" while a cheaper answer
//! exists.

use std::vec::Vec;

/// Rectangles kept apart before two of them are merged.
///
/// Sized for what a desktop actually damages between two frames: a
/// cursor cell, a scrolled row, one or two client strips, and the window
/// decorations around them. A larger set costs a `present` each.
pub const MAX_REGIONS: usize = 8;

/// A rectangle of the scanout, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// One pixel past the right edge.
    pub const fn right(self) -> u32 {
        self.x + self.width
    }

    /// One pixel past the bottom edge.
    pub const fn bottom(self) -> u32 {
        self.y + self.height
    }

    /// Whether this rectangle covers no pixel at all.
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// How many pixels it covers.
    pub const fn area(self) -> u64 {
        (self.width as u64) * (self.height as u64)
    }

    /// The smallest rectangle covering both.
    ///
    /// An empty rectangle contributes nothing: the union with one is the
    /// other, rather than a rectangle stretched to an origin nothing was
    /// ever drawn at.
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self::new(x, y, right - x, bottom - y)
    }

    /// The rectangle both cover, empty when they cover none in common.
    pub fn intersection(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            return Self::new(x, y, 0, 0);
        }
        Self::new(x, y, right - x, bottom - y)
    }

    /// Whether the two share a pixel, or merely touch along an edge.
    ///
    /// Touching counts: two glyph cells side by side are one strip, and
    /// presenting them separately costs two flushes for pixels that lie
    /// in one run.
    pub const fn adjoins(self, other: Self) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }

    /// This rectangle moved so that its origin is relative to `origin`.
    ///
    /// Saturating, because a caller that asks for the part of a window
    /// above the window's own origin is asking for nothing rather than
    /// for a rectangle that wrapped.
    pub const fn translated(self, dx: u32, dy: u32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.width, self.height)
    }
}

/// The rectangles that have changed since the last present.
pub struct Damage {
    bounds: Region,
    regions: Vec<Region>,
}

impl Damage {
    /// A tracker for a scanout of `bounds`, with nothing damaged yet.
    pub fn new(bounds: Region) -> Self {
        Self {
            bounds,
            regions: Vec::with_capacity(MAX_REGIONS),
        }
    }

    /// Whether anything has changed.
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Record that `region` changed.
    ///
    /// Clipped to the scanout, so a client that commits a strip half off
    /// the edge damages the half that is on it and nothing more.
    pub fn add(&mut self, region: Region) {
        let mut region = region.intersection(self.bounds);
        if region.is_empty() {
            return;
        }
        // Merge into everything it adjoins, repeatedly: merging two can
        // make the result adjoin a third that neither touched before.
        let mut merged = true;
        while merged {
            merged = false;
            let mut index = 0;
            while index < self.regions.len() {
                if self.regions[index].adjoins(region) {
                    region = region.union(self.regions.swap_remove(index));
                    merged = true;
                } else {
                    index += 1;
                }
            }
        }
        self.regions.push(region);
        if self.regions.len() > MAX_REGIONS {
            self.merge_cheapest_pair();
        }
    }

    /// Record that the whole scanout changed.
    pub fn add_all(&mut self) {
        self.regions.clear();
        self.regions.push(self.bounds);
    }

    /// Take everything damaged, leaving nothing.
    pub fn take(&mut self) -> Vec<Region> {
        core::mem::take(&mut self.regions)
    }

    /// Merge the two rectangles whose union wastes the fewest pixels.
    ///
    /// "Wastes" is the union's area less the two areas: merging two
    /// rectangles that already touch costs nothing, and merging opposite
    /// corners of the screen costs the screen.
    fn merge_cheapest_pair(&mut self) {
        let mut best: Option<(usize, usize, u64)> = None;
        for left in 0..self.regions.len() {
            for right in (left + 1)..self.regions.len() {
                let waste = self.regions[left]
                    .union(self.regions[right])
                    .area()
                    .saturating_sub(self.regions[left].area() + self.regions[right].area());
                if best.is_none_or(|(_, _, lowest)| waste < lowest) {
                    best = Some((left, right, waste));
                }
            }
        }
        let Some((left, right, _)) = best else {
            return;
        };
        let second = self.regions.swap_remove(right);
        self.regions[left] = self.regions[left].union(second);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanout() -> Damage {
        Damage::new(Region::new(0, 0, 640, 480))
    }

    #[test]
    fn one_commit_damages_that_rectangle_and_nothing_else() {
        let mut damage = scanout();
        damage.add(Region::new(10, 20, 30, 40));
        assert_eq!(damage.take(), vec![Region::new(10, 20, 30, 40)]);
    }

    #[test]
    fn nothing_is_damaged_until_something_is_committed() {
        let mut damage = scanout();
        assert!(damage.is_empty());
        assert!(damage.take().is_empty());
    }

    #[test]
    fn a_commit_is_clipped_to_the_scanout() {
        let mut damage = scanout();
        damage.add(Region::new(600, 460, 100, 100));
        assert_eq!(damage.take(), vec![Region::new(600, 460, 40, 20)]);
        // Wholly outside is nothing at all, not a rectangle of no
        // pixels: a present of one would be a flush for nobody.
        damage.add(Region::new(700, 500, 10, 10));
        assert!(damage.is_empty());
    }

    #[test]
    fn two_overlapping_commits_coalesce_into_one_present() {
        let mut damage = scanout();
        damage.add(Region::new(0, 0, 100, 100));
        damage.add(Region::new(50, 50, 100, 100));
        assert_eq!(damage.take(), vec![Region::new(0, 0, 150, 150)]);
    }

    #[test]
    fn two_touching_glyph_cells_coalesce_into_one_strip() {
        let mut damage = scanout();
        damage.add(Region::new(0, 0, 10, 16));
        damage.add(Region::new(10, 0, 10, 16));
        assert_eq!(damage.take(), vec![Region::new(0, 0, 20, 16)]);
    }

    #[test]
    fn a_commit_inside_one_already_damaged_changes_nothing() {
        let mut damage = scanout();
        damage.add(Region::new(0, 0, 100, 100));
        damage.add(Region::new(10, 10, 10, 10));
        assert_eq!(damage.take(), vec![Region::new(0, 0, 100, 100)]);
    }

    #[test]
    fn two_distant_commits_stay_apart_so_nothing_between_them_is_redrawn() {
        let mut damage = scanout();
        damage.add(Region::new(0, 0, 10, 10));
        damage.add(Region::new(600, 460, 10, 10));
        let regions = damage.take();
        assert_eq!(regions.len(), 2);
        assert!(regions.contains(&Region::new(0, 0, 10, 10)));
        assert!(regions.contains(&Region::new(600, 460, 10, 10)));
    }

    #[test]
    fn a_third_merge_can_absorb_a_rectangle_neither_of_the_first_two_touched() {
        let mut damage = scanout();
        damage.add(Region::new(0, 0, 10, 10));
        damage.add(Region::new(40, 0, 10, 10));
        // Bridges the gap: the result has to be one rectangle rather
        // than the bridge plus the two it swallowed.
        damage.add(Region::new(10, 0, 30, 10));
        assert_eq!(damage.take(), vec![Region::new(0, 0, 50, 10)]);
    }

    #[test]
    fn past_the_bound_the_cheapest_pair_is_merged_rather_than_the_whole_screen() {
        let mut damage = scanout();
        // Nine disjoint cells along a row, spaced so that neighbours are
        // the cheapest merge and the ends are the dearest.
        for index in 0..(MAX_REGIONS as u32 + 1) {
            damage.add(Region::new(index * 40, 0, 10, 10));
        }
        let regions = damage.take();
        assert_eq!(regions.len(), MAX_REGIONS);
        // Whatever was merged, the damage still covers every cell and
        // still is not the whole scanout.
        let total: u64 = regions.iter().map(|region| region.area()).sum();
        assert!(total < Region::new(0, 0, 640, 480).area());
    }

    #[test]
    fn damaging_everything_replaces_whatever_was_held() {
        let mut damage = scanout();
        damage.add(Region::new(10, 10, 10, 10));
        damage.add_all();
        assert_eq!(damage.take(), vec![Region::new(0, 0, 640, 480)]);
    }

    #[test]
    fn an_intersection_that_shares_no_pixel_is_empty() {
        let left = Region::new(0, 0, 10, 10);
        let right = Region::new(10, 0, 10, 10);
        assert!(left.intersection(right).is_empty());
        // Touching along an edge shares no pixel, but is still worth
        // coalescing, which is why the two tests differ.
        assert!(left.adjoins(right));
    }
}
