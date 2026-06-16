//! Recent-search history popup: a small bordered list of past queries overlaid under
//! the search lens icon. PURE: it reads the popup geometry from [`super::layout`] and
//! the entries from the view; the runtime maps a row click to a pick message (and a
//! click-away / Esc to close). Drawn over the panels, under the modal.

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem};
use ratatui::Frame;

use super::layout::LayoutMap;
use super::widgets::truncate;
use crate::theme::Theme;
use crate::view_state::ViewState;

/// Draw the recent-search history popup, if open. No-op otherwise.
pub fn render(frame: &mut Frame, lm: &LayoutMap, view: &ViewState) {
    let Some(layout) = &lm.search_history else {
        return;
    };
    let inner_w = layout.frame.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = view
        .search_history
        .iter()
        .take(layout.options.len())
        .map(|q| ListItem::new(Line::from(format!(" {}", truncate(q, inner_w.saturating_sub(1))))))
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Recent")
        .style(Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT))
        .border_style(Style::default().fg(Theme::BORDER));
    // A mouse-picked list: no persistent selection highlight (the click chooses).
    let list = List::new(items).block(block);

    frame.render_widget(Clear, layout.frame);
    frame.render_widget(list, layout.frame);
}
