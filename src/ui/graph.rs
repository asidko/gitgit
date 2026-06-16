//! Commit-graph gutter: renders one [`GraphRow`] from the lane engine into styled
//! spans of connected box-drawing glyphs.
//!
//! Pure: imports only the engine's row type, the theme, and ratatui. It never
//! computes topology - [`crate::graph_engine`] already did that; here we only turn
//! lanes/edges into characters. The gutter is a fixed grid: each lane owns an even
//! column, with an odd routing column between adjacent lanes so a one-lane shift
//! draws as horizontal + corner that meets the neighbour's vertical.

use ratatui::style::Style;
use ratatui::text::Span;

use crate::graph_engine::{Edge, GraphRow, LaneIndex};
use crate::theme::{Glyph, LaneColor, Theme};

/// Gutter width (columns) for a layout peaking at `max_lanes`: one left pad, then
/// each lane's column plus the routing column to its right. The log panel uses the
/// same formula so the graph and the text columns share one origin.
pub fn gutter_width(max_lanes: usize) -> usize {
    max_lanes * 2 + 1
}

/// Column of lane `lane` within the gutter (1 left pad, lanes on even offsets).
fn col(lane: LaneIndex) -> usize {
    1 + lane * 2
}

/// One painted gutter cell: a glyph plus its lane color.
#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    color: LaneColor,
}

/// Build the gutter spans for one commit row, sized to `gutter_width(max_lanes)`.
/// Draw order is through-lanes, then edge connectors, then the node on top, so the
/// node circle always wins its own cell and connectors meet the verticals beside.
/// `hollow` draws the node as an open circle (the synthetic `<current>` row and any
/// commit not yet pushed to a remote) instead of the filled disc. `tip` draws a branch
/// HEAD (a commit carrying a branch ref) as the ringed disc family (◉/◎) so the newest
/// commit on each branch stands out from its interior commits.
pub fn spans(row: &GraphRow, max_lanes: usize, hollow: bool, tip: bool) -> Vec<Span<'static>> {
    let width = gutter_width(max_lanes);
    let mut cells: Vec<Option<Cell>> = vec![None; width];

    for t in &row.through {
        put(&mut cells, col(t.lane), Glyph::VLINE, t.color);
    }
    for edge in &row.edges {
        draw_edge(&mut cells, edge, row.node_lane);
    }
    let node = match (tip, hollow) {
        (true, false) => Glyph::NODE_TIP,
        (true, true) => Glyph::NODE_TIP_HOLLOW,
        (false, false) => Glyph::NODE,
        (false, true) => Glyph::NODE_HOLLOW,
    };
    put(&mut cells, col(row.node_lane), node, row.node_color);

    to_spans(&cells)
}

/// Paint a single edge's corner + horizontal connector. `from == node_lane` is a
/// branch-out (lane leaves the node going down); `to == node_lane` is a merge-in
/// (a lane arrives from above and joins the node). A simple branch/merge shifts one
/// lane; an OCTOPUS merge (3+ parents) spawns several lanes off the same node, so an
/// edge can span 2+ lanes. The connector fills EVERY routing cell between the node
/// column and the target corner with a horizontal run, so far branch-out lanes stay
/// joined to the node instead of floating disconnected.
fn draw_edge(cells: &mut [Option<Cell>], edge: &Edge, node_lane: LaneIndex) {
    let c = edge.color;
    if edge.from == node_lane {
        // branch-out: node lane -> a lane going down (one or several lanes away).
        if edge.to > edge.from {
            fill_hline(cells, col(edge.from) + 1, col(edge.to), c);
            put(cells, col(edge.to), Glyph::CORNER_DOWN_LEFT, c);
        } else {
            fill_hline(cells, col(edge.to) + 1, col(edge.from), c);
            put(cells, col(edge.to), Glyph::CORNER_DOWN_RIGHT, c);
        }
    } else {
        // merge-in: a lane arriving from above (one or several lanes away) -> node.
        if edge.from > edge.to {
            put(cells, col(edge.from), Glyph::CORNER_UP_LEFT, c);
            fill_hline(cells, col(edge.to) + 1, col(edge.from), c);
        } else {
            put(cells, col(edge.from), Glyph::CORNER_UP_RIGHT, c);
            fill_hline(cells, col(edge.from) + 1, col(edge.to), c);
        }
    }
}

