//! Commit log: graph gutter + subject/refs/author/hash/date columns.

use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem};
use ratatui::Frame;

use super::graph;
use super::widgets::{centered_notice, list_state, pad, str_width, truncate};
use crate::graph_engine::GraphRow;
use crate::model::{subject_color, visible_commits, Commit, Ref, RefKind, RepoModel, Status};
use crate::theme::{Glyph, Theme};
use crate::view_state::{Pane, ViewState};

/// Smallest width (columns) an auto-fit right-hand column may shrink to: keeps a
/// short author / hash / date readable even when every visible cell is tiny.
const COL_MIN: usize = 6;
/// Largest width (columns) an auto-fit right-hand column may grow to: bounds a
/// pathologically long author name so the subject column is never starved.
const COL_MAX: usize = 40;
/// One trailing space of padding appended to each derived column width, so adjacent
/// columns never touch (matches the prior fixed widths' built-in breathing room).
const COL_PAD: usize = 1;

/// The right-hand column widths (columns) the log renders, DERIVED each layout from
/// the actual rendered cell content of the visible commits (see [`log_col_widths`]).
/// Computed once in the pure layout pass and threaded to BOTH the renderer and any
/// future hit-test so the drawn columns and the geometry always agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColWidths {
    pub author: usize,
    pub hash: usize,
    pub date: usize,
}

/// Auto-fit the three right-hand columns to the visible commits' rendered content:
/// each width is the widest rendered cell (author = abbreviated name + a HEAD `*`;
/// hash = short hash; date = the relative/absolute date label) across the visible
/// rows, clamped to `[COL_MIN, COL_MAX]` and padded by [`COL_PAD`]. The synthetic
/// `<current>` working row blanks its author/hash/date cells, so it contributes
/// width 0 and never inflates a column. PURE: shared by the renderer (via the layout
/// map) so the drawn widths and the geometry never drift.
pub fn log_col_widths(repo: &RepoModel, view: &ViewState) -> ColWidths {
    let mut author = 0usize;
    let mut hash = 0usize;
    let mut date = 0usize;
    for i in visible_commits(repo, view) {
        let c = &repo.commits[i];
        author = author.max(str_width(&author_cell(c)));
        hash = hash.max(str_width(&hash_cell(c)));
        date = date.max(str_width(&date_cell(c)));
    }
    let fit = |w: usize| w.clamp(COL_MIN, COL_MAX) + COL_PAD;
    ColWidths {
        author: fit(author),
        hash: fit(hash),
        date: fit(date),
    }
}

/// The author cell text as the log renders it: the abbreviated name (blank for the
/// synthetic working row) plus a trailing `*` on the HEAD commit. The one home both
/// the width-fit and `row_line` read, so the measured width matches the drawn cell.
fn author_cell(c: &Commit) -> String {
    let mut author = if c.is_working { String::new() } else { abbrev_author(&c.author) };
    if c.head {
        author.push('*');
    }
    author
}

/// The hash cell text as the log renders it: the short hash, blank for the synthetic
/// working row (its sentinel `WORKING_REV` is not a real hash).
fn hash_cell(c: &Commit) -> String {
    if c.is_working {
        String::new()
    } else {
        c.hash.clone()
    }
}

