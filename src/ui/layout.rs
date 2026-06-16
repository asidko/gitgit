//! Single pure source of layout geometry, shared by rendering and hit-testing.
//!
//! [`compute_layout`] turns a frame area + [`ViewState`] into a [`LayoutMap`] of
//! every clickable / scrollable rectangle. The `ui` modules render into these
//! rects; the runtime's `map_mouse` hit-tests against the same map, so geometry
//! never drifts between what is drawn and what responds to clicks. PURE: no
//! `store`, no `Msg`.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::model::{filter_label, filter_options, RepoModel};
use crate::view_state::{
    menu_rows, CommitMenu, Dialog, DiffMode, FilesMenu, ViewState,
    FILTER_KINDS, MENUS,
};
pub use crate::view_state::{Divider, FilterKind, MenuAction, MenuId, SplitAxis};

use super::log_panel::{log_col_widths, ColWidths};

/// One top menu-bar menu's hit-test rectangle plus the menu it opens.
#[derive(Clone, Copy, Debug)]
pub struct MenuRect {
    pub rect: Rect,
    pub id: MenuId,
}

/// Geometry of the diff/preview body. In side-by-side mode both `*_new` rects
/// and `divider` are `Some`; in unified mode they are `None` (single column).
#[derive(Clone, Copy, Debug)]
pub struct DiffLayout {
    pub header_old: Rect,
    pub header_new: Option<Rect>,
    pub body_old: Rect,
    pub body_new: Option<Rect>,
    pub divider: Option<Rect>,
    /// The per-line blame gutter strip (View > Blame), carved off the LEFT of the EDITABLE
    /// side's body (the new pane side-by-side, the single column unified) so the diff body
    /// shrinks and its internal gutter/hscroll/click math is untouched. `None` when blame is
    /// off / not loaded / the body is too narrow.
    pub blame: Option<Rect>,
}

/// Width of the per-line blame gutter strip: `aaaaaaa Author DD.MM` packed into a fixed column.
pub const BLAME_GUTTER_W: u16 = 22;

/// What a draggable handle controls: a pane split (a fraction of its parent). The
/// log right-hand columns are content-derived (auto-fit), so they have no draggable
/// boundary; the only draggable handles left are the four pane separators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleKind {
    /// A pane separator; the drag coord becomes a fraction of `parent`.
    Pane(Divider),
}

/// A draggable separator tying its [`HandleKind`] to the geometry the runtime
/// needs to start a drag (`handle`) and to convert a drag coord into a value
/// (`parent` + `axis`).
#[derive(Clone, Copy, Debug)]
pub struct DividerRect {
    pub id: HandleKind,
    pub axis: SplitAxis,
    /// The 1-cell-thick separator rect the cursor must hit to begin a drag.
    pub handle: Rect,
    /// The container the split divides; origin+size convert a coord to a fraction.
    pub parent: Rect,
}

/// Every region the runtime needs to hit-test or scroll for one frame.
#[derive(Clone, Debug)]
pub struct LayoutMap {
    pub menu_bar: Rect,
    pub menus: Vec<MenuRect>,
    pub close_btn: Rect,
    /// The bottom HINT BAR row (lazygit-style key strip); zero-width on a frame too
    /// short to spare a row. Carved off the BOTTOM of the whole frame before any
    /// other band, so every pane simply sits one row higher.
    pub hint_bar: Rect,
    /// Clickable verb chips on the hint bar (label+key spans). A chip click produces
    /// the SAME `Msg` as its hotkey (mapped at the runtime boundary). Empty while the
    /// editable diff is focused (bare letters type there - the bar shows inert
    /// editor hints instead).
    pub hint_chips: Vec<(Rect, crate::view_state::HintKey)>,
    /// The open top-menu popup geometry, when a menu is open.
    pub menu_dropdown: Option<MenuDropdownLayout>,
    /// The open commit-row context-menu popup geometry, when one is open.
    pub commit_menu: Option<CommitMenuLayout>,
    /// The files-pane row's right-click context menu popup geometry, when one is open.
    pub files_menu: Option<FilesMenuLayout>,
    /// `None` when the diff viewer is hidden (`show_diff == false`).
    pub diff: Option<DiffLayout>,
    /// The LEFT log-view toolbar strip (search field + filter labels), over the
    /// commit-log pane. Split from the RIGHT files toolbar at `split_log_h`.
    pub toolbar: Rect,
    /// Clickable sub-regions of the left toolbar (search field, toggles, filter labels).
    pub toolbar_ui: ToolbarLayout,
    /// The RIGHT files toolbar strip (over the file-changes column), aligned with
    /// the right column below it.
    pub files_toolbar: Rect,
    /// The files-pane search field's clickable sub-regions (lens, query, clear `x`,
    /// `.*` toggle), laid out at the LEFT of the files toolbar strip.
    pub files_search: FilesSearchRects,
    /// Clickable controls in the files toolbar (Flat / All / focus), laid out AFTER
    /// the search field.
    pub files_actions: Vec<FilesActionRect>,
    /// The open filter dropdown's popup geometry, when one is open.
    pub dropdown: Option<DropdownLayout>,
    /// The lens icon's recent-search history popup, when open (and non-empty).
    pub search_history: Option<SearchHistoryLayout>,
    /// The revert confirmation modal's geometry, when the modal is open.
    pub revert_modal: Option<RevertModalLayout>,
    /// The open action-bar dialog's geometry (input / confirm / copy), when one is open.
    pub dialog: Option<DialogLayout>,
    pub log_list: Rect,
    /// The auto-fit right-hand log column widths (author / hash / date), DERIVED each
    /// layout from the visible commits' rendered cell content. The renderer reads these
    /// so the drawn columns lean to their content and the subject column flexes to fill.
    pub log_col_widths: ColWidths,
    /// One-row "A vs B" header carved off the TOP of the file-changes column: the diff
    /// base vs the selected side (`<parent> vs <commit> (selected)`, or `HEAD/<head> vs
    /// current changes` on the working row). Zero height when the column is too short.
    pub files_header: Rect,
    pub files_list: Rect,
    /// The leading clickable mark-gutter column of `files_list` (width
    /// [`MARK_GUTTER_W`]): a plain left-click here toggles that file row's mark.
    /// Same geometry the files panel paints the gutter into, so the drawn checkbox
    /// and the click target agree cell-for-cell.
    pub files_gutter: Rect,
    pub detail: Rect,
    /// Hit-test rect of the detail panel's "Show all"/"Hide" branches link.
    /// `None` when no commit is selected or it has no containing branches.
    pub branches_link: Option<Rect>,
    /// Draggable separators present this frame: up to four pane splits (`DiffLog`
    /// and `DiffOldNew` absent when the diff is hidden / in unified mode). The log
    /// right-hand columns are auto-fit, so they contribute no draggable handles.
    pub dividers: Vec<DividerRect>,
}

/// Width (cells) of the changed-files pane's leading mark gutter: a clickable
/// checkbox column that shows the [`crate::theme::Glyph::MARK`] dot on a marked
/// file row and is blank otherwise. Single source shared by `files_panel` (which
/// paints the gutter cell) and `main::map_mouse` (which hit-tests a gutter click
/// vs a body click), so the drawn checkbox and the click target agree cell-for-cell.
pub const MARK_GUTTER_W: u16 = 1;

/// Width of the close button cell region on the right of the toggles bar.
const CLOSE_W: u16 = 3;
/// One space of padding on each side of a pill label.
const PILL_PAD: u16 = 1;

/// Inner text width of the search field (between the lens and the toggles).
pub const FIELD_TEXT_W: u16 = 22;
/// Inner text width of the files-pane search field (narrower than the log search,
/// to leave room for the Flat / All / focus controls on the same strip).
pub const FILES_FIELD_TEXT_W: u16 = 14;
/// One leading space before the search field on the toolbar.
pub const TOOLBAR_LEAD: u16 = 1;
/// Gap (columns) between the search field and the first filter label, and
/// between adjacent filter labels.
pub const TOOLBAR_GAP: u16 = 3;
/// The `.*` toggle label renders as ` xx ` (2 chars + 2 padding).
pub const TOGGLE_W: u16 = 4;
/// Width of the search-field lens cell block: " <lens> " (3 cols).
pub const FIELD_LENS_W: u16 = 3;
/// Width of the in-field clear control ` x ` (1 glyph + 2 padding), shown only while
/// the query is non-empty. Matches a toggle pill's padding so the field stays aligned.
pub const FIELD_CLEAR_W: u16 = 3;
/// Width of the toolbar refresh button ` <refresh> ` (1 glyph + 2 padding).
pub const REFRESH_BTN_W: u16 = 3;

/// Toolbar regions the runtime hit-tests: the search field, the `.*` regex
/// toggle, and each filter label (with the kind it opens). Computed from the
/// SAME span widths `toolbar.rs` renders, so clicks land on what is drawn.
#[derive(Clone, Debug)]
pub struct ToolbarLayout {
    /// The lens icon: a click opens the recent-search history popup.
    pub search_lens: Rect,
    /// The query TEXT region: a click focuses the field for typing.
    pub search_field: Rect,
    /// The clear `x` control, present only while the query is non-empty.
    pub search_clear: Option<Rect>,
    pub regex_toggle: Rect,
    pub filter_labels: Vec<(FilterKind, Rect)>,
    /// The refresh button (Update Project: fetch + ff-pull), right of the last filter.
    pub refresh_btn: Rect,
}

/// The recent-search history popup (the lens icon's dropdown): its bordered frame
/// plus one inset rect per visible history entry, for click hit-testing. Present
/// only while `view.search_history_open` and the history is non-empty.
#[derive(Clone, Debug)]
pub struct SearchHistoryLayout {
    pub frame: Rect,
    pub options: Vec<Rect>,
}

