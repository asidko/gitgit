//! Pure lane-routing engine for the commit-graph gutter.
//!
//! Models the commit log as a DAG and lays out continuous, connected lane edges -
//! the data the UI renders as box-drawing verticals, corners and tees. This is
//! core logic: it imports only [`crate::model::Commit`] (reads hash + parents) and
//! [`crate::theme::LaneColor`]. NO ui, NO IO. Built once after the fixtures load
//! and stored on the model, so [`crate::store::AppState::apply`] stays ZERO IO.
//!
//! Algorithm (deterministic, walks rows top -> bottom, newest first as displayed):
//! maintain `active[lane] = Some(lane awaiting its commit)`. Each commit occupies
//! the lane reserved for it (or the left-most free lane). Its first parent
//! continues that lane (keeping its color); extra parents (a merge) reserve new
//! lanes with freshly minted colors. Every other active lane passes straight down.
//! Each row records an [`Edge`] per lane carried into the next row, capturing the
//! column it leaves from and the column it lands on. A simple branch/merge shifts
//! one column; an octopus merge (3+ parents) spawns several lanes off one node, so a
//! branch-out edge can span 2+ columns. The UI fills the routing cells between the
//! node and the corner with a horizontal run, so even a multi-lane fan renders as a
//! connected staircase, never a floating diagonal.

use crate::model::Commit;
use crate::theme::LaneColor;

/// A gutter lane index (0 = left-most / trunk).
pub type LaneIndex = usize;

/// A connector drawn in this row between the node lane and another lane.
/// `from == node_lane` is a branch-out (a new lane leaves the node going down);
/// `to == node_lane` is a merge-in (a side lane arrives from above and joins the
/// node). A simple branch/merge shifts ONE lane (`|to - from| == 1`); an OCTOPUS
/// merge (3+ parents) spawns several lanes off the same node, so a branch-out edge
/// can span 2+ lanes (`|to - from| >= 1`). The UI fills every routing cell between
/// the node column and the corner with a horizontal run, so a multi-lane edge stays
/// connected. Pure verticals are NOT edges - they are `through` lanes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: LaneIndex,
    pub to: LaneIndex,
    pub color: LaneColor,
}

/// A lane passing straight down through a row (a vertical), with its own color so
/// a side branch keeps its hue while crossing an unrelated commit's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Through {
    pub lane: LaneIndex,
    pub color: LaneColor,
}

/// The layout of a single commit row: where the node sits, which lanes pass
/// straight through, and the edges connecting this row's columns to the next.
#[derive(Clone, Debug)]
pub struct GraphRow {
    /// Lane carrying this row's commit node.
    pub node_lane: LaneIndex,
    /// Color of the node's lane.
    pub node_color: LaneColor,
    /// Lanes (other than the node lane) that pass straight down through this row.
    pub through: Vec<Through>,
    /// Connectors from this row's columns to the next row's columns. A simple
    /// branch/merge shifts one lane; an octopus merge can span several.
    pub edges: Vec<Edge>,
}

/// The whole gutter: one [`GraphRow`] per commit (in `commits` order) plus the
/// peak lane count, which fixes the gutter width.
#[derive(Clone, Debug)]
pub struct GraphLayout {
    pub rows: Vec<GraphRow>,
    pub max_lanes: usize,
}

/// The lane-color palette, cycled as new branches appear. Ordered so the trunk is
/// Green and the first few side branches read as distinct hues.
const PALETTE: [LaneColor; 6] = [
    LaneColor::Green,
    LaneColor::Orange,
    LaneColor::Teal,
    LaneColor::Magenta,
    LaneColor::Blue,
    LaneColor::Red,
];

/// A lane awaiting its commit: the hash to draw next on this lane, plus the lane's
/// color (minted when the lane opened, kept stable down the lane).
#[derive(Clone)]
struct Lane {
    hash: String,
    color: LaneColor,
}

/// Mutable walk state: the active lanes and the next palette color to mint.
struct Builder {
    active: Vec<Option<Lane>>,
    next_color: usize,
}

impl Builder {
    fn new() -> Self {
        Self {
            active: Vec::new(),
            next_color: 0,
        }
    }

    /// Next palette color, advancing the cursor (cycles when exhausted).
    fn mint(&mut self) -> LaneColor {
        let c = PALETTE[self.next_color % PALETTE.len()];
        self.next_color += 1;
        c
    }

