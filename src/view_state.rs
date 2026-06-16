//! Ephemeral UI state: which pane has focus and the selected row in each pane.
//!
//! This is never persisted and never sent to a backend. Selections are stored as
//! plain `usize` indices; the panels build a transient `ListState` per frame from
//! them. Keeping indices (rather than owning `ListState` here) leaves `ViewState`
//! as plain `Clone` data and avoids coupling view state to render-time scroll
//! geometry, matching the panels' existing per-frame `ListState` pattern.

use std::collections::BTreeSet;

use crate::diff::FileView;
use crate::message::{Dir, EditOp};

/// Which pane currently owns keyboard focus (drives selection highlight color).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Log,
    Files,
    /// The diff/preview viewer, focusable only when it is shown (and the editor is
    /// closed). Owns a per-line cursor (`diff_cursor`) so a changed line can be
    /// focused and its hunk reverted from the gutter.
    Diff,
}

/// How the diff body is laid out in the top viewer (user-toggled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffMode {
    SideBySide,
    Unified,
}

/// The four adjustable pane separators, named by the panes they divide. Pure
/// layout vocabulary carrying no data; ui re-exports this for hit-testing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Divider {
    /// Diff region height vs the log body (vertical-position split).
    DiffLog,
    /// Commit-log width vs the right column (horizontal-position split).
    LogRight,
    /// Files-tree height vs the commit detail (vertical-position split).
    FilesDetail,
    /// Diff old-pane width vs new-pane width (horizontal-position split).
    DiffOldNew,
}

/// Orientation of a separator bar. A `Vertical` bar moves along X (its fraction
/// comes from the cursor column); a `Horizontal` bar moves along Y (from the row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    Vertical,
    Horizontal,
}

/// Most recent search queries the lens history popup keeps (and persists).
pub const SEARCH_HISTORY_MAX: usize = 10;

/// The toolbar filter dropdowns, named by what they filter. Pure layout
/// vocabulary carrying no git data; ui re-exports this for hit-testing, the model
/// derives options + predicates per kind, and `ViewState` holds the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterKind {
    Branch,
    User,
    Date,
}

/// The filter kinds in toolbar order. Single source for rendering the labels
/// and for computing their hit-test rects, so the two never drift.
pub const FILTER_KINDS: [FilterKind; 3] = [
    FilterKind::Branch,
    FilterKind::User,
    FilterKind::Date,
];

/// Top menu-bar menus. Pure UI vocabulary; `ui` re-exports it for hit-testing, the
/// store maps a picked action to its toggle, and `ViewState` holds which is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuId {
    Editor,
    View,
    /// The global Git menu (repo-level operations), next to View.
    Git,
}

/// A menu item's effect. Each maps 1:1 to an existing view toggle; carried by
/// `Msg::MenuPick` so the store flips the matching state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Undo,
    Redo,
    Autosave,
    DiffMode,
    WordWrap,
    Whitespace,
    Revert,
    ShowDiff,
    /// Collapse unchanged lines in the displayed diff to a `N unchanged` fold marker
    /// (3-line context around each change), so the diff shows only what changed. "On"
    /// when `hide_unchanged` is set. Mainly affects the editable `<current>` diff (a
    /// commit diff already arrives folded from git).
    HideUnchanged,
    /// Show a per-line git-blame gutter beside EVERY opened file's diff (commit/author/date
    /// of the line's last change), the NEW/editable side staying editable. "On" when
    /// `show_blame` is set; persisted across runs.
    ShowBlame,

    // -- global Git menu (repo-level operations; one-shot actions, never a toggle) -----
    /// Commit the working tree (opens the message dialog).
    GitCommit,
    /// Amend HEAD's message (opens the prefilled message dialog).
    GitAmend,
    /// Tag HEAD (opens the tag-name dialog).
    GitTag,
    /// One-click Update Project: fetch + ff-only pull, tolerant of no-remote/no-upstream.
    GitUpdate,
    /// Fetch from the remote without integrating (behind a confirm).
    GitFetch,
    /// Pull the current branch from its remote (behind a confirm).
    GitPull,
    /// Push the current branch to its remote (behind a confirm).
    GitPush,
    /// Stash the working tree (behind a confirm).
    GitStash,
    /// Apply + drop the latest stash (behind a confirm).
    GitUnstash,
    /// Discard all uncommitted changes to tracked files (behind a confirm; DESTRUCTIVE).
    GitDiscard,
    /// Open the Manage Remotes dialog (list/add/edit/remove the repo's remotes).
    GitRemotes,
    /// Create a new branch rooted at HEAD (opens the name input; HEAD-targeted).
    GitNewBranch,
    /// Pick a branch/tag to check out (opens the ref picker -> checkout confirm).
    GitBranches,
    /// Pick a branch/tag to merge into the current branch (ref picker -> merge confirm).
    GitMerge,
    /// Pick a branch/tag to rebase the current branch onto (ref picker -> rebase confirm).
    GitRebase,
    /// Write the whole working tree's local changes to a `.patch` file (input dialog).
    GitCreatePatch,
    /// Apply a `.patch` file onto the working tree (input dialog -> `git apply`).
    GitApplyPatch,
}

/// The menus in bar order: id + label. Single source for rendering + hit-test.
pub const MENUS: [(MenuId, &str); 3] =
    [(MenuId::Editor, "Editor"), (MenuId::View, "View"), (MenuId::Git, "Git")];

/// One row of a top menu-bar dropdown: an action (with its label) or a thin group
/// separator. `Sep` fences intent groups apart (like the commit menu's [`CommitRow::Sep`]);
/// it occupies a row but is inert (no icon, a click closes the menu). Editor/View are flat
/// (no `Sep`); the Git menu groups its many ops by intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuRow {
    Action(MenuAction, &'static str),
    Sep,
}

/// The leading icon for a Git-menu action, or `None` for the icon-less rows. Only the
/// SOME actions with a clear glyph carry one (commit/tag, the remote-sync arrows, the
/// branch ops, discard, manage-remotes); the rest stay text so the menu is not a wall of
/// glyphs. The layout reserves the icon column for every row so labels align regardless.
pub fn menu_icon(action: MenuAction) -> Option<&'static str> {
    use crate::theme::Glyph;
    match action {
        MenuAction::GitUpdate => Some(Glyph::MENU_UPDATE),
        MenuAction::GitCommit => Some(Glyph::MENU_COMMIT),
        MenuAction::GitTag => Some(Glyph::MENU_TAG),
        MenuAction::GitPull => Some(Glyph::MENU_PULL),
        MenuAction::GitPush => Some(Glyph::MENU_PUSH),
        MenuAction::GitNewBranch => Some(Glyph::MENU_BRANCH),
        MenuAction::GitMerge => Some(Glyph::MENU_MERGE),
        MenuAction::GitDiscard => Some(Glyph::MENU_DELETE),
        MenuAction::GitRemotes => Some(Glyph::MENU_BRANCH_REMOTE),
        _ => None,
    }
}

/// The ordered ROWS of `menu`, grouped by intent with `Sep` separators. Single source the
/// layout (row-rect widths/count), the renderer (labels/icons/rules), and the store/runtime
/// (click -> action) all derive from. Editor groups the editing actions; View the diff
/// viewer's toggles - both flat. Git groups its repo-level ops: HEAD ops / remote sync /
/// branch ops / patch / working tree / remotes config.
pub fn menu_rows(menu: MenuId) -> Vec<MenuRow> {
    use MenuRow::{Action, Sep};
    match menu {
        MenuId::Editor => vec![
            Action(MenuAction::Undo, "Undo"),
            Action(MenuAction::Redo, "Redo"),
            Action(MenuAction::Autosave, "Autosave"),
            Action(MenuAction::Revert, "Revert"),
        ],
        MenuId::View => vec![
            Action(MenuAction::ShowDiff, "Show Diff"),
            Action(MenuAction::HideUnchanged, "Hide unchanged"),
            Action(MenuAction::DiffMode, "Side by side"),
            Action(MenuAction::WordWrap, "Word wrap"),
            Action(MenuAction::Whitespace, "Whitespace"),
            Action(MenuAction::ShowBlame, "Blame"),
        ],
        // Commit-targeted ops (branch/tag/reset at a chosen commit) live in the commit row's
        // context menu, where they have a target; these are the repo-level ops, grouped:
        MenuId::Git => vec![
            // record HEAD
            Action(MenuAction::GitCommit, "Commit..."),
            Action(MenuAction::GitAmend, "Amend..."),
            Action(MenuAction::GitTag, "Tag..."),
            Sep,
            // sync with the remote
            Action(MenuAction::GitUpdate, "Update Project"),
            Action(MenuAction::GitFetch, "Fetch"),
            Action(MenuAction::GitPull, "Pull..."),
            Action(MenuAction::GitPush, "Push..."),
            Sep,
            // branch ops
            Action(MenuAction::GitNewBranch, "New Branch..."),
            Action(MenuAction::GitBranches, "Branches..."),
            Action(MenuAction::GitMerge, "Merge..."),
            Action(MenuAction::GitRebase, "Rebase..."),
            Sep,
            // patches (working-tree diff <-> file)
            Action(MenuAction::GitCreatePatch, "Create Patch..."),
            Action(MenuAction::GitApplyPatch, "Apply Patch..."),
            Sep,
            // working tree
            Action(MenuAction::GitStash, "Stash Changes"),
            Action(MenuAction::GitUnstash, "Unstash Changes..."),
            Action(MenuAction::GitDiscard, "Discard All Changes..."),
            Sep,
            // remotes config
            Action(MenuAction::GitRemotes, "Manage Remotes..."),
        ],
    }
}

/// The flat `(action, label)` list of `menu` (the `Sep` rows dropped), derived from
/// [`menu_rows`]. The action-only view used where row grouping is irrelevant (tests +
/// any flat enumeration). The renderer/layout/hit-test use [`menu_rows`] so a `Sep` row
/// is positioned + inert.
pub fn menu_items(menu: MenuId) -> Vec<(MenuAction, &'static str)> {
    menu_rows(menu)
        .into_iter()
        .filter_map(|r| match r {
            MenuRow::Action(a, l) => Some((a, l)),
            MenuRow::Sep => None,
        })
        .collect()
}

/// An action in a commit row's right-click context menu. Tier 1 is read-only /
/// navigation: each maps to an existing view intent so the action logic stays in
/// one place (mirrors [`MenuAction`]). More (branch/tag/history) actions land in
/// later tiers as the backend grows commit-targeted writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitMenuAction {
    CopyRevision,
    ShowDiff,
    /// Write the selected commit as a patch file (`git format-patch -1 --stdout`). Opens
    /// an input dialog defaulting to `/tmp/<short>.patch`; no history change (read-only).
    CreatePatch,
    EditMessage,
    NewBranch,
    NewTag,
    /// Check out the selected commit (`git checkout <hash>` -> detached HEAD). Behind
    /// a confirm modal; git refuses when the switch would overwrite uncommitted changes
    /// (error -> Notice).
    Checkout,
    /// Cherry-pick the selected commit onto the current branch. Behind a confirm modal;
    /// a conflict aborts the pick (error -> Notice) so the repo never sticks mid-op.
    CherryPick,
    /// Revert the selected commit (an inverse commit on the current branch). Behind a
    /// confirm modal; a conflict aborts the revert (error -> Notice).
    RevertCommit,
    /// Reset the current branch to the selected commit. Opens the mode picker
    /// (Soft/Mixed/Hard/Keep); DESTRUCTIVE, with a pushed-branch warning.
    ResetHere,
    /// Undo the latest commit (soft reset HEAD~1). Enabled only on the tip (HEAD); a
    /// non-HEAD row hints instead.
    UndoCommit,
    /// Open the interactive-rebase dialog over `picked..HEAD` (the mark-items dialog).
    /// DESTRUCTIVE history rewrite; warns when the range is published.
    InteractiveRebase,

    // -- synthetic `<current>` working-row actions (the uncommitted changes) --------
    /// Open the commit-message dialog to commit the working tree (same as the action
    /// bar's Commit). Working-row only.
    CommitChanges,
    /// Stash the working tree (`git stash push --include-untracked`): set the changes
    /// aside, leaving a clean tree (reversible via `git stash pop`). Working-row only.
    StashChanges,
    /// Archive the selected revision to a `.zip` (`git archive`): the working tree's HEAD on
    /// `<current>`, else the picked commit. Opens a prefilled destination dialog (date suffix
    /// for `<current>`, short-hash suffix for a commit). Read-only; on EVERY commit row.
    CreateArchive,
    /// Discard all uncommitted changes to tracked files (`git reset --hard HEAD`).
    /// DESTRUCTIVE and not undoable; behind a confirm. Working-row only.
    DiscardChanges,

    // -- multi-commit selection actions (Ctrl/Shift-click marks >=2 commits) --------
    /// Cherry-pick the marked commits onto the current branch (`git cherry-pick <h...>`,
    /// oldest-first). Behind a confirm; a conflict aborts the whole pick. Multi-select only.
    CherryPickSelected,
    /// Write the marked commits as a numbered patch SERIES (`git format-patch`) into a
    /// directory (prefilled input). Read-only. Multi-select only.
    CreatePatchSeries,
}

/// One parent row of the commit context menu: an action (with its label) or a thin
/// group separator. The `Sep` rows split the menu by intent (inspect / create-ref /
/// apply-onto-current / rewrite-history) so the destructive actions read apart from the
/// safe ones. A `Sep` occupies a row but is inert (no icon, swallows a click).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitRow {
    Action(CommitMenuAction, &'static str),
    Sep,
}

/// The ordered rows of the commit context menu, grouped by intent with separators.
/// Single source shared by the layout (item widths), the renderer (labels/rules), and
/// the runtime (click -> action). Labels match WebStorm's commit-log menu wording.
///
/// `working` picks the menu for the synthetic `<current>` row: the commit-targeted actions
/// (branch/tag/checkout/cherry-pick/reset/rebase) make no sense on uncommitted changes, so
/// that row gets its OWN short menu of working-tree verbs (commit / stash / discard).
///
/// `show_diff_item` includes the leading "Show Diff" row; the caller passes `!show_diff`
/// so the row appears only when the diff viewer is HIDDEN (it would be redundant when the
/// diff is already on screen). Returns an owned Vec because the row set is now conditional.
pub fn commit_menu_items(working: bool, show_diff_item: bool) -> Vec<CommitRow> {
    use CommitMenuAction::*;
    let mut rows = Vec::new();
    // "Show Diff" leads the menu only when the diff viewer is currently HIDDEN (so the
    // item reveals it); when the diff is already shown the row is redundant and dropped.
    if show_diff_item {
        rows.push(CommitRow::Action(ShowDiff, "Show Diff"));
        // The working menu fences Show Diff off from the working-tree verbs with a rule;
        // the real-commit menu keeps Show Diff in the inspect group (no separator yet).
        if working {
            rows.push(CommitRow::Sep);
        }
    }
    if working {
        // Working-tree verbs (act on the uncommitted changes), then the destructive discard.
        rows.extend_from_slice(&[
            CommitRow::Action(CommitChanges, "Commit Changes..."),
            CommitRow::Action(StashChanges, "Stash Changes"),
            CommitRow::Action(CreateArchive, "Create archive..."),
            CommitRow::Sep,
            CommitRow::Action(DiscardChanges, "Discard All Changes..."),
        ]);
    } else {
        rows.extend_from_slice(&[
            // Inspect / export (read-only).
            CommitRow::Action(CopyRevision, "Copy Revision Number"),
            CommitRow::Action(CreatePatch, "Create Patch..."),
            CommitRow::Action(CreateArchive, "Create archive..."),
            CommitRow::Sep,
            // Create a ref here / move HEAD here.
            CommitRow::Action(NewBranch, "New Branch..."),
            CommitRow::Action(NewTag, "New Tag..."),
            CommitRow::Action(Checkout, "Checkout Revision"),
            CommitRow::Sep,
            // Undo this commit onto the current branch (revert = inverse commit, undo = drop the tip).
            CommitRow::Action(CherryPick, "Cherry-Pick"),
            CommitRow::Action(RevertCommit, "Revert Commit"),
            CommitRow::Action(UndoCommit, "Undo Commit"),
            CommitRow::Sep,
            // Rewrite history (destructive).
            CommitRow::Action(EditMessage, "Edit Commit Message..."),
            CommitRow::Action(ResetHere, "Reset Current Branch to Here..."),
            CommitRow::Action(InteractiveRebase, "Interactively Rebase from Here..."),
        ]);
    }
    rows
}