/// The open filter dropdown's popup geometry: its bordered frame plus one rect
/// per option row (already inset past the border) for click hit-testing.
#[derive(Clone, Debug)]
pub struct DropdownLayout {
    pub kind: FilterKind,
    pub frame: Rect,
    /// First visible option index (the scroll offset). A long option list is capped to
    /// [`MAX_DROPDOWN_ROWS`] visible rows; `options[j]` is the rect for the j-th VISIBLE
    /// row, i.e. absolute option `scroll + j`. The renderer windows the list at the same
    /// offset, so a click maps to the option actually drawn there.
    pub scroll: usize,
    pub options: Vec<Rect>,
}

/// Max option rows a filter dropdown shows before it caps + scrolls (a long Branch /
/// User list must not grow into a full-screen overlay). The list scrolls with the
/// keyboard selection / wheel within this window.
pub const MAX_DROPDOWN_ROWS: usize = 8;

/// The revert confirmation modal's geometry: its centered bordered frame plus the
/// `[Yes]` / `[No]` button rects for mouse hit-testing. Present only while
/// `view.revert_confirm.is_some()`; the renderer draws into the same rects.
#[derive(Clone, Copy, Debug)]
pub struct RevertModalLayout {
    pub frame: Rect,
    pub yes: Rect,
    pub no: Rect,
}

/// The open action-bar dialog's geometry (input / confirm / copy), present only while
/// `view.dialog.is_some()`. Mirrors what `ui::dialog` paints so a click lands on the
/// control drawn there: the copy picker's option rows, the optional checkbox row, and
/// the confirm / cancel buttons. A click outside `frame` is swallowed (stays modal).
#[derive(Clone, Debug)]
pub struct DialogLayout {
    pub frame: Rect,
    /// Copy-picker option rows (one per [`crate::view_state::COPY_FIELDS`] entry); empty
    /// for input / confirm dialogs.
    pub rows: Vec<Rect>,
    /// The checkbox row (input dialog that carries one), for a click-to-toggle. Else None.
    pub checkbox: Option<Rect>,
    /// The confirm button ([ OK ] / [ Yes ] / [ Copy ]).
    pub confirm: Rect,
    /// The cancel button ([ Cancel ] / [ No ]).
    pub cancel: Rect,
    /// First visible row index for a windowed dialog (the rebase mark-items list, capped to
    /// [`REBASE_MAX_ROWS`] + the screen). 0 for the short dialogs whose rows all fit.
    /// `rows[j]` is the rect for absolute step `scroll + j`; render + hit-test share it.
    pub scroll: usize,
}

/// Max commit rows the rebase dialog shows before it caps + scrolls (a long `base..HEAD`
/// must not grow into a full-screen modal). The list scrolls to keep the focused row
/// visible, mirroring the filter dropdown's [`MAX_DROPDOWN_ROWS`].
pub const REBASE_MAX_ROWS: usize = 12;

/// Compute all layout rects for `area` under `view`. The single geometry fn used
/// by both the renderer and the mouse hit-tester. Records every draggable
/// separator into `dividers` from the SAME geometry that renders its hairline.
pub fn compute_layout(area: Rect, repo: &RepoModel, view: &ViewState) -> LayoutMap {
    let mut dividers = Vec::with_capacity(4);

    // The bottom HINT BAR (lazygit-style key strip) is carved off the whole frame
    // FIRST, so every band below derives from the shrunk area. (The old bottom
    // ACTION bar stays dead - this is a one-row hint strip, its chips firing the
    // same Msgs as their hotkeys, not a second home for the Git menu.)
    let (area, hint_bar) = carve_hint_bar(area);
    let hint_chips = hint_chips(hint_bar, view);

    // Vertical bands: menu bar, hsep, [diff + hsep], log/body (Min 0).
    let rows = vertical_bands(area, view, &mut dividers);
    let menu_bar = rows.bar;
    let (menus, close_btn) = menu_bar_regions(menu_bar);
    let menu_dropdown = view
        .open_menu
        .map(|id| menu_dropdown_layout(id, area, &menus));
    let commit_menu = view.commit_menu.as_ref().map(|m| commit_menu_layout(m, area));
    let files_menu = view.files_menu.as_ref().map(|m| files_menu_layout(m, area));

    let diff = rows.diff.map(|r| diff_layout(r, view, &mut dividers));
    let body = body_regions(rows.body, view, &mut dividers);

    // Auto-fit the right-hand log columns to the visible commits' content. PURE: the
    // renderer reads these (via the layout map), so the drawn columns and this
    // geometry always agree, and the subject column flexes to fill the rest.
    let log_col_widths = log_col_widths(repo, view);

    let toolbar_ui = toolbar_layout(body.toolbar, view);
    let (files_search, files_actions) = files_toolbar_layout(body.files_toolbar, view);
    let dropdown = view
        .open_dropdown
        .map(|kind| dropdown_layout(kind, area, &toolbar_ui, repo, view.dropdown_sel));

    let search_history = if view.search_history_open {
        search_history_layout(area, &toolbar_ui, view)
    } else {
        None
    };

    let revert_modal = view
        .revert_confirm
        .as_ref()
        .map(|req| revert_modal_layout(area, req.paths.len()));

    let dialog = view.dialog.as_ref().map(|d| dialog_layout(area, d));

    // Branches-link rect from the SAME pure helper the detail panel renders with.
    let branches_link = repo
        .detail
        .as_ref()
        .and_then(|d| super::detail_panel::branches_link_rect(d, view, body.detail));

    LayoutMap {
        menu_bar,
        menus,
        close_btn,
        hint_bar,
        hint_chips,
        menu_dropdown,
        commit_menu,
        files_menu,
        diff,
        toolbar: body.toolbar,
        toolbar_ui,
        files_toolbar: body.files_toolbar,
        files_search,
        files_actions,
        dropdown,
        search_history,
        revert_modal,
        dialog,
        log_list: body.log_list,
        log_col_widths,
        files_header: body.files_header,
        files_list: body.files_list,
        files_gutter: mark_gutter_rect(body.files_list),
        detail: body.detail,
        branches_link,
        dividers,
    }
}

/// Reserve the bottom row for the hint bar. A frame too short to spare one (tiny
/// terminals, degenerate resize frames) keeps its full height and gets a zero-width
/// bar, so every widget keeps rendering and the snapshot tiny-frame cases hold.
fn carve_hint_bar(area: Rect) -> (Rect, Rect) {
    if area.height < HINT_BAR_MIN_H {
        return (area, Rect { x: area.x, y: area.y, width: 0, height: 0 });
    }
    let body = Rect { height: area.height - 1, ..area };
    let bar = Rect { x: area.x, y: area.y + area.height - 1, width: area.width, height: 1 };
    (body, bar)
}

/// Frame height below which the hint bar is dropped (the panes need the row more).
const HINT_BAR_MIN_H: u16 = 10;

/// The hint-bar items for the current input context: `(label, key, action)`. An
/// `action` of `None` is an inert hint (rendered, never clickable). The EDITING
/// context (editable diff focused) shows only editor chords - the repo verbs' bare
/// letters TYPE into the buffer there, and the bar must never lie about a key.
/// Single source for BOTH the chip layout and the renderer, so they cannot drift.
pub fn hint_items(editing: bool) -> &'static [(&'static str, &'static str, Option<crate::view_state::HintKey>)] {
    use crate::view_state::HintKey;
    if editing {
        &[
            ("Save", "ctrl+s", None),
            ("Undo", "ctrl+z", None),
            ("Redo", "ctrl+y", None),
            ("Copy", "ctrl+c", None),
            ("Leave", "esc", None),
        ]
    } else {
        &[
            ("Commit", "c", Some(HintKey::Commit)),
            ("Pull", "p", Some(HintKey::Pull)),
            ("Push", "P", Some(HintKey::Push)),
            ("Stash", "S", Some(HintKey::Stash)),
            ("Mark", "space", None),
            ("Keys", "?", Some(HintKey::Help)),
            ("Quit", "q", Some(HintKey::Quit)),
        ]
    }
}

/// Leading pad + separator the hint-bar renderer paints between chips; the chip
/// rects below advance by the same widths so a click lands on the chip drawn.
pub const HINT_LEAD: u16 = 1;
pub const HINT_SEP_W: u16 = 3;

/// Whether the hint bar is in the EDITING context (the editable diff owns bare keys).
/// The SAME gate `route_key` uses to hand keys to the editor, so the bar and the
/// keymap agree by construction.
pub fn hint_editing(view: &ViewState) -> bool {
    view.focus == crate::view_state::Pane::Diff && view.editor.is_some()
}

/// Clickable chip rects for the hint bar, advancing left-to-right exactly as the
/// renderer paints `label: key` chips joined by separators. Chips that would
/// overflow the bar are dropped (never a partial click target).
fn hint_chips(bar: Rect, view: &ViewState) -> Vec<(Rect, crate::view_state::HintKey)> {
    let mut out = Vec::new();
    if bar.width == 0 {
        return out;
    }
    let mut x = bar.x + HINT_LEAD;
    for (i, (label, key, action)) in hint_items(hint_editing(view)).iter().enumerate() {
        if i > 0 {
            x += HINT_SEP_W;
        }
        let w = (label.chars().count() + 2 + key.chars().count()) as u16;
        if x + w > bar.right() {
            break;
        }
        if let Some(act) = action {
            out.push((Rect { x, y: bar.y, width: w, height: 1 }, *act));
        }
        x += w;
    }
    out
}

