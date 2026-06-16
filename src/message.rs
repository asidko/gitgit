//! The one message type that flows into [`crate::store::AppState::apply`].
//!
//! Input intents are keycode-free (the keymap at the loop boundary maps physical
//! keys -> intents). Backend pushes carry data a future async git worker sends
//! over the channel, waking the poll loop. `RepoLoaded`/`DetailLoaded`/
//! `BackendError` are the seam for that worker; the fixture path uses them today
//! to prove the channel.

use crate::diff::FileView;
use crate::model::{CommitDetail, RepoModel, TreeNode};
use crate::view_state::{
    CommitMenuAction, Divider, FilterKind, MenuAction, MenuId, RebaseAction, RefAction,
};

/// A cursor-movement direction in the editable diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
    /// Start / end of the current line.
    Home,
    End,
    /// Up / down by a page (a fixed line step, like the list panes).
    PageUp,
    PageDown,
}

/// A single text-editing operation in the editable diff (the right/working side).
/// Cursor moves carry no scroll geometry: the renderer derives the scroll offset
/// from the cursor each frame (like the list panes), so `apply` stays geometry-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditOp {
    /// Insert a character at the cursor (replacing any selection first).
    Insert(char),
    /// Split the line at the cursor (Enter; replaces any selection).
    Newline,
    /// Delete the selection if any, else the character before the cursor (joins lines).
    Backspace,
    /// Delete the selection if any, else the character at the cursor (joins the next).
    Delete,
    /// Move the cursor. `select` extends the selection from its anchor (Shift held);
    /// without it the selection is cleared.
    Move { dir: Dir, select: bool },
    /// Place the cursor at an absolute buffer position (a mouse click / drag). `select`
    /// extends the selection from the anchor (a drag), else clears + sets a new anchor.
    Place { row: usize, col: usize, select: bool },
    /// Select the word at buffer `(row, col)` (a double-click): anchor + cursor span the
    /// run of word chars around the click, or the single char when it is not a word char.
    SelectWord { row: usize, col: usize },
    /// Select the whole line at buffer `row` (a triple-click), including its trailing
    /// newline when a following line exists so a delete removes the entire line.
    SelectLine { row: usize },
    /// Copy the selection to the clipboard register (no buffer change).
    Copy,
    /// Cut the selection to the clipboard register (deletes it).
    Cut,
    /// Paste the clipboard register at the cursor (replacing any selection).
    Paste,
    /// Select the whole buffer (Ctrl+A).
    SelectAll,
    /// Undo the last edit group (Ctrl+Z / Editor menu). No-op with nothing to undo.
    Undo,
    /// Redo the last undone edit group (Ctrl+Y / Ctrl+Shift+Z / Editor menu).
    Redo,
}

