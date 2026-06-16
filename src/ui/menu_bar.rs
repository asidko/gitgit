//! Top menu bar: clickable `Editor` / `View` menu labels + a right-aligned close
//! button, plus the open menu's popup.
//!
//! Renders into the exact menu / close / popup rects from [`super::layout`], so the
//! hit-test geometry and the drawn glyphs always agree. PURE: the runtime maps menu
//! clicks and item picks to messages; this module only draws. The popup is drawn
//! LAST in `ui::view` (with the filter dropdown) so it overlays the panels.

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use super::layout::{LayoutMap, MenuDropdownLayout, MenuRect};
use crate::theme::{Glyph, Theme};
use crate::view_state::{
    menu_icon, menu_rows, CommitMenuAction, CommitRow, MenuId, MenuRow,
    RefMenuKind, ViewState,
};

/// The leading icon for a commit context-menu action, or `None` for the icon-less
/// majority. Only the ref-creating pair (Branch/Tag share ONE glyph, reading as "make a
/// ref here") and Cherry-Pick carry an icon; the rest stay text so the menu is not an
/// indistinguishable wall of glyphs. The layout still reserves the icon column so every
/// label aligns whether or not its row has a glyph. The one home of the icon mapping.
pub fn commit_menu_icon(action: CommitMenuAction) -> Option<&'static str> {
    match action {
        CommitMenuAction::NewBranch | CommitMenuAction::NewTag => Some(Glyph::MENU_BRANCH),
        CommitMenuAction::CherryPick => Some(Glyph::MENU_CHERRY),
        _ => None,
    }
}

/// Draw the menu bar background, the menu labels (the open one highlighted), and
/// the close circle, using the rects already computed in `lm`.
pub fn render(frame: &mut Frame, lm: &LayoutMap, view: &ViewState) {
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(Theme::BG_TOOLBAR)),
        lm.menu_bar,
    );

    for menu in &lm.menus {
        render_menu(frame, menu, view);
    }

    // Right-aligned close button: a red filled circle (verified to render).
    let close = Line::from(Span::styled(
        format!(" {} ", Glyph::CLOSE),
        Style::default().bg(Theme::BG_TOOLBAR).fg(Theme::ACCENT_CLOSE),
    ));
    frame.render_widget(Paragraph::new(close), lm.close_btn);
}

/// Draw one menu label as ` <label> `. The open menu reads with the accent
/// background (so it is clear which popup is showing); the others are plain.
fn render_menu(frame: &mut Frame, menu: &MenuRect, view: &ViewState) {
    if menu.rect.width == 0 {
        return;
    }
    let open = view.open_menu == Some(menu.id);
    let style = if open {
        Style::default().bg(Theme::SELECTION_FOCUS).fg(Theme::TEXT).bold()
    } else {
        Style::default().bg(Theme::BG_TOOLBAR).fg(Theme::TEXT)
    };
    let label = format!(" {} ", menu_label(menu.id));
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(label, style))),
        menu.rect,
    );
}

/// Draw the open menu popup, if any. No-op when no menu is open. Drawn alongside
/// the filter dropdown so it overlays the panels.
pub fn render_dropdown(frame: &mut Frame, lm: &LayoutMap, view: &ViewState) {
    let dl = match &lm.menu_dropdown {
        Some(dl) => dl,
        None => return,
    };
    render_popup(frame, dl, view);
}