/// The MULTI-COMMIT context menu rows (a marked set of >=2 commits): the set-applicable verbs.
/// Single-commit actions (branch/tag/checkout/reset/rebase) are absent - they target one row.
pub fn commit_marked_menu_rows() -> Vec<CommitRow> {
    use CommitMenuAction::*;
    vec![
        CommitRow::Action(CherryPickSelected, "Cherry-Pick Selected"),
        CommitRow::Action(CreatePatchSeries, "Create Patch Series..."),
    ]
}

/// The locality of a ref decorating a commit, driving its branch/tag submenu shape.
/// A subset of [`crate::model::RefKind`] (Head is never a submenu target).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefMenuKind {
    LocalBranch,
    RemoteBranch,
    Tag,
}

/// One action inside a branch/tag submenu. The locality-shaped set is built by
/// [`RefMenu::new`]; the store maps a pick to a [`GitAction`] (or an input dialog), the
/// UI maps it to a label + icon. Mirrors how [`CommitMenuAction`] drives the parent menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefAction {
    /// `git checkout <ref>` (a branch attaches HEAD, a tag/remote detaches).
    Checkout,
    /// Open the New Branch dialog rooted at this ref's commit (reuses the commit's hash).
    NewBranchFrom,
    /// `git merge <ref>` into the current branch.
    Merge,
    /// `git rebase <ref>` (rebase the current branch onto the ref).
    RebaseOnto,
    /// `git push <remote> <branch>` (local branch only).
    Push,
    /// `git pull --rebase <remote> <branch>` (remote branch only).
    PullRebase,
    /// `git pull --no-rebase <remote> <branch>` (remote branch only).
    PullMerge,
    /// Open the Rename input dialog (local branch only).
    Rename,
    /// `git branch -d/-D <branch>` / `git tag -d <tag>`.
    Delete,
}

impl RefAction {
    /// The child-row label, with the ref name + current branch woven in (matching
    /// WebStorm's wording). The single label home, shared by the layout (widths), the
    /// render (text), and the store (confirm-prompt wording stays separate but parallel).
    pub fn label(self, ref_name: &str, current: &str) -> String {
        match self {
            RefAction::Checkout => "Checkout".to_string(),
            RefAction::NewBranchFrom => format!("New Branch from '{ref_name}'..."),
            RefAction::Merge => format!("Merge '{ref_name}' into '{current}'"),
            RefAction::RebaseOnto => format!("Rebase '{current}' onto '{ref_name}'"),
            RefAction::Push => "Push...".to_string(),
            RefAction::PullRebase => format!("Pull into '{current}' Using Rebase"),
            RefAction::PullMerge => format!("Pull into '{current}' Using Merge"),
            RefAction::Rename => "Rename...".to_string(),
            RefAction::Delete => "Delete".to_string(),
        }
    }
}

/// A branch/tag fly-out attached to a commit's ref decoration. Snapshotted into
/// [`CommitMenu`] when it opens (from the selected commit's `refs` + the current branch),
/// so the layout/render/hit-test all read the same locality-shaped action list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefMenu {
    /// The ref name (`main`, `origin/main`, `v1.0`).
    pub name: String,
    pub kind: RefMenuKind,
    /// The locality-shaped child actions, in display order.
    pub actions: Vec<RefAction>,
}

impl RefMenu {
    /// Build the locality-shaped action list. `is_current` (a local branch that HEAD is
    /// on) drops the self-referential ops (Checkout/Merge/RebaseOnto/Delete: git would
    /// just error on "merge main into main" / deleting the current branch).
    pub fn new(name: String, kind: RefMenuKind, is_current: bool) -> Self {
        use RefAction::*;
        let actions = match kind {
            RefMenuKind::LocalBranch if is_current => vec![NewBranchFrom, Push, Rename],
            RefMenuKind::LocalBranch => {
                vec![Checkout, NewBranchFrom, Merge, RebaseOnto, Push, Rename, Delete]
            }
            // Remote Delete (deleting the upstream ref) is network-destructive -> deferred.
            RefMenuKind::RemoteBranch => {
                vec![Checkout, NewBranchFrom, Merge, RebaseOnto, PullRebase, PullMerge]
            }
            RefMenuKind::Tag => vec![Checkout, Merge, Delete],
        };
        Self { name, kind, actions }
    }

    /// The parent row label, e.g. `Branch 'main'` / `Tag 'v1.0'`.
    pub fn label(&self) -> String {
        let kind = if self.kind == RefMenuKind::Tag { "Tag" } else { "Branch" };
        format!("{kind} '{}'", self.name)
    }
}

/// An open commit context menu: the (filtered) log row it targets plus the click
/// cell it was opened at (used to anchor the popup). A right-click SELECTS the row
/// first, so `index` always equals the current `log_sel` and the actions read
/// `selected_commit` rather than threading a hash around.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMenu {
    pub index: usize,
    pub col: u16,
    pub row: u16,
    /// First visible item when the menu is taller than the terminal: the popup caps to
    /// the screen height and the wheel windows the items so the bottom (destructive)
    /// actions stay reachable. 0 = top (the open default). The layout re-clamps it to the
    /// last full window, so a stale offset (e.g. after a resize) self-heals.
    pub scroll: usize,
    /// The branch/tag submenus available on the selected commit (one per ref decoration),
    /// appended as fly-out rows AFTER the fixed [`commit_menu_items`] leaves.
    pub refs: Vec<RefMenu>,
    /// The current branch name (HEAD's branch), used in submenu labels ("Merge 'x' into
    /// 'main'"). `None` on a detached HEAD.
    pub current_branch: Option<String>,
    /// Which ref submenu's fly-out is open (index into `refs`), or `None` for no fly-out.
    pub open_ref: Option<usize>,
    /// The target is the synthetic `<current>` working row: the menu shows the working-tree
    /// verbs (commit / stash / discard) instead of the commit-targeted actions, and carries
    /// no ref fly-outs. Picks the [`commit_menu_items`] variant everywhere (layout/render/hit).
    pub working: bool,
    /// Whether the leading "Show Diff" row is present (snapshotted as `!show_diff` at open
    /// time, so the row shows only when the diff viewer is hidden). Read by every site that
    /// calls [`commit_menu_items`] so the layout/render/hit-test agree on the row set.
    pub show_diff_item: bool,
    /// The MARKED commit set this menu acts on. NON-EMPTY makes it a MULTI-COMMIT menu (the
    /// set-verbs over these hashes), overriding the single-commit rows; empty is the normal
    /// single-commit menu keyed off `index`. The ref fly-outs are absent in multi mode.
    pub marked: Vec<String>,
}

impl CommitMenu {
    /// The menu's parent rows: the MULTI-COMMIT set-verbs when a set is marked, else the
    /// single-commit menu. Layout/render/hit-test all read this so they agree on the rows.
    pub fn parent_rows(&self) -> Vec<CommitRow> {
        if !self.marked.is_empty() {
            commit_marked_menu_rows()
        } else {
            commit_menu_items(self.working, self.show_diff_item)
        }
    }

    /// First ref-row index: the parent rows, plus ONE separator row that fences the ref
    /// fly-outs off from the rewrite-history group (only when the commit carries refs - never
    /// in multi-commit mode). The separator is inert (rule + swallowed click).
    pub fn ref_base(&self) -> usize {
        self.parent_rows().len() + usize::from(!self.refs.is_empty())
    }

    /// Total addressable rows: the parent rows, the ref separator (if any), then one row
    /// per ref submenu.
    pub fn row_count(&self) -> usize {
        self.ref_base() + self.refs.len()
    }
}

/// An action in a files-pane row's right-click context menu. The first tier reuses
/// existing intents (reveal the diff; revert the file); later tiers (patch/history/blame)
/// land as the backend grows file-targeted reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilesMenuAction {
    /// Reveal the diff viewer for the selected file (it already shows the file's diff).
    ShowDiff,
    /// Copy the file's local-changes unified diff (`git diff HEAD -- <file>`) to the system
    /// clipboard. Local-changes only (a working-row CHANGED file).
    CopyPatch,
    /// Write the file's local-changes diff to a `.patch` file on disk (prefilled dialog).
    /// Local-changes only.
    CreatePatch,
    /// Commit ONLY this file's working changes (`git commit -- <file>`), behind a message
    /// dialog. Local-changes only.
    CommitFile,
    /// Commit every working change UNDER this directory (`git add --all -- <dir>` then
    /// `git commit -- <dir>`), behind a message dialog. Folder rows only.
    CommitFolder,
    /// Open a read-only inspect overlay showing the file's HEAD revision (its committed
    /// content). Offered only for a file that exists in HEAD (`has_head_version`).
    ShowCurrentRevision,
    /// Pick one of the file's past revisions, then diff the working file against it (read-only
    /// inspect overlay). Offered for a file that exists in HEAD (`has_head_version`).
    CompareWithRevision,
    /// Pick a branch or tag, then diff the working file against that ref (read-only overlay).
    CompareWithBranch,
    /// Pick one of the file's past revisions, then show WHAT that commit changed to the file
    /// (its blob vs its parent's, read-only overlay). Offered for a file that exists in HEAD.
    ShowHistory,
    /// Annotate the file with per-line git blame (commit/author/date per line), read-only
    /// overlay. Offered for a file that exists in HEAD (`has_head_version`).
    Annotate,
    /// Discard the file's uncommitted changes (revert it to its committed state), behind a
    /// confirm modal. Working-row only (you cannot roll back a past commit's working file).
    Rollback,
    /// Delete the file from the working tree AND git (`git rm` / fs remove), behind a confirm
    /// modal. DESTRUCTIVE. Working-row only.
    DeleteFile,
    /// Commit ONLY the marked files (`git commit -- <paths>`), behind a message dialog. The
    /// marked-set analog of [`CommitFile`]/[`CommitFolder`]. Marked-set menu only.
    CommitSelected,
    /// Copy the marked files' combined local-changes patch to the clipboard. Marked-set menu only.
    CopyPatchSelected,
    /// Write the marked files' combined local-changes patch to a `.patch` file (prefilled
    /// dialog). Marked-set menu only.
    CreatePatchSelected,
    /// Discard the marked files' uncommitted changes (revert each to HEAD), behind the confirm
    /// modal - the same path as Alt+R. Marked-set menu only.
    RollbackSelected,
    /// Delete the marked files from the working tree AND git, behind a confirm. DESTRUCTIVE.
    /// Marked-set menu only.
    DeleteSelected,
}

impl FilesMenuAction {
    /// The menu row label.
    pub fn label(self) -> &'static str {
        match self {
            FilesMenuAction::ShowDiff => "Show Diff",
            FilesMenuAction::CopyPatch => "Copy as Patch",
            FilesMenuAction::CreatePatch => "Create Patch...",
            FilesMenuAction::CommitFile => "Commit File...",
            FilesMenuAction::CommitFolder => "Commit Directory...",
            FilesMenuAction::ShowCurrentRevision => "Show Current Revision",
            FilesMenuAction::CompareWithRevision => "Compare with Revision...",
            FilesMenuAction::CompareWithBranch => "Compare with Branch or Tag...",
            FilesMenuAction::ShowHistory => "Show History",
            FilesMenuAction::Annotate => "Annotate with Git Blame",
            FilesMenuAction::Rollback => "Rollback...",
            FilesMenuAction::DeleteFile => "Delete...",
            FilesMenuAction::CommitSelected => "Commit Selected...",
            FilesMenuAction::CopyPatchSelected => "Copy Selected as Patch",
            FilesMenuAction::CreatePatchSelected => "Create Patch from Selected...",
            FilesMenuAction::RollbackSelected => "Rollback Selected...",
            FilesMenuAction::DeleteSelected => "Delete Selected...",
        }
    }
}

/// One row of the files-pane context menu: an action or a thin group separator. Mirrors
/// [`CommitRow`] - a `Sep` occupies a row but is inert (a rule, swallows a click).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilesRow {
    Action(FilesMenuAction),
    Sep,
}

/// The files-pane context menu grouped by INTENT: each inner Vec is one group (inspect /
/// patch+commit / read-only revisions / destructive), some empty per the flags. The single
/// source the flat [`files_menu_items`] and the sectioned [`files_menu_rows`] both derive from.
///
/// `show_diff_item` leads with "Show Diff" only when the diff viewer is HIDDEN (mirrors the
/// commit menu). `local_changes` adds the working-tree items (copy/create patch, commit,
/// rollback, delete) only for a CHANGED file on the `<current>` row. The read-only inspect
/// group reads any committed blob, so it is offered for a changed-but-not-Added working file
/// (`has_head_version`) AND for any file on a historical commit (`committed`).
fn files_menu_groups(
    show_diff_item: bool,
    local_changes: bool,
    has_head_version: bool,
    committed: bool,
    is_dir: bool,
) -> Vec<Vec<FilesMenuAction>> {
    use FilesMenuAction::*;
    // A DIRECTORY row carries only the set-applicable verbs over its changed files (no
    // Show Diff / Compare / Show Current Revision - those need a single blob; no Delete -
    // dropping a whole subtree is too blunt). Gated at open time on a changed file under it.
    if is_dir {
        return vec![vec![CommitFolder], vec![CopyPatch, CreatePatch], vec![Rollback]];
    }
    let mut groups = Vec::new();
    if show_diff_item {
        groups.push(vec![ShowDiff]);
    }
    if local_changes {
        groups.push(vec![CopyPatch, CreatePatch, CommitFile]);
    }
    if has_head_version || committed {
        groups.push(vec![ShowCurrentRevision, CompareWithRevision, CompareWithBranch, ShowHistory, Annotate]);
    }
    if local_changes {
        groups.push(vec![Rollback, DeleteFile]);
    }
    groups
}

/// The flat ordered actions of the files-pane context menu (no separators). An empty result
/// opens no menu. The hit-test/test view of the menu.
pub fn files_menu_items(
    show_diff_item: bool,
    local_changes: bool,
    has_head_version: bool,
    committed: bool,
    is_dir: bool,
) -> Vec<FilesMenuAction> {
    files_menu_groups(show_diff_item, local_changes, has_head_version, committed, is_dir)
        .concat()
}

/// The files-pane menu as RENDERED rows: the intent groups fenced by `Sep` rules, but ONLY
/// when the menu is long (> 5 actions) - a short menu (e.g. the 4-row folder menu) stays flat
/// so a tiny popup is not chopped up. Single source shared by the layout, renderer, and
/// click hit-test (a `Sep` row is inert).
pub fn files_menu_rows(
    show_diff_item: bool,
    local_changes: bool,
    has_head_version: bool,
    committed: bool,
    is_dir: bool,
) -> Vec<FilesRow> {
    let groups: Vec<Vec<FilesMenuAction>> =
        files_menu_groups(show_diff_item, local_changes, has_head_version, committed, is_dir)
            .into_iter()
            .filter(|g| !g.is_empty())
            .collect();
    let sectioned = groups.iter().map(Vec::len).sum::<usize>() > 5;
    let mut rows = Vec::new();
    for group in groups {
        if sectioned && !rows.is_empty() {
            rows.push(FilesRow::Sep);
        }
        rows.extend(group.into_iter().map(FilesRow::Action));
    }
    rows
}