/// Lay out the toolbar's clickable spans left-to-right from `toolbar`, mirroring
/// exactly how `toolbar.rs` renders them: a leading space, the search field
/// (lens + `FIELD_TEXT_W` text + the `.*` toggle), then the three filter labels
/// each followed by a dropdown caret. The single source of toolbar geometry, so
/// render and hit-test never drift.
fn toolbar_layout(toolbar: Rect, view: &ViewState) -> ToolbarLayout {
    let y = toolbar.y;
    let right = toolbar.right();
    let mut x = toolbar.x.saturating_add(TOOLBAR_LEAD);

    // Search field: ` <lens> <text...> [<x>] <.*> ` (one inset block). The clear
    // `x` is present (and shifts the toggle right) ONLY while the query is non-empty -
    // the layout and `toolbar.rs`'s spans gate it on the SAME condition, so widths match.
    let search_lens = clamp_rect(x, y, FIELD_LENS_W, right);
    let search_field = clamp_rect(x + FIELD_LENS_W, y, FIELD_TEXT_W, right);
    let clear_w = if view.search.is_empty() { 0 } else { FIELD_CLEAR_W };
    let search_clear =
        (clear_w > 0).then(|| clamp_rect(x + FIELD_LENS_W + FIELD_TEXT_W, y, FIELD_CLEAR_W, right));
    let toggles_x = x + FIELD_LENS_W + FIELD_TEXT_W + clear_w;
    let regex_toggle = clamp_rect(toggles_x, y, TOGGLE_W, right);
    let field_w = FIELD_LENS_W + FIELD_TEXT_W + clear_w + TOGGLE_W;
    x = x.saturating_add(field_w + TOOLBAR_GAP);

    // Filter labels: each is `<label> <caret>` then a gap.
    let mut filter_labels = Vec::with_capacity(FILTER_KINDS.len());
    for kind in FILTER_KINDS {
        let (text, _) = filter_label(kind, view);
        let w = text.chars().count() as u16 + 2; // label + " " + caret
        filter_labels.push((kind, clamp_rect(x, y, w, right)));
        x = x.saturating_add(w + TOOLBAR_GAP);
    }

    // Refresh button right after the last filter (its own gap already applied above).
    let refresh_btn = clamp_rect(x, y, REFRESH_BTN_W, right);

    ToolbarLayout {
        search_lens,
        search_field,
        search_clear,
        regex_toggle,
        filter_labels,
        refresh_btn,
    }
}

/// Anchor the recent-search history popup under the lens icon: width = widest entry +
/// borders, height = entry count + borders, clamped into `area`. One inset rect per
/// visible entry for hit-testing. `None` when there is no history to show.
fn search_history_layout(area: Rect, tb: &ToolbarLayout, view: &ViewState) -> Option<SearchHistoryLayout> {
    let n = view.search_history.len();
    if n == 0 {
        return None;
    }
    let anchor = tb.search_lens;
    let widest = view.search_history.iter().map(|q| q.chars().count()).max().unwrap_or(0) as u16;
    let inner_w = (widest + 1).clamp(8, 48);
    let width = (inner_w + 2).min(area.width); // +2 side borders
    let height = ((n as u16) + 2).min(area.height); // +2 top/bottom borders
    let x = anchor.x.min(area.right().saturating_sub(width));
    let y = (anchor.y + 1).min(area.bottom().saturating_sub(height));
    let frame = Rect { x, y, width, height };

    let rows = height.saturating_sub(2) as usize;
    let options = (0..n.min(rows))
        .map(|i| Rect {
            x: frame.x + 1,
            y: frame.y + 1 + i as u16,
            width: frame.width.saturating_sub(2),
            height: 1,
        })
        .collect();
    Some(SearchHistoryLayout { frame, options })
}

/// The mnemonic char index within a filter label's leading keyword (the
/// Alt-letter accelerator, the single underlined char; casing is preserved).
/// Branch -> 'B', User -> 'U', Date -> 't' (D is taken by Diff, so "Date" with the
/// 't' underlined). Single source shared by `toolbar` (rendering) and
/// `main::map_key` (the Alt-letter -> Msg arm), so the underlined glyph and the
/// accelerator letter always match.
pub fn filter_mnemonic(kind: FilterKind) -> usize {
    match kind {
        FilterKind::Branch => 0, // B
        FilterKind::User => 0,   // U
        FilterKind::Date => 2,   // t
    }
}

/// The leading [`MARK_GUTTER_W`]-wide column of the changed-files list: the
/// clickable mark gutter, spanning the list's full height. Clamped to the list
/// width so a too-narrow pane never produces an out-of-bounds gutter.
fn mark_gutter_rect(files_list: Rect) -> Rect {
    Rect {
        x: files_list.x,
        y: files_list.y,
        width: MARK_GUTTER_W.min(files_list.width),
        height: files_list.height,
    }
}

/// A 1-row rect at `(x, y)` of width `w`, clipped so it never spills past `right`.
fn clamp_rect(x: u16, y: u16, w: u16, right: u16) -> Rect {
    Rect {
        x,
        y,
        width: w.min(right.saturating_sub(x)),
        height: 1,
    }
}

/// Anchor the popup under filter `kind`'s label: width = widest option + borders,
/// height = min(option count, [`MAX_DROPDOWN_ROWS`]) + borders, clamped into `area`. A
/// longer list caps and SCROLLS: the visible window follows `sel`, and each VISIBLE row
/// gets an inset rect (absolute option `scroll + j`) for hit-testing.
fn dropdown_layout(
    kind: FilterKind,
    area: Rect,
    toolbar_ui: &ToolbarLayout,
    repo: &RepoModel,
    sel: usize,
) -> DropdownLayout {
    let anchor = toolbar_ui
        .filter_labels
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, r)| *r)
        .unwrap_or_default();

    let options = filter_options(repo, kind);
    let widest = options.iter().map(|o| o.chars().count()).max().unwrap_or(4) as u16;
    // +1 for the dropdown's 1-space row lead; max(4) keeps "All" comfortable.
    let inner_w = (widest + 1).max(4);
    let width = (inner_w + 2).min(area.width); // +2 for the side borders
    // Cap the visible rows so a long Branch / User list scrolls instead of overflowing;
    // also clamp to the available screen height (+2 for the top/bottom borders).
    let capped = options.len().min(MAX_DROPDOWN_ROWS);
    let height = (capped as u16 + 2).min(area.height);

    let x = anchor.x.min(area.right().saturating_sub(width));
    let y = (anchor.y + 1).min(area.bottom().saturating_sub(height));
    let frame = Rect { x, y, width, height };

    // The visible window: keep the selected option on screen (the same offset the
    // renderer windows the List at, so a click maps to the row actually drawn).
    let inner_rows = height.saturating_sub(2) as usize;
    let scroll = list_offset(sel.min(options.len().saturating_sub(1)), options.len(), inner_rows);
    let option_rects = (0..options.len().saturating_sub(scroll).min(inner_rows))
        .map(|j| Rect {
            x: frame.x + 1,
            y: frame.y + 1 + j as u16,
            width: frame.width.saturating_sub(2),
            height: 1,
        })
        .collect();

    DropdownLayout {
        kind,
        frame,
        scroll,
        options: option_rects,
    }
}

/// Max file paths the revert modal lists explicitly before collapsing the tail
/// into an "and K more" line. Single source shared by the layout (height) and the
/// renderer (which paths to print), so the box height always fits its content.
pub const MODAL_MAX_LISTED: usize = 6;
/// Inner width (cols) of the revert modal's content area (between the borders).
const MODAL_INNER_W: u16 = 60;

/// Number of body rows (inside the borders, above the button row) the revert modal
/// renders for `n` target paths: a heading line, a blank, the listed paths (capped
/// at [`MODAL_MAX_LISTED`]) plus an optional "and K more" line, a blank, then the
/// warning line. The single source the layout height and the renderer agree on.
pub fn revert_modal_body_rows(n: usize) -> u16 {
    let listed = n.min(MODAL_MAX_LISTED);
    let more = usize::from(n > MODAL_MAX_LISTED);
    // heading + blank + paths + more? + blank + warning
    (1 + 1 + listed + more + 1 + 1) as u16
}

/// Center a confirmation modal in `area` sized to hold `n` listed paths plus a
/// button row. Exposes the `[Yes]`/`[No]` rects for mouse hit-testing; the renderer
/// fills the same frame. Buttons sit on the last interior row, right-aligned.
fn revert_modal_layout(area: Rect, n: usize) -> RevertModalLayout {
    let body = revert_modal_body_rows(n);
    // body rows + a blank gutter row + the button row, all between the borders.
    let inner_h = body + 1 + 1;
    let width = (MODAL_INNER_W + 2).min(area.width);
    let height = (inner_h + 2).min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let frame = Rect { x, y, width, height };

    // Button row: the last interior row (just above the bottom border).
    let btn_y = frame.bottom().saturating_sub(2);
    let no = Rect {
        x: frame.right().saturating_sub(2 + BTN_NO_W),
        y: btn_y,
        width: BTN_NO_W.min(frame.width),
        height: 1,
    };
    let yes = Rect {
        x: no.x.saturating_sub(1 + BTN_YES_W),
        y: btn_y,
        width: BTN_YES_W.min(frame.width),
        height: 1,
    };
    RevertModalLayout { frame, yes, no }
}

/// Outer width (cells, incl. borders) of an action-bar dialog box.
pub const DIALOG_W: u16 = 60;

/// The two button labels for an action-bar dialog: `(confirm, cancel)`. Shared by the
/// layout (button widths) and `ui::dialog` (the drawn text) so the click rects match
/// the painted buttons cell-for-cell.
pub fn dialog_button_labels(dialog: &Dialog) -> (&'static str, &'static str) {
    match dialog {
        Dialog::Input { .. } => ("[OK]", "[Cancel]"),
        Dialog::Confirm { .. } => ("[Yes]", "[No]"),
        Dialog::Copy { .. } => ("[Copy]", "[Cancel]"),
        Dialog::Choice { kind: crate::view_state::ChoiceKind::PullStrategy, .. } => ("[Pull]", "[Cancel]"),
        Dialog::Choice { .. } => ("[Reset]", "[Cancel]"),
        Dialog::Rebase { .. } => ("[Rebase]", "[Cancel]"),
        // Show History views a past revision's diff; the compare pickers diff vs the rev.
        Dialog::Picker { mode: crate::view_state::InspectMode::CommitDiff, .. } => ("[Show]", "[Cancel]"),
        Dialog::Picker { .. } => ("[Compare]", "[Cancel]"),
        Dialog::RefPick { op, .. } => (op.button(), "[Cancel]"),
        Dialog::Remotes { .. } => ("[Edit]", "[Close]"),
        // Read-only: a lone [Close]; the zero-width confirm never paints or hit-tests.
        Dialog::Help => ("", "[Close]"),
    }
}

