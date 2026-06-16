//! Visual theme: colors and glyphs.
//!
//! Palette is sampled from the JetBrains New UI "Dark" theme (GoLand Git Log
//! panel) so the terminal rendering reads as a faithful evocation of the source
//! screenshot within the constraints of a character grid.

use ratatui::style::Color;

use crate::diff::TokenKind;

/// Central color palette. All UI modules pull colors from here so re-skinning
/// is a single-file change.
pub struct Theme;

impl Theme {
    // -- Surfaces ---------------------------------------------------------
    /// Primary panel background (editor / tool-window canvas).
    pub const BG: Color = Color::Rgb(0x2b, 0x2d, 0x30);
    /// Slightly lighter toolbar / header strip.
    pub const BG_TOOLBAR: Color = Color::Rgb(0x31, 0x34, 0x37);
    /// Selected row background when the panel is NOT focused (muted grey).
    pub const SELECTION_BLUR: Color = Color::Rgb(0x43, 0x46, 0x4a);
    /// Selected row background when the panel IS focused (JetBrains blue).
    pub const SELECTION_FOCUS: Color = Color::Rgb(0x2e, 0x43, 0x6e);
    /// Text-selection band INSIDE the editable diff: a clearly brighter blue than
    /// `SELECTION_FOCUS` (the cursor-line band) so a selection that starts/ends on
    /// the caret's own line still reads against it, while keeping each char's syntax
    /// color (fg) on top.
    pub const SELECTION_EDIT: Color = Color::Rgb(0x3d, 0x5a, 0x8f);
    /// Search-field inset background.
    pub const FIELD_BG: Color = Color::Rgb(0x1e, 0x1f, 0x22);
    /// Marked-row band in the changed-files pane (multi-selection). A clearly
    /// brighter teal than the old near-invisible `#2c3d3a` so a marked row obviously
    /// reads as selected, yet stays distinct from the blue cursor highlight (which
    /// still overrides it on the active row).
    pub const FILES_MARKED_BG: Color = Color::Rgb(0x2f, 0x55, 0x4e);
    /// Thin separators / borders between panes.
    pub const BORDER: Color = Color::Rgb(0x39, 0x3b, 0x40);

    // -- Text -------------------------------------------------------------
    /// Primary foreground text.
    pub const TEXT: Color = Color::Rgb(0xdf, 0xe1, 0xe5);
    /// Secondary / dimmed text (counts, metadata, placeholders).
    pub const TEXT_DIM: Color = Color::Rgb(0x7c, 0x7e, 0x81);
    /// Even dimmer (disabled-ish) text.
    pub const TEXT_FAINT: Color = Color::Rgb(0x5a, 0x5d, 0x63);
    /// Blue hyperlink / modified-file text.
    pub const LINK: Color = Color::Rgb(0x6e, 0xa0, 0xf5);
    /// Ref-label (branch/tag) accent.
    pub const REF: Color = Color::Rgb(0xb3, 0x86, 0xe0);

    // -- Action accent ----------------------------------------------------
    /// Green accent for added lines/files (diff "Added" rows, added file names).
    pub const ACCENT_RUN: Color = Color::Rgb(0x5f, 0xad, 0x65);
    /// Close-button accent (reddish), used for the toggles-bar close circle.
    pub const ACCENT_CLOSE: Color = Color::Rgb(0xc7, 0x52, 0x4f);

    // -- Diff / preview viewer --------------------------------------------
    /// Editor canvas background, darker than the panels.
    pub const CODE_BG: Color = Color::Rgb(0x1e, 0x20, 0x22);
    /// Faint full-row band behind an added line.
    pub const DIFF_ADD_BG: Color = Color::Rgb(0x26, 0x33, 0x2b);
    /// Faint full-row band behind a removed line.
    pub const DIFF_DEL_BG: Color = Color::Rgb(0x3a, 0x2b, 0x2b);
    /// Faint full-row band behind a modified (changed) line, also the inline
    /// fallback for a context line.
    pub const DIFF_CHG_BG: Color = Color::Rgb(0x29, 0x35, 0x41);
    /// Stronger inline band on inserted tokens within a changed line.
    pub const INLINE_ADD: Color = Color::Rgb(0x2f, 0x6e, 0x3f);
    /// Stronger inline band on removed tokens within a changed line.
    pub const INLINE_DEL: Color = Color::Rgb(0x6e, 0x30, 0x30);
    /// Brighter gutter line-number color for a changed line.
    pub const GUTTER_HL: Color = Color::Rgb(0x38, 0x55, 0x70);
    /// The diff's horizontal scrollbar: a recessed track (clearly distinct from the code
    /// background so the draggable extent reads) and a brighter thumb (only drawn when a
    /// line overflows the pane, word-wrap off).
    pub const SCROLL_TRACK: Color = Color::Rgb(0x37, 0x3a, 0x40);
    pub const SCROLL_THUMB: Color = Color::Rgb(0x6b, 0x71, 0x7a);
}