/// The date cell text as the log renders it: the relative/absolute date label, blank
/// for the synthetic working row (it is not an authored commit).
fn date_cell(c: &Commit) -> String {
    if c.is_working {
        String::new()
    } else {
        c.date_label.clone()
    }
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    repo: &RepoModel,
    view: &ViewState,
    status: &Status,
    cols: &ColWidths,
) {
    // Non-blocking startup over an empty repo: the shell is up but data has not
    // arrived. Show a centered placeholder until the first RepoLoaded fills the
    // list - a loading notice, or a non-fatal error line if the backend failed
    // (e.g. cwd is not a git repository). A loaded repo always renders the list.
    if repo.commits.is_empty() {
        match status {
            Status::Loading => {
                centered_notice(frame, area, "Loading history...", Theme::TEXT_DIM, Theme::BG)
            }
            Status::Error(msg) => centered_notice(
                frame,
                area,
                &format!("Error: {msg}"),
                Theme::ACCENT_CLOSE,
                Theme::BG,
            ),
            // Ready/Notice over an empty repo means the load finished and there are
            // genuinely no commits (an empty / unborn-HEAD repo). Show an explicit
            // "No commits" placeholder instead of a blank pane that looks broken.
            Status::Ready | Status::Notice(_) => {
                centered_notice(frame, area, "No commits", Theme::TEXT_DIM, Theme::BG)
            }
        }
        return;
    }

    let inner_w = area.width as usize;

    // The log shows only the commits passing search + filters; the selection is
    // an index into THIS filtered list. Single source: model::visible_commits.
    let max_lanes = repo.graph.max_lanes;
    let mut items: Vec<ListItem> = visible_commits(repo, view)
        .into_iter()
        .map(|i| {
            let c = &repo.commits[i];
            let graph_row = repo.graph.rows.get(i);
            let item = ListItem::new(row_line(c, graph_row, max_lanes, inner_w, cols, is_hollow(c, repo), is_tip(c)));
            // A multi-commit-marked row carries the same teal mark band as a marked file row
            // (the cursor row's selection highlight still wins over it via highlight_style).
            if view.commits_marked.contains(&c.hash) {
                item.style(Style::default().bg(Theme::FILES_MARKED_BG))
            } else {
                item
            }
        })
        .collect();
    // A trailing "Load more history" affordance when the loaded commits are a capped slice.
    // It is a footer (NOT a selectable commit): a click on it loads a deeper page. Shown
    // whenever more history exists (with a filter active, loading more feeds the filter).
    if repo.more_history {
        items.push(ListItem::new(load_more_line(inner_w)));
    }

    let sel_bg = if view.focus == Pane::Log {
        Theme::SELECTION_FOCUS
    } else {
        Theme::SELECTION_BLUR
    };

    let list = List::new(items)
        .style(Style::default().bg(Theme::BG).fg(Theme::TEXT))
        .highlight_style(Style::default().bg(sel_bg))
        .highlight_spacing(HighlightSpacing::Never);

    // A wheel tick over the log scrolls the viewport WITHOUT moving the selection
    // (`view.log_scroll`); otherwise the selection drives the offset as before.
    let mut state = list_state(view.log_scroll, view.log_sel, area.height as usize);
    frame.render_stateful_widget(list, area, &mut state);
}

/// The trailing "Load more history" footer row: a dim, centered affordance spanning the log
/// width (clicked to load a deeper commit page). Not a commit, never selected.
fn load_more_line(total_w: usize) -> Line<'static> {
    let label = format!("{} Load more history...", Glyph::LOAD_MORE);
    let pad = total_w.saturating_sub(label.chars().count()) / 2;
    Line::from(Span::styled(
        format!("{}{label}", " ".repeat(pad)),
        Style::default().fg(Theme::LINK),
    ))
}

/// Compose one commit row into a full-width [`Line`]. `graph_row` is this commit's
/// gutter layout (absent only if the engine and commit list ever desync).
fn row_line(
    c: &Commit,
    graph_row: Option<&GraphRow>,
    max_lanes: usize,
    total_w: usize,
    cols: &ColWidths,
    hollow: bool,
    tip: bool,
) -> Line<'static> {
    let gutter_w = graph::gutter_width(max_lanes);
    let mut spans = match graph_row {
        Some(row) => graph::spans(row, max_lanes, hollow, tip),
        None => vec![Span::raw(" ".repeat(gutter_w))],
    };

    let fixed = gutter_w + cols.author + cols.hash + cols.date;
    let middle_w = total_w.saturating_sub(fixed).max(8);
    // The commit-name (subject) column claims at least 40% of the log width even when
    // the flexible middle would be narrower (a squeezed pane); there the right-hand
    // columns clip at the edge - we deliberately have no horizontal scroll.
    let subj_w = middle_w.max(total_w * 2 / 5);

    // `!hollow` = the commit is pushed (not working, on a remote): the tag chip uses
    // the same locality fill as the graph node (filled lozenge when pushed).
    spans.extend(middle_spans(c, subj_w, !hollow));

    // author (bold when it's the current user; trailing '*' marks HEAD). The cells are
    // built by the SAME helpers the width-fit measured, so the drawn text never exceeds
    // its derived column. The column shows the ABBREVIATED name ("Alexandr Sidko" ->
    // "Alexandr S.") to stay narrow; the detail pane keeps the full name.
    let author = pad(&truncate(&author_cell(c), cols.author.saturating_sub(1)), cols.author);
    let author_style = Style::default().fg(Theme::TEXT);
    spans.push(Span::styled(
        author,
        if c.is_me {
            author_style.bold()
        } else {
            author_style
        },
    ));

    // hash + date (dim). The synthetic "<current>" row blanks both cells (see the
    // cell helpers), consistent with its blank author - it is not an authored commit.
    spans.push(Span::styled(
        pad(&truncate(&hash_cell(c), cols.hash), cols.hash),
        Style::default().fg(Theme::TEXT_DIM),
    ));
    spans.push(Span::styled(
        pad(&truncate(&date_cell(c), cols.date), cols.date),
        Style::default().fg(Theme::TEXT_DIM),
    ));

    Line::from(spans)
}

