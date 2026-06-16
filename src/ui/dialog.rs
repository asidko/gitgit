//! Action-bar dialogs: a centered bordered box for the commit/amend/tag text input,
//! the push/pull confirmation, and the copy-field picker. PURE: it renders into the
//! geometry [`super::layout::DialogLayout`] computed (shared with the mouse hit-test),
//! so a click lands on the control it drew. Both keyboard and mouse drive it (the copy
//! rows + the [confirm]/[cancel] buttons are clickable; everything behind it is
//! swallowed). Drawn LAST in `ui::view` so it overlays everything.

use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::layout::{dialog_button_labels, LayoutMap};
use crate::theme::Theme;
use crate::view_state::{
    choice_title, Dialog, InputKind, PickItem, RebaseAction, RebaseStep, TextField, ViewState,
    COPY_FIELDS,
};

/// Draw the open dialog, if any, into the layout's geometry. No-op otherwise.
pub fn render(frame: &mut Frame, lm: &LayoutMap, view: &ViewState) {
    let (Some(dialog), Some(dl)) = (view.dialog.as_ref(), lm.dialog.as_ref()) else {
        return;
    };
    if dl.frame.width == 0 || dl.frame.height == 0 {
        return;
    }
    let (title, body, accent) = match dialog {
        Dialog::Input { kind, field, note, checkbox, .. } => {
            (input_title(*kind), input_body(*kind, field, note.as_deref(), checkbox.as_ref()), Theme::LINK)
        }
        Dialog::Confirm { prompt, .. } => ("Confirm", confirm_body(prompt), Theme::ACCENT_CLOSE),
        Dialog::Copy { sel, fields } => ("Copy", copy_body(*sel, fields), Theme::LINK),
        // Reset is destructive (red close-accent like Confirm); the Pull strategy picker is a
        // normal update, so it wears the blue link-accent.
        Dialog::Choice { kind, sel, note, .. } => {
            use crate::view_state::ChoiceKind;
            let accent = if *kind == ChoiceKind::PullStrategy {
                Theme::LINK
            } else {
                Theme::ACCENT_CLOSE
            };
            (choice_title(*kind), choice_body(*kind, *sel, note.as_deref()), accent)
        }
        // Rebase is a destructive history rewrite -> red accent. The rows window at the
        // layout's `scroll` (visible count = `dl.rows.len()`) so render + hit-test agree.
        Dialog::Rebase { steps, sel, note, .. } => (
            "Interactively rebase from here",
            rebase_body(steps, *sel, note.as_deref(), dl.scroll, dl.rows.len()),
            Theme::ACCENT_CLOSE,
        ),
        // The compare picker is a read-only viewer (not destructive) -> the blue link accent.
        // Rows window at the layout's `scroll`/`dl.rows.len()` so render + hit-test agree.
        Dialog::Picker { title, items, sel, .. } => (
            title.as_str(),
            picker_body(items, *sel, dl.scroll, dl.rows.len()),
            Theme::LINK,
        ),
        Dialog::Remotes { remotes, sel } => (
            "Manage remotes",
            remotes_body(remotes, *sel, dl.scroll, dl.rows.len()),
            Theme::LINK,
        ),
        // Checkout is safe (blue); merge/rebase mutate history -> red close-accent.
        Dialog::RefPick { items, sel, op } => (
            op.title(),
            picker_body(items, *sel, dl.scroll, dl.rows.len()),
            if *op == crate::view_state::RefOp::Checkout { Theme::LINK } else { Theme::ACCENT_CLOSE },
        ),
        Dialog::Help => ("Keybindings", help_body(), Theme::LINK),
    };
    frame.render_widget(Clear, dl.frame);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().bg(Theme::FIELD_BG).fg(Theme::TEXT))
        .border_style(Style::default().fg(accent));
    let inner = block.inner(dl.frame);
    frame.render_widget(block, dl.frame);
    // Layout: content rows at the top, then one dim keyboard-hint row, then the button
    // row (all sized by `dialog_layout`). The buttons are clickable; the hint keeps the
    // keyboard affordances (row navigation / confirm / cancel) discoverable.
    let rows = body.len() as u16;
    let content = Rect { height: rows.min(inner.height), ..inner };
    frame.render_widget(
        Paragraph::new(body).style(Style::default().bg(Theme::FIELD_BG)),
        content,
    );
    if rows < inner.height {
        let hint_rect = Rect { y: inner.y + rows, height: 1, ..inner };
        frame.render_widget(
            Paragraph::new(hint(dialog_hint(dialog))).style(Style::default().bg(Theme::FIELD_BG)),
            hint_rect,
        );
    }
    // Filled buttons matching the revert modal's convention: the confirm button carries
    // the dialog's accent (blue input/copy, red push/pull), cancel a neutral selection bg.
    let (confirm_lbl, cancel_lbl) = dialog_button_labels(dialog);
    render_button(frame, dl.confirm, confirm_lbl, accent);
    render_button(frame, dl.cancel, cancel_lbl, Theme::SELECTION_FOCUS);
}

