//! The state seam: every model change flows through [`AppState::apply`], the one
//! mutation entry point. It does ZERO IO and returns whether a redraw is needed,
//! so the loop can repaint only on real changes and a backend worker can push
//! data through the same door as keyboard input.

use crate::diff::{FileView, LineKind};
use crate::message::Msg;
use crate::model::{self, FlatKind, RepoModel, Status, TreeNode, WORKING_REV};
use crate::view_state::{
    ChoiceKind, CommitMenu, CommitMenuAction, DetailSel, Dialog, Divider, Effect, FilterKind,
    GitAction, InputKind, MenuAction, MenuId, Pane, RebaseAction, RefAction, RefMenu, RefMenuKind,
    ResetMode, ViewState, COPY_FIELDS,
};

/// Build the branch/tag fly-out rows for a commit's ref decorations: one [`RefMenu`] per
/// branch/tag (Head is skipped - it is not an independent ref), locality-shaped, with the
/// current branch flagged so its self-referential ops are dropped.
fn ref_menus_for(refs: &[model::Ref], current: Option<&str>) -> Vec<RefMenu> {
    refs.iter()
        .filter_map(|r| {
            let kind = match r.kind {
                model::RefKind::LocalBranch => RefMenuKind::LocalBranch,
                model::RefKind::RemoteBranch => RefMenuKind::RemoteBranch,
                model::RefKind::Tag => RefMenuKind::Tag,
                model::RefKind::Head => return None,
            };
            let is_current = kind == RefMenuKind::LocalBranch && Some(r.name.as_str()) == current;
            Some(RefMenu::new(r.name.clone(), kind, is_current))
        })
        .collect()
}

/// Advance a rebase row's action one step in the Pick -> Squash -> Fixup -> Drop -> Pick
/// cycle. Shared by the click ([`AppState::dialog_pick_row`]) and Space ([`AppState::
/// dialog_cycle_row`]); the p/s/f/d letter keys set a verb outright via `dialog_set_row`.
fn cycle_rebase_action(action: RebaseAction) -> RebaseAction {
    match action {
        RebaseAction::Pick => RebaseAction::Squash,
        RebaseAction::Squash => RebaseAction::Fixup,
        RebaseAction::Fixup => RebaseAction::Drop,
        RebaseAction::Drop => RebaseAction::Pick,
    }
}

/// Whether `msg` preserves the detail-pane text selection. It is kept by the messages
/// that build/extend/copy/scroll it AND by the background pushes that touch the tree /
/// preview / saved file but NOT the selected commit's detail (so a lazy tree/preview
/// load landing mid-drag does not wipe the selection). Genuine user navigation and the
/// detail-changing pushes (`DetailLoaded`, `RepoLoaded`) fall through and clear it - a
/// changed detail would otherwise leave the selection coordinates stale.
fn keeps_detail_sel(msg: &Msg) -> bool {
    matches!(
        msg,
        Msg::DetailSelectStart { .. }
            | Msg::DetailSelectTo { .. }
            | Msg::DetailSelectWord { .. }
            | Msg::CopyText(_)
            | Msg::ScrollDetail { .. }
            | Msg::TreeLoaded { .. }
            | Msg::PreviewLoaded { .. }
            | Msg::FileSaved { .. }
            | Msg::HunkReverted { .. }
            | Msg::PatchCopied { .. }
            | Msg::ReqFailed { .. }
    )
}

/// Whether `msg` preserves the read-only diff line selection. Kept by the messages that
/// build/extend/copy it and by the diff-body scrolls (so a wheel does not drop a live
/// selection), plus the background pushes that do not change the previewed diff. Genuine
/// navigation / edits / a new preview fall through and clear it (its line indices would
/// otherwise go stale against a different diff). Mirrors [`keeps_detail_sel`].
fn keeps_diff_sel(msg: &Msg) -> bool {
    matches!(
        msg,
        Msg::DiffSelectStart { .. }
            | Msg::DiffSelectTo { .. }
            | Msg::CopyText(_)
            | Msg::ScrollDiff { .. }
            | Msg::ScrollEdit { .. }
            | Msg::ScrollDiffH { .. }
            | Msg::TreeLoaded { .. }
            | Msg::FileSaved { .. }
            | Msg::HunkReverted { .. }
            | Msg::PatchCopied { .. }
            | Msg::ReqFailed { .. }
    )
}

/// Whole-application state: domain (`repo`) + ephemeral UI (`view`) + lifecycle.
pub struct AppState {
    pub repo: RepoModel,
    pub view: ViewState,
    pub status: Status,
    pub quit: bool,
}

impl AppState {
    /// Build state from an already-loaded repository model, with a fresh default
    /// view. The pure constructor (NO backend, NO fixtures): the composition root
    /// wires a backend into a `RepoModel` and the `RepoLoaded` handler and tests
    /// build through here. Detail follows the selection from the first frame via
    /// the cheap synchronous rebuild (B-3); preview starts empty (`None`) and is
    /// filled by a later [`Msg::PreviewLoaded`] (or, for the fixture startup path,
    /// by the composition root before the first draw).
    pub fn from_repo(repo: RepoModel) -> Self {
        let mut state = Self {
            repo,
            view: ViewState::new(0),
            status: Status::Ready,
            quit: false,
        };
        state.rebuild_detail();
        state
    }

    /// Preselect a files row and reset its viewer scroll. The composition root uses
    /// this to seed the default selection before resolving the startup preview; it
    /// is plain view-state mutation (ZERO IO), distinct from the `apply` path.
    pub fn preselect_file(&mut self, files_sel: usize) {
        let max = self.files_rows_len().saturating_sub(1);
        self.view.files_sel = files_sel.min(max);
        self.reset_diff_scroll();
    }

    /// Set the diff/source preview directly (composition-root startup seed). The
    /// runtime path uses [`Msg::PreviewLoaded`]; this is the one synchronous fill
    /// the fixture bootstrap performs before the first frame for byte-identity.
    pub fn set_preview(&mut self, preview: Option<FileView>) {
        self.repo.preview = preview;
    }

    /// Set the lifecycle status directly (composition-root startup seed). The
    /// non-blocking runtime starts `Loading` over an empty repo before the loader
    /// thread pushes the first [`Msg::RepoLoaded`]; the runtime path otherwise sets
    /// status through `apply` (`RepoLoaded` -> Ready, `BackendError` -> Error).
    pub fn set_status(&mut self, status: Status) {
        self.status = status;
    }

    /// Full repository path of the file under the current files selection, or
    /// `None` when a directory / out-of-range row is selected. Public so the
    /// composition root + the runtime's selection -> `Req` wiring can key the
    /// preview load. The FULL path (ancestor dirs joined) is what a real backend's
    /// `file_view` resolves; the fixture keys on the trailing component, unchanged.
    pub fn selected_file_path(&self) -> Option<String> {
        self.visible_files()
            .get(self.view.files_sel)
            .and_then(|(_, path)| path.clone())
    }

    /// The `FileView` the diff pane is currently showing: the transient read-only inspect
    /// overlay when one is open, otherwise the selected file's preview. Render + every diff
    /// hit-test reads through this so an open overlay masks the underlying preview the same way
    /// everywhere (click, scroll, copy).
    pub fn shown_view(&self) -> Option<&FileView> {
        match &self.view.inspect {
            Some(iv) => Some(&iv.view),
            None => self.repo.preview.as_ref(),
        }
    }

    /// The editable working buffer, but ONLY when no read-only inspect overlay masks it - so
    /// an open overlay freezes editing (clicks/keys/wheel do not reach the hidden buffer).
    pub fn live_editor(&self) -> Option<&crate::view_state::EditorState> {
        if self.view.inspect.is_some() {
            return None;
        }
        self.view.editor.as_ref().filter(|e| e.loaded)
    }

    /// The visible files rows (row + full file path) honoring the Flat toggle. The
    /// single source the store's index->path / row-kind lookups and the row count all
    /// read, so they agree with whatever the files panel renders.
    fn visible_files(&self) -> Vec<(model::FlatRow, Option<String>)> {
        model::visible_file_rows(&self.repo, &self.view)
    }

    /// Index of the first visible FILE row (skipping leading directories), or 0 when
    /// the tree has no files. Used to land the startup selection on a previewable file.
    fn first_file_row(&self) -> usize {
        self.visible_files()
            .iter()
            .position(|(r, _)| matches!(r.node, model::FlatKind::File { .. }))
            .unwrap_or(0)
    }

    /// The file paths a multi-select gesture at visible row `target` covers: a flat
    /// row (or a tree FILE row) is just its own file; a tree DIRECTORY row is every
    /// file beneath it (expand-state independent). The flat-aware bridge for marking.
    ///
    /// An active files-search forces a FLAT presentation (every visible row is a single
    /// matched file, regardless of the Flat toggle), so it resolves through the same
    /// flat branch - indexing the nested tree by row would desync from `visible_files`.
    fn paths_under(&self, target: usize) -> Vec<String> {
        if self.view.files_flat || !self.view.files_search.is_empty() {
            self.visible_files()
                .get(target)
                .and_then(|(_, p)| p.clone())
                .into_iter()
                .collect()
        } else {
            // A visible row maps 1:1 to its tree index (no synthetic root offset).
            TreeNode::file_paths_under(&self.repo.tree, target)
        }
    }

    /// Hash of the commit at the (filtered) log selection, if any. Public so the
    /// composition root can key the startup preview on the selected commit.
    pub fn selected_commit_hash(&self) -> Option<String> {
        let vis = model::visible_commits(&self.repo, &self.view);
        vis.get(self.view.log_sel)
            .and_then(|&idx| self.repo.commits.get(idx))
            .map(|c| c.hash.clone())
    }

    /// Apply one message; returns `true` if the frame must redraw. ZERO IO here.
    pub fn apply(&mut self, msg: Msg) -> bool {
        // The detail-pane text selection is a transient mouse artifact: any message
        // that does not build/extend/copy/scroll it clears it (so Ctrl+C only ever
        // targets a fresh selection and the band does not survive an unrelated action).
        // `cleared` ORs into the redraw result so a handler that reports no change still
        // repaints away the now-removed selection band.
        let cleared = self.view.detail_sel.is_some() && !keeps_detail_sel(&msg);
        if cleared {
            self.view.detail_sel = None;
        }
        // The read-only diff line selection is the same kind of transient artifact: drop
        // it on any message that does not build/extend/copy/scroll it (so the band never
        // outlives its diff). ORs into the redraw so the removed band is repainted away.
        let cleared_diff = self.view.diff_sel.is_some() && !keeps_diff_sel(&msg);
        if cleared_diff {
            self.view.diff_sel = None;
        }
        let redraw = match msg {
            Msg::Quit => {
                self.quit = true;
                false
            }
            Msg::ToggleFocus => {
                self.view.toggle_focus();
                true
            }
            Msg::Move(delta) => self.move_selection(delta),
            Msg::ToggleExpand => self.toggle_expand(),
            Msg::ToggleFlat => self.toggle_flat(),
            Msg::ToggleMark(row) => self.toggle_mark(row),
            Msg::SelectRange(row) => self.select_range(row),
            Msg::ClearMarks => self.clear_marks(),
            Msg::ToggleCommitMark(row) => self.toggle_commit_mark(row),
            Msg::SelectCommitRange(row) => self.select_commit_range(row),
            Msg::RequestRevert => self.request_revert(),
            Msg::RevertFile => self.request_revert_shown(),
            Msg::CancelRevert => self.cancel_revert(),
            Msg::ConfirmRevert => self.confirm_revert(),
            Msg::RevertDone { paths, summary } => self.revert_done(paths, summary),
            Msg::OpenCommit => self.open_input(InputKind::Commit, String::new(), None),
            Msg::OpenAmend => {
                let head = self.head_subject();
                self.open_input(InputKind::Amend, head, None)
            }
            Msg::OpenTag => self.open_input(InputKind::Tag, String::new(), None),
            Msg::RequestPush => self.open_confirm(GitAction::Push, "Push to the remote?"),
            Msg::RequestPull => self.open_pull_picker(),
            Msg::RequestUpdate => self.request_update(),
            Msg::OpenHelp => {
                self.close_popups();
                self.view.dialog = Some(Dialog::Help);
                true
            }
            Msg::LoadMore => {
                // Only meaningful when more history exists; a no-op otherwise (a stale row click).
                if self.repo.more_history && !self.view.effects.contains(&Effect::LoadMore) {
                    self.view.effects.push(Effect::LoadMore);
                    self.status = Status::Notice("Loading more history...".to_string());
                    true
                } else {
                    false
                }
            }
            Msg::OpenCopy => self.open_copy(),
            Msg::DialogInput(c) => self.dialog_input(c),
            Msg::DialogBackspace => self.dialog_backspace(),
            Msg::DialogDelete => self.dialog_delete(),
            Msg::DialogCaret { dir, select } => self.dialog_caret(dir, select),
            Msg::DialogSelectAll => self.dialog_select_all(),
            Msg::DialogCopy => self.dialog_copy(),
            Msg::DialogCut => self.dialog_cut(),
            Msg::DialogPaste => self.dialog_paste(),
            Msg::DialogToggleCheck => self.dialog_toggle_check(),
            Msg::DialogCycleArchiveFormat => self.cycle_archive_format(),
            Msg::DialogMove(delta) => self.dialog_move(delta),
            Msg::DialogCycleRow => self.dialog_cycle_row(),
            Msg::DialogSetRow(action) => self.dialog_set_row(action),
            Msg::DialogPickRow(i) => self.dialog_pick_row(i),
            Msg::DialogConfirm => self.dialog_confirm(),
            Msg::DialogCancel => self.dialog_cancel(),
            Msg::GitActionDone { summary, reload } => self.git_action_done(summary, reload),
            Msg::PatchCopied { text } => {
                // The loader produced the local-changes patch; hand the text to the runtime's
                // clipboard drain and report it in the one notice slot (bottom action bar).
                self.view.effects.push(Effect::Clipboard(text));
                self.status = Status::Notice("Copied patch to clipboard".to_string());
                true
            }
            Msg::OpenCommitMenu { index, col, row } => self.open_commit_menu(index, col, row),
            Msg::CloseCommitMenu => self.close_commit_menu(),
            Msg::CommitMenuPick(action) => self.commit_menu_pick(action),
            Msg::OpenFilesMenu { index, col, row } => self.open_files_menu(index, col, row),
            Msg::CloseFilesMenu => self.view.files_menu.take().is_some(),
            Msg::FilesMenuPick(action) => self.files_menu_pick(action),
            Msg::ScrollFilesMenu { offset } => match self.view.files_menu.as_mut() {
                Some(menu) if menu.scroll != offset => {
                    menu.scroll = offset;
                    true
                }
                _ => false,
            },
            Msg::OpenRefSubmenu { ref_idx } => self.open_ref_submenu(ref_idx),
            Msg::RefMenuPick { ref_idx, action } => self.ref_menu_pick(ref_idx, action),
            Msg::ScrollCommitMenu { offset } => match self.view.commit_menu.as_mut() {
                Some(menu) if menu.scroll != offset => {
                    menu.scroll = offset;
                    true
                }
                _ => false,
            },
            Msg::DetailSelectStart { row, col } => {
                self.view.detail_sel = Some(DetailSel { anchor: (row, col), cursor: (row, col) });
                true
            }
            Msg::DetailSelectTo { row, col } => match self.view.detail_sel.as_mut() {
                Some(sel) => {
                    sel.cursor = (row, col);
                    true
                }
                None => false,
            },
            Msg::DetailSelectWord { row, start, end } => {
                self.view.detail_sel =
                    Some(DetailSel { anchor: (row, start), cursor: (row, end) });
                true
            }
            Msg::DiffSelectStart { line, col } => {
                self.view.diff_sel = Some(DetailSel { anchor: (line, col), cursor: (line, col) });
                true
            }
            Msg::DiffSelectTo { line, col } => match self.view.diff_sel.as_mut() {
                Some(sel) => {
                    sel.cursor = (line, col);
                    true
                }
                None => false,
            },
            Msg::CopyText(text) => {
                self.view.effects.push(Effect::Clipboard(text));
                self.status = Status::Notice("Copied selection".to_string());
                true
            }
            Msg::ToggleWordWrap => {
                self.view.word_wrap = !self.view.word_wrap;
                // Wrapping changes per-row physical heights, so a free-scroll top no
                // longer maps cleanly - drop back to the cursor-follow scroll. Wrapping
                // ON also makes horizontal scroll meaningless, so clear it too.
                self.view.edit_scroll = None;
                self.view.diff_hscroll = None;
                true
            }
            Msg::ToggleDiff => {
                // An open inspect overlay IS the diff pane (it forced `show_diff` true and
                // stashed the user's prior). An explicit toggle takes direct control: DROP the
                // overlay and its captured prior (no restore - the toggle below, acting on the
                // currently-forced-true state, hides exactly what the user sees) so the prior
                // can't later override this toggle.
                if self.view.inspect.take().is_some() {
                    self.view.inspect_prior_show_diff = None;
                }
                self.view.show_diff = !self.view.show_diff;
                // Hiding the diff drops the diff-pane focus back to the log.
                if !self.view.show_diff && self.view.focus == Pane::Diff {
                    self.view.focus = Pane::Log;
                }
                true
            }
            Msg::ToggleAllFiles => {
                // ZERO-IO: flip the mode only. The runtime sees the change and
                // re-requests the current commit's tree (changed vs full); the new
                // tree arrives via Msg::TreeLoaded, which swaps + re-clamps + (in All
                // mode) collapses the dirs.
                self.view.show_all_files = !self.view.show_all_files;
                // The user took manual control of the All view, so a pending files-search
                // restore must not later override it.
                self.view.files_prev_all = None;
                true
            }
            Msg::FocusOpenFile => self.focus_open_file(),
            Msg::ToggleDiffMode => {
                self.view.toggle_diff_mode();
                // Side-by-side and unified index different row spaces (paired rows vs
                // raw lines) and different code widths, so neither a free-scroll top nor
                // a horizontal offset survives the flip.
                self.view.edit_scroll = None;
                self.view.diff_hscroll = None;
                true
            }
            Msg::ToggleWhitespace => {
                self.view.show_whitespace = !self.view.show_whitespace;
                // Whitespace markers can change wrapped heights; refollow the caret and
                // drop any horizontal offset (line widths shift with tab markers).
                self.view.edit_scroll = None;
                self.view.diff_hscroll = None;
                true
            }
            Msg::ToggleHideUnchanged => {
                self.view.hide_unchanged = !self.view.hide_unchanged;
                // Folding changes the row set, so a parked free-scroll top / horizontal
                // offset no longer maps - refollow from the top of the new layout.
                self.view.edit_scroll = None;
                self.view.diff_hscroll = None;
                self.view.diff_scroll = 0;
                if self.view.editor.is_some() {
                    // Editable `<current>`: rebuild the live diff in place (re-opening would
                    // discard unsaved edits); refresh_edit_preview folds per the new flag.
                    self.refresh_edit_preview();
                } else {
                    // Read-only commit / compare diff: re-fetch the FULL diff; the delivery
                    // path (apply_preview / inspect_loaded) folds it per the new flag.
                    self.view.effects.push(Effect::ReloadPreview);
                    // A transient read-only OVERLAY (Compare / Show Revision) masks repo.preview
                    // and is NOT re-fetched by Effect::ReloadPreview, so a global fold toggle
                    // would leave it stuck at its open-time fold. Close it - the user drops back
                    // to the underlying file diff, which folds per the new flag.
                    self.dismiss_inspect();
                }
                true
            }
            Msg::ToggleShowBlame => {
                self.view.show_blame = !self.view.show_blame;
                if self.view.show_blame {
                    // Fetch the blame for the open file (the runtime drains the effect).
                    self.view.effects.push(Effect::ReloadBlame);
                } else {
                    self.view.blame = None;
                }
                true
            }
            Msg::BlameLoaded { rev, path, blame } => {
                // Drop a stale reply by matching BOTH halves of the live selection: a
                // path-only guard accepted the wrong-rev gutter when the user navigated
                // commits while keeping the same file selected.
                if self.selected_commit_hash().as_deref() == Some(&rev)
                    && self.selected_file_path().as_deref() == Some(&path)
                {
                    self.view.blame = Some(blame);
                }
                true
            }
            Msg::ToggleAutosave => {
                self.view.autosave = !self.view.autosave;
                true
            }
            Msg::ToggleBranchList => {
                self.view.branches_expanded = !self.view.branches_expanded;
                // Collapsing shrinks the content; reset scroll so it cannot be
                // left parked past the now-shorter list.
                if !self.view.branches_expanded {
                    self.view.detail_scroll = 0;
                }
                true
            }
            Msg::SelectCommit(i) => self.select_commit(i, true),
            Msg::SelectFile(i) => self.select_file(i, false),
            Msg::ClickFile(i) => self.click_file(i),
            Msg::ScrollDiff { delta, pane_height } => self.scroll_diff(delta, pane_height),
            Msg::ScrollEdit { top } => self.set_free_scroll(|v| &mut v.edit_scroll, top),
            Msg::ScrollDiffH { offset } => self.set_free_scroll(|v| &mut v.diff_hscroll, offset),
            Msg::ScrollLog { offset } => {
                // Wheel-scrolling the log dismisses an open commit context menu (it would
                // otherwise float over an unrelated row), matching keyboard-move behavior.
                let closed = self.view.commit_menu.take().is_some();
                // The runtime computes a geometry-clamped offset; clamp again here so an
                // override can never park past the list (the stale-offset invariant).
                let top = offset.min(self.visible_len() + self.repo.more_history as usize);
                self.set_free_scroll(|v| &mut v.log_scroll, top) || closed
            }
            Msg::ScrollFiles { offset } => {
                let top = offset.min(self.files_rows_len());
                self.set_free_scroll(|v| &mut v.files_scroll, top)
            }
            Msg::ScrollDetail { delta, pane_height, content_height } => {
                self.scroll_detail(delta, pane_height, content_height)
            }
            Msg::SetSplit(d, frac) => self.view.set_split(d, frac),
            Msg::NudgeSplit(d, steps) => self.view.nudge_split(d, steps),
            Msg::NudgeFocusedVSplit(steps) => {
                let d = match self.view.focus {
                    Pane::Log => Divider::DiffLog,
                    Pane::Files => Divider::FilesDetail,
                    // The diff pane's vertical-position split is the diff/log divider.
                    Pane::Diff => Divider::DiffLog,
                };
                self.view.nudge_split(d, steps)
            }
            Msg::SearchFocus => self.set_search_active(true),
            Msg::SearchBlur { clear } => self.blur_search(clear),
            Msg::SearchPush(ch) => {
                self.view.search.push(ch);
                self.after_filter_change();
                true
            }
            Msg::SearchBackspace => {
                if self.view.search.pop().is_none() {
                    return false;
                }
                self.after_filter_change();
                true
            }
            Msg::SearchClear => self.clear_search(),
            Msg::ToggleSearchHistory => self.toggle_search_history(),
            Msg::PickSearchHistory(i) => self.pick_search_history(i),
            Msg::ToggleRegex => {
                self.view.regex_on = !self.view.regex_on;
                self.after_filter_change();
                true
            }
            Msg::FilesSearchFocus => self.set_files_search_active(true),
            Msg::FilesSearchBlur { clear } => self.blur_files_search(clear),
            Msg::FilesSearchPush(ch) => {
                self.view.files_search.push(ch);
                self.after_files_search_change();
                true
            }
            Msg::FilesSearchBackspace => {
                if self.view.files_search.pop().is_none() {
                    return false;
                }
                self.after_files_search_change();
                true
            }
            Msg::FilesSearchClear => self.clear_files_search(),
            Msg::ToggleFilesRegex => {
                self.view.files_regex_on = !self.view.files_regex_on;
                self.after_files_search_change();
                true
            }
            Msg::OpenDropdown(kind) => self.open_dropdown(kind),
            Msg::CloseDropdown => self.close_dropdown(),
            Msg::DropdownMove(delta) => self.dropdown_move(delta),
            Msg::DropdownPick(row) => self.dropdown_pick(row),
            Msg::OpenMenu(id) => self.open_menu(id),
            Msg::CloseMenu => self.close_menu(),
            Msg::MenuPick(action) => self.menu_pick(action),
            Msg::SaveEditor => self.save_editor(),
            Msg::DiffBlur => self.diff_blur(),
            Msg::Edit(op) => self.edit(op),
            Msg::EditFileLoaded { commit, path, base, work } => {
                self.edit_file_loaded(&commit, &path, base.as_deref(), &work)
            }
            Msg::FileSaved { path } => self.file_saved(&path),
            Msg::RevertHunk => self.request_hunk_revert(),
            Msg::HunkReverted { summary } => {
                self.status = Status::Notice(summary);
                // The working file changed on disk but the selection did not move, so
                // force a re-open: the runtime drops the cached file and re-fires
                // `OpenFile`, refreshing the diff to show the reverted hunk. Without this
                // the drain branch is unreachable and the stale pre-revert diff lingers.
                self.view.effects.push(Effect::ReloadPreview);
                true
            }
            Msg::RepoLoaded(repo) => {
                // First real load (the prior repo was the empty loading shell): land the
                // files selection on the first FILE row, skipping leading directories, so
                // the startup preview shows a diff rather than a folder ("no selection").
                // A later reload (--watch / post-write) keeps the user's selection.
                let first_load = self.repo.commits.is_empty();
                // Re-anchor the log selection to the SAME commit across a reload: the
                // synthetic "<current>" row appearing/disappearing (tree went dirty/clean)
                // shifts every real commit by one index, so a pure clamp would silently
                // jump the selection. Pin by hash; fall back to the clamp if it is gone.
                let prev_hash = (!first_load).then(|| self.selected_commit_hash()).flatten();
                // `snapshot.tree` is ALWAYS the `<current>` working tree. With a
                // HISTORICAL commit selected, swapping it in would flip the files panel
                // to the wrong file set until the re-fetch below lands - so keep the
                // displayed tree across the swap. (The synthetic `<current>` row always
                // exists, so a working-row selection never needs this.)
                let keep_tree = (!first_load
                    && self.selected_commit().is_some_and(|c| !c.is_working))
                .then(|| {
                    (
                        std::mem::take(&mut self.repo.tree),
                        std::mem::take(&mut self.repo.ignored),
                    )
                });
                self.repo = *repo;
                if let Some((tree, ignored)) = keep_tree {
                    self.repo.tree = tree;
                    self.repo.ignored = ignored;
                }
                // A git-action reload lands here: keep its result notice (parked before the
                // reload) instead of the bare Ready, so the user actually sees the outcome.
                // Mark it sticky so the auto-open of the newly selected file (edit_file_loaded)
                // does not immediately clear it.
                self.status = match self.view.parked_notice.take() {
                    Some(note) => {
                        self.view.notice_sticky = true;
                        Status::Notice(note)
                    }
                    None => {
                        // A follow-up plain reload (no pending notice) bounds the flag's life to
                        // the notice it guarded - else it lingers true until the next genuine nav.
                        self.view.notice_sticky = false;
                        Status::Ready
                    }
                };
                if first_load {
                    self.view.files_sel = self.first_file_row();
                } else if let Some(hash) = prev_hash {
                    let vis = model::visible_commits(&self.repo, &self.view);
                    if let Some(pos) = vis
                        .iter()
                        .position(|&i| self.repo.commits[i].hash == hash)
                    {
                        self.view.log_sel = pos;
                    }
                }
                self.clamp_selections();
                // A fresh repo has different files: the path-keyed marks are stale.
                self.view.files_marked.clear();
                match &self.view.editor {
                    // An in-flight reload landing under an OPEN editor (the watch tick
                    // skips sending while one is open, but a reload may already be in
                    // flight, and a post-write reload always lands here): keep the
                    // buffer's live diff instead of nulling the preview into a stuck
                    // "Loading diff...", and re-anchor the file by PATH once the new
                    // tree arrives (the reload can shift rows under the caret).
                    Some(editor) => {
                        self.view.parked_file_path = Some(editor.path.clone());
                        self.refresh_edit_preview();
                    }
                    // Preview is loaded out-of-band (Msg::PreviewLoaded); clear it so a
                    // stale preview from the old repo cannot linger. Detail uses the
                    // cheap synchronous rebuild (B-3) so the panel never flashes empty.
                    // With a file still selected the cleared view MUST be re-fetched,
                    // or a refresh that keeps the selection (status poll / --watch)
                    // strands the viewer on "Loading diff..." forever.
                    None => {
                        self.repo.preview = None;
                        if !first_load && self.selected_file_path().is_some() {
                            self.view.effects.push(Effect::ReloadPreview);
                        }
                    }
                }
                // Re-fetch the selected commit's tree: the swap above only carried the
                // `<current>` working tree, so a historical selection needs its own tree
                // refreshed (and the `<current>` re-fetch re-lands `apply_tree`, which
                // consumes the parked editor-path reveal). The detail re-fetch restores
                // the async enrichment (committer email, branches) the cheap
                // `rebuild_detail` below cannot supply. Redundant-but-harmless on the
                // full `ReloadRepo` path (the same cycle's requests absorb them).
                if !first_load {
                    self.view.effects.push(Effect::ReloadTree);
                    self.view.effects.push(Effect::ReloadDetail);
                }
                self.rebuild_detail();
                true
            }
            Msg::StatusPolled { sig } => {
                // A changed signature = the working tree / HEAD moved EXTERNALLY:
                // refresh the repo WITHOUT resetting the runtime's selection cache
                // (`RefreshRepo`, not `ReloadRepo`) so an open editor buffer survives.
                // A failed poll (`None`) and a poll racing the first load are no-ops;
                // an unchanged signature is the common idle tick.
                match (sig, self.repo.status_sig) {
                    (Some(new), Some(cur)) if new != cur => {
                        self.view.effects.push(Effect::RefreshRepo);
                        true
                    }
                    _ => false,
                }
            }
            Msg::DetailLoaded { hash, detail } => {
                // Apply only if it is still the selected (visible) commit.
                if self.selected_commit_hash().as_deref() == Some(&hash) {
                    self.repo.detail = Some(detail);
                    true
                } else {
                    false
                }
            }
            Msg::TreeLoaded { hash, tree, ignored } => self.apply_tree(&hash, tree, ignored),
            Msg::PreviewLoaded {
                commit,
                path,
                view,
            } => self.apply_preview(&commit, &path, view),
            Msg::InspectLoaded { title, path, view } => self.inspect_loaded(title, &path, view),
            Msg::CloseInspect => self.close_inspect(),
            // The dialog-opening reads echo the navigation epoch stamped at request time:
            // a reply from before the latest navigation is dropped here, uniformly, so a
            // slow read can never pop a modal long after the user moved on.
            Msg::PickListLoaded { kind, path, items, mode, epoch } => {
                epoch == self.view.nav_epoch && self.pick_list_loaded(kind, path, items, mode)
            }
            Msg::RefListLoaded { op, items, epoch } => {
                epoch == self.view.nav_epoch && self.ref_list_loaded(op, items)
            }
            Msg::RemotesLoaded { remotes, epoch } => {
                epoch == self.view.nav_epoch && self.remotes_loaded(remotes)
            }
            Msg::RemoteAddInput => self.open_remote_add(),
            Msg::RemoteRemove => self.open_remote_remove(),
            Msg::BackendError(message) => {
                self.status = Status::Error(message);
                true
            }
            // A per-request failure on a populated repo: `Status::Error` is only rendered
            // by the empty-repo placeholder, so surface it as the transient Notice (the
            // log-toolbar strip) - a failed load/save must never be a silent no-op.
            Msg::ReqFailed { what, error } => {
                self.view.notice_sticky = false;
                self.status = Status::Notice(format!("{what} failed: {error}"));
                true
            }
        };
        #[cfg(any(test, debug_assertions))]
        self.check_invariants();
        redraw || cleared || cleared_diff
    }

    /// Machine-checked consistency of the whole state, run after EVERY apply in
    /// debug/test builds. Each assert is one documented cross-field rule; a violation
    /// reachable through real input becomes a deterministic test failure (with the
    /// offending Msg on the stack) instead of an in-app glitch. Release builds skip it.
    #[cfg(any(test, debug_assertions))]
    fn check_invariants(&self) {
        let vis = self.visible_len();
        debug_assert!(
            vis == 0 || self.view.log_sel < vis + self.repo.more_history as usize,
            "log_sel {} out of bounds (visible {})",
            self.view.log_sel,
            vis
        );
        let rows = self.files_rows_len();
        debug_assert!(
            rows == 0 || self.view.files_sel < rows,
            "files_sel {} out of bounds (rows {})",
            self.view.files_sel,
            rows
        );
        // Only the synthetic working row is editable (product rule: routing in open_file).
        debug_assert!(
            self.view.editor.is_none() || self.selected_commit().is_none_or(|c| c.is_working),
            "an editable buffer is open on a non-working commit"
        );
        // The read-only char selection never coexists with a visible editable buffer.
        debug_assert!(
            self.view.diff_sel.is_none() || self.live_editor().is_none(),
            "diff_sel set while the editable buffer is live"
        );
        // A free-scroll override never points past its list (stale-offset rule).
        debug_assert!(
            self.view.log_scroll.is_none_or(|top| top <= vis + self.repo.more_history as usize),
            "log_scroll parked past the list"
        );
        debug_assert!(
            self.view.files_scroll.is_none_or(|top| top <= rows),
            "files_scroll parked past the list"
        );
        // A fully-modal dialog excludes the lighter popups (menus close on open).
        debug_assert!(
            self.view.dialog.is_none()
                || (!self.view.search_history_open && self.view.open_dropdown.is_none()),
            "a modal dialog is open under a live popup"
        );
    }

    /// Move selection in the focused pane by `delta` (clamped). Both panes route
    /// through their respective select home (`select_commit`/`select_file`), so a
    /// keyboard move rebuilds the detail (Log) or preview (Files) exactly like a
    /// click. The log selection indexes the FILTERED list (bound = visible count).
    fn move_selection(&mut self, delta: isize) -> bool {
        match self.view.focus {
            Pane::Log => {
                let next = step(self.view.log_sel, delta, self.visible_len());
                self.select_commit(next, false)
            }
            Pane::Files => {
                let next = step(self.view.files_sel, delta, self.files_rows_len());
                self.select_file(next, false)
            }
            Pane::Diff => {
                let next = self.step_diff_cursor(delta);
                let changed = next != self.view.diff_cursor || self.view.hunk_revert_armed;
                self.view.diff_cursor = next;
                self.view.hunk_revert_armed = false; // moving disarms a pending revert
                changed
            }
        }
    }

    /// Number of logical lines in the previewed diff (the diff-cursor bound). Zero
    /// when the preview is absent or not a diff.
    fn diff_line_count(&self) -> usize {
        match self.repo.preview.as_ref() {
            Some(FileView::Diff(d)) => d.lines.len(),
            _ => 0,
        }
    }

    /// Step the diff cursor by `delta`, then skip past any synthetic fold-marker rows in
    /// the travel direction so the cursor never lands on a "N unchanged lines" stand-in
    /// (it is not a real source line). If only markers lie ahead (a boundary), the cursor
    /// stays put rather than resting on one.
    fn step_diff_cursor(&self, delta: isize) -> usize {
        let n = self.diff_line_count();
        let mut next = step(self.view.diff_cursor, delta, n);
        if let Some(FileView::Diff(d)) = self.repo.preview.as_ref() {
            let dir = if delta >= 0 { 1 } else { -1 };
            while d.lines.get(next).is_some_and(|l| l.fold.is_some()) {
                let after = step(next, dir, n);
                if after == next {
                    return self.view.diff_cursor.min(n.saturating_sub(1));
                }
                next = after;
            }
        }
        next
    }

    /// Clamp the diff cursor into range and nudge it forward off a leading fold marker, so
    /// a freshly-opened read-only diff (whose first hunk may start below line 1) never rests
    /// the cursor on a synthetic marker. Forward-only: a marker is always followed by a real
    /// line (the hunk it precedes), so this terminates on a browsable line.
    fn normalize_diff_cursor(&mut self) {
        let n = self.diff_line_count();
        if n == 0 {
            return;
        }
        let mut c = self.view.diff_cursor.min(n - 1);
        if let Some(FileView::Diff(d)) = self.repo.preview.as_ref() {
            while c + 1 < n && d.lines.get(c).is_some_and(|l| l.fold.is_some()) {
                c += 1;
            }
        }
        self.view.diff_cursor = c;
    }

    /// Expand/collapse the foldable row at visible files row `target`, then re-flatten and
    /// clamp the selection into the now shorter/longer pane. A visible row maps 1:1 to its
    /// tree index (no synthetic root offset). A FILE row does nothing. Returns whether a row
    /// toggled.
    fn toggle_dir(&mut self, target: usize) -> bool {
        if !TreeNode::toggle_visible(&mut self.repo.tree, target) {
            return false;
        }
        let max = self.files_rows_len().saturating_sub(1);
        self.view.files_sel = self.view.files_sel.min(max);
        // Expand/collapse changes the row count: drop any wheel free-scroll so the
        // selection re-follows into view (a stale offset could blank/mis-map the panel).
        self.view.files_scroll = None;
        // The list reflowed: dismiss any files context menu floating over it.
        self.view.files_menu = None;
        true
    }

    /// Toggle the files pane between the nested tree and the flat (no-folder) list.
    /// Preserves the SELECTED FILE across the flip by re-finding its path in the new
    /// row order (marks are path-keyed, so they survive automatically); falls back to
    /// a clamped index when the path is absent. Pure view transform - no reload.
    fn toggle_flat(&mut self) -> bool {
        let selected_path = self.selected_file_path();
        self.view.files_flat = !self.view.files_flat;
        let rows = self.visible_files();
        let new_sel = selected_path
            .as_ref()
            .and_then(|p| rows.iter().position(|(_, path)| path.as_ref() == Some(p)))
            .unwrap_or_else(|| self.view.files_sel.min(rows.len().saturating_sub(1)));
        self.view.files_sel = new_sel;
        // Flat<->nested have different row counts: drop the wheel free-scroll so the
        // re-followed selection snaps back into view, mirroring the other choke points.
        self.view.files_scroll = None;
        // The list reflowed: dismiss any files context menu floating over it.
        self.view.files_menu = None;
        true
    }

    /// Reveal the opened diff file in the files list (the files-toolbar focus button). In
    /// the full-tree view, first unfold every directory that holds a changed file so the
    /// whole changed set - including the opened file - is visible; then re-find the opened
    /// file's row (expansion shifts indices) and drop the wheel free-scroll so the
    /// selection-follow scroll snaps it into view. Pure view transform - no reload.
    fn focus_open_file(&mut self) -> bool {
        // The opened file is identified by the PREVIEW/editor path, which persists even
        // when its directory is collapsed (so `selected_file_path` would see nothing).
        let opened = self.opened_file_path();
        // Unfold every directory that holds a change so the opened file (and the whole
        // changed set) is reachable - in BOTH modes: a dir collapsed by hand in the
        // changed-only view would otherwise hide the opened file and the button would
        // silently no-op. The changed-only tree is all-changed, so this expands all of it
        // there; the full-tree view expands only the dirs that contain a change.
        TreeNode::expand_changed_dirs(&mut self.repo.tree);
        // Re-locate the opened file by PATH (indices move when dirs unfold) so the
        // selection points at it, then let selection-follow reveal it.
        if let Some(path) = &opened {
            let rows = self.visible_files();
            if let Some(i) = rows.iter().position(|(_, p)| p.as_ref() == Some(path)) {
                self.view.files_sel = i;
            }
        }
        self.view.focus = Pane::Files;
        self.view.files_scroll = None;
        true
    }

    /// Re-select the file at `path` in the freshly-loaded files tree (Show History navigates to
    /// a commit, then reveals the file it was opened on). Unfolds the dirs holding changes so the
    /// path is reachable, then locates it by path (indices shift as dirs unfold). Leaves the focus
    /// where the caller put it (the log, for a Show History navigation). A no-op if the path is
    /// gone from this commit's tree.
    fn reveal_file_by_path(&mut self, path: &str) -> bool {
        TreeNode::expand_changed_dirs(&mut self.repo.tree);
        let rows = self.visible_files();
        if let Some(i) = rows.iter().position(|(_, p)| p.as_deref() == Some(path)) {
            self.view.files_sel = i;
            self.view.files_scroll = None; // refollow so the revealed file scrolls into view
            true
        } else {
            false
        }
    }

    /// Full path of the file whose diff is currently shown: the live editor's path, else
    /// the read-only preview's, else the file under the current selection. The preview /
    /// editor path survives a directory collapse, so the focus button can still find it.
    fn opened_file_path(&self) -> Option<String> {
        if let Some(e) = &self.view.editor {
            return Some(e.path.clone());
        }
        match &self.repo.preview {
            Some(FileView::Diff(d)) => Some(d.path.clone()),
            Some(FileView::Source(s)) => Some(s.path.clone()),
            Some(FileView::Binary(b)) => Some(b.path.clone()),
            Some(FileView::Blame(b)) => Some(b.path.clone()),
            None => self.selected_file_path(),
        }
    }

    /// Expand/collapse the selected directory row in the files pane (keyboard). Flat mode has
    /// no directory rows, so the fold is inert there.
    fn toggle_expand(&mut self) -> bool {
        if self.view.focus != Pane::Files || self.view.files_flat {
            return false;
        }
        self.toggle_dir(self.view.files_sel)
    }

    /// Select the commit at `i` (an index into the FILTERED list): focus Log, set
    /// selection, rebuild the detail panel from the resolved commit. `keep_scroll` is
    /// set by a MOUSE click (the row is visible by construction, so the parked wheel
    /// free-scroll is preserved - no jump); keyboard / programmatic moves clear it so
    /// the selection snaps back into view.
    fn select_commit(&mut self, i: usize, keep_scroll: bool) -> bool {
        self.view.nav_epoch += 1; // navigation: a pre-move dialog-opening read is now stale
        // Any genuine navigation dismisses a read-only inspect overlay (its rev/path no longer
        // matches what is selected) and restores the user's `show_diff` toggle.
        self.dismiss_inspect();
        // A competing navigation invalidates a parked Show-History reveal (it targeted the tree
        // this move replaces). `show_history_revision`/`show_current_revision` re-set these AFTER
        // their own select_commit.
        self.view.parked_file_path = None;
        self.view.parked_revision = None;
        let max = self.visible_len().saturating_sub(1);
        let next = i.min(max);
        let moved = next != self.view.log_sel;
        // Selecting refollows the log: drop any wheel free-scroll so the selection is
        // brought back into view (and force a repaint if a live override was cleared).
        // A mouse click keeps the viewport pinned - it clicked a row already on screen.
        let had_free_scroll = !keep_scroll && self.view.log_scroll.take().is_some();
        // A keyboard / programmatic move dismisses an open commit context menu (a
        // mouse click that keeps the viewport leaves it for the click resolver). The
        // files context menu closes too - moving commits reloads the file list it floats over.
        let had_menu = !keep_scroll
            && (self.view.commit_menu.take().is_some() | self.view.files_menu.take().is_some());
        let changed = moved || self.view.focus != Pane::Log || self.blur_searches();
        self.view.focus = Pane::Log;
        self.view.log_sel = next;
        // A plain select drops the multi-commit selection (the cursor IS the selection now);
        // the Ctrl/Shift-click handlers save + restore the set around their own select call.
        self.view.commits_marked.clear();
        if changed {
            self.rebuild_detail();
        }
        // Moving to a different commit selects a different file set + a new diff base;
        // the new tree/diff arrive async, but clear eagerly so a stale mark or buffer
        // never flashes against the old commit before the swap. A dirty buffer is
        // autosaved first so changing commits never loses edits.
        if moved {
            self.view.files_marked.clear();
            self.flush_editor_if_dirty();
            self.warn_if_dropping_unsaved();
            self.view.editor = None;
            self.repo.preview = None;
            self.view.blame = None;
            self.view.diff_full_width = false;
            // A commit move swaps the whole diff underneath: a parked diff scroll /
            // hscroll would reopen the NEW commit's first file at the OLD offset.
            // (Caught by navigation_choke_points_reset_their_transients - the missed
            // reset was exactly the pin-feature bug class.)
            self.reset_diff_scroll();
            // Park the file selection on the repo-root row (no file) until the new commit's tree
            // loads: the carried index points into the OLD tree, so leaving it would make the
            // runtime open the WRONG file (the carried path AT the new commit) before `apply_tree`
            // can land on the commit's first file. Row 0 yields no file -> no stale OpenFile fires.
            self.view.files_sel = 0;
            // Genuine log navigation un-sticks a git-action notice (see select_file).
            self.view.notice_sticky = false;
        }
        changed || had_free_scroll || had_menu
    }

    /// Select the file row at `i`: focus Files, set selection, reset scroll, and
    /// CLEAR the preview (ZERO IO - no backend call in `apply`). The runtime sees
    /// the new selection after `apply` returns and asks the loader for the preview,
    /// which arrives asynchronously via [`Msg::PreviewLoaded`]. Pure navigation -
    /// never folds a directory (used by the keyboard move + the wheel). `keep_scroll`
    /// (a MOUSE click) preserves the parked wheel free-scroll - the clicked row is
    /// already on screen, so the viewport must not jump; keyboard / programmatic moves
    /// clear it so the selection snaps back into view.
    fn select_file(&mut self, i: usize, keep_scroll: bool) -> bool {
        self.view.nav_epoch += 1; // navigation: a pre-move dialog-opening read is now stale
        // Navigating to a file dismisses a read-only inspect overlay (it floats over the diff
        // pane this is about to repopulate). `open_files_menu` calls this before opening a new
        // menu, so a right-click on another file also closes a stale overlay.
        self.dismiss_inspect();
        let max = self.files_rows_len().saturating_sub(1);
        let next = i.min(max);
        let row_moved = next != self.view.files_sel;
        let focus_changed = self.view.focus != Pane::Files;
        // Selecting refollows the files list: drop any wheel free-scroll (repaint forced
        // below if it cleared a live override). A mouse click keeps the viewport pinned.
        let had_free_scroll = !keep_scroll && self.view.files_scroll.take().is_some();
        // Only a genuine FILE change resets the editable buffer + preview (the runtime
        // re-requests them); a same-file focus regain keeps the live buffer intact.
        if row_moved {
            self.flush_editor_if_dirty();
            self.warn_if_dropping_unsaved();
            self.view.editor = None;
            self.repo.preview = None;
            self.view.blame = None; // the new file's blame is re-fetched by the runtime
            self.view.diff_full_width = false;
            self.reset_diff_scroll();
            // Genuine navigation un-sticks a git-action notice: the file this opens may clear it.
            self.view.notice_sticky = false;
        }
        self.view.focus = Pane::Files;
        self.view.files_sel = next;
        // A plain (single) select drops the multi-selection: the cursor IS the
        // selection now. This anchors the next range select at `files_sel`, and
        // keeps the default flow (and the golden render) unchanged with no marks.
        let had_marks = !self.view.files_marked.is_empty();
        self.view.files_marked.clear();
        // A selection move closes any open files context menu (it would otherwise float
        // over an unrelated row). `open_files_menu` re-sets it AFTER calling this.
        let had_files_menu = self.view.files_menu.take().is_some();
        // Selecting a file blurs any active search field so the next keystroke acts on the pane.
        let blurred = self.blur_searches();
        row_moved || focus_changed || had_marks || had_free_scroll || had_files_menu || blurred
    }

    /// Click the files row at `i` (mouse): select it (JetBrains-style), and if it
    /// is a directory, also toggle its expanded state. File rows behave exactly
    /// like [`Self::select_file`]; dir rows additionally fold/unfold then re-clamp.
    fn click_file(&mut self, i: usize) -> bool {
        // A mouse click on a visible row keeps the parked viewport (no jump). When the
        // row is a directory, the fold changes the row count, so toggle_dir then clears
        // the override (the re-clamped selection must re-follow into view).
        let selected = self.select_file(i, true);
        let toggled = self.row_is_foldable(self.view.files_sel) && self.toggle_dir(self.view.files_sel);
        selected || toggled
    }

    /// Whether the visible files row at `idx` is a directory (always false in flat
    /// mode, which has no directory rows). The repo-ROOT row is NOT a directory. Test-only:
    /// the click path uses [`Self::row_is_foldable`] (which also folds the root).
    #[cfg(test)]
    fn row_is_dir(&self, idx: usize) -> bool {
        matches!(
            self.visible_files().get(idx).map(|(r, _)| &r.node),
            Some(FlatKind::Dir { .. })
        )
    }

    /// Whether the visible files row at `idx` folds on click: a directory row.
    fn row_is_foldable(&self, idx: usize) -> bool {
        matches!(
            self.visible_files().get(idx).map(|(r, _)| &r.node),
            Some(FlatKind::Dir { .. })
        )
    }

    /// Toggle the file(s) at row `i` in/out of the multi-selection set (Space /
    /// Ctrl-click): a FILE row toggles itself; a DIRECTORY row marks all its
    /// descendant files (or unmarks them if all are already marked). The cursor
    /// moves to `i` and focuses Files so the preview follows it. ZERO IO. Toggling
    /// does NOT clear the set (it is the additive multi-select gesture).
    fn toggle_mark(&mut self, i: usize) -> bool {
        let max = self.files_rows_len().saturating_sub(1);
        let target = i.min(max);
        let paths = self.paths_under(target);
        if paths.is_empty() {
            // A row with no files under it (shouldn't happen for a changed tree);
            // still move the cursor so the gesture is not silently inert.
            return self.move_cursor_to(target);
        }
        // If every target path is already marked, the gesture unmarks them; else it
        // marks the missing ones (so a dir click marks the whole subtree at once).
        let all_marked = paths.iter().all(|p| self.view.files_marked.contains(p));
        for p in &paths {
            if all_marked {
                self.view.files_marked.remove(p);
            } else {
                self.view.files_marked.insert(p.clone());
            }
        }
        self.move_cursor_to(target);
        true
    }

    /// Range-select from the anchor (the current cursor) to row `i` inclusive,
    /// marking every FILE row in the span; the cursor then moves to `i`
    /// (Shift-click / Shift+arrow). Additive over any existing marks. ZERO IO.
    fn select_range(&mut self, i: usize) -> bool {
        let max = self.files_rows_len().saturating_sub(1);
        let target = i.min(max);
        let anchor = self.view.files_sel.min(max);
        let (lo, hi) = (anchor.min(target), anchor.max(target));
        let visible = self.visible_files();
        for (row, (_, path)) in visible.iter().enumerate() {
            if row < lo || row > hi {
                continue;
            }
            match path {
                // A FILE row marks itself.
                Some(p) => {
                    self.view.files_marked.insert(p.clone());
                }
                // A DIRECTORY row (no own path) inside the span marks every file
                // under it - including files hidden inside a COLLAPSED dir - so a
                // range that visually covers a folder includes its files, matching
                // Space/Ctrl-click on a dir (file_paths_under ignores expand state).
                None => {
                    for p in self.paths_under(row) {
                        self.view.files_marked.insert(p);
                    }
                }
            }
        }
        self.move_cursor_to(target);
        true
    }

    /// Move the cursor to row `target`, focus Files, reset diff scroll, and clear
    /// the preview so the runtime reloads it for the new cursor file. Shared by the
    /// multi-select gestures (which, unlike `select_file`, must NOT clear the set).
    fn move_cursor_to(&mut self, target: usize) -> bool {
        self.view.nav_epoch += 1; // navigation: a pre-move dialog-opening read is now stale
        self.dismiss_inspect(); // a cursor jump is navigation: drop a read-only overlay
        self.view.focus = Pane::Files;
        self.view.files_sel = target;
        self.view.files_scroll = None;
        self.reset_diff_scroll();
        self.repo.preview = None;
        self.view.files_menu = None; // the selection moved; drop any floating files menu
        true
    }

    /// Clear the multi-selection set (Esc). Returns whether anything was cleared.
    fn clear_marks(&mut self) -> bool {
        if self.view.files_marked.is_empty() && self.view.commits_marked.is_empty() {
            return false;
        }
        self.view.files_marked.clear();
        self.view.commits_marked.clear();
        true
    }

    /// Toggle the commit at visible log row `i` in/out of the multi-commit selection (Ctrl-click).
    /// The synthetic `<current>` row has no hash to act on, so it just selects. Saves + restores
    /// the mark set around `select_commit` (which clears it) so the cursor moves + detail updates.
    fn toggle_commit_mark(&mut self, i: usize) -> bool {
        let vis = model::visible_commits(&self.repo, &self.view);
        let Some(&ci) = vis.get(i) else { return false };
        let c = &self.repo.commits[ci];
        if c.is_working {
            return self.select_commit(i, true);
        }
        let hash = c.hash.clone();
        let mut marks = self.view.commits_marked.clone();
        if !marks.remove(&hash) {
            marks.insert(hash);
        }
        self.select_commit(i, true);
        self.view.commits_marked = marks;
        true
    }

    /// Range-mark commits from the cursor (`log_sel`) to visible row `i` inclusive (Shift-click),
    /// marking each REAL commit's hash (the `<current>` row is skipped). Additive over any
    /// existing marks; the cursor moves to `i`.
    fn select_commit_range(&mut self, i: usize) -> bool {
        let vis = model::visible_commits(&self.repo, &self.view);
        let max = vis.len().saturating_sub(1);
        let target = i.min(max);
        let anchor = self.view.log_sel.min(max);
        let (lo, hi) = (anchor.min(target), anchor.max(target));
        let mut marks = self.view.commits_marked.clone();
        for &ci in &vis[lo..=hi] {
            let c = &self.repo.commits[ci];
            if !c.is_working {
                marks.insert(c.hash.clone());
            }
        }
        self.select_commit(target, true);
        self.view.commits_marked = marks;
        true
    }

    /// The target file paths for a revert: the marked set if non-empty, else the
    /// single cursor file (so Revert works with or without a multi-selection). An
    /// out-of-range / directory cursor with no marks yields an empty list.
    ///
    /// UNCHANGED rows are excluded: in the All-files view the tree carries the full
    /// file tree (Unchanged rows included), and reverting an unchanged file is a
    /// meaningless no-op that would still prune the row from the view. The
    /// changed-only tree has no Unchanged rows, so this filter is a no-op there.
    fn revert_target_paths(&self) -> Vec<String> {
        let changed = TreeNode::changed_paths(&self.repo.tree);
        if !self.view.files_marked.is_empty() {
            return self
                .view
                .files_marked
                .iter()
                .filter(|p| changed.contains(*p))
                .cloned()
                .collect();
        }
        self.selected_file_path()
            .filter(|p| changed.contains(p))
            .into_iter()
            .collect()
    }

    // -- repo-level git ops (top Git menu): dialogs + git actions ------------

    /// HEAD's commit subject (the row just below the synthetic "<current>", else the
    /// top row), to prefill the Amend dialog. Empty when there is no real commit.
    fn head_subject(&self) -> String {
        self.repo
            .commits
            .iter()
            .find(|c| !c.is_working)
            .map(model::commit_subject)
            .unwrap_or_default()
    }

    /// Close any open top-menu / filter dropdown so only the modal dialog shows (a
    /// dialog can be opened via an Alt accelerator while a menu is open). Mirrors the
    /// existing menu/dropdown mutual exclusion.
    fn close_popups(&mut self) {
        self.view.open_menu = None;
        self.view.open_dropdown = None;
        self.view.commit_menu = None;
        self.view.files_menu = None;
        self.view.search_history_open = false;
        self.view.files_search_active = false;
    }

    /// Open a HEAD/upstream text-input dialog (commit/amend/tag) with `initial` text.
    fn open_input(&mut self, kind: InputKind, initial: String, commit: Option<String>) -> bool {
        self.open_input_full(kind, initial, commit, None, None)
    }

    /// Open an input dialog with the full field set: editable `initial` text, an
    /// optional target `commit`, an optional dim `note` (warning), and an optional
    /// `checkbox` `(label, checked)`.
    fn open_input_full(
        &mut self,
        kind: InputKind,
        initial: String,
        commit: Option<String>,
        note: Option<String>,
        checkbox: Option<(String, bool)>,
    ) -> bool {
        self.close_popups();
        self.view.dialog = Some(Dialog::Input {
            kind,
            field: crate::view_state::TextField::new(initial),
            commit,
            note,
            checkbox,
        });
        true
    }

    /// Open the New Branch dialog for the selected commit (snapshotting its hash), with
    /// the "Checkout new branch" checkbox on by default. Hints on the working row.
    fn open_branch_input(&mut self) -> bool {
        let Some(commit) = self.selected_real_commit().map(|c| c.full_hash.clone()) else {
            self.status = Status::Notice("Select a commit".to_string());
            return true;
        };
        self.open_input_full(
            InputKind::NewBranch,
            String::new(),
            Some(commit),
            None,
            Some(("Checkout new branch".to_string(), true)),
        )
    }

    /// Open the New Tag dialog for the selected commit (snapshotting its hash). Hints
    /// on the working row.
    fn open_tag_input(&mut self) -> bool {
        let Some(commit) = self.selected_real_commit().map(|c| c.full_hash.clone()) else {
            self.status = Status::Notice("Select a commit".to_string());
            return true;
        };
        self.open_input(InputKind::NewTag, String::new(), Some(commit))
    }

    /// Open the Create Patch dialog for the selected commit: snapshot its hash and
    /// prefill an editable `/tmp/<short>.patch` destination. Hints on the working row.
    fn open_patch_input(&mut self) -> bool {
        let c = match self.selected_real_commit() {
            Some(c) => c,
            None => {
                self.status = Status::Notice("Select a commit".to_string());
                return true;
            }
        };
        let initial = format!("/tmp/{}.patch", c.hash);
        let commit = c.full_hash.clone();
        self.open_input(InputKind::CreatePatch, initial, Some(commit))
    }

    /// Open the reword dialog for the selected commit: prefill its message, snapshot
    /// its hash, and warn when rewording rewrites history (not HEAD) or published
    /// history (already pushed). Hints on the working row.
    fn open_reword_input(&mut self) -> bool {
        let c = match self.selected_real_commit() {
            Some(c) => c,
            None => {
                self.status = Status::Notice("Select a commit".to_string());
                return true;
            }
        };
        let commit = c.full_hash.clone();
        let message = model::commit_subject(c);
        let pushed = self.repo.has_remotes && !self.repo.unpushed.contains(&c.full_hash);
        let note = match (c.head, pushed) {
            (true, false) => None,
            (false, false) => Some("Rewrites history: newer commits get new hashes.".to_string()),
            (true, true) => Some("Already pushed: avoid rewriting published history.".to_string()),
            (false, true) => {
                Some("Pushed + has children: rewrites published history.".to_string())
            }
        };
        self.open_input_full(InputKind::Reword, message, Some(commit), note, None)
    }

    /// The active input field (the editable line), if an input dialog is open.
    fn input_field(&mut self) -> Option<&mut crate::view_state::TextField> {
        match &mut self.view.dialog {
            Some(Dialog::Input { field, .. }) => Some(field),
            _ => None,
        }
    }

    /// Open a yes/no confirmation for an outward-facing action (push/pull).
    fn open_confirm(&mut self, action: GitAction, prompt: &str) -> bool {
        self.close_popups();
        self.view.dialog = Some(Dialog::Confirm { action, prompt: prompt.to_string() });
        true
    }

    /// Open the copy-field picker. No-op (status hint) when no REAL commit is selected
    /// (the synthetic working row has no hash/commit to copy).
    fn open_copy(&mut self) -> bool {
        match self.copy_fields() {
            Some(fields) => {
                self.close_popups();
                self.view.dialog = Some(Dialog::Copy { sel: 0, fields });
                true
            }
            None => {
                self.status = Status::Notice("Select a commit to copy".to_string());
                true
            }
        }
    }

    /// The commit at the (filtered) log selection, if any.
    fn selected_commit(&self) -> Option<&model::Commit> {
        let vis = model::visible_commits(&self.repo, &self.view);
        vis.get(self.view.log_sel).and_then(|&i| self.repo.commits.get(i))
    }

    /// The selected REAL commit, i.e. not the synthetic `<current>` working row (which
    /// has no hash to target). The single home for the "is this a commit-targetable
    /// row" rule that every commit-targeted menu action funnels through (each caller
    /// supplies its own "Select a commit" notice on `None`, since the wording varies).
    fn selected_real_commit(&self) -> Option<&model::Commit> {
        self.selected_commit().filter(|c| !c.is_working)
    }

    /// Insert a char at the input dialog's caret (replacing any selection).
    fn dialog_input(&mut self, c: char) -> bool {
        match self.input_field() {
            Some(f) => {
                f.insert(c);
                true
            }
            None => false,
        }
    }

    /// Backspace in the input dialog (the selection, else the char before the caret).
    fn dialog_backspace(&mut self) -> bool {
        match self.input_field() {
            Some(f) => {
                f.backspace();
                true
            }
            None => false,
        }
    }

    /// Forward-delete in the input dialog.
    fn dialog_delete(&mut self) -> bool {
        match self.input_field() {
            Some(f) => {
                f.delete();
                true
            }
            None => false,
        }
    }

    /// Move the input dialog's caret; `select` extends the selection.
    fn dialog_caret(&mut self, dir: crate::message::Dir, select: bool) -> bool {
        match self.input_field() {
            Some(f) => {
                f.move_caret(dir, select);
                true
            }
            None => false,
        }
    }

    /// Select the whole input field.
    fn dialog_select_all(&mut self) -> bool {
        match self.input_field() {
            Some(f) => {
                f.select_all();
                true
            }
            None => false,
        }
    }

    /// Copy the input selection into the internal clipboard register (like the editor).
    fn dialog_copy(&mut self) -> bool {
        match self.input_field().and_then(|f| f.selected_text()) {
            Some(t) => {
                self.view.clipboard = t;
                true
            }
            None => false,
        }
    }

    /// Cut the input selection into the internal clipboard register.
    fn dialog_cut(&mut self) -> bool {
        match self.input_field().and_then(|f| f.cut_take()) {
            Some(t) => {
                self.view.clipboard = t;
                true
            }
            None => false,
        }
    }

    /// Paste the internal clipboard register at the input caret.
    fn dialog_paste(&mut self) -> bool {
        let reg = self.view.clipboard.clone();
        match self.input_field() {
            Some(f) => {
                f.insert_str(&reg);
                true
            }
            None => false,
        }
    }

    /// Toggle the input dialog's checkbox (new-branch checkout). No-op if absent.
    fn dialog_toggle_check(&mut self) -> bool {
        if let Some(Dialog::Input { checkbox: Some((_, on)), .. }) = &mut self.view.dialog {
            *on = !*on;
            return true;
        }
        false
    }

    /// Click a picker option row `i`. The COPY picker selects + confirms (copy + close)
    /// in one gesture - a single click copies the field. The CHOICE picker only SELECTS
    /// (a destructive op needs the explicit [Reset]/Enter confirm, not a one-click fire).
    fn dialog_pick_row(&mut self, i: usize) -> bool {
        match &mut self.view.dialog {
            Some(Dialog::Copy { sel, .. }) => {
                *sel = i.min(COPY_FIELDS.len() - 1);
                self.dialog_confirm()
            }
            Some(Dialog::Choice { kind, sel, .. }) => {
                *sel = i.min(crate::view_state::choice_options(*kind).len().saturating_sub(1));
                true
            }
            // A rebase row click CYCLES that row's action (and focuses it) - the
            // destructive rebase still needs the explicit [Rebase] button, like Choice.
            Some(Dialog::Rebase { steps, sel, .. }) => {
                if let Some(step) = steps.get_mut(i) {
                    *sel = i;
                    step.action = cycle_rebase_action(step.action);
                }
                true
            }
            // A compare-picker row click selects + CONFIRMS (a read-only compare needs no extra
            // step), like Copy.
            Some(Dialog::Picker { items, sel, .. }) => {
                *sel = i.min(items.len().saturating_sub(1));
                self.dialog_confirm()
            }
            // A ref-pick row click selects + CONFIRMS, opening the op's confirm (checkout/merge/
            // rebase). The destructive op still needs the explicit Yes on that confirm.
            Some(Dialog::RefPick { items, sel, .. }) => {
                *sel = i.min(items.len().saturating_sub(1));
                self.dialog_confirm()
            }
            // A remotes row click only SELECTS (the edit/add/remove verbs are the buttons + keys);
            // the destructive remove needs its explicit confirm, like Choice.
            Some(Dialog::Remotes { remotes, sel }) => {
                *sel = i.min(remotes.len().saturating_sub(1));
                true
            }
            _ => false,
        }
    }

    /// Move the open picker's selection by `delta`, clamped to its row list (the COPY
    /// field list, the CHOICE option list, or the REBASE step list).
    fn dialog_move(&mut self, delta: isize) -> bool {
        let len = match &self.view.dialog {
            Some(Dialog::Copy { .. }) => COPY_FIELDS.len(),
            Some(Dialog::Choice { kind, .. }) => crate::view_state::choice_options(*kind).len(),
            Some(Dialog::Rebase { steps, .. }) => steps.len(),
            Some(Dialog::Picker { items, .. }) => items.len(),
            Some(Dialog::RefPick { items, .. }) => items.len(),
            Some(Dialog::Remotes { remotes, .. }) => remotes.len(),
            _ => return false,
        };
        if len == 0 {
            return false;
        }
        let sel = match &mut self.view.dialog {
            Some(Dialog::Copy { sel, .. })
            | Some(Dialog::Choice { sel, .. })
            | Some(Dialog::Rebase { sel, .. })
            | Some(Dialog::Picker { sel, .. })
            | Some(Dialog::RefPick { sel, .. })
            | Some(Dialog::Remotes { sel, .. }) => sel,
            _ => return false,
        };
        *sel = (*sel as isize + delta).clamp(0, len as isize - 1) as usize;
        true
    }

    /// Cycle the focused rebase row's action one step (the keyboard Space affordance).
    fn dialog_cycle_row(&mut self) -> bool {
        if let Some(Dialog::Rebase { steps, sel, .. }) = &mut self.view.dialog {
            if let Some(step) = steps.get_mut(*sel) {
                step.action = cycle_rebase_action(step.action);
                return true;
            }
        }
        false
    }

    /// Set the focused rebase row's action outright (the p/s/f/d letter keys).
    fn dialog_set_row(&mut self, action: RebaseAction) -> bool {
        if let Some(Dialog::Rebase { steps, sel, .. }) = &mut self.view.dialog {
            if let Some(step) = steps.get_mut(*sel) {
                step.action = action;
                return true;
            }
        }
        false
    }

    /// Confirm the open dialog: an input parks its git action (an empty message/name
    /// is a no-op hint), a confirm parks its action, the copy picker parks the chosen
    /// field's text as an `Effect::Clipboard`. Always closes the dialog. ZERO IO.
    fn dialog_confirm(&mut self) -> bool {
        let dialog = match self.view.dialog.take() {
            Some(d) => d,
            None => return false,
        };
        match dialog {
            Dialog::Input { kind, field, commit, checkbox, .. } => {
                let text = field.trimmed();
                if text.is_empty() {
                    self.status = Status::Notice("Empty - nothing to do".to_string());
                    return true;
                }
                // Commit/Amend `git add -A` the on-DISK tree, but the in-diff editor is
                // the live working file held in memory. Flush a dirty buffer FIRST (the
                // runtime drains SaveFile before Git) so the commit includes exactly what
                // the user sees - and so the post-commit reload does not overwrite + drop
                // the unsaved edits. NOT gated on the autosave toggle: a commit must
                // capture the visible content regardless.
                if matches!(kind, InputKind::Commit | InputKind::Amend | InputKind::CommitFile | InputKind::CommitFolder | InputKind::CommitSelected) {
                    if let Some(e) = &self.view.editor {
                        if e.loaded && e.dirty {
                            self.view.effects.push(Effect::Save { path: e.path.clone(), content: e.to_content() });
                        }
                    }
                }
                // The commit-targeted kinds REQUIRE a snapshotted hash (set at open
                // time). Bail loudly rather than parking a blank ref if the invariant is
                // ever broken by a future refactor - an empty hash would mangle git args.
                let action = match kind {
                    InputKind::Commit => GitAction::Commit(text),
                    InputKind::Amend => GitAction::Amend(text),
                    InputKind::Tag => GitAction::Tag(text),
                    // "name url" -> add a remote. Both parts required; a missing URL hints.
                    InputKind::RemoteAdd => {
                        let mut parts = text.splitn(2, char::is_whitespace);
                        let name = parts.next().unwrap_or("").to_string();
                        let url = parts.next().map(str::trim).unwrap_or("").to_string();
                        if name.is_empty() || url.is_empty() {
                            self.status = Status::Notice("Enter: name url".to_string());
                            return true;
                        }
                        GitAction::RemoteAdd { name, url }
                    }
                    // Whole-tree working patch (empty file = whole tree) / apply a patch file.
                    InputKind::CreatePatchAll => {
                        GitAction::CreateWorkingPatch { file: String::new(), path: text }
                    }
                    InputKind::ApplyPatch => GitAction::ApplyPatch { path: text },
                    // Marked-set commit / patch: the paths were parked in `parked_marked` at
                    // menu-open (decoupled from a later mark change); drain them here.
                    InputKind::CommitSelected => {
                        let paths = std::mem::take(&mut self.view.parked_marked);
                        GitAction::CommitSelected { paths, message: text }
                    }
                    InputKind::CreatePatchSelected => {
                        let paths = std::mem::take(&mut self.view.parked_marked);
                        GitAction::CreatePatchSelected { paths, path: text }
                    }
                    InputKind::CreatePatchSeries => {
                        let commits = std::mem::take(&mut self.view.parked_marked);
                        GitAction::CreatePatchSeries { commits, dir: text }
                    }
                    InputKind::NewBranch
                    | InputKind::NewTag
                    | InputKind::Reword
                    | InputKind::CreatePatch
                    | InputKind::CreateWorkingPatch
                    | InputKind::CommitFile
                    | InputKind::CommitFolder
                    | InputKind::ArchiveProject
                    | InputKind::RenameBranch
                    | InputKind::RemoteSetUrl => {
                        // These kinds snapshot a target in the `commit` slot at open time
                        // (a commit hash / `WORKING_REV` for ArchiveProject; for RenameBranch the
                        // OLD branch name; for CreateWorkingPatch / CommitFile the FILE path).
                        let commit = match commit {
                            Some(c) => c,
                            None => {
                                self.status = Status::Notice("No commit selected".to_string());
                                return true;
                            }
                        };
                        match kind {
                            InputKind::NewBranch => GitAction::BranchAt {
                                name: text,
                                commit,
                                checkout: checkbox.is_some_and(|(_, on)| on),
                            },
                            InputKind::NewTag => GitAction::TagAt { name: text, commit },
                            InputKind::CreatePatch => GitAction::CreatePatch { commit, path: text },
                            InputKind::CreateWorkingPatch => {
                                GitAction::CreateWorkingPatch { file: commit, path: text }
                            }
                            InputKind::CommitFile => {
                                GitAction::CommitFile { file: commit, message: text }
                            }
                            InputKind::CommitFolder => {
                                GitAction::CommitFolder { dir: commit, message: text }
                            }
                            InputKind::Reword => GitAction::RewordAt { commit, message: text },
                            InputKind::ArchiveProject => GitAction::ArchiveProject { rev: commit, path: text },
                            InputKind::RenameBranch => GitAction::BranchRename { old: commit, new: text },
                            InputKind::RemoteSetUrl => GitAction::RemoteSetUrl { name: commit, url: text },
                            // The outer guard admits only the commit-targeted kinds; a
                            // HEAD/upstream kind here is a refactor bug, not a blank ref.
                            _ => unreachable!("non-commit-targeted kind in the commit-targeted arm"),
                        }
                    }
                };
                self.view.effects.push(Effect::Git(action));
            }
            Dialog::Confirm { action, .. } => {
                self.view.effects.push(Effect::Git(action));
            }
            Dialog::Copy { sel, fields } => {
                self.view.effects.push(Effect::Clipboard(fields[sel].clone()));
                self.status = Status::Notice(format!("Copied {}", COPY_FIELDS[sel]));
            }
            Dialog::Choice { kind, sel, commit, .. } => match kind {
                ChoiceKind::ResetMode => {
                    let mode = ResetMode::ALL[sel];
                    self.view.effects.push(Effect::Git(GitAction::ResetTo { commit, mode }));
                }
                ChoiceKind::PullStrategy => {
                    let rebase = crate::view_state::PullStrategy::ALL[sel].rebase();
                    self.view.effects.push(Effect::Git(GitAction::PullStrategy { rebase }));
                }
            },
            Dialog::Rebase { steps, base, .. } => {
                // The FIRST kept commit in todo order (oldest first = the display tail) cannot
                // be squashed/fixed-up: there is no older kept commit to meld it into, and git
                // would refuse ("Cannot 'squash' without a previous commit"). Catch it here so
                // the user sees why instead of an aborted-rebase error.
                let first_kept = steps.iter().rev().find(|s| s.action != RebaseAction::Drop);
                if first_kept.is_some_and(|s| s.action.is_meld()) {
                    self.status = Status::Notice(
                        "Oldest kept commit cannot be squashed (nothing to meld into)".to_string(),
                    );
                    return true;
                }
                // Carry only the non-pick rows (a pick needs no todo rewrite), each with its verb.
                let ops: Vec<(String, RebaseAction)> = steps
                    .iter()
                    .filter(|s| s.action != RebaseAction::Pick)
                    .map(|s| (s.full.clone(), s.action))
                    .collect();
                if ops.is_empty() {
                    self.status = Status::Notice("No commits marked".to_string());
                } else {
                    self.view.effects.push(Effect::Git(GitAction::RebaseTodo { base, ops }));
                }
            }
            // Confirm a picked revision/ref. Compare diffs the working file vs that rev as a
            // read-only overlay. Show History (CommitDiff) instead NAVIGATES the log to the
            // picked commit and re-selects this file - the historical commit's own (read-only,
            // parent-vs-commit) diff then shows in place, and the log cursor lands on it.
            Dialog::Picker { path, items, sel, mode, .. } => {
                if let Some(item) = items.get(sel) {
                    use crate::view_state::InspectMode;
                    match mode {
                        // Only Compare/CommitDiff ever open a picker (Source/Blame park directly).
                        InspectMode::Source | InspectMode::Blame => {
                            unreachable!("a picker only carries Compare or CommitDiff")
                        }
                        InspectMode::CommitDiff => return self.show_history_revision(&item.rev, path),
                        InspectMode::Compare => {
                            // Compare `rev` against the row the menu was opened on: the working
                            // tree (<current>) or the selected historical commit's blob.
                            let base = self.selected_commit_hash().unwrap_or_else(|| WORKING_REV.to_string());
                            self.view.effects.push(Effect::Inspect(crate::view_state::InspectReq {
                                rev: item.rev.clone(),
                                path,
                                title: format!("vs {} - Esc to close", item.rev),
                                mode,
                                base,
                            }));
                            self.status = Status::Notice("Loading compare...".to_string());
                        }
                    }
                }
            }
            // Confirm a branch/tag pick: map the op + ref to its commit-targeted GitAction behind a
            // confirm (the destructive merge/rebase warn). Mirrors the ref-chip menu's wording.
            Dialog::RefPick { items, sel, op } => {
                use crate::view_state::RefOp;
                if let Some(item) = items.get(sel) {
                    let name = item.rev.clone();
                    // The list label ends with the ref kind ("(branch)" local / "(remote)" /
                    // "(tag)"); only a LOCAL branch attaches HEAD - a remote ref or tag detaches it.
                    let local_branch = item.label.ends_with("(branch)");
                    let cur = self.current_branch().unwrap_or_else(|| "HEAD".to_string());
                    return match op {
                        RefOp::Checkout => {
                            // Warn on the detach (remote/tag) so it is never a surprise; a local
                            // branch (incl. a slashed name like feature/x) attaches cleanly.
                            let prompt = if local_branch {
                                format!("Checkout '{name}'?")
                            } else {
                                format!("Checkout '{name}'? HEAD will detach.")
                            };
                            self.open_confirm(GitAction::CheckoutRef { name }, &prompt)
                        }
                        RefOp::Merge => self.open_confirm(
                            GitAction::MergeRef { name: name.clone() },
                            &format!("Merge '{name}' into '{cur}'?"),
                        ),
                        RefOp::Rebase => self.open_confirm(
                            GitAction::RebaseOnto { name: name.clone() },
                            &format!("Rebase '{cur}' onto '{name}'? Rewrites history."),
                        ),
                    };
                }
            }
            // Enter / [Edit] on a remote: open its URL editor (prefilled, name in the commit slot).
            Dialog::Remotes { remotes, sel } => {
                if let Some((name, url)) = remotes.get(sel) {
                    return self.open_input(InputKind::RemoteSetUrl, url.clone(), Some(name.clone()));
                }
                self.status = Status::Notice("No remote selected".to_string());
            }
            // Read-only: confirm just closes it (the take above already did).
            Dialog::Help => {}
        }
        true
    }

    /// Dismiss the open dialog without acting.
    fn dialog_cancel(&mut self) -> bool {
        self.view.dialog.take().is_some()
    }

    /// The full copy text for every [`COPY_FIELDS`] entry from the selected commit:
    /// short hash, full hash, message (subject), full info (every log column). The
    /// array is parallel to `COPY_FIELDS`. `None` with no real commit selected (the
    /// synthetic working row has nothing to copy).
    fn copy_fields(&self) -> Option<[String; 4]> {
        let c = self.selected_real_commit()?;
        let subject = model::commit_subject(c);
        Some([
            c.hash.clone(),
            c.full_hash.clone(),
            subject.clone(),
            format!("{} {} {} {}", subject, c.author, c.hash, c.date),
        ])
    }

    /// A repo-level git action finished on the loader: surface `summary` and, when the
    /// log/working row changed (`reload`), park a full repo reload via the watch path.
    fn git_action_done(&mut self, summary: String, reload: bool) -> bool {
        if reload {
            // The commit graph / working row changed: re-request the whole repo. Reuse
            // the same handoff `--watch` uses so the runtime fires one `Req::Reload`. The
            // incoming `RepoLoaded` resets status to Ready, so carry the notice across it
            // (else "Committed/Deleted/..." would flash and vanish before it is read).
            self.view.effects.push(Effect::ReloadRepo);
            self.view.parked_notice = Some(summary.clone());
        } else {
            // A non-reload (transient) notice is never sticky: clear any leftover sticky flag
            // from a prior reload-action so this hint does not outlive its own next file-open.
            self.view.notice_sticky = false;
        }
        self.status = Status::Notice(summary);
        true
    }

    /// Open the revert confirmation modal over the target set (marks or cursor).
    /// An empty target -> a brief status hint, no modal. ZERO IO. Fired by Alt+R.
    fn request_revert(&mut self) -> bool {
        let paths = self.revert_target_paths();
        self.open_revert_modal(paths)
    }

    /// Open the revert modal for ONLY the file whose diff is shown (the Editor-menu
    /// Revert): ignores multi-select marks, targets the cursor / previewed file. An
    /// unchanged or absent file -> a status hint, no modal.
    fn request_revert_shown(&mut self) -> bool {
        let changed = TreeNode::changed_paths(&self.repo.tree);
        let paths = self
            .selected_file_path()
            .filter(|p| changed.contains(p))
            .into_iter()
            .collect();
        self.open_revert_modal(paths)
    }

    /// Build the `RevertRequest` from `paths` + the selected commit and open the
    /// confirmation modal. ZERO IO; an empty target / no commit -> a status hint.
    fn open_revert_modal(&mut self, paths: Vec<String>) -> bool {
        if paths.is_empty() {
            self.status = Status::Notice("Nothing selected to revert".to_string());
            return true;
        }
        let (commit_hash, commit_label) = match self.selected_commit_hash() {
            Some(hash) => (hash, self.selected_commit_label()),
            None => {
                self.status = Status::Notice("No commit selected to revert".to_string());
                return true;
            }
        };
        self.view.revert_confirm = Some(crate::view_state::RevertRequest {
            commit_hash,
            commit_label,
            paths,
        });
        true
    }

    /// Subject of the commit at the (filtered) log selection, for the modal prompt.
    /// Empty when nothing is selected.
    fn selected_commit_label(&self) -> String {
        let vis = model::visible_commits(&self.repo, &self.view);
        vis.get(self.view.log_sel)
            .and_then(|&idx| self.repo.commits.get(idx))
            .map(model::commit_subject)
            .unwrap_or_default()
    }

    /// Dismiss the revert modal without reverting. Returns whether it was open.
    fn cancel_revert(&mut self) -> bool {
        self.view.revert_confirm.take().is_some()
    }

    /// Confirm the revert: close the modal and hand the request to the runtime via
    /// an `Effect::Revert` (the event loop drains it into a batch `Req::Revert`).
    /// ZERO IO here - the loader does the destructive working-tree write.
    fn confirm_revert(&mut self) -> bool {
        match self.view.revert_confirm.take() {
            Some(req) => {
                self.view.effects.push(Effect::Revert(req));
                true
            }
            None => false,
        }
    }

    /// Apply a finished batch revert (ZERO IO - the git write already happened in the
    /// loader): PRUNE the successfully-reverted file rows from the tree (and any
    /// now-empty parent dirs), re-clamp `files_sel` + `diff_scroll` into the shorter
    /// tree, clear the now-stale marks, drop the preview if its file was reverted, and
    /// set `summary` as the status. Reverting every file empties the tree (it renders
    /// its empty state without panic).
    fn revert_done(&mut self, paths: Vec<String>, summary: String) -> bool {
        // Was the previewed (cursor) file among the reverted? If so its preview is
        // stale (the change is gone), so it must be cleared/reloaded.
        let previewed_reverted = self
            .selected_file_path()
            .is_some_and(|p| paths.contains(&p));

        if self.view.show_all_files {
            // All-files view: the reverted files STILL EXIST on disk (a Modified
            // file is now Unchanged, a Deleted file is restored), so pruning would
            // wrongly vanish them. Park a reload so the runtime re-fetches the full
            // tree from disk; do NOT mutate the tree here (ZERO-IO).
            self.view.effects.push(Effect::ReloadTree);
        } else {
            // Changed-only view: a reverted file is no longer a change, so it leaves
            // the changed list - prune its row (and any now-empty parent dir).
            TreeNode::prune_paths(&mut self.repo.tree, &paths);
            let max = self.files_rows_len().saturating_sub(1);
            self.view.files_sel = self.view.files_sel.min(max);
        }
        self.reset_diff_scroll();
        self.view.files_scroll = None;
        self.view.files_marked.clear();
        if previewed_reverted || self.repo.tree.is_empty() {
            self.repo.preview = None;
        }
        self.status = Status::Notice(summary);
        true
    }

    /// Reset the diff body's vertical scroll on a file/selection change: the browse
    /// offset to the top AND the editable free-scroll override OFF (a freshly opened
    /// file follows its caret again). The single place both diff-body scrolls reset.
    fn reset_diff_scroll(&mut self) {
        self.view.diff_scroll = 0;
        self.view.edit_scroll = None;
        self.view.diff_hscroll = None;
    }

    /// Park a wheel-driven free-scroll override (the editable diff / log / files list)
    /// at first-visible row `top`, reporting whether it changed (so the frame repaints).
    /// The runtime computed AND clamped `top` against the live viewport, so apply just
    /// stores it. `field` selects which override to set. ZERO-IO.
    fn set_free_scroll(
        &mut self,
        field: impl Fn(&mut ViewState) -> &mut Option<usize>,
        top: usize,
    ) -> bool {
        let slot = field(&mut self.view);
        let changed = *slot != Some(top);
        *slot = Some(top);
        changed
    }

    /// Scroll the diff/preview body by `delta` lines within a `pane_height`-row
    /// viewport, clamped to `[0, max(0, visual_rows - pane_height)]` so the last
    /// content row pins to the body bottom and the body never scrolls off into
    /// blank rows. Mirrors [`Self::scroll_detail`]. The visual-row count collapses
    /// side-by-side modified pairs, so it matches what `diff_view` actually paints.
    fn scroll_diff(&mut self, delta: isize, pane_height: usize) -> bool {
        let side_by_side = self.view.diff_mode == crate::view_state::DiffMode::SideBySide;
        // Clamp against whatever the pane is SHOWING - the inspect overlay when open, else the
        // preview - so a wheel over the read-only overlay scrolls its own line count.
        let rows = self.shown_view().map_or(0, |v| v.visual_rows(side_by_side));
        let max = rows.saturating_sub(pane_height);
        let next = (self.view.diff_scroll as isize + delta).clamp(0, max as isize) as usize;
        let changed = next != self.view.diff_scroll;
        self.view.diff_scroll = next;
        changed
    }

    /// Scroll the detail pane by `delta` lines within a `pane_height`-row viewport,
    /// clamped to `[0, max(0, content_height - pane_height)]` so it never scrolls past
    /// the last content row. Both `pane_height` and `content_height` (the WRAPPED
    /// visual-line count) arrive from the runtime boundary; geometry never reaches
    /// apply directly, and the wrapped count keeps a folded subject's tail reachable.
    fn scroll_detail(&mut self, delta: isize, pane_height: usize, content_height: usize) -> bool {
        let max = content_height.saturating_sub(pane_height);
        let next = (self.view.detail_scroll as isize + delta).clamp(0, max as isize) as usize;
        let changed = next != self.view.detail_scroll;
        self.view.detail_scroll = next;
        changed
    }

    /// Number of selectable rows in the files panel (honoring the Flat toggle).
    /// Public so the runtime's mouse hit-test can bound a files-row click.
    pub fn files_rows_len(&self) -> usize {
        self.visible_files().len()
    }

    /// Write a loaded preview ONLY when it still matches the current selection,
    /// mirroring the [`Msg::DetailLoaded`] hash-staleness guard: a preview that
    /// finished loading for a now-deselected (commit, path) is dropped so the
    /// viewer never shows the wrong file. ZERO IO.
    fn apply_preview(&mut self, commit: &str, path: &str, view: Option<FileView>) -> bool {
        let current = self.selected_commit_hash();
        if current.as_deref() == Some(commit) && self.selected_file_path().as_deref() == Some(path)
        {
            // A read-only preview arrived: this file is not editable (binary / no
            // working copy), so drop any editable buffer.
            self.view.editor = None;
            // The backend delivers the FULL diff (every unchanged line); "Hide unchanged"
            // folds the middle here so a picked commit's read-only diff folds uniformly with
            // the editable `<current>` (one fold implementation, `textdiff::fold_unchanged`).
            self.repo.preview = view.map(|v| self.fold_readonly(v));
            // A read-only diff always carries real changes (an unchanged read-only file
            // is delivered as a full-width `Source`), so the two-pane diff stays.
            self.view.diff_full_width = false;
            // A diff whose first hunk starts below line 1 opens with a LEADING fold marker
            // at index 0; nudge the browse cursor off it so Tab-into-Diff lands on a real
            // line (Move already skips markers, but the initial position is not a Move).
            self.normalize_diff_cursor();
            true
        } else {
            false
        }
    }

    /// A read-only inspect overlay (Show Current Revision / a compared revision) finished
    /// loading: open it over the diff pane and focus that pane so its scroll/selection keys
    /// route there. A `None` view means the path did not exist at that revision - report it as
    /// a Notice instead of opening an empty overlay. Does NOT touch `repo.preview`/the editor.
    fn inspect_loaded(&mut self, title: String, path: &str, view: Option<FileView>) -> bool {
        match view {
            Some(view) => {
                // Staleness guard (mirrors apply_preview): a reply that lands after the user
                // navigated to a different file must NOT reopen the overlay over the wrong file.
                // The choke points already cleared `view.inspect`; dropping the stale reply keeps
                // it cleared instead of resurrecting it.
                if self.selected_file_path().as_deref() != Some(path) {
                    return false;
                }
                let view = self.fold_readonly(view);
                self.view.inspect = Some(crate::view_state::InspectView { title, view });
                // The overlay IS the diff pane; force it visible (the menu offers Show Current
                // Revision / Compare even when View > Show Diff is OFF, where compute_layout
                // would otherwise yield no diff pane to render - an invisible, key-swallowing
                // dead state). Capture the user's toggle FIRST (once per overlay session) so
                // `dismiss_inspect` can restore it instead of silently flipping the persisted
                // setting. Focus the pane so its scroll/selection keys route there.
                if self.view.inspect_prior_show_diff.is_none() {
                    self.view.inspect_prior_show_diff = Some(self.view.show_diff);
                }
                self.view.show_diff = true;
                self.view.focus = Pane::Diff;
                self.reset_diff_scroll();
                // The overlay itself is the indicator now; clear the "Loading revision..." hint.
                if matches!(self.status, Status::Notice(_)) {
                    self.status = Status::Ready;
                }
                true
            }
            None => {
                self.status = Status::Notice("No committed revision for this file".to_string());
                true
            }
        }
    }

    /// Take the read-only inspect overlay (Esc / every navigation + filter choke point) and
    /// restore the user's `show_diff` toggle if the overlay had forced the diff pane visible, so
    /// the persisted setting is never left flipped. Returns whether an overlay was open.
    fn close_inspect(&mut self) -> bool {
        self.dismiss_inspect()
    }

    fn dismiss_inspect(&mut self) -> bool {
        let was_open = self.view.inspect.take().is_some();
        if let Some(prior) = self.view.inspect_prior_show_diff.take() {
            self.view.show_diff = prior;
        }
        was_open
    }

    /// Swap in a freshly-loaded changed-files tree ONLY when `hash` is still the
    /// selected commit, mirroring the [`Msg::DetailLoaded`] staleness guard. The
    /// new tree invalidates the old file selection, so we re-flatten, clamp
    /// `files_sel`, reset the diff scroll, and clear the preview (which is reloaded
    /// out-of-band by the loader). ZERO IO.
    fn apply_tree(
        &mut self,
        hash: &str,
        tree: Vec<TreeNode>,
        ignored: std::collections::HashSet<String>,
    ) -> bool {
        if self.selected_commit_hash().as_deref() != Some(hash) {
            return false;
        }
        self.repo.tree = tree;
        self.repo.ignored = ignored;
        // All mode opens with directories COLLAPSED so a big tree is not a wall of
        // files (the user expands via Expand/Collapse); the changed-only tree stays
        // expanded as the backend built it (the golden render is unaffected).
        if self.view.show_all_files {
            TreeNode::set_all_expanded(&mut self.repo.tree, false);
        }
        let max = self.files_rows_len().saturating_sub(1);
        self.view.files_sel = self.view.files_sel.min(max);
        self.view.diff_scroll = 0;
        // The tree's row set changed (refresh / All-toggle): a files free-scroll offset
        // would be stale, so refollow the selection.
        self.view.files_scroll = None;
        // The list reflowed under any open files context menu (an All-toggle or a
        // `--watch` refresh): dismiss it so it cannot float over a drifted row.
        self.view.files_menu = None;
        // Do NOT clear the preview here. This handler runs for the ALREADY-selected
        // commit (the hash guard above), including the redundant tree the initial
        // `RepoLoaded` triggers and an All-toggle / revert refresh of the SAME commit.
        // Clearing the preview here raced the initial file-open (bug 3): the OpenFile
        // reply loaded the diff, then this TreeLoaded wiped it back to a stuck "Loading
        // diff...". Preview clearing is owned by the selection sites (select_commit/
        // select_file) on a real move.
        //
        // Marks are path-keyed, so KEEP the ones whose file still exists (an All-toggle
        // / refresh must not drop the user's multi-selection) and only drop marks for
        // paths gone from the new tree (e.g. just-reverted files). Enumerate via
        // `flatten_flat` (EVERY file, regardless of dir expand state) - the visible
        // flatten only descends expanded dirs, and All-mode collapses them all first,
        // which would wrongly prune every mark on a nested file.
        let present: std::collections::HashSet<String> = TreeNode::flatten_flat(&self.repo.tree)
            .into_iter()
            .filter_map(|(_, p)| p)
            .collect();
        self.view.files_marked.retain(|p| present.contains(p));
        // Show History navigated to this commit and parked the file it was opened on: reveal it
        // now its tree exists (preview was cleared by select_commit, so the runtime then loads
        // the revealed file's read-only diff).
        if let Some(path) = self.view.parked_file_path.take() {
            let found = self.reveal_file_by_path(&path);
            // "Show Current Revision" fallback: the file has no working-tree row (an unchanged
            // file - nothing to edit), so show its committed content read-only instead of
            // landing on an unrelated file. A plain reveal (Show History) leaves this None.
            if let Some(rev_path) = self.view.parked_revision.take() {
                if !found {
                    self.show_committed_revision(rev_path);
                }
            }
        } else if self.repo.preview.is_none()
            && self.view.editor.is_none()
            && self.selected_file_path().is_none()
        {
            // A FRESH commit move (select_commit cleared the preview) whose carried selection
            // landed on a NON-file row - the synthetic repo-root or a directory - opens the
            // commit's FIRST file so its diff shows instead of a stale/empty viewer. A
            // same-commit refresh (All-toggle / --watch) keeps its preview, so it never snaps
            // here and the user's selection persists.
            self.view.files_sel = self.first_file_row();
        }
        true
    }

    /// Number of commits visible under the current search + filters. Public so the
    /// runtime's log hit-test bounds a click against the FILTERED list.
    pub fn visible_len(&self) -> usize {
        model::visible_commits(&self.repo, &self.view).len()
    }

    /// Recompute `repo.detail` from the commit at the (filtered) log selection.
    /// Empty result -> `None` (the detail panel renders its empty state). ZERO IO.
    fn rebuild_detail(&mut self) {
        let vis = model::visible_commits(&self.repo, &self.view);
        self.repo.detail = vis
            .get(self.view.log_sel)
            .and_then(|&idx| self.repo.commits.get(idx))
            .map(model::detail_from);
        // A new commit is selected: start its detail at the top.
        self.view.detail_scroll = 0;
    }

    /// Re-clamp the log selection into the filtered list and rebuild the detail
    /// panel. The single tail every search/filter/toggle change runs through, so
    /// the selection and detail always track the visible commits.
    fn after_filter_change(&mut self) {
        self.view.nav_epoch += 1; // a filter move is navigation: stale a pre-move read
        self.dismiss_inspect(); // a filter can move the selection: drop a stale overlay
        let max = self.visible_len().saturating_sub(1);
        self.view.log_sel = self.view.log_sel.min(max);
        // The filtered list changed shape, so a log free-scroll offset is stale: refollow.
        self.view.log_scroll = None;
        self.rebuild_detail();
    }

    /// Blur BOTH search-input fields (log toolbar + files toolbar) without touching their
    /// queries. Called when a click/selection moves focus into a pane, so the next keystroke
    /// edits that pane instead of a still-focused filter field. Returns whether anything changed.
    fn blur_searches(&mut self) -> bool {
        let was = self.view.search_active || self.view.files_search_active;
        self.view.search_active = false;
        self.view.files_search_active = false;
        was
    }

    /// Focus or blur the search field. Focusing closes any open dropdown so the
    /// two input modes never overlap.
    fn set_search_active(&mut self, active: bool) -> bool {
        if self.view.search_active == active {
            return false;
        }
        self.view.search_active = active;
        if active {
            self.view.open_dropdown = None;
            self.view.search_history_open = false;
            self.view.files_search_active = false;
            self.view.commit_menu = None;
            self.view.files_menu = None;
        }
        true
    }

    /// Leave text-input mode; `clear` also wipes the query and re-filters. A committed
    /// (Enter, non-clear) non-empty query is recorded in the recent-search history.
    fn blur_search(&mut self, clear: bool) -> bool {
        let was_active = self.view.search_active;
        self.view.search_active = false;
        if clear && !self.view.search.is_empty() {
            self.view.search.clear();
            self.after_filter_change();
            return true;
        }
        if !clear {
            let query = self.view.search.clone();
            self.view.record_search(&query);
        }
        was_active
    }

    /// Clear the search query (the field's `x` icon) and re-filter to all commits. A
    /// no-op (no redraw) when the query is already empty.
    fn clear_search(&mut self) -> bool {
        if self.view.search.is_empty() {
            return false;
        }
        self.view.search.clear();
        self.after_filter_change();
        true
    }

    /// Focus or blur the files-pane search field. Focusing closes the other input modes
    /// (main search / dropdown / menus) so only one field captures typing at a time.
    fn set_files_search_active(&mut self, active: bool) -> bool {
        if self.view.files_search_active == active {
            return false;
        }
        self.view.files_search_active = active;
        if active {
            self.view.search_active = false;
            self.view.open_dropdown = None;
            self.view.search_history_open = false;
            self.view.open_menu = None;
            self.view.commit_menu = None;
            self.view.files_menu = None;
        }
        true
    }

    /// Leave files-search text input; `clear` also wipes the query and re-filters.
    fn blur_files_search(&mut self, clear: bool) -> bool {
        let was_active = self.view.files_search_active;
        self.view.files_search_active = false;
        if clear && !self.view.files_search.is_empty() {
            self.view.files_search.clear();
            self.after_files_search_change();
            return true;
        }
        was_active
    }

    /// Clear the files-search query (the field's `x` icon) and re-filter to all files.
    /// A no-op (no redraw) when the query is already empty.
    fn clear_files_search(&mut self) -> bool {
        if self.view.files_search.is_empty() {
            return false;
        }
        self.view.files_search.clear();
        self.after_files_search_change();
        true
    }

    /// The single tail every files-search change runs through. While a query is active the
    /// files pane forces the All (full-tree) view so the search spans the whole repo (the
    /// prior mode snapshotted into `files_prev_all` is restored when the query clears); the
    /// runtime re-fetches the tree on that `show_all_files` flip. The files selection is
    /// re-landed on the first matching FILE row and the wheel free-scroll is reset so the
    /// preview follows the filtered list.
    fn after_files_search_change(&mut self) {
        if self.view.files_search.is_empty() {
            if let Some(prev) = self.view.files_prev_all.take() {
                self.view.show_all_files = prev;
            }
        } else if self.view.files_prev_all.is_none() {
            self.view.files_prev_all = Some(self.view.show_all_files);
            self.view.show_all_files = true;
        }
        // A files-search reshapes the file list and re-lands the selection, so it is
        // navigation: drop a transient read-only overlay. Mirrors `select_file`/
        // `after_filter_change`; without it the overlay freezes over the re-landed file.
        self.view.nav_epoch += 1;
        self.dismiss_inspect();
        self.view.files_scroll = None;
        let max = self.files_rows_len().saturating_sub(1);
        self.view.files_sel = self.first_file_row().min(max);
        // Re-landing the selection may point at a DIFFERENT file than the open editable
        // buffer (only the `<current>` row is editable). Mirror every other navigation
        // path: flush a dirty buffer to disk (or warn when autosave is off) and drop it,
        // so a files-search keystroke never silently discards unsaved edits. Gate on the
        // file PATH (not the row index, which shifts as the filtered list reshapes).
        let new_path = self.selected_file_path();
        let editor_path = self.view.editor.as_ref().map(|e| e.path.clone());
        if editor_path.is_some() && editor_path != new_path {
            self.flush_editor_if_dirty();
            self.warn_if_dropping_unsaved();
            self.view.editor = None;
            self.repo.preview = None;
            self.view.diff_full_width = false;
            self.reset_diff_scroll();
        }
    }

    /// Toggle the lens's recent-search history popup. Opening closes the other input
    /// modes (menu / filter dropdown). A no-op (no redraw) when there is no history.
    fn toggle_search_history(&mut self) -> bool {
        if !self.view.search_history_open && self.view.search_history.is_empty() {
            return false;
        }
        self.view.search_history_open = !self.view.search_history_open;
        if self.view.search_history_open {
            // Mutually exclusive with every other popup.
            self.view.open_menu = None;
            self.view.open_dropdown = None;
            self.view.commit_menu = None;
            self.view.files_menu = None;
        }
        true
    }

    /// Run the history entry at `i` (a click in the lens popup): set it as the query,
    /// close the popup, and re-filter. Out-of-range -> just close.
    fn pick_search_history(&mut self, i: usize) -> bool {
        self.view.search_history_open = false;
        match self.view.search_history.get(i).cloned() {
            Some(query) => {
                self.view.search = query;
                self.after_filter_change();
                true
            }
            None => true,
        }
    }

    /// Open `kind`'s dropdown, seeding the highlight on the current selection's
    /// row (or the "All" row 0). Closes the search input mode.
    fn open_dropdown(&mut self, kind: FilterKind) -> bool {
        self.view.search_active = false;
        self.view.files_search_active = false;
        self.view.open_menu = None; // popups are mutually exclusive
        self.view.commit_menu = None;
        self.view.files_menu = None;
        self.view.search_history_open = false;
        self.view.open_dropdown = Some(kind);
        self.view.dropdown_sel = self.selected_option_row(kind);
        true
    }

    /// Open menu `id` as a popup, closing any open filter dropdown and search input
    /// (the popups are mutually exclusive).
    fn open_menu(&mut self, id: MenuId) -> bool {
        self.view.search_active = false;
        self.view.files_search_active = false;
        self.view.search_history_open = false;
        self.view.open_dropdown = None;
        self.view.commit_menu = None;
        self.view.files_menu = None;
        self.view.open_menu = Some(id);
        true
    }

    /// Close the open menu, if any.
    fn close_menu(&mut self) -> bool {
        self.view.open_menu.take().is_some()
    }

    /// Pick a menu item: close the menu, then run the matching action by
    /// re-dispatching its existing message (keeps the action logic in one place).
    fn menu_pick(&mut self, action: MenuAction) -> bool {
        use MenuAction::*;
        self.view.open_menu = None;
        let msg = match action {
            Undo => Msg::Edit(crate::message::EditOp::Undo),
            Redo => Msg::Edit(crate::message::EditOp::Redo),
            Autosave => Msg::ToggleAutosave,
            DiffMode => Msg::ToggleDiffMode,
            WordWrap => Msg::ToggleWordWrap,
            Whitespace => Msg::ToggleWhitespace,
            Revert => Msg::RevertFile,
            ShowDiff => Msg::ToggleDiff,
            HideUnchanged => Msg::ToggleHideUnchanged,
            ShowBlame => Msg::ToggleShowBlame,
            // Global Git menu -> the existing repo-level dialog intents.
            GitCommit => Msg::OpenCommit,
            GitAmend => Msg::OpenAmend,
            GitTag => Msg::OpenTag,
            GitPull => Msg::RequestPull,
            GitPush => Msg::RequestPush,
            GitUpdate => Msg::RequestUpdate,
            // Confirm-backed git ops: open the confirm directly (no dedicated Msg).
            GitFetch => return self.open_confirm(GitAction::Fetch, "Fetch from the remote?"),
            GitStash => {
                return self.open_confirm(GitAction::Stash, "Stash all changes? (restore with git stash pop)")
            }
            GitUnstash => {
                return self.open_confirm(GitAction::Unstash, "Apply and drop the latest stash (git stash pop)?")
            }
            GitDiscard => return self.open_confirm(
                GitAction::DiscardAll,
                "Discard all uncommitted changes, including new files? This cannot be undone.",
            ),
            // Read the remote list off-thread, then open the Manage Remotes dialog on the reply.
            GitRemotes => {
                self.close_popups();
                self.view.effects.push(Effect::LoadRemotes);
                self.status = Status::Notice("Loading remotes...".to_string());
                return true;
            }
            // New branch rooted at HEAD (not a commit-targeted row): open the name input with
            // the checkout checkbox on, targeting the literal "HEAD".
            GitNewBranch => {
                return self.open_input_full(
                    InputKind::NewBranch,
                    String::new(),
                    Some("HEAD".to_string()),
                    None,
                    Some(("Checkout new branch".to_string(), true)),
                )
            }
            // Read the branch/tag list off-thread, then open the ref picker for this op.
            GitBranches => return self.open_refpick(crate::view_state::RefOp::Checkout),
            GitMerge => return self.open_refpick(crate::view_state::RefOp::Merge),
            GitRebase => return self.open_refpick(crate::view_state::RefOp::Rebase),
            // Patch ops: a destination/source path input (prefilled /tmp default).
            GitCreatePatch => {
                return self.open_input(InputKind::CreatePatchAll, "/tmp/working.patch".to_string(), None)
            }
            GitApplyPatch => {
                return self.open_input(InputKind::ApplyPatch, "/tmp/working.patch".to_string(), None)
            }
        };
        self.apply(msg);
        true
    }

    /// Right-click a log row: select that commit (keeping the wheel viewport - the
    /// clicked row is on screen) and open its context menu anchored at the click
    /// cell. Closing the other popups first keeps them mutually exclusive.
    fn open_commit_menu(&mut self, index: usize, col: u16, row: u16) -> bool {
        // A MULTI-COMMIT menu opens when right-clicking a MARKED commit with >=2 marks: snapshot
        // the set BEFORE select_commit clears it, ordered OLDEST-FIRST (cherry-pick order) by the
        // commits vec (newest-first, so reversed). Restored after so the marks stay visible.
        let vis = model::visible_commits(&self.repo, &self.view);
        let clicked = vis.get(index).map(|&ci| self.repo.commits[ci].hash.clone());
        let marked = self.view.commits_marked.clone();
        let is_multi = marked.len() >= 2 && clicked.as_ref().is_some_and(|h| marked.contains(h));
        if is_multi {
            let ordered: Vec<String> = self
                .repo
                .commits
                .iter()
                .rev()
                .filter(|c| marked.contains(&c.hash))
                .map(|c| c.hash.clone())
                .collect();
            self.select_commit(index, true);
            self.close_popups();
            self.view.commits_marked = marked;
            self.view.commit_menu = Some(CommitMenu {
                index: self.view.log_sel,
                col,
                row,
                scroll: 0,
                refs: Vec::new(),
                current_branch: self.current_branch(),
                open_ref: None,
                working: false,
                show_diff_item: !self.view.show_diff,
                marked: ordered,
            });
            return true;
        }
        self.select_commit(index, true);
        self.close_popups();
        // Snapshot the selected commit's ref decorations into branch/tag fly-out rows, plus
        // the current branch (for the "Merge 'x' into 'main'" labels). Read both before the
        // borrow so the assignment owns no `self` reference.
        let current_branch = self.current_branch();
        let working = self.selected_commit().is_some_and(|c| c.is_working);
        // The working row gets the working-tree verb menu and NO ref fly-outs (the
        // current-branch chip it carries is not a commit-targeted ref to act on here).
        let refs = if working {
            Vec::new()
        } else {
            self.selected_commit()
                .map(|c| ref_menus_for(&c.refs, current_branch.as_deref()))
                .unwrap_or_default()
        };
        self.view.commit_menu = Some(CommitMenu {
            index: self.view.log_sel,
            col,
            row,
            scroll: 0,
            refs,
            current_branch,
            open_ref: None,
            working,
            // Show "Show Diff" only when the diff viewer is currently hidden.
            show_diff_item: !self.view.show_diff,
            marked: Vec::new(),
        });
        true
    }

    /// The current branch name (HEAD's branch), read off the synthetic `<current>` row's
    /// working summary. `None` on a detached HEAD (or before the working row loads).
    fn current_branch(&self) -> Option<String> {
        self.repo
            .commits
            .iter()
            .find(|c| c.is_working)
            .and_then(|c| c.working.as_ref())
            .and_then(|w| w.branch.clone())
    }

    /// Close the open commit context menu, if any.
    fn close_commit_menu(&mut self) -> bool {
        self.view.commit_menu.take().is_some()
    }

    /// Dismiss whichever context menu (commit / files) is open; returns whether one was.
    /// Bitwise `|` so BOTH `take()`s run (never short-circuit a clear). Used as the
    /// click-away when a right-click lands on a files row that opens no menu.
    fn dismiss_context_menus(&mut self) -> bool {
        self.view.commit_menu.take().is_some() | self.view.files_menu.take().is_some()
    }

    /// Right-click a files-pane row: open its context menu anchored at the click cell. The
    /// gating is computed FIRST, from the row at `index`, WITHOUT moving the selection -
    /// `select_file` autosaves a dirty buffer and drops the editor on a move, so that side
    /// effect must not fire for a directory row (no file target) or an empty menu (e.g. a
    /// historical file with the diff already shown), which open nothing. The file is selected
    /// only once a menu will actually open (so Show Diff / Rollback act on the right target).
    fn open_files_menu(&mut self, index: usize, col: u16, row: u16) -> bool {
        let Some(path) = self.visible_files().get(index).and_then(|(_, p)| p.clone()) else {
            // A directory row (no file target) opens the FOLDER menu - the set-verbs over
            // its changed files - instead of nothing.
            return self.open_folder_menu(index, col, row);
        };
        let working = self.selected_commit().is_some_and(|c| c.is_working);
        // Marked-set menu: right-click a MARKED file (with >=2 marks) on the working row opens
        // the bulk verbs over the whole marked set. WebStorm keeps the full single-file menu for
        // a lone selection, so require >=2 marks; an UNMARKED file falls through to the normal
        // menu. Snapshot the set and do NOT `select_file` (it clears marks) - the marks stay
        // visible and the actions read the snapshot.
        if working && self.view.files_marked.len() >= 2 && self.view.files_marked.contains(&path) {
            let marked: Vec<String> = self.view.files_marked.iter().cloned().collect();
            self.close_popups();
            self.view.files_menu = Some(crate::view_state::FilesMenu {
                path,
                col,
                row,
                scroll: 0,
                show_diff_item: false,
                local_changes: true,
                has_head_version: false,
                committed: false,
                is_dir: false,
                marked,
            });
            return true;
        }
        // The local-changes items (copy/create patch, rollback) apply only to a CHANGED file
        // on the working row (Editor > Revert gating); an unchanged All-files-view row would
        // otherwise offer dead no-ops.
        let local_changes = working && TreeNode::changed_paths(&self.repo.tree).contains(&path);
        // "Show Current Revision" needs a committed version to display, so it is offered for a
        // changed working file that exists in HEAD (Modified/Deleted) - never a new (Added) one.
        let has_head_version = local_changes
            && !matches!(model::TreeNode::status_of(&self.repo.tree, &path), Some(model::FileStatus::Added));
        // A file on a HISTORICAL commit (every row of a real commit's tree is a committed blob)
        // gets the read-only inspect group - Show Diff / Show Current Revision / Compare / Show
        // History / Annotate - with no working-tree ops.
        let committed = !working;
        let show_diff_item = !self.view.show_diff;
        // Nothing to offer -> no menu, and crucially NO selection move / autosave; still
        // dismiss any open context menu (the right-click is a click-away there).
        if crate::view_state::files_menu_items(show_diff_item, local_changes, has_head_version, committed, false)
            .is_empty()
        {
            return self.dismiss_context_menus();
        }
        // A menu WILL open: now select the clicked file (keeping the wheel viewport).
        self.select_file(index, true);
        self.close_popups();
        self.view.files_menu = Some(crate::view_state::FilesMenu {
            path,
            col,
            row,
            scroll: 0,
            show_diff_item,
            local_changes,
            has_head_version,
            committed,
            is_dir: false,
            marked: Vec::new(),
        });
        true
    }

    /// Right-click a DIRECTORY row: open the folder context menu (Commit Directory / Copy +
    /// Create Patch / Rollback) over the changed files UNDER that folder. Offered only on the
    /// working row and only when at least one changed file lives under the prefix - otherwise
    /// the items would be dead no-ops. Does NOT move the file selection (a dir has no editable
    /// buffer to autosave, and the folder actions read the snapshotted prefix, not the cursor).
    fn open_folder_menu(&mut self, index: usize, col: u16, row: u16) -> bool {
        let working = self.selected_commit().is_some_and(|c| c.is_working);
        // A visible row maps 1:1 to its tree index (no synthetic root offset).
        let prefix = model::TreeNode::dir_prefix_at(&self.repo.tree, index);
        let has_changes = prefix
            .as_deref()
            .is_some_and(|p| !model::TreeNode::changed_paths_under(&self.repo.tree, p).is_empty());
        let Some(prefix) = prefix.filter(|_| working && has_changes) else {
            // Not a folder with working changes -> no menu; still a click-away dismissal.
            return self.dismiss_context_menus();
        };
        self.close_popups();
        self.view.files_menu = Some(crate::view_state::FilesMenu {
            path: prefix,
            col,
            row,
            scroll: 0,
            show_diff_item: false,
            local_changes: true,
            has_head_version: false,
            committed: false,
            is_dir: true,
            marked: Vec::new(),
        });
        true
    }

    /// Pick a files-menu item: close the menu, then run the action against the menu's OWN
    /// snapshotted target file (`FilesMenu.path`), NOT the live selection - so a list reflow
    /// (an All/Flat toggle, a `--watch` refresh) that drifts the cursor while the menu floats
    /// can never make the destructive Rollback act on a different file than was right-clicked.
    fn files_menu_pick(&mut self, action: crate::view_state::FilesMenuAction) -> bool {
        use crate::view_state::FilesMenuAction;
        let menu = self.view.files_menu.take();
        let is_dir = menu.as_ref().is_some_and(|m| m.is_dir);
        // The marked set this menu acts on (a marked-set menu); empty for a normal file/folder menu.
        let marked: Vec<String> = menu.as_ref().map(|m| m.marked.clone()).unwrap_or_default();
        let path = menu.map(|m| m.path);
        match action {
            // Reveal the diff viewer (the selected file's diff is already what it shows).
            // Always repaint: the menu was just closed, so the frame must redraw to erase it.
            FilesMenuAction::ShowDiff => {
                self.view.show_diff = true;
                true
            }
            // Park the snapshotted file for the runtime's local-changes-diff round-trip
            // (Req::CopyPatch -> Msg::PatchCopied -> clipboard). A directory/dead path is a
            // no-op (the menu only offers this for a changed working file).
            FilesMenuAction::CopyPatch => {
                if let Some(p) = path {
                    self.view.effects.push(Effect::CopyPatch(p));
                }
                true
            }
            // Open the prefilled destination dialog for "Create Patch from Local Changes",
            // threading the snapshotted file path.
            FilesMenuAction::CreatePatch => match path {
                Some(p) => self.open_working_patch_input(p),
                None => true,
            },
            // Open the commit-message dialog scoped to JUST this file (threads the path).
            FilesMenuAction::CommitFile => match path {
                Some(p) => self.open_commit_file_input(p),
                None => true,
            },
            // Open the commit-message dialog scoped to the whole DIRECTORY (threads the prefix).
            FilesMenuAction::CommitFolder => match path {
                Some(p) => self.open_commit_folder_input(p),
                None => true,
            },
            // Open the file's CURRENT version EDITABLE: navigate to the `<current>` working
            // row and re-select the file by path (a read-only HEAD overlay would be useless -
            // you cannot edit a committed blob). From a historical view this is a jump-to-edit.
            FilesMenuAction::ShowCurrentRevision => match path {
                Some(p) => self.show_current_revision(p),
                None => true,
            },
            // Park the picker-list fetch for "Compare with...": the runtime enumerates the
            // options (file revisions / refs) off-thread and replies Msg::PickListLoaded.
            FilesMenuAction::CompareWithRevision => self.open_picklist(
                crate::view_state::PickKind::FileRevisions,
                path,
                crate::view_state::InspectMode::Compare,
            ),
            FilesMenuAction::CompareWithBranch => self.open_picklist(
                crate::view_state::PickKind::Refs,
                path,
                crate::view_state::InspectMode::Compare,
            ),
            // Show History reuses the file-revisions picker, but confirming a row opens the
            // commit-vs-parent diff for that revision (CommitDiff) instead of a working compare.
            FilesMenuAction::ShowHistory => self.open_picklist(
                crate::view_state::PickKind::FileRevisions,
                path,
                crate::view_state::InspectMode::CommitDiff,
            ),
            // Park a read-only blame overlay of the file at the selected revision: the live
            // working tree on `<current>` (an uncommitted line reads "Not Committed Yet"), or
            // the selected commit's blob on a historical row. The runtime fetches it off-thread
            // (Req::Inspect -> backend.blame).
            FilesMenuAction::Annotate => match path {
                Some(p) => {
                    let rev = self.selected_commit_hash().unwrap_or_else(|| WORKING_REV.to_string());
                    let label = if rev == WORKING_REV { "working tree" } else { rev.as_str() };
                    self.view.effects.push(Effect::Inspect(crate::view_state::InspectReq {
                        title: format!("blame {label} - Esc to close"),
                        rev,
                        path: p,
                        mode: crate::view_state::InspectMode::Blame,
                        base: WORKING_REV.to_string(),
                    }));
                    self.status = Status::Notice("Loading blame...".to_string());
                    true
                }
                None => true,
            },
            // Discard the right-clicked file's (or whole folder's) uncommitted changes, behind
            // the confirm modal. A folder rolls back every changed file under its prefix; a file
            // just itself (still only if it remains a changed path).
            FilesMenuAction::Rollback => {
                let paths = match (path, is_dir) {
                    (Some(prefix), true) => TreeNode::changed_paths_under(&self.repo.tree, &prefix),
                    (Some(p), false) => {
                        let changed = TreeNode::changed_paths(&self.repo.tree);
                        Some(p).filter(|p| changed.contains(p)).into_iter().collect()
                    }
                    (None, _) => Vec::new(),
                };
                self.open_revert_modal(paths)
            }
            // Delete the right-clicked file from the working tree + git, behind the confirm.
            FilesMenuAction::DeleteFile => match path {
                Some(p) => self.open_confirm(
                    GitAction::DeleteFile { file: p.clone() },
                    &format!("Delete {p} from the working tree and git? This cannot be undone."),
                ),
                None => true,
            },
            // Marked-set verbs: act on the snapshotted `marked` paths, not the live mark set.
            FilesMenuAction::CommitSelected => self.open_commit_selected_input(marked),
            FilesMenuAction::CopyPatchSelected => {
                self.view.effects.push(Effect::CopyPatchMulti(marked));
                true
            }
            FilesMenuAction::CreatePatchSelected => self.open_create_patch_selected_input(marked),
            // Rollback only the marked files still carrying changes (mirrors the single Rollback gate).
            FilesMenuAction::RollbackSelected => {
                let changed = TreeNode::changed_paths(&self.repo.tree);
                let paths: Vec<String> = marked.into_iter().filter(|p| changed.contains(p)).collect();
                self.open_revert_modal(paths)
            }
            FilesMenuAction::DeleteSelected => self.open_confirm(
                GitAction::DeleteSelected { paths: marked.clone() },
                &format!(
                    "Delete {} selected file(s) from the working tree and git? This cannot be undone.",
                    marked.len()
                ),
            ),
        }
    }

    /// Open the "Create archive" dialog: ONE input with the filename field (prefilled
    /// `/tmp/<repo>-<suffix>.zip`, suffix = today for the working tree else the short hash) PLUS
    /// the format chips (zip / tar.gz / tar) - Tab cycles them, rewriting the extension; the
    /// backend reads the format from the FINAL extension. The rev rides the `commit` slot.
    fn open_archive_input(&mut self) -> bool {
        self.close_popups();
        let rev = self.selected_commit_hash().unwrap_or_else(|| WORKING_REV.to_string());
        let name = if self.view.repo_root_name.is_empty() {
            "project"
        } else {
            self.view.repo_root_name.as_str()
        };
        let suffix = if rev == WORKING_REV {
            // The clock is not in `apply`; the runtime seeds `today` at boot. Fall back to a
            // stable label if it is unseeded (e.g. the synchronous golden path).
            if self.view.today.is_empty() { "current".to_string() } else { self.view.today.clone() }
        } else {
            rev.chars().take(8).collect()
        };
        let initial = format!("/tmp/{name}-{suffix}.zip");
        self.open_input(InputKind::ArchiveProject, initial, Some(rev))
    }

    /// Tab in the archive dialog cycles the filename's extension zip -> tar.gz -> tar -> zip
    /// (the active format = whichever extension the field currently carries). Rewrites just the
    /// extension, preserving the typed path stem; a no-op outside the archive input.
    fn cycle_archive_format(&mut self) -> bool {
        use crate::view_state::ArchiveFormat;
        let Some(Dialog::Input { kind: InputKind::ArchiveProject, field, .. }) = &mut self.view.dialog else {
            return false;
        };
        let text = field.text().to_string();
        // Strip the current known extension, then append the next format's.
        let stem = ArchiveFormat::ALL
            .iter()
            .find_map(|f| text.strip_suffix(&format!(".{}", f.ext())))
            .unwrap_or(text.trim_end_matches('.'));
        let current = ArchiveFormat::ALL
            .iter()
            .position(|f| text.ends_with(&format!(".{}", f.ext())))
            .unwrap_or(0);
        let next = ArchiveFormat::ALL[(current + 1) % ArchiveFormat::ALL.len()];
        *field = crate::view_state::TextField::new(format!("{stem}.{}", next.ext()));
        true
    }

    /// Park a "Compare with..." / "Show History" picker-list fetch for the snapshotted `path`:
    /// the runtime enumerates `kind` (file revisions / refs) off-thread and replies
    /// `Msg::PickListLoaded`, which opens the picker dialog. `mode` is the inspect parked when a
    /// row is confirmed (Compare vs CommitDiff). A directory/dead path is a no-op.
    fn open_picklist(
        &mut self,
        kind: crate::view_state::PickKind,
        path: Option<String>,
        mode: crate::view_state::InspectMode,
    ) -> bool {
        match path {
            Some(p) => {
                self.view.effects.push(Effect::PickList(crate::view_state::PickListReq { kind, path: p, mode }));
                self.status = Status::Notice("Loading...".to_string());
                true
            }
            None => true,
        }
    }

    /// A picker list arrived: open the picker dialog over `items`, UNLESS the reply is stale (the
    /// user navigated away - `path` no longer matches the selection) or the list is empty (nothing
    /// to show -> a Notice). `mode` rides onto the dialog so confirm parks the right inspect.
    /// Mirrors `inspect_loaded`'s guard.
    fn pick_list_loaded(
        &mut self,
        kind: crate::view_state::PickKind,
        path: String,
        items: Vec<crate::view_state::PickItem>,
        mode: crate::view_state::InspectMode,
    ) -> bool {
        if self.selected_file_path().as_deref() != Some(&path) {
            return false; // stale: the selection moved on before the list loaded.
        }
        // A modal opened DURING the async list walk (e.g. the user pressed Alt+C and started
        // typing a commit message) must not be silently clobbered by the arriving picker; drop
        // the late reply instead. `close_popups` below clears menus/search but NOT a dialog.
        if self.view.dialog.is_some() {
            return false;
        }
        use crate::view_state::{InspectMode, PickKind};
        if items.is_empty() {
            let what = match (mode, kind) {
                (InspectMode::CommitDiff, _) => "No history for this file",
                (_, PickKind::FileRevisions) => "No earlier revisions of this file",
                (_, PickKind::Refs) => "No branches or tags to compare",
            };
            self.status = Status::Notice(what.to_string());
            return true;
        }
        let title = match (mode, kind) {
            (InspectMode::CommitDiff, _) => "File history",
            (_, PickKind::FileRevisions) => "Compare with revision",
            (_, PickKind::Refs) => "Compare with branch or tag",
        };
        self.close_popups();
        self.view.dialog = Some(Dialog::Picker { title: title.to_string(), path, items, sel: 0, mode });
        true
    }

    /// Park a branch/tag-list read for the global Git menu's Branches/Merge/Rebase op; the
    /// reply (`Msg::RefListLoaded`) opens the ref picker.
    fn open_refpick(&mut self, op: crate::view_state::RefOp) -> bool {
        self.close_popups();
        self.view.effects.push(Effect::RefPick(op));
        self.status = Status::Notice("Loading branches...".to_string());
        true
    }

    /// The branch/tag list arrived: open the ref picker for `op`. Dropped if a modal opened
    /// during the load; an empty list (no other branch/tag) is a Notice.
    fn ref_list_loaded(
        &mut self,
        op: crate::view_state::RefOp,
        items: Vec<crate::view_state::PickItem>,
    ) -> bool {
        if self.view.dialog.is_some() {
            return false;
        }
        if items.is_empty() {
            self.status = Status::Notice("No branches or tags".to_string());
            return true;
        }
        self.close_popups();
        self.view.dialog = Some(Dialog::RefPick { items, sel: 0, op });
        true
    }

    /// The repo's remote list arrived: open the Manage Remotes dialog. Dropped if a modal
    /// opened during the async walk (mirrors `pick_list_loaded`); an empty list still opens (the
    /// user can add the first remote).
    fn remotes_loaded(&mut self, remotes: Vec<(String, String)>) -> bool {
        if self.view.dialog.is_some() {
            return false;
        }
        self.close_popups();
        self.view.dialog = Some(Dialog::Remotes { remotes, sel: 0 });
        true
    }

    /// Open the add-remote input (a single `name url` line) from the Manage Remotes dialog.
    fn open_remote_add(&mut self) -> bool {
        self.open_input_full(
            InputKind::RemoteAdd,
            String::new(),
            None,
            Some("Enter: name url (e.g. origin https://...)".to_string()),
            None,
        )
    }

    /// Open the confirm to remove the selected remote from the Manage Remotes dialog. A no-op
    /// (status hint) when the list is empty (nothing selected).
    fn open_remote_remove(&mut self) -> bool {
        let name = match &self.view.dialog {
            Some(Dialog::Remotes { remotes, sel }) => remotes.get(*sel).map(|(n, _)| n.clone()),
            _ => None,
        };
        match name {
            Some(name) => self.open_confirm(
                GitAction::RemoteRemove { name: name.clone() },
                &format!("Remove remote '{name}'?"),
            ),
            None => {
                self.status = Status::Notice("No remote to remove".to_string());
                true
            }
        }
    }

    /// "Show Current Revision": jump to the file's live, EDITABLE working version. Navigate the
    /// log to the synthetic `<current>` working row (the only editable context) and park `path`
    /// to re-select once its tree loads, so a CHANGED file opens as the editable buffer. The
    /// working row is filter/search-exempt so it is always present. If the file has NO working
    /// change (no row in the `<current>` pane - nothing to edit), `apply_tree` falls back to a
    /// read-only HEAD overlay (via `parked_revision`) instead of landing on an unrelated file.
    /// Reuses the Show-History navigate-and-re-select mechanism.
    fn show_current_revision(&mut self, path: String) -> bool {
        // Already on `<current>`: the working tree is loaded, so reveal the file in place (it
        // opens editable) - navigating to the same row is a no-op that would not reveal it.
        if self.selected_commit().is_some_and(|c| c.is_working) {
            if !self.reveal_file_by_path(&path) {
                self.show_committed_revision(path); // unchanged file -> read-only HEAD overlay
            }
            return true;
        }
        // On a HISTORICAL commit: navigate to the working row and park the file to reveal once
        // its tree loads (`apply_tree` opens it editable, or falls back to the read-only overlay).
        let vis = model::visible_commits(&self.repo, &self.view);
        match vis.iter().position(|&i| self.repo.commits[i].is_working) {
            Some(pos) => {
                self.select_commit(pos, false);
                self.view.parked_file_path = Some(path.clone());
                self.view.parked_revision = Some(path);
                true
            }
            None => {
                self.status = Status::Notice("No working revision".to_string());
                true
            }
        }
    }

    /// Park a read-only inspect load of `path`'s committed HEAD content (the "Show Current
    /// Revision" fallback for an unchanged file - nothing to edit, so show what is committed).
    fn show_committed_revision(&mut self, path: String) {
        self.view.effects.push(Effect::Inspect(crate::view_state::InspectReq {
            rev: "HEAD".to_string(),
            path,
            title: "HEAD - Esc to close".to_string(),
            mode: crate::view_state::InspectMode::Source,
            base: WORKING_REV.to_string(),
        }));
        self.status = Status::Notice("Loading revision...".to_string());
    }

    /// Show History confirm: navigate the log to commit `rev` (its short hash, as the picker
    /// listed it) and park `path` to re-select once that commit's tree loads, so the historical
    /// commit's own read-only (parent-vs-commit) diff for the file shows in place and the log
    /// cursor lands on it. If the commit is filtered out of the visible log, fall back to the
    /// inspect overlay so the file's history diff is still reachable.
    fn show_history_revision(&mut self, rev: &str, path: String) -> bool {
        let vis = model::visible_commits(&self.repo, &self.view);
        match vis.iter().position(|&i| self.repo.commits[i].hash == rev) {
            Some(pos) => {
                self.select_commit(pos, false);
                self.view.parked_file_path = Some(path);
            }
            None => {
                self.view.effects.push(Effect::Inspect(crate::view_state::InspectReq {
                    rev: rev.to_string(),
                    path,
                    title: format!("{rev} - Esc to close"),
                    mode: crate::view_state::InspectMode::CommitDiff,
                    base: WORKING_REV.to_string(),
                }));
                self.status = Status::Notice("Loading history...".to_string());
            }
        }
        true
    }

    /// Open the "Commit File" dialog for `file`: an empty message field, the FILE path
    /// snapshotted in the `commit` slot (reused by `dialog_confirm` as the `git commit
    /// -- <file>` target).
    fn open_commit_file_input(&mut self, file: String) -> bool {
        self.open_input(InputKind::CommitFile, String::new(), Some(file))
    }

    /// Open the "Commit Directory" dialog for `dir`: an empty message field, the folder prefix
    /// snapshotted in the `commit` slot (reused by `dialog_confirm` as the `git commit -- <dir>`
    /// target).
    fn open_commit_folder_input(&mut self, dir: String) -> bool {
        self.open_input(InputKind::CommitFolder, String::new(), Some(dir))
    }

    /// Open the "Create Patch from Local Changes" dialog for `file`: prefill an editable
    /// `/tmp/<basename>.patch` destination and snapshot the FILE path in the `commit` slot
    /// (reused by `dialog_confirm` as the `git diff HEAD -- <file>` source).
    fn open_working_patch_input(&mut self, file: String) -> bool {
        let base = file.rsplit('/').next().unwrap_or(&file);
        let initial = format!("/tmp/{base}.patch");
        self.open_input(InputKind::CreateWorkingPatch, initial, Some(file))
    }

    /// Open the "Commit Selected" dialog: an empty message field, the MARKED paths parked in
    /// `parked_marked` (drained by `dialog_confirm` into `GitAction::CommitSelected`).
    fn open_commit_selected_input(&mut self, paths: Vec<String>) -> bool {
        self.view.parked_marked = paths;
        self.open_input(InputKind::CommitSelected, String::new(), None)
    }

    /// Open the "Create Patch from Selected" dialog: prefill `/tmp/selected.patch`, the MARKED
    /// paths parked in `parked_marked` (drained into `GitAction::CreatePatchSelected`).
    fn open_create_patch_selected_input(&mut self, paths: Vec<String>) -> bool {
        self.view.parked_marked = paths;
        self.open_input(InputKind::CreatePatchSelected, "/tmp/selected.patch".to_string(), None)
    }

    /// Toggle a branch/tag fly-out: open `ref_idx`'s submenu, or close it if already open.
    fn open_ref_submenu(&mut self, ref_idx: usize) -> bool {
        match self.view.commit_menu.as_mut() {
            Some(cm) if ref_idx < cm.refs.len() => {
                cm.open_ref = (cm.open_ref != Some(ref_idx)).then_some(ref_idx);
                true
            }
            _ => false,
        }
    }

    /// Pick an action from an open branch/tag fly-out: close the whole menu, then run the
    /// ref action (re-dispatching to a confirm/input dialog or a parked `GitAction`, the
    /// same one-home pattern as [`Self::commit_menu_pick`]).
    fn ref_menu_pick(&mut self, ref_idx: usize, action: RefAction) -> bool {
        let Some(rm) =
            self.view.commit_menu.as_ref().and_then(|cm| cm.refs.get(ref_idx)).cloned()
        else {
            return false;
        };
        let current = self.view.commit_menu.as_ref().and_then(|cm| cm.current_branch.clone());
        self.view.commit_menu = None;
        self.dispatch_ref_action(action, &rm, current.as_deref())
    }

    /// Map a ref action to its dialog/`GitAction` (the single ref-action home). A branch
    /// management op opens a confirm or input; the conflict/network ops warn in the prompt.
    fn dispatch_ref_action(&mut self, action: RefAction, rm: &RefMenu, current: Option<&str>) -> bool {
        let name = rm.name.clone();
        let cur = current.unwrap_or("HEAD").to_string();
        match action {
            RefAction::Checkout => {
                // A local branch ATTACHES HEAD; a remote-tracking branch or tag detaches it
                // (you land on a commit, not a branch). Warn like the commit-level Checkout
                // Revision does, so the detach is never a surprise.
                let prompt = if rm.kind == RefMenuKind::LocalBranch {
                    format!("Checkout '{name}'?")
                } else {
                    format!("Checkout '{name}'? HEAD will detach.")
                };
                self.open_confirm(GitAction::CheckoutRef { name: name.clone() }, &prompt)
            }
            // Roots a new branch at the selected commit (where this ref points) - the same
            // dialog as the menu's New Branch, so the logic stays in one place.
            RefAction::NewBranchFrom => self.open_branch_input(),
            RefAction::Merge => self.open_confirm(
                GitAction::MergeRef { name: name.clone() },
                &format!("Merge '{name}' into '{cur}'?"),
            ),
            RefAction::RebaseOnto => self.open_confirm(
                GitAction::RebaseOnto { name: name.clone() },
                &format!("Rebase '{cur}' onto '{name}'? Rewrites history."),
            ),
            RefAction::Push => self
                .open_confirm(GitAction::PushRef { name: name.clone() }, &format!("Push '{name}' to its remote?")),
            RefAction::PullRebase => self.open_pull_ref_confirm(&name, true, &cur),
            RefAction::PullMerge => self.open_pull_ref_confirm(&name, false, &cur),
            RefAction::Rename => self.open_rename_input(&name),
            RefAction::Delete => self.open_ref_delete_confirm(rm),
        }
    }

    /// Pull a remote ref into the current branch: split `origin/main` into remote + branch
    /// (the first `/`; remote names never contain one), then confirm rebase vs merge.
    fn open_pull_ref_confirm(&mut self, ref_name: &str, rebase: bool, cur: &str) -> bool {
        let (remote, branch) = ref_name.split_once('/').unwrap_or(("origin", ref_name));
        let how = if rebase { "Rebase" } else { "Merge" };
        self.open_confirm(
            GitAction::PullRef { remote: remote.to_string(), branch: branch.to_string(), rebase },
            &format!("Pull '{ref_name}' into '{cur}' using {how}?"),
        )
    }

    /// Open the Rename input dialog for a local branch: prefilled with the current name,
    /// the OLD name snapshotted in the dialog's `commit` slot (reused as the rename source).
    fn open_rename_input(&mut self, old: &str) -> bool {
        self.open_input_full(InputKind::RenameBranch, old.to_string(), Some(old.to_string()), None, None)
    }

    /// Confirm a branch/tag delete. A branch uses the SAFE `git branch -d`: on an unmerged
    /// branch git refuses with a clear "not fully merged - run 'git branch -D'" error (shown
    /// as a Notice), so the UI never silently discards unpushed work - matching the app's
    /// abort-on-conflict posture. Force-delete is a deliberate CLI-only escape hatch.
    fn open_ref_delete_confirm(&mut self, rm: &RefMenu) -> bool {
        let name = rm.name.clone();
        if rm.kind == RefMenuKind::Tag {
            self.open_confirm(GitAction::TagDelete { name: name.clone() }, &format!("Delete tag '{name}'?"))
        } else {
            self.open_confirm(
                GitAction::BranchDelete { name: name.clone() },
                &format!("Delete branch '{name}'?"),
            )
        }
    }

    /// Pick a commit-menu item: close the menu, then run the action by re-dispatching
    /// an existing intent (keeps the action logic in one place, like [`Self::menu_pick`]).
    fn commit_menu_pick(&mut self, action: CommitMenuAction) -> bool {
        // Capture the marked set (multi-commit menu) before closing the menu.
        let marked = self.view.commit_menu.take().map(|m| m.marked).unwrap_or_default();
        match action {
            // -- multi-commit set verbs (act on the snapshotted marked set) -------------
            CommitMenuAction::CherryPickSelected => self.open_confirm(
                GitAction::CherryPickSelected { commits: marked.clone() },
                &format!("Cherry-pick {} selected commits onto the current branch?", marked.len()),
            ),
            CommitMenuAction::CreatePatchSeries => {
                self.view.parked_marked = marked;
                self.open_input(InputKind::CreatePatchSeries, "/tmp/patches".to_string(), None)
            }
            CommitMenuAction::CopyRevision => self.copy_revision(),
            CommitMenuAction::ShowDiff => {
                // Reveal the diff viewer for the selected commit (it already shows
                // commit-vs-working when visible). Always return true: the menu was
                // just closed above, so the frame MUST repaint to erase the popup -
                // returning false on the already-shown case leaves a ghost menu.
                self.view.show_diff = true;
                true
            }
            CommitMenuAction::CreatePatch => self.open_patch_input(),
            CommitMenuAction::EditMessage => self.open_reword_input(),
            CommitMenuAction::NewBranch => self.open_branch_input(),
            CommitMenuAction::NewTag => self.open_tag_input(),
            CommitMenuAction::Checkout => self.open_commit_confirm(
                |commit| GitAction::Checkout { commit },
                |h| format!("Checkout {h}? HEAD will detach."),
            ),
            CommitMenuAction::CherryPick => self.open_commit_confirm(
                |commit| GitAction::CherryPick { commit },
                |h| format!("Cherry-pick {h} onto the current branch?"),
            ),
            CommitMenuAction::RevertCommit => self.open_commit_confirm(
                |commit| GitAction::RevertCommit { commit },
                |h| format!("Revert {h}? Creates an inverse commit."),
            ),
            CommitMenuAction::ResetHere => self.open_reset_picker(),
            CommitMenuAction::UndoCommit => self.open_undo_confirm(),
            CommitMenuAction::InteractiveRebase => self.open_rebase_dialog(),
            // -- synthetic <current> working-row verbs --------------------------------
            CommitMenuAction::CommitChanges => self.open_input(InputKind::Commit, String::new(), None),
            CommitMenuAction::StashChanges => {
                self.open_confirm(GitAction::Stash, "Stash all changes? (restore with git stash pop)")
            }
            CommitMenuAction::CreateArchive => self.open_archive_input(),
            CommitMenuAction::DiscardChanges => self.open_confirm(
                GitAction::DiscardAll,
                "Discard all uncommitted changes, including new files? This cannot be undone.",
            ),
        }
    }

    /// Open the interactive-rebase mark-items dialog over `picked..HEAD`: walk first-parent
    /// from HEAD down to (and including) the selected commit, one Pick row each, newest
    /// first. Rejects a commit not on HEAD's first-parent line (nothing to rebase) and the
    /// synthetic working row. Warns when the range includes published commits.
    fn open_rebase_dialog(&mut self) -> bool {
        let Some(base) = self.selected_real_commit().map(|c| c.full_hash.clone()) else {
            self.status = Status::Notice("Select a commit".to_string());
            return true;
        };
        let mut steps = Vec::new();
        let mut any_pushed = false;
        let mut cur = self.repo.commits.iter().find(|c| c.head);
        loop {
            let Some(c) = cur else {
                // Walked off the first-parent chain without reaching the picked commit:
                // it is not an ancestor of HEAD, so there is nothing to rebase from it.
                self.status = Status::Notice("Not on the current branch".to_string());
                return true;
            };
            // A merge in the range can't be represented as a Pick/Drop row (and a drop sed
            // would silently no-op the `merge` todo line), so refuse the whole range.
            if c.parents.len() > 1 {
                self.status = Status::Notice("Cannot rebase across a merge commit".to_string());
                return true;
            }
            any_pushed |= self.repo.has_remotes && !self.repo.unpushed.contains(&c.full_hash);
            steps.push(crate::view_state::RebaseStep {
                short: c.hash.clone(),
                full: c.full_hash.clone(),
                subject: model::commit_subject(c),
                action: crate::view_state::RebaseAction::Pick,
            });
            if c.full_hash == base {
                break;
            }
            // Follow the mainline parent (short hash) to the next older trunk commit.
            cur = c.parents.first().and_then(|p| self.repo.commits.iter().find(|x| &x.hash == p));
        }
        let note = any_pushed
            .then(|| "Rewrites published history: avoid on pushed commits.".to_string());
        self.close_popups();
        self.view.dialog = Some(Dialog::Rebase { steps, sel: 0, base, note });
        true
    }

    /// Open the reset-mode picker for the selected commit (snapshotting its hash). Warns
    /// when the current branch is published (a reset that rewinds past pushed commits
    /// rewrites published history). Hints on the synthetic working row.
    fn open_reset_picker(&mut self) -> bool {
        let Some(commit) = self.selected_real_commit().map(|c| c.full_hash.clone()) else {
            self.status = Status::Notice("Select a commit".to_string());
            return true;
        };
        // The branch tip is published when the repo has remotes and HEAD's commit is not
        // in the unpushed set - resetting it away rewrites history others may have pulled.
        let note = self
            .repo
            .commits
            .iter()
            .find(|c| c.head)
            .filter(|c| self.repo.has_remotes && !self.repo.unpushed.contains(&c.full_hash))
            .map(|_| "Branch is pushed: a reset rewrites published history.".to_string());
        self.close_popups();
        self.view.dialog = Some(Dialog::Choice { kind: ChoiceKind::ResetMode, sel: 0, commit, note });
        true
    }

    /// Open the Pull strategy picker (WebStorm's "Update Project"): fast-forward-only / merge /
    /// rebase. No commit target (it pulls the current branch from its upstream), so the Choice's
    /// `commit` slot is empty - the PullStrategy confirm ignores it.
    fn open_pull_picker(&mut self) -> bool {
        self.close_popups();
        self.view.dialog = Some(Dialog::Choice {
            kind: ChoiceKind::PullStrategy,
            sel: 0,
            commit: String::new(),
            note: None,
        });
        true
    }

    /// One-click Update Project (toolbar refresh button + Git menu): park the fetch+pull op
    /// and show progress. No confirm - it is non-destructive and abort-safe (the backend
    /// tolerates no-remote/no-upstream and aborts a conflicting rebase). The result surfaces
    /// as a Notice; the repo reloads on the reply.
    fn request_update(&mut self) -> bool {
        self.close_popups();
        self.view.effects.push(Effect::Git(GitAction::UpdateProject));
        self.status = Status::Notice("Updating project...".to_string());
        true
    }

    /// Open the Undo-Commit confirmation. Undo applies ONLY to the latest commit (a soft
    /// reset of the tip), so it is enabled only when the selected row is HEAD; any other
    /// row (or the working row) hints instead.
    fn open_undo_confirm(&mut self) -> bool {
        match self.selected_real_commit() {
            Some(c) if c.head => {
                self.open_confirm(
                    GitAction::UndoCommit,
                    "Undo the last commit? Its changes return to staging.",
                )
            }
            _ => {
                self.status = Status::Notice("Undo applies only to the latest commit".to_string());
                true
            }
        }
    }

    /// Open a confirm modal for a commit-targeted git write: snapshot the selected REAL
    /// commit's full hash into the action `build`s from it, with a `prompt` built from
    /// the short hash. Hints on the synthetic working row (no real commit to target).
    /// The single home for the checkout / cherry-pick / revert confirm flow.
    fn open_commit_confirm(
        &mut self,
        build: impl FnOnce(String) -> GitAction,
        prompt: impl FnOnce(&str) -> String,
    ) -> bool {
        let Some((full, short)) =
            self.selected_real_commit().map(|c| (c.full_hash.clone(), c.hash.clone()))
        else {
            self.status = Status::Notice("Select a commit".to_string());
            return true;
        };
        let text = prompt(&short);
        self.open_confirm(build(full), &text)
    }

    /// Copy the selected commit's full revision hash to the system clipboard. A
    /// status hint when the synthetic working row is selected (it has no hash).
    fn copy_revision(&mut self) -> bool {
        match self.selected_real_commit().map(|c| c.full_hash.clone()) {
            Some(h) => {
                self.view.effects.push(Effect::Clipboard(h));
                self.status = Status::Notice("Copied revision".to_string());
            }
            None => self.status = Status::Notice("Select a commit to copy".to_string()),
        }
        true
    }

    /// Leave editing (Esc): autosave a dirty buffer, then move focus off the diff
    /// pane back to the files list. The buffer stays loaded (re-focus to keep editing).
    fn diff_blur(&mut self) -> bool {
        if self.view.focus != Pane::Diff {
            return false;
        }
        self.flush_editor_if_dirty();
        self.view.focus = Pane::Files;
        true
    }

    /// Autosave the editable buffer if it has unsaved edits: parks a `Req::SaveFile`
    /// for the runtime (ZERO-IO here). Called on Esc and before the selection moves to
    /// another file/commit (so navigating away never silently loses edits).
    fn flush_editor_if_dirty(&mut self) {
        // Autosave OFF: navigating away does NOT write; only Ctrl+S persists, so
        // unsaved edits are intentionally dropped (the deliberate "no autosave" path).
        if !self.view.autosave {
            return;
        }
        if let Some(e) = &self.view.editor {
            if e.loaded && e.dirty {
                self.view.effects.push(Effect::Save { path: e.path.clone(), content: e.to_content() });
            }
        }
    }

    /// When autosave is OFF and the editable buffer has unsaved edits, surface a status
    /// notice as it is dropped on navigation, so the loss is never silent. No-op when
    /// autosave is on (the buffer was just parked for saving by `flush_editor_if_dirty`)
    /// or the buffer is clean. Pair this with `flush_editor_if_dirty` right before the
    /// editor is cleared on a file/commit change.
    fn warn_if_dropping_unsaved(&mut self) {
        if self.view.autosave {
            return;
        }
        if let Some(e) = &self.view.editor {
            if e.loaded && e.dirty {
                self.status =
                    Status::Notice(format!("Discarded unsaved edits to {} (autosave off)", e.path));
            }
        }
    }

    /// Park the editable buffer for the runtime to write off-thread (Ctrl+S; ZERO-IO
    /// here). No-op without a loaded buffer.
    fn save_editor(&mut self) -> bool {
        let editor = match &self.view.editor {
            Some(e) if e.loaded => e,
            _ => return false,
        };
        self.view.effects.push(Effect::Save { path: editor.path.clone(), content: editor.to_content() });
        self.status = Status::Notice(format!("Saving {}...", editor.path));
        true
    }

    /// Apply a text edit op to the buffer (using the shared clipboard register), then
    /// recompute the live diff so the right side + bands update immediately. No-op
    /// without a loaded buffer.
    fn edit(&mut self, op: crate::message::EditOp) -> bool {
        use crate::message::EditOp;
        let is_place = matches!(op, EditOp::Place { .. });
        // Mouse gestures (click / drag / double / triple) target text already on screen,
        // so they KEEP the parked wheel free-scroll - the viewport must not jump. Keyboard
        // caret moves + text edits clear it so the caret snaps back into view.
        let mouse_gesture = matches!(
            op,
            EditOp::Place { .. } | EditOp::SelectWord { .. } | EditOp::SelectLine { .. }
        );
        // Only ops that mutate the buffer TEXT need the live diff rebuilt. Cursor/
        // selection moves (Move/Place/SelectWord/SelectLine/SelectAll) redraw
        // the caret/band but leave the text identical, so they must NOT trigger the
        // O(file) live_diff - that is the "never full-file per keystroke" rule.
        let text_op = matches!(
            op,
            EditOp::Insert(_)
                | EditOp::Newline
                | EditOp::Backspace
                | EditOp::Delete
                | EditOp::Cut
                | EditOp::Paste
                | EditOp::Undo
                | EditOp::Redo
        );
        // Copy/Cut also feed the OS clipboard (see the mirror below), not only the in-app register.
        let copy_op = matches!(op, EditOp::Copy | EditOp::Cut);
        let view = &mut self.view;
        let changed = match &mut view.editor {
            Some(e) if e.loaded => e.apply_op(op, &mut view.clipboard),
            _ => return false,
        };
        // Ctrl+C / Ctrl+X in the editable diff must reach the SYSTEM clipboard too: `apply_op`
        // only fills the in-app `clipboard` register (which feeds in-app Paste). Mirror a
        // non-empty Copy/Cut into an `Effect::Clipboard` so the runtime hands
        // it to wl-copy/xclip - matching the read-only diff's `Msg::CopyText` path.
        if copy_op && !self.view.clipboard.is_empty() {
            self.view.effects.push(Effect::Clipboard(self.view.clipboard.clone()));
            self.status = Status::Notice("Copied selection".to_string());
        }
        // A click into the editable diff focuses it and blurs any active search field, so the
        // next keystroke edits the buffer instead of a still-focused filter input.
        if is_place {
            self.view.focus = Pane::Diff;
            self.blur_searches();
        }
        // Any edit / caret move restores the cursor-follow scroll: drop the wheel's free
        // overrides (vertical AND horizontal) so the caret is brought back into view.
        // Clearing a live override must force a repaint even when the op itself moved
        // nothing (e.g. a boundary arrow). A mouse gesture keeps the overrides - it acts
        // on a cell already visible.
        let dropped_v = !mouse_gesture && self.view.edit_scroll.take().is_some();
        let dropped_h = !mouse_gesture && self.view.diff_hscroll.take().is_some();
        let dropped_free_scroll = dropped_v || dropped_h;
        // A mouse Place (click/drag) on an editable buffer also focuses the diff pane
        // so the keyboard then edits the buffer the user just clicked into.
        if is_place {
            self.view.focus = Pane::Diff;
        }
        if changed && text_op {
            self.refresh_edit_preview();
        }
        changed || dropped_free_scroll
    }

    /// Fold the unchanged middle of a READ-ONLY `Diff` view when "Hide unchanged" is on,
    /// so a picked commit's diff (and a compare/inspect overlay) folds with the SAME
    /// `textdiff::fold_unchanged` the editable `<current>` uses. A `Source`/`Binary` view
    /// (an unchanged file / binary) has no unchanged middle to fold, so it passes through.
    fn fold_readonly(&self, view: FileView) -> FileView {
        match view {
            FileView::Diff(mut d) if self.view.hide_unchanged => {
                d.lines = crate::textdiff::fold_unchanged(d.lines);
                FileView::Diff(d)
            }
            other => other,
        }
    }

    /// Recompute the live diff preview from the editor's base + buffer. The single
    /// place the editable diff is built, so an open and every keystroke agree.
    fn refresh_edit_preview(&mut self) {
        if let Some(e) = &self.view.editor {
            let mut diff = crate::textdiff::live_diff(&e.base, &e.lines, &e.path, "working tree");
            // "Hide unchanged" collapses the unchanged runs to fold markers so the live
            // diff shows only the changes (a commit diff already arrives folded from git).
            if self.view.hide_unchanged {
                diff.lines = crate::textdiff::fold_unchanged(diff.lines);
            }
            self.repo.preview = Some(FileView::Diff(diff));
        }
    }

    /// Request a hunk revert on the focused diff line. Only valid with the diff pane
    /// focused on a CHANGED line of a real diff. First request ARMS (status warns); a
    /// second confirms and parks `(commit, path, hunk)` for the runtime. A context
    /// line (or non-diff preview) -> a status hint, nothing armed.
    fn request_hunk_revert(&mut self) -> bool {
        if self.view.focus != Pane::Diff {
            return false;
        }
        // The "<current>" row's diff is the LIVE editable working file (its `hunk`
        // indices come from textdiff's contiguous-run heuristic, not libgit2's
        // commit->parent grouping). A hunk revert there has no real commit to revert
        // against (the sentinel WORKING_REV would error at find_commit), so steer the
        // user to edit/undo the file directly instead.
        if self.selected_commit_hash().as_deref() == Some(WORKING_REV) {
            self.view.hunk_revert_armed = false;
            self.status =
                Status::Notice("Edit the file directly to undo working changes".to_string());
            return true;
        }
        let diff = match self.repo.preview.as_ref() {
            Some(FileView::Diff(d)) => d,
            _ => {
                self.status = Status::Notice("No diff to revert".to_string());
                return true;
            }
        };
        let line = match diff.lines.get(self.view.diff_cursor) {
            Some(l) if l.kind != LineKind::Context => l,
            _ => {
                self.view.hunk_revert_armed = false;
                self.status = Status::Notice("No change on this line to revert".to_string());
                return true;
            }
        };
        let commit = match self.selected_commit_hash() {
            Some(c) => c,
            None => return false,
        };
        if !self.view.hunk_revert_armed {
            self.view.hunk_revert_armed = true;
            self.status =
                Status::Notice("Revert this hunk in the working tree? Enter again to confirm".to_string());
            return true;
        }
        self.view.hunk_revert_armed = false;
        self.view.effects.push(Effect::HunkRevert { commit, path: diff.path.clone(), hunk: line.hunk });
        self.status = Status::Notice("Reverting hunk...".to_string());
        true
    }

    /// The selected file's two diff sides arrived: set up the editable buffer + the
    /// live diff. Applied only while the (commit, path) is still selected (a staleness
    /// guard mirroring the preview guard), so a fast navigation cannot install a stale
    /// buffer. Does NOT steal focus - editing begins when the user focuses the diff.
    fn edit_file_loaded(&mut self, commit: &str, path: &str, base: Option<&str>, work: &str) -> bool {
        let stale = self.selected_commit_hash().as_deref() != Some(commit)
            || self.selected_file_path().as_deref() != Some(path);
        if stale {
            return false;
        }
        let mut editor = crate::view_state::EditorState::opening(path.to_string());
        editor.load_edit(base, work);
        self.view.editor = Some(editor);
        self.refresh_edit_preview();
        // Decide the full-width layout ONCE, at open: a file with no changes (identical
        // base/work) has nothing to diff, so it shows as one full-width editable pane
        // instead of the same text on both sides. Held stable as the user types (the
        // per-keystroke `refresh_edit_preview` never touches this), so it does not reflow.
        self.view.diff_full_width = !self.preview_has_changes();
        // Opening a file clears a stale status hint - UNLESS a git-action result notice is
        // sticky (this open is the reload's auto-open, not a user navigation), so the
        // "Committed/Deleted/..." outcome survives until the user actually navigates.
        if !self.view.notice_sticky && matches!(self.status, Status::Notice(_)) {
            self.status = Status::Ready;
        }
        // The flag's whole job is to survive THIS one reload auto-open; consume it now so it
        // cannot leak true into a later unrelated transient notice (e.g. a Copy) before the
        // next navigation un-sticks it.
        self.view.notice_sticky = false;
        true
    }

    /// Whether the current diff preview carries any non-context (added/removed) line.
    /// Drives the "stable at open" full-width decision; a `Source`/`Binary`/absent
    /// preview counts as no changes.
    fn preview_has_changes(&self) -> bool {
        matches!(
            self.repo.preview.as_ref(),
            Some(FileView::Diff(d)) if d.lines.iter().any(|l| l.kind != LineKind::Context)
        )
    }

    /// The save finished: clear the dirty flag if it is still the open file. The base
    /// stays the committed side (saving the working tree does not change the commit),
    /// so the live diff keeps showing the still-uncommitted changes.
    fn file_saved(&mut self, path: &str) -> bool {
        if let Some(e) = &mut self.view.editor {
            if e.path == path {
                e.dirty = false;
            }
        }
        self.status = Status::Notice(format!("Saved {path}"));
        true
    }

    /// Close the open dropdown, if any.
    fn close_dropdown(&mut self) -> bool {
        if self.view.open_dropdown.take().is_some() {
            self.view.dropdown_sel = 0;
            true
        } else {
            false
        }
    }

    /// Move the highlight in the open dropdown by `delta`, clamped to its options.
    fn dropdown_move(&mut self, delta: isize) -> bool {
        let kind = match self.view.open_dropdown {
            Some(k) => k,
            None => return false,
        };
        let len = model::filter_options(&self.repo, kind).len();
        let next = step(self.view.dropdown_sel, delta, len);
        let changed = next != self.view.dropdown_sel;
        self.view.dropdown_sel = next;
        changed
    }

    /// Pick option `row` in the open dropdown: row 0 clears the filter, any other
    /// row sets it. Closes the popup and re-filters.
    fn dropdown_pick(&mut self, row: usize) -> bool {
        let kind = match self.view.open_dropdown {
            Some(k) => k,
            None => return false,
        };
        let options = model::filter_options(&self.repo, kind);
        let sel = match row {
            0 => None,
            r => options.get(r).cloned(),
        };
        self.view.open_dropdown = None;
        self.view.dropdown_sel = 0;
        self.apply_filter(kind, sel);
        true
    }

    /// Write a filter selection (`None` -> "All") and run the re-clamp/rebuild tail.
    fn apply_filter(&mut self, kind: FilterKind, sel: Option<String>) {
        *self.view.filter_mut(kind) = sel;
        self.after_filter_change();
    }

    /// The dropdown row matching `kind`'s current selection (0 = "All"/none).
    fn selected_option_row(&self, kind: FilterKind) -> usize {
        let sel = match self.view.filter(kind) {
            Some(s) => s,
            None => return 0,
        };
        model::filter_options(&self.repo, kind)
            .iter()
            .position(|o| o == sel)
            .unwrap_or(0)
    }

    /// Re-clamp both selections into range after the repo data changes. The log
    /// selection is bounded by the FILTERED list, not the raw commit count.
    fn clamp_selections(&mut self) {
        let log_max = self.visible_len().saturating_sub(1);
        self.view.log_sel = self.view.log_sel.min(log_max);
        // Flat-aware: bound against what the panel actually renders (the lone files
        // index site that must honor the Flat toggle), so an initial/--watch reload in
        // flat mode does not clamp the selection against the taller nested-tree count.
        let files_max = self.files_rows_len().saturating_sub(1);
        self.view.files_sel = self.view.files_sel.min(files_max);
        // The repo data changed shape: any wheel free-scroll offset is stale, refollow.
        self.view.log_scroll = None;
        self.view.files_scroll = None;
    }

}

/// Clamped index step; `len == 0` stays at 0.
fn step(cur: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let max = len as isize - 1;
    (cur as isize + delta).clamp(0, max) as usize
}

// -- B2 seam tests: Msg -> state, ZERO IO -------------------------------------
//
// Built from `RepoModel::empty()` or tiny hand-built models (NO fixtures, NO
// backend). They pin the apply contract: the staleness guards, preview clearing
// without a data:: call, and the relocated scroll clamps.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffLine, FileDiff, LineKind};
    use crate::model::{Commit, CommitDetail, FileStatus, Signature, SubjectSpan};

    fn sig(name: &str) -> Signature {
        Signature {
            name: name.to_string(),
            email: String::new(),
            when: "01.01.2026, 00:00".to_string(),
        }
    }

    fn commit(hash: &str, subject: &str) -> Commit {
        Commit {
            hash: hash.to_string(),
            full_hash: hash.to_string(),
            parents: vec![],
            subject: vec![SubjectSpan::plain(subject)],
            refs: vec![],
            author: "Tester".to_string(),
            date: "01.01.2026, 00:00".to_string(),
            date_label: "01.01.2026, 00:00".to_string(),
            is_me: false,
            head: false,
            containing_branches: vec![],
            is_working: false,
            working: None,
        }
    }

    fn detail(hash: &str, subject: &str) -> CommitDetail {
        CommitDetail {
            subject: subject.to_string(),
            short_hash: hash.to_string(),
            author: sig("Tester"),
            committer: sig("Tester"),
            containing_branches: vec![],
            working: None,
        }
    }

    /// A repo with `n` commits and a single added file row, graph laid out.
    fn repo_with(n: usize) -> RepoModel {
        let mut repo = RepoModel::empty();
        repo.commits = (0..n).map(|i| commit(&format!("h{i:06}"), &format!("s{i}"))).collect();
        repo.graph = crate::graph_engine::build_layout(&repo.commits);
        repo.tree = vec![TreeNode::File {
            name: "f.go".to_string(),
            status: FileStatus::Added,
        }];
        repo
    }

    fn state_with(n: usize) -> AppState {
        AppState::from_repo(repo_with(n))
    }

    /// A git action that triggers a reload must keep its result Notice ACROSS the reload:
    /// RepoLoaded resets to Ready, but the parked parked_notice re-applies the message so a
    /// "Committed/Deleted/..." outcome is not wiped before the user sees it.
    #[test]
    fn a_reload_git_action_keeps_its_notice_after_reloading() {
        let mut state = state_with(3);
        state.apply(Msg::GitActionDone { summary: "Deleted x.txt".to_string(), reload: true });
        assert!(matches!(&state.status, Status::Notice(n) if n == "Deleted x.txt"));
        assert!(state.view.effects.contains(&Effect::ReloadRepo), "a reload was parked");
        // The reload lands: status would reset to Ready, but the notice survives, marked
        // STICKY so the reload's auto-open of the newly selected file does not wipe it.
        state.apply(Msg::RepoLoaded(Box::new(repo_with(3))));
        assert!(
            matches!(&state.status, Status::Notice(n) if n == "Deleted x.txt"),
            "the git-action notice survives the reload it triggered, got {:?}",
            state.status
        );
        assert!(state.view.notice_sticky, "the notice is sticky across the auto-open");
        // A SUBSEQUENT plain reload (--watch tick, no parked notice) clears to Ready.
        state.apply(Msg::RepoLoaded(Box::new(repo_with(3))));
        assert!(matches!(state.status, Status::Ready), "an unrelated reload returns to Ready");
    }

    /// The periodic status poll: only a CHANGED signature queues the gentle refresh
    /// (`RefreshRepo` - selection cache kept so an open buffer survives); an unchanged
    /// signature, a failed poll (`None`), and a poll racing the first load are no-ops.
    #[test]
    fn a_changed_status_poll_queues_a_gentle_refresh() {
        let mut state = state_with(2);
        state.repo.status_sig = Some(7);
        assert!(!state.apply(Msg::StatusPolled { sig: Some(7) }), "unchanged sig = idle tick");
        assert!(!state.apply(Msg::StatusPolled { sig: None }), "a failed poll is skipped");
        assert!(state.view.effects.is_empty(), "no refresh queued yet");
        assert!(state.apply(Msg::StatusPolled { sig: Some(9) }), "a moved sig refreshes");
        assert!(state.view.effects.contains(&Effect::RefreshRepo));
        assert!(!state.view.effects.contains(&Effect::ReloadRepo), "gentle, not cache-resetting");
    }

    #[test]
    fn a_poll_before_the_first_load_is_dropped() {
        let mut state = state_with(2);
        assert_eq!(state.repo.status_sig, None, "no snapshot signature yet");
        assert!(!state.apply(Msg::StatusPolled { sig: Some(9) }));
        assert!(state.view.effects.is_empty());
    }

    /// A reload landing with a HISTORICAL commit selected must NOT clobber the files
    /// panel with the snapshot's `<current>` working tree: the displayed tree is kept
    /// and the selected commit's own tree re-fetched (`ReloadTree`).
    #[test]
    fn a_refresh_keeps_a_historical_selections_tree_and_refetches_it() {
        let mut state = state_with(3);
        state.repo.status_sig = Some(1); // not the first load
        state.repo.tree = vec![TreeNode::File { name: "old.rs".to_string(), status: FileStatus::Modified }];
        let mut fresh = repo_with(3);
        fresh.tree = vec![TreeNode::File { name: "working.txt".to_string(), status: FileStatus::Added }];
        state.apply(Msg::RepoLoaded(Box::new(fresh)));
        assert!(
            matches!(&state.repo.tree[0], TreeNode::File { name, .. } if name == "old.rs"),
            "the historical selection's tree survives the swap"
        );
        assert!(state.view.effects.contains(&Effect::ReloadTree), "its own tree re-fetches");
        assert!(
            state.view.effects.contains(&Effect::ReloadDetail),
            "the rich detail re-fetches (rebuild_detail only restores the cheap fields)"
        );
    }

    /// A reload that KEEPS the selection (status poll / --watch) clears the loaded
    /// preview; with a file still selected and no editor it must queue a re-fetch, or
    /// the viewer is stranded on "Loading diff..." (nothing else re-requests it).
    #[test]
    fn a_refresh_under_a_loaded_readonly_preview_requeues_it() {
        let mut state = state_with(3);
        state.repo.preview = Some(FileView::Diff(FileDiff {
            path: "f.go".to_string(),
            old_rev: "a".to_string(),
            new_rev: "b".to_string(),
            lines: vec![],
        }));
        assert!(state.selected_file_path().is_some(), "a file row is selected");
        state.apply(Msg::RepoLoaded(Box::new(repo_with(3))));
        assert!(state.repo.preview.is_none(), "the stale preview cleared");
        assert!(state.view.effects.contains(&Effect::ReloadPreview), "and re-fetches");
    }

    /// `edit_file_loaded` clears a stale Notice when a file opens - UNLESS the notice is a
    /// sticky git-action result (the reload's auto-open), which must survive until the user
    /// navigates. Genuine navigation (`select_file`) un-sticks it.
    #[test]
    fn sticky_notice_survives_the_reload_auto_open_then_clears_on_nav() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true; // only the working row opens editable
        // Give the selected commit a one-file tree so edit_file_loaded's staleness guard
        // passes (it applies only while that (commit, path) is selected).
        state.repo.tree = vec![TreeNode::File { name: "f.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.preselect_file(0); // root at index 0; f.txt is the next row
        let (commit, path) = (state.selected_commit_hash().unwrap(), "f.txt".to_string());

        // Sticky notice (as RepoLoaded would set after a git action): the auto-open keeps it.
        state.status = Status::Notice("Committed f.txt".to_string());
        state.view.notice_sticky = true;
        state.apply(Msg::EditFileLoaded { commit: commit.clone(), path: path.clone(), base: None, work: "x".to_string() });
        assert!(
            matches!(&state.status, Status::Notice(n) if n == "Committed f.txt"),
            "sticky notice survives the auto-open, got {:?}", state.status
        );

        // A NON-sticky notice (an ordinary hint) is cleared when a file opens.
        state.status = Status::Notice("some hint".to_string());
        state.view.notice_sticky = false;
        state.apply(Msg::EditFileLoaded { commit, path, base: None, work: "y".to_string() });
        assert!(matches!(state.status, Status::Ready), "a non-sticky notice clears on open");
    }

    /// Clicking into the editable diff blurs an active search field (log OR files) and focuses
    /// the Diff pane, so the next keystroke edits the buffer - not the still-focused filter.
    #[test]
    fn clicking_the_diff_editor_blurs_an_active_search_field() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.repo.tree = vec![TreeNode::File { name: "f.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.preselect_file(0); // root at index 0; f.txt is the next row
        let (commit, path) = (state.selected_commit_hash().unwrap(), "f.txt".to_string());
        state.apply(Msg::EditFileLoaded { commit, path, base: None, work: "hello\nworld\n".to_string() });
        assert!(state.view.editor.is_some(), "an editable buffer is loaded");

        // The user filtered the files list, then clicks into the diff editor to type.
        state.view.files_search_active = true;
        state.view.focus = Pane::Files;
        assert!(state.apply(Msg::Edit(crate::message::EditOp::Place { row: 0, col: 3, select: false })));
        assert!(!state.view.files_search_active, "the diff click blurs the files search");
        assert_eq!(state.view.focus, Pane::Diff, "focus moves to the diff editor");
    }

    /// The sticky flag must not leak true past the one auto-open it guards: the reload
    /// auto-open consumes it, and a follow-up NON-reload notice clears any leftover - so a
    /// later transient hint (a Copy) cannot inherit the stale sticky.
    #[test]
    fn notice_sticky_does_not_leak_into_a_later_transient_notice() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true; // only the working row opens editable
        state.repo.tree = vec![TreeNode::File { name: "f.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.preselect_file(0); // root at index 0; f.txt is the next row
        let (commit, path) = (state.selected_commit_hash().unwrap(), "f.txt".to_string());

        // The auto-open of the reload's file CONSUMES the flag (after respecting it once).
        state.status = Status::Notice("Committed f.txt".to_string());
        state.view.notice_sticky = true;
        state.apply(Msg::EditFileLoaded { commit, path, base: None, work: "x".to_string() });
        assert!(!state.view.notice_sticky, "the auto-open consumes the sticky flag");

        // A non-reload git action that sets a fresh transient notice also clears a leftover.
        state.view.notice_sticky = true;
        state.apply(Msg::GitActionDone { summary: "Copied".to_string(), reload: false });
        assert!(!state.view.notice_sticky, "a non-reload notice is never sticky");
    }

    fn diff_preview(lines: usize) -> FileView {
        FileView::Diff(FileDiff {
            path: "f.go".to_string(),
            old_rev: "a".to_string(),
            new_rev: "b".to_string(),
            lines: (0..lines)
                .map(|i| DiffLine {
                    old_no: Some(i + 1),
                    new_no: Some(i + 1),
                    kind: LineKind::Context,
                    tokens: vec![],
                    inline_hl: None,
                    hunk: 0,
                    fold: None,
                })
                .collect(),
        })
    }

    #[test]
    fn focus_open_file_unfolds_changed_dirs_and_reveals_the_opened_file() {
        let mut state = state_with(3);
        // Full-tree view with a collapsed `src/` holding the changed, opened file and a
        // clean `vendor/` that must stay folded.
        state.view.show_all_files = true;
        state.repo.tree = vec![
            TreeNode::Dir {
                name: "src".to_string(),
                file_count: 1,
                expanded: false,
                children: vec![TreeNode::File { name: "a.go".to_string(), status: FileStatus::Modified }],
            },
            TreeNode::Dir {
                name: "vendor".to_string(),
                file_count: 1,
                expanded: false,
                children: vec![TreeNode::File { name: "lib.go".to_string(), status: FileStatus::Unchanged }],
            },
        ];
        // The opened file: its dir is collapsed, so only the preview path identifies it.
        state.repo.preview = Some(FileView::Diff(FileDiff {
            path: "src/a.go".to_string(),
            old_rev: "a".to_string(),
            new_rev: "b".to_string(),
            lines: vec![],
        }));
        state.view.files_scroll = Some(5);

        assert!(state.apply(Msg::FocusOpenFile));
        let expanded = |n: &TreeNode| matches!(n, TreeNode::Dir { expanded, .. } if *expanded);
        assert!(expanded(&state.repo.tree[0]), "src/ unfolds (holds a change)");
        assert!(!expanded(&state.repo.tree[1]), "vendor/ stays folded (all unchanged)");
        assert_eq!(state.selected_file_path().as_deref(), Some("src/a.go"), "selection reveals the opened file");
        assert_eq!(state.view.files_scroll, None, "the wheel free-scroll is cleared so it snaps into view");
    }

    /// A files-search query forces the All (full-tree) view, snapshotting the prior mode,
    /// filters the files list by path, and restores the mode when the query clears.
    #[test]
    fn files_search_forces_all_view_filters_and_restores() {
        let mut state = state_with(3);
        state.view.show_all_files = false;
        state.repo.tree = vec![
            TreeNode::Dir {
                name: "src".to_string(),
                file_count: 2,
                expanded: true,
                children: vec![
                    TreeNode::File { name: "main.rs".to_string(), status: FileStatus::Modified },
                    TreeNode::File { name: "store.rs".to_string(), status: FileStatus::Modified },
                ],
            },
        ];
        // Type "store": forces All on (remembering false), narrows to src/ + store.rs.
        assert!(state.apply(Msg::FilesSearchFocus));
        for ch in "store".chars() {
            state.apply(Msg::FilesSearchPush(ch));
        }
        assert!(state.view.show_all_files, "a query forces the All view");
        assert_eq!(state.view.files_prev_all, Some(false), "the prior mode is snapshotted");
        assert_eq!(state.files_rows_len(), 1, "filtered (flat) to just src/store.rs");
        // Clear: restores the prior All mode and drops the snapshot.
        assert!(state.apply(Msg::FilesSearchClear));
        assert!(!state.view.show_all_files, "clearing restores the prior (changed-only) mode");
        assert_eq!(state.view.files_prev_all, None, "the snapshot is consumed");
        assert_eq!(state.files_rows_len(), 3, "the full src/ tree is visible again");
    }

    /// With an active query the visible list is FLAT (matched files only) even in nested
    /// mode, so a mark gesture at a visible row must mark THAT single file - not the whole
    /// directory subtree the same row index points at in the unfiltered nested flatten.
    #[test]
    fn files_search_marks_the_matched_file_not_a_dir_subtree() {
        let mut state = state_with(3);
        state.view.files_flat = false; // nested mode (where row 0 is the `src` dir)
        state.repo.tree = vec![TreeNode::Dir {
            name: "src".to_string(),
            file_count: 2,
            expanded: true,
            children: vec![
                TreeNode::File { name: "main.rs".to_string(), status: FileStatus::Modified },
                TreeNode::File { name: "store.rs".to_string(), status: FileStatus::Modified },
            ],
        }];
        // Filter to the single match; visible row 1 (after root) is now src/store.rs (flat), NOT src/.
        state.apply(Msg::FilesSearchFocus);
        for ch in "store".chars() {
            state.apply(Msg::FilesSearchPush(ch));
        }
        // Mark the matched row (root at 0, src/store.rs at 1): it must mark ONLY
        // src/store.rs, not the whole src/ subtree.
        state.apply(Msg::ToggleMark(0));
        assert!(state.view.is_marked("src/store.rs"), "the matched file is marked");
        assert!(!state.view.is_marked("src/main.rs"), "a sibling under the same dir is NOT marked");
    }

    #[test]
    fn menu_pick_toggles_action_and_closes() {
        let mut state = state_with(3);
        state.apply(Msg::OpenMenu(MenuId::View));
        assert_eq!(state.view.open_menu, Some(MenuId::View));
        let before = state.view.show_diff;
        state.apply(Msg::MenuPick(MenuAction::ShowDiff));
        assert_eq!(state.view.show_diff, !before, "ShowDiff item flips show_diff");
        assert_eq!(state.view.open_menu, None, "picking an item closes the menu");
    }

    /// The global Git menu's items route to the existing repo-level dialogs/confirms:
    /// Commit/Amend/Tag open an input, the remote + stash ops open a confirm carrying the
    /// matching GitAction (incl. the new Fetch / Unstash).
    #[test]
    fn git_menu_items_route_to_their_dialogs() {
        let mut state = state_with(3);
        let pick = |s: &mut AppState, a: MenuAction| {
            s.apply(Msg::OpenMenu(MenuId::Git));
            assert_eq!(s.view.open_menu, Some(MenuId::Git));
            s.apply(Msg::MenuPick(a));
            assert_eq!(s.view.open_menu, None, "picking closes the menu");
        };
        pick(&mut state, MenuAction::GitCommit);
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { kind: InputKind::Commit, .. })));
        state.apply(Msg::DialogCancel);

        pick(&mut state, MenuAction::GitAmend);
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { kind: InputKind::Amend, .. })));
        state.apply(Msg::DialogCancel);

        pick(&mut state, MenuAction::GitTag);
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { kind: InputKind::Tag, .. })));
        state.apply(Msg::DialogCancel);

        pick(&mut state, MenuAction::GitFetch);
        assert!(matches!(&state.view.dialog, Some(Dialog::Confirm { action: GitAction::Fetch, .. })));
        state.apply(Msg::DialogCancel);

        pick(&mut state, MenuAction::GitPull);
        assert!(matches!(
            &state.view.dialog,
            Some(Dialog::Choice { kind: crate::view_state::ChoiceKind::PullStrategy, .. })
        ));
        // Confirming the rebase row (index 2) parks a rebase-pull.
        state.apply(Msg::DialogMove(2));
        state.apply(Msg::DialogConfirm);
        assert!(matches!(state.view.queued_git(), Some(GitAction::PullStrategy { rebase: Some(true) })));
        state.view.effects.clear();

        pick(&mut state, MenuAction::GitPush);
        assert!(matches!(&state.view.dialog, Some(Dialog::Confirm { action: GitAction::Push, .. })));
        state.apply(Msg::DialogCancel);

        pick(&mut state, MenuAction::GitStash);
        assert!(matches!(&state.view.dialog, Some(Dialog::Confirm { action: GitAction::Stash, .. })));
        state.apply(Msg::DialogCancel);

        pick(&mut state, MenuAction::GitUnstash);
        assert!(matches!(&state.view.dialog, Some(Dialog::Confirm { action: GitAction::Unstash, .. })));
        state.apply(Msg::DialogCancel);

        // DESTRUCTIVE route: a mis-wire to the wrong GitAction must fail the build.
        pick(&mut state, MenuAction::GitDiscard);
        assert!(matches!(&state.view.dialog, Some(Dialog::Confirm { action: GitAction::DiscardAll, .. })));
        state.apply(Msg::DialogCancel);

        // Manage Remotes parks an async remote-list read (no dialog yet).
        pick(&mut state, MenuAction::GitRemotes);
        assert!(state.view.effects.contains(&Effect::LoadRemotes), "Manage Remotes parks a remote-list read");
        assert!(state.view.dialog.is_none(), "the dialog opens only on the load reply");
        state.view.effects.clear();

        // New Branch from HEAD opens the name input targeting the literal HEAD.
        pick(&mut state, MenuAction::GitNewBranch);
        assert!(matches!(
            &state.view.dialog,
            Some(Dialog::Input { kind: InputKind::NewBranch, commit: Some(c), .. }) if c == "HEAD"
        ));
        state.apply(Msg::DialogCancel);

        // Create / Apply Patch open a path input each.
        pick(&mut state, MenuAction::GitCreatePatch);
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { kind: InputKind::CreatePatchAll, .. })));
        state.apply(Msg::DialogCancel);
        pick(&mut state, MenuAction::GitApplyPatch);
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { kind: InputKind::ApplyPatch, .. })));
        state.apply(Msg::DialogCancel);

        // Branches / Merge / Rebase each park an async ref-list read for their op.
        for (action, op) in [
            (MenuAction::GitBranches, crate::view_state::RefOp::Checkout),
            (MenuAction::GitMerge, crate::view_state::RefOp::Merge),
            (MenuAction::GitRebase, crate::view_state::RefOp::Rebase),
        ] {
            pick(&mut state, action);
            assert!(state.view.effects.contains(&Effect::RefPick(op)), "parks the ref-list read for the op");
            assert!(state.view.dialog.is_none(), "the picker opens only on the reply");
            state.view.effects.clear();
        }
    }

    /// The ref picker flow: a loaded ref list opens the picker, and confirming a row parks the
    /// op's GitAction behind a confirm (checkout/merge/rebase), with merge/rebase naming the
    /// current branch.
    #[test]
    fn ref_picker_confirm_parks_the_op_git_action() {
        use crate::view_state::{PickItem, RefOp};
        let items = || vec![PickItem { rev: "feature".to_string(), label: "feature (branch)".to_string() }];

        // Merge: confirming the row parks a MergeRef behind a confirm.
        let mut state = state_with(3);
        state.view.effects.push(Effect::RefPick(RefOp::Merge));
        state.apply(Msg::RefListLoaded { op: RefOp::Merge, items: items(), epoch: state.view.nav_epoch });
        assert!(matches!(&state.view.dialog, Some(Dialog::RefPick { op: RefOp::Merge, sel: 0, .. })));
        state.apply(Msg::DialogConfirm);
        assert!(matches!(
            &state.view.dialog,
            Some(Dialog::Confirm { action: GitAction::MergeRef { name }, .. }) if name == "feature"
        ));

        // Checkout of a remote-tracking ref warns the HEAD detaches.
        let mut state = state_with(3);
        state.apply(Msg::RefListLoaded {
            op: RefOp::Checkout,
            items: vec![PickItem { rev: "origin/main".to_string(), label: "origin/main  (remote)".to_string() }],
            epoch: state.view.nav_epoch,
        });
        state.apply(Msg::DialogConfirm);
        assert!(matches!(
            &state.view.dialog,
            Some(Dialog::Confirm { action: GitAction::CheckoutRef { name }, prompt })
                if name == "origin/main" && prompt.contains("detach")
        ));

        // A LOCAL branch with a slash in its name (feature/x) attaches - it must NOT warn detach.
        let mut state = state_with(3);
        state.apply(Msg::RefListLoaded {
            op: RefOp::Checkout,
            items: vec![PickItem { rev: "feature/topic-1".to_string(), label: "feature/topic-1  (branch)".to_string() }],
            epoch: state.view.nav_epoch,
        });
        state.apply(Msg::DialogConfirm);
        assert!(matches!(
            &state.view.dialog,
            Some(Dialog::Confirm { action: GitAction::CheckoutRef { name }, prompt })
                if name == "feature/topic-1" && !prompt.contains("detach")
        ));
    }

    /// The Manage Remotes dialog flow: the load opens the list, `a` parses a `name url` add,
    /// Enter edits the selected URL, and `d` confirms a remove - each parking the right GitAction.
    #[test]
    fn empty_remotes_dialog_arrows_do_not_panic() {
        // A repo with NO remotes opens an empty Manage Remotes list; Up/Down route to
        // dialog_move, which used to clamp(0, -1) and panic. The guard returns false.
        let mut state = state_with(3);
        state.view.effects.push(Effect::LoadRemotes);
        state.apply(Msg::RemotesLoaded { remotes: vec![], epoch: state.view.nav_epoch });
        assert!(matches!(&state.view.dialog, Some(Dialog::Remotes { sel: 0, .. })));
        assert!(!state.apply(Msg::DialogMove(1)), "no movement in an empty list");
        assert!(!state.apply(Msg::DialogMove(-1)));
        assert!(matches!(&state.view.dialog, Some(Dialog::Remotes { sel: 0, .. })), "still open, no crash");
    }

    #[test]
    fn manage_remotes_dialog_drives_add_edit_remove() {
        let mut state = state_with(3);
        state.view.effects.push(Effect::LoadRemotes); // simulate the menu pick having queued the read
        let remotes = vec![("origin".to_string(), "https://example.com/a.git".to_string())];
        state.apply(Msg::RemotesLoaded { remotes, epoch: state.view.nav_epoch });
        assert!(matches!(&state.view.dialog, Some(Dialog::Remotes { sel: 0, .. })));

        // `a` -> the add input; a "name url" line parses into a RemoteAdd.
        state.apply(Msg::RemoteAddInput);
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { kind: InputKind::RemoteAdd, .. })));
        for ch in "upstream https://example.com/u.git".chars() {
            state.apply(Msg::DialogInput(ch));
        }
        state.apply(Msg::DialogConfirm);
        assert!(matches!(
            state.view.queued_git(),
            Some(GitAction::RemoteAdd { name, url })
                if name == "upstream" && url == "https://example.com/u.git"
        ));
        state.view.effects.clear();

        // Reopen the list; Enter edits the selected remote's URL (name snapshotted).
        state.apply(Msg::RemotesLoaded {
            remotes: vec![("origin".to_string(), "https://example.com/a.git".to_string())],
            epoch: state.view.nav_epoch,
        });
        state.apply(Msg::DialogConfirm);
        assert!(matches!(
            &state.view.dialog,
            Some(Dialog::Input { kind: InputKind::RemoteSetUrl, commit: Some(n), .. }) if n == "origin"
        ));
        state.apply(Msg::DialogCancel);

        // Reopen; `d` confirms a remove of the selected remote.
        state.apply(Msg::RemotesLoaded {
            remotes: vec![("origin".to_string(), "https://example.com/a.git".to_string())],
            epoch: state.view.nav_epoch,
        });
        state.apply(Msg::RemoteRemove);
        assert!(matches!(
            &state.view.dialog,
            Some(Dialog::Confirm { action: GitAction::RemoteRemove { name }, .. }) if name == "origin"
        ));
    }

    #[test]
    fn opening_a_popup_closes_the_other() {
        let mut state = state_with(3);
        // A menu open then a filter dropdown open: the menu must close.
        state.apply(Msg::OpenMenu(MenuId::Editor));
        state.apply(Msg::OpenDropdown(FilterKind::Branch));
        assert_eq!(state.view.open_menu, None, "opening a dropdown closes the menu");
        assert_eq!(state.view.open_dropdown, Some(FilterKind::Branch));
        // ...and vice versa.
        state.apply(Msg::OpenMenu(MenuId::Editor));
        assert_eq!(state.view.open_dropdown, None, "opening a menu closes the dropdown");
        assert_eq!(state.view.open_menu, Some(MenuId::Editor));
        assert!(state.apply(Msg::CloseMenu), "CloseMenu reports the change");
        assert_eq!(state.view.open_menu, None);
    }

    #[test]
    fn right_click_selects_the_row_and_opens_the_commit_menu() {
        let mut state = state_with(3);
        assert!(state.apply(Msg::OpenCommitMenu { index: 1, col: 5, row: 4 }));
        assert_eq!(state.view.log_sel, 1, "right-click selects the targeted row");
        assert_eq!(state.view.focus, Pane::Log);
        let m = state.view.commit_menu.expect("the menu is open");
        assert_eq!((m.index, m.col, m.row), (1, 5, 4), "menu anchors at the click");
    }

    #[test]
    fn commit_menu_copy_revision_parks_the_full_hash() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 2, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::CopyRevision)));
        assert_eq!(state.view.queued_clipboard(), Some("h000002"));
        assert_eq!(state.view.commit_menu, None, "picking an item closes the menu");
    }

    #[test]
    fn commit_menu_show_diff_reveals_a_hidden_viewer() {
        let mut state = state_with(3);
        state.view.show_diff = false;
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::ShowDiff)));
        assert!(state.view.show_diff, "Show Diff reveals the hidden viewer");
    }

    #[test]
    fn commit_menu_show_diff_repaints_even_when_already_shown() {
        // The menu was just closed, so picking Show Diff with the viewer ALREADY
        // visible must still report a change - else the popup ghosts on screen
        // until the next event (the P2 review finding).
        let mut state = state_with(3);
        state.view.show_diff = true;
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::ShowDiff)));
        assert_eq!(state.view.commit_menu, None, "the menu is closed");
    }

    #[test]
    fn commit_menu_reword_prefills_message_and_parks_reword_at() {
        let mut state = state_with(3);
        state.repo.commits[0].head = true; // index 0 is HEAD in this fixture
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::EditMessage)));
        match &state.view.dialog {
            Some(Dialog::Input { kind: InputKind::Reword, field, commit: Some(h), note, .. }) => {
                assert_eq!(field.text(), "s0", "the dialog prefills the commit message");
                assert_eq!(h, "h000000", "the target commit hash is snapshotted");
                assert!(note.is_none(), "HEAD (unpushed) reword shows no warning");
            }
            other => panic!("expected a Reword input dialog, got {other:?}"),
        }
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::RewordAt { commit: "h000000".to_string(), message: "s0".to_string() })
        );
    }

    #[test]
    fn commit_menu_create_patch_prefills_path_then_parks_create_patch() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::CreatePatch)));
        match &state.view.dialog {
            Some(Dialog::Input { kind: InputKind::CreatePatch, field, commit: Some(h), .. }) => {
                assert_eq!(field.text(), "/tmp/h000001.patch", "defaults to /tmp/<short>.patch");
                assert_eq!(h, "h000001", "the target commit hash is snapshotted");
            }
            other => panic!("expected a CreatePatch input dialog, got {other:?}"),
        }
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::CreatePatch {
                commit: "h000001".to_string(),
                path: "/tmp/h000001.patch".to_string(),
            })
        );
    }

    #[test]
    fn commit_menu_create_patch_skips_the_working_row() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true; // mark row 0 as the synthetic <current>
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 }); // selects the working row
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::CreatePatch)));
        assert!(state.view.dialog.is_none(), "no patch dialog on the working row");
        assert!(matches!(state.status, Status::Notice(_)), "it hints instead");
    }

    #[test]
    fn working_row_menu_shows_working_verbs_and_no_ref_flyouts() {
        use crate::view_state::commit_menu_items;
        // The working-row menu is a distinct, short slice of working-tree verbs (with the
        // diff hidden, so "Show Diff" leads).
        let working = commit_menu_items(true, true);
        let actions: Vec<CommitMenuAction> = working
            .iter()
            .filter_map(|r| match r {
                crate::view_state::CommitRow::Action(a, _) => Some(*a),
                crate::view_state::CommitRow::Sep => None,
            })
            .collect();
        assert_eq!(
            actions,
            vec![
                CommitMenuAction::ShowDiff,
                CommitMenuAction::CommitChanges,
                CommitMenuAction::StashChanges,
                CommitMenuAction::CreateArchive,
                CommitMenuAction::DiscardChanges,
            ],
            "working menu = inspect + the working-tree verbs (incl. zip), no commit-targeted actions"
        );

        // Opening it on the synthetic row sets `working` and drops the ref fly-outs (the
        // current-branch chip the row carries is not a commit-targeted ref to act on here).
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.repo.commits[0].refs =
            vec![model::Ref { name: "main".to_string(), kind: model::RefKind::LocalBranch }];
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        let menu = state.view.commit_menu.as_ref().expect("menu open on the working row");
        assert!(menu.working, "the working flag routes the working-verb slice");
        assert!(menu.refs.is_empty(), "no ref fly-outs on the working row");
    }

    /// "Show Diff" is hidden from the context menu when the diff viewer is already open
    /// (redundant), and appears only when it is hidden - on both the real-commit and the
    /// `<current>` working menus.
    #[test]
    fn show_diff_row_hidden_when_diff_already_shown() {
        use crate::view_state::{commit_menu_items, CommitMenuAction::ShowDiff, CommitRow};
        let has_show_diff = |rows: &[CommitRow]| {
            rows.iter().any(|r| matches!(r, CommitRow::Action(ShowDiff, _)))
        };
        for working in [false, true] {
            assert!(has_show_diff(&commit_menu_items(working, true)), "shown when diff hidden");
            assert!(!has_show_diff(&commit_menu_items(working, false)), "hidden when diff shown");
        }
        // Opening the menu snapshots `show_diff_item = !show_diff`.
        let mut state = state_with(3);
        state.view.show_diff = true;
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(!state.view.commit_menu.as_ref().unwrap().show_diff_item, "diff shown => no Show Diff row");
        state.apply(Msg::CloseCommitMenu);
        state.view.show_diff = false;
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.view.commit_menu.as_ref().unwrap().show_diff_item, "diff hidden => Show Diff row present");
    }

    /// Right-clicking a file row on the `<current>` working row opens the files context
    /// menu (Show Diff + the local-changes items) targeting that file; Show Diff reveals the
    /// viewer, Copy as Patch parks the clipboard round-trip, and Rollback opens the confirm.
    #[test]
    fn files_menu_show_diff_and_rollback_on_the_working_row() {
        use crate::view_state::FilesMenuAction;
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0)); // select the working row
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.view.show_diff = false;

        assert!(state.apply(Msg::OpenFilesMenu { index: 0, col: 5, row: 7 }));
        let menu = state.view.files_menu.as_ref().expect("files menu open on the file row");
        assert_eq!(menu.path, "a.txt", "the menu targets the right-clicked file");
        assert!(menu.local_changes && menu.show_diff_item && menu.has_head_version);
        assert_eq!(
            menu.items(),
            vec![
                FilesMenuAction::ShowDiff,
                FilesMenuAction::CopyPatch,
                FilesMenuAction::CreatePatch,
                FilesMenuAction::CommitFile,
                FilesMenuAction::ShowCurrentRevision,
                FilesMenuAction::CompareWithRevision,
                FilesMenuAction::CompareWithBranch,
                FilesMenuAction::ShowHistory,
                FilesMenuAction::Annotate,
                FilesMenuAction::Rollback,
                FilesMenuAction::DeleteFile,
            ],
            "changed (in-HEAD) working file + diff-hidden = all rows"
        );
        assert!(state.view.commit_menu.is_none(), "opening the files menu closes the commit menu");

        // Show Diff reveals the hidden viewer and closes the menu.
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::ShowDiff)));
        assert!(state.view.show_diff, "Show Diff reveals the viewer");
        assert!(state.view.files_menu.is_none(), "picking closes the menu");

        // Copy as Patch parks the clipboard round-trip for the snapshotted file.
        state.apply(Msg::OpenFilesMenu { index: 0, col: 5, row: 7 });
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::CopyPatch)));
        assert!(state.view.effects.contains(&Effect::CopyPatch("a.txt".to_string())), "parks the file for Req::CopyPatch");
        assert!(state.view.files_menu.is_none(), "picking closes the menu");

        // Create Patch opens the prefilled destination dialog threading the file path.
        state.apply(Msg::OpenFilesMenu { index: 0, col: 5, row: 7 });
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::CreatePatch)));
        match &state.view.dialog {
            Some(Dialog::Input { kind: InputKind::CreateWorkingPatch, field, commit, .. }) => {
                assert_eq!(field.text(), "/tmp/a.txt.patch", "prefills /tmp/<basename>.patch");
                assert_eq!(commit.as_deref(), Some("a.txt"), "snapshots the file in the commit slot");
            }
            other => panic!("expected a CreateWorkingPatch input dialog, got {other:?}"),
        }
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::CreateWorkingPatch { file: "a.txt".to_string(), path: "/tmp/a.txt.patch".to_string() }),
            "confirm parks the working-patch write"
        );
        state.view.effects.clear(); // the runtime would have drained the queue

        // Commit File opens an empty message dialog threading the file, then parks CommitFile.
        state.apply(Msg::OpenFilesMenu { index: 0, col: 5, row: 7 });
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::CommitFile)));
        match &state.view.dialog {
            Some(Dialog::Input { kind: InputKind::CommitFile, field, commit, .. }) => {
                assert_eq!(field.text(), "", "the message starts empty");
                assert_eq!(commit.as_deref(), Some("a.txt"), "snapshots the file in the commit slot");
            }
            other => panic!("expected a CommitFile input dialog, got {other:?}"),
        }
        state.input_field().unwrap().insert_str("fix a");
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::CommitFile { file: "a.txt".to_string(), message: "fix a".to_string() }),
            "confirm parks the per-file commit"
        );

        // Delete opens a destructive confirm carrying DeleteFile for the snapshotted path.
        state.apply(Msg::OpenFilesMenu { index: 0, col: 5, row: 7 });
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::DeleteFile)));
        assert!(matches!(&state.view.dialog, Some(Dialog::Confirm { action: GitAction::DeleteFile { file }, .. }) if file == "a.txt"));
        state.apply(Msg::DialogCancel);

        // Rollback opens the revert confirm modal for the file.
        state.apply(Msg::OpenFilesMenu { index: 0, col: 5, row: 7 });
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::Rollback)));
        let req = state.view.revert_confirm.as_ref().expect("Rollback opens the revert confirm");
        assert_eq!(req.paths, vec!["a.txt".to_string()], "Rollback targets the right-clicked file");
    }

    /// Rollback reverts the menu's SNAPSHOTTED file even if the live selection drifts after
    /// the menu opens (a list reflow), never the drifted file - it targets `FilesMenu.path`.
    #[test]
    fn files_menu_rollback_targets_snapshot_not_drifted_selection() {
        use crate::view_state::FilesMenuAction;
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![
            TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "b.txt".to_string(), status: FileStatus::Modified },
        ];
        state.view.files_flat = true;
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // right-click a.txt (root at 0)
        assert_eq!(state.view.files_menu.as_ref().unwrap().path, "a.txt");
        // Drift the live selection to b.txt WITHOUT going through a dismissing path.
        state.view.files_sel = 1;
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::Rollback)));
        assert_eq!(
            state.view.revert_confirm.as_ref().unwrap().paths,
            vec!["a.txt".to_string()],
            "reverts the snapshotted right-clicked file, not the drifted selection"
        );
    }

    /// Show Current Revision opens the file's EDITABLE working version: it navigates to the
    /// `<current>` working row and parks the file path to re-select once that tree loads (no
    /// read-only overlay).
    #[test]
    fn show_current_revision_navigates_to_current_and_opens_the_editable_file() {
        use crate::view_state::FilesMenuAction;
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true; // the <current> working row
        state.repo.commits[2].is_working = false;
        // Browse a HISTORICAL commit (row 2), then Show Current Revision jumps back to <current>.
        state.apply(Msg::SelectCommit(2));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;

        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::ShowCurrentRevision)));
        assert!(
            state.selected_commit().is_some_and(|c| c.is_working),
            "it navigates the log to the <current> working row"
        );
        assert_eq!(
            state.view.parked_file_path.as_deref(),
            Some("a.txt"),
            "the file is parked to re-select (and open editable) once the working tree loads"
        );
        assert!(state.view.queued_inspect().is_none(), "no read-only overlay is parked");

        // Now the parked file loads on the <current> tree: it IS a working change -> revealed
        // (editable), and the read-only fallback is dropped.
        state.repo.commits[0].hash = WORKING_REV.to_string();
        state.apply(Msg::SelectCommit(0)); // back to <current>
        state.repo.commits[0].is_working = true;
        state.view.parked_file_path = Some("a.txt".to_string());
        state.view.parked_revision = Some("a.txt".to_string());
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.apply(Msg::TreeLoaded { hash: WORKING_REV.to_string(), tree: state.repo.tree.clone(), ignored: Default::default() });
        assert_eq!(state.selected_file_path().as_deref(), Some("a.txt"), "the changed file is revealed");
        assert!(state.view.queued_inspect().is_none(), "a found file needs no read-only fallback");
    }

    /// Show Current Revision on the `<current>` row itself reveals the changed file IN PLACE
    /// (no navigate-away, no dangling parked state) - the working tree is already loaded.
    #[test]
    fn show_current_revision_on_current_reveals_in_place() {
        use crate::view_state::FilesMenuAction;
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.repo.commits[0].hash = WORKING_REV.to_string();
        state.apply(Msg::SelectCommit(0)); // on <current>
        state.repo.tree = vec![
            TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "b.txt".to_string(), status: FileStatus::Modified },
        ];
        state.view.files_flat = true;
        state.apply(Msg::OpenFilesMenu { index: 1, col: 0, row: 0 }); // right-click b.txt
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::ShowCurrentRevision)));
        assert_eq!(state.view.files_sel, 1, "b.txt is revealed in place");
        assert_eq!(state.selected_file_path().as_deref(), Some("b.txt"));
        assert!(state.view.parked_file_path.is_none(), "no dangling parked reveal on the same row");
        assert!(state.view.parked_revision.is_none(), "no dangling parked fallback");
        assert!(state.view.queued_inspect().is_none(), "a changed file needs no read-only overlay");
    }

    /// Show Current Revision on an UNCHANGED file (no working-tree row): the navigate finds no
    /// row, so it falls back to a read-only HEAD overlay instead of landing on nothing.
    #[test]
    fn show_current_revision_falls_back_to_read_only_for_an_unchanged_file() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.repo.commits[0].hash = WORKING_REV.to_string();
        state.apply(Msg::SelectCommit(0));
        // The <current> changed tree does NOT contain "gone.txt".
        state.repo.tree = vec![TreeNode::File { name: "other.txt".to_string(), status: FileStatus::Modified }];
        state.view.parked_file_path = Some("gone.txt".to_string());
        state.view.parked_revision = Some("gone.txt".to_string());
        state.apply(Msg::TreeLoaded { hash: WORKING_REV.to_string(), tree: state.repo.tree.clone(), ignored: Default::default() });
        assert_eq!(
            state.view.queued_inspect().map(|r| (r.rev.as_str(), r.path.as_str())),
            Some(("HEAD", "gone.txt")),
            "an unchanged file falls back to a read-only HEAD overlay"
        );
    }

    #[test]
    fn annotate_parks_a_working_tree_blame_overlay() {
        use crate::view_state::{FilesMenuAction, InspectMode, InspectReq};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.repo.commits[0].hash = WORKING_REV.to_string();
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;

        // On the <current> row, Annotate blames the live working tree.
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; a.txt at 1
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::Annotate)));
        assert_eq!(
            state.view.queued_inspect().cloned(),
            Some(InspectReq {
                rev: WORKING_REV.to_string(),
                path: "a.txt".to_string(),
                title: "blame working tree - Esc to close".to_string(),
                mode: InspectMode::Blame,
                base: WORKING_REV.to_string(),
            }),
            "a working-tree blame is parked for the right-clicked file"
        );

        // The loaded overlay opens read-only and focuses the diff pane.
        state.apply(Msg::InspectLoaded {
            title: "blame working tree - Esc to close".to_string(),
            path: "a.txt".to_string(),
            view: Some(diff_preview(4)),
        });
        assert!(state.view.inspect.is_some(), "the blame overlay opens");
        assert_eq!(state.view.focus, Pane::Diff, "focus moves to the overlay");
        assert!(state.apply(Msg::CloseInspect));
        assert!(state.view.inspect.is_none(), "Esc closes the blame overlay");
    }

    /// Opening an overlay with View > Show Diff OFF forces the pane visible, but dismissing the
    /// overlay (Esc OR navigation) restores the user's toggle - it is not silently flipped on
    /// (which would also persist to state.toml across runs).
    #[test]
    fn inspect_overlay_restores_show_diff_when_dismissed() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.apply(Msg::SelectFile(0)); // root at 0; a.txt at 1
        // User turned the diff pane OFF.
        state.view.show_diff = false;

        // Overlay forces the pane visible...
        state.apply(Msg::InspectLoaded { title: "HEAD".to_string(), path: "a.txt".to_string(), view: Some(diff_preview(4)) });
        assert!(state.view.show_diff, "overlay forces the diff pane visible");

        // ...and Esc restores the user's OFF toggle.
        state.apply(Msg::CloseInspect);
        assert!(!state.view.show_diff, "Esc restores the user's Show Diff = OFF toggle");
        assert!(state.view.inspect_prior_show_diff.is_none(), "the captured prior is consumed");

        // Re-open, then dismiss by NAVIGATION instead of Esc: the toggle still restores.
        state.apply(Msg::InspectLoaded { title: "HEAD".to_string(), path: "a.txt".to_string(), view: Some(diff_preview(4)) });
        assert!(state.view.show_diff);
        state.apply(Msg::SelectCommit(1));
        assert!(!state.view.show_diff, "navigation also restores the user's toggle");
    }

    /// ToggleDiff while an overlay is open takes direct control of the diff pane: it drops the
    /// overlay AND its captured prior, then flips from the currently-forced-true state (so it
    /// hides what the user sees). No stale prior can later override the toggle.
    #[test]
    fn toggle_diff_while_overlay_open_drops_it_without_a_stale_prior() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.apply(Msg::SelectFile(0)); // root at 0; a.txt at 1
        state.view.show_diff = false; // user had the diff pane OFF

        // Overlay forces the pane visible and captures prior=false.
        state.apply(Msg::InspectLoaded { title: "HEAD".to_string(), path: "a.txt".to_string(), view: Some(diff_preview(4)) });
        assert!(state.view.show_diff && state.view.inspect.is_some());

        // Explicit ToggleDiff: overlay gone, pane hidden (toggled off what was visible), prior cleared.
        state.apply(Msg::ToggleDiff);
        assert!(state.view.inspect.is_none(), "the overlay is dropped by an explicit toggle");
        assert!(!state.view.show_diff, "the visible pane toggles OFF");
        assert!(state.view.inspect_prior_show_diff.is_none(), "no stale prior survives to override later");

        // A subsequent navigation must NOT resurrect the old prior and flip show_diff back on.
        state.apply(Msg::SelectCommit(1));
        assert!(!state.view.show_diff, "navigation does not re-apply a consumed prior");
    }

    /// A loaded inspect with NO view (the path did not exist at that revision) does not open an
    /// empty overlay - it reports a Notice instead.
    #[test]
    fn inspect_with_no_revision_reports_a_notice() {
        let mut state = state_with(3);
        assert!(state.apply(Msg::InspectLoaded { title: "HEAD".to_string(), path: "x".to_string(), view: None }));
        assert!(state.view.inspect.is_none(), "no overlay for a missing revision");
        assert!(matches!(&state.status, Status::Notice(n) if n.contains("No committed revision")));
    }

    /// Compare with Revision parks a file-revisions picker fetch; the loaded list opens the
    /// Picker dialog, and confirming a row parks a COMPARE inspect against that revision.
    #[test]
    fn compare_with_revision_opens_a_picker_then_parks_a_compare() {
        use crate::view_state::{FilesMenuAction, InspectMode, PickItem, PickKind};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;

        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; a.txt at 1
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::CompareWithRevision)));
        assert_eq!(
            state.view.queued_picklist().cloned(),
            Some(crate::view_state::PickListReq { kind: PickKind::FileRevisions, path: "a.txt".to_string(), mode: InspectMode::Compare }),
            "parks the file-revisions picker fetch"
        );

        // The list arrives -> the Picker dialog opens over it.
        let items = vec![
            PickItem { rev: "aaa111".to_string(), label: "aaa111  first".to_string() },
            PickItem { rev: "bbb222".to_string(), label: "bbb222  second".to_string() },
        ];
        assert!(state.apply(Msg::PickListLoaded { kind: PickKind::FileRevisions, path: "a.txt".to_string(), items, mode: InspectMode::Compare, epoch: state.view.nav_epoch }));
        assert!(matches!(&state.view.dialog, Some(Dialog::Picker { path, items, .. }) if path == "a.txt" && items.len() == 2));

        // Move to the second row and confirm -> a compare inspect against that rev is parked.
        state.apply(Msg::DialogMove(1));
        assert!(state.apply(Msg::DialogConfirm));
        assert!(state.view.dialog.is_none(), "confirming closes the picker");
        let req = state.view.queued_inspect().expect("a compare inspect is parked");
        assert_eq!(req.rev, "bbb222");
        assert_eq!(req.path, "a.txt");
        assert_eq!(req.mode, InspectMode::Compare);
    }

    /// Show History reuses the file-revisions picker but in CommitDiff mode: the fetch and the
    /// opened picker both carry CommitDiff, and confirming a row parks a CommitDiff inspect
    /// (what that commit changed to the file) under a "File history" title.
    #[test]
    fn show_history_navigates_the_log_to_the_picked_commit_and_reveals_the_file() {
        use crate::view_state::{FilesMenuAction, InspectMode, PickItem, PickKind};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.repo.commits[0].hash = WORKING_REV.to_string();
        state.repo.commits[1].hash = "c0ffee1".to_string(); // a real commit the picker will resolve to
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;

        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; a.txt at 1
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::ShowHistory)));
        let req = state.view.queued_picklist().expect("a picklist fetch is parked");
        assert_eq!((req.kind, req.mode), (PickKind::FileRevisions, InspectMode::CommitDiff));

        let items = vec![PickItem { rev: "c0ffee1".to_string(), label: "c0ffee1  01.01.2026  edit a".to_string() }];
        assert!(state.apply(Msg::PickListLoaded { kind: PickKind::FileRevisions, path: "a.txt".to_string(), items, mode: InspectMode::CommitDiff, epoch: state.view.nav_epoch }));
        assert!(matches!(&state.view.dialog, Some(Dialog::Picker { title, mode, .. }) if title == "File history" && *mode == InspectMode::CommitDiff));

        // Confirm NAVIGATES the log to the picked commit (no overlay) and parks the file reveal.
        assert!(state.apply(Msg::DialogConfirm));
        assert!(state.view.queued_inspect().is_none(), "Show History navigates instead of overlaying");
        assert_eq!(state.selected_commit_hash().as_deref(), Some("c0ffee1"), "the log selection moved to the picked commit");
        assert_eq!(state.view.parked_file_path.as_deref(), Some("a.txt"), "the opened file is parked to reveal");

        // The picked commit's tree arrives -> the file is re-selected by path and the park clears.
        state.apply(Msg::TreeLoaded {
            hash: "c0ffee1".to_string(),
            tree: vec![
                TreeNode::File { name: "z.txt".to_string(), status: FileStatus::Modified },
                TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified },
            ],
            ignored: std::collections::HashSet::new(),
        });
        assert!(state.view.parked_file_path.is_none(), "the parked reveal is consumed");
        assert_eq!(state.selected_file_path().as_deref(), Some("a.txt"), "the opened file is revealed by path");
    }

    #[test]
    fn show_history_falls_back_to_an_overlay_when_the_commit_is_filtered_out() {
        use crate::view_state::{InspectMode, PickItem, PickKind};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; a.txt at 1
        state.apply(Msg::FilesMenuPick(crate::view_state::FilesMenuAction::ShowHistory));
        // The picked rev matches no visible commit -> keep the read-only overlay path.
        let items = vec![PickItem { rev: "deadbee".to_string(), label: "deadbee  x".to_string() }];
        state.apply(Msg::PickListLoaded { kind: PickKind::FileRevisions, path: "a.txt".to_string(), items, mode: InspectMode::CommitDiff, epoch: state.view.nav_epoch });
        assert!(state.apply(Msg::DialogConfirm));
        let req = state.view.queued_inspect().expect("a commit-diff overlay is parked as fallback");
        assert_eq!((req.rev.as_str(), req.mode), ("deadbee", InspectMode::CommitDiff));
        assert!(state.view.parked_file_path.is_none(), "no navigation reveal on the fallback");
    }

    /// Compare with Branch parks a refs picker fetch; a STALE list (the selection moved) and an
    /// EMPTY list both avoid opening a picker (empty -> a Notice).
    #[test]
    fn compare_picklist_is_dropped_when_stale_or_empty() {
        use crate::view_state::{FilesMenuAction, PickKind};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![
            TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "b.txt".to_string(), status: FileStatus::Modified },
        ];
        state.view.files_flat = true;

        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // selects a.txt (root at 0)
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::CompareWithBranch)));
        assert_eq!(state.view.queued_picklist().unwrap().kind, PickKind::Refs);

        // Navigate away to b.txt, then a late list for a.txt is dropped (no picker over b.txt).
        state.apply(Msg::SelectFile(1));
        assert!(!state.apply(Msg::PickListLoaded {
            kind: PickKind::Refs,
            path: "a.txt".to_string(),
            items: vec![crate::view_state::PickItem { rev: "main".to_string(), label: "main  (branch)".to_string() }],
            mode: crate::view_state::InspectMode::Compare,
            epoch: state.view.nav_epoch,
        }));
        assert!(state.view.dialog.is_none(), "a stale picker list does not open");

        // An empty list for the CURRENT selection reports a Notice, opens no picker.
        assert!(state.apply(Msg::PickListLoaded { kind: PickKind::Refs, path: "b.txt".to_string(), items: vec![], mode: crate::view_state::InspectMode::Compare, epoch: state.view.nav_epoch }));
        assert!(state.view.dialog.is_none(), "empty list = no picker");
        assert!(matches!(&state.status, Status::Notice(n) if n.contains("No branches or tags")));
    }

    /// A late picker list must NOT clobber a modal the user opened during the async walk
    /// (e.g. an Alt+C commit input with a half-typed message). The reply is dropped instead.
    #[test]
    fn compare_picklist_does_not_clobber_a_dialog_opened_mid_load() {
        use crate::view_state::{InputKind, PickItem, PickKind};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "b.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.apply(Msg::SelectFile(0)); // b.txt selected (root at 0), still the picklist target

        // User opens a commit dialog while the file-revisions walk is still in flight.
        state.view.dialog = Some(Dialog::Input {
            kind: InputKind::Commit,
            field: crate::view_state::TextField::new("half typed".to_string()),
            commit: None,
            note: None,
            checkbox: None,
        });

        // The late list for the SAME selected file is dropped - the input survives intact.
        assert!(!state.apply(Msg::PickListLoaded {
            kind: PickKind::FileRevisions,
            path: "b.txt".to_string(),
            items: vec![PickItem { rev: "abc1234".to_string(), label: "abc1234  01.01.2026  x".to_string() }],
            mode: crate::view_state::InspectMode::Compare,
            epoch: state.view.nav_epoch,
        }));
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { kind: InputKind::Commit, field, .. }) if field.text() == "half typed"),
            "the user's commit input is not clobbered by the late picker");
    }

    /// A newly-Added working file has no HEAD version, so the menu hides Show Current Revision.
    #[test]
    fn show_current_revision_hidden_for_an_added_file() {
        use crate::view_state::FilesMenuAction;
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "new.txt".to_string(), status: FileStatus::Added }];
        state.view.files_flat = true;

        assert!(state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 })); // root at 0; new.txt at 1
        let menu = state.view.files_menu.as_ref().unwrap();
        assert!(!menu.has_head_version, "an Added file has no HEAD version");
        assert!(
            !menu.items().contains(&FilesMenuAction::ShowCurrentRevision),
            "Show Current Revision is hidden for a new file"
        );
    }

    /// A historical (non-working) commit's file gets the read-only inspect group (Show Current
    /// Revision / Compare / Show History / Annotate) with NO working-tree ops, even when the diff
    /// is already shown. A directory row on a historical commit opens nothing.
    #[test]
    fn files_menu_offers_the_readonly_group_on_a_historical_commit() {
        use crate::view_state::files_menu_items;
        use crate::view_state::FilesMenuAction::{
            Annotate, CommitFile, CopyPatch, CreatePatch, DeleteFile, Rollback, ShowCurrentRevision, ShowHistory,
        };
        // A working file with no local changes: no local-changes items.
        assert!(
            files_menu_items(true, false, false, false, false)
                .iter()
                .all(|a| !matches!(a, CopyPatch | CreatePatch | CommitFile | Rollback | DeleteFile)),
            "no local-changes items without local changes"
        );
        // Nothing committed + no diff item to add: the menu is empty.
        assert!(
            files_menu_items(false, false, false, false, false).is_empty(),
            "no items without changes, a diff to show, or a committed version"
        );
        // A historical commit's file (committed=true) offers the read-only group - even with the
        // diff already shown (show_diff_item=false) - and never a working-tree op.
        let hist = files_menu_items(false, false, false, true, false);
        assert!(
            hist.contains(&ShowCurrentRevision) && hist.contains(&ShowHistory) && hist.contains(&Annotate),
            "the read-only inspect group is offered: {hist:?}"
        );
        assert!(
            hist.iter().all(|a| !matches!(a, Rollback | DeleteFile | CommitFile | CopyPatch | CreatePatch)),
            "no working-tree ops on a committed file: {hist:?}"
        );

        // A historical commit opens that menu live (with the diff already shown).
        let mut state = state_with(3);
        state.apply(Msg::SelectCommit(1)); // a real commit in this fixture
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.view.show_diff = true;
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; a.txt at 1
        let menu = state.view.files_menu.as_ref().expect("a historical commit opens the read-only menu");
        assert!(menu.committed && !menu.local_changes, "committed file, no local changes");
        assert!(menu.items().contains(&Annotate) && !menu.items().contains(&Rollback));

        // A directory row (no file path) on a historical commit opens no menu.
        state.view.files_menu = None;
        state.view.show_diff = false;
        state.repo.tree = vec![TreeNode::Dir {
            name: "src".to_string(),
            file_count: 1,
            expanded: false,
            children: vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }],
        }];
        state.view.files_flat = false;
        assert!(!state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 })); // row 1 = the src/ dir (root at 0)
        assert!(state.view.files_menu.is_none(), "a directory row opens no files menu");

        // An UNCHANGED file on the working row (All-files view) offers Show Diff but NOT
        // Rollback (it has nothing to revert) - no dead no-op item.
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "clean.txt".to_string(), status: FileStatus::Unchanged }];
        state.view.files_flat = true;
        state.view.show_diff = false;
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; clean.txt at 1
        let menu = state.view.files_menu.as_ref().expect("Show Diff still offered");
        assert!(!menu.local_changes, "an unchanged file has no local-changes items");
        // Show Diff only (no Rollback - nothing to revert).
        use crate::view_state::FilesMenuAction::ShowDiff;
        assert_eq!(menu.items(), [ShowDiff]);
    }

    /// Right-clicking a DIRECTORY row opens the FOLDER menu but must have NO file-selection
    /// side effects: no autosave, no editor drop, no selection move (folder actions read the
    /// snapshotted prefix, not the cursor, so `select_file` must never run for a dir).
    #[test]
    fn files_menu_right_click_on_a_dir_opens_the_folder_menu_without_side_effects() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![
            TreeNode::Dir {
                name: "src".to_string(),
                file_count: 1,
                expanded: false, // collapsed: x.txt is hidden, so row 1 = the dir, row 2 = b.txt (root at 0)
                children: vec![TreeNode::File { name: "x.txt".to_string(), status: FileStatus::Modified }],
            },
            TreeNode::File { name: "b.txt".to_string(), status: FileStatus::Modified },
        ];
        state.view.files_flat = false;
        // Select b.txt (row 2) and load a dirty editable buffer on it.
        state.view.files_sel = 1;
        let path = state.selected_file_path().expect("b.txt selected");
        let commit = state.selected_commit_hash().unwrap();
        state.apply(Msg::EditFileLoaded { commit, path, base: Some("a\n".to_string()), work: "a\n".to_string() });
        state.view.focus = Pane::Diff;
        state.apply(Msg::Edit(crate::message::EditOp::Insert('Z'))); // dirty
        assert!(state.view.editor.as_ref().unwrap().dirty);
        state.view.effects.clear();

        // Right-click the DIRECTORY row (index 0): the folder menu opens, scoped to "src".
        assert!(state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }));
        let menu = state.view.files_menu.as_ref().expect("the folder menu opens");
        assert!(menu.is_dir && menu.path == "src", "the menu targets the src/ prefix");
        // But nothing about the file selection / editor is disturbed.
        assert!(state.view.editor.is_some(), "the editable buffer is NOT dropped");
        assert!(state.view.queued_save().is_none(), "no silent autosave on a dir right-click");
        assert_eq!(state.view.files_sel, 1, "the selection did not move");
    }

    /// A directory with NO changed file under it (an All-files-view dir of only unchanged
    /// files, or a non-working commit) opens NO folder menu - the verbs would be dead no-ops.
    #[test]
    fn dir_with_no_changes_opens_no_folder_menu() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::Dir {
            name: "src".to_string(),
            file_count: 1,
            expanded: false,
            children: vec![TreeNode::File { name: "x.txt".to_string(), status: FileStatus::Unchanged }],
        }];
        state.view.files_flat = false;
        assert!(!state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 })); // root at 0; src/ dir at 1
        assert!(state.view.files_menu.is_none(), "no folder menu when nothing under it changed");
    }

    /// The folder menu's Commit Directory item opens a commit dialog threading the folder
    /// prefix; confirming parks a `CommitFolder` git action against that prefix. Rollback
    /// collects every changed file under the prefix.
    #[test]
    fn folder_menu_commit_and_rollback_target_the_prefix() {
        use crate::view_state::{FilesMenuAction, GitAction, InputKind};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::Dir {
            name: "src".to_string(),
            file_count: 2,
            expanded: true,
            children: vec![
                TreeNode::File { name: "a.rs".to_string(), status: FileStatus::Modified },
                TreeNode::File { name: "b.rs".to_string(), status: FileStatus::Added },
            ],
        }];
        state.view.files_flat = false;

        // Commit Directory -> a commit dialog threading "src", confirm -> CommitFolder { dir: src }.
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; src/ dir at 1
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::CommitFolder)));
        match &state.view.dialog {
            Some(Dialog::Input { kind: InputKind::CommitFolder, commit, .. }) => {
                assert_eq!(commit.as_deref(), Some("src"), "threads the folder prefix");
            }
            other => panic!("expected a CommitFolder input dialog, got {other:?}"),
        }
        state.apply(Msg::DialogInput('m'));
        state.apply(Msg::DialogConfirm);
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::CommitFolder { dir: "src".to_string(), message: "m".to_string() }),
        );

        // Rollback -> the revert modal lists BOTH changed files under src/.
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; src/ dir at 1
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::Rollback)));
        let req = state.view.revert_confirm.as_ref().expect("a revert modal opens");
        assert_eq!(req.paths, vec!["src/a.rs".to_string(), "src/b.rs".to_string()], "every changed file under src/");
    }

    /// Right-clicking a MARKED file (with >=2 marks) on the working row opens the MARKED-SET
    /// menu (the bulk verbs over the marked set), keeps the marks visible (no select_file
    /// clear), and routes each action to its marked-set GitAction/dialog over the snapshot.
    #[test]
    fn marked_set_menu_runs_bulk_verbs_over_the_selection() {
        use crate::view_state::{FilesMenuAction, GitAction, InputKind};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![
            TreeNode::File { name: "a.rs".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "b.rs".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "c.rs".to_string(), status: FileStatus::Modified },
        ];
        state.view.files_flat = true;
        // Mark a.rs and b.rs (Space toggles the cursor row).
        state.view.files_sel = 0;
        state.apply(Msg::ToggleMark(0));
        state.apply(Msg::ToggleMark(1));
        assert_eq!(state.view.files_marked.len(), 2, "two files marked");

        // Right-click a MARKED file (a.rs, row 0) -> the marked-set menu, marks intact.
        assert!(state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }));
        let menu = state.view.files_menu.as_ref().expect("the marked-set menu opens");
        assert_eq!(menu.marked, vec!["a.rs".to_string(), "b.rs".to_string()], "snapshots the marked set");
        assert_eq!(
            menu.items(),
            [
                FilesMenuAction::CommitSelected,
                FilesMenuAction::CopyPatchSelected,
                FilesMenuAction::CreatePatchSelected,
                FilesMenuAction::RollbackSelected,
                FilesMenuAction::DeleteSelected,
            ],
            "the bulk set-verbs",
        );
        assert_eq!(state.view.files_marked.len(), 2, "the marks stay visible (no select_file clear)");

        // Commit Selected -> a message dialog; confirm -> CommitSelected over the snapshot.
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::CommitSelected)));
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { kind: InputKind::CommitSelected, .. })));
        state.apply(Msg::DialogInput('m'));
        state.apply(Msg::DialogConfirm);
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::CommitSelected {
                paths: vec!["a.rs".to_string(), "b.rs".to_string()],
                message: "m".to_string(),
            }),
        );

        // Delete Selected -> a confirm parking DeleteSelected over the snapshot. The marks
        // persist through the dialog flow (only a repo reload clears them), so re-open directly.
        state.view.effects.clear();
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::DeleteSelected)));
        state.apply(Msg::DialogConfirm);
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::DeleteSelected { paths: vec!["a.rs".to_string(), "b.rs".to_string()] }),
        );

        // Copy Selected as Patch -> queues the multi-path clipboard request.
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::FilesMenuPick(FilesMenuAction::CopyPatchSelected)));
        assert_eq!(
            state.view.effects.last().cloned(),
            Some(Effect::CopyPatchMulti(vec!["a.rs".to_string(), "b.rs".to_string()])),
        );
    }

    /// "Create archive" opens ONE input (filename prefilled `.zip`) with a format chip row; Tab
    /// cycles the format zip -> tar.gz -> tar by rewriting the filename extension, and confirming
    /// parks `ArchiveProject` over the entered path (the backend reads the format from the ext).
    #[test]
    fn create_archive_is_one_dialog_with_a_tab_cycled_format() {
        use crate::view_state::{CommitMenuAction, GitAction, InputKind};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.view.repo_root_name = "demo".to_string();
        state.view.today = "2026-06-09".to_string();
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::CreateArchive)));
        // ONE input dialog (no Choice), prefilled with the .zip default.
        match &state.view.dialog {
            Some(Dialog::Input { kind: InputKind::ArchiveProject, field, .. }) => {
                assert!(field.text().ends_with(".zip"), "default is zip: {:?}", field.text());
            }
            other => panic!("expected the archive input, got {other:?}"),
        }
        // Tab cycles the extension: zip -> tar.gz.
        state.apply(Msg::DialogCycleArchiveFormat);
        match &state.view.dialog {
            Some(Dialog::Input { kind: InputKind::ArchiveProject, field, .. }) => {
                assert!(
                    field.text().starts_with("/tmp/demo-") && field.text().ends_with(".tar.gz"),
                    "Tab rewrote the extension to .tar.gz keeping the stem: {:?}",
                    field.text(),
                );
            }
            other => panic!("expected the archive input, got {other:?}"),
        }
        // Tab again -> tar; a third -> back to zip (cycle).
        state.apply(Msg::DialogCycleArchiveFormat);
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { field, .. }) if field.text().ends_with(".tar") && !field.text().ends_with(".tar.gz")));
        state.apply(Msg::DialogCycleArchiveFormat);
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { field, .. }) if field.text().ends_with(".zip")));

        // Cycle to tar.gz and confirm -> ArchiveProject over the .tar.gz path.
        state.apply(Msg::DialogCycleArchiveFormat);
        state.apply(Msg::DialogConfirm);
        assert!(
            matches!(state.view.queued_git(), Some(GitAction::ArchiveProject { path, .. }) if path.ends_with(".tar.gz")),
            "parks ArchiveProject over the .tar.gz path: {:?}",
            state.view.queued_git().cloned(),
        );
    }

    /// Ctrl-click marks commits into the multi-selection; right-clicking a MARKED commit (>=2
    /// marks) opens the MULTI-COMMIT menu (cherry-pick / patch series) over the snapshot,
    /// ordered oldest-first; a plain select clears the set.
    #[test]
    fn multi_commit_menu_acts_on_the_marked_set_oldest_first() {
        use crate::view_state::{CommitMenuAction, GitAction, InputKind};
        let mut state = state_with(3); // commits h000000 (newest) .. h000002 (oldest)
        // Ctrl-click marks visible rows 0 and 1 (h000000, h000001).
        state.apply(Msg::ToggleCommitMark(0));
        state.apply(Msg::ToggleCommitMark(1));
        assert_eq!(state.view.commits_marked.len(), 2, "two commits marked");

        // Right-click a MARKED commit -> the multi-commit menu, marks kept visible.
        assert!(state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 }));
        let menu = state.view.commit_menu.as_ref().expect("the multi-commit menu opens");
        // Ordered oldest-first: h000001 before h000000 (the newest is last).
        assert_eq!(menu.marked, vec!["h000001".to_string(), "h000000".to_string()]);
        assert_eq!(
            menu.parent_rows(),
            crate::view_state::commit_marked_menu_rows(),
            "the menu shows the set-verbs, not the single-commit rows",
        );
        assert_eq!(state.view.commits_marked.len(), 2, "marks stay visible");

        // Cherry-Pick Selected -> a confirm parking the ordered set.
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::CherryPickSelected)));
        state.apply(Msg::DialogConfirm);
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::CherryPickSelected {
                commits: vec!["h000001".to_string(), "h000000".to_string()],
            }),
        );

        // Create Patch Series -> a directory input parking the set; confirm -> the action.
        state.view.effects.clear();
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::CreatePatchSeries)));
        assert!(matches!(&state.view.dialog, Some(Dialog::Input { kind: InputKind::CreatePatchSeries, .. })));
        state.apply(Msg::DialogConfirm);
        assert!(
            matches!(state.view.queued_git(), Some(GitAction::CreatePatchSeries { commits, .. }) if commits.len() == 2),
            "parks the patch-series action over the 2 commits: {:?}",
            state.view.queued_git().cloned(),
        );

        // A plain select clears the multi-selection.
        state.apply(Msg::SelectCommit(2));
        assert!(state.view.commits_marked.is_empty(), "a plain click drops the set");
    }

    /// The log's "Load more history" row queues `Effect::LoadMore` (the runtime turns it into
    /// `Req::LoadMore`) only when more history exists; it is a no-op otherwise (a stale click).
    #[test]
    fn load_more_parks_only_when_more_history_exists() {
        let mut state = state_with(3);
        state.repo.more_history = false;
        assert!(!state.apply(Msg::LoadMore), "no-op when the whole history is loaded");
        assert!(!state.view.effects.contains(&Effect::LoadMore));

        state.repo.more_history = true;
        assert!(state.apply(Msg::LoadMore), "parks the deeper load");
        assert!(state.view.effects.contains(&Effect::LoadMore), "the runtime will fire Req::LoadMore");
        // A second LoadMore while one is already parked does not double-arm.
        assert!(!state.apply(Msg::LoadMore));
    }

    /// View > Blame toggles `show_blame`: turning it ON parks a blame fetch; OFF drops the
    /// loaded blame. A `BlameLoaded` reply is kept only if its path still matches the selection.
    #[test]
    fn blame_toggle_parks_a_fetch_and_loaded_blame_matches_the_selection() {
        use crate::diff::{BlameFile, BlameLine};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.rs".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.view.files_sel = 0;

        assert!(state.apply(Msg::ToggleShowBlame));
        assert!(state.view.show_blame && state.view.effects.contains(&Effect::ReloadBlame), "ON parks a fetch");

        // A reply for the SELECTED file is stored.
        let blame = BlameFile {
            path: "a.rs".to_string(),
            lines: vec![BlameLine { commit: "abc1234".into(), author: "Ada".into(), date: "x".into(), tokens: vec![] }],
        };
        state.apply(Msg::BlameLoaded { rev: state.selected_commit_hash().unwrap(), path: "a.rs".to_string(), blame });
        assert!(state.view.blame.is_some(), "blame for the selected file is stored");

        // A stale reply (different path) is dropped.
        let stale = BlameFile { path: "other.rs".to_string(), lines: vec![] };
        state.apply(Msg::BlameLoaded { rev: state.selected_commit_hash().unwrap(), path: "other.rs".to_string(), blame: stale });
        assert_eq!(state.view.blame.as_ref().unwrap().path, "a.rs", "stale reply dropped");

        // OFF drops the blame.
        assert!(state.apply(Msg::ToggleShowBlame));
        assert!(!state.view.show_blame && state.view.blame.is_none(), "OFF clears it");
    }

    /// A reload landing while an editable buffer is OPEN must not null the preview into a
    /// stuck "Loading diff..." (the watch tick skips sending while an editor is open, but a
    /// reload can already be in flight, and every post-write reload lands here). The buffer's
    /// live diff is kept and the file re-anchors by path on the next tree.
    #[test]
    fn repo_reload_under_an_open_editor_keeps_the_live_diff() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.preselect_file(0);
        let commit = state.selected_commit_hash().unwrap();
        state.apply(Msg::EditFileLoaded {
            commit,
            path: "a.txt".to_string(),
            base: Some("x\n".to_string()),
            work: "x\ny\n".to_string(),
        });
        assert!(state.repo.preview.is_some(), "the live diff is up");

        let mut fresh = repo_with(3);
        fresh.commits[0].is_working = true;
        state.apply(Msg::RepoLoaded(Box::new(fresh)));
        assert!(state.view.editor.is_some(), "the buffer survives the reload");
        assert!(state.repo.preview.is_some(), "the live diff is rebuilt, not nulled");
        assert_eq!(
            state.view.parked_file_path.as_deref(),
            Some("a.txt"),
            "the file re-anchors by path once the new tree lands"
        );
    }

    #[test]
    fn poll_refresh_under_a_dirty_editor_swaps_the_working_tree() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.preselect_file(0);
        state.repo.status_sig = Some(1);
        let commit = state.selected_commit_hash().unwrap();
        state.apply(Msg::EditFileLoaded {
            commit,
            path: "a.txt".to_string(),
            base: Some("x\n".to_string()),
            work: "x\ny\n".to_string(),
        });
        state.apply(Msg::Edit(crate::message::EditOp::Insert('Z')));
        assert!(state.view.editor.as_ref().unwrap().dirty, "buffer dirty");
        assert!(state.apply(Msg::StatusPolled { sig: Some(2) }));
        assert!(state.view.effects.contains(&Effect::RefreshRepo));
        state.view.effects.clear();

        let mut fresh = repo_with(3);
        fresh.commits[0].is_working = true;
        fresh.status_sig = Some(2);
        fresh.tree = vec![
            TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "second.txt".to_string(), status: FileStatus::Added },
        ];
        state.apply(Msg::RepoLoaded(Box::new(fresh)));
        assert!(state.view.editor.as_ref().unwrap().dirty, "dirty buffer survives");
        assert_eq!(state.repo.tree.len(), 2, "the fresh working tree is swapped in, not kept");
        assert!(state.view.effects.contains(&Effect::ReloadTree), "tree re-fetch queued");
    }

    /// THE CHOKE-POINT MATRIX, as one executable table: every navigation-transient seeded
    /// non-default, every navigation-class Msg applied, the documented reset policy asserted.
    /// This is the spec for the "state that survives navigation" bug class (the one that
    /// killed the pin feature twice): a choke point that stops resetting a transient - or a
    /// new transient missing a row here - fails THIS test instead of shipping as a frozen
    /// overlay. Extend the table when adding either.
    #[test]
    fn navigation_choke_points_reset_their_transients() {
        use crate::diff::BlameFile;
        use crate::view_state::InspectView;

        // A 3-commit working-row repo with two files, an open editable buffer, and EVERY
        // transient seeded: parked wheel scrolls, a manual hscroll, a blame gutter, a
        // read-only overlay (built last so dismiss paths are exercised).
        let seeded = || {
            let mut state = state_with(3);
            state.repo.commits[0].is_working = true;
            state.apply(Msg::SelectCommit(0));
            state.repo.tree = vec![
                TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified },
                TreeNode::File { name: "b.txt".to_string(), status: FileStatus::Modified },
            ];
            state.view.files_flat = true;
            state.preselect_file(0); // root at 0; a.txt at 1, b.txt at 2
            state.view.log_scroll = Some(1);
            state.view.files_scroll = Some(1);
            state.view.edit_scroll = Some(1);
            state.view.diff_hscroll = Some(1);
            state.view.blame = Some(BlameFile { path: "a.txt".to_string(), lines: vec![] });
            state.view.inspect =
                Some(InspectView { title: "t".to_string(), view: diff_preview(4) });
            state.view.effects.clear();
            state
        };

        // Keyboard commit move: EVERYTHING refollows (the cursor must snap into view).
        let mut s = seeded();
        s.view.focus = Pane::Log;
        s.apply(Msg::Move(1));
        assert_eq!(s.view.log_scroll, None, "keyboard log move refollows the log");
        assert_eq!((s.view.edit_scroll, s.view.diff_hscroll), (None, None), "diff scrolls reset");
        assert!(s.view.blame.is_none(), "a commit move drops the old rev's blame");
        assert!(s.view.inspect.is_none(), "a commit move dismisses the overlay");

        // Mouse commit click: the parked log viewport is KEPT (the row is on screen by
        // construction); the diff-pane transients still reset.
        let mut s = seeded();
        s.apply(Msg::SelectCommit(2));
        assert_eq!(s.view.log_scroll, Some(1), "a mouse click keeps the parked viewport");
        assert!(s.view.blame.is_none() && s.view.inspect.is_none(), "diff transients reset");

        // Programmatic file select: the files viewport refollows; diff transients reset.
        let mut s = seeded();
        s.apply(Msg::SelectFile(2));
        assert_eq!(s.view.files_scroll, None, "SelectFile refollows the files list");
        assert_eq!((s.view.edit_scroll, s.view.diff_hscroll), (None, None));
        assert!(s.view.blame.is_none(), "a file move drops the old file's blame");
        assert!(s.view.inspect.is_none(), "a file move dismisses the overlay");

        // Mouse file click: the parked files viewport is KEPT; the rest still resets.
        let mut s = seeded();
        s.apply(Msg::ClickFile(2));
        assert_eq!(s.view.files_scroll, Some(1), "a mouse click keeps the parked viewport");
        assert!(s.view.blame.is_none() && s.view.inspect.is_none(), "diff transients reset");

        // A files-search keystroke re-lands the selection: a navigation choke point.
        let mut s = seeded();
        s.apply(Msg::FilesSearchFocus);
        s.apply(Msg::FilesSearchPush('b'));
        assert_eq!(s.view.files_scroll, None, "a search reshape refollows the list");
        assert!(s.view.inspect.is_none(), "a search reshape dismisses the overlay");
    }

    /// A blame reply whose PATH still matches but whose REV does not (the user navigated
    /// commits keeping the same file selected) is dropped: a path-only guard accepted the
    /// wrong-rev gutter.
    #[test]
    fn stale_wrong_rev_blame_reply_is_dropped() {
        use crate::diff::BlameFile;
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.rs".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.view.files_sel = 0;
        state.view.show_blame = true;
        let working_rev = state.selected_commit_hash().unwrap();

        // Navigate to a historical commit; the same file path stays selected in this fixture.
        state.apply(Msg::SelectCommit(1));
        state.view.files_sel = 0;

        // The in-flight working-tree blame lands AFTER the move: same path, old rev -> dropped.
        let stale = BlameFile { path: "a.rs".to_string(), lines: vec![] };
        state.apply(Msg::BlameLoaded { rev: working_rev, path: "a.rs".to_string(), blame: stale });
        assert!(state.view.blame.is_none(), "a wrong-rev blame reply must not paint the gutter");
    }

    /// A dialog-opening read (picker / ref list / remotes) that lands AFTER a navigation is
    /// dropped by the epoch guard instead of popping a modal over whatever the user moved to.
    #[test]
    fn late_dialog_opening_reply_after_navigation_is_dropped() {
        use crate::view_state::{InspectMode, PickItem, PickKind, RefOp};
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        let stale_epoch = state.view.nav_epoch;

        // The user navigates before the replies land: every later reply is stale.
        state.apply(Msg::SelectCommit(1));
        let items = vec![PickItem { rev: "abc".to_string(), label: "abc".to_string() }];
        assert!(!state.apply(Msg::PickListLoaded {
            kind: PickKind::Refs,
            path: "a.txt".to_string(),
            items: items.clone(),
            mode: InspectMode::Compare,
            epoch: stale_epoch,
        }));
        assert!(!state.apply(Msg::RefListLoaded { op: RefOp::Merge, items, epoch: stale_epoch }));
        assert!(!state.apply(Msg::RemotesLoaded { remotes: vec![], epoch: stale_epoch }));
        assert!(state.view.dialog.is_none(), "no late modal pops after a navigation");
    }

    /// Ctrl+C in the EDITABLE diff must reach the SYSTEM clipboard (not only the in-app
    /// register): a Copy with a selection queues `Effect::Clipboard` so the runtime hands it to
    /// wl-copy/xclip, the same as the read-only diff's `Msg::CopyText`.
    #[test]
    fn editor_copy_mirrors_the_selection_to_the_system_clipboard() {
        use crate::message::EditOp;
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.rs".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.view.files_sel = 0;
        let commit = state.selected_commit_hash().unwrap();
        state.apply(Msg::EditFileLoaded {
            commit,
            path: "a.rs".to_string(),
            base: Some("hello\n".to_string()),
            work: "hello\n".to_string(),
        });
        state.view.focus = Pane::Diff;
        state.apply(Msg::Edit(EditOp::SelectAll));
        state.apply(Msg::Edit(EditOp::Copy));
        assert_eq!(
            state.view.queued_clipboard(),
            Some("hello"),
            "the editable-diff Copy reaches the system clipboard",
        );
    }

    /// A lone mark (a single selected file) keeps the FULL single-file menu, not the bulk one
    /// (WebStorm behavior): the marked-set menu needs >=2 marks.
    #[test]
    fn a_single_mark_keeps_the_single_file_menu() {
        use crate::view_state::FilesMenuAction;
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![
            TreeNode::File { name: "a.rs".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "b.rs".to_string(), status: FileStatus::Modified },
        ];
        state.view.files_flat = true;
        state.view.files_sel = 0;
        state.apply(Msg::ToggleMark(0));
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 });
        let menu = state.view.files_menu.as_ref().expect("a menu opens");
        assert!(menu.marked.is_empty(), "one mark is NOT a marked-set menu");
        assert!(menu.items().contains(&FilesMenuAction::CommitFile), "the single-file verbs are present");
    }

    /// A right-click on a files row that opens NO menu still acts as a click-away: it
    /// dismisses an already-open commit/files context menu (no stale floating popup).
    #[test]
    fn right_click_no_menu_files_row_dismisses_an_open_context_menu() {
        let mut state = state_with(3);
        state.repo.tree = vec![TreeNode::Dir {
            name: "src".to_string(),
            file_count: 1,
            expanded: false,
            children: vec![TreeNode::File { name: "x.txt".to_string(), status: FileStatus::Unchanged }],
        }];
        state.view.files_flat = false;
        state.view.show_diff = true;
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.view.commit_menu.is_some());
        // Right-click the directory row (no files menu) -> dismiss the open commit menu.
        assert!(state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 })); // root at 0; src/ dir at 1
        assert!(state.view.commit_menu.is_none(), "the right-click click-away dismisses the commit menu");
        assert!(state.view.files_menu.is_none(), "and opens no files menu on a dir row");
    }

    /// A keyboard log navigation closes an open files context menu (the file list it floats
    /// over reloads for the new commit); opening a commit menu closes it too.
    #[test]
    fn files_menu_closes_on_commit_navigation_and_commit_menu() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.view.show_diff = false;

        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; a.txt at 1
        assert!(state.view.files_menu.is_some());
        // A keyboard log navigation (focus Log -> select_commit) closes it.
        state.view.focus = Pane::Log;
        state.apply(Msg::Move(1));
        assert!(state.view.files_menu.is_none(), "log navigation closes the files menu");

        // Re-open, then opening a commit context menu closes it (mutually exclusive).
        state.apply(Msg::SelectCommit(0));
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; a.txt at 1
        assert!(state.view.files_menu.is_some());
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.view.files_menu.is_none(), "opening the commit menu closes the files menu");
        assert!(state.view.commit_menu.is_some());
    }

    #[test]
    fn working_row_verbs_route_to_their_dialogs() {
        let open_working = || {
            let mut s = state_with(3);
            s.repo.commits[0].is_working = true;
            s.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
            s
        };
        // Commit Changes opens the same input dialog as the action-bar Commit.
        let mut s = open_working();
        s.apply(Msg::CommitMenuPick(CommitMenuAction::CommitChanges));
        assert!(
            matches!(&s.view.dialog, Some(Dialog::Input { kind: InputKind::Commit, .. })),
            "Commit Changes opens the commit message dialog"
        );
        // Stash + Discard open a confirm carrying the matching working-tree GitAction.
        let mut s = open_working();
        s.apply(Msg::CommitMenuPick(CommitMenuAction::StashChanges));
        assert!(matches!(&s.view.dialog, Some(Dialog::Confirm { action: GitAction::Stash, .. })));
        let mut s = open_working();
        s.apply(Msg::CommitMenuPick(CommitMenuAction::DiscardChanges));
        assert!(matches!(&s.view.dialog, Some(Dialog::Confirm { action: GitAction::DiscardAll, .. })));
    }

    #[test]
    fn real_commit_menu_groups_undo_with_revert() {
        use crate::view_state::{commit_menu_items, CommitMenuAction::*, CommitRow};
        // Task: Undo Commit sits in the SAME separator-group as Revert Commit (the "undo this
        // commit" group), not down in the rewrite-history group with Edit/Reset/Rebase.
        let rows = commit_menu_items(false, true);
        let revert = rows.iter().position(|r| matches!(r, CommitRow::Action(RevertCommit, _))).unwrap();
        let undo = rows.iter().position(|r| matches!(r, CommitRow::Action(UndoCommit, _))).unwrap();
        let sep_between = rows[revert..undo].iter().any(|r| matches!(r, CommitRow::Sep));
        assert!(undo > revert && !sep_between, "Undo Commit follows Revert Commit with no separator between");
    }

    #[test]
    fn commit_menu_reword_warns_when_rewriting_history() {
        // A non-HEAD commit -> rewording rewrites newer commits; the dialog warns.
        let mut state = state_with(3);
        state.repo.commits[0].head = true;
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::EditMessage));
        match &state.view.dialog {
            Some(Dialog::Input { kind: InputKind::Reword, note: Some(n), .. }) => {
                assert!(n.contains("Rewrites history"), "warns about rewriting history: {n}")
            }
            other => panic!("expected a warned Reword dialog, got {other:?}"),
        }
    }

    #[test]
    fn commit_menu_reword_warns_when_pushed() {
        // A pushed commit -> rewording rewrites published history; the dialog warns.
        let mut state = state_with(3);
        state.repo.has_remotes = true;
        state.repo.commits[0].head = true; // HEAD, but pushed (not in unpushed)
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::EditMessage));
        match &state.view.dialog {
            Some(Dialog::Input { kind: InputKind::Reword, note: Some(n), .. }) => {
                assert!(n.contains("pushed"), "warns about published history: {n}")
            }
            other => panic!("expected a warned Reword dialog, got {other:?}"),
        }
    }

    #[test]
    fn opening_the_commit_menu_closes_other_popups_and_vice_versa() {
        let mut state = state_with(3);
        state.apply(Msg::OpenMenu(MenuId::Editor));
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert_eq!(state.view.open_menu, None, "opening the commit menu closes the top menu");
        assert!(state.view.commit_menu.is_some());
        // Opening a top menu closes the commit menu.
        state.apply(Msg::OpenMenu(MenuId::Editor));
        assert_eq!(state.view.commit_menu, None);
    }

    #[test]
    fn keyboard_move_dismisses_the_commit_menu() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::Move(1)), "the move repaints (menu closed / selection moved)");
        assert_eq!(state.view.commit_menu, None, "a keyboard move closes the menu");
    }

    #[test]
    fn close_commit_menu_reports_the_change() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CloseCommitMenu));
        assert_eq!(state.view.commit_menu, None);
        assert!(!state.apply(Msg::CloseCommitMenu), "closing again is a no-op");
    }

    #[test]
    fn commit_menu_new_branch_opens_input_then_parks_branch_at() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::NewBranch)));
        assert_eq!(state.view.commit_menu, None, "opening the dialog closes the menu");
        match &state.view.dialog {
            Some(Dialog::Input { kind: InputKind::NewBranch, commit: Some(h), .. }) => {
                assert_eq!(h, "h000001", "the target commit hash is snapshotted")
            }
            other => panic!("expected a NewBranch input dialog, got {other:?}"),
        }
        state.apply(Msg::DialogInput('x'));
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::BranchAt {
                name: "x".to_string(),
                commit: "h000001".to_string(),
                checkout: true, // the checkbox defaults on
            })
        );
    }

    #[test]
    fn commit_menu_new_branch_checkbox_toggles_checkout() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::NewBranch));
        state.apply(Msg::DialogToggleCheck); // turn checkout OFF
        state.apply(Msg::DialogInput('x'));
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::BranchAt {
                name: "x".to_string(),
                commit: "h000001".to_string(),
                checkout: false,
            })
        );
    }

    #[test]
    fn commit_menu_new_tag_parks_tag_at() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 2, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::NewTag));
        state.apply(Msg::DialogInput('v'));
        state.apply(Msg::DialogInput('1'));
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::TagAt { name: "v1".to_string(), commit: "h000002".to_string() })
        );
    }

    #[test]
    fn commit_menu_branch_tag_skip_the_working_row() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true; // make the top row synthetic
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::NewBranch)));
        assert!(state.view.dialog.is_none(), "no branch dialog on the working row");
    }

    #[test]
    fn commit_menu_checkout_opens_confirm_then_parks_checkout() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::Checkout)));
        assert_eq!(state.view.commit_menu, None, "opening the confirm closes the menu");
        match &state.view.dialog {
            Some(Dialog::Confirm { action: GitAction::Checkout { commit }, prompt }) => {
                assert_eq!(commit, "h000001", "the target commit hash is snapshotted");
                assert!(prompt.contains("detach"), "the prompt warns HEAD detaches: {prompt:?}");
            }
            other => panic!("expected a Checkout confirm dialog, got {other:?}"),
        }
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::Checkout { commit: "h000001".to_string() })
        );
    }

    #[test]
    fn commit_menu_checkout_skips_the_working_row() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true; // make the top row synthetic
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::Checkout)));
        assert!(state.view.dialog.is_none(), "no checkout confirm on the working row");
        assert!(matches!(state.status, Status::Notice(_)), "it hints instead");
    }

    #[test]
    fn commit_menu_cherry_pick_opens_confirm_then_parks_action() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 2, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::CherryPick)));
        match &state.view.dialog {
            Some(Dialog::Confirm { action: GitAction::CherryPick { commit }, prompt }) => {
                assert_eq!(commit, "h000002", "the target commit hash is snapshotted");
                assert!(prompt.contains("Cherry-pick"), "the prompt names the op: {prompt:?}");
            }
            other => panic!("expected a CherryPick confirm dialog, got {other:?}"),
        }
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::CherryPick { commit: "h000002".to_string() })
        );
    }

    #[test]
    fn commit_menu_revert_commit_opens_confirm_then_parks_action() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::RevertCommit)));
        match &state.view.dialog {
            Some(Dialog::Confirm { action: GitAction::RevertCommit { commit }, prompt }) => {
                assert_eq!(commit, "h000001", "the target commit hash is snapshotted");
                assert!(prompt.contains("Revert"), "the prompt names the op: {prompt:?}");
            }
            other => panic!("expected a RevertCommit confirm dialog, got {other:?}"),
        }
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::RevertCommit { commit: "h000001".to_string() })
        );
    }

    #[test]
    fn commit_menu_cherry_pick_revert_skip_the_working_row() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true; // make the top row synthetic
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::CherryPick)));
        assert!(state.view.dialog.is_none(), "no cherry-pick confirm on the working row");
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::RevertCommit)));
        assert!(state.view.dialog.is_none(), "no revert confirm on the working row");
    }

    #[test]
    fn commit_menu_reset_opens_mode_picker_then_parks_chosen_mode() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::ResetHere)));
        match &state.view.dialog {
            Some(Dialog::Choice { kind: ChoiceKind::ResetMode, sel: 0, commit, .. }) => {
                assert_eq!(commit, "h000001", "the target commit hash is snapshotted");
            }
            other => panic!("expected a ResetMode choice dialog, got {other:?}"),
        }
        // Move to "Hard" (index 2 in ResetMode::ALL) and confirm.
        state.apply(Msg::DialogMove(1));
        state.apply(Msg::DialogMove(1));
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::ResetTo { commit: "h000001".to_string(), mode: ResetMode::Hard })
        );
    }

    #[test]
    fn commit_menu_reset_clamps_selection_and_skips_the_working_row() {
        // Down past the last mode clamps to "Keep" (index 3), not out of range.
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::ResetHere));
        for _ in 0..9 {
            state.apply(Msg::DialogMove(1));
        }
        assert!(matches!(state.view.dialog, Some(Dialog::Choice { sel: 3, .. })), "clamped to Keep");
        // The synthetic working row opens no picker.
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::ResetHere)));
        assert!(state.view.dialog.is_none(), "no reset picker on the working row");
    }

    #[test]
    fn reset_picker_click_selects_but_does_not_fire() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::ResetHere));
        // Clicking a mode row SELECTS it but must NOT park the reset (a destructive op
        // needs the explicit [Reset]/Enter confirm, unlike the one-click Copy picker).
        assert!(state.apply(Msg::DialogPickRow(2)));
        assert!(matches!(state.view.dialog, Some(Dialog::Choice { sel: 2, .. })), "Hard selected");
        assert!(state.view.queued_git().is_none(), "a click only selects; it does not reset");
    }

    #[test]
    fn commit_menu_undo_is_enabled_only_on_the_head_commit() {
        // On HEAD -> a confirm parking UndoCommit.
        let mut state = state_with(3);
        state.repo.commits[0].head = true;
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::UndoCommit)));
        assert!(matches!(
            state.view.dialog,
            Some(Dialog::Confirm { action: GitAction::UndoCommit, .. })
        ));
        // Off the tip -> a hint, no dialog.
        let mut state = state_with(3);
        state.repo.commits[0].head = true; // HEAD is row 0, but we open on row 2
        state.apply(Msg::OpenCommitMenu { index: 2, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::UndoCommit)));
        assert!(state.view.dialog.is_none(), "no undo confirm off the tip");
        assert!(matches!(state.status, Status::Notice(_)), "it hints instead");
    }

    /// Wire the fixture's first-parent chain 0 -> 1 -> 2 -> ... and mark row 0 HEAD, so the
    /// rebase walk from HEAD can reach an older picked commit.
    fn linear_head(state: &mut AppState) {
        state.repo.commits[0].head = true;
        let n = state.repo.commits.len();
        for i in 0..n - 1 {
            let next = state.repo.commits[i + 1].hash.clone();
            state.repo.commits[i].parents = vec![next];
        }
    }

    #[test]
    fn commit_menu_interactive_rebase_builds_steps_from_picked_to_head() {
        use crate::view_state::RebaseAction;
        let mut state = state_with(3);
        linear_head(&mut state);
        // Pick the middle commit (h000001): range = h000001..HEAD = {h000000, h000001}.
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::InteractiveRebase)));
        match &state.view.dialog {
            Some(Dialog::Rebase { steps, base, sel, note }) => {
                assert_eq!(steps.len(), 2, "HEAD down to (and incl.) the picked commit");
                assert_eq!(steps[0].short, "h000000", "newest first");
                assert_eq!(steps[1].short, "h000001", "picked commit is last (oldest)");
                assert_eq!(base, "h000001", "rebase base = the picked commit");
                assert_eq!(*sel, 0);
                assert!(note.is_none(), "no remote -> no published-history warning");
                assert!(steps.iter().all(|s| s.action == RebaseAction::Pick), "all default to pick");
            }
            other => panic!("expected a Rebase dialog, got {other:?}"),
        }
    }

    #[test]
    fn commit_menu_rebase_cycle_then_confirm_parks_the_op_set() {
        use crate::view_state::RebaseAction;
        let mut state = state_with(3);
        linear_head(&mut state);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::InteractiveRebase));
        // Click row 0 (h000000) three times to cycle Pick -> Squash -> Fixup -> Drop.
        state.apply(Msg::DialogPickRow(0));
        state.apply(Msg::DialogPickRow(0));
        assert!(state.apply(Msg::DialogPickRow(0)));
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::RebaseTodo {
                base: "h000001".to_string(),
                // FULL hash (== short in the fixture); a pick row is omitted from the ops.
                ops: vec![("h000000".to_string(), RebaseAction::Drop)],
            })
        );
    }

    #[test]
    fn commit_menu_rebase_squash_parks_a_squash_op() {
        use crate::view_state::RebaseAction;
        let mut state = state_with(3);
        linear_head(&mut state);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::InteractiveRebase));
        // Squash row 0 (newest, h000000) into the older kept commit (h000001).
        assert!(state.apply(Msg::DialogSetRow(RebaseAction::Squash)));
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(
            state.view.queued_git().cloned(),
            Some(GitAction::RebaseTodo {
                base: "h000001".to_string(),
                ops: vec![("h000000".to_string(), RebaseAction::Squash)],
            })
        );
    }

    #[test]
    fn commit_menu_rebase_refuses_squash_on_the_oldest_kept_commit() {
        use crate::view_state::RebaseAction;
        let mut state = state_with(3);
        linear_head(&mut state);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::InteractiveRebase));
        // Squash the OLDEST row (the picked base, last in display) - nothing older to meld into.
        state.apply(Msg::DialogMove(1)); // focus row 1 (oldest)
        state.apply(Msg::DialogSetRow(RebaseAction::Squash));
        assert!(state.apply(Msg::DialogConfirm));
        assert!(state.view.queued_git().is_none(), "an invalid squash parks nothing");
        assert!(matches!(state.status, Status::Notice(_)), "it hints why instead");
    }

    #[test]
    fn commit_menu_rebase_keyboard_set_marks_the_focused_row() {
        use crate::view_state::RebaseAction;
        let mut state = state_with(3);
        linear_head(&mut state);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::InteractiveRebase));
        state.apply(Msg::DialogMove(1)); // focus row 1
        assert!(state.apply(Msg::DialogSetRow(RebaseAction::Drop)));
        match &state.view.dialog {
            Some(Dialog::Rebase { steps, .. }) => {
                assert_eq!(steps[1].action, RebaseAction::Drop, "the focused row was set to drop");
                assert_eq!(steps[0].action, RebaseAction::Pick, "the other row is untouched");
            }
            other => panic!("expected a Rebase dialog, got {other:?}"),
        }
    }

    #[test]
    fn commit_menu_rebase_confirm_with_no_ops_hints_and_parks_nothing() {
        let mut state = state_with(3);
        linear_head(&mut state);
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::CommitMenuPick(CommitMenuAction::InteractiveRebase));
        assert!(state.apply(Msg::DialogConfirm));
        assert!(state.view.queued_git().is_none(), "nothing marked -> no rebase parked");
        assert!(matches!(state.status, Status::Notice(_)), "it hints instead");
    }

    #[test]
    fn commit_menu_rebase_rejects_a_commit_not_on_the_current_branch() {
        // No first-parent chain wired: the picked older commit is unreachable from HEAD.
        let mut state = state_with(3);
        state.repo.commits[0].head = true; // HEAD has no parents -> walk reaches only itself
        state.apply(Msg::OpenCommitMenu { index: 2, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::InteractiveRebase)));
        assert!(state.view.dialog.is_none(), "no dialog for an off-branch commit");
        assert!(matches!(state.status, Status::Notice(_)), "it hints instead");
    }

    #[test]
    fn commit_menu_rebase_refuses_a_range_crossing_a_merge() {
        // HEAD (row 0) is a merge (two parents); the walk must refuse the whole range.
        let mut state = state_with(3);
        linear_head(&mut state);
        state.repo.commits[0].parents = vec!["h000001".to_string(), "h000002".to_string()];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::InteractiveRebase)));
        assert!(state.view.dialog.is_none(), "no rebase dialog when the range has a merge");
        assert!(matches!(state.status, Status::Notice(_)), "it hints instead");
    }

    #[test]
    fn commit_menu_rebase_skips_the_working_row() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        assert!(state.apply(Msg::CommitMenuPick(CommitMenuAction::InteractiveRebase)));
        assert!(state.view.dialog.is_none(), "no rebase dialog on the working row");
        assert!(matches!(state.status, Status::Notice(_)), "it hints instead");
    }

    #[test]
    fn commit_menu_scroll_parks_the_offset_and_a_noop_is_dropped() {
        let mut state = state_with(3);
        state.apply(Msg::OpenCommitMenu { index: 0, col: 0, row: 0 });
        // A wheel tick parks the offset (the runtime supplies the clamped value).
        assert!(state.apply(Msg::ScrollCommitMenu { offset: 2 }));
        assert_eq!(state.view.commit_menu.as_ref().unwrap().scroll, 2);
        // Re-parking the same offset is a no-op (no redundant repaint).
        assert!(!state.apply(Msg::ScrollCommitMenu { offset: 2 }));
        // With no menu open the scroll is inert.
        state.apply(Msg::CloseCommitMenu);
        assert!(!state.apply(Msg::ScrollCommitMenu { offset: 1 }));
    }

    #[test]
    fn files_menu_scroll_parks_the_offset_and_a_noop_is_dropped() {
        let mut state = state_with(3);
        state.repo.commits[0].is_working = true;
        state.apply(Msg::SelectCommit(0));
        state.repo.tree = vec![TreeNode::File { name: "a.txt".to_string(), status: FileStatus::Modified }];
        state.view.files_flat = true;
        state.preselect_file(0);
        state.apply(Msg::OpenFilesMenu { index: 0, col: 0, row: 0 }); // root at 0; a.txt at 1
        assert!(state.view.files_menu.is_some(), "the file menu opened");
        // A wheel tick parks the offset (the runtime supplies the clamped value).
        assert!(state.apply(Msg::ScrollFilesMenu { offset: 2 }));
        assert_eq!(state.view.files_menu.as_ref().unwrap().scroll, 2);
        // Re-parking the same offset is a no-op (no redundant repaint).
        assert!(!state.apply(Msg::ScrollFilesMenu { offset: 2 }));
        // With no menu open the scroll is inert.
        state.apply(Msg::CloseFilesMenu);
        assert!(!state.apply(Msg::ScrollFilesMenu { offset: 1 }));
    }

    // -- branch/tag submenu -----------------------------------------------------

    /// Set the synthetic working row's current branch, used by the submenu's `is_current`
    /// shaping + the "into '<branch>'" labels.
    fn set_current_branch(state: &mut AppState, branch: &str) {
        state.repo.commits[0].is_working = true;
        state.repo.commits[0].working = Some(crate::model::WorkingSummary {
            branch: Some(branch.to_string()),
            added: 0,
            changed: 0,
            deleted: 0,
        });
    }

    fn branch_ref(name: &str) -> crate::model::Ref {
        crate::model::Ref { name: name.to_string(), kind: crate::model::RefKind::LocalBranch }
    }

    #[test]
    fn commit_menu_builds_branch_and_tag_submenus_from_refs() {
        let mut state = state_with(3);
        set_current_branch(&mut state, "dev");
        state.repo.commits[1].refs = vec![
            branch_ref("feature"),
            crate::model::Ref { name: "v1.0".to_string(), kind: crate::model::RefKind::Tag },
        ];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        let menu = state.view.commit_menu.as_ref().expect("menu open");
        assert_eq!(menu.refs.len(), 2, "one fly-out per ref decoration");
        assert_eq!(menu.refs[0].kind, RefMenuKind::LocalBranch);
        // A non-current local branch gets the full management set incl. Delete.
        assert!(menu.refs[0].actions.contains(&RefAction::Delete));
        assert!(menu.refs[0].actions.contains(&RefAction::Checkout));
        // The tag set is exactly Checkout / Merge / Delete.
        assert_eq!(
            menu.refs[1].actions,
            vec![RefAction::Checkout, RefAction::Merge, RefAction::Delete]
        );
    }

    #[test]
    fn commit_menu_current_branch_ref_drops_self_referential_actions() {
        let mut state = state_with(3);
        set_current_branch(&mut state, "main");
        state.repo.commits[1].refs = vec![branch_ref("main")];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        let menu = state.view.commit_menu.as_ref().unwrap();
        // The current branch can't be checked out into itself / merged into itself / deleted.
        let acts = &menu.refs[0].actions;
        assert!(!acts.contains(&RefAction::Checkout), "no self-checkout");
        assert!(!acts.contains(&RefAction::Merge), "no merge into self");
        assert!(!acts.contains(&RefAction::Delete), "no delete of the current branch");
        assert!(acts.contains(&RefAction::Rename), "rename + push stay available");
    }

    #[test]
    fn open_ref_submenu_toggles_the_flyout() {
        let mut state = state_with(3);
        set_current_branch(&mut state, "dev");
        state.repo.commits[1].refs = vec![branch_ref("feature")];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.apply(Msg::OpenRefSubmenu { ref_idx: 0 }));
        assert_eq!(state.view.commit_menu.as_ref().unwrap().open_ref, Some(0));
        // Clicking the same ref again closes its fly-out.
        assert!(state.apply(Msg::OpenRefSubmenu { ref_idx: 0 }));
        assert_eq!(state.view.commit_menu.as_ref().unwrap().open_ref, None);
    }

    #[test]
    fn ref_menu_pick_checkout_parks_a_confirm_and_closes_the_menu() {
        let mut state = state_with(3);
        set_current_branch(&mut state, "dev");
        state.repo.commits[1].refs = vec![branch_ref("feature")];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        assert!(state.apply(Msg::RefMenuPick { ref_idx: 0, action: RefAction::Checkout }));
        assert!(state.view.commit_menu.is_none(), "the menu closes on a ref pick");
        match &state.view.dialog {
            Some(Dialog::Confirm { action, prompt }) => {
                assert_eq!(*action, GitAction::CheckoutRef { name: "feature".to_string() });
                assert!(!prompt.contains("detach"), "a local branch attaches HEAD: {prompt}");
            }
            other => panic!("expected a Checkout confirm, got {other:?}"),
        }
    }

    #[test]
    fn ref_menu_pick_checkout_of_a_tag_warns_that_head_detaches() {
        let mut state = state_with(3);
        set_current_branch(&mut state, "dev");
        state.repo.commits[1].refs =
            vec![crate::model::Ref { name: "v1.0".to_string(), kind: crate::model::RefKind::Tag }];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::RefMenuPick { ref_idx: 0, action: RefAction::Checkout });
        match &state.view.dialog {
            Some(Dialog::Confirm { action, prompt }) => {
                assert_eq!(*action, GitAction::CheckoutRef { name: "v1.0".to_string() });
                assert!(prompt.contains("HEAD will detach"), "a tag detaches HEAD: {prompt}");
            }
            other => panic!("expected a Checkout confirm, got {other:?}"),
        }
    }

    #[test]
    fn ref_menu_pick_merge_parks_a_merge_into_the_current_branch() {
        let mut state = state_with(3);
        set_current_branch(&mut state, "dev");
        state.repo.commits[1].refs = vec![branch_ref("feature")];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::RefMenuPick { ref_idx: 0, action: RefAction::Merge });
        match &state.view.dialog {
            Some(Dialog::Confirm { action, prompt }) => {
                assert_eq!(*action, GitAction::MergeRef { name: "feature".to_string() });
                assert!(prompt.contains("into 'dev'"), "prompt names the current branch: {prompt}");
            }
            other => panic!("expected a Merge confirm, got {other:?}"),
        }
    }

    #[test]
    fn ref_menu_pick_tag_delete_parks_a_tag_delete() {
        let mut state = state_with(3);
        set_current_branch(&mut state, "dev");
        state.repo.commits[1].refs =
            vec![crate::model::Ref { name: "v2".to_string(), kind: crate::model::RefKind::Tag }];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::RefMenuPick { ref_idx: 0, action: RefAction::Delete });
        match &state.view.dialog {
            Some(Dialog::Confirm { action, .. }) => {
                assert_eq!(*action, GitAction::TagDelete { name: "v2".to_string() });
            }
            other => panic!("expected a TagDelete confirm, got {other:?}"),
        }
    }

    #[test]
    fn ref_menu_pick_branch_delete_parks_the_forceless_safe_delete() {
        // The safe-delete contract: a branch Delete parks BranchDelete { name } with NO
        // force field, so the loader runs `git branch -d` (refuses an unmerged branch)
        // rather than the destructive `-D`. A non-current branch is the only one offered Delete.
        let mut state = state_with(3);
        set_current_branch(&mut state, "dev");
        state.repo.commits[1].refs = vec![branch_ref("feature")];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::RefMenuPick { ref_idx: 0, action: RefAction::Delete });
        match &state.view.dialog {
            Some(Dialog::Confirm { action, prompt }) => {
                assert_eq!(*action, GitAction::BranchDelete { name: "feature".to_string() });
                assert!(prompt.contains("Delete branch 'feature'"), "names the branch: {prompt}");
            }
            other => panic!("expected a BranchDelete confirm, got {other:?}"),
        }
    }

    #[test]
    fn ref_menu_pick_rename_opens_an_input_seeded_with_the_old_name() {
        let mut state = state_with(3);
        set_current_branch(&mut state, "dev");
        state.repo.commits[1].refs = vec![branch_ref("feature")];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::RefMenuPick { ref_idx: 0, action: RefAction::Rename });
        match &state.view.dialog {
            Some(Dialog::Input { kind, field, commit, .. }) => {
                assert_eq!(*kind, InputKind::RenameBranch);
                assert_eq!(field.text(), "feature", "prefilled with the current name");
                assert_eq!(commit.as_deref(), Some("feature"), "old name snapshotted");
            }
            other => panic!("expected a Rename input, got {other:?}"),
        }
    }

    #[test]
    fn ref_menu_pick_pull_rebase_splits_the_remote_ref() {
        let mut state = state_with(3);
        set_current_branch(&mut state, "main");
        state.repo.commits[1].refs = vec![crate::model::Ref {
            name: "origin/main".to_string(),
            kind: crate::model::RefKind::RemoteBranch,
        }];
        state.apply(Msg::OpenCommitMenu { index: 1, col: 0, row: 0 });
        state.apply(Msg::RefMenuPick { ref_idx: 0, action: RefAction::PullRebase });
        match &state.view.dialog {
            Some(Dialog::Confirm { action, .. }) => {
                assert_eq!(
                    *action,
                    GitAction::PullRef {
                        remote: "origin".to_string(),
                        branch: "main".to_string(),
                        rebase: true,
                    }
                );
            }
            other => panic!("expected a PullRef confirm, got {other:?}"),
        }
    }

    /// A two-line diff: a Context line then a Removed line in hunk 7 (so the hunk
    /// index threads through the revert request).
    fn diff_with_change() -> FileView {
        FileView::Diff(FileDiff {
            path: "x.go".to_string(),
            old_rev: "a".to_string(),
            new_rev: "b".to_string(),
            lines: vec![
                DiffLine { old_no: Some(1), new_no: Some(1), kind: LineKind::Context, tokens: vec![], inline_hl: None, hunk: 0, fold: None },
                DiffLine { old_no: Some(2), new_no: None, kind: LineKind::Removed, tokens: vec![], inline_hl: None, hunk: 7, fold: None },
            ],
        })
    }

    #[test]
    fn toggle_focus_cycles_through_diff_only_when_shown() {
        let mut state = state_with(3);
        state.view.show_diff = true;
        assert_eq!(state.view.focus, Pane::Log);
        state.apply(Msg::ToggleFocus);
        assert_eq!(state.view.focus, Pane::Files);
        state.apply(Msg::ToggleFocus);
        assert_eq!(state.view.focus, Pane::Diff, "diff joins the cycle when shown");
        state.apply(Msg::ToggleFocus);
        assert_eq!(state.view.focus, Pane::Log);

        // With the diff hidden, the cycle skips it (two-pane).
        state.view.show_diff = false;
        state.view.focus = Pane::Files;
        state.apply(Msg::ToggleFocus);
        assert_eq!(state.view.focus, Pane::Log, "hidden diff is skipped");
    }

    #[test]
    fn hiding_diff_drops_diff_focus_to_log() {
        let mut state = state_with(3);
        state.view.show_diff = true;
        state.view.focus = Pane::Diff;
        state.apply(Msg::ToggleDiff); // hide
        assert!(!state.view.show_diff);
        assert_eq!(state.view.focus, Pane::Log, "focus falls back off the hidden pane");
    }

    #[test]
    fn hunk_revert_arms_then_parks_for_changed_line() {
        let mut state = state_with(3);
        state.set_preview(Some(diff_with_change()));
        state.view.focus = Pane::Diff;
        state.view.diff_cursor = 1; // the Removed line (hunk 7)

        // First Enter only arms (warns), parks nothing.
        assert!(state.apply(Msg::RevertHunk));
        assert!(state.view.hunk_revert_armed);
        assert!(state.view.queued_hunk_revert().is_none());

        // Second Enter confirms: parks (commit, path, hunk) and disarms.
        assert!(state.apply(Msg::RevertHunk));
        assert!(!state.view.hunk_revert_armed);
        let (_, path, hunk) = state.view.queued_hunk_revert().expect("parked");
        assert_eq!(path, "x.go");
        assert_eq!(hunk, 7, "the focused line's hunk index is targeted");
    }

    #[test]
    fn hunk_reverted_forces_a_preview_reload() {
        // The disk write changed the working file but the selection did not move, so the
        // completion must park a preview reload (the runtime re-opens the file). Without
        // it the diff would keep showing the pre-revert content.
        let mut state = state_with(3);
        assert!(!state.view.effects.contains(&Effect::ReloadPreview));
        assert!(state.apply(Msg::HunkReverted { summary: "Reverted hunk".to_string() }));
        assert!(
            state.view.effects.contains(&Effect::ReloadPreview),
            "a completed hunk revert parks a preview reload"
        );
    }

    #[test]
    fn hunk_revert_on_context_line_hints_and_disarms() {
        let mut state = state_with(3);
        state.set_preview(Some(diff_with_change()));
        state.view.focus = Pane::Diff;
        state.view.diff_cursor = 0; // the Context line
        assert!(state.apply(Msg::RevertHunk));
        assert!(!state.view.hunk_revert_armed, "a context line never arms");
        assert!(state.view.queued_hunk_revert().is_none());
    }

    #[test]
    fn hunk_revert_ignored_off_the_diff_pane() {
        let mut state = state_with(3);
        state.set_preview(Some(diff_with_change()));
        state.view.focus = Pane::Files; // not the diff pane
        assert!(!state.apply(Msg::RevertHunk), "no-op when the diff pane is unfocused");
    }

    #[test]
    fn moving_diff_cursor_disarms_pending_revert() {
        let mut state = state_with(3);
        state.set_preview(Some(diff_with_change()));
        state.view.focus = Pane::Diff;
        state.view.diff_cursor = 1;
        state.apply(Msg::RevertHunk); // arm
        assert!(state.view.hunk_revert_armed);
        state.apply(Msg::Move(-1)); // move up disarms
        assert!(!state.view.hunk_revert_armed);
        assert_eq!(state.view.diff_cursor, 0);
    }

    #[test]
    fn diff_cursor_skips_fold_markers_when_moving() {
        // A change, a synthetic fold marker (git's omitted middle), another change. The
        // browse cursor must STEP OVER the marker - it is not a real line.
        let mut state = state_with(3);
        let lines = vec![
            DiffLine { old_no: None, new_no: Some(1), kind: LineKind::Added, tokens: vec![], inline_hl: None, hunk: 0, fold: None },
            crate::diff::DiffLine::fold_marker(5),
            DiffLine { old_no: None, new_no: Some(7), kind: LineKind::Added, tokens: vec![], inline_hl: None, hunk: 1, fold: None },
        ];
        state.set_preview(Some(FileView::Diff(FileDiff {
            path: "x.go".to_string(), old_rev: "a".to_string(), new_rev: "b".to_string(), lines,
        })));
        state.view.focus = Pane::Diff;
        state.view.diff_cursor = 0;
        state.apply(Msg::Move(1));
        assert_eq!(state.view.diff_cursor, 2, "Down skips the marker (idx 1) to the next change");
        state.apply(Msg::Move(-1));
        assert_eq!(state.view.diff_cursor, 0, "Up skips the marker back to the first change");
    }

    #[test]
    fn opening_a_diff_with_a_leading_fold_marker_nudges_the_cursor_off_it() {
        // First hunk below line 1 -> the diff opens with a leading marker at index 0. The
        // browse cursor must not rest on it: apply_preview normalizes it onto the first
        // real line, and Up from there cannot fall back onto the marker.
        let mut state = state_with(3);
        let (commit, path) = select_a_file(&mut state);
        let lines = vec![
            crate::diff::DiffLine::fold_marker(2),
            DiffLine { old_no: Some(3), new_no: Some(3), kind: LineKind::Context, tokens: vec![], inline_hl: None, hunk: 0, fold: None },
            DiffLine { old_no: None, new_no: Some(4), kind: LineKind::Added, tokens: vec![], inline_hl: None, hunk: 0, fold: None },
        ];
        state.view.diff_cursor = 0;
        state.apply(Msg::PreviewLoaded {
            commit,
            path,
            view: Some(FileView::Diff(FileDiff { path: "x.go".to_string(), old_rev: "a".to_string(), new_rev: "b".to_string(), lines })),
        });
        assert_eq!(state.view.diff_cursor, 1, "the leading marker (idx 0) is skipped to the first real line");
        state.view.focus = Pane::Diff;
        state.apply(Msg::Move(-1));
        assert_eq!(state.view.diff_cursor, 1, "Up at the top stays on the real line, never the marker");
    }

    /// Select the first file row and return its (commit, path) so an `EditFileLoaded`
    /// passes the staleness guard.
    fn select_a_file(state: &mut AppState) -> (String, String) {
        // The editable-buffer tests open EditFileLoaded on this commit; only the
        // synthetic working row is editable (the check_invariants rule), so flag it
        // the way the real <current> row arrives.
        state.repo.commits[state.view.log_sel].is_working = true;
        state.view.focus = Pane::Files;
        // The first row carrying a path (a file, not a directory).
        state.view.files_sel = state
            .visible_files()
            .iter()
            .position(|(_, p)| p.is_some())
            .expect("a file row exists");
        let path = state.selected_file_path().expect("a file is selected");
        let commit = state.selected_commit_hash().expect("a commit is selected");
        (commit, path)
    }

    #[test]
    fn edit_file_loaded_sets_up_editable_diff() {
        let mut state = state_with(3);
        let (commit, path) = select_a_file(&mut state);

        assert!(state.apply(Msg::EditFileLoaded {
            commit: commit.clone(),
            path: path.clone(),
            base: Some("x\n".to_string()),
            work: "x\ny\n".to_string(),
        }));
        let editor = state.view.editor.as_ref().expect("editable buffer");
        assert!(editor.loaded);
        assert_eq!(editor.lines, vec!["x", "y"]);
        assert_eq!(editor.base, vec!["x"], "committed base loaded");
        // The preview is the live diff (y is the Added line).
        match state.repo.preview.as_ref() {
            Some(FileView::Diff(d)) => {
                assert!(d.lines.iter().any(|l| l.kind == LineKind::Added && l.new_no == Some(2)));
            }
            _ => panic!("editing builds a live diff preview"),
        }

        // A load for a DIFFERENT commit is stale -> ignored.
        assert!(!state.apply(Msg::EditFileLoaded {
            commit: "deadbeef".to_string(),
            path: path.clone(),
            base: None,
            work: "z\n".to_string(),
        }));
        assert_eq!(state.view.editor.as_ref().unwrap().lines, vec!["x", "y"]);
    }

    #[test]
    fn wheel_free_scroll_overrides_park_then_clear_on_navigation_and_edit() {
        let mut state = state_with(5);

        // A log wheel tick parks the offset WITHOUT moving the selection.
        let log_sel = state.view.log_sel;
        assert!(state.apply(Msg::ScrollLog { offset: 3 }));
        assert_eq!(state.view.log_scroll, Some(3));
        assert_eq!(state.view.log_sel, log_sel, "the wheel did not move the selection");
        // Re-parking the same offset is a no-op (no redraw).
        assert!(!state.apply(Msg::ScrollLog { offset: 3 }));
        // A MOUSE click selects a visible row but KEEPS the parked viewport - the clicked
        // row is on screen by construction, so the view must not jump.
        assert!(state.apply(Msg::SelectCommit(2)));
        assert_eq!(state.view.log_sel, 2);
        assert_eq!(state.view.log_scroll, Some(3), "a click keeps the parked free-scroll");
        // A KEYBOARD move refollows the log: clears the override AND forces a redraw.
        assert!(state.apply(Msg::Move(-1)));
        assert_eq!(state.view.log_scroll, None, "keyboard nav drops the free-scroll");

        // Same for the files list: a programmatic SelectFile refollows (clears it).
        // The one-file fixture clamps the parked offset to the list end (1).
        assert!(state.apply(Msg::ScrollFiles { offset: 2 }));
        assert_eq!(state.view.files_scroll, Some(1), "the override clamps to the list");
        assert!(state.apply(Msg::SelectFile(0))); // root at 0; the file is the next row
        assert_eq!(state.view.files_scroll, None, "selecting a file refollows the list");

        // The editable diff: a wheel tick parks `edit_scroll`; an edit refollows the caret.
        let (commit, path) = select_a_file(&mut state);
        state.apply(Msg::EditFileLoaded {
            commit,
            path,
            base: Some("x\n".to_string()),
            work: "x\ny\n".to_string(),
        });
        assert!(state.apply(Msg::ScrollEdit { top: 1 }));
        assert_eq!(state.view.edit_scroll, Some(1));
        // A horizontal wheel parks `diff_hscroll`; an edit drops it too (caret refollows).
        assert!(state.apply(Msg::ScrollDiffH { offset: 4 }));
        assert_eq!(state.view.diff_hscroll, Some(4));
        // Typing drops BOTH overrides (the caret snaps back) AND forces a redraw.
        assert!(state.apply(Msg::Edit(crate::message::EditOp::Insert('Z'))));
        assert_eq!(state.view.edit_scroll, None);
        assert_eq!(state.view.diff_hscroll, None);
    }

    #[test]
    fn mouse_click_keeps_parked_scroll_in_files_and_diff() {
        let mut state = state_with(3);

        // Files: a parked wheel offset survives a mouse ClickFile on a (visible) file row
        // - no jump - but a keyboard move clears it.
        state.repo.tree = vec![
            TreeNode::File { name: "a.go".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "b.go".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "c.go".to_string(), status: FileStatus::Modified },
        ];
        state.view.focus = Pane::Files;
        state.view.files_scroll = Some(1);
        assert!(state.apply(Msg::ClickFile(2))); // c.go is row 2
        assert_eq!(state.view.files_sel, 2, "the click selected the row");
        assert_eq!(state.view.files_scroll, Some(1), "a files click keeps the parked scroll");
        assert!(state.apply(Msg::Move(-1)));
        assert_eq!(state.view.files_scroll, None, "keyboard nav drops the files free-scroll");

        // Diff: a mouse Place keeps the parked edit_scroll (clicked a visible cell); an
        // edit clears it.
        let (commit, path) = select_a_file(&mut state);
        state.apply(Msg::EditFileLoaded {
            commit,
            path,
            base: Some("a\nb\nc\n".to_string()),
            work: "a\nb\nc\n".to_string(),
        });
        assert!(state.apply(Msg::ScrollEdit { top: 1 }));
        assert_eq!(state.view.edit_scroll, Some(1));
        assert!(state.apply(Msg::Edit(crate::message::EditOp::Place { row: 1, col: 0, select: false })));
        assert_eq!(state.view.edit_scroll, Some(1), "a diff click keeps the parked scroll");
    }

    #[test]
    fn search_history_records_dedups_clears_and_picks() {
        let mut state = state_with(3);
        let typed = |state: &mut AppState, q: &str| {
            state.apply(Msg::SearchClear); // start a fresh query (a committed one persists)
            state.apply(Msg::SearchFocus);
            for ch in q.chars() {
                state.apply(Msg::SearchPush(ch));
            }
            state.apply(Msg::SearchBlur { clear: false }); // Enter commits + records
        };
        typed(&mut state, "alpha");
        typed(&mut state, "beta");
        assert_eq!(
            state.view.search_history,
            vec!["beta".to_string(), "alpha".to_string()],
            "most-recent first"
        );
        // Re-running alpha moves it to the front (de-dup, not a duplicate).
        typed(&mut state, "alpha");
        assert_eq!(
            state.view.search_history,
            vec!["alpha".to_string(), "beta".to_string()]
        );
        // The lens popup opens only with history; a pick runs the entry + closes it.
        assert!(state.apply(Msg::ToggleSearchHistory));
        assert!(state.view.search_history_open);
        assert!(state.apply(Msg::PickSearchHistory(1)));
        assert_eq!(state.view.search, "beta");
        assert!(!state.view.search_history_open, "a pick closes the popup");
        // The clear icon empties the live query and re-filters.
        assert!(state.apply(Msg::SearchClear));
        assert!(state.view.search.is_empty());
        assert!(!state.apply(Msg::SearchClear), "clearing an empty query is a no-op");
    }

    #[test]
    fn search_history_popup_needs_history_and_is_mutually_exclusive() {
        let mut state = state_with(2);
        assert!(!state.apply(Msg::ToggleSearchHistory), "no history -> no popup, no redraw");
        assert!(!state.view.search_history_open);
        // With history, opening the popup closes a filter dropdown (mutual exclusion).
        state.view.search_history = vec!["q".to_string()];
        state.apply(Msg::OpenDropdown(FilterKind::Branch));
        assert!(state.apply(Msg::ToggleSearchHistory));
        assert!(state.view.search_history_open);
        assert!(state.view.open_dropdown.is_none(), "opening the popup closes the dropdown");
        // Focusing the search field closes the popup.
        state.apply(Msg::SearchFocus);
        assert!(!state.view.search_history_open, "focusing the field closes the popup");
    }

    #[test]
    fn files_free_scroll_clears_on_flat_toggle_and_dir_expand() {
        // A dir tree so both Flat and expand/collapse change the row count.
        let mut state = state_with(1);
        state.repo.tree = vec![
            TreeNode::File { name: "1top.go".to_string(), status: FileStatus::Modified },
            TreeNode::Dir {
                name: "src".to_string(),
                file_count: 2,
                expanded: false,
                children: vec![
                    TreeNode::File { name: "a.go".to_string(), status: FileStatus::Added },
                    TreeNode::File { name: "b.go".to_string(), status: FileStatus::Added },
                ],
            },
            TreeNode::File { name: "ztop.go".to_string(), status: FileStatus::Modified },
        ];
        state.view.focus = Pane::Files;

        // Flat toggle (different row count) drops a parked wheel offset so the selection
        // re-follows into view.
        state.view.files_scroll = Some(2);
        assert!(state.apply(Msg::ToggleFlat));
        assert_eq!(state.view.files_scroll, None, "flat toggle clears the free-scroll override");

        // Expand a directory (row count grows) likewise clears it.
        state.apply(Msg::ToggleFlat); // back to nested
        state.view.files_sel = 1; // the src/ dir row (root at 0, 1top.go at 1)
        state.view.files_scroll = Some(2);
        assert!(state.apply(Msg::ToggleExpand));
        assert_eq!(state.view.files_scroll, None, "dir expand clears the free-scroll override");
    }

    #[test]
    fn editing_recomputes_live_diff() {
        let mut state = state_with(3);
        let (commit, path) = select_a_file(&mut state);
        state.apply(Msg::EditFileLoaded {
            commit,
            path,
            base: Some("a\n".to_string()),
            work: "a\n".to_string(),
        });
        if let Some(FileView::Diff(d)) = state.repo.preview.as_ref() {
            assert!(d.lines.iter().all(|l| l.kind == LineKind::Context), "identical -> all context");
        } else {
            panic!("live diff");
        }
        // Type while the diff pane is focused -> the line gains an Added, live.
        state.view.focus = Pane::Diff;
        assert!(state.apply(Msg::Edit(crate::message::EditOp::Insert('Z'))));
        assert!(state.view.editor.as_ref().unwrap().dirty);
        if let Some(FileView::Diff(d)) = state.repo.preview.as_ref() {
            assert!(d.lines.iter().any(|l| l.kind == LineKind::Added), "edit re-diffs live");
        } else {
            panic!("live diff after edit");
        }
    }

    #[test]
    fn read_only_preview_clears_the_editable_buffer() {
        let mut state = state_with(3);
        let (commit, path) = select_a_file(&mut state);
        // An editable buffer is loaded...
        state.apply(Msg::EditFileLoaded {
            commit: commit.clone(),
            path: path.clone(),
            base: None,
            work: "a\n".to_string(),
        });
        assert!(state.view.editor.is_some());
        // ...then a read-only preview (binary / no working copy) arrives -> editor cleared.
        assert!(state.apply(Msg::PreviewLoaded { commit, path, view: None }));
        assert!(state.view.editor.is_none(), "a read-only file is not editable");
    }

    #[test]
    fn edit_dirties_and_save_parks_then_clears() {
        let mut state = state_with(3);
        let (commit, path) = select_a_file(&mut state);
        state.apply(Msg::EditFileLoaded {
            commit,
            path: path.clone(),
            base: Some("a\n".to_string()),
            work: "a\n".to_string(),
        });
        state.view.focus = Pane::Diff;

        assert!(state.apply(Msg::Edit(crate::message::EditOp::Insert('Z'))));
        assert!(state.view.editor.as_ref().unwrap().dirty, "an edit dirties the buffer");

        assert!(state.apply(Msg::SaveEditor));
        let parked = state.view.queued_save().expect("save parked");
        assert_eq!(parked.0, path);
        assert_eq!(parked.1, "Za\n", "saved content is the edited buffer");

        assert!(state.apply(Msg::FileSaved { path }));
        assert!(!state.view.editor.as_ref().unwrap().dirty, "save clears dirty");
    }

    #[test]
    fn diff_blur_autosaves_dirty_and_unfocuses() {
        let mut state = state_with(3);
        let (commit, path) = select_a_file(&mut state);
        state.apply(Msg::EditFileLoaded {
            commit,
            path: path.clone(),
            base: Some("a\n".to_string()),
            work: "a\n".to_string(),
        });
        state.view.focus = Pane::Diff;
        state.apply(Msg::Edit(crate::message::EditOp::Insert('Z'))); // dirty

        assert!(state.apply(Msg::DiffBlur));
        assert_eq!(state.view.focus, Pane::Files, "Esc leaves the diff pane");
        let parked = state.view.queued_save().expect("dirty blur autosaves");
        assert_eq!(parked.0, path);
        assert!(state.view.editor.is_some(), "buffer stays loaded after blur");
    }

    #[test]
    fn repo_loaded_swaps_and_clamps_selection() {
        let mut state = state_with(10);
        state.view.log_sel = 9;
        // A smaller repo arrives: the selection must clamp into the new range.
        state.apply(Msg::RepoLoaded(Box::new(repo_with(3))));
        assert_eq!(state.repo.commits.len(), 3);
        assert_eq!(state.view.log_sel, 2, "log_sel clamps to the new visible max");
        assert!(state.repo.preview.is_none(), "RepoLoaded clears stale preview");
    }

    #[test]
    fn detail_loaded_ignored_when_hash_stale() {
        let mut state = state_with(3);
        state.view.log_sel = 0; // selected commit hash is "h000000".
        let before = state.repo.detail.clone();
        let changed = state.apply(Msg::DetailLoaded {
            hash: "h000001".to_string(), // not the selected commit
            detail: detail("h000001", "other"),
        });
        assert!(!changed, "a stale DetailLoaded is ignored");
        assert_eq!(
            state.repo.detail.as_ref().map(|d| d.short_hash.clone()),
            before.as_ref().map(|d| d.short_hash.clone()),
            "detail unchanged by the stale push"
        );
    }

    #[test]
    fn preview_loaded_applied_only_for_current_selection() {
        let mut state = state_with(3);
        state.view.focus = Pane::Files;
        state.view.files_sel = 0; // file path "f.go" (root at 0); commit "h000000".
        // Matching (commit, path) -> applied.
        let ok = state.apply(Msg::PreviewLoaded {
            commit: "h000000".to_string(),
            path: "f.go".to_string(),
            view: Some(diff_preview(2)),
        });
        assert!(ok, "matching preview is applied");
        assert!(state.repo.preview.is_some());
        // Stale path -> dropped.
        let stale = state.apply(Msg::PreviewLoaded {
            commit: "h000000".to_string(),
            path: "other.go".to_string(),
            view: Some(diff_preview(5)),
        });
        assert!(!stale, "a preview for a non-selected path is dropped");
        assert_eq!(
            state.repo.preview.as_ref().map(FileView::line_count),
            Some(2),
            "the applied preview is unchanged by the stale push"
        );
    }

    #[test]
    fn tree_loaded_swaps_for_selected_commit_keeping_preview() {
        let mut state = state_with(3);
        state.view.log_sel = 0; // selected commit hash is "h000000".
        state.repo.preview = Some(diff_preview(4));
        state.view.files_sel = 4; // a stale, out-of-range selection.
        // A matching-hash tree arrives: swapped in, re-flattened, clamped. The preview
        // is KEPT - clearing it here raced the initial file-open (bug 3); the selection
        // sites (select_commit/select_file) own preview clearing on a real move.
        let new_tree = vec![
            TreeNode::File {
                name: "new_a.go".to_string(),
                status: FileStatus::Added,
            },
            TreeNode::File {
                name: "new_b.go".to_string(),
                status: FileStatus::Modified,
            },
        ];
        let changed = state.apply(Msg::TreeLoaded {
            hash: "h000000".to_string(),
            tree: new_tree,
            ignored: Default::default(),
        });
        assert!(changed, "matching-hash tree swap reports a redraw");
        assert_eq!(state.files_rows_len(), 2, "tree replaced (two new files)");
        assert_eq!(state.view.files_sel, 1, "files_sel clamped into the new tree (last of 2 files)");
        assert!(
            state.repo.preview.is_some(),
            "a same-commit tree swap keeps the loaded preview (no bug-3 wipe)"
        );

        // A stale-hash tree is ignored (the guard mirrors DetailLoaded).
        let stale = state.apply(Msg::TreeLoaded {
            hash: "h000001".to_string(),
            ignored: Default::default(),
            tree: vec![TreeNode::File {
                name: "wrong.go".to_string(),
                status: FileStatus::Deleted,
            }],
        });
        assert!(!stale, "a tree for a non-selected commit is ignored");
        assert_eq!(state.files_rows_len(), 2, "tree unchanged by the stale push (2 files)");
    }

    #[test]
    fn tree_loaded_stores_the_ignored_path_set() {
        let mut state = state_with(2);
        state.view.log_sel = 0; // selects commit "h000000".
        let ignored: std::collections::HashSet<String> =
            ["build/out.o".to_string(), "x.log".to_string()].into_iter().collect();
        state.apply(Msg::TreeLoaded {
            hash: "h000000".to_string(),
            tree: vec![TreeNode::File { name: "x.log".to_string(), status: FileStatus::Added }],
            ignored: ignored.clone(),
        });
        assert_eq!(state.repo.ignored, ignored, "the ignored set lands in repo.ignored");
        // A later changed-only tree (empty ignored set) clears it.
        state.apply(Msg::TreeLoaded {
            hash: "h000000".to_string(),
            tree: vec![TreeNode::File { name: "x.log".to_string(), status: FileStatus::Added }],
            ignored: std::collections::HashSet::new(),
        });
        assert!(state.repo.ignored.is_empty(), "an empty ignored set clears the prior one");
    }

    #[test]
    fn toggle_all_files_flips_the_mode_flag_zero_io() {
        let mut state = state_with(3);
        assert!(!state.view.show_all_files, "All mode is OFF by default");
        assert!(state.apply(Msg::ToggleAllFiles), "ToggleAllFiles redraws");
        assert!(state.view.show_all_files, "the flag flips ON");
        // ZERO-IO: apply does NOT touch the tree itself (the runtime re-requests it);
        // a second toggle flips back OFF.
        state.apply(Msg::ToggleAllFiles);
        assert!(!state.view.show_all_files, "a second toggle flips back OFF");
    }

    #[test]
    fn all_mode_tree_load_collapses_dirs() {
        // In All mode a freshly-loaded tree opens with its dirs COLLAPSED (so a big
        // tree is not a wall of files); in changed-only mode it stays as built.
        let nested = || {
            vec![TreeNode::Dir {
                name: "src".to_string(),
                file_count: 1,
                expanded: true,
                children: vec![TreeNode::File {
                    name: "a.go".to_string(),
                    status: FileStatus::Unchanged,
                }],
            }]
        };
        // OFF: the dir stays expanded (2 visible rows: dir + file).
        let mut off = state_with(1);
        off.view.log_sel = 0;
        off.apply(Msg::TreeLoaded { hash: "h000000".to_string(), tree: nested(), ignored: Default::default() });
        assert_eq!(off.files_rows_len(), 2, "changed-only keeps the tree as built (dir + file)");
        // ON: the dir is collapsed (1 visible row: just the dir, plus root).
        let mut on = state_with(1);
        on.view.log_sel = 0;
        on.view.show_all_files = true;
        on.apply(Msg::TreeLoaded { hash: "h000000".to_string(), tree: nested(), ignored: Default::default() });
        assert_eq!(on.files_rows_len(), 1, "All mode collapses dirs on load (just the dir)");
    }

    #[test]
    fn selecting_a_different_file_clears_preview_same_keeps_it() {
        let mut state = state_with(3);
        state.repo.tree = vec![
            TreeNode::File { name: "a.go".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "b.go".to_string(), status: FileStatus::Modified },
        ];
        state.view.focus = Pane::Files;
        state.view.files_sel = 1; // b.go (root at 0, a.go at 1)
        state.repo.preview = Some(diff_preview(4));

        // Re-selecting the SAME row (no file change) keeps the preview.
        state.apply(Msg::SelectFile(1));
        assert!(state.repo.preview.is_some(), "same-file reselect keeps the preview");

        // Moving to a DIFFERENT row clears it (the runtime reloads the new file).
        state.apply(Msg::SelectFile(0));
        assert!(state.repo.preview.is_none(), "a file change clears the stale preview");
    }

    #[test]
    fn scroll_diff_clamps_to_last_row_at_body_bottom() {
        // Context-only diff: every line is its own visual row in both modes. A 200-row
        // diff in an 18-row body must clamp the LAST line to the body bottom, i.e.
        // diff_scroll == 200 - 18 == 182 - NOT 199 (which would leave one line at top).
        let mut state = state_with(1);
        state.repo.preview = Some(diff_preview(200));
        state.apply(Msg::ScrollDiff { delta: 1_000_000, pane_height: 18 });
        assert_eq!(
            state.view.diff_scroll, 182,
            "scroll stops when the last content row reaches the body bottom"
        );
        // A body taller than the content -> no scroll (everything already fits).
        state.view.diff_scroll = 0;
        state.apply(Msg::ScrollDiff { delta: 1_000_000, pane_height: 500 });
        assert_eq!(state.view.diff_scroll, 0, "a body taller than the diff cannot scroll");
        // No preview -> stays at 0.
        state.repo.preview = None;
        state.apply(Msg::ScrollDiff { delta: 100, pane_height: 4 });
        assert_eq!(state.view.diff_scroll, 0);
    }

    #[test]
    fn scroll_diff_side_by_side_clamps_to_paired_visual_rows() {
        // 40 lines = 20 Removed+Added modified pairs. In side-by-side each pair
        // collapses to ONE visual row (20 rows), so an 8-row body clamps to 20-8=12.
        // In unified the 40 lines are 40 rows, clamping to 40-8=32. The clamp must
        // match the mode's actual painted row count, not the raw line count.
        let pairs = || {
            FileView::Diff(FileDiff {
                path: "f.go".to_string(),
                old_rev: "a".to_string(),
                new_rev: "b".to_string(),
                lines: (0..20)
                    .flat_map(|i| {
                        [
                            DiffLine { old_no: Some(i + 1), new_no: None, kind: LineKind::Removed, tokens: vec![], inline_hl: None, hunk: 0, fold: None },
                            DiffLine { old_no: None, new_no: Some(i + 1), kind: LineKind::Added, tokens: vec![], inline_hl: None, hunk: 0, fold: None },
                        ]
                    })
                    .collect(),
            })
        };
        let mut sbs = state_with(1);
        sbs.view.diff_mode = crate::view_state::DiffMode::SideBySide;
        sbs.repo.preview = Some(pairs());
        sbs.apply(Msg::ScrollDiff { delta: 1_000_000, pane_height: 8 });
        assert_eq!(sbs.view.diff_scroll, 12, "side-by-side clamps to paired rows - body");

        let mut uni = state_with(1);
        uni.view.diff_mode = crate::view_state::DiffMode::Unified;
        uni.repo.preview = Some(pairs());
        uni.apply(Msg::ScrollDiff { delta: 1_000_000, pane_height: 8 });
        assert_eq!(uni.view.diff_scroll, 32, "unified clamps to line count - body");
    }

    #[test]
    fn clearing_detail_sel_forces_a_redraw_even_when_the_handler_reports_none() {
        let mut state = state_with(1);
        state.view.detail_sel =
            Some(crate::view_state::DetailSel { anchor: (0, 0), cursor: (0, 1) });
        // Quit's handler returns false (no redraw), but clearing the live selection band
        // must still repaint, so apply reports a redraw and the selection is gone.
        assert!(state.apply(Msg::Quit), "clearing a live detail selection forces a redraw");
        assert!(state.view.detail_sel.is_none(), "the selection was cleared");
    }

    #[test]
    fn scroll_detail_clamps_via_content_height() {
        let mut state = state_with(1);
        state.repo.detail = Some(detail("h000000", "subject"));
        // The runtime sources the wrapped content height; a 7-row content in a 2-row
        // viewport caps scroll at content - pane_height = 5.
        state.apply(Msg::ScrollDetail {
            delta: 100,
            pane_height: 2,
            content_height: 7,
        });
        assert_eq!(state.view.detail_scroll, 5);
    }

    // -- multi-select + revert apply tests (ZERO IO, no git) ------------------

    /// A state whose selected commit's tree is three sibling files (rows 0,1,2),
    /// Files focused, so the multi-select gestures have distinct file rows.
    #[test]
    fn commit_dialog_confirm_parks_the_git_action() {
        let mut state = state_with(2);
        assert!(state.apply(Msg::OpenCommit));
        assert!(matches!(state.view.dialog, Some(Dialog::Input { kind: InputKind::Commit, .. })));
        for c in "fix bug".chars() {
            state.apply(Msg::DialogInput(c));
        }
        assert!(state.apply(Msg::DialogConfirm));
        assert!(state.view.dialog.is_none(), "confirm closes the dialog");
        assert_eq!(state.view.queued_git().cloned(), Some(GitAction::Commit("fix bug".to_string())));
    }

    #[test]
    fn empty_input_confirm_is_a_noop_notice() {
        let mut state = state_with(2);
        state.apply(Msg::OpenTag);
        state.apply(Msg::DialogInput(' ')); // whitespace only
        assert!(state.apply(Msg::DialogConfirm));
        assert!(state.view.queued_git().is_none(), "an empty name parks nothing");
        assert!(matches!(state.status, Status::Notice(_)));
    }

    #[test]
    fn push_confirm_parks_push() {
        let mut state = state_with(2);
        assert!(state.apply(Msg::RequestPush));
        assert!(matches!(state.view.dialog, Some(Dialog::Confirm { action: GitAction::Push, .. })));
        assert!(state.apply(Msg::DialogConfirm));
        assert_eq!(state.view.queued_git().cloned(), Some(GitAction::Push));
    }

    #[test]
    fn copy_picker_parks_the_selected_field_text() {
        let mut state = state_with(2); // log_sel 0 -> commit "h000000"
        assert!(state.apply(Msg::OpenCopy));
        assert!(matches!(state.view.dialog, Some(Dialog::Copy { sel: 0, .. })));
        state.apply(Msg::DialogMove(1)); // -> "Full hash"
        assert!(state.apply(Msg::DialogConfirm));
        // The test `commit` helper sets full_hash == hash.
        assert_eq!(state.view.queued_clipboard(), Some("h000000"));
        assert!(state.view.dialog.is_none());
    }

    #[test]
    fn copy_picker_row_click_selects_and_copies_in_one_gesture() {
        // A single click on a copy-picker row (DialogPickRow) selects that field AND
        // confirms (copies + closes), so a click on the field copies it - the dialog
        // mouse-click fix.
        let mut state = state_with(2);
        assert!(state.apply(Msg::OpenCopy));
        assert!(state.apply(Msg::DialogPickRow(1))); // "Full hash"
        assert_eq!(state.view.queued_clipboard(), Some("h000000"));
        assert!(state.view.dialog.is_none(), "a row click closes the dialog");
    }

    #[test]
    fn dialog_pick_row_is_a_noop_for_a_non_copy_dialog() {
        let mut state = state_with(2);
        state.apply(Msg::OpenCommit);
        assert!(!state.apply(Msg::DialogPickRow(0)), "input dialog ignores a row pick");
        assert!(matches!(state.view.dialog, Some(Dialog::Input { .. })), "the input stays open");
    }

    #[test]
    fn diff_char_selection_builds_extends_keeps_on_scroll_and_clears_on_nav() {
        let mut state = state_with(3);
        // A press anchors a character selection at (line, col); a drag extends its cursor.
        assert!(state.apply(Msg::DiffSelectStart { line: 2, col: 4 }));
        assert_eq!(
            state.view.diff_sel.map(|s| (s.anchor, s.cursor)),
            Some(((2, 4), (2, 4))),
            "the press anchors anchor == cursor at the clicked cell"
        );
        assert!(state.apply(Msg::DiffSelectTo { line: 5, col: 7 }));
        assert_eq!(state.view.diff_sel.map(|s| (s.anchor, s.cursor)), Some(((2, 4), (5, 7))));
        // A diff scroll KEEPS it (a wheel must not drop a live selection).
        state.apply(Msg::ScrollDiff { delta: 1, pane_height: 10 });
        assert!(state.view.diff_sel.is_some(), "a diff scroll keeps the selection");
        // Genuine navigation clears it (its line/col indices would go stale).
        assert!(state.apply(Msg::Move(1)));
        assert!(state.view.diff_sel.is_none(), "navigation clears the diff selection");
    }

    #[test]
    fn copy_is_blocked_on_the_working_row() {
        let mut state = state_with(2);
        state.repo.commits[0].is_working = true; // pretend row 0 is "<current>"
        assert!(state.apply(Msg::OpenCopy));
        assert!(state.view.dialog.is_none(), "no picker opens for the working row");
        assert!(matches!(state.status, Status::Notice(_)));
    }

    fn state_multi() -> AppState {
        let mut state = state_with(1);
        state.repo.tree = vec![
            TreeNode::File { name: "a.go".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "b.go".to_string(), status: FileStatus::Added },
            TreeNode::File { name: "c.go".to_string(), status: FileStatus::Deleted },
        ];
        state.view.focus = Pane::Files;
        state
    }

    #[test]
    fn toggle_mark_adds_then_removes_a_path() {
        let mut state = state_multi();
        state.apply(Msg::ToggleMark(1)); // mark b.go (a.go at 0, b.go at 1)
        assert!(state.view.files_marked.contains("b.go"), "ToggleMark adds the path");
        assert_eq!(state.view.files_sel, 1, "the cursor moves to the toggled row");
        state.apply(Msg::ToggleMark(1)); // unmark b.go
        assert!(state.view.files_marked.is_empty(), "a second ToggleMark removes it");
    }

    #[test]
    fn select_range_marks_the_inclusive_span() {
        let mut state = state_multi();
        state.view.files_sel = 0; // anchor at a.go (row 0)
        state.apply(Msg::SelectRange(2)); // -> rows 0,1,2 (a.go, b.go, c.go) marked
        let marked: Vec<&str> = state.view.files_marked.iter().map(|s| s.as_str()).collect();
        assert_eq!(marked, vec!["a.go", "b.go", "c.go"], "the inclusive range is marked");
        assert_eq!(state.view.files_sel, 2, "the cursor moves to the range target");
    }

    #[test]
    fn select_range_includes_files_under_a_collapsed_dir_in_the_span() {
        // Tree: 1top.go, src/(a.go, b.go) COLLAPSED, ztop.go. Visible rows are
        // [1top.go, src/, ztop.go]. A range from row 0 to row 2 spans the collapsed
        // src/ dir, so its hidden files must be marked too (matching ToggleMark on a
        // dir) - not silently skipped because the dir row carries no own path.
        let mut state = state_with(1);
        state.repo.tree = vec![
            TreeNode::File { name: "1top.go".to_string(), status: FileStatus::Modified },
            TreeNode::Dir {
                name: "src".to_string(),
                file_count: 2,
                expanded: false,
                children: vec![
                    TreeNode::File { name: "a.go".to_string(), status: FileStatus::Added },
                    TreeNode::File { name: "b.go".to_string(), status: FileStatus::Added },
                ],
            },
            TreeNode::File { name: "ztop.go".to_string(), status: FileStatus::Modified },
        ];
        state.view.focus = Pane::Files;
        assert_eq!(state.files_rows_len(), 3, "3 visible rows (src/ collapsed)");
        state.view.files_sel = 0; // anchor at 1top.go (row 0)
        state.apply(Msg::SelectRange(2)); // span rows 0..=2 (covers the collapsed src/)
        let mut marked: Vec<&str> = state.view.files_marked.iter().map(|s| s.as_str()).collect();
        marked.sort_unstable();
        assert_eq!(
            marked,
            vec!["1top.go", "src/a.go", "src/b.go", "ztop.go"],
            "the collapsed dir's hidden files are included in the range"
        );
    }

    #[test]
    fn flat_lists_files_without_folders_and_preserves_the_selected_file() {
        // Tree (collapsed): [1top.go, src/, ztop.go] = 3 rows; ztop.go is row 2.
        let mut state = state_with(1);
        state.repo.tree = vec![
            TreeNode::File { name: "1top.go".to_string(), status: FileStatus::Modified },
            TreeNode::Dir {
                name: "src".to_string(),
                file_count: 2,
                expanded: false,
                children: vec![
                    TreeNode::File { name: "a.go".to_string(), status: FileStatus::Added },
                    TreeNode::File { name: "b.go".to_string(), status: FileStatus::Added },
                ],
            },
            TreeNode::File { name: "ztop.go".to_string(), status: FileStatus::Modified },
        ];
        state.view.focus = Pane::Files;
        state.view.files_sel = 2; // ztop.go in the tree view (row 2)
        assert_eq!(state.selected_file_path().as_deref(), Some("ztop.go"));

        assert!(state.apply(Msg::ToggleFlat));
        assert!(state.view.files_flat, "flat is on");
        // Flat shows every file, no folder rows: [1top.go, src/a.go, src/b.go, ztop.go].
        assert_eq!(state.files_rows_len(), 4, "all files visible, no dir rows");
        assert!(!state.row_is_dir(1), "no row is a directory in flat mode");
        // The previously-selected file is preserved by path (now at row 3).
        assert_eq!(state.view.files_sel, 3, "selection follows the file across the flip");
        assert_eq!(state.selected_file_path().as_deref(), Some("ztop.go"));

        // A mark in flat mode targets exactly that one file (full path).
        state.apply(Msg::ToggleMark(1)); // src/a.go (1top.go at 0, src/a.go at 1)
        assert!(state.view.files_marked.contains("src/a.go"), "flat mark is the single file");
        assert_eq!(state.view.files_marked.len(), 1);
    }

    #[test]
    fn autosave_off_drops_unsaved_edits_on_navigate() {
        let mut state = state_with(3);
        let (commit, path) = select_a_file(&mut state);
        state.apply(Msg::EditFileLoaded {
            commit,
            path,
            base: Some("a\n".to_string()),
            work: "a\n".to_string(),
        });
        state.view.focus = Pane::Diff;
        state.view.autosave = false; // user turned autosave OFF
        state.apply(Msg::Edit(crate::message::EditOp::Insert('Z'))); // dirty

        // Leaving the pane must NOT park a save when autosave is off.
        assert!(state.apply(Msg::DiffBlur));
        assert!(state.view.queued_save().is_none(), "autosave off => no implicit save");

        // Ctrl+S still saves explicitly regardless of the toggle.
        state.view.focus = Pane::Diff;
        assert!(state.apply(Msg::SaveEditor));
        assert!(state.view.queued_save().is_some(), "explicit save ignores the autosave toggle");
    }

    /// A files-search keystroke that moves the selection off a dirty editable buffer must
    /// autosave it first (the same autosave-on-navigate guard `select_commit`/`select_file`
    /// run), never silently discard the edits.
    #[test]
    fn files_search_autosaves_dirty_editor_before_leaving_the_file() {
        let mut state = state_with(3);
        state.repo.tree = vec![
            TreeNode::File { name: "alpha.txt".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "beta.txt".to_string(), status: FileStatus::Modified },
        ];
        state.view.files_flat = true;
        let (commit, path) = select_a_file(&mut state); // alpha.txt (row 1, root at 0)
        state.apply(Msg::EditFileLoaded {
            commit,
            path: path.clone(),
            base: Some("a\n".to_string()),
            work: "a\n".to_string(),
        });
        state.view.focus = Pane::Diff;
        assert!(state.view.autosave, "autosave is on by default");
        state.apply(Msg::Edit(crate::message::EditOp::Insert('Z'))); // dirty alpha.txt
        // Filter to beta -> the selection leaves alpha.txt; its dirty buffer must autosave.
        state.apply(Msg::FilesSearchFocus);
        for ch in "beta".chars() {
            state.apply(Msg::FilesSearchPush(ch));
        }
        assert_eq!(
            state.view.queued_save().map(|(p, _)| p),
            Some(path.as_str()),
            "the dirty alpha.txt buffer is autosaved before the files-search moves off it"
        );
        assert!(state.view.editor.is_none(), "the editor is dropped after leaving the file");
    }

    #[test]
    fn autosave_off_surfaces_a_notice_when_dropping_edits_on_navigation() {
        let mut state = state_with(3);
        let (commit, path) = select_a_file(&mut state);
        state.apply(Msg::EditFileLoaded {
            commit,
            path,
            base: Some("a\n".to_string()),
            work: "a\n".to_string(),
        });
        state.view.focus = Pane::Diff;
        state.view.autosave = false;
        state.apply(Msg::Edit(crate::message::EditOp::Insert('Z'))); // dirty

        // Navigating to another commit drops the buffer; the loss must NOT be silent.
        state.apply(Msg::SelectCommit(1));
        assert!(state.view.editor.is_none(), "navigation clears the buffer");
        assert!(state.view.queued_save().is_none(), "autosave off => nothing parked");
        match &state.status {
            Status::Notice(m) => assert!(
                m.contains("Discarded unsaved edits"),
                "the dropped edits are surfaced, got: {m}"
            ),
            other => panic!("expected a discard notice, got {other:?}"),
        }
    }

    #[test]
    fn plain_select_clears_the_marked_set() {
        let mut state = state_multi();
        state.apply(Msg::ToggleMark(0)); // a.go (root at 0)
        state.apply(Msg::ToggleMark(2)); // c.go
        assert_eq!(state.view.files_marked.len(), 2, "two marks set up");
        // A plain (single) select drops the multi-selection: the cursor IS selection.
        state.apply(Msg::SelectFile(0));
        assert!(state.view.files_marked.is_empty(), "a plain select clears the marks");
        assert_eq!(state.view.files_sel, 0);
    }

    #[test]
    fn clear_marks_empties_the_set() {
        let mut state = state_multi();
        state.apply(Msg::ToggleMark(0)); // mark a.go (root at 0)
        assert!(!state.view.files_marked.is_empty());
        assert!(state.apply(Msg::ClearMarks), "ClearMarks reports a redraw");
        assert!(state.view.files_marked.is_empty(), "ClearMarks empties the set");
        assert!(!state.apply(Msg::ClearMarks), "ClearMarks on an empty set is a no-op");
    }

    #[test]
    fn tree_loaded_keeps_surviving_marks_and_select_commit_clears_them() {
        let mut state = {
            let mut s = state_with(3);
            s.repo.tree = vec![
                TreeNode::File { name: "a.go".to_string(), status: FileStatus::Modified },
                TreeNode::File { name: "b.go".to_string(), status: FileStatus::Added },
            ];
            s.view.focus = Pane::Files;
            s
        };
        state.view.log_sel = 0; // selected commit hash "h000000".
        state.apply(Msg::ToggleMark(0)); // mark a.go (root at 0)
        assert!(!state.view.files_marked.is_empty());
        // A same-commit tree swap that STILL contains a.go keeps its mark (e.g. an
        // All-toggle / refresh must not lose the multi-selection).
        state.apply(Msg::TreeLoaded {
            hash: "h000000".to_string(),
            ignored: Default::default(),
            tree: vec![
                TreeNode::File { name: "a.go".to_string(), status: FileStatus::Modified },
                TreeNode::File { name: "c.go".to_string(), status: FileStatus::Added },
            ],
        });
        assert!(!state.view.files_marked.is_empty(), "a surviving path keeps its mark");
        // A swap that drops a.go (e.g. it was reverted) prunes its now-stale mark.
        state.apply(Msg::TreeLoaded {
            hash: "h000000".to_string(),
            ignored: Default::default(),
            tree: vec![TreeNode::File { name: "z.go".to_string(), status: FileStatus::Added }],
        });
        assert!(state.view.files_marked.is_empty(), "a gone path drops its mark");

        // Re-mark, then move to a different commit -> marks clear (owned by select).
        state.apply(Msg::ToggleMark(0)); // root at 0; the file is the next row
        assert!(!state.view.files_marked.is_empty());
        state.apply(Msg::SelectCommit(1));
        assert!(state.view.files_marked.is_empty(), "SelectCommit (move) clears the marks");
    }

    #[test]
    fn tree_loaded_keeps_marks_on_files_inside_collapsed_dirs() {
        // A mark on a file nested in a COLLAPSED directory must survive a tree refresh:
        // the prune must enumerate every file (flatten_flat), not just the visible rows
        // (which skip collapsed dirs) - else an All-toggle would wipe nested marks.
        let mut state = state_with(3);
        state.view.log_sel = 0; // selected commit hash "h000000".
        state.view.files_marked.insert("src/lib.go".to_string());
        let tree = vec![TreeNode::Dir {
            name: "src".to_string(),
            file_count: 1,
            expanded: false, // collapsed: its file is NOT in the visible flatten
            children: vec![TreeNode::File {
                name: "lib.go".to_string(),
                status: FileStatus::Modified,
            }],
        }];
        state.apply(Msg::TreeLoaded { hash: "h000000".to_string(), tree, ignored: Default::default() });
        assert!(
            state.view.files_marked.contains("src/lib.go"),
            "a mark on a file inside a collapsed dir survives the refresh"
        );
    }

    #[test]
    fn request_revert_with_marks_opens_modal_listing_all() {
        let mut state = state_multi();
        state.apply(Msg::ToggleMark(0)); // a.go (root at 0)
        state.apply(Msg::ToggleMark(2)); // c.go
        assert!(state.apply(Msg::RequestRevert), "RequestRevert redraws");
        let req = state.view.revert_confirm.clone().expect("modal opened");
        let mut paths = req.paths.clone();
        paths.sort();
        assert_eq!(paths, vec!["a.go".to_string(), "c.go".to_string()], "modal lists all marked");
        assert!(state.view.queued_revert().is_none(), "RequestRevert does NOT park IO");
    }

    #[test]
    fn request_revert_with_no_marks_targets_the_cursor_file() {
        let mut state = state_multi();
        state.apply(Msg::SelectFile(1)); // cursor on b.go (root at 0, a.go at 1), no marks
        assert!(state.view.files_marked.is_empty());
        state.apply(Msg::RequestRevert);
        let req = state.view.revert_confirm.clone().expect("modal opened");
        assert_eq!(req.paths, vec!["b.go".to_string()], "a 1-file modal over the cursor");
    }

    #[test]
    fn request_revert_with_nothing_hints_and_opens_no_modal() {
        let mut state = state_with(1);
        // The single file row is selected by default, so move the cursor off any file
        // by emptying the tree -> no marks, no cursor file.
        state.repo.tree = Vec::new();
        state.view.files_marked.clear();
        assert!(state.selected_file_path().is_none());
        state.apply(Msg::RequestRevert);
        assert!(state.view.revert_confirm.is_none(), "no modal with nothing to revert");
        assert!(matches!(state.status, Status::Notice(_)), "a hint is shown");
    }

    #[test]
    fn confirm_revert_parks_pending_and_cancel_closes() {
        let mut state = state_multi();
        state.apply(Msg::ToggleMark(0)); // mark a.go (root at 0)
        state.apply(Msg::RequestRevert);
        // Cancel closes the modal with nothing parked.
        let mut cancelled = state_multi();
        cancelled.apply(Msg::ToggleMark(0)); // mark a.go (root at 0)
        cancelled.apply(Msg::RequestRevert);
        assert!(cancelled.apply(Msg::CancelRevert));
        assert!(cancelled.view.revert_confirm.is_none() && cancelled.view.queued_revert().is_none());
        // Confirm closes the modal and parks the request for the runtime.
        assert!(state.apply(Msg::ConfirmRevert));
        assert!(state.view.revert_confirm.is_none(), "modal closed on confirm");
        assert_eq!(
            state.view.queued_revert().map(|r| r.paths.clone()),
            Some(vec!["a.go".to_string()]),
            "the confirmed request is parked"
        );
    }

    #[test]
    fn revert_not_offered_for_unchanged_rows_in_all_view() {
        // All-files view: a changed.go (Modified) + untouched.txt (Unchanged). The
        // cursor on the Unchanged row yields an EMPTY revert target (no modal, a
        // hint), and a marked set mixing both reverts ONLY the changed file.
        let mut state = state_with(1);
        state.view.show_all_files = true;
        state.repo.tree = vec![
            TreeNode::File { name: "changed.go".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "untouched.txt".to_string(), status: FileStatus::Unchanged },
        ];
        state.view.focus = Pane::Files;
        // Cursor on the Unchanged row -> nothing to revert (root at 0, changed.go at 1).
        state.apply(Msg::SelectFile(1));
        state.apply(Msg::RequestRevert);
        assert!(state.view.revert_confirm.is_none(), "no modal for an Unchanged row");
        assert!(matches!(state.status, Status::Notice(_)), "a hint is shown instead");
        // Mark BOTH rows; only the changed file survives into the revert target.
        state.apply(Msg::ToggleMark(0));
        state.apply(Msg::ToggleMark(1));
        state.apply(Msg::RequestRevert);
        let req = state.view.revert_confirm.clone().expect("modal for the changed file");
        assert_eq!(req.paths, vec!["changed.go".to_string()], "Unchanged path excluded");
    }

    #[test]
    fn revert_done_in_all_view_reloads_instead_of_pruning() {
        // In the All view the reverted file still exists on disk (Modified -> Unchanged,
        // Deleted -> restored), so the row is NOT pruned; instead a tree reload is
        // parked for the runtime, and the tree is left intact for it to swap.
        let mut state = state_with(1);
        state.view.show_all_files = true;
        state.repo.tree = vec![
            TreeNode::File { name: "changed.go".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "untouched.txt".to_string(), status: FileStatus::Unchanged },
        ];
        state.apply(Msg::RevertDone {
            paths: vec!["changed.go".to_string()],
            summary: "Reverted 1 file".to_string(),
        });
        assert!(state.view.effects.contains(&Effect::ReloadTree), "All-view revert parks a tree reload");
        assert_eq!(state.files_rows_len(), 2, "the All view does NOT prune (2 files still exist)");

        // Changed-only view still prunes (and parks no reload).
        let mut changed = state_multi();
        changed.apply(Msg::RevertDone {
            paths: vec!["a.go".to_string()],
            summary: "Reverted 1 file".to_string(),
        });
        assert!(!changed.view.effects.contains(&Effect::ReloadTree), "changed-only view parks no reload");
        assert_eq!(changed.files_rows_len(), 2, "changed-only view prunes the row (3 -> 2 files)");
    }

    #[test]
    fn revert_done_clears_marks_and_sets_status() {
        let mut state = state_multi();
        state.apply(Msg::ToggleMark(0)); // a.go (root at 0)
        state.apply(Msg::ToggleMark(1)); // b.go
        assert!(state.apply(Msg::RevertDone {
            paths: vec!["a.go".to_string(), "b.go".to_string()],
            summary: "Reverted 2 files".to_string(),
        }));
        assert!(state.view.files_marked.is_empty(), "RevertDone clears the applied marks");
        assert_eq!(state.status, Status::Notice("Reverted 2 files".to_string()));
    }

    #[test]
    fn revert_done_prunes_reverted_paths() {
        // A 3-file tree, revert one file -> the row disappears, leaving two; marks
        // clear; status is set; a redraw is reported.
        let mut state = state_multi();
        state.apply(Msg::ToggleMark(0)); // mark a.go (root at 0; cleared by the revert)
        let redraw = state.apply(Msg::RevertDone {
            paths: vec!["a.go".to_string()],
            summary: "Reverted 1 file".to_string(),
        });
        assert!(redraw, "RevertDone reports a redraw");
        // 2 files remain after the reverted a.go is pruned (no synthetic root row anymore).
        assert_eq!(state.files_rows_len(), 2, "the reverted row is pruned (3 -> 2 files)");
        let rows = TreeNode::flatten(&state.repo.tree);
        let names: Vec<String> = rows
            .iter()
            .map(|r| match &r.node {
                FlatKind::File { name, .. } | FlatKind::Dir { name, .. } => name.clone(),
            })
            .collect();
        assert_eq!(names, vec!["b.go".to_string(), "c.go".to_string()], "only a.go is gone");
        assert!(state.view.files_marked.is_empty(), "marks cleared after the prune");
        assert!(matches!(state.status, Status::Notice(_)), "status set to the summary");
    }

    #[test]
    fn revert_done_prune_all_empties_tree() {
        // Reverting every file empties the tree without panicking; selection clamps to
        // the root row (the only row left) and the (now meaningless) preview is dropped.
        let mut state = state_multi();
        state.view.files_sel = 2; // c.go (root at 0)
        state.repo.preview = Some(diff_preview(3));
        state.apply(Msg::RevertDone {
            paths: vec!["a.go".to_string(), "b.go".to_string(), "c.go".to_string()],
            summary: "Reverted 3 files".to_string(),
        });
        assert_eq!(state.files_rows_len(), 0, "the tree is pruned to empty");
        assert_eq!(state.view.files_sel, 0, "files_sel clamps to 0 on the empty tree");
        assert!(state.repo.preview.is_none(), "an empty tree drops the preview");
    }
}