/// A single state-changing event.
#[derive(Clone, Debug)]
pub enum Msg {
    // -- input intents (produced by the keymap, never carry keycodes) --
    Quit,
    ToggleFocus,
    /// Move selection in the focused pane by a (possibly negative) step.
    Move(isize),
    /// Expand/collapse the selected directory in the files pane.
    ToggleExpand,
    /// Toggle the files pane between the nested directory tree and a FLAT list of
    /// files (no folders). Files-toolbar "Flat" control / Alt+F. Pure view transform.
    ToggleFlat,
    /// Reveal the opened diff file in the files list (clear the wheel free-scroll so the
    /// selection snaps into view); in the full-tree view also unfold every directory that
    /// holds a changed file. Files-toolbar focus (bullseye) button. Pure view transform.
    FocusOpenFile,
    /// Toggle the file at the given files-panel row in/out of the multi-selection
    /// set (Space / Ctrl-click): additive, the cursor moves there, preview follows.
    /// A directory row marks/unmarks all its descendant FILE rows.
    ToggleMark(usize),
    /// Range-select from the anchor (the cursor at the last plain select) to the
    /// given files-panel row inclusive, marking every FILE row in between
    /// (Shift-click / Shift+arrow). The cursor moves to the target.
    SelectRange(usize),
    /// Clear the multi-selection set (Esc with no other mode open).
    ClearMarks,
    /// Toggle the commit at the given log row in/out of the multi-commit selection
    /// (Ctrl-click in the log); the cursor moves there. Additive.
    ToggleCommitMark(usize),
    /// Range-select commits from the cursor (`log_sel`) to the given log row inclusive
    /// (Shift-click), marking each; the cursor moves to the target.
    SelectCommitRange(usize),
    /// Open the revert confirmation modal over the current target set (the marked
    /// files, else the cursor file). Empty target -> a status hint, no modal.
    /// Fired by Alt+R. `apply` collects paths ZERO-IO.
    RequestRevert,
    /// Open the revert confirmation modal for ONLY the file whose diff is shown (the
    /// Editor-menu Revert), ignoring any multi-select marks. Empty -> a status hint.
    RevertFile,
    /// Dismiss the revert confirmation modal without reverting.
    CancelRevert,
    /// Confirm the revert: `apply` clears the modal and hands the request off to the
    /// runtime via `view.pending_revert` (the event loop sends the batch `Req`).
    ConfirmRevert,
    /// The loader finished a batch revert: the `paths` it SUCCESSFULLY reverted (in
    /// order, possibly partial on error) plus a human-readable `summary`. `apply`
    /// prunes the reverted file rows from the tree, clears the marked set, refreshes
    /// the preview if its file was reverted, and sets `summary` as the status.
    RevertDone { paths: Vec<String>, summary: String },
    /// Toggle word-wrap in the diff/preview viewer.
    ToggleWordWrap,
    /// Show/hide the diff/preview viewer (collapses the top region).
    ToggleDiff,
    /// Flip the files pane between CHANGED-only (default) and the commit's FULL
    /// file tree. ZERO-IO: `apply` flips `view.show_all_files`; the runtime then
    /// re-requests the current commit's tree in the new mode (changed vs full).
    ToggleAllFiles,
    /// Cycle the diff body between side-by-side and unified.
    ToggleDiffMode,
    /// Toggle whitespace markers in the viewer.
    ToggleWhitespace,
    /// Toggle "Hide unchanged": collapse unchanged runs in the displayed diff to a
    /// `N unchanged` fold marker (3-line context around each change), the way a
    /// read-only commit diff already folds - applied to the editable `<current>`
    /// live diff, which otherwise emits the whole file.
    ToggleHideUnchanged,
    /// Toggle the per-line blame gutter (View > Blame). Turning it ON fetches the blame for
    /// the open file; OFF drops it.
    ToggleShowBlame,
    /// Toggle autosave-on-navigate for the editable buffer (Editor menu).
    ToggleAutosave,
    /// Expand/collapse the detail panel's "In N branches" containment list.
    ToggleBranchList,
    /// Select the commit at the given log index (focuses Log, rebuilds preview).
    SelectCommit(usize),
    /// Select the file at the given files-panel index (focuses Files, rebuilds
    /// preview). Pure navigation: used by the keyboard move and the wheel.
    SelectFile(usize),
    /// Click the files-panel row at the given index (focuses Files, selects, and
    /// for a directory row also toggles its expanded state, JetBrains-style).
    ClickFile(usize),
    /// Scroll the diff/preview body by `delta` lines within a `pane_height`-row
    /// viewport. Like [`Msg::ScrollDetail`], the runtime sources `pane_height` from
    /// the diff body rect so apply can clamp the LAST content row to the body bottom
    /// (never scrolling the body off into blank rows); geometry stops at the boundary.
    ScrollDiff { delta: isize, pane_height: usize },
    /// Scroll the EDITABLE diff to logical top row `top` WITHOUT moving the caret (a
    /// mouse-wheel tick over the editable new side). The runtime computes `top` from the
    /// current viewport (already clamped to the row count) since the wrap-aware geometry
    /// lives in `ui`; apply just parks it in `view.edit_scroll`. ZERO-IO. Any subsequent
    /// edit / navigation clears it, restoring the cursor-follow scroll.
    ScrollEdit { top: usize },
    /// Scroll the diff HORIZONTALLY to code-column `offset` (a shift-wheel / horizontal
    /// wheel tick over the diff, word-wrap off). The runtime computes `offset` from the
    /// pane width + longest line (geometry lives in `ui`); apply just parks it in
    /// `view.diff_hscroll`. ZERO-IO. Any edit / caret move / selection change clears it.
    ScrollDiffH { offset: usize },
    /// Scroll the commit-log list to first-visible row `offset` WITHOUT moving the
    /// selection (a wheel tick over the log). The runtime computes `offset` from the
    /// viewport (already clamped to the row count); apply parks it in `view.log_scroll`.
    /// ZERO-IO. Any selection move / filter change clears it (the list refollows).
    ScrollLog { offset: usize },
    /// Scroll the changed-files list to first-visible row `offset` WITHOUT moving the
    /// file selection (a wheel tick over the files panel). Mirrors [`Msg::ScrollLog`].
    ScrollFiles { offset: usize },
    /// Scroll the commit detail pane by `delta` lines within a `pane_height`-row
    /// viewport. The runtime sources both `pane_height` AND `content_height` (the
    /// WRAPPED visual-line count for the current pane width) from the detail rect, so
    /// apply can clamp to the wrapped tail without `ui`-layer wrapping leaking into it.
    ScrollDetail { delta: isize, pane_height: usize, content_height: usize },
    /// Set a pane split to an absolute fraction (mouse drag); apply clamps it.
    SetSplit(Divider, f32),
    /// Nudge a specific pane split by relative keyboard steps; apply clamps it.
    NudgeSplit(Divider, isize),
    /// Nudge the vertical-position split of the focused pane (Ctrl+Up/Down);
    /// apply maps focus Log -> DiffLog, Files -> FilesDetail.
    NudgeFocusedVSplit(isize),

