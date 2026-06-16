//! Top toolbar: the live search field (with a `.*` regex match-mode toggle) and
//! the three filter dropdown labels. PURE: it renders the spans whose widths
//! [`super::layout`] mirrors for hit-testing; the runtime maps clicks to messages.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::layout::{filter_mnemonic, ToolbarLayout, FIELD_TEXT_W, TOGGLE_W, TOOLBAR_GAP};
use super::widgets::{mnemonic_spans, pad, truncate};
use crate::model::{filter_label, filter_name, Status};
use crate::theme::{Glyph, Theme};
use crate::view_state::{FilterKind, ViewState, FILTER_KINDS};

/// Placeholder shown in an empty, unfocused search field.
const PLACEHOLDER: &str = "Text or hash";

pub fn render(frame: &mut Frame, area: Rect, ui: &ToolbarLayout, view: &ViewState, status: &Status) {
    // Paint toolbar strip background, then the single left-aligned span line.
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(Theme::BG_TOOLBAR)),
        area,
    );
    frame.render_widget(
        Paragraph::new(toolbar_line(view))
            .style(Style::default().bg(Theme::BG_TOOLBAR))
            .alignment(Alignment::Left),
        area,
    );
    // A transient notice (Copy / commit / revert / push result) right-aligns in the free
    // margin AFTER the filter labels - never over the search field / Branch / User / Date
    // controls (the log toolbar is narrow + control-packed). The SINGLE notice home now
    // that the bottom action bar is gone.
    if let Status::Notice(text) = status {
        render_notice(frame, area, ui, text);
    }
}

/// Right-align a transient notice in the toolbar's free margin to the RIGHT of the last
/// filter label, so it never overdraws the search field or the Branch/User/Date controls.
/// A long notice truncates to that margin instead of covering them. Dim, trailing pad.
fn render_notice(frame: &mut Frame, area: Rect, ui: &ToolbarLayout, text: &str) {
    let content_end = ui.refresh_btn.right();
    let start = content_end.saturating_add(1).max(area.x);
    if start >= area.right() {
        return; // no free room past the filters this frame
    }
    let margin = Rect { x: start, width: area.right() - start, ..area };
    let max = margin.width.saturating_sub(1) as usize;
    let label = format!("{} ", truncate(text, max));
    frame.render_widget(
        Paragraph::new(Span::styled(
            label,
            Style::default().bg(Theme::BG_TOOLBAR).fg(Theme::TEXT_DIM),
        ))
        .alignment(Alignment::Right),
        margin,
    );
}

/// The full toolbar line: a leading space, the search field, then the filter
/// labels. Span widths match `layout::toolbar_layout` cell-for-cell.
fn toolbar_line(view: &ViewState) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(field_spans(view));
    for kind in FILTER_KINDS {
        spans.push(Span::raw(" ".repeat(TOOLBAR_GAP as usize)));
        spans.extend(filter_spans(kind, view));
    }
    // Refresh button (Update Project): the layout applies a trailing gap after the last filter
    // then places the button, so match it with a leading gap before the ` <refresh> ` block.
    spans.push(Span::raw(" ".repeat(TOOLBAR_GAP as usize)));
    spans.push(Span::styled(
        format!(" {} ", Glyph::MENU_UPDATE),
        Style::default().bg(Theme::BG_TOOLBAR).fg(Theme::LINK),
    ));
    Line::from(spans)
}

/// The inset search field: lens, the live query (or placeholder) padded to a
/// fixed width with a caret when focused, then the `.*` regex toggle.
fn field_spans(view: &ViewState) -> Vec<Span<'static>> {
    let field_bg = Style::default().bg(Theme::FIELD_BG);
    let mut spans = vec![
        Span::styled(format!(" {} ", Glyph::SEARCH), field_bg.fg(Theme::TEXT_DIM)),
        query_span(view, field_bg),
    ];
    // The clear `x` renders ONLY while the query is non-empty - the SAME gate
    // `layout::toolbar_layout` uses for its `FIELD_CLEAR_W` rect, so widths agree.
    if !view.search.is_empty() {
        spans.push(Span::styled(
            format!(" {} ", Glyph::SEARCH_CLEAR),
            field_bg.fg(Theme::ACCENT_CLOSE),
        ));
    }
    spans.push(toggle_span(".*", view.regex_on, field_bg));
    spans
}

/// The query text region, exactly `FIELD_TEXT_W` cells wide. Shows the live query
/// with a block caret while focused; an empty unfocused field shows a placeholder.
fn query_span(view: &ViewState, field_bg: Style) -> Span<'static> {
    let width = FIELD_TEXT_W as usize;
    if view.search.is_empty() && !view.search_active {
        let text = pad(&truncate(PLACEHOLDER, width), width);
        return Span::styled(text, field_bg.fg(Theme::TEXT_FAINT));
    }
    // Reserve one cell for the caret while focused so the text never overflows.
    let budget = if view.search_active { width.saturating_sub(1) } else { width };
    let mut text = truncate(&view.search, budget);
    if view.search_active {
        text.push('\u{2588}'); // block caret (single-width, in-font)
    }
    Span::styled(pad(&text, width), field_bg.fg(Theme::TEXT))
}

/// The `.*` match-mode toggle pill styled as ` xx ` (matches `TOGGLE_W`): accent
/// text when on, dim when off.
fn toggle_span(label: &str, on: bool, field_bg: Style) -> Span<'static> {
    debug_assert_eq!(TOGGLE_W as usize, label.chars().count() + 2);
    let style = if on {
        field_bg.fg(Theme::LINK).bold()
    } else {
        field_bg.fg(Theme::TEXT_DIM)
    };
    Span::styled(format!(" {label} "), style)
}

/// One filter label `<name|name: sel> <caret>`: accent when a selection is set,
/// dim ("All") otherwise. Only the leading keyword carries the mnemonic (its
/// accelerator char underlined, casing preserved; Alt+B/U/T opens the dropdown);
/// any `: <sel>` remainder is rendered verbatim so the selection text is untouched.
/// Width matches `layout::toolbar_layout` (label + 2: a space + the caret).
fn filter_spans(kind: FilterKind, view: &ViewState) -> Vec<Span<'static>> {
    let (text, active) = filter_label(kind, view);
    let style = if active {
        Style::default().fg(Theme::LINK).bg(Theme::BG_TOOLBAR)
    } else {
        Style::default().fg(Theme::TEXT_DIM).bg(Theme::BG_TOOLBAR)
    };
    // Mnemonic-ize only the leading keyword; keep the `: <sel>` tail verbatim.
    let keyword_len = filter_name(kind).chars().count();
    let mut spans = mnemonic_spans(&text[..keyword_len], filter_mnemonic(kind), style);
    if text.len() > keyword_len {
        spans.push(Span::styled(text[keyword_len..].to_string(), style));
    }
    spans.push(Span::styled(format!(" {}", Glyph::DROPDOWN), style));
    spans
}
