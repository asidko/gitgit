//! Small shared rendering helpers.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{ListState, Paragraph};
use ratatui::Frame;

use crate::theme::Theme;

/// Build the [`ListState`] for a wheel-scrollable selection list (log / files).
///
/// `scroll` is the free-scroll override: a mouse-wheel tick over the list pins the
/// first visible row independently of the selection.
/// - `Some(offset)`: the viewport is pinned at `offset` and the selection is marked
///   ONLY while it stays within the visible `window` rows. Off-window, the selection is
///   left UNSET - there is nothing to highlight (it has scrolled out of view) and, more
///   importantly, `List` will NOT snap the offset back to it, so the wheel can scroll
///   the selection off-screen.
/// - `None` (the default): the selection drives the offset, so `List` keeps it visible -
///   the original behavior, byte-identical for the golden render.
pub fn list_state(scroll: Option<usize>, sel: usize, window: usize) -> ListState {
    match scroll {
        Some(offset) => {
            let on_screen = window > 0 && sel >= offset && sel < offset + window;
            ListState::default().with_offset(offset).with_selected(on_screen.then_some(sel))
        }
        None => ListState::default().with_selected(Some(sel)),
    }
}

/// Paint a single dim line of `text`, centered horizontally and vertically, over
/// `bg` within `area`. The ONE per-panel loading/error placeholder so every pane's
/// transient notice (`Loading history...`, `Loading commit...`, `Loading diff...`,
/// the error banner) reads identically. PURE: just a Paragraph render.
pub fn centered_notice(frame: &mut Frame, area: Rect, text: &str, fg: Color, bg: Color) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mid = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text.to_string(), Style::default().fg(fg)))
            .alignment(Alignment::Center)
            .style(Style::default().bg(bg)),
        mid,
    );
}

/// Render `label` as styled spans, PRESERVING the label's original casing and
/// only adding [`Modifier::UNDERLINED`] to the single char at `mnemonic` (the
/// Alt-letter accelerator). No char is upper/lower-cased, so a title reads
/// naturally with exactly one underlined letter (e.g. "Whitespace" keeps a
/// lowercase underlined `h`, "Date" a lowercase underlined `t`, "Branch" its
/// natural capital `B`). All spans are painted with `style`. One source for every
/// toolbar's mnemonic label so the accelerator hint renders identically
/// everywhere. `mnemonic` out of range yields the verbatim label with no underline.
pub fn mnemonic_spans(label: &str, mnemonic: usize, style: Style) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(3);
    let mut before = String::new();
    let mut after = String::new();
    let mut hit: Option<char> = None;
    for (i, ch) in label.chars().enumerate() {
        if i == mnemonic {
            hit = Some(ch);
        } else if hit.is_none() {
            before.push(ch);
        } else {
            after.push(ch);
        }
    }
    if !before.is_empty() {
        out.push(Span::styled(before, style));
    }
    if let Some(ch) = hit {
        out.push(Span::styled(
            ch.to_string(),
            style.add_modifier(Modifier::UNDERLINED),
        ));
    }
    if !after.is_empty() {
        out.push(Span::styled(after, style));
    }
    out
}

/// Whether `area` is non-empty AND its origin lies inside the frame's buffer, so a
/// Paragraph render into it cannot index off-buffer. A region the layout collapsed
/// and parked at the frame edge has its origin AT `frame.area().bottom()`/`right()`
/// (one past the last valid cell), which this rejects.
fn rect_in_frame(frame: &Frame, area: Rect) -> bool {
    let frame_area = frame.area();
    area.height > 0
        && area.width > 0
        && area.x < frame_area.right()
        && area.y < frame_area.bottom()
}

/// Horizontal hairline separator filling `area` (expected 1 row tall). Bails on a
/// zero-extent OR out-of-frame rect (origin at/after the buffer edge) so a region
/// the layout collapsed to the frame edge never indexes off-buffer (ratatui panics
/// on a write whose origin lies outside the buffer).
pub fn hsep(frame: &mut Frame, area: Rect) {
    if !rect_in_frame(frame, area) {
        return;
    }
    let line = "\u{2500}".repeat(area.width as usize); // ─
    frame.render_widget(
        Paragraph::new(line).style(Style::default().fg(Theme::BORDER).bg(Theme::BG)),
        area,
    );
}

/// Vertical hairline separator filling `area` (expected 1 col wide). Bails on a
/// zero-extent OR out-of-frame rect (origin at/after the buffer edge), same guard
/// as [`hsep`].
pub fn vsep(frame: &mut Frame, area: Rect) {
    if !rect_in_frame(frame, area) {
        return;
    }
    let col: String = std::iter::repeat_n("\u{2502}\n", area.height as usize)
        .collect();
    frame.render_widget(
        Paragraph::new(col).style(Style::default().fg(Theme::BORDER).bg(Theme::BG)),
        area,
    );
}

