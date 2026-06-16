//! Changed-files tree for the selected commit.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, HighlightSpacing, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::model::{
    selected_commit, status_color, Commit, FileStatus, FlatKind, FlatRow, RepoModel,
};
#[cfg(test)]
use crate::model::TreeNode;
use crate::theme::{Glyph, Theme};
use crate::ui::layout::MARK_GUTTER_W;
use crate::ui::widgets::truncate;
use crate::view_state::{Pane, ViewState};

/// Draw the one-row "A vs B" header above the files list: it names the diff's two
/// sides so the user always knows what the left (base) and right (selected) panes show.
/// A real commit reads `<parent> vs <commit> (selected)`; the `<current>` working row
/// reads `HEAD/<head> vs current changes`. No-op when the carved rect is zero height.
pub fn render_header(frame: &mut Frame, area: Rect, repo: &RepoModel, view: &ViewState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(
        Block::default().style(Style::default().bg(Theme::BG)),
        area,
    );
    let Some(c) = selected_commit(repo, view) else { return };
    let text = format!(" {}", truncate(&vs_header(c), area.width.saturating_sub(1) as usize));
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, Style::default().fg(Theme::TEXT_DIM)))),
        area,
    );
}

/// The "left vs right" label for the selected commit: the diff BASE (left) vs the
/// selected side (right). A real commit shows `<parent> vs <commit> (selected)` (the
/// parent's blob vs the commit's blob); the `<current>` working row shows
/// `HEAD/<head> vs current changes` (HEAD vs the live working tree). `parents[0]` is
/// the base's short hash in both cases (the mainline parent / the HEAD commit).
fn vs_header(c: &Commit) -> String {
    let base = c.parents.first().map(String::as_str);
    if c.is_working {
        match base {
            Some(head) => format!("HEAD/{head} vs current changes"),
            None => "HEAD vs current changes".to_string(),
        }
    } else {
        format!("{} vs {} (selected)", base.unwrap_or("(root)"), c.hash)
    }
}

pub fn render(frame: &mut Frame, area: Rect, repo: &RepoModel, view: &ViewState) {
    // Recompute the visible rows per frame from the canonical tree paired with each
    // file row's full path, honoring the Flat toggle; the store hit-tests against the
    // same flatten order, so selection + marked indices stay aligned.
    let rows = crate::model::visible_file_rows(repo, view);
    // A files-search query that matches nothing leaves the pane blank, which reads as
    // "broken / no changes" rather than "no matches". Render an explicit placeholder so
    // the empty result is unambiguous (only under an active query - a clean tree with no
    // query keeps its plain empty pane).
    if rows.is_empty() && !view.files_search.is_empty() {
        let note = format!(" No files match \"{}\"", view.files_search);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(note, Style::default().fg(Theme::TEXT_FAINT))))
                .style(Style::default().bg(Theme::BG)),
            area,
        );
        return;
    }
    // A tracked-but-gitignored file or folder (surfaced only in the All view) renders
    // faint so it reads as "ignored" without hiding it. The flags are computed once here
    // (and unit-tested) so the panel render and the reconstruction logic never drift.
    let ignored = ignored_flags(&rows, &repo.ignored);
    let items: Vec<ListItem> = rows
        .iter()
        .zip(ignored)
        .map(|((r, path), ignored)| {
            // A marked file row gets a distinct background fill PLUS a filled dot in
            // the leading mark gutter; an UNMARKED row leaves the gutter blank and
            // carries no per-row background.
            let marked = path.as_deref().is_some_and(|p| view.is_marked(p));
            ListItem::new(row_line(r, marked, ignored))
        })
        .collect();

    let sel_bg = if view.focus == Pane::Files {
        Theme::SELECTION_FOCUS
    } else {
        Theme::SELECTION_BLUR
    };

    let list = List::new(items)
        .style(Style::default().bg(Theme::BG).fg(Theme::TEXT))
        .highlight_style(Style::default().bg(sel_bg))
        .highlight_spacing(HighlightSpacing::Never);

    // A wheel tick over the files panel scrolls the viewport WITHOUT moving the
    // selection (`view.files_scroll`); otherwise the selection drives the offset.
    let mut state = crate::ui::widgets::list_state(view.files_scroll, view.files_sel, area.height as usize);
    frame.render_stateful_widget(list, area, &mut state);
}