/// Fill the horizontal routing cells in `[start, end)` (the columns strictly between
/// the node and the corner) with [`Glyph::HLINE`] in color `c`. Empty when the corner
/// is in the node's adjacent routing cell (the one-lane-shift case), where the corner
/// alone connects; non-empty for a multi-lane (octopus) shift so the run stays solid.
fn fill_hline(cells: &mut [Option<Cell>], start: usize, end: usize, c: LaneColor) {
    for column in start..end {
        put(cells, column, Glyph::HLINE, c);
    }
}

/// Set the glyph + color at `column`, ignoring out-of-range columns. When a
/// connector lands on a cell that already holds a complementary corner (a
/// merge-in and a branch-out sharing one adjacent-lane cell on the same row),
/// the two are composed into a tee so BOTH edges stay connected instead of one
/// overwriting the other. The composed tee keeps the new (branch-out) edge's
/// color, the lane that continues downward.
fn put(cells: &mut [Option<Cell>], column: usize, ch: char, color: LaneColor) {
    if let Some(slot) = cells.get_mut(column) {
        let ch = match slot.map(|c| c.ch) {
            Some(prev) => compose(prev, ch),
            None => ch,
        };
        *slot = Some(Cell { ch, color });
    }
}

/// Combine two box-drawing corners that meet on one cell into the tee covering
/// both arms; otherwise the incoming glyph wins. `╯`+`╮` (above/below, both
/// joining left) -> `┤`; `╰`+`╭` (above/below, both joining right) -> `├`.
fn compose(prev: char, next: char) -> char {
    match (prev.min(next), prev.max(next)) {
        (Glyph::CORNER_DOWN_LEFT, Glyph::CORNER_UP_LEFT) => Glyph::TEE_LEFT,
        (Glyph::CORNER_DOWN_RIGHT, Glyph::CORNER_UP_RIGHT) => Glyph::TEE_RIGHT,
        _ => next,
    }
}