impl TokenKind {
    /// Concrete color for a syntax token kind. Lives here so the highlighter and
    /// the `ui` layer stay color-agnostic and the palette is single-sourced.
    pub fn color(self) -> Color {
        match self {
            TokenKind::Keyword => Color::Rgb(0xcf, 0x8e, 0x6d),
            TokenKind::Func => Color::Rgb(0xe0, 0xb3, 0x5f),
            TokenKind::Str => Color::Rgb(0x6a, 0xab, 0x73),
            TokenKind::Number => Color::Rgb(0x5b, 0x9b, 0xd5),
            TokenKind::Type => Color::Rgb(0x2a, 0xa1, 0xb3),
            TokenKind::Ident => Color::Rgb(0xbc, 0xbe, 0xc4),
            TokenKind::Punct => Color::Rgb(0x8a, 0x90, 0x99),
            TokenKind::Comment => Color::Rgb(0x7a, 0x7e, 0x85),
        }
    }
}

/// Distinct colors used to paint commit-graph lanes. Mirrors the cyclic palette
/// JetBrains assigns to branches in the log graph. The engine cycles all six as
/// branches appear; `Red` (the 6th) is only reached by topologies wider than the
/// current fixtures, so it is allowed to be unused.
#[allow(dead_code)] // Red is the deepest palette slot; current fixtures peak at 5 branches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneColor {
    Green,
    Orange,
    Teal,
    Magenta,
    Red,
    Blue,
}

impl LaneColor {
    pub fn color(self) -> Color {
        match self {
            LaneColor::Green => Color::Rgb(0x59, 0xa8, 0x69),
            LaneColor::Orange => Color::Rgb(0xb0, 0x72, 0x2f),
            LaneColor::Teal => Color::Rgb(0x38, 0x9f, 0xa0),
            LaneColor::Magenta => Color::Rgb(0xa8, 0x3b, 0x5f),
            LaneColor::Red => Color::Rgb(0xc7, 0x52, 0x4f),
            LaneColor::Blue => Color::Rgb(0x46, 0x82, 0xd6),
        }
    }
}

/// Glyphs used throughout the UI. Kept in one place so a future ASCII / nerd-font
/// variant is a single swap.
///
/// Every glyph below was verified to render single-cell-width with visible ink
/// in Noto Sans Mono (the snapshot render font), which has NO emoji fallback:
/// flags, folders, clocks, rotation arrows and magnifiers are all absent and
/// render as blanks, so we approximate them with the closest BMP glyph that the
/// font actually carries. Chevrons that turn out blank fall back to ASCII-ish
/// substitutes for the same reason.
pub struct Glyph;

impl Glyph {
    // Toolbar - left (search field + filter dropdowns)
    /// Search-field lens (magnifier). Clicking it opens the recent-search history.
    pub const SEARCH: &'static str = "\u{2315}"; // ⌕
    /// Clear-the-search "x", shown in the field only while the query is non-empty.
    pub const SEARCH_CLEAR: &'static str = "\u{00d7}"; // ×
    /// Down caret for a filter dropdown label and the popup's selected row.
    pub const DROPDOWN: &'static str = "\u{25be}"; // ▾
    /// Files-toolbar "focus" target: reveal the opened file in the list (and, in the
    /// full-tree view, unfold every directory that holds a changed file). A bullseye
    /// (Geometric Shapes block, like the node/ref glyphs) - the crosshair U+2316 renders
    /// as tofu in the common terminal fonts (DejaVu), this reads as a locate/target icon.
    pub const FOCUS: &'static str = "\u{25ce}"; // ◎

