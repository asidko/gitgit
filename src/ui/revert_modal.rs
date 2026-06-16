//! Revert confirmation modal: a centered bordered box naming the commit + the
//! target file(s), warning that the revert OVERWRITES the working tree, with
//! `[Yes]`/`[No]` buttons. PURE: it reads `view.revert_confirm` and the geometry
//! from [`super::layout`]; the runtime routes the button clicks / keys. Drawn LAST
//! in `ui::view` so it overlays everything, including the dropdown.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::layout::{LayoutMap, RevertModalLayout, MODAL_MAX_LISTED};
use super::widgets::truncate;
use crate::theme::Theme;
use crate::view_state::{RevertRequest, ViewState};

/// Draw the revert modal, if one is open. No-op otherwise.
pub fn render(frame: &mut Frame, lm: &LayoutMap, view: &ViewState) {
    let (ml, req) = match (&lm.revert_modal, &view.revert_confirm) {
        (Some(ml), Some(req)) => (ml, req),
        _ => return,
    };
    render_modal(frame, ml, req);
}

/// Render the bordered frame, the prompt lines, and the two buttons.
fn render_modal(frame: &mut Frame, ml: &RevertModalLayout, req: &RevertRequest) {
    // Clear underlying cells so panels do not bleed through the box.
    frame.render_widget(Clear, ml.frame);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Revert Selected Changes")
        .style(Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT))
        .border_style(Style::default().fg(Theme::ACCENT_CLOSE));
    let inner = block.inner(ml.frame);
    frame.render_widget(block, ml.frame);

    render_body(frame, inner, req);
    render_buttons(frame, ml);
}

/// Paint the heading, the listed paths (+ "and K more"), and the warning line.
fn render_body(frame: &mut Frame, inner: Rect, req: &RevertRequest) {
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    let n = req.paths.len();
    let noun = if n == 1 { "file" } else { "files" };
    let commit = truncate(&req.commit_label, w.saturating_sub(20));
    lines.push(Line::from(Span::styled(
        format!("Revert {n} {noun} to before {}:", commit),
        Style::default().fg(Theme::TEXT),
    )));
    lines.push(Line::from(""));

    for path in req.paths.iter().take(MODAL_MAX_LISTED) {
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(path, w.saturating_sub(2))),
            Style::default().fg(Theme::LINK),
        )));
    }
    if n > MODAL_MAX_LISTED {
        lines.push(Line::from(Span::styled(
            format!("  and {} more", n - MODAL_MAX_LISTED),
            Style::default().fg(Theme::TEXT_DIM),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "This OVERWRITES the working tree and cannot be undone.",
        Style::default().fg(Theme::ACCENT_CLOSE),
    )));

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(Theme::FIELD_BG)),
        inner,
    );
}

/// Draw the `[Yes]` (destructive accent) and `[No]` (neutral) buttons into the
/// exact hit-test rects the layout exposes.
fn render_buttons(frame: &mut Frame, ml: &RevertModalLayout) {
    let yes = Span::styled(
        "[Yes]",
        Style::default().bg(Theme::ACCENT_CLOSE).fg(Theme::TEXT).bold(),
    );
    let no = Span::styled(
        "[No]",
        Style::default().bg(Theme::SELECTION_FOCUS).fg(Theme::TEXT),
    );
    frame.render_widget(
        Paragraph::new(Line::from(yes)).alignment(Alignment::Left),
        ml.yes,
    );
    frame.render_widget(
        Paragraph::new(Line::from(no)).alignment(Alignment::Left),
        ml.no,
    );
}