/// The MARKED-SET context menu rows: the set-applicable verbs over the marked files, fenced
/// by intent groups (mirrors the folder menu shape). Single-blob items (Show Diff / Compare /
/// Blame / History) are absent - they need one file. Destructive Rollback/Delete fenced last.
pub fn files_marked_menu_rows() -> Vec<FilesRow> {
    use FilesMenuAction::*;
    let groups = [
        vec![CommitSelected],
        vec![CopyPatchSelected, CreatePatchSelected],
        vec![RollbackSelected, DeleteSelected],
    ];
    let mut rows = Vec::new();
    for group in groups {
        if !rows.is_empty() {
            rows.push(FilesRow::Sep);
        }
        rows.extend(group.into_iter().map(FilesRow::Action));
    }
    rows
}

/// The flat marked-set actions (no separators) - the click/test view.
pub fn files_marked_menu_items() -> Vec<FilesMenuAction> {
    files_marked_menu_rows()
        .into_iter()
        .filter_map(|r| match r {
            FilesRow::Action(a) => Some(a),
            FilesRow::Sep => None,
        })
        .collect()
}

/// An open files-pane context menu: the file path it targets plus the click cell it was
/// anchored at. A right-click SELECTS the row first, so the path is the current files
/// selection; the actions read it (or the current selection) rather than threading it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilesMenu {
    /// Repo-relative path of the right-clicked file (the menu's target).
    pub path: String,
    pub col: u16,
    pub row: u16,
    /// First visible row when the menu is taller than the terminal: the popup caps to the
    /// screen height and the wheel windows the rows so the bottom (destructive) actions stay
    /// reachable. 0 = top (the open default). The layout re-clamps a stale offset (e.g. after
    /// a resize) so it never scrolls into empty space. Mirrors [`CommitMenu::scroll`].
    pub scroll: usize,
    /// Include the leading "Show Diff" row (snapshotted as `!show_diff` at open time).
    pub show_diff_item: bool,
    /// The targeted file carries uncommitted changes (a CHANGED file on the `<current>`
    /// row), enabling the local-changes items (copy/create patch, rollback). Snapshotted at
    /// open time so layout/render/hit-test agree on the rows.
    pub local_changes: bool,
    /// The targeted file exists in HEAD (a changed, non-`Added` working file), enabling the
    /// read-only "Show Current Revision" viewer. Snapshotted at open time.
    pub has_head_version: bool,
    /// The row is a file on a HISTORICAL commit (not the `<current>` working row), enabling the
    /// read-only inspect group (Show Current Revision / Compare / Show History / Annotate) with
    /// NO working-tree ops. Snapshotted at open time.
    pub committed: bool,
    /// The right-clicked row is a DIRECTORY (so `path` is a folder prefix and the menu carries
    /// the folder set-verbs). Snapshotted at open time.
    pub is_dir: bool,
    /// The marked file set this menu acts on (snapshotted at open). NON-EMPTY makes this a
    /// MARKED-SET menu (the set-verbs over these paths), overriding the single-file/folder rows;
    /// empty is the normal file/folder menu keyed off `path`.
    pub marked: Vec<String>,
}

impl FilesMenu {
    /// The flat actions (no separators) - the click/test view of the menu.
    pub fn items(&self) -> Vec<FilesMenuAction> {
        if !self.marked.is_empty() {
            return files_marked_menu_items();
        }
        files_menu_items(self.show_diff_item, self.local_changes, self.has_head_version, self.committed, self.is_dir)
    }

    /// The RENDERED rows (intent groups fenced by separators when long). Shared by the layout,
    /// renderer, and click hit-test so all three agree on which row is which.
    pub fn rows(&self) -> Vec<FilesRow> {
        if !self.marked.is_empty() {
            return files_marked_menu_rows();
        }
        files_menu_rows(self.show_diff_item, self.local_changes, self.has_head_version, self.committed, self.is_dir)
    }
}

/// A transient read-only overlay on the diff pane: a file's content at some revision (the
/// HEAD revision, or - later - a compared revision), shown read-only with its own header
/// `title`. Set by [`Msg::InspectLoaded`], closed by Esc or any navigation/edit choke. Does
/// NOT touch `RepoModel::preview` or the editable buffer (which it masks while open).
#[derive(Clone, Debug)]
pub struct InspectView {
    /// The header label (e.g. "HEAD - Esc to close"); the path comes from the `FileView`.
    pub title: String,
    pub view: FileView,
}

/// What an inspect overlay shows for its `(rev, path)`: the file's CONTENT at that revision
/// (Show Current Revision -> a read-only `Source`), or a DIFF of the working file against that
/// revision (Compare with Revision/Branch -> a read-only `Diff`). Selects the backend call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectMode {
    /// The file's blob at `rev`, read-only (`revision_source`).
    Source,
    /// The working file diffed against `rev`'s blob, read-only (`compare_view`).
    Compare,
    /// What `rev` CHANGED to the file: its blob vs its mainline parent's, read-only
    /// (`file_view`) - the per-commit diff behind Show History.
    CommitDiff,
    /// `rev`'s file annotated with per-line git blame, read-only (`blame`) - the overlay
    /// behind Annotate with Git Blame. `WORKING_REV` blames the live working tree.
    Blame,
}

/// A parked request to load an inspect overlay: read `path` at `rev` in `mode` and show it
/// under `title`. Set ZERO-IO by `apply`, drained by the runtime into a `Req::Inspect`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectReq {
    pub rev: String,
    pub path: String,
    pub title: String,
    pub mode: InspectMode,
    /// Compare-only: the NEW (right) side to diff `rev` against - `WORKING_REV` for the live
    /// working tree (the `<current>` row) or a commit hash (a historical row, comparing that
    /// commit's blob). Ignored by Source/Blame/CommitDiff (set to `WORKING_REV`).
    pub base: String,
}

/// One row of a "Compare with..." picker: a human `label` (a revision's `hash  date  subject`,
/// or a ref name) and the `rev` it resolves to (a hash or ref name passed to the backend).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickItem {
    pub label: String,
    pub rev: String,
}

/// Which list a "Compare with..." picker enumerates: a file's own revision history, or the
/// repo's branches + tags. Selects the backend list call for the picker round-trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickKind {
    FileRevisions,
    Refs,
}

/// A parked request to load a picker's option list off-thread: enumerate `kind` for `path`,
/// reply with `Msg::PickListLoaded`. `mode` is the inspect to park when a row is confirmed
/// (Compare for "Compare with...", CommitDiff for "Show History"); it rides along so the picker
/// remembers its purpose. Set ZERO-IO by `apply`, drained into a `Req::PickList`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickListReq {
    pub kind: PickKind,
    pub path: String,
    pub mode: InspectMode,
}

/// One page step (lines) for the editor's PageUp/PageDown, matching the list panes'
/// fixed `Move(10)` step so cursor paging feels the same everywhere.
pub const EDITOR_PAGE: usize = 10;

/// Maximum retained undo snapshots. Bounds memory on a long editing session; the
/// oldest entry is dropped once the stack grows past this.
const UNDO_CAP: usize = 200;

/// One reversible point in the buffer's history: the lines plus the cursor at the
/// moment the edit group began. Selection is intentionally not restored (an undo
/// lands the cursor, not a range), matching common editor behavior.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EditSnapshot {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

/// The kind of the last mutating edit, used to coalesce a run of same-kind edits
/// (a typing burst, a backspace run) into ONE undo group. A different kind, a
/// cursor move, or a load starts a fresh group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    Insert,
    Backspace,
    Delete,
}

/// The editable diff's working buffer: the right/new side the user edits in place,
/// plus the committed `base` (left side) for the live diff. `None` on `ViewState`
/// means the current file is not editable (read-only / binary / loading). Holds the
/// cursor and an optional selection anchor; the renderer derives scroll from the
/// cursor each frame, so no scroll lives here. A `(row, col)` is `(line index, char
/// index into that line)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorState {
    /// Repo-relative path of the working-tree file being edited.
    pub path: String,
    /// The committed lines (the live diff's fixed LEFT/old side). Empty for a file
    /// absent in the base commit (everything reads as Added).
    pub base: Vec<String>,
    /// The buffer, one entry per line (no trailing newline stored).
    pub lines: Vec<String>,
    /// Cursor line (index into `lines`).
    pub cursor_row: usize,
    /// Cursor column as a CHAR index into the current line (0..=line char count).
    pub cursor_col: usize,
    /// Selection anchor `(row, col)`. A selection is active when this is `Some` and
    /// differs from the cursor; a click sets it to the cursor (armed for a drag).
    pub anchor: Option<(usize, usize)>,
    /// Unsaved edits since the last load/save.
    pub dirty: bool,
    /// The file content has arrived (`false` shows a "loading" placeholder).
    pub loaded: bool,
    /// Past states for undo (most recent last). Capped at [`UNDO_CAP`].
    undo: Vec<EditSnapshot>,
    /// States undone-from, for redo (most recent last). Cleared by any fresh edit.
    redo: Vec<EditSnapshot>,
    /// Kind of the last recorded mutating edit, for run coalescing. `None` after a
    /// cursor move / load / undo so the next edit opens a new group.
    last_edit: Option<EditKind>,
}

impl EditorState {
    /// A freshly-opened, not-yet-loaded buffer on `path`.
    pub fn opening(path: String) -> Self {
        EditorState {
            path,
            base: Vec::new(),
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            anchor: None,
            dirty: false,
            loaded: false,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: None,
        }
    }

    /// Fill the buffer from `content`, resetting cursor + selection to the top and
    /// clearing the undo/redo history (a freshly loaded file has no edit history).
    pub fn load(&mut self, content: &str) {
        self.lines = Self::split_lines(content);
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.anchor = None;
        self.dirty = false;
        self.loaded = true;
        self.undo.clear();
        self.redo.clear();
        self.last_edit = None;
    }

    /// Load both diff sides: `base` (committed blob, `None` -> empty) and `work` (the
    /// editable buffer). Resets the cursor + selection and clears dirty.
    pub fn load_edit(&mut self, base: Option<&str>, work: &str) {
        self.base = base.map(Self::split_lines).unwrap_or_default();
        self.load(work);
    }

    /// Split file `content` into buffer lines, dropping the trailing-newline's empty
    /// final element (re-added on save) but never going empty (the cursor needs a line).
    fn split_lines(content: &str) -> Vec<String> {
        let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
        if lines.len() > 1 && lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }

    /// Serialize the buffer back to file text: lines joined by `\n` with a trailing
    /// newline (the POSIX text-file convention).
    pub fn to_content(&self) -> String {
        let mut s = self.lines.join("\n");
        s.push('\n');
        s
    }

    /// Char count of line `row`.
    fn row_len(&self, row: usize) -> usize {
        self.lines.get(row).map_or(0, |l| l.chars().count())
    }

    /// Char count of the cursor's line (its column bound).
    fn line_len(&self) -> usize {
        self.row_len(self.cursor_row)
    }

    /// Clamp the column into the current line (after a vertical move).
    fn clamp_col(&mut self) {
        self.cursor_col = self.cursor_col.min(self.line_len());
    }