/// Whether commit `c` draws a HOLLOW graph node: the synthetic `<current>` working row,
/// or a commit not yet on any remote (the `unpushed` set, keyed by FULL hash). The one
/// home both render and its test read, so a regression (dropping `is_working`, or keying
/// on the short hash) is caught here, not only in manual QA.
fn is_hollow(c: &Commit, repo: &RepoModel) -> bool {
    c.is_working || repo.unpushed.contains(&c.full_hash)
}

/// Whether commit `c` is a branch TIP: a real commit carrying a branch ref decoration
/// (local or remote-tracking), i.e. the newest commit on some branch. Drives the ringed
/// node glyph (◉/◎). A tag-only decoration does NOT make a tip (a tag can sit on any
/// commit); the synthetic `<current>` row is never a tip (it is not an authored commit,
/// even though it carries the current-branch chip).
fn is_tip(c: &Commit) -> bool {
    !c.is_working
        && c.refs
            .iter()
            .any(|r| matches!(r.kind, RefKind::LocalBranch | RefKind::RemoteBranch))
}

/// Shorten an author name for the narrow log column: a multi-word name keeps its
/// first word and the initial of the second ("Alexandr Sidko" -> "Alexandr S."), so
/// the column stays tight; a single word (or empty) is returned unchanged. The detail
/// pane shows the FULL name, so nothing is lost there.
pub(super) fn abbrev_author(name: &str) -> String {
    let mut words = name.split_whitespace();
    let Some(first) = words.next() else {
        return String::new();
    };
    match words.next().and_then(|w| w.chars().next()) {
        Some(initial) => format!("{first} {initial}."),
        None => first.to_string(),
    }
}

/// Subject (with inline links) + ref labels, truncated/padded to `width`.
/// `pushed` (the commit is on a remote) drives the tag chip's locality fill.
fn middle_spans(c: &Commit, width: usize, pushed: bool) -> Vec<Span<'static>> {
    let refs_spans = ref_spans(&c.refs, pushed);
    let refs_w: usize = refs_spans.iter().map(|s| str_width(&s.content)).sum();

    // A 1-col trailing gap keeps the ref chip off the author column.
    const TRAIL_GAP: usize = 1;

    // Reserve space for refs (plus the lead gap and trailing gap) when they fit;
    // else drop them.
    let (show_refs, subj_budget) = if refs_w + 2 + TRAIL_GAP < width {
        (true, width - refs_w - 2 - TRAIL_GAP)
    } else {
        (false, width)
    };

    let mut out = subject_spans(c, subj_budget);
    // Display width (not char count): a wide-char subject consumes its true column
    // footprint, so the gap before the refs / the trailing pad lands correctly.
    let used: usize = out.iter().map(|s| str_width(&s.content)).sum();

    if show_refs {
        let gap = width - used - refs_w - TRAIL_GAP;
        out.push(Span::raw(" ".repeat(gap.max(1))));
        out.extend(refs_spans);
        out.push(Span::raw(" ".repeat(TRAIL_GAP)));
    } else if used < width {
        out.push(Span::raw(" ".repeat(width - used)));
    }
    out
}

/// Subject text spans, truncated to `budget` columns, preserving per-span tone
/// coloring (plain / link / a `<current>` change-count colored by file status).
fn subject_spans(c: &Commit, budget: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut remaining = budget;
    for span in &c.subject {
        if remaining == 0 {
            break;
        }
        let piece = truncate(&span.text, remaining);
        // Decrement by the piece's DISPLAY width so a wide-char span consumes its
        // true column footprint against the budget (not just its char count).
        remaining = remaining.saturating_sub(str_width(&piece));
        let style = Style::default().fg(subject_color(span.tone));
        out.push(Span::styled(piece, style));
    }
    out
}