    // -- toolbar search (typing routed by the runtime while search is active) --
    /// Give the search field keyboard focus (click the field or press `/`).
    SearchFocus,
    /// Leave text-input mode. `clear` true -> also wipe the query (Esc); false
    /// keeps it (Enter applies + exits typing).
    SearchBlur { clear: bool },
    /// Append a typed character to the query.
    SearchPush(char),
    /// Delete the last character of the query.
    SearchBackspace,
    /// Clear the search query (the field's `x` clear icon), re-filtering to all.
    SearchClear,
    /// Toggle the recent-search history popup (the lens icon). No-op with no history.
    ToggleSearchHistory,
    /// Run the history entry at `i` (a click in the lens popup): set it as the query,
    /// close the popup, and re-filter.
    PickSearchHistory(usize),
    /// Toggle the `.*` regex matching mode.
    ToggleRegex,

    // -- files-pane search (filters the changed-files list by path) -----------
    /// Give the files-pane search field keyboard focus (click it).
    FilesSearchFocus,
    /// Leave files-search text input. `clear` true -> also wipe the query (Esc);
    /// false keeps it (Enter applies + exits typing).
    FilesSearchBlur { clear: bool },
    /// Append a typed character to the files-search query.
    FilesSearchPush(char),
    /// Delete the last character of the files-search query.
    FilesSearchBackspace,
    /// Clear the files-search query (the field's `x` clear icon).
    FilesSearchClear,
    /// Toggle the files-search `.*` regex matching mode.
    ToggleFilesRegex,

    // -- toolbar filter dropdowns ---------------------------------------------
    /// Open the popup for a filter (click its label or press its mnemonic).
    OpenDropdown(FilterKind),
    /// Close the open dropdown without changing the selection.
    CloseDropdown,
    /// Move the highlighted row in the open dropdown by a (clamped) step.
    DropdownMove(isize),
    /// Pick the option at `row` in the open dropdown (row 0 -> clear the filter).
    DropdownPick(usize),

    // -- top menu bar (Editor / View) -----------------------------------------
    /// Open a top menu-bar menu as a popup (click its label). Closes any open
    /// filter dropdown (the two popups are mutually exclusive).
    OpenMenu(MenuId),
    /// Close the open menu popup without picking an item.
    CloseMenu,
    /// Pick a menu item: flip the matching view toggle and close the menu.
    MenuPick(MenuAction),

    // -- commit row right-click context menu ----------------------------------
    /// Right-click a commit row: select that commit (the row is on screen, so the
    /// wheel viewport is kept) and open its context menu anchored at `(col, row)`.
    OpenCommitMenu { index: usize, col: u16, row: u16 },
    /// Close the open commit context menu without picking an item.
    CloseCommitMenu,
    /// Pick an item from the open commit context menu.
    CommitMenuPick(CommitMenuAction),
    /// Open (or toggle shut) the branch/tag fly-out for ref `ref_idx` (a click on a
    /// "Branch '<name>'" / "Tag '<name>'" parent row). Mutually exclusive: opening one
    /// fly-out closes any other.
    OpenRefSubmenu { ref_idx: usize },
    /// Pick an action from an open branch/tag fly-out: close the whole menu, then run the
    /// ref action (a confirm/input dialog or a parked [`GitAction`]).
    RefMenuPick { ref_idx: usize, action: RefAction },
    /// Scroll the open commit context menu to first-visible item `offset` (a wheel tick
    /// over the popup) when it is taller than the terminal. The runtime clamps `offset`
    /// to the last full window; apply parks it in `view.commit_menu.scroll`. ZERO-IO.
    ScrollCommitMenu { offset: usize },