/// Number of CONTENT rows a dialog renders above its blank gutter + button row: the
/// editable line plus an optional note + checkbox (input), the single prompt (confirm),
/// or one row per copy field. The single source the layout and `ui::dialog` agree on.
pub fn dialog_content_rows(dialog: &Dialog) -> u16 {
    match dialog {
        Dialog::Input { note, checkbox, .. } => {
            1 + u16::from(note.is_some()) + u16::from(checkbox.is_some())
        }
        Dialog::Confirm { .. } => 1,
        Dialog::Copy { fields, .. } => fields.len() as u16,
        Dialog::Choice { kind, note, .. } => {
            crate::view_state::choice_options(*kind).len() as u16 + u16::from(note.is_some())
        }
        // The rebase step list capped to REBASE_MAX_ROWS (then clamped to the screen in
        // `dialog_layout`), plus the optional warning note below it.
        Dialog::Rebase { steps, note, .. } => {
            steps.len().min(REBASE_MAX_ROWS) as u16 + u16::from(note.is_some())
        }
        // The compare-picker list shares the rebase cap-and-scroll bound.
        Dialog::Picker { items, .. } => items.len().min(REBASE_MAX_ROWS) as u16,
        // The ref picker shares the cap-and-scroll bound.
        Dialog::RefPick { items, .. } => items.len().min(REBASE_MAX_ROWS) as u16,
        // The remotes list shares the cap-and-scroll bound; an empty list still draws one
        // "(no remotes)" placeholder row so the dialog has a body.
        Dialog::Remotes { remotes, .. } => remotes.len().clamp(1, REBASE_MAX_ROWS) as u16,
        Dialog::Help => super::dialog::HELP_LINES.len() as u16,
    }
}

/// Center an action-bar dialog in `area`, exposing the clickable control rects (copy
/// option rows, the optional checkbox, the confirm / cancel buttons) for mouse
/// hit-testing. `ui::dialog` paints the same frame + content rows + buttons. The frame
/// holds the content rows, a blank gutter row, then the button row, all inside borders.
fn dialog_layout(area: Rect, dialog: &Dialog) -> DialogLayout {
    let content = dialog_content_rows(dialog);
    let inner_h = content + 1 + 1; // content rows + blank gutter + button row
    let width = DIALOG_W.min(area.width.saturating_sub(2));
    let height = (inner_h + 2).min(area.height.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let frame = Rect { x, y, width, height };

    let inner_x = frame.x + 1;
    let inner_w = frame.width.saturating_sub(2);
    let inner_y = frame.y + 1;
    let row_at = |i: u16| Rect { x: inner_x, y: inner_y + i, width: inner_w, height: 1 };

    let mut scroll = 0usize;
    let (rows, checkbox) = match dialog {
        Dialog::Copy { fields, .. } => {
            ((0..fields.len() as u16).map(row_at).collect(), None)
        }
        // The CHOICE option rows are the first N content rows (the optional note renders
        // BELOW them), so row index == option index for the hit-test.
        Dialog::Choice { kind, .. } => {
            let n = crate::view_state::choice_options(*kind).len() as u16;
            ((0..n).map(row_at).collect(), None)
        }
        // The REBASE step list windows to the rows that fit (capped at REBASE_MAX_ROWS,
        // then by the clamped frame height), scrolled to keep `sel` visible. `rows[j]` is
        // absolute step `scroll + j`; the renderer windows + the note sits below the rows.
        Dialog::Rebase { steps, sel, note, .. } => {
            let note_rows = u16::from(note.is_some());
            // Interior step rows = frame minus borders(2) + gutter(1) + button(1) + note,
            // and never more than REBASE_MAX_ROWS (the cap `dialog_content_rows` sized the
            // frame to - applied here too so the window is locally bounded, not by proxy).
            let avail = (frame.height.saturating_sub(4 + note_rows) as usize).min(REBASE_MAX_ROWS);
            let visible = avail.min(steps.len());
            scroll = list_offset((*sel).min(steps.len().saturating_sub(1)), steps.len(), visible);
            ((0..visible as u16).map(row_at).collect(), None)
        }
        // The compare picker windows exactly like Rebase: visible rows clamped to the frame +
        // REBASE_MAX_ROWS, scrolled to keep `sel` on screen; `rows[j]` is absolute item scroll+j.
        Dialog::Picker { items, sel, .. } => {
            let avail = (frame.height.saturating_sub(4) as usize).min(REBASE_MAX_ROWS);
            let visible = avail.min(items.len());
            scroll = list_offset((*sel).min(items.len().saturating_sub(1)), items.len(), visible);
            ((0..visible as u16).map(row_at).collect(), None)
        }
        // The ref picker windows exactly like the compare picker.
        Dialog::RefPick { items, sel, .. } => {
            let avail = (frame.height.saturating_sub(4) as usize).min(REBASE_MAX_ROWS);
            let visible = avail.min(items.len());
            scroll = list_offset((*sel).min(items.len().saturating_sub(1)), items.len(), visible);
            ((0..visible as u16).map(row_at).collect(), None)
        }
        // The remotes list windows like the picker; an empty list has no clickable rows (the
        // placeholder row is render-only), so `a` (add) is the only live affordance then.
        Dialog::Remotes { remotes, sel } => {
            let avail = (frame.height.saturating_sub(4) as usize).min(REBASE_MAX_ROWS);
            let visible = avail.min(remotes.len());
            scroll = list_offset((*sel).min(remotes.len().saturating_sub(1)), remotes.len(), visible);
            ((0..visible as u16).map(row_at).collect(), None)
        }
        Dialog::Input { note, checkbox, .. } => {
            let cb = checkbox.as_ref().map(|_| row_at(1 + u16::from(note.is_some())));
            (Vec::new(), cb)
        }
        Dialog::Confirm { .. } | Dialog::Help => (Vec::new(), None),
    };

    // Buttons on the last interior row, right-aligned: cancel flush-right, confirm to
    // its left. Widths come from the shared labels so render + hit-test agree. On a frame
    // too short to hold content + gutter + the button row (the height clamped below
    // `inner_h + 2`), the buttons are zero-width so they neither paint over a content row
    // nor offer a phantom click target - the dialog stays keyboard-drivable (Enter/Esc).
    let (confirm_lbl, cancel_lbl) = dialog_button_labels(dialog);
    let buttons_fit = frame.height >= inner_h + 2;
    let cw = if buttons_fit { confirm_lbl.chars().count() as u16 } else { 0 };
    let nw = if buttons_fit { cancel_lbl.chars().count() as u16 } else { 0 };
    let btn_y = frame.bottom().saturating_sub(2);
    let cancel = Rect {
        x: frame.right().saturating_sub(2 + nw),
        y: btn_y,
        width: nw.min(frame.width),
        height: 1,
    };
    let confirm = Rect {
        x: cancel.x.saturating_sub(1 + cw),
        y: btn_y,
        width: cw.min(frame.width),
        height: 1,
    };
    DialogLayout { frame, rows, checkbox, confirm, cancel, scroll }
}

/// Rendered width of the `[Yes]` button label (single source for layout + render).
pub const BTN_YES_W: u16 = 5;
/// Rendered width of the `[No]` button label.
pub const BTN_NO_W: u16 = 4;

/// `[first pane | 1 sep | rest]`. `frac` sizes the first pane; the rest takes
/// `Min(0)`. At a default `frac` this reproduces the prior hardcoded
/// `Percentage(..)` exactly: e.g. `0.62 -> Percentage(62), Length(1), Min(0)`.
fn split2(frac: f32) -> [Constraint; 3] {
    [
        Constraint::Percentage((frac * 100.0) as u16),
        Constraint::Length(1),
        Constraint::Min(0),
    ]
}

/// The major vertical bands of the frame.
struct Bands {
    bar: Rect,
    /// `Some` only when the diff viewer is shown.
    diff: Option<Rect>,
    body: Rect,
}

/// Split `area` top-to-bottom into the toggles bar, optional diff region (sized
/// by `split_diff_v`), and the log body. Separator rows are consumed here (not
/// part of any region). When the diff shows, records the `DiffLog` divider over
/// the hsep between diff and body; its parent spans diff+sep+body.
fn vertical_bands(area: Rect, view: &ViewState, dividers: &mut Vec<DividerRect>) -> Bands {
    // The menu bar sits flush against the content (no separator row beneath it) so the
    // single-row bar does not read as a tall stranded header.
    if view.show_diff {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // menu bar
                Constraint::Percentage((view.split_diff_v * 100.0) as u16), // diff viewer
                Constraint::Length(1), // hsep
                Constraint::Min(0), // log body
            ])
            .split(area);
        dividers.push(DividerRect {
            id: HandleKind::Pane(Divider::DiffLog),
            axis: SplitAxis::Horizontal,
            handle: parts[2],
            parent: union_v(parts[1], parts[3]),
        });
        Bands {
            bar: parts[0],
            diff: Some(parts[1]),
            body: parts[3],
        }
    } else {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // menu bar
                Constraint::Min(0),    // log body
            ])
            .split(area);
        Bands {
            bar: parts[0],
            diff: None,
            body: parts[1],
        }
    }
}

/// The vertical span covering both `top` and `bottom` (and any gap between).
/// Used as a divider parent so a drag-row maps to a fraction of the full span.
fn union_v(top: Rect, bottom: Rect) -> Rect {
    Rect {
        x: top.x,
        y: top.y,
        width: top.width,
        height: bottom.bottom().saturating_sub(top.y),
    }
}