    /// The current cursor position.
    fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    /// The ordered selection span `[(start), (end))`, or `None` when nothing is
    /// selected (no anchor, or the anchor coincides with the cursor).
    pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.anchor?;
        let c = self.cursor();
        if a == c {
            None
        } else if a <= c {
            Some((a, c))
        } else {
            Some((c, a))
        }
    }

    /// Whether position `(row, col)` lies within the active selection (for rendering).
    pub fn in_selection(&self, row: usize, col: usize) -> bool {
        match self.selection() {
            Some((s, e)) => (row, col) >= s && (row, col) < e,
            None => false,
        }
    }

    /// The selected text (lines joined by `\n`), or empty when nothing is selected.
    pub fn selected_text(&self) -> String {
        let ((sr, sc), (er, ec)) = match self.selection() {
            Some(sel) => sel,
            None => return String::new(),
        };
        if sr == er {
            return self.slice(sr, sc, ec);
        }
        let mut out = self.slice(sr, sc, self.row_len(sr));
        for row in (sr + 1)..er {
            out.push('\n');
            out.push_str(&self.lines[row]);
        }
        out.push('\n');
        out.push_str(&self.slice(er, 0, ec));
        out
    }

    /// Chars `[from, to)` of line `row` as a String.
    fn slice(&self, row: usize, from: usize, to: usize) -> String {
        self.lines[row].chars().skip(from).take(to.saturating_sub(from)).collect()
    }

    /// Delete the active selection, leaving the cursor at its start; clears the
    /// anchor. Returns whether anything was deleted.
    fn delete_selection(&mut self) -> bool {
        let ((sr, sc), (er, ec)) = match self.selection() {
            Some(sel) => sel,
            None => return false,
        };
        let head: String = self.lines[sr].chars().take(sc).collect();
        let tail: String = self.lines[er].chars().skip(ec).collect();
        let merged = format!("{head}{tail}");
        self.lines.splice(sr..=er, std::iter::once(merged));
        self.cursor_row = sr;
        self.cursor_col = sc;
        self.anchor = None;
        self.dirty = true;
        true
    }

    /// Insert multi-line `text` at the cursor (used by paste). Splits on `\n`.
    fn insert_text(&mut self, text: &str) {
        let mut parts = text.split('\n');
        let first = parts.next().unwrap_or("");
        // Split the current line at the cursor; the inserted text goes between.
        let chars: Vec<char> = self.lines[self.cursor_row].chars().collect();
        let head: String = chars[..self.cursor_col.min(chars.len())].iter().collect();
        let tail: String = chars[self.cursor_col.min(chars.len())..].iter().collect();
        let rest: Vec<&str> = parts.collect();
        if rest.is_empty() {
            // Single-line paste.
            self.lines[self.cursor_row] = format!("{head}{first}{tail}");
            self.cursor_col += first.chars().count();
        } else {
            let mut new_lines = Vec::with_capacity(rest.len() + 1);
            new_lines.push(format!("{head}{first}"));
            for mid in &rest[..rest.len() - 1] {
                new_lines.push((*mid).to_string());
            }
            let last = rest[rest.len() - 1];
            new_lines.push(format!("{last}{tail}"));
            let added = new_lines.len();
            self.lines.splice(self.cursor_row..=self.cursor_row, new_lines);
            self.cursor_row += added - 1;
            self.cursor_col = last.chars().count();
        }
        self.dirty = true;
    }

    /// Snapshot the current buffer + cursor for the undo stack.
    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            lines: self.lines.clone(),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
        }
    }

    /// Restore a snapshot into the buffer (used by undo/redo), clearing selection.
    fn restore(&mut self, s: EditSnapshot) {
        self.lines = s.lines;
        self.cursor_row = s.cursor_row;
        self.cursor_col = s.cursor_col;
        self.anchor = None;
    }

    /// Open an undo group for a mutating edit of `kind`. A run of the SAME kind
    /// coalesces (no new snapshot) so one Ctrl+Z reverts a whole typing/backspace
    /// burst; a different kind pushes a fresh pre-edit snapshot and clears redo.
    /// Call this BEFORE the buffer is mutated.
    fn begin_edit(&mut self, kind: EditKind) {
        if self.last_edit != Some(kind) {
            self.undo.push(self.snapshot());
            if self.undo.len() > UNDO_CAP {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.last_edit = Some(kind);
    }

    /// Open a one-shot (never-coalescing) undo group for a structural edit
    /// (newline, paste, cut). Always snapshots and breaks any run.
    fn begin_edit_atomic(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.last_edit = None;
    }

    /// Restore the previous buffer state, moving the current state onto the redo
    /// stack. Returns whether anything was undone.
    fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.snapshot());
        self.restore(prev);
        self.last_edit = None;
        self.dirty = true;
        true
    }

    /// Re-apply the last undone state, moving the current state back onto undo.
    /// Returns whether anything was redone.
    fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.snapshot());
        self.restore(next);
        self.last_edit = None;
        self.dirty = true;
        true
    }

    /// Whether there is an edit to undo (drives the menu item's enabled state).
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether there is an undone edit to redo.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Apply one edit op against the buffer, using `clipboard` for copy/cut/paste.
    /// Returns whether anything changed (for redraw).
    pub fn apply_op(&mut self, op: EditOp, clipboard: &mut String) -> bool {
        match op {
            EditOp::Insert(c) => {
                self.begin_edit(EditKind::Insert);
                self.delete_selection();
                self.current_chars_mut(|chars, col| chars.insert(*col, c));
                self.cursor_col += 1;
                self.anchor = None;
                self.dirty = true;
                true
            }
            EditOp::Newline => {
                self.begin_edit_atomic();
                self.delete_selection();
                let chars: Vec<char> = self.lines[self.cursor_row].chars().collect();
                let split = self.cursor_col.min(chars.len());
                let head: String = chars[..split].iter().collect();
                let tail: String = chars[split..].iter().collect();
                self.lines[self.cursor_row] = head;
                self.lines.insert(self.cursor_row + 1, tail);
                self.cursor_row += 1;
                self.cursor_col = 0;
                self.anchor = None;
                self.dirty = true;
                true
            }
            EditOp::Backspace => {
                // Nothing to delete: no selection and the cursor at the buffer origin.
                if self.selection().is_none() && self.cursor_col == 0 && self.cursor_row == 0 {
                    return false;
                }
                self.begin_edit(EditKind::Backspace);
                if self.delete_selection() {
                    return true;
                }
                if self.cursor_col > 0 {
                    self.current_chars_mut(|chars, col| {
                        chars.remove(*col - 1);
                    });
                    self.cursor_col -= 1;
                } else {
                    let line = self.lines.remove(self.cursor_row);
                    self.cursor_row -= 1;
                    self.cursor_col = self.line_len();
                    self.lines[self.cursor_row].push_str(&line);
                }
                self.dirty = true;
                true
            }
            EditOp::Delete => {
                let at_buffer_end =
                    self.cursor_col >= self.line_len() && self.cursor_row + 1 >= self.lines.len();
                // Nothing to delete: no selection and the cursor at the very end.
                if self.selection().is_none() && at_buffer_end {
                    return false;
                }
                self.begin_edit(EditKind::Delete);
                if self.delete_selection() {
                    return true;
                }
                if self.cursor_col < self.line_len() {
                    self.current_chars_mut(|chars, col| {
                        chars.remove(*col);
                    });
                } else {
                    let next = self.lines.remove(self.cursor_row + 1);
                    self.lines[self.cursor_row].push_str(&next);
                }
                self.dirty = true;
                true
            }
            EditOp::Move { dir, select } => {
                self.last_edit = None;
                if select {
                    if self.anchor.is_none() {
                        self.anchor = Some(self.cursor());
                    }
                } else {
                    self.anchor = None;
                }
                self.move_cursor(dir);
                true
            }
            EditOp::Place { row, col, select } => {
                self.last_edit = None;
                if select {
                    if self.anchor.is_none() {
                        self.anchor = Some(self.cursor());
                    }
                } else {
                    self.anchor = None;
                }
                self.cursor_row = row.min(self.lines.len().saturating_sub(1));
                self.cursor_col = col.min(self.line_len());
                if !select {
                    self.anchor = Some(self.cursor()); // armed for a drag from here
                }
                true
            }
            EditOp::SelectWord { row, col } => {
                self.last_edit = None;
                let r = row.min(self.lines.len().saturating_sub(1));
                let (start, end) = self.word_span(r, col);
                self.anchor = Some((r, start));
                self.cursor_row = r;
                self.cursor_col = end;
                true
            }
            EditOp::SelectLine { row } => {
                self.last_edit = None;
                let last = self.lines.len().saturating_sub(1);
                let r = row.min(last);
                self.anchor = Some((r, 0));
                if r < last {
                    // Include the trailing newline (start of the next line) so a delete
                    // removes the whole line, matching a triple-click in real editors.
                    self.cursor_row = r + 1;
                    self.cursor_col = 0;
                } else {
                    self.cursor_row = r;
                    self.cursor_col = self.row_len(r);
                }
                true
            }
            EditOp::Copy => {
                self.last_edit = None;
                let sel = self.selected_text();
                if sel.is_empty() {
                    return false;
                }
                *clipboard = sel;
                false // no buffer change (no redraw needed)
            }
            EditOp::Cut => {
                let sel = self.selected_text();
                if sel.is_empty() {
                    return false;
                }
                self.begin_edit_atomic();
                *clipboard = sel;
                self.delete_selection();
                true
            }
            EditOp::Paste => {
                if clipboard.is_empty() {
                    return false;
                }
                self.begin_edit_atomic();
                self.delete_selection();
                let text = clipboard.clone();
                self.insert_text(&text);
                self.anchor = None;
                true
            }
            EditOp::SelectAll => {
                self.last_edit = None;
                let last = self.lines.len().saturating_sub(1);
                self.anchor = Some((0, 0));
                self.cursor_row = last;
                self.cursor_col = self.row_len(last);
                true
            }
            EditOp::Undo => self.undo(),
            EditOp::Redo => self.redo(),
        }
    }

    /// Move the cursor one step in `dir`, clamping to the buffer.
    fn move_cursor(&mut self, dir: Dir) {
        match dir {
            Dir::Left => {
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                } else if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                    self.cursor_col = self.line_len();
                }
            }
            Dir::Right => {
                if self.cursor_col < self.line_len() {
                    self.cursor_col += 1;
                } else if self.cursor_row + 1 < self.lines.len() {
                    self.cursor_row += 1;
                    self.cursor_col = 0;
                }
            }
            Dir::Up => {
                self.cursor_row = self.cursor_row.saturating_sub(1);
                self.clamp_col();
            }
            Dir::Down => {
                if self.cursor_row + 1 < self.lines.len() {
                    self.cursor_row += 1;
                }
                self.clamp_col();
            }
            Dir::Home => self.cursor_col = 0,
            Dir::End => self.cursor_col = self.line_len(),
            Dir::PageUp => {
                self.cursor_row = self.cursor_row.saturating_sub(EDITOR_PAGE);
                self.clamp_col();
            }
            Dir::PageDown => {
                self.cursor_row = (self.cursor_row + EDITOR_PAGE).min(self.lines.len().saturating_sub(1));
                self.clamp_col();
            }
        }
    }

    /// The `[start, end)` char range of the word at `(row, col)`: the maximal run of
    /// word chars (alphanumeric or `_`) covering `col`. When `col` is not on a word
    /// char (whitespace/punctuation), selects just that single cell. Empty line -> empty.
    fn word_span(&self, row: usize, col: usize) -> (usize, usize) {
        let chars: Vec<char> = self.lines[row].chars().collect();
        let n = chars.len();
        if n == 0 {
            return (0, 0);
        }
        let c = col.min(n - 1);
        let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
        if is_word(chars[c]) {
            let mut s = c;
            while s > 0 && is_word(chars[s - 1]) {
                s -= 1;
            }
            let mut e = c + 1;
            while e < n && is_word(chars[e]) {
                e += 1;
            }
            (s, e)
        } else {
            (c, c + 1)
        }
    }

    /// Mutate the current line as a `Vec<char>` at the cursor column, then write it
    /// back. The single splice helper so char-indexed edits stay UTF-8 correct.
    fn current_chars_mut(&mut self, f: impl FnOnce(&mut Vec<char>, &usize)) {
        let mut chars: Vec<char> = self.lines[self.cursor_row].chars().collect();
        let col = self.cursor_col.min(chars.len());
        f(&mut chars, &col);
        self.lines[self.cursor_row] = chars.into_iter().collect();
    }
}

/// A pending "Revert Selected Changes" request: the selected commit + the target
/// file paths. Built by `apply` (ZERO-IO) when the user confirms the modal, it is
/// drained by the runtime into a batch `Req::Revert` for the loader to execute.
/// Plain Clone view data carrying NO git handle; the modal reads it to render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertRequest {
    /// Short hash of the commit whose change is being undone in the working tree.
    pub commit_hash: String,
    /// Human-readable commit label (its subject) for the confirmation prompt.
    pub commit_label: String,
    /// Full repository paths of the files to revert (the marked set, or the cursor).
    pub paths: Vec<String>,
}

/// A single-line editable text field: text + a caret and an optional selection
/// anchor, char-indexed (unicode-aware), no newlines. Backs the input dialog so it
/// behaves like a real field - caret movement, shift-select, clipboard - mirroring
/// the editor. Clipboard ops use the session's internal register (`ViewState::clipboard`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TextField {
    text: String,
    /// Caret as a CHAR index into `text` (0..=char count).
    caret: usize,
    /// Selection anchor (char index); a selection is active when `Some` and != caret.
    anchor: Option<usize>,
}

impl TextField {
    /// A field holding `text` with the caret at the end and no selection.
    pub fn new(text: String) -> Self {
        let caret = text.chars().count();
        Self { text, caret, anchor: None }
    }

    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn caret(&self) -> usize {
        self.caret
    }
    /// The text trimmed of surrounding whitespace (what an action consumes).
    pub fn trimmed(&self) -> String {
        self.text.trim().to_string()
    }
    fn len_chars(&self) -> usize {
        self.text.chars().count()
    }
    /// Byte offset of char index `i` (or the end for `i >= len`).
    fn byte_at(&self, i: usize) -> usize {
        self.text.char_indices().nth(i).map(|(b, _)| b).unwrap_or(self.text.len())
    }

    /// Ordered selection span `[start, end)` in char indices, if non-empty.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.caret).then(|| (a.min(self.caret), a.max(self.caret)))
    }

    /// Delete the active selection (if any), leaving the caret at its start. Returns
    /// whether anything was deleted; always clears the anchor.
    fn delete_selection(&mut self) -> bool {
        let span = self.selection();
        self.anchor = None;
        if let Some((s, e)) = span {
            self.text.replace_range(self.byte_at(s)..self.byte_at(e), "");
            self.caret = s;
            true
        } else {
            false
        }
    }

    /// Insert `c` at the caret (replacing any selection first).
    pub fn insert(&mut self, c: char) {
        self.delete_selection();
        self.text.insert(self.byte_at(self.caret), c);
        self.caret += 1;
    }

    /// Insert `s` at the caret (replacing any selection); newlines become spaces.
    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.delete_selection();
        let clean: String = s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
        self.text.insert_str(self.byte_at(self.caret), &clean);
        self.caret += clean.chars().count();
    }

    /// Backspace: delete the selection, else the char before the caret.
    pub fn backspace(&mut self) {
        if self.delete_selection() || self.caret == 0 {
            return;
        }
        let prev = self.caret - 1;
        self.text.replace_range(self.byte_at(prev)..self.byte_at(self.caret), "");
        self.caret = prev;
    }

    /// Forward-delete: delete the selection, else the char at the caret.
    pub fn delete(&mut self) {
        if self.delete_selection() || self.caret >= self.len_chars() {
            return;
        }
        self.text.replace_range(self.byte_at(self.caret)..self.byte_at(self.caret + 1), "");
    }

    /// Move the caret one step in `dir`; `select` extends the selection (sets/keeps the
    /// anchor), else collapses it. Up/Down/Page are no-ops (single line).
    pub fn move_caret(&mut self, dir: Dir, select: bool) {
        if select {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        let n = self.len_chars();
        self.caret = match dir {
            Dir::Left => self.caret.saturating_sub(1),
            Dir::Right => (self.caret + 1).min(n),
            Dir::Home => 0,
            Dir::End => n,
            _ => self.caret,
        };
    }

    /// Select the whole field.
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.len_chars();
    }

    /// The selected text, if any.
    pub fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        Some(self.text.chars().skip(s).take(e - s).collect())
    }

    /// Cut: return the selected text and remove it (or `None` with no selection).
    pub fn cut_take(&mut self) -> Option<String> {
        let t = self.selected_text()?;
        self.delete_selection();
        Some(t)
    }
}

/// A repo-level git write the loader executes off-thread (the SOLE git-IO owner).
/// Parked ZERO-IO by `apply` as an [`Effect::Git`], drained by the runtime
/// into a `Req::Git`. `Commit`/`Amend` carry the message, `Tag` the name; `Push`/`Pull`
/// take no argument (they use the system git's configured remote + credentials).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitAction {
    Commit(String),
    Amend(String),
    Tag(String),
    Push,
    Pull,
    /// Create a branch `name` at `commit`; `checkout` switches to it (`checkout -b`)
    /// vs leaving HEAD put (`branch`). Commit-targeted (from the context menu).
    BranchAt { name: String, commit: String, checkout: bool },
    /// Create a lightweight tag `name` at `commit` (commit-targeted).
    TagAt { name: String, commit: String },
    /// Reword `commit`'s message. HEAD -> message-only amend; an older commit ->
    /// rebase-reword (autostash, abort on conflict). Commit-targeted.
    RewordAt { commit: String, message: String },
    /// Check out `commit` (`git checkout <commit>`); detaches HEAD. Git refuses on a
    /// dirty tree. Commit-targeted (from the context menu, behind a confirm).
    Checkout { commit: String },
    /// Cherry-pick `commit` onto the current branch (`git cherry-pick <commit>`). A
    /// conflict aborts the pick so the repo never sticks mid-op. Commit-targeted.
    CherryPick { commit: String },
    /// Revert `commit` with an inverse commit (`git revert --no-edit <commit>`). A
    /// conflict aborts the revert. Commit-targeted.
    RevertCommit { commit: String },
    /// Reset the current branch to `commit` (`git reset --<mode> <commit>`). DESTRUCTIVE
    /// (`Hard` discards the working tree); commit-targeted, behind the mode picker.
    ResetTo { commit: String, mode: ResetMode },
    /// Undo the latest commit (`git reset --soft HEAD~1`): move HEAD back one, keeping
    /// its changes staged. HEAD-only (no payload - always the tip).
    UndoCommit,
    /// Stash the working tree (`git stash push --include-untracked`): set the uncommitted
    /// changes aside, leaving a clean tree (reversible via `git stash pop`). Working-row.
    Stash,
    /// Discard all uncommitted changes to tracked files (`git reset --hard HEAD`).
    /// DESTRUCTIVE and not undoable; behind a confirm. Working-row.
    DiscardAll,
    /// Fetch from the remote without integrating (`git fetch`). Network IO; global Git menu.
    Fetch,
    /// One-click Update Project: fetch then ff-only pull (rebase fallback on divergence),
    /// tolerating a local-only repo or upstream-less branch. Network IO; global Git menu +
    /// toolbar refresh button.
    UpdateProject,
    /// Apply + drop the latest stash (`git stash pop`). Global Git menu.
    Unstash,
    /// Write `commit` as a patch to `path` (`git format-patch -1 --stdout <commit>` into
    /// the file). Read-only on the repo (no graph/working-tree change); commit-targeted.
    CreatePatch { commit: String, path: String },
    /// Write `file`'s local-changes diff (`git diff HEAD -- <file>`) to `path`. Read-only on
    /// the repo (no graph/working-tree change); files-pane working-row item.
    CreateWorkingPatch { file: String, path: String },
    /// Archive the repository AT `rev` to `path` (`git archive --format=zip -o <path> <rev>`;
    /// `WORKING_REV` archives HEAD). Read-only on the repo (no graph/working-tree change);
    /// the commit-log context menu's "Create archive" item.
    ArchiveProject { rev: String, path: String },
    /// Commit ONLY `file`'s working changes (`git commit -m <message> -- <file>`), leaving the
    /// rest of the working tree untouched. Moves HEAD; files-pane working-row item.
    CommitFile { file: String, message: String },
    /// Commit every working change under `dir` (`git add --all -- <dir>` then `git commit -m
    /// <message> -- <dir>`). Moves HEAD; files-pane folder-row item.
    CommitFolder { dir: String, message: String },
    /// Delete `file` from the working tree and git (`git rm` if tracked, else fs remove).
    /// DESTRUCTIVE; files-pane working-row item.
    DeleteFile { file: String },
    /// Commit ONLY the marked `paths` (`git add --all -- <paths>` then `git commit -- <paths>`).
    /// Moves HEAD; files-pane marked-set item.
    CommitSelected { paths: Vec<String>, message: String },
    /// Write the marked `paths`' combined local-changes patch to `path`. Files-pane marked-set item.
    CreatePatchSelected { paths: Vec<String>, path: String },
    /// Delete the marked `paths` from the working tree and git. DESTRUCTIVE; files-pane
    /// marked-set item.
    DeleteSelected { paths: Vec<String> },
    /// Cherry-pick the marked `commits` (oldest-first) onto the current branch (`git cherry-pick
    /// <h...>`); a conflict aborts the whole pick. Moves HEAD; multi-commit menu.
    CherryPickSelected { commits: Vec<String> },
    /// Write the marked `commits` as a numbered patch series (`git format-patch`) into `dir`.
    /// Read-only; multi-commit menu.
    CreatePatchSeries { commits: Vec<String>, dir: String },
    /// Interactive rebase of `base..HEAD` applying a non-`pick` todo verb to each listed
    /// commit (FULL hash + its [`RebaseAction`]; the backend re-derives git's todo
    /// abbreviation and rewrites that line). `Drop` removes the commit; `Squash`/`Fixup`
    /// meld it into the preceding (older) kept commit. A non-interactive `git rebase -i
    /// <base>` whose todo marks those lines; a conflict aborts so the repo never sticks.
    /// Only the non-`pick` rows are carried (a pick needs no rewrite). DESTRUCTIVE rewrite.
    RebaseTodo { base: String, ops: Vec<(String, RebaseAction)> },
    /// Check out the ref `name` (`git checkout <name>`) from the branch/tag submenu: a
    /// branch attaches HEAD, a tag/remote ref detaches. Behind a confirm.
    CheckoutRef { name: String },
    /// Merge the ref `name` into the current branch (`git merge --no-edit <name>`). A
    /// conflict aborts. From the branch/tag submenu, behind a confirm.
    MergeRef { name: String },
    /// Rebase the current branch onto the ref `name` (`git rebase <name>`). A conflict
    /// aborts. DESTRUCTIVE rewrite; from the branch submenu, behind a confirm.
    RebaseOnto { name: String },
    /// Rename local branch `old` to `new` (`git branch -m`). From the branch submenu's
    /// Rename input dialog.
    BranchRename { old: String, new: String },
    /// Delete local branch `name`, ALWAYS the safe `git branch -d`. From the branch
    /// submenu, behind a confirm. `-d` refuses an unmerged branch with git's own clear
    /// "not fully merged - run 'git branch -D'" error (surfaced as a Notice), so the UI
    /// never silently discards unpushed work; force-delete stays a CLI-only escape hatch.
    BranchDelete { name: String },
    /// Delete tag `name` (`git tag -d`). From the tag submenu, behind a confirm.
    TagDelete { name: String },
    /// Push local branch `name` to its remote (`git push <remote> <name>`). From the
    /// branch submenu, behind a confirm. Network IO.
    PushRef { name: String },
    /// Pull `remote`/`branch` into the current branch using rebase or merge (`git pull
    /// --rebase|--no-rebase`). From the remote-branch submenu, behind a confirm. Network IO.
    PullRef { remote: String, branch: String, rebase: bool },
    /// Add a remote `name` -> `url` (`git remote add`). From the Manage Remotes dialog.
    RemoteAdd { name: String, url: String },
    /// Remove the remote `name` (`git remote remove`). From the Manage Remotes dialog, behind
    /// a confirm.
    RemoteRemove { name: String },
    /// Set the remote `name`'s fetch URL (`git remote set-url`). From the Manage Remotes dialog.
    RemoteSetUrl { name: String, url: String },
    /// Apply a unified-diff patch file onto the working tree (`git apply <path>`). From the
    /// global Git menu's Apply Patch item.
    ApplyPatch { path: String },
    /// Pull the current branch from its upstream with an integration strategy (`git pull` +
    /// `--ff-only`/`--no-rebase`/`--rebase`): `None` = ff-only, `Some(false)` = merge,
    /// `Some(true)` = rebase. The global Git menu's Pull (Update Project) strategy picker.
    PullStrategy { rebase: Option<bool> },
}