/// Paint one filled dialog button (label on a `bg` band), mirroring the revert modal's
/// buttons. The whole rect is the click target the layout computed, so the painted button
/// and the hit-test agree cell-for-cell.
fn render_button(frame: &mut Frame, rect: Rect, label: &str, bg: ratatui::style::Color) {
    if rect.width == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label.to_string(),
            Style::default().bg(bg).fg(Theme::TEXT).bold(),
        ))),
        rect,
    );
}

/// The dim keyboard-hint line for `dialog`: the copy picker keeps its row-navigation
/// hint (the clickable rows + [Copy] button do not convey Up/Down), and the input /
/// confirm dialogs keep their confirm/cancel keys alongside the buttons.
fn dialog_hint(dialog: &Dialog) -> &'static str {
    match dialog {
        Dialog::Input { .. } => "Enter confirm   Esc cancel",
        Dialog::Confirm { .. } => "Y confirm   N / Esc cancel",
        Dialog::Copy { .. } => "Up/Down choose   Enter copy   Esc cancel",
        Dialog::Choice { kind: crate::view_state::ChoiceKind::PullStrategy, .. } => "Up/Down choose   Enter pull   Esc cancel",
        Dialog::Choice { .. } => "Up/Down choose   Enter reset   Esc cancel",
        Dialog::Rebase { .. } => "p pick  s squash  f fixup  d drop   Enter / Esc",
        Dialog::Picker { mode: crate::view_state::InspectMode::CommitDiff, .. } => "Up/Down choose   Enter show   Esc cancel",
        Dialog::Picker { .. } => "Up/Down choose   Enter compare   Esc cancel",
        Dialog::Remotes { .. } => "a add   e edit URL   d remove   Esc close",
        Dialog::Help => "Enter / Esc close",
        Dialog::RefPick { .. } => "Up/Down choose   Enter select   Esc cancel",
    }
}

/// The input dialog's title by kind.
fn input_title(kind: InputKind) -> &'static str {
    match kind {
        InputKind::Commit => "Commit message",
        InputKind::Amend => "Amend message",
        InputKind::Tag => "Tag name",
        InputKind::NewBranch => "New branch name",
        InputKind::NewTag => "New tag name",
        InputKind::Reword => "Edit commit message",
        InputKind::CreatePatch => "Patch file path",
        InputKind::CreateWorkingPatch => "Patch file path (local changes)",
        InputKind::CommitFile => "Commit message (this file)",
        InputKind::CommitFolder => "Commit message (this folder)",
        InputKind::CommitSelected => "Commit message (selected files)",
        InputKind::CreatePatchSelected => "Patch file path (selected files)",
        InputKind::CreatePatchSeries => "Patch series directory (selected commits)",
        InputKind::RenameBranch => "Rename branch",
        InputKind::ArchiveProject => "Archive path (.zip / .tar.gz / .tar)",
        InputKind::RemoteAdd => "Add remote (name url)",
        InputKind::RemoteSetUrl => "Remote URL",
        InputKind::CreatePatchAll => "Create patch (working tree) - file path",
        InputKind::ApplyPatch => "Apply patch - file path",
    }
}