/// Render ref labels as ` <diamond> name` chips (glyph accented, name dimmed).
/// The fill encodes LOCALITY: a remote-tracking branch -> filled diamond, a local
/// branch -> hollow diamond. A tag follows the SAME convention via its commit's
/// pushed state (`pushed`): a tag on a pushed commit -> filled lozenge, on an
/// unpushed / local-only commit -> hollow lozenge (a tag on an unpushed commit
/// cannot exist on the remote).
fn ref_spans(refs: &[Ref], pushed: bool) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for r in refs {
        let glyph = match r.kind {
            RefKind::RemoteBranch => Glyph::REF_ON_REMOTE,
            RefKind::LocalBranch => Glyph::REF_BRANCH,
            RefKind::Tag if pushed => Glyph::REF_TAG_ON_REMOTE,
            _ => Glyph::REF_TAG,
        };
        out.push(Span::raw(" "));
        out.push(Span::styled(glyph, Style::default().fg(Theme::REF)));
        out.push(Span::raw(" "));
        out.push(Span::styled(
            r.name.clone(),
            Style::default().fg(Theme::TEXT_DIM),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str, author: &str, hash: &str, date: &str) -> Commit {
        use crate::model::SubjectSpan;
        Commit {
            hash: hash.to_string(),
            full_hash: hash.to_string(),
            parents: vec![],
            subject: vec![SubjectSpan::plain(subject)],
            refs: vec![],
            author: author.to_string(),
            date: date.to_string(),
            date_label: date.to_string(),
            is_me: false,
            head: false,
            containing_branches: vec![],
            is_working: false,
            working: None,
        }
    }

    /// Total DISPLAY width of a composed row line (the sum of each span's columns).
    fn line_width(line: &Line<'static>) -> usize {
        line.spans.iter().map(|s| str_width(&s.content)).sum()
    }

    #[test]
    fn wide_char_subject_keeps_the_row_width_and_date_intact() {
        // A CJK + emoji subject must not shove the right-hand columns off the row: the
        // composed line stays exactly total_w columns and the date renders in full (a
        // char-count budget would over-fill the subject and clip the date's tail).
        let total_w = 120usize;
        let cols = ColWidths { author: 18, hash: 9, date: 18 };
        let date = "22.05.2026, 12:08";
        let cjk = commit("添加文件 with 🎉 emoji subject that is quite long here", "Test Author", "abc12345", date);
        let line = row_line(&cjk, None, 1, total_w, &cols, false, false);
        assert_eq!(
            line_width(&line), total_w,
            "a wide-char subject row is exactly total_w columns (no overflow shove)"
        );
        // The full date appears verbatim (the 'tail' is not clipped past the boundary).
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(text.contains(date), "the date column is rendered in full, not clipped");

        // The same row built from an ASCII subject is also exactly total_w (parity).
        let ascii = commit("add files with emoji subject that is quite long over here", "Test Author", "abc12345", date);
        let ascii_line = row_line(&ascii, None, 1, total_w, &cols, false, false);
        assert_eq!(line_width(&ascii_line), total_w, "the ASCII row is the same width");
    }

    #[test]
    fn author_abbreviation_keeps_first_word_and_second_initial() {
        assert_eq!(abbrev_author("Alexandr Sidko"), "Alexandr S.");
        assert_eq!(abbrev_author("Ada Lovelace"), "Ada L.");
        // Three words: first word + the SECOND word's initial.
        assert_eq!(abbrev_author("Mary Jane Watson"), "Mary J.");
        // A single token (handle / mononym) is left whole.
        assert_eq!(abbrev_author("torvalds"), "torvalds");
        assert_eq!(abbrev_author(""), "");
        // Surrounding whitespace does not produce a bogus empty initial.
        assert_eq!(abbrev_author("  Grace Hopper  "), "Grace H.");
    }

    #[test]
    fn hollow_node_for_working_row_and_unpushed_full_hash_only() {
        use crate::model::RepoModel;
        let mut repo = RepoModel::empty();
        let mut working = commit("s", "A", "", "");
        working.is_working = true;
        let pushed = commit("sub", "A", "aaaa1111", "d"); // full_hash == "aaaa1111"
        let unpushed = commit("sub", "A", "bbbb2222", "d");
        repo.unpushed.insert("bbbb2222".to_string());
        assert!(is_hollow(&working, &repo), "the working row is hollow");
        assert!(is_hollow(&unpushed, &repo), "a commit whose full hash is unpushed is hollow");
        assert!(!is_hollow(&pushed, &repo), "a pushed commit is solid");
        // The set is keyed on the FULL hash: a short-hash-shaped key must NOT flag it.
        repo.unpushed.insert("aaaa".to_string());
        assert!(!is_hollow(&pushed, &repo), "a short-hash key does not flag a full-hash commit");
    }

    #[test]
    fn branch_tip_is_a_real_commit_carrying_a_branch_ref_not_a_tag() {
        let mut plain = commit("sub", "A", "aaaa1111", "d");
        assert!(!is_tip(&plain), "a commit with no refs is not a tip");
        plain.refs = vec![Ref { name: "v1.0".to_string(), kind: RefKind::Tag }];
        assert!(!is_tip(&plain), "a tag-only decoration does not make a tip");
        let mut local = commit("sub", "A", "bbbb2222", "d");
        local.refs = vec![Ref { name: "main".to_string(), kind: RefKind::LocalBranch }];
        assert!(is_tip(&local), "a local-branch head is a tip");
        let mut remote = commit("sub", "A", "cccc3333", "d");
        remote.refs = vec![Ref { name: "origin/main".to_string(), kind: RefKind::RemoteBranch }];
        assert!(is_tip(&remote), "a remote-tracking head is a tip");
        // The synthetic <current> row carries the current-branch chip but is never a tip.
        let mut working = commit("s", "A", "", "");
        working.is_working = true;
        working.refs = vec![Ref { name: "main".to_string(), kind: RefKind::LocalBranch }];
        assert!(!is_tip(&working), "the working row is not a tip");
    }

    #[test]
    fn log_row_shows_the_abbreviated_author_not_the_full_name() {
        let cols = ColWidths { author: 18, hash: 9, date: 18 };
        let c = commit("subject", "Alexandr Sidko", "abc12345", "22.05.2026, 12:08");
        let line = row_line(&c, None, 1, 120, &cols, false, false);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Alexandr S."), "the log column abbreviates: {text:?}");
        assert!(!text.contains("Alexandr Sidko"), "the full surname is not in the log column");
    }

    #[test]
    fn ref_chip_glyph_encodes_locality() {
        let glyph = |kind, pushed| {
            let spans = ref_spans(&[Ref { name: "x".to_string(), kind }], pushed);
            spans.iter().flat_map(|s| s.content.chars()).collect::<String>()
        };
        // A branch's fill encodes locality intrinsically (its ref kind), independent of
        // the commit's pushed state: remote-tracking -> filled diamond, local -> unfilled.
        assert!(glyph(RefKind::RemoteBranch, false).contains(Glyph::REF_ON_REMOTE), "remote -> filled");
        assert!(glyph(RefKind::LocalBranch, true).contains(Glyph::REF_BRANCH), "local -> unfilled");
        assert!(!glyph(RefKind::LocalBranch, true).contains(Glyph::REF_ON_REMOTE), "local is never filled");
        // A tag follows the SAME diamond convention via its commit's pushed state: a tag on
        // a pushed commit is the filled diamond, on an unpushed commit the hollow diamond.
        assert!(glyph(RefKind::Tag, true).contains(Glyph::REF_TAG_ON_REMOTE), "pushed tag -> filled diamond");
        assert!(glyph(RefKind::Tag, false).contains(Glyph::REF_TAG), "unpushed tag -> hollow diamond");
        assert!(!glyph(RefKind::Tag, false).contains(Glyph::REF_TAG_ON_REMOTE), "unpushed tag is never filled");
    }

    #[test]
    fn tag_chip_fill_follows_the_row_pushed_state() {
        // End-to-end wiring: row_line passes `!hollow` (pushed) down to the tag chip, so a
        // tag on a solid (pushed) row is the filled lozenge and on a hollow (unpushed) row
        // the hollow lozenge - matching that row's graph node.
        let cols = ColWidths { author: 18, hash: 9, date: 18 };
        let mut c = commit("release", "A", "abc12345", "22.05.2026, 12:08");
        c.refs = vec![Ref { name: "v1.0".to_string(), kind: RefKind::Tag }];
        let row_chars = |hollow| -> String {
            row_line(&c, None, 1, 120, &cols, hollow, false)
                .spans.iter().flat_map(|s| s.content.chars()).collect()
        };
        assert!(row_chars(false).contains(Glyph::REF_TAG_ON_REMOTE), "pushed row -> filled tag");
        assert!(row_chars(true).contains(Glyph::REF_TAG), "unpushed row -> hollow tag");
    }
}
