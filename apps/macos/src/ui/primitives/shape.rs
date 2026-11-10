//! Corner rounding (`docs/DESIGN.md`, Corners): a view says what a shape is and the radius
//! comes from `radius::`, so no call site picks or computes a corner by hand.

use super::tokens::radius;
use gpui::{Bounds, Corners, Hsla, PaintQuad, Pixels, Styled, fill};

pub(crate) trait Shape: Styled + Sized {
    /// A container: groups, cards, banners, popovers, dialogs, the composer, bubbles and
    /// the sidebar.
    fn tile_corners(self) -> Self {
        self.rounded(radius::TILE)
    }

    /// An accent tile behind a hero glyph.
    fn hero_corners(self) -> Self {
        self.rounded(radius::HERO)
    }

    /// A control: buttons, fields, rows, tabs and rail items.
    fn control_corners(self) -> Self {
        self.rounded(radius::CONTROL)
    }

    /// A pill, dot, swatch or skeleton bar.
    fn pill_corners(self) -> Self {
        self.rounded(radius::FULL)
    }

    /// A usage matrix cell.
    fn cell_corners(self) -> Self {
        self.rounded(radius::CELL)
    }

    /// A fill or clipped picture `inset` inside a frame with `parent` corners.
    fn inner_corners(self, parent: Pixels, inset: Pixels) -> Self {
        self.rounded(radius::inner(parent, inset))
    }

    /// A ring drawn `gap` outside a shape with `child` corners.
    fn outer_corners(self, child: Pixels, gap: Pixels) -> Self {
        self.rounded(radius::outer(child, gap))
    }

    /// Only the top corners, where the shape meets a square edge below it.
    fn top_corners(self, radius: Pixels) -> Self {
        self.rounded_t(radius)
    }

    /// Only the top-left corner, where the shape meets square edges on its other sides.
    fn top_left_corner(self, radius: Pixels) -> Self {
        self.rounded_tl(radius)
    }
}

impl<E: Styled> Shape for E {}

/// A painted pill or dot: a quad rounded as far as its size allows. `paint_quad` does not
/// clamp radii itself.
pub(crate) fn pill_quad(bounds: Bounds<Pixels>, color: Hsla) -> PaintQuad {
    let corners = Corners::all(radius::FULL).clamp_radii_for_quad_size(bounds.size);
    fill(bounds, color).corner_radii(corners)
}