    // -- files-pane row right-click context menu ------------------------------
    /// Right-click a files-pane row: select that file (the row is on screen, so the wheel
    /// viewport is kept) and open its context menu anchored at `(col, row)`. A directory
    /// row (no file path) opens no menu.
    OpenFilesMenu { index: usize, col: u16, row: u16 },
    /// Close the open files context menu without picking an item.
    CloseFilesMenu,
    /// Pick an item from the open files context menu.
    FilesMenuPick(crate::view_state::FilesMenuAction),
    /// Scroll the open files context menu to first-visible row `offset` (a wheel tick over the
    /// popup) when it is taller than the terminal. The runtime clamps `offset` to the last full
    /// window; apply parks it in `view.files_menu.scroll`. Mirrors [`Msg::ScrollCommitMenu`]. ZERO-IO.
    ScrollFilesMenu { offset: usize },
    /// Close the read-only inspect overlay (Esc), returning to the file's diff/editor.
    CloseInspect,
    /// A "Compare with..." / "Show History" picker's option list finished loading off-thread:
    /// open the picker over `items` for `path`. `kind` labels the dialog (revisions vs refs);
    /// `mode` is the inspect parked on confirm (echoed from the request); `path` echoes the
    /// request target so a stale reply (the user navigated away) is dropped. Empty -> a Notice.
    /// `epoch` echoes the navigation epoch at request time: a reply from before a navigation
    /// is dropped instead of popping a modal long after the user moved on.
    PickListLoaded {
        kind: crate::view_state::PickKind,
        path: String,
        items: Vec<crate::view_state::PickItem>,
        mode: crate::view_state::InspectMode,
        epoch: u64,
    },
    /// The repo's branches/tags finished loading off-thread (`Req::RefList`): open the branch
    /// picker for `op` (checkout / merge / rebase) over `items`. Empty -> a Notice. A reply
    /// from before a navigation (`epoch` mismatch) is dropped, never a late modal pop.
    RefListLoaded { op: crate::view_state::RefOp, items: Vec<crate::view_state::PickItem>, epoch: u64 },
    /// The repo's remotes finished loading off-thread (`git remote`): open the Manage
    /// Remotes dialog over `remotes` (`(name, url)` pairs). Empty -> an empty list the
    /// user can still add to. A pre-navigation reply (`epoch` mismatch) is dropped.
    RemotesLoaded { remotes: Vec<(String, String)>, epoch: u64 },
    /// Open the "add remote" input from the Manage Remotes dialog (the `a` key): a
    /// `name url` text entry. ZERO-IO.
    RemoteAddInput,
    /// Open the confirm to remove the selected remote from the Manage Remotes dialog (the
    /// `d` key). ZERO-IO.
    RemoteRemove,

    // -- in-diff editing (the right/working side is always editable) ----------
    /// Save the editable buffer to disk: `apply` parks `(path, content)` in
    /// `view.pending_save` for the runtime to write off-thread (ZERO-IO here).
    SaveEditor,
    /// Leave editing (Esc): autosave if dirty, then move focus off the diff pane.
    DiffBlur,
    /// A single text-editing operation against the editable buffer.
    Edit(EditOp),
    /// The selected file's two diff sides finished loading (backend push): `base` =
    /// the selected commit's blob (`None` when absent -> empty base), `work` = the
    /// current working-tree text (the live-editable right side). Sets up the editable
    /// buffer + live diff. Applied only while the (commit, path) is still selected.
    EditFileLoaded { commit: String, path: String, base: Option<String>, work: String },
    /// The save finished writing `path` to disk (backend push): clears the dirty flag.
    FileSaved { path: String },

