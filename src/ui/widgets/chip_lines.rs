//! Chips wrapped over a fixed number of lines. What does not fit stays hidden,
//! and the page offers it through View All, so the shelves below stay in reach.
//! AdwWrapBox wraps the same way, but has no way to stop.

use std::cell::Cell;

use gtk::{glib, prelude::*, subclass::prelude::*};

const SPACING: i32 = 8;
/// A window this narrow is a phone, where three lines still hold few chips.
const NARROW: i32 = 500;

fn max_lines(width: i32) -> usize {
    if width < NARROW { 3 } else { 2 }
}

/// Where each widget of a row goes: its line and x, or None when it stays hidden.
/// `widths` is in order, a true `divider` is a separator, which only stands between two chips on one line.
pub fn place(widths: &[(i32, bool)], width: i32, lines: usize) -> Vec<Option<(usize, i32)>> {
    let mut placed = vec![None; widths.len()];
    let (mut line, mut x) = (0, 0);
    for (index, &(wide, divider)) in widths.iter().enumerate() {
        if divider {
            let next = widths.get(index + 1).map_or(0, |w| w.0);
            if x > 0 && x + wide + SPACING + next <= width {
                placed[index] = Some((line, x));
                x += wide + SPACING;
            }
            continue;
        }
        if x > 0 && x + wide > width {
            (line, x) = (line + 1, 0);
        }
        if line >= lines {
            break;
        }
        placed[index] = Some((line, x));
        x += wide + SPACING;
    }
    placed
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ChipLines {
        pub line_height: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ChipLines {
        const NAME: &'static str = "MixtapesChipLines";
        type Type = super::ChipLines;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ChipLines {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for ChipLines {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let widths = self.obj().widths();
            if orientation == gtk::Orientation::Horizontal {
                let natural: i32 = widths.iter().map(|w| w.0 + SPACING).sum();
                return (widths.iter().map(|w| w.0).max().unwrap_or(0), natural.max(0), -1, -1);
            }
            let width = if for_size < 0 { i32::MAX / 2 } else { for_size };
            let lines = place(&widths, width, max_lines(width)).iter().flatten().map(|p| p.0 + 1).max().unwrap_or(0) as i32;
            let height = lines * self.obj().line_height() + (lines - 1).max(0) * SPACING;
            (height, height, -1, -1)
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            let widget = self.obj();
            let widths = widget.widths();
            let line_height = widget.line_height();
            let placed = place(&widths, width, max_lines(width));
            let mut child = widget.first_child();
            for (spot, (wide, _)) in placed.iter().zip(&widths) {
                let Some(current) = child else { break };
                current.set_child_visible(spot.is_some());
                if let Some((line, x)) = spot {
                    let y = *line as i32 * (line_height + SPACING);
                    current.size_allocate(&gtk::Allocation::new(*x, y, *wide, line_height), -1);
                }
                child = current.next_sibling();
            }
        }
    }
}

glib::wrapper! {
    pub struct ChipLines(ObjectSubclass<imp::ChipLines>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ChipLines {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn append(&self, child: &impl IsA<gtk::Widget>) {
        child.set_parent(self);
    }

    fn children(&self) -> impl Iterator<Item = gtk::Widget> {
        std::iter::successors(self.first_child(), |c| c.next_sibling())
    }

    fn widths(&self) -> Vec<(i32, bool)> {
        self.children().map(|c| (c.measure(gtk::Orientation::Horizontal, -1).1, c.is::<gtk::Separator>())).collect()
    }

    fn line_height(&self) -> i32 {
        self.children().map(|c| c.measure(gtk::Orientation::Vertical, -1).1).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chips(count: usize) -> Vec<(i32, bool)> {
        vec![(100, false); count]
    }

    #[test]
    fn chips_stop_at_the_last_line() {
        let spots = place(&chips(10), 330, 2);
        assert_eq!(spots.iter().flatten().count(), 6, "three a line, two lines");
        assert_eq!(spots[3], Some((1, 0)));
        assert_eq!(spots[6], None);
    }

    #[test]
    fn a_separator_never_starts_or_ends_a_line() {
        let row = [(100, false), (100, false), (1, true), (100, false)];
        let tight = place(&row, 210, 2);
        assert_eq!(tight[2], None, "the next chip goes to a new line, so the line goes");
        assert_eq!(tight[3], Some((1, 0)));
        assert!(place(&row, 400, 2)[2].is_some());
    }
}
