//! Painter-order-preserving batching of a layer's draw items.
//!
//! A layer replays its commands as *ops*: a solid-geometry range, a run of
//! images, or one glyphon text renderer. Consecutive commands of one kind
//! share an op, but a stream that interleaves kinds (a line background, then
//! its text, then the next line's background...) would make one op per
//! command, and every text op costs its own glyphon renderer, vertex buffer
//! and prepare.
//!
//! [`Batcher`] cuts that down without changing a pixel. An item may join the
//! nearest *earlier* group of its own kind when its bounds overlap nothing in
//! the groups it would hop over; commands that overlap are never reordered,
//! so painter order only changes between items that cannot see each other.
//! Bounds are conservative (a rectangle that contains everything the item
//! draws), and an item with no bounds-based answer simply starts a new group.
//!
//! Clip scopes are barriers the caller handles: it uses one [`Batcher`] per
//! stretch of commands between clip changes.

/// `[min_x, min_y, max_x, max_y]` in physical pixels.
pub(super) type Rect = [f32; 4];

/// What an op draws; only items of one kind share a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Solid,
    Image,
    Text,
}

/// Most steps (a group visited, or a rectangle compared) spent placing one
/// item. Past it the item starts a new group, which is always correct and
/// only costs a batch, so a pathological layer cannot turn placement
/// quadratic.
const MAX_CHECKS_PER_ITEM: usize = 2048;

#[derive(Debug)]
pub(super) struct Group {
    pub(super) kind: Kind,
    /// Item ids in original (painter) order.
    pub(super) items: Vec<usize>,
    rects: Vec<Rect>,
    union: Rect,
}

impl Group {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            items: Vec::new(),
            rects: Vec::new(),
            union: EMPTY,
        }
    }

    fn push(&mut self, item: usize, bounds: Option<Rect>) {
        self.items.push(item);
        if let Some(rect) = bounds {
            // A NaN rectangle makes the union NaN for good, so the group is
            // never skipped by its union (`overlaps` treats NaN as overlap).
            if rect.iter().any(|v| v.is_nan()) || self.union[0].is_nan() {
                self.union = [f32::NAN; 4];
            } else {
                self.union = [
                    self.union[0].min(rect[0]),
                    self.union[1].min(rect[1]),
                    self.union[2].max(rect[2]),
                    self.union[3].max(rect[3]),
                ];
            }
            self.rects.push(rect);
        }
    }
}

/// The union of nothing: overlaps no rectangle.
const EMPTY: Rect = [
    f32::INFINITY,
    f32::INFINITY,
    f32::NEG_INFINITY,
    f32::NEG_INFINITY,
];

/// Whether two rectangles share area. Touching edges do not overlap; a NaN
/// anywhere counts as overlapping, the conservative answer.
pub(super) fn overlaps(a: &Rect, b: &Rect) -> bool {
    !(a[2] <= b[0] || b[2] <= a[0] || a[3] <= b[1] || b[3] <= a[1])
}

pub(super) struct Batcher {
    groups: Vec<Group>,
    /// With merging off, an item only joins the last group (the order-
    /// preserving behaviour of one op per run of equal kind).
    merge: bool,
}

impl Batcher {
    pub(super) fn new(merge: bool) -> Self {
        Self {
            groups: Vec::new(),
            merge,
        }
    }

    /// Place `item` (an id the caller resolves after [`Batcher::finish`]).
    /// `bounds` is everything the item draws, or `None` when it draws
    /// nothing.
    pub(super) fn place(&mut self, kind: Kind, bounds: Option<Rect>, item: usize) {
        let target = self.find_group(kind, bounds.as_ref());
        match target {
            Some(index) => self.groups[index].push(item, bounds),
            None => {
                let mut group = Group::new(kind);
                group.push(item, bounds);
                self.groups.push(group);
            }
        }
    }

    /// The nearest group of `kind` that `bounds` can reach without crossing
    /// an overlapping item.
    fn find_group(&self, kind: Kind, bounds: Option<&Rect>) -> Option<usize> {
        let last = self.groups.len().checked_sub(1)?;
        if self.groups[last].kind == kind {
            return Some(last);
        }
        if !self.merge {
            return None;
        }
        let mut budget = MAX_CHECKS_PER_ITEM;
        for index in (0..=last).rev() {
            let group = &self.groups[index];
            if group.kind == kind {
                return Some(index);
            }
            if budget == 0 {
                return None;
            }
            budget -= 1;
            // Nothing drawn: free to hop over anything.
            let Some(bounds) = bounds else { continue };
            if !overlaps(bounds, &group.union) {
                continue;
            }
            for rect in &group.rects {
                if budget == 0 || overlaps(bounds, rect) {
                    return None;
                }
                budget -= 1;
            }
        }
        None
    }