/// Assign each menu a rect along the bar (left to right) and the close button a
/// rect on the right edge. Pure mirror of how `menu_bar` renders the labels: each
/// label is padded ` <label> ` with a 1-col gap between menus.
fn menu_bar_regions(bar: Rect) -> (Vec<MenuRect>, Rect) {
    let mut menus = Vec::with_capacity(MENUS.len());
    let mut x = bar.x;
    for (id, label) in MENUS {
        let w = label.chars().count() as u16 + PILL_PAD * 2;
        let rect = Rect {
            x,
            y: bar.y,
            width: w.min(bar.right().saturating_sub(x)),
            height: 1,
        };
        menus.push(MenuRect { rect, id });
        x = x.saturating_add(w + 1); // 1-col gap between menu labels
    }
    let close_x = bar.right().saturating_sub(CLOSE_W);
    let close_btn = Rect {
        x: close_x,
        y: bar.y,
        width: CLOSE_W.min(bar.width),
        height: 1,
    };
    (menus, close_btn)
}

/// The open menu popup's geometry: its bordered frame plus one rect per item row
/// (inset past the border) for click hit-testing. Anchored at the menu's label.
#[derive(Clone, Debug)]
pub struct MenuDropdownLayout {
    pub id: MenuId,
    pub frame: Rect,
    pub items: Vec<Rect>,
}

/// Anchor the popup under `id`'s menu label: width = widest item + ` ` lead +
/// borders, height = item count + borders, clamped into `area`. Mirrors
/// [`dropdown_layout`] (the filter popup) so the two read identically.
fn menu_dropdown_layout(id: MenuId, area: Rect, menus: &[MenuRect]) -> MenuDropdownLayout {
    let anchor = menus
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.rect)
        .unwrap_or_default();

    let rows = menu_rows(id);
    let widest = rows
        .iter()
        .filter_map(|r| match r {
            crate::view_state::MenuRow::Action(_, l) => Some(l.chars().count()),
            crate::view_state::MenuRow::Sep => None,
        })
        .max()
        .unwrap_or(4) as u16;
    // +3 for the row lead space + the reserved icon cell + its trailing space (` {icon} `),
    // so labels align whether or not a row carries an icon (mirrors the commit menu).
    let inner_w = (widest + 3).max(4);
    let width = (inner_w + 2).min(area.width); // +2 side borders
    let height = (rows.len() as u16 + 2).min(area.height); // +2 top/bottom borders

    let x = anchor.x.min(area.right().saturating_sub(width));
    let y = (anchor.y + 1).min(area.bottom().saturating_sub(height));
    let frame = Rect { x, y, width, height };

    let inner_rows = height.saturating_sub(2) as usize;
    let item_rects = (0..rows.len().min(inner_rows))
        .map(|i| Rect {
            x: frame.x + 1,
            y: frame.y + 1 + i as u16,
            width: frame.width.saturating_sub(2),
            height: 1,
        })
        .collect();

    MenuDropdownLayout {
        id,
        frame,
        items: item_rects,
    }
}

/// The open commit context menu's geometry: bordered frame + one rect per item row
/// (inset past the border) for click hit-testing. Anchored at the click cell and
/// clamped into `area`. Mirrors [`menu_dropdown_layout`] but anchors at an arbitrary
/// cell (the right-click point) rather than a fixed menu-bar label.
#[derive(Clone, Debug)]
pub struct CommitMenuLayout {
    pub frame: Rect,
    /// First visible item index (the scroll offset, re-clamped to the last full window):
    /// the popup caps to the screen height, so `items[j]` is the rect for absolute item
    /// `scroll + j`. The renderer windows the item list at this SAME offset and the click
    /// hit-test adds it back, so a click on a scrolled menu maps to the item drawn there.
    pub scroll: usize,
    /// Total addressable parent rows (the fixed leaf actions then one row per ref submenu).
    /// The wheel clamp reads this so a menu grown by ref rows scrolls correctly.
    pub rows: usize,
    pub items: Vec<Rect>,
    /// The open branch/tag fly-out's geometry, when `menu.open_ref` is set.
    pub submenu: Option<SubmenuLayout>,
}

/// The open branch/tag fly-out child popup: a bordered frame to the side of the parent
/// menu + one rect per ref-action row. Short (<= 7 rows) so it never needs its own scroll.
#[derive(Clone, Debug)]
pub struct SubmenuLayout {
    pub frame: Rect,
    pub items: Vec<Rect>,
    /// Which ref (index into the menu's `refs`) this fly-out belongs to.
    pub ref_idx: usize,
}

/// The last first-visible item a popup menu can scroll to inside a `frame_height`-row frame
/// (`rows` total minus the inner window height; 0 when every row fits). The ONE home of a
/// menu's scroll bound, shared by [`commit_menu_layout`]/[`files_menu_layout`]'s defensive
/// re-clamp and `main::wheel`'s clamp so the two cannot drift (commit AND files menus).
pub fn menu_max_scroll(rows: usize, frame_height: u16) -> usize {
    let inner_rows = frame_height.saturating_sub(2) as usize;
    rows.saturating_sub(inner_rows)
}

fn commit_menu_layout(menu: &CommitMenu, area: Rect) -> CommitMenuLayout {
    let leaves = menu.parent_rows();
    let rows = menu.row_count();
    // Widest parent row: action labels (separators are inert rules), and each ref row
    // (its label + the " >" submenu marker).
    let widest_leaf = leaves
        .iter()
        .filter_map(|r| match r {
            crate::view_state::CommitRow::Action(_, l) => Some(l.chars().count()),
            crate::view_state::CommitRow::Sep => None,
        })
        .max()
        .unwrap_or(4);
    let widest_ref = menu.refs.iter().map(|r| r.label().chars().count() + 2).max().unwrap_or(0);
    // " {icon} {label}": 1 lead space + 1 icon cell + 1 gap, then the widest label.
    let inner_w = (widest_leaf.max(widest_ref) as u16 + 3).max(4);
    let width = (inner_w + 2).min(area.width); // +2 side borders
    let height = (rows as u16 + 2).min(area.height); // +2 top/bottom borders

    let x = menu.col.min(area.right().saturating_sub(width));
    let y = menu.row.min(area.bottom().saturating_sub(height));
    let frame = Rect { x, y, width, height };

    // Cap the visible rows to the popup height and window the items: a menu taller than
    // the terminal scrolls (wheel) so its bottom actions stay reachable. Re-clamp a stale
    // offset (e.g. after a resize grew the popup) so it never scrolls into empty space.
    let inner_rows = height.saturating_sub(2) as usize;
    let scroll = menu.scroll.min(menu_max_scroll(rows, height));
    let item_rects = (0..rows.saturating_sub(scroll).min(inner_rows))
        .map(|i| Rect {
            x: frame.x + 1,
            y: frame.y + 1 + i as u16,
            width: frame.width.saturating_sub(2),
            height: 1,
        })
        .collect();

    let submenu =
        menu.open_ref.and_then(|ri| submenu_layout(menu, ri, menu.ref_base(), scroll, frame, area));

    CommitMenuLayout {
        frame,
        scroll,
        rows,
        items: item_rects,
        submenu,
    }
}

/// The files-pane row's right-click context menu popup: a bordered frame anchored at the
/// click cell + one rect per VISIBLE row. The full menu (15 rows on a `<current>` file) can
/// exceed a short terminal, so it caps to the screen and the wheel windows the rows (like the
/// commit menu) - else the bottom destructive actions (Rollback/Delete) clip unreachably.
#[derive(Clone, Debug)]
pub struct FilesMenuLayout {
    pub frame: Rect,
    /// Clamped first-visible row: visible item `j` is absolute row `scroll + j` (the shared
    /// contract with the render + hit-test). 0 = top (the open default).
    pub scroll: usize,
    /// Total rows (actions + separators) the menu holds - the wheel-clamp bound's input.
    pub rows: usize,
    pub items: Vec<Rect>,
}

/// Lay out the files context menu at its click anchor, sized to its labels, clamped onto
/// the screen (it shifts up/left at the bottom/right edge like the commit menu) and capped
/// to the screen height with the rows windowed at `menu.scroll`.
fn files_menu_layout(menu: &FilesMenu, area: Rect) -> FilesMenuLayout {
    // Size over the RENDERED rows (actions + separators); widths from the action labels.
    let rows = menu.rows().len();
    let widest = menu.items().iter().map(|a| a.label().chars().count()).max().unwrap_or(4) as u16;
    let inner_w = (widest + 2).max(4); // 1 lead + 1 trailing space around the label
    let width = (inner_w + 2).min(area.width); // +2 side borders
    let height = (rows as u16 + 2).min(area.height); // +2 top/bottom borders
    let x = menu.col.min(area.right().saturating_sub(width));
    let y = menu.row.min(area.bottom().saturating_sub(height));
    let frame = Rect { x, y, width, height };
    // Cap the visible rows to the popup height and window the items at the (re-clamped) scroll
    // so a menu taller than the terminal scrolls instead of clipping its bottom actions.
    let inner_rows = height.saturating_sub(2) as usize;
    let scroll = menu.scroll.min(menu_max_scroll(rows, height));
    let items = (0..rows.saturating_sub(scroll).min(inner_rows))
        .map(|i| Rect {
            x: frame.x + 1,
            y: frame.y + 1 + i as u16,
            width: frame.width.saturating_sub(2),
            height: 1,
        })
        .collect();
    FilesMenuLayout { frame, scroll, rows, items }
}