/// Input body: the editable line (caret reversed, selection banded), an optional dim
/// note, an optional checkbox, then the key hint.
fn input_body(
    kind: InputKind,
    field: &TextField,
    note: Option<&str>,
    checkbox: Option<&(String, bool)>,
) -> Vec<Line<'static>> {
    let mut lines = vec![field_line(field)];
    if let Some(text) = note {
        lines.push(Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(Theme::ACCENT_CLOSE),
        )));
    }
    if let Some((label, on)) = checkbox {
        let box_ = if *on { "[x] " } else { "[ ] " };
        lines.push(Line::from(Span::styled(
            format!("{box_}{label}  (Tab)"),
            Style::default().fg(Theme::TEXT),
        )));
    }
    // The archive dialog carries a FORMAT chip row: the chip whose extension matches the
    // filename is highlighted; Tab cycles them (rewriting the extension). The active format is
    // derived from the field, so it always agrees with the path that gets archived.
    if kind == InputKind::ArchiveProject {
        lines.push(archive_format_line(field.text()));
    }
    lines
}

/// The archive-format chip row: `Format (Tab):  zip  tar.gz  tar`, the chip matching `path`'s
/// extension reversed (active), the rest dim. Mirrors the backend's extension -> format map.
fn archive_format_line(path: &str) -> Line<'static> {
    use crate::view_state::ArchiveFormat;
    let active = ArchiveFormat::ALL
        .iter()
        .copied()
        .find(|f| path.ends_with(&format!(".{}", f.ext())))
        .unwrap_or(ArchiveFormat::Zip);
    let mut spans = vec![Span::styled("Format (Tab): ", Style::default().fg(Theme::TEXT_DIM))];
    for f in ArchiveFormat::ALL {
        let style = if f == active {
            Style::default().bg(Theme::LINK).fg(Theme::FIELD_BG).bold()
        } else {
            Style::default().fg(Theme::TEXT_DIM)
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!(" {} ", f.label()), style));
    }
    Line::from(spans)
}

/// Render the editable single line: each char styled, the caret char reversed, the
/// selection span banded; a trailing caret block when the caret sits past the end.
fn field_line(field: &TextField) -> Line<'static> {
    let chars: Vec<char> = field.text().chars().collect();
    let caret = field.caret();
    let sel = field.selection();
    let mut spans = Vec::with_capacity(chars.len() + 1);
    for (i, &c) in chars.iter().enumerate() {
        let style = if i == caret {
            Style::default().bg(Theme::TEXT).fg(Theme::FIELD_BG)
        } else if sel.is_some_and(|(s, e)| i >= s && i < e) {
            Style::default().bg(Theme::SELECTION_FOCUS).fg(Theme::TEXT)
        } else {
            Style::default().fg(Theme::TEXT)
        };
        spans.push(Span::styled(c.to_string(), style));
    }
    if caret >= chars.len() {
        spans.push(Span::styled(" ", Style::default().bg(Theme::TEXT).fg(Theme::FIELD_BG)));
    }
    Line::from(spans)
}

/// Confirm body: the prompt (the y/n action is the clickable [Yes]/[No] button row).
fn confirm_body(prompt: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(prompt.to_string(), Style::default().fg(Theme::TEXT)))]
}

/// The `?` keybindings popup, one row per `(category, keys)` pair. The row count is
/// the single source `layout::dialog_content_rows` sizes the frame by.
pub const HELP_LINES: [(&str, &str); 7] = [
    ("Repo", "c commit   p pull   P push   S stash"),
    ("Log", "space mark   shift+up/down range   y copy"),
    ("Files", "space mark   shift+click range   alt+r revert"),
    ("View", "d diff   s split   w wrap   W whitespace"),
    ("Filters", "/ search   b branch   u user   t date"),
    ("Editor", "ctrl+s save   ctrl+z/y undo/redo   esc leave"),
    ("App", "tab panes   q quit   F10 exit   ? this help"),
];