    // Graph - node, lines and corners (all single-width box-drawing)
    pub const NODE: char = '\u{25cf}'; // ● (a normal, pushed commit)
    pub const NODE_HOLLOW: char = '\u{25cb}'; // ○ (the <current> row + unpushed commits)
    /// Branch-TIP node (the newest commit on a branch, carrying its ref decoration): a
    /// ringed disc so a branch head reads distinct from an interior commit. The fill
    /// follows the same pushed convention as [`Self::NODE`]: filled fisheye when pushed,
    /// open bullseye when the tip is unpushed/local-only.
    pub const NODE_TIP: char = '\u{25c9}'; // ◉ (a pushed branch tip)
    pub const NODE_TIP_HOLLOW: char = '\u{25ce}'; // ◎ (an unpushed branch tip)
    pub const VLINE: char = '\u{2502}'; // │
    pub const HLINE: char = '\u{2500}'; // ─
    pub const CORNER_UP_RIGHT: char = '\u{2570}'; // ╰
    pub const CORNER_DOWN_RIGHT: char = '\u{256d}'; // ╭
    pub const CORNER_UP_LEFT: char = '\u{256f}'; // ╯
    pub const CORNER_DOWN_LEFT: char = '\u{256e}'; // ╮
    /// Vertical with a left stub: a lane that both arrives from above and leaves
    /// below while joining the node on its left (a merge-in and branch-out share
    /// one adjacent-lane cell). Combines CORNER_UP_LEFT + CORNER_DOWN_LEFT.
    pub const TEE_LEFT: char = '\u{2524}'; // ┤
    /// Mirror of [`Self::TEE_LEFT`] when the node lies to the right. Combines
    /// CORNER_UP_RIGHT + CORNER_DOWN_RIGHT.
    pub const TEE_RIGHT: char = '\u{251c}'; // ├

    // Tree
    pub const CHEVRON_OPEN: &'static str = "\u{25be}"; // ▾ (U+2304 blank in font)
    pub const CHEVRON_CLOSED: &'static str = "\u{203a}"; // ›
    /// Lines-of-file marker. The trigram (U+2630) is blank; a triple-bar renders.
    pub const FILE: &'static str = "\u{2261}"; // ≡
    /// Mark-gutter glyph shown on a MARKED file row (the leading clickable
    /// checkbox cell). The filled node dot (U+25CF) is verified single-width with
    /// visible ink in Noto Sans Mono; an unmarked row leaves the cell blank.
    pub const MARK: char = '\u{25cf}'; // ●

    // Ref chips / detail
    /// Tag chip accent; tags share the SAME diamond family as branches (the lozenge ◊
    /// read as a different, unrelated shape). The fill encodes the same pushed convention
    /// via the tag's commit state: the unfilled diamond is a local / not-yet-pushed tag,
    /// the filled diamond a pushed tag - matching the branch decorations cell-for-cell.
    pub const REF_TAG: &'static str = "\u{25c7}"; // ◇ (local / unpushed tag)
    pub const REF_TAG_ON_REMOTE: &'static str = "\u{25c6}"; // ◆ (pushed tag)
    /// Branch decoration; the fill encodes LOCALITY: a FILLED diamond for a
    /// remote-tracking ref (lives on the remote), the UNFILLED diamond for a
    /// local branch (local-only).
    pub const REF_ON_REMOTE: &'static str = "\u{25c6}"; // ◆ (remote-tracking ref)
    pub const REF_BRANCH: &'static str = "\u{25c7}"; // ◇ (local branch)

    // Files toolbar - icon-only buttons (all width-1 BMP; unicode-width == 1)
    /// Leading icon on the Revert button: an undo/hook arrow. U+21A9 LEFTWARDS
    /// ARROW WITH HOOK (EAW narrow -> 1 cell). Rendered as "<icon> Revert".
    pub const REVERT: &'static str = "\u{21a9}"; // ↩

    // Toggles bar
    /// Close button. A filled circle, verified single-width with visible ink in
    /// the render font (the emoji "x" code points are all blank).
    pub const CLOSE: &'static str = "\u{25cf}"; // ●
    /// Folded-context (collapsed unchanged region) marker fill.
    pub const FOLD: char = '\u{223c}'; // ∼

