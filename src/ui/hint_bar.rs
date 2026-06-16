//! The bottom HINT BAR: a one-row, lazygit-style key strip. PURE render over the
//! geometry `layout` computed - the chips advance by the SAME label/key/separator
//! widths `layout::hint_chips` used, so a chip click lands on the verb drawn. The
//! items come from `layout::hint_items` (one source for layout, render, and the
//! keymap's truth: every shown key is live in the current input context).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::layout::{hint_editing, hint_items, LayoutMap, HINT_LEAD};
use crate::theme::Theme;
use crate::view_state::ViewState;

/// Draw the hint bar, if the frame spared its row. Labels dim, keys in the link
/// accent, separators faint - quiet enough to ignore, readable when needed.
pub fn render(frame: &mut Frame, lm: &LayoutMap, view: &ViewState) {
    let bar = lm.hint_bar;
    if bar.width == 0 || bar.height == 0 {
        return;
    }
    frame.render_widget(Block::default().style(Style::default().bg(Theme::BG_TOOLBAR)), bar);
    let mut spans = vec![Span::raw(" ".repeat(HINT_LEAD as usize))];
    for (i, (label, key, _)) in hint_items(hint_editing(view)).iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(Theme::TEXT_FAINT)));
        }
        spans.push(Span::styled(format!("{label}: "), Style::default().fg(Theme::TEXT_DIM)));
        spans.push(Span::styled((*key).to_string(), Style::default().fg(Theme::LINK)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Theme::BG_TOOLBAR)),
        Rect { ..bar },
    );
}