/// Column the help key text aligns to (longest category + pad).
const HELP_CAT_W: usize = 9;

fn help_body() -> Vec<Line<'static>> {
    HELP_LINES
        .iter()
        .map(|(cat, keys)| {
            Line::from(vec![
                Span::styled(format!("{cat:<HELP_CAT_W$}"), Style::default().fg(Theme::TEXT_DIM)),
                Span::styled((*keys).to_string(), Style::default().fg(Theme::TEXT)),
            ])
        })
        .collect()
}

/// Longest [`COPY_FIELDS`] label ("Short hash"), the column the dim previews align to.
const LABEL_W: usize = 12;
/// Max chars of a field's value shown as a dim preview before it is trimmed with "...".
const PREVIEW_MAX: usize = 40;

/// Copy body: each field as `label  <dim trimmed value>`, the selected row highlighted
/// (its band extends across the preview), then the hint. `fields` is parallel to
/// [`COPY_FIELDS`] and carries the full text snapshotted at open time.
fn copy_body(sel: usize, fields: &[String; 4]) -> Vec<Line<'static>> {
    COPY_FIELDS
        .iter()
        .enumerate()
        .map(|(i, label)| picker_row(label, preview(&fields[i]), i == sel))
        .collect()
}

/// One picker row: `> Label   <trailing>`, the selected row banded (label bold) with the
/// trailing text kept at full TEXT (dim grey on the selection blue is too low-contrast),
/// an unselected row's trailing dim. Shared by the Copy field picker and the Choice mode
/// picker so their row look stays identical.
fn picker_row(label: &str, trailing: String, selected: bool) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let (label_style, trail_style) = if selected {
        (
            Style::default().bg(Theme::SELECTION_FOCUS).fg(Theme::TEXT).bold(),
            Style::default().bg(Theme::SELECTION_FOCUS).fg(Theme::TEXT),
        )
    } else {
        (Style::default().fg(Theme::TEXT), Style::default().fg(Theme::TEXT_DIM))
    };
    Line::from(vec![
        Span::styled(format!("{marker}{label:<LABEL_W$}"), label_style),
        Span::styled(trailing, trail_style),
    ])
}

/// Choice body: one row per [`choice_options`] entry as `> Label  <dim description>`,
/// the selected row banded; then an optional dim-red warning note BELOW the options (so
/// the option-row indices stay 0..N for the hit-test). Shares the Copy picker's row look.
fn choice_body(
    kind: crate::view_state::ChoiceKind,
    sel: usize,
    note: Option<&str>,
) -> Vec<Line<'static>> {
    let options = crate::view_state::choice_options(kind);
    let mut lines: Vec<Line> = options
        .iter()
        .enumerate()
        .map(|(i, (label, desc))| picker_row(label, (*desc).to_string(), i == sel))
        .collect();
    if let Some(text) = note {
        lines.push(Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(Theme::ACCENT_CLOSE),
        )));
    }
    lines
}

/// Rebase body: the windowed step rows (`steps[scroll..][..visible]`, one per commit) with
/// the focused row banded and Drop rows red+dimmed, then an optional dim-red warning note
/// BELOW the rows (so a visible row's index stays `0..visible` for the hit-test).
fn rebase_body(
    steps: &[RebaseStep],
    sel: usize,
    note: Option<&str>,
    scroll: usize,
    visible: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = steps
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .map(|(i, step)| rebase_row(step, i == sel, meld_target(steps, i)))
        .collect();
    if let Some(text) = note {
        lines.push(Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(Theme::ACCENT_CLOSE),
        )));
    }
    lines
}

/// Compare-picker body: the windowed option rows (`items[scroll..][..visible]`), the focused
/// row banded. Each row is its label (a revision's `hash  date  subject`, or a ref name); a long
/// label clips at the dialog edge. The `sel` row reads against the selection band.
fn picker_body(items: &[PickItem], sel: usize, scroll: usize, visible: usize) -> Vec<Line<'static>> {
    items
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .map(|(i, item)| {
            let focused = i == sel;
            let marker = if focused { "> " } else { "  " };
            let style = if focused {
                Style::default().fg(Theme::TEXT).bg(Theme::SELECTION_FOCUS)
            } else {
                Style::default().fg(Theme::TEXT)
            };
            Line::from(Span::styled(format!("{marker}{}", item.label), style))
        })
        .collect()
}