    // Diff viewer
    /// Revision marker in the diff header. The padlock / commit emoji are blank
    /// in the render font; a small filled diamond reads as a revision chip.
    pub const REV_LOCK: &'static str = "\u{25c6}"; // ◆
    /// Space marker shown when whitespace rendering is on: a faint mid-dot. One cell
    /// wide so the caret/column math is unchanged.
    pub const WS_SPACE: char = '\u{00b7}'; // ·
    /// Tab marker shown when whitespace rendering is on: a right-arrow (U+2192, one
    /// cell in a Latin locale) so the column count never shifts.
    pub const WS_TAB: char = '\u{2192}'; // ->
    /// Horizontal scrollbar fill: a LOWER HALF block, so the track + thumb read as a thin
    /// bar hugging the bottom of the row (half the height of a full-cell fill) while still
    /// occupying one clickable terminal row.
    pub const SCROLL_BAR: char = '\u{2584}'; // half block

    // Commit context-menu item icons (a leading icon column, WebStorm-style). Each is a
    // BMP glyph verified to render with visible ink in the DejaVu Sans Mono QA font (the
    // emoji clipboard/branch/cherry code points are all blank tofu there). Their East Asian
    // Width is Narrow or Ambiguous; both measure 1 cell under `unicode-width` (the crate
    // ratatui sizes spans with), so the fixed icon column stays in lockstep with the layout
    // (the same convention the ref-chip diamond/lozenge already rely on). The lone caveat is
    // a terminal configured to paint Ambiguous glyphs double-width (some CJK setups), where
    // the icon would overrun its cell - the same pre-existing risk as the ref decorations.
    /// A thin group separator between intent tiers of the commit menu.
    pub const MENU_SEP: &'static str = "\u{2500}"; // horizontal rule
    /// Edit Commit Message / Rename ref - a pencil. U+270E (not U+270F, which is
    /// emoji-presentation, width 2).
    pub const MENU_EDIT: &'static str = "\u{270e}"; // pencil
    /// New Branch / New Tag (one shared "make a ref here" glyph) - the local-branch
    /// diamond, consistent with the branch ref decoration.
    pub const MENU_BRANCH: &'static str = "\u{25c7}"; // diamond
    /// Tag ref in a submenu - the diamond, consistent with the tag ref decoration (tags
    /// share the branch diamond family; the label "Tag '<name>'" carries the distinction).
    pub const MENU_TAG: &'static str = "\u{25c7}"; // ◇ diamond
    /// Checkout - a rightwards hook arrow reads as "switch to".
    pub const MENU_CHECKOUT: &'static str = "\u{21aa}"; // hook arrow
    /// Cherry-Pick - circled plus reads as "apply this commit onto".
    pub const MENU_CHERRY: &'static str = "\u{2295}"; // circled plus
    /// Rebase Onto (submenu) - up/down arrows read as "reorder/rebase".
    pub const MENU_REBASE: &'static str = "\u{21c5}"; // up-down arrows

    // Branch/tag submenu icons (same width discipline + DejaVu verification as above).
    /// Merge a ref into the current branch - an up-right arrow reads as "branch in".
    pub const MENU_MERGE: &'static str = "\u{21b1}"; // arrow up then right
    /// Push a branch to its remote - an upwards arrow.
    pub const MENU_PUSH: &'static str = "\u{2191}"; // up arrow
    /// Pull a remote branch into the current branch - a downwards arrow.
    pub const MENU_PULL: &'static str = "\u{2193}"; // down arrow
    /// Delete a branch/tag - a heavy multiplication x.
    pub const MENU_DELETE: &'static str = "\u{2715}"; // x
    /// A remote-tracking branch row in the submenu list - the filled (on-remote) diamond.
    pub const MENU_BRANCH_REMOTE: &'static str = "\u{25c6}"; // filled diamond
    /// Commit (global Git menu) - a check mark reads as "record this". U+2713 (width 1 in
    /// DejaVu; U+2714 is emoji-presentation, width 2).
    pub const MENU_COMMIT: &'static str = "\u{2713}"; // check mark
    /// Update Project (fetch + pull) - a clockwise refresh arrow. Also the toolbar refresh
    /// button glyph. U+21BB (width 1 in DejaVu).
    pub const MENU_UPDATE: &'static str = "\u{21bb}"; // clockwise open-circle arrow
    /// The log's "Load more history" footer row - a downwards arrow reads as "older below".
    pub const LOAD_MORE: &'static str = "\u{2193}"; // down arrow
}