    pub(super) fn finish(self) -> Vec<Group> {
        self.groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Option<Rect> {
        Some([x, y, x + w, y + h])
    }

    fn shape(groups: &[Group]) -> Vec<(Kind, Vec<usize>)> {
        groups
            .iter()
            .map(|group| (group.kind, group.items.clone()))
            .collect()
    }

    #[test]
    fn disjoint_lines_collapse_to_one_group_per_kind() {
        let mut batcher = Batcher::new(true);
        let mut id = 0;
        for line in 0..4 {
            let y = line as f32 * 10.0;
            batcher.place(Kind::Solid, rect(0.0, y, 100.0, 10.0), id);
            batcher.place(Kind::Text, rect(2.0, y + 1.0, 50.0, 8.0), id + 1);
            id += 2;
        }
        assert_eq!(
            shape(&batcher.finish()),
            vec![
                (Kind::Solid, vec![0, 2, 4, 6]),
                (Kind::Text, vec![1, 3, 5, 7])
            ]
        );
    }

    #[test]
    fn an_overlapping_item_is_never_hoisted() {
        // text, a rectangle over it, then more text: the last text overlaps
        // the rectangle it would hop over, so it must stay above it.
        let mut batcher = Batcher::new(true);
        batcher.place(Kind::Text, rect(0.0, 0.0, 50.0, 10.0), 0);
        batcher.place(Kind::Solid, rect(10.0, 0.0, 10.0, 10.0), 1);
        batcher.place(Kind::Text, rect(12.0, 0.0, 10.0, 10.0), 2);
        assert_eq!(
            shape(&batcher.finish()),
            vec![
                (Kind::Text, vec![0]),
                (Kind::Solid, vec![1]),
                (Kind::Text, vec![2])
            ]
        );
    }

    #[test]
    fn a_hop_must_clear_every_group_in_between() {
        let mut batcher = Batcher::new(true);
        batcher.place(Kind::Solid, rect(0.0, 0.0, 10.0, 10.0), 0);
        batcher.place(Kind::Text, rect(0.0, 20.0, 10.0, 10.0), 1);
        batcher.place(Kind::Image, rect(0.0, 40.0, 10.0, 10.0), 2);
        // Clears the text and image, so it reaches the earlier solid group.
        batcher.place(Kind::Solid, rect(20.0, 0.0, 10.0, 10.0), 3);
        // Overlaps the image, so it cannot reach the earlier solid group.
        batcher.place(Kind::Solid, rect(0.0, 40.0, 5.0, 5.0), 4);
        assert_eq!(
            shape(&batcher.finish()),
            vec![
                (Kind::Solid, vec![0, 3]),
                (Kind::Text, vec![1]),
                (Kind::Image, vec![2]),
                (Kind::Solid, vec![4])
            ]
        );
    }

    #[test]
    fn touching_edges_do_not_overlap_but_nan_does() {
        assert!(!overlaps(&[0.0, 0.0, 10.0, 10.0], &[10.0, 0.0, 20.0, 10.0]));
        assert!(overlaps(&[0.0, 0.0, 10.0, 10.0], &[9.9, 0.0, 20.0, 10.0]));
        assert!(overlaps(&[f32::NAN; 4], &[0.0, 0.0, 1.0, 1.0]));
    }

    #[test]
    fn merging_off_only_joins_runs() {
        let mut batcher = Batcher::new(false);
        batcher.place(Kind::Solid, rect(0.0, 0.0, 10.0, 10.0), 0);
        batcher.place(Kind::Text, rect(0.0, 20.0, 10.0, 10.0), 1);
        batcher.place(Kind::Solid, rect(0.0, 40.0, 10.0, 10.0), 2);
        batcher.place(Kind::Solid, rect(0.0, 60.0, 10.0, 10.0), 3);
        assert_eq!(
            shape(&batcher.finish()),
            vec![
                (Kind::Solid, vec![0]),
                (Kind::Text, vec![1]),
                (Kind::Solid, vec![2, 3])
            ]
        );
    }

    #[test]
    fn an_item_that_draws_nothing_hops_freely() {
        let mut batcher = Batcher::new(true);
        batcher.place(Kind::Text, rect(0.0, 0.0, 10.0, 10.0), 0);
        batcher.place(Kind::Solid, rect(0.0, 0.0, 10.0, 10.0), 1);
        batcher.place(Kind::Text, None, 2);
        assert_eq!(
            shape(&batcher.finish()),
            vec![(Kind::Text, vec![0, 2]), (Kind::Solid, vec![1])]
        );
    }

    #[test]
    fn a_nan_item_is_never_hopped_over() {
        let mut batcher = Batcher::new(true);
        batcher.place(Kind::Text, rect(0.0, 0.0, 10.0, 10.0), 0);
        batcher.place(Kind::Solid, Some([f32::NAN; 4]), 1);
        batcher.place(Kind::Text, rect(500.0, 500.0, 10.0, 10.0), 2);
        assert_eq!(batcher.finish().len(), 3);
    }

    #[test]
    fn group_walks_are_bounded_too() {
        // Overlapping alternating kinds (so each gets its own group), far from the probe: every group is visited, but
        // the budget stops the walk.
        let mut batcher = Batcher::new(true);
        batcher.place(Kind::Image, rect(-100.0, -100.0, 1.0, 1.0), 0);
        for i in 0..6000 {
            let kind = if i % 2 == 0 { Kind::Solid } else { Kind::Text };
            batcher.place(kind, rect(0.0, 0.0, 1.0, 1.0), 1 + i);
        }
        let before = batcher.groups.len();
        batcher.place(Kind::Image, rect(-200.0, -200.0, 1.0, 1.0), 6001);
        assert_eq!(batcher.groups.len(), before + 1);
    }

    #[test]
    fn placement_cost_is_bounded() {
        // 5000 solids in one group, then a text overlapping the last of
        // them: the scan gives up at the budget and starts a new group.
        let mut batcher = Batcher::new(true);
        batcher.place(Kind::Text, rect(-100.0, -100.0, 1.0, 1.0), 0);
        for i in 0..5000 {
            batcher.place(Kind::Solid, rect(i as f32 * 2.0, 0.0, 1.0, 1.0), 1 + i);
        }
        batcher.place(Kind::Text, rect(9998.0, 0.0, 1.0, 1.0), 5001);
        assert_eq!(batcher.finish().len(), 3);
    }
}
