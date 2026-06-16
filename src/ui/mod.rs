//! UI composition: drives the pure layout geometry and delegates each region to
//! its module. The whole frame is laid out by [`layout::compute_layout`] - the
//! same geometry the runtime hit-tests against - so what is drawn and what
//! responds to clicks never diverge.

pub mod detail_panel;
pub mod diff_view;
mod dropdown;
mod files_panel;
mod files_toolbar;
mod graph;
mod hint_bar;
pub mod layout;
mod dialog;
mod log_panel;
mod menu_bar;
mod revert_modal;
mod search_history;
mod toolbar;
mod widgets;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Block;
use ratatui::Frame;

use crate::model::{FlatKind, RepoModel, Status};
use crate::theme::Theme;
use crate::view_state::ViewState;

use layout::{compute_layout, LayoutMap};

/// Render the whole application for one frame. Pure read-only: it takes the
/// domain + view halves and the lifecycle `status` directly and never mutates,
/// imports store, or sees `Msg`. `status` drives the per-panel loading / error
/// placeholders (the panels read it alongside the model's emptiness).
pub fn view(frame: &mut Frame, repo: &RepoModel, view: &ViewState, status: &Status) {
    let area = frame.area();

    // A terminal can report a zero-row or zero-column frame (a degenerate window, or
    // a transient SIGWINCH race during resize). There is nothing to draw, and every
    // downstream widget that writes a Paragraph would index off a zero-extent buffer
    // and panic ratatui; bail before any render.
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Paint the global background first so every gap inherits the panel color.
    frame.render_widget(Block::default().style(Style::default().bg(Theme::BG)), area);

    let lm = compute_layout(area, repo, view);

    menu_bar::render(frame, &lm, view);
    separators(frame, area, &lm);

    // The diff/preview viewer. While the editable diff is open the preview holds the
    // live HEAD-vs-working diff and the renderer draws the editable cursor on the new
    // side; otherwise it shows the selected commit's historical diff.
    if let Some(dl) = &lm.diff {
        let file_selected = selected_file_is_file(repo, view);
        // A read-only inspect overlay (Show Current Revision / a compared revision) masks the
        // preview: render ITS view read-only under its own header title, else the file preview.
        let (preview, inspect_title) = match &view.inspect {
            Some(iv) => (Some(&iv.view), Some(iv.title.as_str())),
            None => (repo.preview.as_ref(), None),
        };
        diff_view::render(frame, dl, preview, view, status, file_selected, inspect_title);
    }

    // The two toolbar halves are split by a 1-col divider column (the `split2` gap)
    // that neither half paints and the pane vsep (which starts BELOW the toolbar) never
    // covers - it would otherwise show the dark global BG as a thin seam splitting the
    // bar. Paint BG_TOOLBAR across the whole band first so the strip reads as one piece;
    // the halves then render on top in the same color.
    frame.render_widget(
        Block::default().style(Style::default().bg(Theme::BG_TOOLBAR)),
        area.intersection(union_h(lm.toolbar, lm.files_toolbar)),
    );
    toolbar::render(frame, lm.toolbar, &lm.toolbar_ui, view, status);
    files_toolbar::render(frame, &lm, view);
    log_panel::render(frame, lm.log_list, repo, view, status, &lm.log_col_widths);
    files_panel::render_header(frame, lm.files_header, repo, view);
    files_panel::render(frame, lm.files_list, repo, view);
    detail_panel::render(frame, lm.detail, repo.detail.as_ref(), view, status);
    hint_bar::render(frame, &lm, view);

    // The popups overlay the panels, so they are drawn last (only one is ever
    // open: the menus and filter dropdowns are mutually exclusive).
    dropdown::render(frame, &lm, repo, view);
    search_history::render(frame, &lm, view);
    menu_bar::render_dropdown(frame, &lm, view);
    menu_bar::render_commit_menu(frame, &lm, view);
    menu_bar::render_files_menu(frame, &lm, view);

    // The revert confirmation modal + the input/confirm dialogs overlay EVERYTHING
    // (including the dropdown), so they are drawn absolutely last. Only one is ever
    // open at a time.
    revert_modal::render(frame, &lm, view);
    dialog::render(frame, &lm, view);
}

/// Draw the horizontal hairlines (under the toggles bar, under the diff region,
/// under the toolbar, and between files and detail) plus the vertical separator
/// beside the log list. All derived from the region rects so they always land in
/// the separator rows `compute_layout` reserved.
fn separators(frame: &mut Frame, area: Rect, lm: &LayoutMap) {
    // On a short/narrow frame the layout solver collapses panes to height/width 0
    // and parks them AT the buffer edge (y == area.bottom()), so a row_below them
    // lands one past the last valid row. Clip every derived separator rect to the
    // frame so an out-of-bounds origin draws nothing instead of panicking ratatui.
    let clip = |r: Rect| area.intersection(r);

    // (No separator under the menu bar: it sits flush against the content below.)

    // The toolbar row spans both toolbars; its separators must too. Union the
    // left (search/filters) and right (files) toolbar rects so the hairlines run
    // the full body width, not just under the narrower left strip.
    let toolbar_row = union_h(lm.toolbar, lm.files_toolbar);

    // hsep between the diff region and the log body (row above the toolbars).
    if lm.diff.is_some() {
        widgets::hsep(frame, clip(row_above(toolbar_row)));
    }

    // hsep under the toolbars (above the panes).
    widgets::hsep(frame, clip(row_below(toolbar_row)));

    // Vertical separator between the log list and the right column.
    widgets::vsep(frame, clip(vsep_col(area, lm)));

    // hsep between files list and detail in the right column.
    widgets::hsep(frame, clip(row_below(lm.files_list)));
}

/// The 1-row strip immediately below `r`.
fn row_below(r: Rect) -> Rect {
    Rect {
        x: r.x,
        y: r.bottom(),
        width: r.width,
        height: 1,
    }
}

/// The horizontal span covering both `left` and `right` (and the gap between),
/// at `left`'s row. Used to run the toolbar-row hairlines across both toolbars.
fn union_h(left: Rect, right: Rect) -> Rect {
    Rect {
        x: left.x,
        y: left.y,
        width: right.right().saturating_sub(left.x),
        height: left.height,
    }
}

/// The 1-row strip immediately above `r`.
fn row_above(r: Rect) -> Rect {
    Rect {
        x: r.x,
        y: r.y.saturating_sub(1),
        width: r.width,
        height: 1,
    }
}

/// Whether the files selection currently points at a FILE row (not a directory or
/// an out-of-range index). Pure (model + view only): the diff viewer reads it to
/// tell "a file is selected, preview still loading" from "nothing selected". The
/// runtime's `selected_file_path` (in `store`) is the IO-side mirror of this walk.
fn selected_file_is_file(repo: &RepoModel, view: &ViewState) -> bool {
    matches!(
        crate::model::visible_file_rows(repo, view).get(view.files_sel).map(|(r, _)| &r.node),
        Some(FlatKind::File { .. })
    )
}

/// The 1-col vertical strip between the log list and the right column.
fn vsep_col(area: Rect, lm: &LayoutMap) -> Rect {
    Rect {
        x: lm.log_list.right(),
        y: lm.log_list.y,
        width: 1,
        height: area.bottom().saturating_sub(lm.log_list.y),
    }
}