/// Render the bordered popup frame and its item rows. Each item that is currently
/// "on" (`menu_action_active`) gets the accent background + bright text - the same
/// on/off visual the toggle pills used before they moved into the menus.
fn render_popup(frame: &mut Frame, dl: &MenuDropdownLayout, view: &ViewState) {
    let inner_w = dl.frame.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = menu_rows(dl.id)
        .iter()
        .map(|row| match row {
            MenuRow::Action(action, label) => {
                let style = if !view.menu_action_enabled(*action) {
                    // A disabled action (e.g. Undo with empty history) reads dimmed.
                    Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT_DIM)
                } else if view.menu_action_active(*action) {
                    Style::default().bg(Theme::SELECTION_FOCUS).fg(Theme::TEXT).bold()
                } else {
                    Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT)
                };
                // Reserve the icon cell even when iconless (a space) so labels align.
                let icon = menu_icon(*action).unwrap_or(" ");
                ListItem::new(Line::from(Span::styled(format!(" {icon} {label}"), style)))
            }
            // A thin BORDER-colored rule filling the inner width (a group separator).
            MenuRow::Sep => ListItem::new(Line::from(Span::styled(
                Glyph::MENU_SEP.repeat(inner_w),
                Style::default().bg(Theme::FIELD_BG).fg(Theme::BORDER),
            ))),
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(menu_label(dl.id))
        .style(Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT))
        .border_style(Style::default().fg(Theme::BORDER));

    // Clear the cells under the popup so panels do not bleed through.
    frame.render_widget(Clear, dl.frame);
    frame.render_widget(List::new(items).block(block), dl.frame);
}

/// The icon for a ref submenu's PARENT row, encoding locality like the log decorations:
/// a local branch is the hollow diamond, a remote branch the filled one, a tag the diamond
/// (tags share the branch diamond family; the "Tag '<name>'" label carries the distinction).
fn ref_kind_icon(kind: RefMenuKind) -> &'static str {
    match kind {
        RefMenuKind::LocalBranch => Glyph::MENU_BRANCH,
        RefMenuKind::RemoteBranch => Glyph::MENU_BRANCH_REMOTE,
        RefMenuKind::Tag => Glyph::MENU_TAG,
    }
}

/// Draw the open commit context menu popup, if any. No-op when none is open. Drawn
/// alongside the other popups so it overlays the panels. The fixed leaf actions are always
/// available; a commit's branch/tag refs append fly-out rows (a trailing `>`), and the open
/// fly-out is painted last so it overlays the parent.
pub fn render_commit_menu(frame: &mut Frame, lm: &LayoutMap, view: &ViewState) {
    let (dl, menu) = match (&lm.commit_menu, &view.commit_menu) {
        (Some(dl), Some(menu)) => (dl, menu),
        _ => return,
    };
    let leaves = menu.parent_rows();
    let n_leaves = leaves.len();
    let inner_w = dl.frame.width.saturating_sub(2) as usize;
    // Window the parent rows at the layout's clamped scroll so row j shows absolute item
    // scroll+j (matching the hit-test); a menu taller than the terminal scrolls instead of
    // silently clipping its bottom actions. A leaf renders its icon+label; a ref row renders
    // its kind icon + "Branch '<name>'" with a right-aligned `>` submenu marker.
    let ref_base = menu.ref_base();
    // A thin BORDER-colored rule filling the inner width (a group separator).
    let sep_item = || {
        ListItem::new(Line::from(Span::styled(
            Glyph::MENU_SEP.repeat(inner_w),
            Style::default().bg(Theme::FIELD_BG).fg(Theme::BORDER),
        )))
    };
    let items: Vec<ListItem> = (0..dl.items.len())
        .map(|j| {
            let abs = dl.scroll + j;
            if abs < n_leaves {
                match leaves[abs] {
                    CommitRow::Action(action, label) => {
                        // Reserve the icon cell even when iconless (a space) so labels align.
                        let icon = commit_menu_icon(action).unwrap_or(" ");
                        let style = Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT);
                        ListItem::new(Line::from(Span::styled(format!(" {icon} {label}"), style)))
                    }
                    CommitRow::Sep => sep_item(),
                }
            } else if abs < ref_base {
                // The separator fencing the ref fly-outs off from the rewrite-history group.
                sep_item()
            } else if let Some(rm) = menu.refs.get(abs - ref_base) {
                // `.get()` over raw indexing: layout + render share this frame's menu so the
                // index is in bounds, but a render-path panic would crash the TUI - cheap guard.
                let icon = ref_kind_icon(rm.kind);
                let base = format!(" {icon} {}", rm.label());
                let pad = inner_w.saturating_sub(base.chars().count() + 1);
                let text = format!("{base}{}>", " ".repeat(pad));
                // Highlight the row whose fly-out is open so the active submenu is obvious.
                let bg = if menu.open_ref == Some(abs - ref_base) {
                    Theme::SELECTION_FOCUS
                } else {
                    Theme::FIELD_BG
                };
                ListItem::new(Line::from(Span::styled(text, Style::default().bg(bg).fg(Theme::TEXT))))
            } else {
                ListItem::new(Line::default())
            }
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT))
        .border_style(Style::default().fg(Theme::BORDER));

    frame.render_widget(Clear, dl.frame);
    frame.render_widget(List::new(items).block(block), dl.frame);

    // The open branch/tag fly-out, painted last so it overlays the parent menu. `.get()`
    // guards the same in-bounds invariant the layout already enforces (it only emits a
    // submenu for an existing ref) without risking a render-path panic.
    if let Some((sub, rm)) =
        dl.submenu.as_ref().and_then(|s| menu.refs.get(s.ref_idx).map(|rm| (s, rm)))
    {
        let current = menu.current_branch.as_deref().unwrap_or("HEAD");
        let rows: Vec<ListItem> = rm
            .actions
            .iter()
            .map(|a| {
                // No leading icon in the fly-out: a column of glyphs there reads as spam (the
                // action label alone is clear). The parent ref row keeps its kind diamond.
                let label = a.label(&rm.name, current);
                ListItem::new(Line::from(Span::styled(
                    format!(" {label}"),
                    Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT),
                )))
            })
            .collect();
        let sub_block = Block::default()
            .borders(Borders::ALL)
            .style(Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT))
            .border_style(Style::default().fg(Theme::BORDER));
        frame.render_widget(Clear, sub.frame);
        frame.render_widget(List::new(rows).block(sub_block), sub.frame);
    }
}