/// Geometry for ref `ref_idx`'s fly-out: a child popup beside the parent menu, sized to
/// its locality-shaped action labels, anchored to the parent's open ref row. Opens to the
/// RIGHT of the parent, flipping LEFT when it would overflow the screen; clamped vertically
/// so it always fits. `ref_base`/`scroll` place the anchor at the row the user clicked
/// (`ref_base` = the parent rows plus the ref-section separator).
fn submenu_layout(
    menu: &CommitMenu,
    ref_idx: usize,
    ref_base: usize,
    scroll: usize,
    parent: Rect,
    area: Rect,
) -> Option<SubmenuLayout> {
    let rm = menu.refs.get(ref_idx)?;
    let current = menu.current_branch.as_deref().unwrap_or("HEAD");
    let widest = rm
        .actions
        .iter()
        .map(|a| a.label(&rm.name, current).chars().count())
        .max()
        .unwrap_or(4) as u16;
    let inner_w = (widest + 1).max(4); // 1 lead space (no icon column), then the widest label
    let width = (inner_w + 2).min(area.width);
    let height = (rm.actions.len() as u16 + 2).min(area.height);

    // Anchor to the parent's open ref row (its windowed screen y = first inner row +
    // index-below-scroll); if that row is scrolled above the window the anchor pins to the
    // first inner row. The final `.min/.max` clamps the whole fly-out onto the screen.
    let parent_index = ref_base + ref_idx;
    let anchor_y = parent.y + 1 + (parent_index.saturating_sub(scroll)) as u16;
    let y = anchor_y.min(area.bottom().saturating_sub(height)).max(area.y);
    // Open to the right; flip left if the fly-out would spill past the screen edge.
    let x = if parent.right().saturating_add(width) <= area.right() {
        parent.right()
    } else {
        parent.x.saturating_sub(width)
    };
    let frame = Rect { x, y, width, height };
    let items = (0..rm.actions.len())
        .map(|i| Rect {
            x: frame.x + 1,
            y: frame.y + 1 + i as u16,
            width: frame.width.saturating_sub(2),
            height: 1,
        })
        .collect();
    Some(SubmenuLayout { frame, items, ref_idx })
}

/// Identifies a clickable control in the right-hand files toolbar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilesAction {
    /// Toggle the files pane between the nested tree and a FLAT (no-folder) list.
    Flat,
    /// Toggle the files pane between CHANGED-only (default) and the full file tree.
    AllFiles,
    /// Reveal the opened diff file in the list (scroll it into view); in the full-tree
    /// view also unfold every directory that holds a changed file.
    Focus,
}

/// A mnemonic index past any label's length: a label rendered with no underlined
/// accelerator. `mnemonic_spans` renders a label verbatim for an out-of-range index,
/// so this disables the underline cleanly. Retained for callers that opt out.
pub const NO_MNEMONIC: usize = usize::MAX;

/// The ordered files-toolbar controls: id + rendered label + the mnemonic char
/// index (the Alt-letter accelerator, the single underlined char; casing is
/// preserved). "Flat" toggles the no-folder list (Alt+F), "All" the full tree
/// (Alt+A), and the bullseye glyph is the click-only Focus button (reveal the opened
/// file; `NO_MNEMONIC` = no Alt letter). Single source shared by [`compute_layout`]
/// (assigns rects from `label.chars().count()`), `files_toolbar` (renders the label),
/// and `main::map_key` (the Alt-letter -> Msg accelerator), so the hit-test widths and
/// the drawn glyphs stay identical. (Revert moved to the Editor menu.)
pub const FILES_ACTIONS: [(FilesAction, &str, usize); 3] = [
    (FilesAction::Flat, "Flat", 0),    // Alt+F (flat list vs nested tree)
    (FilesAction::AllFiles, "All", 0), // Alt+A (full tree vs changed-only)
    // A glyph-only icon button (no Alt letter): reveal the opened file + unfold changed dirs.
    (FilesAction::Focus, crate::theme::Glyph::FOCUS, NO_MNEMONIC),
];

/// One files-toolbar control's hit-test rectangle plus the action it triggers.
#[derive(Clone, Copy, Debug)]
pub struct FilesActionRect {
    pub rect: Rect,
    pub action: FilesAction,
}

/// The files-pane search field's clickable sub-regions, mirroring the log search
/// field's geometry: lens, the query text, the clear `x` (only while non-empty), and
/// the `.*` regex toggle. Computed from the SAME span widths `files_toolbar.rs`
/// renders, so clicks land on what is drawn.
#[derive(Clone, Copy, Debug)]
pub struct FilesSearchRects {
    pub lens: Rect,
    pub field: Rect,
    pub clear: Option<Rect>,
    pub regex: Rect,
}

/// Split the diff region into a header strip (row 0) and the body below, then
/// into old/new panes (side-by-side) or a single column (unified). In
/// side-by-side, records the `DiffOldNew` divider over the body split (parent =
/// body: full-height, so the column->fraction math is exact).
fn diff_layout(area: Rect, view: &ViewState, dividers: &mut Vec<DividerRect>) -> DiffLayout {
    let header = Rect {
        height: area.height.min(1),
        ..area
    };
    let body = Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    };
    match view.diff_mode {
        DiffMode::Unified => {
            // Blame strip carves off the LEFT of the single column (left of its gutter).
            let (blame, body_old) = carve_blame(body, view);
            DiffLayout {
                header_old: header,
                header_new: None,
                body_old,
                body_new: None,
                divider: None,
                blame,
            }
        }
        DiffMode::SideBySide => {
            let (header_old, _, header_new) = split_panes(header, view.split_diff_h);
            let (body_old, divider, body_new) = split_panes(body, view.split_diff_h);
            dividers.push(DividerRect {
                id: HandleKind::Pane(Divider::DiffOldNew),
                axis: SplitAxis::Vertical,
                handle: divider,
                parent: body,
            });
            // Blame annotates the NEW (editable) side, so the strip carves off its LEFT edge.
            let (blame, body_new) = carve_blame(body_new, view);
            DiffLayout {
                header_old,
                header_new: Some(header_new),
                body_old,
                body_new: Some(body_new),
                divider: Some(divider),
                blame,
            }
        }
    }
}

/// Carve the blame gutter off the LEFT of `body` when View > Blame is on and the blame has
/// loaded, returning `(strip, shrunk_body)`. A too-narrow body keeps the whole width (no strip),
/// so the diff never collapses to nothing just to show blame.
fn carve_blame(body: Rect, view: &ViewState) -> (Option<Rect>, Rect) {
    // Blame gutter is NO-WRAP only: under word-wrap a logical row spans a variable number of
    // physical rows, so the per-line strip cannot align 1:1 (a later enhancement).
    // Also skip the full-width no-change render (a separate single-pane path that spans the
    // whole region; carving there would leave a blank gap). Blame on an unchanged file is a v1 gap.
    if view.show_blame
        && view.blame.is_some()
        && !view.word_wrap
        && !view.diff_full_width
        && body.width > BLAME_GUTTER_W + 12
    {
        let strip = Rect { width: BLAME_GUTTER_W, ..body };
        let shrunk = Rect {
            x: body.x + BLAME_GUTTER_W,
            width: body.width - BLAME_GUTTER_W,
            ..body
        };
        (Some(strip), shrunk)
    } else {
        (None, body)
    }
}

/// Split a strip into [left pane | 1-col divider | right pane], the left pane
/// sized by `frac`.
fn split_panes(area: Rect, frac: f32) -> (Rect, Rect, Rect) {
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(split2(frac))
        .split(area);
    (parts[0], parts[1], parts[2])
}

/// The body's pane/toolbar geometry: the two toolbar strips (left over the log,
/// right over the file-changes column, split at `split_log_h`) and the three
/// clickable list/detail rects below them.
struct BodyRegions {
    toolbar: Rect,
    files_toolbar: Rect,
    log_list: Rect,
    files_header: Rect,
    files_list: Rect,
    detail: Rect,
}

/// Split the log body into [toolbar row | hsep | (log | vsep | right col)], with
/// the right column as [files list | hsep | detail]. The toolbar row is split at
/// the SAME `split_log_h` boundary as the panes, so the left toolbar sits over the
/// log and the right toolbar over the file-changes column. Records the `LogRight`
/// (vertical) and `FilesDetail` (horizontal) dividers over the separators they share.
fn body_regions(body: Rect, view: &ViewState, dividers: &mut Vec<DividerRect>) -> BodyRegions {
    let bands = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // toolbar row
            Constraint::Length(1), // hsep
            Constraint::Min(0), // panes
        ])
        .split(body);

    // Split the toolbar row at the same boundary as the panes below, so the two
    // toolbars stay aligned with the log pane and the right column.
    let tb_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(split2(view.split_log_h))
        .split(bands[0]);
    let toolbar = tb_cols[0];
    let files_toolbar = tb_cols[2];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(split2(view.split_log_h))
        .split(bands[2]);
    dividers.push(DividerRect {
        id: HandleKind::Pane(Divider::LogRight),
        axis: SplitAxis::Vertical,
        handle: cols[1],
        parent: bands[2],
    });

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints(split2(view.split_right_v))
        .split(cols[2]);
    dividers.push(DividerRect {
        id: HandleKind::Pane(Divider::FilesDetail),
        axis: SplitAxis::Horizontal,
        handle: right[1],
        parent: cols[2],
    });

    // Carve a 1-row "A vs B" header off the TOP of the file-changes column when there is
    // room for it plus at least one list row; otherwise the header is zero height and the
    // list keeps the whole column.
    let files_col = right[0];
    let (files_header, files_list) = if files_col.height >= 2 {
        (
            Rect { height: 1, ..files_col },
            Rect { y: files_col.y + 1, height: files_col.height - 1, ..files_col },
        )
    } else {
        (Rect { height: 0, ..files_col }, files_col)
    };

    BodyRegions {
        toolbar,
        files_toolbar,
        log_list: cols[0],
        files_header,
        files_list,
        detail: right[2],
    }
}