    /// Left-most free lane index, extending the lane vector when all are taken.
    fn free_lane(&mut self) -> LaneIndex {
        match self.active.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.active.push(None);
                self.active.len() - 1
            }
        }
    }

    /// Index of the left-most lane currently awaiting `hash`, if any.
    fn lane_of(&self, hash: &str) -> Option<LaneIndex> {
        self.active
            .iter()
            .position(|l| l.as_ref().is_some_and(|l| l.hash == hash))
    }

    /// Lay out one commit row, returning the row AND advancing lane state to the
    /// next row. Edges are captured by snapshotting the columns each surviving
    /// lane occupies before vs. after wiring this commit's parents.
    fn step(&mut self, commit: &Commit) -> GraphRow {
        // (1) Lane this commit draws on: the reserved one, else a fresh left-most
        // lane with a newly minted color.
        let node_lane = match self.lane_of(&commit.hash) {
            Some(i) => i,
            None => {
                let i = self.free_lane();
                let color = self.mint();
                self.active[i] = Some(Lane {
                    hash: commit.hash.clone(),
                    color,
                });
                i
            }
        };
        let node_color = self.active[node_lane]
            .as_ref()
            .expect("node lane was just assigned")
            .color;

        // Snapshot each lane's column under the hash it carried INTO this row, so
        // a merging side lane keeps its column for the outgoing edge.
        let incoming: Vec<Option<Lane>> = self.active.clone();

        // (2) Every OTHER lane awaiting this commit is a branch merging in; it
        // terminates here (its edge bends into the node lane below).
        for (i, lane) in self.active.iter_mut().enumerate() {
            if i != node_lane && lane.as_ref().is_some_and(|l| l.hash == commit.hash) {
                *lane = None;
            }
        }

        // (3) Wire parents. First parent continues the node lane (same color);
        // extra parents reuse a lane already awaiting them, else open a new lane
        // with a freshly minted color (a new branch = a new color).
        match commit.parents.split_first() {
            Some((first, rest)) => {
                self.active[node_lane] = Some(Lane {
                    hash: first.clone(),
                    color: node_color,
                });
                for parent in rest {
                    if self.lane_of(parent).is_none() {
                        let i = self.free_lane();
                        let color = self.mint();
                        self.active[i] = Some(Lane {
                            hash: parent.clone(),
                            color,
                        });
                    }
                }
            }
            // (4) Root commit: no parents -> its lane closes.
            None => self.active[node_lane] = None,
        }

        // (5) through-lanes = lanes occupied both before AND after wiring (so they
        // pass straight down), excluding the node lane. A lane the node just opened
        // is a branch-out EDGE, not a vertical, so it is filtered out here.
        let through = self
            .active
            .iter()
            .enumerate()
            .filter_map(|(i, lane)| {
                let lane = lane.as_ref()?;
                let passes_through = i != node_lane && column_in(&incoming, &lane.hash).is_some();
                passes_through.then_some(Through {
                    lane: i,
                    color: lane.color,
                })
            })
            .collect();

        // (6) Edges connecting this row's columns to the next row's columns: lanes
        // continuing/branching down, plus any side lane merging into the node.
        let edges = self.edges_from(&incoming, commit, node_lane);

        // (7) Trim trailing empty lanes so the width tracks the live topology.
        while matches!(self.active.last(), Some(None)) {
            self.active.pop();
        }

        GraphRow {
            node_lane,
            node_color,
            through,
            edges,
        }
    }

    /// Build this row's connector edges (corners), each between the node lane and
    /// an adjacent lane. Two kinds; pure verticals are excluded (they are `through`
    /// lanes), so every edge has `from != to`:
    ///
    /// - branch-out: a lane the node just spawned leaves the node going down. Its
    ///   source is the node lane (`from == node_lane`), its target the new column.
    /// - merge-in: a side lane present in `incoming` but now closed because it was
    ///   awaiting THIS commit bends from above into the node (`to == node_lane`).
    ///
    /// Edge color is the moving lane's own color, so a merge keeps the side
    /// branch's hue as it joins the trunk.
    fn edges_from(
        &self,
        incoming: &[Option<Lane>],
        commit: &Commit,
        node_lane: LaneIndex,
    ) -> Vec<Edge> {
        let branch_out = self.active.iter().enumerate().filter_map(move |(to, lane)| {
            let lane = lane.as_ref()?;
            // A lane the node just opened has no prior column -> emanates from the
            // node. Lanes that were already flowing keep their own column (vertical
            // through-lanes), so they are not edges.
            if to != node_lane && column_in(incoming, &lane.hash).is_none() {
                Some(Edge {
                    from: node_lane,
                    to,
                    color: lane.color,
                })
            } else {
                None
            }
        });
        let merge_in = incoming.iter().enumerate().filter_map(move |(from, lane)| {
            let lane = lane.as_ref()?;
            // A side lane that was awaiting THIS commit merges into the node. It
            // terminates as a branch here regardless of whether a freshly opened
            // parent lane reuses its slot index on the same row - the merge edge
            // must still be emitted, or the merge would visibly drop.
            if from != node_lane && lane.hash == commit.hash {
                Some(Edge {
                    from,
                    to: node_lane,
                    color: lane.color,
                })
            } else {
                None
            }
        });
        branch_out.chain(merge_in).collect()
    }
}

/// Column of the lane awaiting `hash` in a lane snapshot, if any.
fn column_in(lanes: &[Option<Lane>], hash: &str) -> Option<LaneIndex> {
    lanes
        .iter()
        .position(|l| l.as_ref().is_some_and(|l| l.hash == hash))
}

/// Build the gutter layout for the commit log (in display order, newest first).
/// Pure: derives every lane and edge from the commits' hashes and parents.
pub fn build_layout(commits: &[Commit]) -> GraphLayout {
    let mut builder = Builder::new();
    let mut rows = Vec::with_capacity(commits.len());
    let mut max_lanes = 0;
    for commit in commits {
        let row = builder.step(commit);
        max_lanes = max_lanes.max(lanes_used(&row));
        rows.push(row);
    }
    GraphLayout { rows, max_lanes }
}

/// Peak lane index touched by a row (node + through + edge endpoints), as a
/// 1-based count - the gutter must be wide enough for all of them.
fn lanes_used(row: &GraphRow) -> usize {
    let mut top = row.node_lane;
    for t in &row.through {
        top = top.max(t.lane);
    }
    for e in &row.edges {
        top = top.max(e.from).max(e.to);
    }
    top + 1
}