/// How a reset moves the working tree + index relative to the branch move, mapped to
/// the `git reset` flag. The store-layer vocabulary for the reset-mode picker; the
/// loader translates it to the primitive flag for the backend (which stays git-only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
    Keep,
}

impl ResetMode {
    /// The picker's options, in display order. `Choice`'s `sel` indexes this.
    pub const ALL: [ResetMode; 4] = [Self::Soft, Self::Mixed, Self::Hard, Self::Keep];

    /// Short title-cased name shown as the option label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Soft => "Soft",
            Self::Mixed => "Mixed",
            Self::Hard => "Hard",
            Self::Keep => "Keep",
        }
    }

    /// One-line description of what the mode does to the index + working tree.
    pub fn description(self) -> &'static str {
        match self {
            Self::Soft => "move the branch; keep changes staged",
            Self::Mixed => "move the branch; keep changes unstaged",
            Self::Hard => "move the branch; DISCARD all local changes",
            Self::Keep => "move the branch; keep local edits (abort on conflict)",
        }
    }

    /// The `git reset` flag this mode maps to.
    pub fn flag(self) -> &'static str {
        match self {
            Self::Soft => "--soft",
            Self::Mixed => "--mixed",
            Self::Hard => "--hard",
            Self::Keep => "--keep",
        }
    }
}

/// What a [`Dialog::Choice`] single-select picker is choosing: drives its title + the
/// option list + the action `sel` builds. Generalizes the picker so later destination
/// pickers reuse the same render/layout/key-nav path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceKind {
    ResetMode,
    PullStrategy,
}

/// The archive format the commit-log "Create archive" picker seeds: zip (default), gzipped
/// tar, or plain tar. The choice only sets the prefilled destination EXTENSION - the backend
/// reads the final extension - so a user-edited path still wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    TarGz,
    Tar,
}

impl ArchiveFormat {
    /// The picker's options, in display order (zip first = the default). `Choice`'s `sel` indexes this.
    pub const ALL: [ArchiveFormat; 3] = [Self::Zip, Self::TarGz, Self::Tar];

    /// Short label shown as the option.
    pub fn label(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::TarGz => "tar.gz",
            Self::Tar => "tar",
        }
    }

    /// One-line description of the format.
    pub fn description(self) -> &'static str {
        match self {
            Self::Zip => "a .zip archive (default)",
            Self::TarGz => "a gzip-compressed tarball",
            Self::Tar => "an uncompressed tarball",
        }
    }

    /// The destination filename extension (no leading dot) the default name uses.
    pub fn ext(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::TarGz => "tar.gz",
            Self::Tar => "tar",
        }
    }
}

/// How the global Git menu's Pull (WebStorm's "Update Project") integrates the fetched
/// remote work into the current branch: fast-forward only (refuse to create a merge), a
/// merge commit, or rebase the local commits on top. The picker's options; the confirm
/// maps the choice to a `git pull` flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullStrategy {
    FastForward,
    Merge,
    Rebase,
}

impl PullStrategy {
    /// The picker's options, in display order. `Choice`'s `sel` indexes this.
    pub const ALL: [PullStrategy; 3] = [Self::FastForward, Self::Merge, Self::Rebase];

    /// Short label shown as the option.
    pub fn label(self) -> &'static str {
        match self {
            Self::FastForward => "Fast-forward only",
            Self::Merge => "Merge",
            Self::Rebase => "Rebase",
        }
    }

    /// One-line description of the integration behaviour.
    pub fn description(self) -> &'static str {
        match self {
            Self::FastForward => "refuse if a merge would be needed (safest)",
            Self::Merge => "merge the remote into the current branch",
            Self::Rebase => "replay local commits on top of the remote",
        }
    }

    /// The `git pull` integration flag: `None` = fast-forward-only, `Some(false)` = merge
    /// (`--no-rebase`), `Some(true)` = rebase (`--rebase`). The loader maps it to the backend.
    pub fn rebase(self) -> Option<bool> {
        match self {
            Self::FastForward => None,
            Self::Merge => Some(false),
            Self::Rebase => Some(true),
        }
    }
}

/// What a [`Dialog::RefPick`] branch/tag picker does with the chosen ref: the global Git
/// menu's Branches (checkout) / Merge / Rebase entries. Each maps the picked ref name to a
/// commit-targeted [`GitAction`] behind a confirm (the destructive ones warn). Decoupled from
/// the file-inspect [`Dialog::Picker`] (which carries an [`InspectMode`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefOp {
    Checkout,
    Merge,
    Rebase,
}

impl RefOp {
    /// The picker title for this op.
    pub fn title(self) -> &'static str {
        match self {
            RefOp::Checkout => "Checkout branch or tag",
            RefOp::Merge => "Merge branch or tag into current",
            RefOp::Rebase => "Rebase current onto branch or tag",
        }
    }

    /// The confirm button label.
    pub fn button(self) -> &'static str {
        match self {
            RefOp::Checkout => "[Checkout]",
            RefOp::Merge => "[Merge]",
            RefOp::Rebase => "[Rebase]",
        }
    }
}

/// The picker title for `kind`.
pub fn choice_title(kind: ChoiceKind) -> &'static str {
    match kind {
        ChoiceKind::ResetMode => "Reset current branch to here",
        ChoiceKind::PullStrategy => "Pull - update the current branch",
    }
}

/// The picker's `(label, description)` options for `kind`, in display order. `Choice`'s
/// `sel` indexes this; the confirm path maps `sel` back through the same order.
pub fn choice_options(kind: ChoiceKind) -> Vec<(&'static str, &'static str)> {
    match kind {
        ChoiceKind::ResetMode => {
            ResetMode::ALL.iter().map(|m| (m.label(), m.description())).collect()
        }
        ChoiceKind::PullStrategy => {
            PullStrategy::ALL.iter().map(|s| (s.label(), s.description())).collect()
        }
    }
}

/// What an [`Dialog::Rebase`] row does to its commit when the rebase runs. `Pick` keeps
/// the commit as-is; `Squash`/`Fixup` meld it into the preceding (older) kept commit -
/// squash combines both messages, fixup discards this one's; `Drop` removes it. Maps 1:1
/// to a git interactive-rebase todo verb (the backend rewrites the line to it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseAction {
    Pick,
    Squash,
    Fixup,
    Drop,
}

impl RebaseAction {
    /// The lowercase git todo-verb, also the row's action label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
        }
    }

    /// Whether the verb melds this commit into the preceding kept commit. Such a verb is
    /// invalid on the first KEPT commit of the range (nothing older to meld into), which
    /// the store validates before parking the rebase.
    pub fn is_meld(self) -> bool {
        matches!(self, Self::Squash | Self::Fixup)
    }
}

/// One row of the interactive-rebase dialog: a commit (short hash + subject, snapshotted
/// from the model at open time) plus the [`RebaseAction`] the user assigned it. The dialog
/// holds `base..HEAD` worth of these, newest first (the model's order); the backend matches
/// the todo by hash, so row order is display-only and never reordered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseStep {
    /// Short hash for the row label.
    pub short: String,
    /// Full hash carried into the rebase op set; the backend re-derives git's todo
    /// abbreviation from it (`rev-parse --short`) so the todo-rewrite sed matches the line.
    pub full: String,
    pub subject: String,
    pub action: RebaseAction,
}

/// Which text-input dialog is open (drives its title + the action its text feeds).
/// `Commit`/`Amend`/`Tag` target HEAD/upstream; `NewBranch`/`NewTag` target a specific
/// commit (its hash snapshotted onto the open [`Dialog::Input`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    Commit,
    Amend,
    Tag,
    NewBranch,
    NewTag,
    /// Reword HEAD's commit message (prefilled, message-only; targets HEAD).
    Reword,
    /// Write the selected commit as a patch to the entered file path (prefilled with a
    /// `/tmp/<short>.patch` default; targets the snapshotted commit).
    CreatePatch,
    /// Write a files-pane file's LOCAL CHANGES to the entered patch path (prefilled with a
    /// `/tmp/<basename>.patch` default; the FILE path is snapshotted in the `commit` slot).
    CreateWorkingPatch,
    /// Commit message for a single files-pane file (`git commit -- <file>`; the FILE path is
    /// snapshotted in the `commit` slot, the entered text is the message).
    CommitFile,
    /// Commit message for a files-pane DIRECTORY (`git commit -- <dir>`; the folder prefix is
    /// snapshotted in the `commit` slot, the entered text is the message).
    CommitFolder,
    /// Commit message for the MARKED file set (`git commit -- <paths>`; the paths are parked in
    /// `view.parked_marked`, the entered text is the message). Files-pane marked-set item.
    CommitSelected,
    /// Destination `.patch` path for the MARKED file set's combined patch (prefilled
    /// `/tmp/selected.patch`; the paths are parked in `view.parked_marked`).
    CreatePatchSelected,
    /// Destination DIRECTORY for the marked commits' patch series (prefilled `/tmp/patches`;
    /// the commit hashes are parked in `view.parked_marked`). Multi-commit menu.
    CreatePatchSeries,
    /// Rename a local branch (prefilled with its current name; the OLD name is snapshotted
    /// in the dialog's `commit` slot, reused as the `git branch -m` source).
    RenameBranch,
    /// Archive the repository to the entered `.zip` path (prefilled with a `/tmp/<repo>.zip`
    /// default; no target - the whole repo at HEAD). Repo-root files-pane item.
    ArchiveProject,
    /// Add a remote: a single `name url` entry (space-separated; first token = name, the rest
    /// = url). No target. From the Manage Remotes dialog.
    RemoteAdd,
    /// Edit a remote's URL (prefilled with its current URL; the remote NAME is snapshotted in
    /// the `commit` slot). From the Manage Remotes dialog.
    RemoteSetUrl,
    /// Write the WHOLE working tree's local-changes diff to the entered patch path (prefilled
    /// `/tmp/working.patch`; no target - `CreateWorkingPatch` with an empty file = whole tree).
    /// The global Git menu's Create Patch item.
    CreatePatchAll,
    /// Read a unified-diff patch from the entered path and `git apply` it (prefilled
    /// `/tmp/working.patch`). The global Git menu's Apply Patch item.
    ApplyPatch,
}

/// The four "copy commit field" picker options, in display order. The selected commit
/// supplies the actual text at confirm time. `Full info` is every log column (subject,
/// author, short hash, date); `Full hash` is the 40-char oid.
pub const COPY_FIELDS: [&str; 4] = ["Short hash", "Full hash", "Message", "Full info"];

