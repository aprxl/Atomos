//! Tests raw `taffy` only, not Atomos code: it checks the assumption the
//! virtual-list design rests on, so it will fail if a taffy upgrade changes it.
//!
//! Spike for design risk R1: can detached "row island" subtrees live in the
//! same `TaffyTree` as the main root, with independent caches and dirty
//! marks? Counts measure calls to see which solves reuse cached results.

use std::cell::Cell;

use taffy::prelude::*;

#[test]
fn detached_islands_keep_independent_caches() {
    let calls = Cell::new(0u32);
    let mut tree: TaffyTree<u32> = TaffyTree::new();
    tree.disable_rounding();
    let fill = |tree: &mut TaffyTree<u32>, ctx: u32| {
        tree.new_leaf_with_context(Style::default(), ctx).unwrap()
    };
    let main_leaf = fill(&mut tree, 1);
    let main = tree
        .new_with_children(Style { size: Size { width: length(200.0), height: length(100.0) }, ..Default::default() }, &[main_leaf])
        .unwrap();
    // An island: a row subtree that is NOT a child of `main`.
    let row_leaf = fill(&mut tree, 2);
    let row = tree
        .new_with_children(Style { size: Size { width: length(200.0), height: auto() }, ..Default::default() }, &[row_leaf])
        .unwrap();

    let solve = |tree: &mut TaffyTree<u32>, root: NodeId| {
        tree.compute_layout_with_measure(root, Size::MAX_CONTENT, |input, _, ctx, style| {
            taffy::compute_leaf_layout(input, style, |_, _| 0.0, |_, _| {
                calls.set(calls.get() + 1);
                Size { width: 10.0 * ctx.map(|c| *c).unwrap_or(0) as f32, height: 16.0 }
            })
        })
        .unwrap();
    };
    solve(&mut tree, main);
    solve(&mut tree, row);
    let warm = calls.get();
    assert!(warm >= 2);

    // Re-solving either without changes hits the cache.
    solve(&mut tree, main);
    solve(&mut tree, row);
    assert_eq!(calls.get(), warm, "unchanged solves must be cached");

    // Dirty the island only: the main root must stay cached.
    tree.mark_dirty(row_leaf).unwrap();
    solve(&mut tree, main);
    assert_eq!(calls.get(), warm, "main root re-measured after an island change");
    solve(&mut tree, row);
    assert!(calls.get() > warm, "island did not re-measure after being dirtied");

    // And the reverse.
    let after = calls.get();
    tree.mark_dirty(main_leaf).unwrap();
    solve(&mut tree, row);
    assert_eq!(calls.get(), after, "island re-measured after a main-tree change");
}