/// Turn the cell buffer into one styled `Span` per column (blanks included), so
/// the gutter is a fixed-width prefix the log columns align against.
fn to_spans(cells: &[Option<Cell>]) -> Vec<Span<'static>> {
    cells
        .iter()
        .map(|cell| match cell {
            Some(c) => Span::styled(c.ch.to_string(), Style::default().fg(c.color.color())),
            None => Span::styled(" ", Style::default().fg(Theme::BG)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_engine::build_layout;
    use crate::model::{Commit, SubjectSpan};

    fn commit(hash: &str, parents: &[&str]) -> Commit {
        Commit {
            hash: hash.to_string(),
            full_hash: hash.to_string(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            subject: vec![SubjectSpan::plain(hash)],
            refs: vec![],
            author: "t".to_string(),
            date: "01.01.2026, 00:00".to_string(),
            date_label: "01.01.2026, 00:00".to_string(),
            is_me: false,
            head: false,
            containing_branches: vec![],
            is_working: false,
            working: None,
        }
    }

    /// The rendered glyph at each gutter column (a space for a blank cell).
    fn glyphs(row: &GraphRow, max_lanes: usize) -> Vec<char> {
        spans(row, max_lanes, false, false)
            .iter()
            .map(|s| s.content.chars().next().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn octopus_merge_branch_out_row_has_a_continuous_connector() {
        // An octopus merge: M has THREE parents (a base + two side branches), so the
        // engine spawns extra lanes off M's node - branch-out edges that shift 2 and 3
        // lanes. Before the fix only a single routing cell was drawn per edge, leaving
        // blank cells between the node and the far corners (a floating `●─╮ ╮ ╮`).
        // Now every routing cell between the node and each corner is a horizontal run.
        //
        // Display order (newest first): M, then its three parents, then base.
        let commits = vec![
            commit("M", &["base", "ba", "bb"]),
            commit("ba", &["base"]),
            commit("bb", &["base"]),
            commit("base", &[]),
        ];
        let layout = build_layout(&commits);
        let merge = &layout.rows[0];
        // The merge node spawns multi-lane branch-out edges (|to-from| > 1 exists).
        assert!(
            merge.edges.iter().any(|e| e.from == merge.node_lane && e.to.abs_diff(e.from) > 1),
            "octopus merge emits a multi-lane branch-out edge"
        );
        let g = glyphs(merge, layout.max_lanes);
        let node = col(merge.node_lane);
        // Every cell from just past the node up to the farthest branch-out corner is a
        // connector glyph (HLINE or a corner), never a blank gap - the connector is
        // continuous across the whole octopus fan.
        let farthest = merge
            .edges
            .iter()
            .filter(|e| e.from == merge.node_lane && e.to > e.from)
            .map(|e| col(e.to))
            .max()
            .expect("a rightward branch-out");
        for (x, &cell) in g.iter().enumerate().take(farthest + 1).skip(node + 1) {
            assert!(
                cell == Glyph::HLINE || cell == Glyph::CORNER_DOWN_LEFT,
                "gutter cell {x} between node and farthest corner is connected, got {cell:?}"
            );
        }
    }

    #[test]
    fn simple_one_lane_branch_out_is_unchanged() {
        // A plain 2-parent merge shifts exactly one lane; the connector is the single
        // adjacent routing cell + corner, byte-identical to before the octopus fix.
        let commits = vec![
            commit("M", &["base", "side"]),
            commit("side", &["base"]),
            commit("base", &[]),
        ];
        let layout = build_layout(&commits);
        let merge = &layout.rows[0];
        let g = glyphs(merge, layout.max_lanes);
        let node = col(merge.node_lane);
        // One-lane shift to the right: routing cell is the HLINE, then the corner.
        assert_eq!(g[node + 1], Glyph::HLINE, "the single routing cell is a HLINE");
        assert_eq!(g[node + 2], Glyph::CORNER_DOWN_LEFT, "the corner meets the new lane");
    }

    #[test]
    fn hollow_flag_swaps_the_node_glyph() {
        // The unpushed/<current> path: spans must draw the OPEN circle when hollow, the
        // filled disc otherwise. `glyphs()` only ever passes false, so pin true here.
        let layout = build_layout(&[commit("a", &[])]);
        let row = &layout.rows[0];
        let solid: String = spans(row, layout.max_lanes, false, false).iter().flat_map(|s| s.content.chars()).collect();
        let hollow: String = spans(row, layout.max_lanes, true, false).iter().flat_map(|s| s.content.chars()).collect();
        assert!(solid.contains(Glyph::NODE), "pushed -> filled disc");
        assert!(hollow.contains(Glyph::NODE_HOLLOW), "unpushed/working -> open circle");
        assert!(!hollow.contains(Glyph::NODE), "the hollow row has no filled disc");
    }

    #[test]
    fn branch_tip_node_uses_the_ringed_disc_family() {
        let layout = build_layout(&[commit("a", &[])]);
        let row = &layout.rows[0];
        let chars = |hollow, tip| -> String {
            spans(row, layout.max_lanes, hollow, tip).iter().flat_map(|s| s.content.chars()).collect()
        };
        assert!(chars(false, true).contains(Glyph::NODE_TIP), "pushed tip -> filled fisheye");
        assert!(chars(true, true).contains(Glyph::NODE_TIP_HOLLOW), "unpushed tip -> open bullseye");
        assert!(!chars(false, true).contains(Glyph::NODE), "a tip is never the plain disc");
        assert!(!chars(true, false).contains(Glyph::NODE_TIP_HOLLOW), "a non-tip is never ringed");
    }
}