    // -- diff gutter hunk-revert (Stage C) -------------------------------------
    /// Request reverting the hunk under the focused diff line (Enter on the diff
    /// pane). First request ARMS (status warns); a second confirms and parks a
    /// `Req::RevertHunk`. A non-changed focused line -> a status hint.
    RevertHunk,
    /// A hunk revert finished (backend push): sets the summary status.
    HunkReverted { summary: String },

    // -- repo-level git ops (top Git menu): their dialogs + handoffs ---------
    /// Open the commit-message input dialog (Git > Commit / Alt+C). `apply` ZERO-IO.
    OpenCommit,
    /// Open the amend input dialog, prefilled with HEAD's subject (Amend / Alt+M).
    OpenAmend,
    /// Open the tag-name input dialog (Tag / Alt+G).
    OpenTag,
    /// Open the push confirmation dialog (Push / Alt+P).
    RequestPush,
    /// Open the pull confirmation dialog (Pull / Alt+L).
    RequestPull,
    /// One-click Update Project: fetch + ff-pull (toolbar refresh button + Git menu).
    RequestUpdate,
    /// Open the keybindings popup (`?` / the hint bar's Keys chip). `apply` ZERO-IO.
    OpenHelp,
    /// Load a deeper slice of commit history (the log's trailing "Load more history" row).
    LoadMore,
    /// Open the "copy commit field" picker (Copy / Alt+Y). No-op with no real commit
    /// selected (e.g. the synthetic working row).
    OpenCopy,
    /// Type a character into the open input dialog (at the caret, replacing a selection).
    DialogInput(char),
    /// Backspace in the open input dialog (delete the selection, else the char before).
    DialogBackspace,
    /// Forward-delete in the open input dialog (the selection, else the char at caret).
    DialogDelete,
    /// Move the input dialog's caret; `select` extends the selection (shift+arrow).
    DialogCaret { dir: Dir, select: bool },
    /// Select the whole input field (Ctrl+A).
    DialogSelectAll,
    /// Copy the input selection to the internal clipboard register (Ctrl+C).
    DialogCopy,
    /// Cut the input selection to the internal clipboard register (Ctrl+X).
    DialogCut,
    /// Paste the internal clipboard register at the caret (Ctrl+V).
    DialogPaste,
    /// Toggle the input dialog's checkbox (e.g. new-branch checkout). No-op if absent.
    DialogToggleCheck,
    /// Cycle the archive dialog's format (zip -> tar.gz -> tar), rewriting the filename extension.
    DialogCycleArchiveFormat,
    /// Move the open picker's highlighted row by a (clamped) step.
    DialogMove(isize),
    /// Cycle the focused interactive-rebase row's action one step (Pick -> Squash ->
    /// Fixup -> Drop -> Pick), the Space / click affordance. No-op for non-rebase dialogs.
    DialogCycleRow,
    /// Set the focused interactive-rebase row's action outright (the p/s/f/d letter keys).
    /// No-op for non-rebase dialogs.
    DialogSetRow(RebaseAction),
    /// Click the copy picker's option row `i`: select it and confirm (copy + close) in
    /// one gesture, so a single click on a field copies it. No-op for non-copy dialogs.
    DialogPickRow(usize),
    /// Confirm the open dialog: parks the git action (input/confirm) or the clipboard
    /// text (copy picker) for the runtime, then closes the dialog. ZERO-IO.
    DialogConfirm,
    /// Dismiss the open dialog without acting.
    DialogCancel,
    /// A repo-level git action finished on the loader: `summary` -> the status line;
    /// `reload` triggers a fresh `RepoLoaded` (the commit log / working row changed).
    GitActionDone { summary: String, reload: bool },
    /// The loader computed a file's local-changes patch for "Copy as Patch": `apply` parks
    /// `text` in `pending_clipboard` (ZERO-IO) and sets a "Copied patch" notice; the runtime
    /// shells out to the clipboard. Empty/error cases come back as `GitActionDone` instead.
    PatchCopied { text: String },

    // -- commit detail pane: read-only text selection + copy (mouse-driven) ----
    /// Begin a detail-pane text selection at wrapped visual `(row, col)` (a left
    /// press): anchor and cursor both land there. ZERO-IO.
    DetailSelectStart { row: usize, col: usize },
    /// Extend the detail-pane selection's cursor to wrapped visual `(row, col)` (a
    /// drag). No-op when no selection is in progress.
    DetailSelectTo { row: usize, col: usize },
    /// Select chars `[start, end)` on wrapped visual `row` (a double-click selects the
    /// word; a triple-click the whole line). Replaces any current selection.
    DetailSelectWord { row: usize, start: usize, end: usize },