/// The modal dialog currently open over the panes (at most one). Distinct from the
/// revert confirmation modal (`revert_confirm`), which predates this and stays its own
/// flow. Built + driven ZERO-IO by `apply`; the runtime executes the parked result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dialog {
    /// A single-line text entry (commit/amend message, tag name, new branch/tag,
    /// reword). `field` is the editable line (caret/selection/clipboard). `commit` is
    /// the target hash for the commit-targeted kinds (new branch/tag, reword),
    /// snapshotted at open time; `None` for the HEAD/upstream kinds (commit/amend/tag).
    /// `note` is an optional dim warning line (e.g. rewording a pushed/older commit).
    /// `checkbox` is an optional `(label, checked)` toggle (new-branch checkout).
    Input {
        kind: InputKind,
        field: TextField,
        commit: Option<String>,
        note: Option<String>,
        checkbox: Option<(String, bool)>,
    },
    /// A yes/no confirmation for an outward-facing action (push / pull).
    Confirm { action: GitAction, prompt: String },
    /// A field picker: copy the selected commit's chosen field to the clipboard.
    /// `fields` holds the full text of each [`COPY_FIELDS`] entry (snapshotted at
    /// open time from the selected commit) so the picker can show a dim preview and
    /// copy without re-reading the commit. Indexed parallel to `COPY_FIELDS`.
    Copy { sel: usize, fields: [String; 4] },
    /// A single-select picker over [`choice_options`]`(kind)`: the `sel` row builds a
    /// commit-targeted [`GitAction`] on confirm (the reset-mode picker). `commit` is the
    /// snapshotted target hash; `note` an optional dim warning (e.g. resetting a pushed
    /// branch). Rows click-to-SELECT only (not auto-confirm) - a destructive op needs the
    /// explicit confirm, unlike [`Dialog::Copy`].
    Choice { kind: ChoiceKind, sel: usize, commit: String, note: Option<String> },
    /// The interactive-rebase mark-items dialog: one [`RebaseStep`] per commit in
    /// `base..HEAD` (newest first, the model's order), each togglable Pick/Drop. `sel` is
    /// the focused row; `base` is the snapshotted rebase base (the picked commit's hash -
    /// the backend derives `^`/`--root`). `note` warns when the range is published. Rows
    /// click-TOGGLE the action (not auto-confirm); the explicit [Rebase] button runs it.
    Rebase { steps: Vec<RebaseStep>, sel: usize, base: String, note: Option<String> },
    /// A scrollable single-select picker for the "Compare with..." / "Show History" actions:
    /// each `items` row is a `(label, rev)`; the `sel` row's rev becomes the target on confirm
    /// (Enter / click), parking an inspect of `mode` for `path`. `title` labels the dialog. `sel`
    /// windows the list (cap-and-scroll like Rebase). Rows click-to-CONFIRM (a read-only view
    /// needs no extra step), unlike the destructive Choice/Rebase.
    Picker { title: String, path: String, items: Vec<PickItem>, sel: usize, mode: InspectMode },
    /// A branch/tag picker for the global Git menu's Branches / Merge / Rebase entries: each
    /// `items` row is a `(label, ref_name)`; confirming the `sel` row opens a confirm carrying the
    /// `op`'s [`GitAction`] (checkout / merge / rebase-onto) for that ref. Caps + scrolls like the
    /// other list dialogs. Decoupled from [`Dialog::Picker`] (which parks a file inspect).
    RefPick { items: Vec<PickItem>, sel: usize, op: RefOp },
    /// The Manage Remotes list: one row per remote (`(name, url)`), `sel` the focused row.
    /// Up/Down move; `a` opens the add-remote input, `e`/Enter edits the selected URL, `d`
    /// removes it (behind a confirm), Esc closes. Each add/edit/remove fires a git write then
    /// closes; the user reopens the dialog to see the refreshed list. Caps + scrolls like the
    /// other list dialogs.
    Remotes { remotes: Vec<(String, String)>, sel: usize },
    /// The read-only keybindings overview (the lazygit-style `?` popup). Static content
    /// (rendered by `ui::dialog::help_body`); Enter/Esc/`?`/[Close] dismiss it.
    Help,
}

/// A clickable verb chip on the bottom HINT BAR (the lazygit-style key strip). The
/// layout exposes `(Rect, HintKey)` pairs; the runtime maps a chip click to the SAME
/// `Msg` its hotkey produces, so the bar can never drift from the keymap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintKey {
    /// `c` - open the commit-message input.
    Commit,
    /// `p` - open the pull-strategy picker.
    Pull,
    /// `P` - open the push confirm.
    Push,
    /// `S` - open the stash confirm.
    Stash,
    /// `?` - open the keybindings popup.
    Help,
    /// `q` - quit.
    Quit,
}

/// A read-only text selection in the commit-detail pane, in WRAPPED visual coordinates
/// (`row` = visual line index after wrapping, `col` = char index into that line). Set
/// by a mouse drag / double-click; Ctrl+C copies the spanned text to the system
/// clipboard. Cleared by any unrelated interaction. The renderer paints the band and
/// the runtime maps clicks; both read `span` so order is normalized in one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DetailSel {
    /// Where the selection was anchored (the press / word start).
    pub anchor: (usize, usize),
    /// The moving end (the drag point / word end).
    pub cursor: (usize, usize),
}

impl DetailSel {
    /// The ordered span `[(start), (end))` with anchor/cursor sorted.
    pub fn span(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Whether visual `(row, col)` lies inside the selection (half-open, for rendering).
    pub fn contains(&self, row: usize, col: usize) -> bool {
        let (s, e) = self.span();
        (row, col) >= s && (row, col) < e
    }

    /// Whether the selection spans no characters (anchor == cursor).
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }
}

/// Smallest fraction a pane may shrink to (and, by symmetry, the largest it grows).
pub const SPLIT_MIN: f32 = 0.15;
/// Largest fraction a pane may grow to.
pub const SPLIT_MAX: f32 = 0.85;
/// One keyboard nudge step, as a fraction of the parent span.
const SPLIT_STEP: f32 = 0.03;

/// Selection + focus + view options the UI needs to render and respond to input.
#[derive(Clone, Debug)]
pub struct ViewState {
    pub focus: Pane,
    pub log_sel: usize,
    pub files_sel: usize,
    /// Multi-selection of changed-file rows, keyed by FULL path so it survives
    /// fold/unfold and flatten-index shifts. EMPTY == "just the cursor" (the
    /// default flow); the cursor (`files_sel`) is always the active row driving the
    /// single diff/preview. Cleared whenever the commit/tree changes (a new commit
    /// has different files). The anchor for a range select is `files_sel` at the
    /// last plain (single) select.
    pub files_marked: BTreeSet<String>,
    /// Marked COMMIT hashes for a multi-commit selection (Ctrl/Shift-click in the log) - the
    /// set the multi-commit context menu acts on (cherry-pick / patch series). Like
    /// `files_marked` but for the log; the cursor (`log_sel`) is still the single active row.
    /// Cleared on a plain select / a repo reload. Holds the `Commit::hash` of each marked row.
    pub commits_marked: BTreeSet<String>,
    /// Whether the top diff/preview viewer is shown (false -> log takes full height).
    pub show_diff: bool,
    /// Files pane mode: OFF (default) shows only the selected commit's CHANGED
    /// files; ON shows the commit's FULL file tree (changed files keep their status
    /// color, unchanged files render plain). Flipping it triggers a tree reload for
    /// the current commit in the chosen mode. Default false keeps the golden render.
    pub show_all_files: bool,
    /// Files pane layout: OFF (default) shows the nested directory tree; ON shows a
    /// FLAT list of files (full path as the name, no folder rows). Purely a view
    /// transform over the same tree - no reload. Default false keeps the golden render.
    pub files_flat: bool,
    /// The repository directory basename - the `<repo>` part of the zip-archive default
    /// filename. Seeded at boot from the opened path (the fixture seeds a deterministic name).
    pub repo_root_name: String,
    /// Today's date `YYYY-MM-DD`, seeded once by the runtime at boot (the clock is not
    /// available to the zero-IO `apply`). The `<current>` zip-archive prefill's date suffix;
    /// empty until seeded (the synchronous golden path leaves it blank for determinism).
    pub today: String,
    /// Side-by-side vs unified layout of the diff body.
    pub diff_mode: DiffMode,
    /// Wrap long code lines in the viewer instead of clipping.
    pub word_wrap: bool,
    /// Render whitespace markers in the viewer.
    pub show_whitespace: bool,
    /// Collapse unchanged runs in the editable `<current>` live diff to a fold marker
    /// (3-line context around each change), so it shows only the changes instead of the
    /// whole working file. A read-only commit diff already arrives folded from git, so
    /// this flag only changes the live diff. Default off (the full file is editable).
    pub hide_unchanged: bool,
    /// Show a per-line git-blame gutter beside the diff for EVERY opened file (the View > Blame
    /// toggle), persisted across runs. The annotated lines key off the NEW (editable) side.
    pub show_blame: bool,
    /// The blame for the currently-shown file (one `BlameLine` per NEW-side line), fetched when
    /// `show_blame` is on and the file opens/navigates. `None` until it loads (or when off).
    pub blame: Option<crate::diff::BlameFile>,
    /// First visible line of the diff/preview body (vertical scroll offset).
    pub diff_scroll: usize,
    /// The editable diff's free-scroll override: `Some(top)` pins the first visible
    /// logical row WITHOUT moving the caret (a mouse-wheel tick over the editable diff),
    /// so the view scrolls independently of the cursor and the caret may leave the
    /// viewport. `None` (the default) restores the cursor-follow scroll, so the caret is
    /// always visible. Any edit or navigation resets it to `None` (the caret snaps back
    /// into view). Read by the renderer AND the click hit-test, so both agree cell-for-cell.
    pub edit_scroll: Option<usize>,
    /// The diff's HORIZONTAL scroll override (code columns hidden off the left), used
    /// only when word-wrap is OFF (wrapping never scrolls sideways). `Some(n)` parks a
    /// wheel-driven offset shared by BOTH the committed (browse) and editable sides so a
    /// long line's tail is reachable; `None` (the default) keeps the caret in view while
    /// editing (cursor-follow) and column 0 while browsing. Any edit / caret move /
    /// selection change resets it to `None`. Read by the renderer AND the click hit-test.
    pub diff_hscroll: Option<usize>,
    /// The commit-log list's free-scroll override: `Some(offset)` pins the first visible
    /// row WITHOUT moving the selection (a wheel tick over the log), so the selected
    /// commit may scroll out of view. `None` (the default) lets the selection drive the
    /// offset (the list keeps it visible). Any selection move / filter change resets it.
    pub log_scroll: Option<usize>,
    /// The files list's free-scroll override, mirroring [`Self::log_scroll`] for the
    /// changed-files panel: `Some(offset)` scrolls without moving the file selection.
    pub files_scroll: Option<usize>,
    /// Focused diff line (index into the previewed `FileDiff.lines`) when the diff
    /// pane has focus. Drives the gutter revert icon + per-hunk revert target.
    pub diff_cursor: usize,
    /// A hunk-revert was requested once on the focused changed line; a second request
    /// confirms (a status warns in between). Reset by any other action.
    pub hunk_revert_armed: bool,
    /// Diff region height as a fraction of the diff+body span. See [`Divider::DiffLog`].
    pub split_diff_v: f32,
    /// Commit-log width as a fraction of the panes row. See [`Divider::LogRight`].
    pub split_log_h: f32,
    /// Files-tree height as a fraction of the right column. See [`Divider::FilesDetail`].
    pub split_right_v: f32,
    /// Diff old-pane width as a fraction of the diff body. See [`Divider::DiffOldNew`].
    pub split_diff_h: f32,

    // -- toolbar search + filters (all plain Clone data, no git data) --------
    /// Live search query over commit subject/author/hash. Empty -> all commits.
    pub search: String,
    /// The search field owns keyboard text input (caret shown, keys routed to it).
    pub search_active: bool,
    /// Recent search queries, most-recent FIRST, de-duplicated, capped at
    /// [`SEARCH_HISTORY_MAX`]. Persisted across runs (state.toml). The lens icon opens
    /// a popup of these to re-run a past query.
    pub search_history: Vec<String>,
    /// Whether the lens's recent-search history popup is open.
    pub search_history_open: bool,
    /// `.*` toggle: treat `search` as a regex instead of a plain substring. Matching
    /// is ALWAYS case-insensitive (there is no case toggle).
    pub regex_on: bool,
    /// Live search over the changed-files list, matching the file PATH. Empty -> all
    /// files. While non-empty the files pane forces the All (full-tree) view so the
    /// query spans the whole repo, restoring the prior mode on clear.
    pub files_search: String,
    /// The files-search field owns keyboard text input (caret shown, keys routed to it).
    pub files_search_active: bool,
    /// `.*` toggle for the files search (always case-insensitive, like the log search).
    pub files_regex_on: bool,
    /// The `show_all_files` value to restore when the files query clears; `Some` only
    /// while a query forced the All view on (or the user took manual control of All).
    pub files_prev_all: Option<bool>,
    /// Selected option for each filter dropdown; `None` means "All" (no filter).
    pub filter_branch: Option<String>,
    pub filter_user: Option<String>,
    pub filter_date: Option<String>,
    /// The dropdown currently open as a popup, if any.
    pub open_dropdown: Option<FilterKind>,
    /// Highlighted row in the open dropdown (index into its option list).
    pub dropdown_sel: usize,
    /// The top menu-bar menu open as a popup, if any. Mutually exclusive with an
    /// open filter dropdown (opening one closes the other).
    pub open_menu: Option<MenuId>,
    /// The commit row's right-click context menu, open as a popup over the log, if
    /// any. Mutually exclusive with the menus/dropdowns (opening any closes it; a
    /// selection move / Esc / click-away closes it).
    pub commit_menu: Option<CommitMenu>,
    /// The files-pane row's right-click context menu, open as a popup over the files
    /// list, if any. Mutually exclusive with every other popup (the same choke points
    /// close it); a selection move / Esc / click-away closes it.
    pub files_menu: Option<FilesMenu>,

    // -- commit detail panel --------------------------------------------------
    /// Whether the detail panel's "In N branches" list is expanded (one branch
    /// per line) vs collapsed (single truncated line + a "Show all" link).
    pub branches_expanded: bool,
    /// First visible line of the detail panel (vertical scroll offset). Mainly
    /// scrolls the expanded branch list when it outgrows the pane height.
    pub detail_scroll: usize,
    /// The active read-only text selection in the detail pane (mouse drag / double-
    /// click), or `None`. Copied with Ctrl+C; cleared by any unrelated interaction.
    pub detail_sel: Option<DetailSel>,
    /// The active CHARACTER-level selection in a READ-ONLY diff (a mouse drag over the
    /// committed code), or `None`. `anchor`/`cursor` are `(logical diff-line, char column)`.
    /// Copied with Ctrl+C; cleared on any navigation / edit. Never set while the editable
    /// buffer is open (that side has its own editor selection).
    pub diff_sel: Option<DetailSel>,
    /// Whether the open file is shown as ONE full-width pane instead of the two-pane
    /// diff. Decided at file-open time ("stable at open"): true when the file had NO
    /// changes (identical sides - a plain file with nothing to compare), so it never
    /// renders the same text twice. Held stable while editing (re-evaluated only on the
    /// next file open), so a buffer that gains edits does not reflow mid-typing.
    pub diff_full_width: bool,

    // -- revert flow ----------------------------------------------------------
    /// The confirmation modal for "Revert Selected Changes", `Some` while it is
    /// open. Built ZERO-IO by `apply` from the target paths; the modal renderer
    /// reads it and `apply` gates keys/clicks to Confirm/Cancel while it is set.
    pub revert_confirm: Option<RevertRequest>,

    /// Monotonic NAVIGATION epoch: bumped by every selection/filter/search move
    /// (`select_commit`/`select_file`/`move_cursor_to`/`after_filter_change`/
    /// `after_files_search_change`). The runtime stamps it into the dialog-opening
    /// reads (PickList/RefList/Remotes) and their replies echo it; `apply` drops a
    /// reply whose epoch predates the latest navigation, so a slow read can never
    /// pop a modal long after the user moved on. (Selection-scoped reads keep their
    /// value guards; this covers the replies that HAVE no value to guard by.)
    pub nav_epoch: u64,

    // -- effects queue ---------------------------------------------------------
    /// The ordered side-effect queue: everything `apply` (ZERO-IO) asks the runtime
    /// to DO - loader requests, the system clipboard, selection-cache resets. Pushed
    /// in program order, drained FIFO by the runtime after every apply, so the
    /// relative order of effects (a save queued before a git write runs before it)
    /// holds by construction. Replaces the per-feature `pending_*` mailbox fields:
    /// a new async op is one `Effect` variant + one exhaustive runtime match arm,
    /// not a field + a hand-ordered drain site.
    pub effects: Vec<Effect>,

    // -- in-diff editing ------------------------------------------------------
    /// The editable working buffer for the selected file, or `None` when the file is
    /// not editable (read-only / binary / still loading). Auto-loaded on selection.
    pub editor: Option<EditorState>,
    /// The internal clipboard register for editor copy/cut/paste. Plain in-memory
    /// text (no system clipboard dependency); shared across files in the session.
    pub clipboard: String,
    /// Autosave the editable buffer when navigating away / on Esc (default true).
    /// When OFF, only Ctrl+S writes to disk; unsaved edits are dropped on navigation,
    /// matching a deliberate "no autosave" choice. Toggled from the Editor menu.
    pub autosave: bool,