/// Draw the open files-pane context menu popup, if any. No-op when none is open. Drawn
/// alongside the other popups so it overlays the panels. The rows are windowed at the
/// layout's clamped `dl.scroll` (like the commit menu) so a menu taller than the terminal
/// scrolls instead of clipping its bottom actions; it has no fly-outs.
pub fn render_files_menu(frame: &mut Frame, lm: &LayoutMap, view: &ViewState) {
    let (dl, menu) = match (&lm.files_menu, &view.files_menu) {
        (Some(dl), Some(menu)) => (dl, menu),
        _ => return,
    };
    let inner_w = dl.frame.width.saturating_sub(2) as usize;
    let rows = menu.rows();
    // Window the rows at the layout's clamped scroll so visible row j shows absolute row
    // scroll+j (matching the hit-test); a menu taller than the terminal scrolls instead of
    // silently clipping its bottom (destructive) actions.
    let items: Vec<ListItem> = (0..dl.items.len())
        .filter_map(|j| rows.get(dl.scroll + j))
        .map(|row| match row {
            crate::view_state::FilesRow::Action(a) => ListItem::new(Line::from(Span::styled(
                format!(" {}", a.label()),
                Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT),
            ))),
            // A thin BORDER-colored rule filling the inner width (an intent-group separator).
            crate::view_state::FilesRow::Sep => ListItem::new(Line::from(Span::styled(
                Glyph::MENU_SEP.repeat(inner_w),
                Style::default().bg(Theme::FIELD_BG).fg(Theme::BORDER),
            ))),
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT))
        .border_style(Style::default().fg(Theme::BORDER));
    frame.render_widget(Clear, dl.frame);
    frame.render_widget(List::new(items).block(block), dl.frame);
}

/// The label for a menu id, from the single `MENUS` source.
fn menu_label(id: MenuId) -> &'static str {
    crate::view_state::MENUS
        .iter()
        .find(|(m, _)| *m == id)
        .map(|(_, l)| *l)
        .unwrap_or("")
}