    // -- read-only diff: character-level text selection + copy (mouse-driven) --
    /// Begin a read-only diff selection at logical diff-line `line`, character column
    /// `col` (a left press on the committed code): anchor and cursor both land there.
    /// ZERO-IO.
    DiffSelectStart { line: usize, col: usize },
    /// Extend the read-only diff selection's cursor to `(line, col)` (a drag). No-op
    /// when no diff selection is in progress.
    DiffSelectTo { line: usize, col: usize },
    /// Put `text` on the SYSTEM clipboard: `apply` parks it in `pending_clipboard`
    /// (ZERO-IO) and the runtime shells out (wl-copy/xclip). Used by the detail-pane
    /// selection copy (Ctrl+C) - the runtime composes the text from the selection.
    CopyText(String),

    // Backend pushes: the async seam. The `loader` worker thread constructs these
    // and sends them over the mpsc channel; `apply` handles them ZERO-IO, and the
    // runtime drives the selection -> `Req` requests that produce them.
    /// The repository model finished its initial load. Boxed: `RepoModel` is by far
    /// the largest payload, so boxing keeps every other `Msg` variant small on the
    /// channel (clippy `large_enum_variant`).
    RepoLoaded(Box<RepoModel>),
    /// Full detail for commit `hash` finished loading; applied only while `hash`
    /// is still the selected commit (a staleness guard).
    DetailLoaded { hash: String, detail: CommitDetail },
    /// The changed-files tree finished loading for commit `hash`. `apply` swaps it
    /// into `repo.tree` ONLY when `hash` is still the selected commit (a staleness
    /// guard mirroring [`Msg::DetailLoaded`]); the swap re-flattens, clamps the
    /// files selection, and clears the now-stale preview.
    TreeLoaded {
        hash: String,
        tree: Vec<TreeNode>,
        /// Full paths in `tree` that match a `.gitignore` rule (All-files view only;
        /// empty for the changed-only / working trees). Stored into `repo.ignored`.
        ignored: std::collections::HashSet<String>,
    },
    /// A diff/source preview finished loading for a (commit, path). `apply` writes
    /// it ONLY when that pair is still the current selection (a staleness guard
    /// mirroring [`Msg::DetailLoaded`]); a `None` view clears to the empty viewer.
    PreviewLoaded {
        commit: String,
        path: String,
        view: Option<FileView>,
    },
    /// A read-only inspect overlay finished loading (Show Current Revision / a compared
    /// revision): show `view` under `title`. A `None` view means the path did not exist at
    /// that revision - surfaced as a Notice instead of opening an empty overlay. `path` echoes
    /// the request's target so a late reply that lands after the user navigated away is dropped
    /// instead of reopening the overlay over a different file (staleness guard).
    InspectLoaded {
        title: String,
        path: String,
        view: Option<FileView>,
    },
    /// The per-line blame for `path` at `rev` (the View > Blame gutter), fetched off-thread.
    /// Stored only if BOTH `rev` and `path` still match the selection: a path-only guard
    /// accepted a wrong-rev gutter when navigating commits while keeping the same file.
    BlameLoaded {
        rev: String,
        path: String,
        blame: crate::diff::BlameFile,
    },
    /// The periodic working-tree signature poll came back. `Some(sig)` differing from
    /// the loaded snapshot's signature = the tree/HEAD changed EXTERNALLY -> the repo
    /// refreshes so the files panel + log track it. `None` = the poll itself failed
    /// (e.g. a transient index.lock during an external git op): silently skipped BY
    /// DESIGN - the 2s cadence would spam `ReqFailed` notices, and the next tick
    /// self-heals.
    StatusPolled { sig: Option<u64> },
    /// A backend failure during the INITIAL load (boot over an empty shell); sets
    /// `Status::Error`, rendered by the log panel's empty-repo placeholder.
    BackendError(String),
    /// A per-request read/write failure on a POPULATED repo (`Status::Error` is only
    /// rendered over an empty one): surfaces as a transient Notice naming the op, so
    /// a failed load/save is never a silent no-op.
    ReqFailed {
        what: &'static str,
        error: String,
    },
}