/// Lay out the files toolbar's clickable controls left-to-right from
/// `files_toolbar`, mirroring how `files_toolbar.rs` renders them: a leading
/// space, then each label as ` <label> ` with a 1-col gap. The single source of
/// files-toolbar geometry, so render and hit-test never drift. Controls that would
/// spill past the strip are clamped to zero width (skipped on hit-test + render).
fn files_toolbar_layout(strip: Rect, view: &ViewState) -> (FilesSearchRects, Vec<FilesActionRect>) {
    let y = strip.y;
    let right = strip.right();
    let mut x = strip.x.saturating_add(TOOLBAR_LEAD);

    // Search field first: ` <lens> <text...> [<x>] <.*> ` (mirrors the log search). The
    // clear `x` is present (and shifts the toggle right) ONLY while the query is non-empty,
    // gated on the SAME condition `files_toolbar.rs` uses, so widths match.
    let lens = clamp_rect(x, y, FIELD_LENS_W, right);
    let field = clamp_rect(x + FIELD_LENS_W, y, FILES_FIELD_TEXT_W, right);
    let clear_w = if view.files_search.is_empty() { 0 } else { FIELD_CLEAR_W };
    let clear = (clear_w > 0)
        .then(|| clamp_rect(x + FIELD_LENS_W + FILES_FIELD_TEXT_W, y, FIELD_CLEAR_W, right));
    let regex = clamp_rect(x + FIELD_LENS_W + FILES_FIELD_TEXT_W + clear_w, y, TOGGLE_W, right);
    let search = FilesSearchRects { lens, field, clear, regex };
    x = x.saturating_add(FIELD_LENS_W + FILES_FIELD_TEXT_W + clear_w + TOGGLE_W + TOOLBAR_GAP);

    let mut out = Vec::with_capacity(FILES_ACTIONS.len());
    for (action, label, _) in FILES_ACTIONS {
        let w = label.chars().count() as u16 + PILL_PAD * 2;
        out.push(FilesActionRect {
            rect: clamp_rect(x, y, w, right),
            action,
        });
        x = x.saturating_add(w + 1); // 1-col gap between controls
    }
    (search, out)
}

/// First visible list index so `sel` stays on screen within `height` rows.
pub fn list_offset(sel: usize, len: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    let max_off = len - height;
    if sel < height {
        0
    } else {
        (sel + 1 - height).min(max_off)
    }
}