/// Per-row "is this gitignored?" flags, in flatten order: a FILE row by its own full
/// path, a DIRECTORY row by its reconstructed collapsed path. File rows carry their
/// path directly; dir rows intentionally do NOT (a dir path must never become a file
/// selection), so a depth-stack rebuilds each dir's full path - holding the ancestor
/// dir name at each depth, a dir at depth d truncates to d then pushes its name so
/// `join('/')` is the SAME key the backend's `collect_ignored` set uses. Pure + tested,
/// so the render and the reconstruction can never silently desync.
fn ignored_flags(
    rows: &[(FlatRow, Option<String>)],
    ignored: &std::collections::HashSet<String>,
) -> Vec<bool> {
    let mut dir_stack: Vec<String> = Vec::new();
    rows.iter()
        .map(|(r, path)| match &r.node {
            FlatKind::Dir { name, .. } => {
                dir_stack.truncate(r.depth);
                dir_stack.push(name.clone());
                ignored.contains(&dir_stack.join("/"))
            }
            FlatKind::File { .. } => path.as_deref().is_some_and(|p| ignored.contains(p)),
        })
        .collect()
}

/// Render one flattened row, led by the [`MARK_GUTTER_W`]-wide mark gutter. A
/// MARKED file row shows the filled gutter dot and paints the whole-row
/// multi-selection band; an unmarked row leaves the gutter blank and carries NO
/// row background, so the empty-set render is free of marks. The cursor highlight
/// (applied by the `List` widget) still overrides the band on the active row, so a
/// row that is both cursor AND marked reads as the cursor.
fn row_line(r: &FlatRow, marked: bool, ignored: bool) -> Line<'static> {
    let mut spans = vec![mark_gutter_span(marked)];
    spans.extend(build_row_spans(r, ignored));
    let line = Line::from(spans);
    if marked {
        line.style(Style::default().bg(Theme::FILES_MARKED_BG))
    } else {
        line
    }
}

/// The leading mark-gutter cell: the filled dot on a marked row, else blank. Its
/// width is always [`MARK_GUTTER_W`], matching the gutter rect the runtime
/// hit-tests, so a click in this column maps to a mark toggle.
fn mark_gutter_span(marked: bool) -> Span<'static> {
    let glyph = if marked {
        Glyph::MARK.to_string()
    } else {
        " ".repeat(MARK_GUTTER_W as usize)
    };
    Span::styled(glyph, Style::default().fg(Theme::TEXT_DIM))
}

/// The row body spans (after the leading gutter cell). The body indent drops the
/// gutter's leading column so the total leading width is unchanged.
fn build_row_spans(r: &FlatRow, ignored: bool) -> Vec<Span<'static>> {
    // The gutter consumes the row's first column, so the body indent is the
    // remaining tree indentation (`depth*2` spaces).
    let indent = " ".repeat(r.depth * 2);
    match &r.node {
        FlatKind::Dir {
            name,
            file_count,
            expanded,
        } => {
            let chevron = if *expanded {
                Glyph::CHEVRON_OPEN
            } else {
                Glyph::CHEVRON_CLOSED
            };
            let count = if *file_count == 1 {
                "1 file".to_string()
            } else {
                format!("{file_count} files")
            };
            // No folder glyph survives the render font (every folder code point
            // is blank in Noto Sans Mono), so directories are marked by the
            // expand chevron plus a trailing slash on the name. Dir rows have NO
            // checkbox: the gutter cell stays blank (a dir's descendants are marked
            // via Space / Ctrl-click, not a gutter click). An ignored folder dims its
            // whole row (chevron + name + count) to the faint tone.
            let name_fg = if ignored { Theme::TEXT_FAINT } else { Theme::TEXT };
            let dim_fg = if ignored { Theme::TEXT_FAINT } else { Theme::TEXT_DIM };
            vec![
                Span::raw(indent),
                Span::styled(format!("{chevron} "), Style::default().fg(dim_fg)),
                Span::styled(format!("{name}/"), Style::default().fg(name_fg)),
                Span::raw("  "),
                Span::styled(count, Style::default().fg(dim_fg)),
            ]
        }
        FlatKind::File { name, status } => {
            // A gitignored file dims its leading glyph too, so the whole row reads faint.
            let glyph_fg = if ignored { Theme::TEXT_FAINT } else { Theme::TEXT_DIM };
            vec![
                Span::raw(indent),
                Span::styled(format!("{} ", Glyph::FILE), Style::default().fg(glyph_fg)),
                Span::styled(name.clone(), name_style(*status, ignored)),
            ]
        }
    }
}