/// Remotes body: the windowed remote rows (`remotes[scroll..][..visible]`) as `name  <dim url>`,
/// the focused row banded. An empty list draws one dim "(no remotes - press a to add)" placeholder
/// (the layout reserves a single row for it), so the dialog never renders an empty body.
fn remotes_body(
    remotes: &[(String, String)],
    sel: usize,
    scroll: usize,
    visible: usize,
) -> Vec<Line<'static>> {
    if remotes.is_empty() {
        return vec![Line::from(Span::styled(
            "  (no remotes - press a to add)".to_string(),
            Style::default().fg(Theme::TEXT_DIM),
        ))];
    }
    remotes
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .map(|(i, (name, url))| picker_row(name, preview(url), i == sel))
        .collect()
}

/// The short hash a meld row at display index `i` folds into: the nearest OLDER kept
/// (non-Drop) commit, which in the newest-first list is the next non-Drop row AFTER `i`.
/// `None` when nothing older survives (an invalid squash/fixup - the row flags it red).
fn meld_target(steps: &[RebaseStep], i: usize) -> Option<&str> {
    steps[i + 1..].iter().find(|s| s.action != RebaseAction::Drop).map(|s| s.short.as_str())
}

/// One rebase row: `> <verb> <short> [-> <target>] <subject>`. The focused row is banded;
/// the verb is colored by kind - Drop red (close-accent), Squash/Fixup blue (a meld), Pick
/// plain. A Squash/Fixup row shows which OLDER commit it melds into (`-> <target>`, dim), or
/// flags the dead-end in red (`-> (none)`) when nothing older survives. A Drop dims its
/// subject (struck-through in spirit). `meld_into` is only read for a meld verb.
fn rebase_row(step: &RebaseStep, focused: bool, meld_into: Option<&str>) -> Line<'static> {
    let dropped = step.action == RebaseAction::Drop;
    let verb_color = match step.action {
        RebaseAction::Drop => Theme::ACCENT_CLOSE,
        RebaseAction::Squash | RebaseAction::Fixup => Theme::LINK,
        RebaseAction::Pick => Theme::TEXT,
    };
    let marker = if focused { "> " } else { "  " };
    let band = |s: Style| if focused { s.bg(Theme::SELECTION_FOCUS) } else { s };
    let verb = band(Style::default().fg(verb_color));
    let hash = band(Style::default().fg(Theme::TEXT_DIM));
    let subject = band(Style::default().fg(if dropped { Theme::TEXT_DIM } else { Theme::TEXT }));
    let mut spans = vec![
        Span::styled(format!("{marker}{:<6} ", step.action.label()), verb),
        Span::styled(format!("{} ", step.short), hash),
    ];
    // A meld verb shows its absorbing (older) commit up front so the direction is never
    // ambiguous in a newest-first list; a long subject clips after it, the arrow does not.
    if step.action.is_meld() {
        let (text, color) = match meld_into {
            Some(tgt) => (format!("-> {tgt}  "), Theme::TEXT_DIM),
            None => ("-> (none)  ".to_string(), Theme::ACCENT_CLOSE),
        };
        spans.push(Span::styled(text, band(Style::default().fg(color))));
    }
    spans.push(Span::styled(step.subject.clone(), subject));
    Line::from(spans)
}

/// A dim key-hint line.
fn hint(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(Theme::TEXT_DIM)))
}

/// A single-line, length-bounded preview of a field value: newlines collapse to spaces
/// and anything past [`PREVIEW_MAX`] chars is trimmed with a trailing "...".
fn preview(text: &str) -> String {
    let one: String = text.replace('\n', " ");
    let mut chars = one.chars();
    let head: String = chars.by_ref().take(PREVIEW_MAX).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