/// Truncate `s` to `max` DISPLAY columns, appending a 1-column ellipsis when cut.
/// Uses Unicode display width (a CJK char / emoji is 2 columns), so a subject of any
/// script keeps the right-hand log columns at their fixed positions instead of a
/// wide char overflowing its budget and shoving them off-grid. A wide char that
/// would straddle the cut boundary is dropped whole (never split mid-cell).
pub fn truncate(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if str_width(s) <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "\u{2026}".to_string(); // …
    }
    // Keep the ellipsis's 1 column in reserve.
    let budget = max - 1;
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('\u{2026}');
    out
}

/// Pad `s` with trailing spaces to exactly `width` DISPLAY columns (no truncation).
/// Width-aware so a wide-char string reserves its true column footprint and the
/// trailing pad lands the next column at the right boundary.
pub fn pad(s: &str, width: usize) -> String {
    let w = str_width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - w))
}

/// Display width (terminal columns) of `s` in Unicode width units. The single
/// width measure `truncate`/`pad` and their tests share.
pub fn str_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reassemble the visible text of a span list (casing must be preserved).
    fn spans_text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The single span carrying the UNDERLINED modifier, if any.
    fn underlined<'a>(spans: &'a [Span<'static>]) -> Option<&'a Span<'static>> {
        spans
            .iter()
            .find(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
    }

    #[test]
    fn list_state_follow_mode_selects_and_zeroes_offset() {
        // No override: the selection is set and the offset stays at 0 (the List widget
        // computes the window from the selection, the original behavior).
        let st = list_state(None, 7, 10);
        assert_eq!(st.selected(), Some(7));
        assert_eq!(st.offset(), 0);
    }

    #[test]
    fn list_state_free_scroll_keeps_offset_and_hides_offscreen_selection() {
        // Override at offset 5, window 4 rows -> visible rows are 5..9. A selection at 12
        // is OFF-screen, so it is left UNSET (nothing to highlight, and the List will not
        // snap the offset back to it).
        let off = list_state(Some(5), 12, 4);
        assert_eq!(off.offset(), 5);
        assert_eq!(off.selected(), None, "an off-screen selection is not marked");
        // A selection at 6 IS within 5..9, so it is marked and the offset is honored.
        let on = list_state(Some(5), 6, 4);
        assert_eq!(on.offset(), 5);
        assert_eq!(on.selected(), Some(6));
    }

    #[test]
    fn mnemonic_spans_preserves_case() {
        let style = Style::default();
        // "Whitespace" idx 1 -> the lowercase 'h' is underlined, NOTHING is recased.
        let spans = mnemonic_spans("Whitespace", 1, style);
        assert_eq!(spans_text(&spans), "Whitespace", "the label text is unchanged");
        let hit = underlined(&spans).expect("one underlined mnemonic char");
        assert_eq!(hit.content.as_ref(), "h", "the underlined char is a lowercase h");

        // A first-letter mnemonic keeps its natural capital ("Date" idx 0 -> 'D').
        let spans = mnemonic_spans("Date", 0, style);
        assert_eq!(spans_text(&spans), "Date", "first-letter label keeps its capital");
        assert_eq!(
            underlined(&spans).map(|s| s.content.as_ref()),
            Some("D"),
            "the natural first capital is the underlined char"
        );

        // "Date" idx 2 -> the lowercase 't' is underlined (D is taken by Diff).
        let spans = mnemonic_spans("Date", 2, style);
        assert_eq!(spans_text(&spans), "Date");
        assert_eq!(underlined(&spans).map(|s| s.content.as_ref()), Some("t"));

        // Out-of-range mnemonic -> verbatim label, no underline.
        let spans = mnemonic_spans("Side", 9, style);
        assert_eq!(spans_text(&spans), "Side");
        assert!(underlined(&spans).is_none(), "out-of-range adds no underline");
    }

    #[test]
    fn truncate_and_pad_budget_by_display_width_not_char_count() {
        // ASCII (1 col/char) is unchanged.
        assert_eq!(truncate("hello", 10), "hello", "short ASCII passes through");
        assert_eq!(str_width(&pad("ab", 5)), 5, "ASCII pads to the column width");

        // A CJK char is 2 columns: "中文" is 4 columns wide, not 2.
        assert_eq!(str_width("中文"), 4, "two CJK chars span four columns");
        // pad must reach the column target, not over-pad by the wide-char overflow.
        assert_eq!(str_width(&pad("中文", 8)), 8, "wide-char pad lands at the column width");
        // truncate budgets by column: "中文字" (6 cols) into 5 cols -> drop the char
        // that would straddle the cut, keep <= 4 cols + a 1-col ellipsis (<= 5 total).
        let t = truncate("中文字", 5);
        assert!(str_width(&t) <= 5, "truncated width never exceeds the column budget");
        assert!(t.ends_with('\u{2026}'), "a cut string ends with the ellipsis");
        // A string already within budget is returned verbatim (no ellipsis).
        assert_eq!(truncate("中", 5), "中", "a string within the column budget is verbatim");
        // Emoji are 2 columns too.
        assert_eq!(str_width("🎉"), 2, "an emoji spans two columns");
        assert_eq!(str_width(&pad("🎉x", 6)), 6, "emoji+ASCII pads to the column width");
    }
}