/// Map a click `click_y` within list `list` to a row index. The visible window starts
/// at the wheel free-scroll override `scroll` when set (the list was scrolled away from
/// its selection), else the selection-follow offset - the SAME offset the renderer windows
/// the list at (`widgets::list_state`), so a click lands on the row actually drawn.
/// `None` if the click is outside the list or past the last row.
pub fn row_to_index(
    list: Rect,
    click_y: u16,
    scroll: Option<usize>,
    sel: usize,
    len: usize,
) -> Option<usize> {
    if click_y < list.y || click_y >= list.bottom() {
        return None;
    }
    let offset = scroll.unwrap_or_else(|| list_offset(sel, len, list.height as usize));
    let row = (click_y - list.y) as usize + offset;
    (row < len).then_some(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RepoModel;
    use crate::view_state::ViewState;

    /// Whether `col` lies within `r`'s horizontal span, mirroring `main::in_x` (the
    /// exact gutter-vs-body decision `left_click` makes for a files-row click).
    fn in_x(col: u16, r: Rect) -> bool {
        col >= r.x && col < r.right()
    }

    /// The hint bar takes the bottom row on a normal frame, vanishes on a tiny one,
    /// and its chip rects advance by the exact label/key/separator widths the
    /// renderer paints - the click target IS the chip drawn.
    #[test]
    fn hint_bar_carves_the_bottom_row_and_chips_match_the_render_math() {
        let repo = RepoModel::empty();
        let view = ViewState::new(0);
        let lm = compute_layout(Rect { x: 0, y: 0, width: 120, height: 36 }, &repo, &view);
        assert_eq!(lm.hint_bar, Rect { x: 0, y: 35, width: 120, height: 1 });
        // " Commit: c" = lead 1, label 6 + ": " + key 1 -> rect at x=1, w=9.
        let (first_rect, first_key) = lm.hint_chips[0];
        assert_eq!(first_rect, Rect { x: 1, y: 35, width: 9, height: 1 });
        assert_eq!(first_key, crate::view_state::HintKey::Commit);
        // The inert "Mark: space" chip is skipped: Stash is followed by Keys.
        let keys: Vec<_> = lm.hint_chips.iter().map(|(_, k)| *k).collect();
        use crate::view_state::HintKey;
        assert_eq!(
            keys,
            [HintKey::Commit, HintKey::Pull, HintKey::Push, HintKey::Stash, HintKey::Help, HintKey::Quit]
        );
        // No pane reaches into the bar's row.
        assert!(lm.log_list.bottom() <= 35 && lm.detail.bottom() <= 35);

        // A frame too short keeps its rows: zero-width bar, no chips.
        let tiny = compute_layout(Rect { x: 0, y: 0, width: 40, height: 8 }, &repo, &view);
        assert_eq!(tiny.hint_bar.width, 0);
        assert!(tiny.hint_chips.is_empty());

        // The EDITING context exposes no clickable chips (bare letters type there).
        let mut editing = ViewState::new(0);
        editing.focus = crate::view_state::Pane::Diff;
        editing.editor = Some(crate::view_state::EditorState::opening("f".to_string()));
        let lm_edit = compute_layout(Rect { x: 0, y: 0, width: 120, height: 36 }, &repo, &editing);
        assert!(lm_edit.hint_chips.is_empty());
    }

    #[test]
    fn row_to_index_honors_the_free_scroll_override() {
        let list = Rect { x: 0, y: 5, width: 40, height: 10 };
        // Follow mode (None): the offset is the selection-follow value. With sel 0 and a
        // 10-row window, the top visible row maps to index 0.
        assert_eq!(row_to_index(list, 5, None, 0, 50), Some(0));
        // A selection scrolled to the bottom shifts the window: sel 49 in 50 rows -> the
        // top visible row is 40.
        assert_eq!(row_to_index(list, 5, None, 49, 50), Some(40));
        // Free-scroll override: the window starts at `scroll` REGARDLESS of the
        // selection, so the top visible row maps to the override - the SAME row the
        // renderer drew there (the render==hit-test invariant under a wheel-scrolled list).
        assert_eq!(row_to_index(list, 5, Some(20), 0, 50), Some(20));
        assert_eq!(row_to_index(list, 7, Some(20), 0, 50), Some(22), "third visible row");
        // A click past the last row (override near the end) returns None.
        assert_eq!(row_to_index(list, 14, Some(48), 0, 50), None, "row 50 is out of range");
    }

    #[test]
    fn gutter_click_toggles_mark() {
        // The files pane exposes a leading mark-gutter rect; a click in it is a gutter
        // (-> ToggleMark) click, a click on the rest of the row is a body (-> ClickFile)
        // click. This pins the SAME geometry both the renderer and `map_mouse` use.
        let area = Rect { x: 0, y: 0, width: 200, height: 50 };
        let repo = RepoModel::empty();
        let view = ViewState::new(0);
        let lm = compute_layout(area, &repo, &view);

        // The gutter is the leading MARK_GUTTER_W column of the files list.
        assert_eq!(lm.files_gutter.x, lm.files_list.x, "gutter starts at the list left");
        assert_eq!(lm.files_gutter.width, MARK_GUTTER_W, "gutter is MARK_GUTTER_W wide");
        assert_eq!(lm.files_gutter.height, lm.files_list.height, "gutter spans the list");

        // A click in the gutter column reads as a gutter click (ToggleMark path)...
        let gutter_col = lm.files_gutter.x;
        assert!(in_x(gutter_col, lm.files_gutter), "leading column is a gutter click");
        // ...while a click one column past the gutter is a body click (ClickFile path).
        let body_col = lm.files_gutter.right();
        assert!(in_x(body_col, lm.files_list), "the body column is inside the list");
        assert!(!in_x(body_col, lm.files_gutter), "the body column is NOT a gutter click");
    }

    #[test]
    fn open_menu_yields_anchored_popup() {
        // With a menu open, compute_layout produces a popup with one item rect per
        // menu item, anchored at (or below) the menu label - mirroring the filter
        // dropdown geometry the renderer reuses.
        let area = Rect { x: 0, y: 0, width: 200, height: 50 };
        let repo = RepoModel::empty();
        let mut view = ViewState::new(0);
        view.open_menu = Some(MenuId::Editor);
        let lm = compute_layout(area, &repo, &view);

        let dl = lm.menu_dropdown.expect("an open menu yields a popup");
        assert_eq!(dl.id, MenuId::Editor);
        assert_eq!(
            dl.items.len(),
            crate::view_state::menu_items(MenuId::Editor).len(),
            "one item rect per menu item"
        );
        let editor = lm.menus.iter().find(|m| m.id == MenuId::Editor).unwrap();
        assert_eq!(dl.frame.x, editor.rect.x, "popup anchors at the menu label x");
        assert_eq!(dl.frame.y, editor.rect.y + 1, "popup sits just below the bar");

        // No popup when nothing is open (the default frame stays clean).
        let closed = compute_layout(area, &repo, &ViewState::new(0));
        assert!(closed.menu_dropdown.is_none());
    }

    #[test]
    fn commit_menu_caps_and_windows_items_on_a_short_terminal() {
        let n = crate::view_state::commit_menu_items(false, true).len();
        // A terminal too short to hold every item plus borders forces the cap.
        let area = Rect { x: 0, y: 0, width: 80, height: (n as u16 + 2) - 3 };
        let inner_rows = (area.height - 2) as usize;
        let max_scroll = n - inner_rows;
        // The shared bound helper (used by the wheel clamp) agrees with the local math.
        // No ref rows here, so the total row count == the leaf count `n`.
        assert_eq!(menu_max_scroll(n, area.height), max_scroll, "shared scroll bound");

        // Unscrolled: the popup fills the height, shows the first `inner_rows` items.
        let unscrolled = commit_menu_layout(
            &CommitMenu {
                index: 0,
                col: 0,
                row: 0,
                scroll: 0,
                refs: Vec::new(),
                current_branch: None,
                open_ref: None,
                working: false,
                show_diff_item: true,
            marked: Vec::new(),
            },
            area,
        );
        assert_eq!(unscrolled.frame.height, area.height, "the popup caps to the screen");
        assert_eq!(unscrolled.items.len(), inner_rows, "only the fitting rows get a rect");
        assert_eq!(unscrolled.scroll, 0);

        // Scrolled to the bottom: the last item is now visible and the offset clamps so the
        // window never runs past the end (the rect count stays a full window).
        let scrolled = commit_menu_layout(
            &CommitMenu {
                index: 0,
                col: 0,
                row: 0,
                scroll: max_scroll + 5,
                refs: Vec::new(),
                current_branch: None,
                open_ref: None,
                working: false,
                show_diff_item: true,
            marked: Vec::new(),
            },
            area,
        );
        assert_eq!(scrolled.scroll, max_scroll, "a stale/overshot offset clamps to the last window");
        assert_eq!(scrolled.items.len(), inner_rows, "still a full window of rects");
        // The bottom item (absolute index n-1) is reachable: scroll + last visible row.
        assert_eq!(scrolled.scroll + (scrolled.items.len() - 1), n - 1, "the last item is on screen");
    }

    #[test]
    fn files_menu_caps_and_windows_rows_on_a_short_terminal() {
        // The full `<current>` changed-file menu (the tallest variant): all groups present.
        let menu = FilesMenu {
            path: "src/a.rs".to_string(),
            col: 0,
            row: 0,
            scroll: 0,
            show_diff_item: true,
            local_changes: true,
            has_head_version: true,
            committed: false,
            is_dir: false,
            marked: Vec::new(),
        };
        let n = menu.rows().len();
        // A terminal too short to hold every row plus borders forces the cap.
        let area = Rect { x: 0, y: 0, width: 80, height: (n as u16 + 2) - 3 };
        let inner_rows = (area.height - 2) as usize;
        let max_scroll = n - inner_rows;
        assert_eq!(menu_max_scroll(n, area.height), max_scroll, "shared scroll bound");

        // Unscrolled: the popup caps to the screen and shows the first `inner_rows` rows.
        let unscrolled = files_menu_layout(&menu, area);
        assert_eq!(unscrolled.frame.height, area.height, "the popup caps to the screen");
        assert_eq!(unscrolled.items.len(), inner_rows, "only the fitting rows get a rect");
        assert_eq!(unscrolled.scroll, 0);
        assert_eq!(unscrolled.rows, n, "the layout carries the full row count");

        // Overshot offset clamps to the last full window; the bottom row stays reachable.
        let scrolled = files_menu_layout(&FilesMenu { scroll: max_scroll + 5, ..menu.clone() }, area);
        assert_eq!(scrolled.scroll, max_scroll, "a stale/overshot offset clamps to the last window");
        assert_eq!(scrolled.items.len(), inner_rows, "still a full window of rects");
        assert_eq!(scrolled.scroll + (scrolled.items.len() - 1), n - 1, "the last row is on screen");
    }

    #[test]
    fn commit_menu_submenu_opens_beside_the_parent_and_flips_at_the_edge() {
        use crate::view_state::{RefMenu, RefMenuKind};
        let area = Rect { x: 0, y: 0, width: 120, height: 40 };
        let menu = CommitMenu {
            index: 0,
            col: 2,
            row: 2,
            scroll: 0,
            refs: vec![RefMenu::new("feature".to_string(), RefMenuKind::LocalBranch, false)],
            current_branch: Some("main".to_string()),
            open_ref: Some(0),
            working: false,
            show_diff_item: true,
            marked: Vec::new(),
        };
        let lay = commit_menu_layout(&menu, area);
        let sub = lay.submenu.expect("the open fly-out is laid out");
        // Opens to the RIGHT of the parent frame, one rect per ref action.
        assert_eq!(sub.frame.x, lay.frame.right(), "fly-out opens beside the parent");
        assert_eq!(sub.items.len(), menu.refs[0].actions.len(), "one rect per action");
        assert!(sub.frame.y >= lay.frame.y, "anchored at/under the parent top");

        // Anchored at the right edge, the fly-out flips LEFT so it stays on screen.
        let near_edge = CommitMenu { col: area.width - 1, ..menu.clone() };
        let lay2 = commit_menu_layout(&near_edge, area);
        let sub2 = lay2.submenu.expect("fly-out still laid out near the edge");
        assert!(sub2.frame.x < lay2.frame.x, "flips to the left of the parent");
        assert!(sub2.frame.right() <= lay2.frame.x, "the flipped fly-out stays on screen");
    }

    #[test]
    fn rebase_dialog_caps_and_scrolls_a_long_range() {
        use crate::view_state::{Dialog, RebaseAction, RebaseStep};
        // 20 commits, but only REBASE_MAX_ROWS fit the dialog body even on a tall screen.
        let steps: Vec<RebaseStep> = (0..20)
            .map(|i| RebaseStep {
                short: format!("h{i:02}"),
                full: format!("h{i:02}"),
                subject: format!("s{i}"),
                action: RebaseAction::Pick,
            })
            .collect();
        let area = Rect { x: 0, y: 0, width: 80, height: 40 };

        // Focus the TOP row: window starts at 0, shows REBASE_MAX_ROWS rows.
        let top = dialog_layout(area, &Dialog::Rebase { steps: steps.clone(), sel: 0, base: "h00".into(), note: None });
        assert_eq!(top.rows.len(), REBASE_MAX_ROWS, "the list caps at REBASE_MAX_ROWS even on a tall screen");
        assert_eq!(top.scroll, 0, "the top row needs no scroll");

        // Focus the LAST row: the window scrolls so it is visible, clamped to the last page.
        let bottom = dialog_layout(area, &Dialog::Rebase { steps, sel: 19, base: "h19".into(), note: None });
        assert_eq!(bottom.rows.len(), REBASE_MAX_ROWS, "still a full window of rows");
        assert_eq!(bottom.scroll, 20 - REBASE_MAX_ROWS, "scrolled to keep the last row on screen");
        // The bottom row maps to absolute step 19 (scroll + last visible index).
        assert_eq!(bottom.scroll + (bottom.rows.len() - 1), 19, "the focused last row is on screen");
    }

    #[test]
    fn blame_strip_carves_the_editable_side_only_when_on_and_loaded() {
        use crate::diff::BlameFile;
        let area = Rect { x: 0, y: 0, width: 240, height: 62 };
        let repo = RepoModel::empty();

        // Off by default: no strip, the new pane keeps its full width.
        let off = compute_layout(area, &repo, &ViewState::new(0));
        assert!(off.diff.as_ref().is_none_or(|dl| dl.blame.is_none()), "no strip when blame off");

        // On + loaded (side-by-side, no wrap): a BLAME_GUTTER_W strip carves off the new pane's left.
        let mut view = ViewState::new(0);
        view.show_blame = true;
        view.blame = Some(BlameFile { path: "f".into(), lines: Vec::new() });
        let on = compute_layout(area, &repo, &view);
        let dl = on.diff.expect("the diff region exists");
        let strip = dl.blame.expect("blame strip carved");
        let new = dl.body_new.expect("side-by-side has a new pane");
        assert_eq!(strip.width, BLAME_GUTTER_W, "strip is the blame-gutter width");
        assert_eq!(strip.x + BLAME_GUTTER_W, new.x, "the new pane starts right after the strip");

        // Word-wrap removes the strip (a wrapped row cannot align 1:1).
        view.word_wrap = true;
        let wrapped = compute_layout(area, &repo, &view);
        assert!(wrapped.diff.unwrap().blame.is_none(), "no blame strip under word-wrap");
    }

    #[test]
    fn search_clear_rect_appears_only_with_a_query_and_shifts_the_toggles() {
        let area = Rect { x: 0, y: 0, width: 240, height: 62 };
        let repo = RepoModel::empty();

        let empty = compute_layout(area, &repo, &ViewState::new(0));
        assert!(empty.toolbar_ui.search_clear.is_none(), "no query -> no clear icon");
        // The lens leads the text field (a click there opens history, not focus).
        assert!(empty.toolbar_ui.search_lens.x < empty.toolbar_ui.search_field.x);
        let regex_x = empty.toolbar_ui.regex_toggle.x;

        let mut view = ViewState::new(0);
        view.search = "abc".to_string();
        let filled = compute_layout(area, &repo, &view);
        let clear = filled.toolbar_ui.search_clear.expect("a query shows the clear icon");
        // The clear icon sits between the text field and the `.*` toggle, and pushes
        // the toggles right by exactly its width.
        assert!(clear.x >= filled.toolbar_ui.search_field.right());
        assert_eq!(filled.toolbar_ui.regex_toggle.x, regex_x + FIELD_CLEAR_W);
    }

    #[test]
    fn dialog_buttons_hide_when_the_frame_is_too_short_for_them() {
        let dialog = crate::view_state::Dialog::Copy {
            sel: 0,
            fields: ["a".into(), "b".into(), "c".into(), "d".into()],
        };
        // Tall enough: buttons drawn on the last interior row, BELOW the content rows.
        let tall = dialog_layout(Rect { x: 0, y: 0, width: 80, height: 40 }, &dialog);
        assert!(tall.confirm.width > 0 && tall.cancel.width > 0, "buttons drawn when they fit");
        let last_content_y = tall.rows.last().expect("copy rows").y;
        assert!(tall.confirm.y > last_content_y, "button row sits below the content rows");
        // Too short to hold content + gutter + button row: buttons zero-width (not painted,
        // no phantom click target) so they cannot overprint a content row.
        let short = dialog_layout(Rect { x: 0, y: 0, width: 80, height: 8 }, &dialog);
        assert_eq!((short.confirm.width, short.cancel.width), (0, 0), "buttons hidden on a clamped frame");
    }

    #[test]
    fn search_history_popup_opens_under_the_lens_when_set() {
        let area = Rect { x: 0, y: 0, width: 240, height: 62 };
        let repo = RepoModel::empty();
        let mut view = ViewState::new(0);
        view.search_history = vec!["alpha".to_string(), "beta".to_string()];
        view.search_history_open = true;
        let lm = compute_layout(area, &repo, &view);
        let popup = lm.search_history.expect("popup present when open + non-empty");
        assert_eq!(popup.options.len(), 2, "one row per history entry");
        assert_eq!(popup.frame.x, lm.toolbar_ui.search_lens.x, "anchored under the lens");
        // Closed / empty -> no popup.
        view.search_history_open = false;
        assert!(compute_layout(area, &repo, &view).search_history.is_none());
    }
}