    // -- repo-level git ops (top Git menu): dialogs + parked state ------------
    /// The open modal dialog (commit/amend/tag input, push/pull confirm, copy picker,
    /// compare/history picker), or `None`. Built + driven ZERO-IO by `apply`; gates keys
    /// while open.
    pub dialog: Option<Dialog>,
    /// Snapshot of the marked file set when a marked-set menu action opens a dialog (commit
    /// message / patch destination): the paths are parked here, drained by the dialog confirm
    /// into the `GitAction`'s `paths`. Decouples the action from a later mark change.
    /// PARKED cross-apply state (consumed by a LATER apply), not a runtime effect.
    pub parked_marked: Vec<String>,
    /// Carry a git-action result Notice ACROSS the reload it triggered: `RepoLoaded` resets
    /// the status to `Ready`, which would otherwise wipe the "Committed/Deleted/..." notice
    /// before the user sees it. `git_action_done` parks it here on a `reload` action and
    /// `RepoLoaded` re-applies it after Ready. One-shot (taken on apply). PARKED state.
    pub parked_notice: Option<String>,
    /// Set when `RepoLoaded` re-applies a git result notice: the reload AUTO-OPENS the newly
    /// selected file, and `edit_file_loaded` would otherwise clear the notice to Ready as the
    /// editable buffer opens. While sticky, that clear is suppressed so the result survives the
    /// auto-open; genuine navigation (`select_file`/`select_commit`) un-sticks it so the next
    /// file-open clears it normally.
    pub notice_sticky: bool,

    // -- read-only inspect overlay --------------------------------------------
    /// A transient read-only overlay on the diff pane (Show Current Revision / a compared
    /// revision), or `None`. When set, the diff pane renders THIS view read-only and freezes
    /// the editable buffer beneath it; Esc or any navigation/edit choke clears it.
    pub inspect: Option<InspectView>,
    /// The user's `show_diff` toggle captured when an overlay forced the diff pane visible
    /// (the menu offers inspect even with View > Show Diff OFF). Restored when the overlay is
    /// dismissed so the persistent toggle is not silently flipped. `None` = no overlay forcing.
    pub inspect_prior_show_diff: Option<bool>,
    /// A file path to REVEAL in the files pane once the next commit's tree loads (Show History
    /// navigates the log to the picked commit, then re-selects this file by path). Consumed by
    /// the first matching `apply_tree`. `None` = no pending reveal. PARKED cross-apply state.
    pub parked_file_path: Option<String>,
    /// "Show Current Revision" read-only FALLBACK path: set alongside `parked_file_path` when
    /// jumping to `<current>` to edit a file. If the working tree has no row for it (an unchanged
    /// file - nothing to edit), `apply_tree` parks a read-only HEAD overlay of this path instead
    /// of navigating away to nothing. `None` for a plain reveal (Show History never falls back).
    pub parked_revision: Option<String>,
}

/// A side effect `apply` (ZERO-IO) asks the runtime to perform, queued in
/// [`ViewState::effects`] in program order and drained FIFO after every apply.
/// The vocabulary is view-layer (the loader translates to its `Req`s), so the
/// store never imports the loader. One exhaustive runtime match consumes these:
/// adding a variant without a drain arm is a compile error, never a dead mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Batch-revert the request's paths in the working tree (`Req::Revert`).
    Revert(RevertRequest),
    /// Write the editor buffer to disk (`Req::SaveFile`).
    Save { path: String, content: String },
    /// Revert one hunk of `commit`'s change to `path` (`Req::RevertHunk`).
    HunkRevert { commit: String, path: String, hunk: usize },
    /// A repo-level git write (`Req::Git`).
    Git(GitAction),
    /// Compute a file's local-changes patch for the clipboard (`Req::CopyPatch`).
    CopyPatch(String),
    /// The marked set's combined patch for the clipboard (`Req::CopyPatchMulti`).
    CopyPatchMulti(Vec<String>),
    /// Load a deeper commit slice (`Req::LoadMore`).
    LoadMore,
    /// Fetch a read-only inspect overlay (`Req::Inspect`).
    Inspect(InspectReq),
    /// Enumerate a picker's options off-thread (`Req::PickList`).
    PickList(PickListReq),
    /// Enumerate the repo's remotes (`Req::Remotes`).
    LoadRemotes,
    /// Enumerate branches/tags for the ref picker of this op (`Req::RefList`).
    RefPick(RefOp),
    /// Full repo reload (`Req::Reload`) + reset of the runtime's selection cache.
    ReloadRepo,
    /// Repo reload (`Req::Reload`) KEEPING the runtime's selection cache: the status
    /// poll detected an external change, so the log/files refresh but the open
    /// editor buffer and the selected file's loaded view are NOT re-fetched (a
    /// re-open would replace a dirty buffer and reset the caret).
    RefreshRepo,
    /// Put `text` on the SYSTEM clipboard (wl-copy/xclip at the runtime boundary).
    Clipboard(String),
    /// Re-request the selected file's view even though the selection did not move
    /// (a hunk revert / fold toggle changed what it should show).
    ReloadPreview,
    /// Re-request the selected commit's tree (a revert in the All view changed
    /// file statuses without pruning rows).
    ReloadTree,
    /// Re-request the selected commit's FULL detail (a selection-keeping repo
    /// refresh rebuilt the cheap sync detail, dropping the loaded enrichment -
    /// committer email, containing branches - until this re-fetch lands).
    ReloadDetail,
    /// Re-fetch the blame gutter for the open file (the Blame toggle flipped on).
    ReloadBlame,
}

impl ViewState {
    /// Initial view state: log pane focused, given files-row preselected, diff
    /// viewer shown side-by-side with mouse-friendly defaults.
    pub fn new(files_sel: usize) -> Self {
        Self {
            focus: Pane::Log,
            log_sel: 0,
            files_sel,
            files_marked: BTreeSet::new(),
            commits_marked: BTreeSet::new(),
            show_diff: true,
            show_all_files: false,
            files_flat: false,
            repo_root_name: String::new(),
            today: String::new(),
            diff_mode: DiffMode::SideBySide,
            word_wrap: false,
            show_whitespace: false,
            hide_unchanged: false,
            show_blame: false,
            blame: None,
            diff_scroll: 0,
            edit_scroll: None,
            diff_hscroll: None,
            log_scroll: None,
            files_scroll: None,
            diff_cursor: 0,
            hunk_revert_armed: false,
            split_diff_v: 0.50,
            split_log_h: 0.62,
            split_right_v: 0.58,
            split_diff_h: 0.50,
            search: String::new(),
            search_active: false,
            search_history: Vec::new(),
            search_history_open: false,
            regex_on: false,
            files_search: String::new(),
            files_search_active: false,
            files_regex_on: false,
            files_prev_all: None,
            filter_branch: None,
            filter_user: None,
            filter_date: None,
            open_dropdown: None,
            dropdown_sel: 0,
            open_menu: None,
            commit_menu: None,
            files_menu: None,
            branches_expanded: false,
            detail_scroll: 0,
            detail_sel: None,
            diff_sel: None,
            diff_full_width: false,
            revert_confirm: None,
            nav_epoch: 0,
            effects: Vec::new(),
            editor: None,
            clipboard: String::new(),
            autosave: true,
            dialog: None,
            parked_marked: Vec::new(),
            parked_notice: None,
            notice_sticky: false,
            inspect: None,
            inspect_prior_show_diff: None,
            parked_file_path: None,
            parked_revision: None,
        }
    }

    /// Whether the file at `path` is in the multi-selection set.
    pub fn is_marked(&self, path: &str) -> bool {
        self.files_marked.contains(path)
    }

    // -- queued-effect accessors (the queue is drained FIFO by the runtime; these
    //    find the first queued effect of a kind, mainly for tests asserting on the
    //    effects an apply emitted) ---------------------------------------------
    /// The first queued repo-level git write, if any.
    pub fn queued_git(&self) -> Option<&GitAction> {
        self.effects.iter().find_map(|e| match e {
            Effect::Git(a) => Some(a),
            _ => None,
        })
    }

    /// The first queued inspect-overlay fetch, if any.
    pub fn queued_inspect(&self) -> Option<&InspectReq> {
        self.effects.iter().find_map(|e| match e {
            Effect::Inspect(r) => Some(r),
            _ => None,
        })
    }

    /// The first queued picker enumeration, if any.
    pub fn queued_picklist(&self) -> Option<&PickListReq> {
        self.effects.iter().find_map(|e| match e {
            Effect::PickList(r) => Some(r),
            _ => None,
        })
    }

    /// The first queued editor save as `(path, content)`, if any.
    pub fn queued_save(&self) -> Option<(&str, &str)> {
        self.effects.iter().find_map(|e| match e {
            Effect::Save { path, content } => Some((path.as_str(), content.as_str())),
            _ => None,
        })
    }

    /// The first queued system-clipboard hand-off, if any.
    pub fn queued_clipboard(&self) -> Option<&str> {
        self.effects.iter().find_map(|e| match e {
            Effect::Clipboard(t) => Some(t.as_str()),
            _ => None,
        })
    }

    /// The first queued batch revert, if any.
    pub fn queued_revert(&self) -> Option<&RevertRequest> {
        self.effects.iter().find_map(|e| match e {
            Effect::Revert(r) => Some(r),
            _ => None,
        })
    }

    /// The first queued hunk revert as `(commit, path, hunk)`, if any.
    pub fn queued_hunk_revert(&self) -> Option<(&str, &str, usize)> {
        self.effects.iter().find_map(|e| match e {
            Effect::HunkRevert { commit, path, hunk } => {
                Some((commit.as_str(), path.as_str(), *hunk))
            }
            _ => None,
        })
    }

    /// Whether a menu action currently reads as "on" (drives the item's accent
    /// highlight). DiffMode is "on" when side-by-side; the others mirror their flag.
    pub fn menu_action_active(&self, action: MenuAction) -> bool {
        match action {
            MenuAction::DiffMode => self.diff_mode == DiffMode::SideBySide,
            MenuAction::WordWrap => self.word_wrap,
            MenuAction::Whitespace => self.show_whitespace,
            MenuAction::ShowDiff => self.show_diff,
            MenuAction::Autosave => self.autosave,
            MenuAction::HideUnchanged => self.hide_unchanged,
            MenuAction::ShowBlame => self.show_blame,
            // One-shot actions, never a persistent "on" state (incl. every Git-menu op).
            MenuAction::Undo | MenuAction::Redo | MenuAction::Revert => false,
            MenuAction::GitCommit
            | MenuAction::GitAmend
            | MenuAction::GitTag
            | MenuAction::GitUpdate
            | MenuAction::GitFetch
            | MenuAction::GitPull
            | MenuAction::GitPush
            | MenuAction::GitStash
            | MenuAction::GitUnstash
            | MenuAction::GitDiscard
            | MenuAction::GitRemotes
            | MenuAction::GitNewBranch
            | MenuAction::GitBranches
            | MenuAction::GitMerge
            | MenuAction::GitRebase
            | MenuAction::GitCreatePatch
            | MenuAction::GitApplyPatch => false,
        }
    }

    /// Whether a menu action can currently fire (drives the dimmed/disabled look).
    /// Undo/Redo depend on the editable buffer's history; the toggles are always on.
    pub fn menu_action_enabled(&self, action: MenuAction) -> bool {
        match action {
            MenuAction::Undo => self.editor.as_ref().is_some_and(EditorState::can_undo),
            MenuAction::Redo => self.editor.as_ref().is_some_and(EditorState::can_redo),
            // Revert acts on the file whose diff is shown (an open editable buffer).
            MenuAction::Revert => self.editor.is_some(),
            _ => true,
        }
    }


    /// Record a committed query at the FRONT of the recent-search history
    /// (most-recent-first), de-duplicated, capped at [`SEARCH_HISTORY_MAX`]. A blank
    /// query is ignored. The single home so the apply path and the boot overlay agree.
    pub fn record_search(&mut self, query: &str) {
        // Strip control chars so the history stays printable AND the hand-written
        // state.toml can never carry a raw control byte.
        let q: String = query.chars().filter(|c| !c.is_control()).collect();
        let q = q.trim();
        if q.is_empty() {
            return;
        }
        self.search_history.retain(|h| h != q);
        self.search_history.insert(0, q.to_string());
        self.search_history.truncate(SEARCH_HISTORY_MAX);
    }

    /// The selection slot backing filter `kind`. Single home for reading/writing
    /// a filter so the predicate, the active-label, and the dropdown pick agree.
    pub fn filter_mut(&mut self, kind: FilterKind) -> &mut Option<String> {
        match kind {
            FilterKind::Branch => &mut self.filter_branch,
            FilterKind::User => &mut self.filter_user,
            FilterKind::Date => &mut self.filter_date,
        }
    }

    /// The current selection for filter `kind` (read-only), `None` -> "All".
    pub fn filter(&self, kind: FilterKind) -> Option<&str> {
        match kind {
            FilterKind::Branch => self.filter_branch.as_deref(),
            FilterKind::User => self.filter_user.as_deref(),
            FilterKind::Date => self.filter_date.as_deref(),
        }
    }

    /// Cycle keyboard focus Log -> Files -> Diff -> Log. The Diff pane joins the
    /// cycle whenever the diff viewer is shown, so keyboard users can Tab into the
    /// live-editable right side and type (the runtime routes keys to the editor when
    /// a buffer is loaded, else to diff browsing). When the viewer is hidden, Tab
    /// stays a two-pane Log<->Files cycle.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Pane::Log => Pane::Files,
            Pane::Files if self.show_diff => Pane::Diff,
            Pane::Files => Pane::Log,
            Pane::Diff => Pane::Log,
        };
    }

    /// Cycle the diff body layout between side-by-side and unified.
    pub fn toggle_diff_mode(&mut self) {
        self.diff_mode = match self.diff_mode {
            DiffMode::SideBySide => DiffMode::Unified,
            DiffMode::Unified => DiffMode::SideBySide,
        };
    }

    /// Move divider `d` by `steps` keyboard nudges (relative). Returns whether
    /// the clamped fraction actually changed.
    pub fn nudge_split(&mut self, d: Divider, steps: isize) -> bool {
        let slot = self.split_mut(d);
        Self::store(slot, *slot + steps as f32 * SPLIT_STEP)
    }

    /// Set divider `d` to absolute fraction `frac` (mouse drag). Returns whether
    /// the clamped fraction actually changed.
    pub fn set_split(&mut self, d: Divider, frac: f32) -> bool {
        let slot = self.split_mut(d);
        Self::store(slot, frac)
    }

    /// Clamp `next` into `[SPLIT_MIN, SPLIT_MAX]` and write it to `slot`,
    /// reporting whether it changed. The single home of split clamping.
    fn store(slot: &mut f32, next: f32) -> bool {
        let clamped = next.clamp(SPLIT_MIN, SPLIT_MAX);
        let changed = clamped != *slot;
        *slot = clamped;
        changed
    }

    /// The fraction field backing divider `d`.
    fn split_mut(&mut self, d: Divider) -> &mut f32 {
        match d {
            Divider::DiffLog => &mut self.split_diff_v,
            Divider::LogRight => &mut self.split_log_h,
            Divider::FilesDetail => &mut self.split_right_v,
            Divider::DiffOldNew => &mut self.split_diff_h,
        }
    }
}

#[cfg(test)]
mod menu_bar_tests {
    use super::*;

