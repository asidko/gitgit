//! Filter dropdown popup: a small bordered option list overlaid under the open
//! filter label. PURE: it reads the open dropdown's geometry from
//! [`super::layout`] and the options from the model; the runtime maps row clicks
//! and Up/Down/Enter/Esc to messages. Drawn LAST in `ui::view` so it overlays the
//! panels.

use ratatui::style::{Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState};
use ratatui::Frame;

use super::layout::{DropdownLayout, LayoutMap};
use crate::model::{filter_name, filter_options, RepoModel};
use crate::theme::Theme;
use crate::view_state::ViewState;

/// Draw the open filter dropdown, if any. No-op when no dropdown is open.
pub fn render(frame: &mut Frame, lm: &LayoutMap, repo: &RepoModel, view: &ViewState) {
    let dl = match &lm.dropdown {
        Some(dl) => dl,
        None => return,
    };
    render_popup(frame, dl, repo, view);
}

/// Render the bordered popup frame and its highlighted option rows.
fn render_popup(frame: &mut Frame, dl: &DropdownLayout, repo: &RepoModel, view: &ViewState) {
    let options = filter_options(repo, dl.kind);
    let items: Vec<ListItem> = options
        .iter()
        .map(|o| ListItem::new(Line::from(format!(" {o}"))))
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(filter_name(dl.kind))
        .style(Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT))
        .border_style(Style::default().fg(Theme::BORDER));

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Theme::SELECTION_FOCUS).fg(Theme::TEXT).bold())
        .highlight_spacing(HighlightSpacing::Never);

    // Window the (possibly long, capped) list at the layout's scroll offset so the drawn
    // rows line up with the hit-test rects; the selection stays highlighted within it.
    let sel = view.dropdown_sel.min(options.len().saturating_sub(1));
    let mut state = ListState::default().with_offset(dl.scroll).with_selected(Some(sel));

    // Clear the cells under the popup so panels do not bleed through.
    frame.render_widget(Clear, dl.frame);
    frame.render_stateful_widget(list, dl.frame, &mut state);
}
