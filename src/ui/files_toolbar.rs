//! Right-hand files toolbar: meaningful controls over the file-changes column.
//!
//! Renders into the exact action rects from [`super::layout`], so the hit-test
//! geometry and the drawn glyphs always agree. PURE: the runtime maps control
//! clicks to messages; this module only draws.
//!
//! Controls (in strip order): a files-search field (lens + query + clear `x` + `.*`
//! regex toggle) that filters the changed-files list by path, then the one-word "Flat"
//! toggle (flat no-folder list vs the nested tree), the "All" toggle (full tree vs
//! changed-only) - each reflecting its on/off state - then a glyph-only focus button
//! (the U+25CE bullseye) that reveals the opened file in the list. The two toggles keep
//! an underlined accelerator letter (casing preserved); Alt+letter fires the same Msg as
//! a click (Alt+F / Alt+A). The focus button is click-only. The diff show/hide toggle
//! moved to the View menu.

use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::layout::{
    FilesAction, FilesActionRect, FilesSearchRects, LayoutMap, FILES_FIELD_TEXT_W,
};
use super::widgets::{mnemonic_spans, pad, truncate};
use crate::theme::{Glyph, Theme};
use crate::view_state::ViewState;

/// Placeholder shown in an empty, unfocused files-search field.
const FILES_PLACEHOLDER: &str = "Filter files";

/// Draw the files-toolbar background and each control, using the rects already
/// computed in `lm`. Transient notices do NOT render here: this strip is fully
/// occupied by the search field + Flat/All/focus, so a notice lives on the log
/// filter toolbar's free margin instead (see `toolbar.rs`).
pub fn render(frame: &mut Frame, lm: &LayoutMap, view: &ViewState) {
    // On a short frame the layout collapses this strip to height 0 and parks it AT
    // the buffer edge (y == frame height); its action rects inherit that out-of-frame
    // origin. Bail before any render so a Paragraph never indexes off-buffer (ratatui
    // panics on a write whose origin lies outside the buffer).
    let strip = lm.files_toolbar;
    let fa = frame.area();
    if strip.height == 0 || strip.width == 0 || strip.x >= fa.right() || strip.y >= fa.bottom() {
        return;
    }
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(Theme::BG_TOOLBAR)),
        strip,
    );
    render_search(frame, &lm.files_search, view);
    for control in &lm.files_actions {
        render_control(frame, control, view);
    }
}

/// Draw the files-pane search field into its layout rects: lens, the live query (or
/// placeholder) padded to a fixed width with a caret while focused, the clear `x` (only
/// while non-empty), then the `.*` regex toggle. Mirrors the log search field so the two
/// read the same; the runtime maps clicks to the files-search messages.
fn render_search(frame: &mut Frame, r: &FilesSearchRects, view: &ViewState) {
    let field_bg = Style::default().bg(Theme::FIELD_BG);
    cell(frame, r.lens, format!(" {} ", Glyph::SEARCH), field_bg.fg(Theme::TEXT_DIM));
    cell(frame, r.field, files_query_text(view), field_bg.fg(query_fg(view)));
    if let Some(clear) = r.clear {
        cell(frame, clear, format!(" {} ", Glyph::SEARCH_CLEAR), field_bg.fg(Theme::ACCENT_CLOSE));
    }
    let regex_style = if view.files_regex_on {
        field_bg.fg(Theme::LINK).bold()
    } else {
        field_bg.fg(Theme::TEXT_DIM)
    };
    cell(frame, r.regex, " .* ".to_string(), regex_style);
}

/// The query text region, exactly `FILES_FIELD_TEXT_W` cells wide: the live query with a
/// block caret while focused, or a placeholder for an empty unfocused field.
fn files_query_text(view: &ViewState) -> String {
    let width = FILES_FIELD_TEXT_W as usize;
    if view.files_search.is_empty() && !view.files_search_active {
        return pad(&truncate(FILES_PLACEHOLDER, width), width);
    }
    let budget = if view.files_search_active { width.saturating_sub(1) } else { width };
    let mut text = truncate(&view.files_search, budget);
    if view.files_search_active {
        text.push('\u{2588}'); // block caret (single-width, in-font)
    }
    pad(&text, width)
}

/// Foreground for the query text: faint for the unfocused placeholder, else normal.
fn query_fg(view: &ViewState) -> ratatui::style::Color {
    if view.files_search.is_empty() && !view.files_search_active {
        Theme::TEXT_FAINT
    } else {
        Theme::TEXT
    }
}

/// Paint one search-field cell block (skips a zero-width clamped rect).
fn cell(frame: &mut Frame, rect: Rect, text: String, style: Style) {
    if rect.width == 0 {
        return;
    }
    frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), rect);
}

/// Draw one control. The Show-diff toggle gets an accent background + bright text
/// when the viewer is shown (a live state indicator); the action buttons read as
/// dim labels.
fn render_control(frame: &mut Frame, control: &FilesActionRect, view: &ViewState) {
    if control.rect.width == 0 {
        return;
    }
    let (label, mnemonic) = label_mnemonic(control.action);
    // The Flat and All toggles get the accent background + bright bold text when
    // active (a live on/off indicator); every other control reads as a dim label.
    let active = matches!(control.action, FilesAction::AllFiles if view.show_all_files)
        || matches!(control.action, FilesAction::Flat if view.files_flat);
    let style = if active {
        Style::default().bg(Theme::SELECTION_FOCUS).fg(Theme::TEXT).bold()
    } else {
        Style::default().bg(Theme::BG_TOOLBAR).fg(Theme::TEXT_DIM)
    };
    // ` <label> `: a leading/trailing pad span matches `files_toolbar_layout`'s
    // +2 width, with the accelerator char underlined (casing preserved) inside.
    let mut spans = vec![Span::styled(" ", style)];
    spans.extend(mnemonic_spans(label, mnemonic, style));
    spans.push(Span::styled(" ", style));
    frame.render_widget(Paragraph::new(Line::from(spans)), control.rect);
}

/// The visible label + mnemonic char index for a control. Single source shared
/// with [`super::layout`]'s `FILES_ACTIONS`, so the hit-test widths and the drawn
/// glyphs stay identical.
fn label_mnemonic(action: FilesAction) -> (&'static str, usize) {
    super::layout::FILES_ACTIONS
        .iter()
        .find(|(a, _, _)| *a == action)
        .map(|(_, label, m)| (*label, *m))
        .unwrap_or(("", 0))
}