/// File-name style by git status: the [`status_color`] accent, plus a
/// strike-through for a deleted file (it no longer exists, so the name reads as
/// crossed out). An Unchanged file (All-files view) reads as plain text; a
/// gitignored file overrides the color to the faint tone regardless of status.
fn name_style(status: FileStatus, ignored: bool) -> Style {
    let fg = if ignored { Theme::TEXT_FAINT } else { status_color(status) };
    let style = Style::default().fg(fg);
    match status {
        FileStatus::Deleted => style.add_modifier(Modifier::CROSSED_OUT),
        _ => style,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> TreeNode {
        TreeNode::File { name: name.to_string(), status: FileStatus::Unchanged }
    }
    fn dir(name: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode::Dir {
            name: name.to_string(),
            file_count: children.len(),
            expanded: true,
            children,
        }
    }

    /// The panel's dir-key reconstruction must agree with the backend's `collect_ignored`
    /// keys for COLLAPSED ("node_modules/leftpad") and NESTED ("…/lib") dirs, so an
    /// ignored folder + its descendants dim while a sibling normal tree stays bright.
    /// This locks the two independent path derivations together.
    #[test]
    fn ignored_flags_match_backend_keys_for_collapsed_and_nested_dirs() {
        // A collapsed ignored dir with a nested subdir, beside a normal dir.
        let tree = vec![
            dir("cmd", vec![file("main.go")]),
            dir(
                "node_modules/leftpad",
                vec![file("index.js"), dir("lib", vec![file("deep.js")])],
            ),
        ];
        // The backend's `collect_ignored` keys these exact collapsed paths.
        let ignored: std::collections::HashSet<String> = [
            "node_modules/leftpad",
            "node_modules/leftpad/index.js",
            "node_modules/leftpad/lib",
            "node_modules/leftpad/lib/deep.js",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let rows = TreeNode::flatten_paths_view(&tree, false);
        let flags = ignored_flags(&rows, &ignored);
        // Pair each flag with its row name for a legible assertion.
        let labeled: Vec<(String, bool)> = rows
            .iter()
            .zip(&flags)
            .map(|((r, _), &ig)| {
                let name = match &r.node {
                    FlatKind::Dir { name, .. } | FlatKind::File { name, .. } => name.clone(),
                };
                (name, ig)
            })
            .collect();

        let flag_of = |n: &str| labeled.iter().find(|(name, _)| name == n).map(|(_, f)| *f);
        assert_eq!(flag_of("cmd"), Some(false), "a normal dir is not dimmed");
        assert_eq!(flag_of("main.go"), Some(false), "a file in a normal dir is not dimmed");
        assert_eq!(flag_of("node_modules/leftpad"), Some(true), "the collapsed ignored dir is dimmed");
        assert_eq!(flag_of("index.js"), Some(true), "a file in the ignored dir is dimmed");
        assert_eq!(flag_of("lib"), Some(true), "the NESTED ignored subdir is dimmed");
        assert_eq!(flag_of("deep.js"), Some(true), "the deeply-nested ignored file is dimmed");
    }

    /// In flat mode there are no dir rows; ignored FILES (full-path rows) still flag.
    #[test]
    fn ignored_flags_flat_mode_keys_files_by_full_path() {
        let tree = vec![dir("node_modules/leftpad", vec![file("index.js")]), dir("cmd", vec![file("main.go")])];
        let ignored: std::collections::HashSet<String> =
            ["node_modules/leftpad/index.js".to_string()].into_iter().collect();
        let rows = TreeNode::flatten_paths_view(&tree, true); // flat
        let flags = ignored_flags(&rows, &ignored);
        let any_dir = rows.iter().any(|(r, _)| matches!(r.node, FlatKind::Dir { .. }));
        assert!(!any_dir, "flat mode emits no dir rows");
        let idx = rows
            .iter()
            .position(|(_, p)| p.as_deref() == Some("node_modules/leftpad/index.js"))
            .expect("the ignored file row exists in flat mode");
        assert!(flags[idx], "the ignored file is flagged by its full path in flat mode");
    }
}