    #[test]
    fn git_menu_is_grouped_with_separators_others_flat() {
        // The Git menu fences its many ops into intent groups; menu_items drops the Seps.
        let git = menu_rows(MenuId::Git);
        assert!(git.iter().any(|r| matches!(r, MenuRow::Sep)), "Git menu has separators");
        // No leading/trailing Sep and no back-to-back Seps.
        assert!(!matches!(git.first(), Some(MenuRow::Sep)) && !matches!(git.last(), Some(MenuRow::Sep)));
        assert!(
            !git.windows(2).any(|w| matches!(w, [MenuRow::Sep, MenuRow::Sep])),
            "no doubled separators"
        );
        // menu_items is the flat action view (Seps removed), so its count = action rows.
        let actions = git.iter().filter(|r| matches!(r, MenuRow::Action(..))).count();
        assert_eq!(menu_items(MenuId::Git).len(), actions);
        // Editor/View stay flat (no separators).
        assert!(!menu_rows(MenuId::Editor).iter().any(|r| matches!(r, MenuRow::Sep)));
        assert!(!menu_rows(MenuId::View).iter().any(|r| matches!(r, MenuRow::Sep)));
    }

    #[test]
    fn only_some_git_actions_carry_an_icon() {
        // "Add icons to SOME" - the branch/sync/discard/remotes ops have one, Amend/Stash do not.
        assert!(menu_icon(MenuAction::GitMerge).is_some());
        assert!(menu_icon(MenuAction::GitPush).is_some());
        assert!(menu_icon(MenuAction::GitRemotes).is_some());
        assert!(menu_icon(MenuAction::GitAmend).is_none());
        assert!(menu_icon(MenuAction::GitStash).is_none());
        // Rebase / Branches / Fetch are intentionally text (no icon).
        assert!(menu_icon(MenuAction::GitRebase).is_none());
        assert!(menu_icon(MenuAction::GitBranches).is_none());
        assert!(menu_icon(MenuAction::GitFetch).is_none());
        // No menu item carries a known-tofu code point (the diamond/arrow family is DejaVu-safe).
        for (a, _) in menu_items(MenuId::Git) {
            if let Some(g) = menu_icon(a) {
                assert!(!g.is_empty() && g.chars().count() == 1, "icon is a single cell");
            }
        }
    }
}

#[cfg(test)]
mod files_menu_row_tests {
    use super::*;

    #[test]
    fn long_menu_is_sectioned_short_menu_is_flat() {
        // The full working menu (11 actions) is fenced into intent groups by separators.
        let full = files_menu_rows(true, true, true, false, false);
        assert!(full.iter().any(|r| matches!(r, FilesRow::Sep)), "a long menu gets separators");
        // Separators never lead, never trail, and never sit back-to-back.
        assert!(!matches!(full.first(), Some(FilesRow::Sep)));
        assert!(!matches!(full.last(), Some(FilesRow::Sep)));
        assert!(!full.windows(2).any(|w| matches!(w, [FilesRow::Sep, FilesRow::Sep])));
        // The action sequence (separators stripped) equals the flat item list.
        let actions: Vec<FilesMenuAction> =
            full.iter().filter_map(|r| if let FilesRow::Action(a) = r { Some(*a) } else { None }).collect();
        assert_eq!(actions, files_menu_items(true, true, true, false, false));

        // The 4-row folder menu stays flat (<= 5 actions -> no separators).
        let folder = files_menu_rows(false, false, false, false, true);
        assert!(folder.iter().all(|r| matches!(r, FilesRow::Action(_))), "a short menu is not chopped up");
        assert_eq!(folder.len(), 4);

        // A minimal file menu (just Show Diff) stays flat.
        let small = files_menu_rows(true, false, false, false, false);
        assert_eq!(small.iter().filter(|r| matches!(r, FilesRow::Action(_))).count(), 1);
        assert!(small.iter().all(|r| matches!(r, FilesRow::Action(_))), "a 1-action menu stays flat");

        // A historical commit's file menu (the read-only group of 5) stays flat (not >5).
        let hist = files_menu_rows(false, false, false, true, false);
        assert_eq!(hist.iter().filter(|r| matches!(r, FilesRow::Action(_))).count(), 5);
        assert!(hist.iter().all(|r| matches!(r, FilesRow::Action(_))), "a 5-action menu stays flat");
    }
}

#[cfg(test)]
mod text_field_tests {
    use super::*;
    use crate::message::Dir;

    #[test]
    fn new_places_caret_at_end() {
        let f = TextField::new("abc".to_string());
        assert_eq!(f.caret(), 3);
        assert_eq!(f.selection(), None);
    }

    #[test]
    fn caret_move_and_insert_mid_string() {
        let mut f = TextField::new("ac".to_string());
        f.move_caret(Dir::Left, false); // between a and c
        f.insert('b');
        assert_eq!(f.text(), "abc");
        assert_eq!(f.caret(), 2);
    }

    #[test]
    fn home_end_and_bounds() {
        let mut f = TextField::new("abc".to_string());
        f.move_caret(Dir::Home, false);
        assert_eq!(f.caret(), 0);
        f.move_caret(Dir::Left, false); // clamps at 0
        assert_eq!(f.caret(), 0);
        f.move_caret(Dir::End, false);
        assert_eq!(f.caret(), 3);
        f.move_caret(Dir::Right, false); // clamps at len
        assert_eq!(f.caret(), 3);
    }

    #[test]
    fn shift_select_then_replace() {
        let mut f = TextField::new("hello".to_string());
        f.move_caret(Dir::Home, false);
        f.move_caret(Dir::Right, true);
        f.move_caret(Dir::Right, true); // selects "he"
        assert_eq!(f.selection(), Some((0, 2)));
        f.insert('H'); // replaces the selection
        assert_eq!(f.text(), "Hllo");
        assert_eq!(f.selection(), None);
    }

    #[test]
    fn backspace_and_forward_delete() {
        let mut f = TextField::new("abc".to_string());
        f.backspace(); // at end -> removes 'c'
        assert_eq!(f.text(), "ab");
        f.move_caret(Dir::Home, false);
        f.delete(); // removes 'a'
        assert_eq!(f.text(), "b");
    }

    #[test]
    fn select_all_then_cut_and_paste() {
        let mut f = TextField::new("payload".to_string());
        f.select_all();
        let cut = f.cut_take().expect("a selection");
        assert_eq!(cut, "payload");
        assert_eq!(f.text(), "");
        f.insert_str("x");
        f.insert_str(&cut); // simulate paste at caret
        assert_eq!(f.text(), "xpayload");
    }

    #[test]
    fn insert_str_strips_newlines() {
        let mut f = TextField::new(String::new());
        f.insert_str("a\nb\rc");
        assert_eq!(f.text(), "a b c", "newlines collapse to spaces (single line)");
    }

    #[test]
    fn unicode_caret_is_char_indexed() {
        let mut f = TextField::new("aeb".to_string());
        // Replace the middle char with a multibyte one to prove byte/char handling.
        f.move_caret(Dir::Home, false);
        f.move_caret(Dir::Right, false);
        f.insert('\u{00e9}'); // e-acute, 2 bytes
        assert_eq!(f.text(), "a\u{00e9}eb");
        assert_eq!(f.caret(), 2);
        f.backspace();
        assert_eq!(f.text(), "aeb");
    }
}

#[cfg(test)]
mod editor_tests {
    use super::*;
    use crate::message::{Dir, EditOp};

    fn ed(content: &str) -> EditorState {
        let mut e = EditorState::opening("f.txt".to_string());
        e.load(content);
        e
    }

    /// Apply an op with a throwaway clipboard (most tests do not exercise the register).
    fn op(e: &mut EditorState, op: EditOp) -> bool {
        let mut clip = String::new();
        e.apply_op(op, &mut clip)
    }
    fn mv(e: &mut EditorState, dir: Dir, select: bool) -> bool {
        op(e, EditOp::Move { dir, select })
    }

    #[test]
    fn load_splits_lines_and_drops_trailing_newline() {
        let e = ed("a\nb\nc\n");
        assert_eq!(e.lines, vec!["a", "b", "c"]);
        assert!(e.loaded && !e.dirty);
        assert_eq!(e.to_content(), "a\nb\nc\n");
    }

    #[test]
    fn empty_file_is_one_empty_line() {
        let e = ed("");
        assert_eq!(e.lines, vec![""]);
        assert_eq!(e.to_content(), "\n");
    }

    #[test]
    fn insert_and_newline_split() {
        let mut e = ed("ab\n");
        e.cursor_col = 1;
        assert!(op(&mut e, EditOp::Insert('X')));
        assert_eq!(e.lines[0], "aXb");
        assert_eq!(e.cursor_col, 2);
        assert!(e.dirty);
        assert!(op(&mut e, EditOp::Newline));
        assert_eq!(e.lines, vec!["aX", "b"]);
        assert_eq!((e.cursor_row, e.cursor_col), (1, 0));
    }

    #[test]
    fn backspace_joins_with_previous_line() {
        let mut e = ed("ab\ncd\n");
        e.cursor_row = 1;
        e.cursor_col = 0;
        assert!(op(&mut e, EditOp::Backspace));
        assert_eq!(e.lines, vec!["abcd"]);
        assert_eq!((e.cursor_row, e.cursor_col), (0, 2));
    }

    #[test]
    fn delete_pulls_next_line_up() {
        let mut e = ed("ab\ncd\n");
        e.cursor_col = 2;
        assert!(op(&mut e, EditOp::Delete));
        assert_eq!(e.lines, vec!["abcd"]);
    }

    #[test]
    fn backspace_at_origin_is_noop() {
        let mut e = ed("a\n");
        assert!(!op(&mut e, EditOp::Backspace), "nothing to delete at (0,0)");
        assert!(!e.dirty);
    }

    #[test]
    fn vertical_move_clamps_column() {
        let mut e = ed("longline\nx\n");
        e.cursor_col = 8;
        assert!(mv(&mut e, Dir::Down, false));
        assert_eq!(e.cursor_row, 1);
        assert_eq!(e.cursor_col, 1, "column clamps to the shorter line");
    }

    #[test]
    fn multibyte_insert_is_char_indexed() {
        let mut e = ed("café\n");
        e.cursor_col = 4;
        assert!(op(&mut e, EditOp::Insert('!')));
        assert_eq!(e.lines[0], "café!");
        assert_eq!(e.cursor_col, 5);
    }

    #[test]
    fn shift_move_builds_selection_then_plain_move_clears_it() {
        let mut e = ed("hello\n");
        // Shift+Right twice selects "he".
        mv(&mut e, Dir::Right, true);
        mv(&mut e, Dir::Right, true);
        assert_eq!(e.selected_text(), "he");
        assert_eq!(e.selection(), Some(((0, 0), (0, 2))));
        // A plain move clears the selection.
        mv(&mut e, Dir::Right, false);
        assert!(e.selection().is_none());
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut e = ed("hello\n");
        mv(&mut e, Dir::Right, true);
        mv(&mut e, Dir::Right, true); // select "he"
        assert!(op(&mut e, EditOp::Insert('X')));
        assert_eq!(e.lines[0], "Xllo");
        assert_eq!(e.cursor_col, 1);
        assert!(e.selection().is_none());
    }

    #[test]
    fn multiline_selection_text_and_delete() {
        let mut e = ed("abc\ndef\nghi\n");
        // Select from (0,1) to (2,2): "bc\ndef\ngh".
        op(&mut e, EditOp::Place { row: 0, col: 1, select: false });
        op(&mut e, EditOp::Place { row: 2, col: 2, select: true });
        assert_eq!(e.selected_text(), "bc\ndef\ngh");
        assert!(op(&mut e, EditOp::Backspace), "backspace deletes the selection");
        assert_eq!(e.lines, vec!["ai"]);
        assert_eq!((e.cursor_row, e.cursor_col), (0, 1));
    }

    #[test]
    fn copy_paste_round_trips_through_clipboard() {
        let mut e = ed("hello\n");
        let mut clip = String::new();
        e.apply_op(EditOp::Move { dir: Dir::End, select: true }, &mut clip); // select "hello"
        e.apply_op(EditOp::Copy, &mut clip);
        assert_eq!(clip, "hello");
        // Move to end of buffer and paste.
        e.apply_op(EditOp::Move { dir: Dir::End, select: false }, &mut clip);
        e.apply_op(EditOp::Paste, &mut clip);
        assert_eq!(e.lines[0], "hellohello");
    }

    #[test]
    fn cut_removes_selection_into_clipboard() {
        let mut e = ed("hello\n");
        let mut clip = String::new();
        e.apply_op(EditOp::Move { dir: Dir::Right, select: true }, &mut clip);
        e.apply_op(EditOp::Move { dir: Dir::Right, select: true }, &mut clip); // "he"
        assert!(e.apply_op(EditOp::Cut, &mut clip));
        assert_eq!(clip, "he");
        assert_eq!(e.lines[0], "llo");
    }

    #[test]
    fn multiline_paste_splits_lines() {
        let mut e = ed("XY\n");
        e.cursor_col = 1; // between X and Y
        let mut clip = "a\nb".to_string();
        e.apply_op(EditOp::Paste, &mut clip);
        assert_eq!(e.lines, vec!["Xa", "bY"]);
        assert_eq!((e.cursor_row, e.cursor_col), (1, 1));
    }

    #[test]
    fn select_all_spans_whole_buffer() {
        let mut e = ed("ab\ncd\n");
        op(&mut e, EditOp::SelectAll);
        assert_eq!(e.selected_text(), "ab\ncd");
        assert_eq!(e.cursor(), (1, 2));
    }

    #[test]
    fn place_clamps_into_buffer() {
        let mut e = ed("ab\n");
        op(&mut e, EditOp::Place { row: 9, col: 9, select: false });
        assert_eq!((e.cursor_row, e.cursor_col), (0, 2), "clamped to the only line's end");
    }

    #[test]
    fn undo_reverts_a_whole_typing_run_as_one_group() {
        let mut e = ed("\n");
        for c in "abc".chars() {
            op(&mut e, EditOp::Insert(c));
        }
        assert_eq!(e.lines[0], "abc");
        // A single undo reverts the whole coalesced typing burst.
        assert!(op(&mut e, EditOp::Undo));
        assert_eq!(e.lines[0], "", "one undo reverts the whole run");
        assert!(!e.can_undo(), "history empty after undoing the only group");
        assert!(e.can_redo(), "the undone run is now redoable");
    }

    #[test]
    fn redo_reapplies_after_undo() {
        let mut e = ed("\n");
        op(&mut e, EditOp::Insert('x'));
        op(&mut e, EditOp::Undo);
        assert_eq!(e.lines[0], "");
        assert!(op(&mut e, EditOp::Redo));
        assert_eq!(e.lines[0], "x", "redo reapplies the undone edit");
    }

    #[test]
    fn a_cursor_move_breaks_the_undo_group() {
        let mut e = ed("\n");
        op(&mut e, EditOp::Insert('a'));
        mv(&mut e, Dir::Left, false); // move breaks coalescing
        op(&mut e, EditOp::Insert('b'));
        assert_eq!(e.lines[0], "ba");
        // Two distinct groups: each undo peels one.
        op(&mut e, EditOp::Undo);
        assert_eq!(e.lines[0], "a", "first undo removes the post-move insert");
        op(&mut e, EditOp::Undo);
        assert_eq!(e.lines[0], "", "second undo removes the first insert");
    }

    #[test]
    fn a_fresh_edit_clears_the_redo_stack() {
        let mut e = ed("\n");
        op(&mut e, EditOp::Insert('a'));
        op(&mut e, EditOp::Undo);
        assert!(e.can_redo());
        op(&mut e, EditOp::Insert('b')); // a new edit invalidates redo
        assert!(!e.can_redo(), "a fresh edit drops the redo history");
        assert_eq!(e.lines[0], "b");
    }

    #[test]
    fn undo_with_empty_history_is_a_noop() {
        let mut e = ed("hi\n");
        assert!(!op(&mut e, EditOp::Undo), "nothing to undo");
        assert!(!op(&mut e, EditOp::Redo), "nothing to redo");
        assert_eq!(e.lines[0], "hi");
    }

    #[test]
    fn a_noop_edit_records_no_undo() {
        let mut e = ed("\n"); // empty buffer, cursor at origin
        assert!(!op(&mut e, EditOp::Backspace), "backspace at origin is a no-op");
        assert!(!e.can_undo(), "a no-op edit must not push an undo snapshot");
    }
}
