//! The async IO seam: a SINGLE worker thread that owns the [`GitBackend`] and is
//! the SOLE owner of git IO.
//!
//! The runtime never blocks on git: it sends a [`Req`] down a channel and the
//! worker answers with an owned [`Msg`] pushed back over the UI's `mpsc`, waking
//! the poll loop. One thread (not thread-per-keystroke) means fast navigation
//! cannot stack threads or out-of-order results: requests are answered serially.
//!
//! COALESCING (latest-wins): before serving a request the worker drains every
//! queued `Req` and keeps only the LAST, so a burst of selection moves collapses
//! to the final target - it never loads detail/tree/preview for commits the user
//! already scrolled past. A result that nonetheless lands stale is still dropped
//! by the (hash/commit, path) staleness guards in `apply`, so correctness does not
//! depend on the coalescing; it is purely a load-shedding optimization.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use crate::backend::{build_repo_model, GitBackend};
use crate::message::Msg;
use crate::tokenize;

/// A lazy-load request from the runtime to the loader worker. The INITIAL repo
/// load is implicit (the worker performs it before the request loop), so the
/// runtime only ever asks for the per-selection upgrades.
pub enum Req {
    /// Full detail for the commit at `hash` (committer, containing branches).
    Detail(String),
    /// The files tree for the commit at `hash`. `all` selects the mode: `false` ->
    /// the CHANGED-files tree (`changed_files`); `true` -> the commit's FULL file
    /// tree with Unchanged overlay (`full_tree`). Both reply with `Msg::TreeLoaded`.
    Tree { hash: String, all: bool },
    /// Open `path` (selected at `commit`) for the file viewer: an editable working
    /// buffer (-> `Msg::EditFileLoaded`) or a read-only view (-> `Msg::PreviewLoaded`).
    /// Coalesced latest-wins (a navigation-class read).
    OpenFile { commit: String, path: String },
    /// Revert `commit`'s change to each `path` in the WORKING TREE (batch). A
    /// confirmed, user-requested DESTRUCTIVE write; the worker is the SOLE IO owner.
    /// NOT coalesced (it is a one-shot action, not a navigation upgrade).
    Revert { commit: String, paths: Vec<String> },
    /// Re-run the initial repo load (`--watch` timer or manual refresh), replying
    /// with a fresh `Msg::RepoLoaded`. Coalesced latest-wins like a navigation
    /// request: a burst of ticks collapses to one reload.
    Reload,
    /// Grow the commit cap and re-load (the log's "Load more history" row), replying with a
    /// fresh `Msg::RepoLoaded`. Coalesced like `Reload` (a double-click loads one deeper page).
    LoadMore,
    /// Write the editor's `content` to the working-tree file at `path` (the save).
    /// A one-shot DESTRUCTIVE action; never coalesced (queued like a revert).
    SaveFile { path: String, content: String },
    /// Revert hunk `hunk` of `commit`'s change to `path` in the working tree. A
    /// one-shot DESTRUCTIVE action; never coalesced (queued like a revert/save).
    RevertHunk { commit: String, path: String, hunk: usize },
    /// A repo-level git write (commit/amend/tag/push/pull). A one-shot DESTRUCTIVE /
    /// outward-facing action; never coalesced. Replies with `Msg::GitActionDone`.
    Git(crate::view_state::GitAction),
    /// Compute `file`'s local-changes diff (`git diff HEAD -- <file>`) for the clipboard. A
    /// read-only one-shot; replies with `Msg::PatchCopied` (text -> clipboard) or, when there
    /// is nothing to copy / git errors, a `Msg::GitActionDone` notice. Never coalesced.
    CopyPatch { file: String },
    /// Compute the MARKED file set's combined local-changes patch for the clipboard (the
    /// multi-file analog of `CopyPatch`). Same reply contract.
    CopyPatchMulti { paths: Vec<String> },
    /// Load `path` at `rev` for the read-only inspect overlay (`mode` = its content as a Source
    /// or its diff vs the working file), replying with `Msg::InspectLoaded` under `title`. A
    /// navigation-class read (coalesced latest-wins).
    Inspect { rev: String, path: String, title: String, mode: crate::view_state::InspectMode, base: String },
    /// Enumerate a "Compare with..." / "Show History" picker's option list off-thread (`kind`
    /// for `path`), replying with `Msg::PickListLoaded`. `mode` and `epoch` ride along (echoed
    /// into the reply): the picker knows which inspect to park, and a pre-navigation reply is
    /// dropped instead of popping a late modal. Navigation-class (latest-wins).
    PickList { kind: crate::view_state::PickKind, path: String, mode: crate::view_state::InspectMode, epoch: u64 },
    /// Enumerate the repo's remotes off-thread (`git remote`), replying with
    /// `Msg::RemotesLoaded` (echoing `epoch`). A read-only one-shot for the Manage Remotes dialog.
    Remotes { epoch: u64 },
    /// Enumerate the repo's branches/tags off-thread (`git`), replying with `Msg::RefListLoaded`.
    /// `op` and `epoch` ride along (echoed) so the picker knows checkout/merge/rebase and a
    /// pre-navigation reply is dropped.
    RefList { op: crate::view_state::RefOp, epoch: u64 },
    /// Blame `path` at `rev` for the per-line gutter (View > Blame), replying with
    /// `Msg::BlameLoaded`. A navigation-class read (coalesced latest-wins).
    Blame { rev: String, path: String },
    /// Recompute the working-tree change signature (the runtime's periodic external-change
    /// poll), replying with `Msg::StatusPolled`. Coalesced latest-wins and served LAST -
    /// a background tick must never delay a user-visible read.
    StatusPoll,
}

/// The per-class latest-wins slots a burst of requests collapses into. Draining
/// the channel into these keeps only the NEWEST navigation request of each class,
/// so fast navigation never stacks stale Detail/Tree/Preview loads, yet no class is
/// lost. One-shot ACTIONS (revert/save/hunk/git write/one-shot reads) are never
/// coalesced AND share ONE queue preserving ARRIVAL order across classes: serving
/// reverts-then-saves by class would let a stale autosave queued BEFORE a rollback
/// run AFTER it and silently rewrite the file the user just reverted.
#[derive(Default)]
struct Pending {
    detail: Option<Req>,
    tree: Option<Req>,
    open: Option<Req>,
    inspect: Option<Req>,
    blame: Option<Req>,
    picklist: Option<Req>,
    reload: bool,
    load_more: bool,
    status_poll: bool,
    actions: Vec<Req>,
}

impl Pending {
    /// Slot `req` into its class, overwriting any older NAVIGATION request of that
    /// class. An action is appended to the one ordered queue (never collapsed).
    fn insert(&mut self, req: Req) {
        match req {
            Req::Detail(_) => self.detail = Some(req),
            Req::Tree { .. } => self.tree = Some(req),
            Req::OpenFile { .. } => self.open = Some(req),
            Req::Inspect { .. } => self.inspect = Some(req),
            Req::Blame { .. } => self.blame = Some(req),
            Req::PickList { .. } => self.picklist = Some(req),
            Req::Reload => self.reload = true,
            Req::LoadMore => self.load_more = true,
            Req::StatusPoll => self.status_poll = true,
            Req::Revert { .. }
            | Req::SaveFile { .. }
            | Req::RevertHunk { .. }
            | Req::Git(_)
            | Req::CopyPatch { .. }
            | Req::CopyPatchMulti { .. }
            | Req::Remotes { .. }
            | Req::RefList { .. } => self.actions.push(req),
        }
    }

    /// Take the requests to serve: every queued action first (one queue, arrival
    /// order), then any reload, then the latest open/detail/tree navigation
    /// request of each class.
    fn drain(&mut self) -> Vec<Req> {
        let mut out = std::mem::take(&mut self.actions);
        if std::mem::take(&mut self.reload) {
            out.push(Req::Reload);
        }
        if std::mem::take(&mut self.load_more) {
            out.push(Req::LoadMore);
        }
        out.extend(
            [
                self.open.take(),
                self.inspect.take(),
                self.blame.take(),
                self.picklist.take(),
                self.detail.take(),
                self.tree.take(),
            ]
            .into_iter()
            .flatten(),
        );
        // The background poll goes LAST: freshness-by-2s must never queue ahead of a
        // read the user is waiting on.
        if std::mem::take(&mut self.status_poll) {
            out.push(Req::StatusPoll);
        }
        out
    }
}

/// Spawn the loader worker over `backend`, returning the request channel.
///
/// The worker first performs the INITIAL load - `load_repo` -> [`build_repo_model`]
/// -> [`Msg::RepoLoaded`] (or [`Msg::BackendError`] on failure) - then loops on the
/// returned [`Req`] receiver until the runtime drops the sender (quit). Every reply
/// is owned data sent over `tx`; the worker is the sole git-IO owner.
pub fn spawn_loader(backend: Arc<dyn GitBackend + Send + Sync>, tx: Sender<Msg>) -> Sender<Req> {
    let (req_tx, req_rx) = mpsc::channel::<Req>();
    thread::spawn(move || {
        tx.send(initial_load(backend.as_ref())).ok();
        serve_requests(backend.as_ref(), &req_rx, &tx);
    });
    req_tx
}

/// The INITIAL load message: the built repo model, or a backend error.
fn initial_load(backend: &dyn GitBackend) -> Msg {
    match backend.load_repo() {
        Ok(snapshot) => Msg::RepoLoaded(Box::new(build_repo_model(snapshot))),
        Err(e) => Msg::BackendError(e.to_string()),
    }
}

/// Serve requests until the sender is dropped. Each turn blocks for one request,
/// then drains every other queued request into the per-class latest-wins
/// [`Pending`] slots (load-shedding) and serves the surviving requests in order,
/// pushing each owned reply over `tx`.
fn serve_requests(backend: &dyn GitBackend, req_rx: &Receiver<Req>, tx: &Sender<Msg>) {
    while let Ok(first) = req_rx.recv() {
        let mut pending = Pending::default();
        pending.insert(first);
        while let Ok(next) = req_rx.try_recv() {
            pending.insert(next); // collapse a burst to the newest of each class.
        }
        for req in pending.drain() {
            tx.send(serve_one(backend, req)).ok();
        }
    }
}

/// Serve one (already coalesced) request, producing its owned reply message.
fn serve_one(backend: &dyn GitBackend, req: Req) -> Msg {
    match req {
        Req::Detail(hash) => match backend.commit_detail(&hash) {
            Ok(detail) => Msg::DetailLoaded { hash, detail },
            Err(e) => read_failed("Load commit detail", e),
        },
        Req::Tree { hash, all } => {
            let loaded = if all {
                backend.full_tree(&hash)
            } else {
                backend.changed_files(&hash)
            };
            match loaded {
                // Ignored paths only matter in the All view; the changed-only tree
                // never lists ignored files, so skip the extra walk for it.
                Ok(tree) => {
                    let ignored = if all {
                        backend.ignored_paths(&hash).unwrap_or_default()
                    } else {
                        std::collections::HashSet::new()
                    };
                    Msg::TreeLoaded { hash, tree, ignored }
                }
                Err(e) => read_failed("Load files", e),
            }
        }
        Req::OpenFile { commit, path } => match backend.open_file(&commit, &path) {
            // Editable: feed the live diff with raw text (highlighted later on save/
            // recompute is unnecessary - the live diff uses plain tokens).
            Ok(crate::backend::OpenFile::Editable { base, work }) => {
                Msg::EditFileLoaded { commit, path, base, work }
            }
            // Read-only: the historical/binary view, syntax-highlighted like before.
            Ok(crate::backend::OpenFile::ReadOnly(view)) => Msg::PreviewLoaded {
                commit,
                path,
                view: view.map(tokenize::highlight),
            },
            Err(e) => read_failed("Open file", e),
        },
        Req::Revert { commit, paths } => revert_batch(backend, &commit, &paths),
        Req::Reload => match backend.load_repo() {
            Ok(snapshot) => Msg::RepoLoaded(Box::new(build_repo_model(snapshot))),
            // A failed RE-load must stay visible on the populated repo it reloads.
            Err(e) => read_failed("Reload", e),
        },
        Req::LoadMore => match backend.load_more() {
            Ok(snapshot) => Msg::RepoLoaded(Box::new(build_repo_model(snapshot))),
            Err(e) => read_failed("Load more history", e),
        },
        Req::SaveFile { path, content } => match backend.write_worktree(&path, &content) {
            Ok(()) => Msg::FileSaved { path },
            // A silent failed save reads as saved - the worst silent no-op of all.
            Err(e) => read_failed("Save", e),
        },
        Req::RevertHunk { commit, path, hunk } => match backend.revert_hunk(&commit, &path, hunk) {
            Ok(_) => Msg::HunkReverted {
                summary: format!("Reverted a hunk in {path}"),
            },
            Err(e) => read_failed("Revert hunk", e),
        },
        Req::Git(action) => git_action(backend, action),
        Req::CopyPatch { file } => match backend.working_patch(&file) {
            // A non-empty diff goes to the clipboard; an empty one (a tracked file with no
            // changes - the menu never offers this) and any git error surface as a Notice.
            Ok(text) if !text.is_empty() => Msg::PatchCopied { text },
            Ok(_) => Msg::GitActionDone { summary: format!("No local changes in {file}"), reload: false },
            Err(e) => Msg::GitActionDone { summary: e.to_string(), reload: false },
        },
        Req::CopyPatchMulti { paths } => match backend.working_patch_multi(&paths) {
            Ok(text) if !text.is_empty() => Msg::PatchCopied { text },
            Ok(_) => Msg::GitActionDone {
                summary: "No local changes in the selected files".to_string(),
                reload: false,
            },
            Err(e) => Msg::GitActionDone { summary: e.to_string(), reload: false },
        },
        Req::Inspect { rev, path, title, mode, base } => {
            use crate::view_state::InspectMode;
            // Source = the file's blob at `rev`; Compare = `rev` diffed against `base` (the live
            // working tree on <current>, or a commit's blob on a historical row).
            let loaded = match mode {
                InspectMode::Source => backend.revision_source(&rev, &path),
                InspectMode::Compare => backend.compare_view(&base, &rev, &path),
                // CommitDiff = what `rev` changed to the file: its blob vs its parent's.
                InspectMode::CommitDiff => backend.file_view(&rev, &path),
                // Blame = `rev`'s file annotated per line (WORKING_REV = the live working tree).
                InspectMode::Blame => backend.blame(&rev, &path),
            };
            match loaded {
                // Highlight it like any read-only view (mirrors the OpenFile read-only arm); a
                // None view (path absent at that rev) reaches `apply` as a Notice, not an
                // overlay. Echo `path` so a stale reply (the user navigated away) is dropped.
                Ok(view) => Msg::InspectLoaded { title, path, view: view.map(tokenize::highlight) },
                Err(e) => read_failed("Inspect", e),
            }
        }
        Req::PickList { kind, path, mode, epoch } => {
            use crate::view_state::{PickItem, PickKind};
            // Enumerate the picker's options (file revisions or refs) as (rev, label) pairs.
            let listed = match kind {
                PickKind::FileRevisions => backend.file_revisions(&path),
                PickKind::Refs => backend.list_refs(),
            };
            match listed {
                Ok(pairs) => {
                    let items = pairs
                        .into_iter()
                        .map(|(rev, label)| PickItem { rev, label })
                        .collect();
                    Msg::PickListLoaded { kind, path, items, mode, epoch }
                }
                Err(e) => read_failed("Load picker", e),
            }
        }
        Req::Blame { rev, path } => match backend.blame(&rev, &path) {
            // Blame returns a Blame FileView; lift out the per-line data for the gutter (a
            // missing path / non-blame view just drops the gutter, not an error banner).
            Ok(Some(crate::diff::FileView::Blame(blame))) => Msg::BlameLoaded { rev, path, blame },
            Ok(_) => Msg::BlameLoaded { rev, path, blame: crate::diff::BlameFile { path: String::new(), lines: Vec::new() } },
            Err(e) => read_failed("Blame", e),
        },
        Req::Remotes { epoch } => match backend.remote_list() {
            Ok(remotes) => Msg::RemotesLoaded { remotes, epoch },
            Err(e) => read_failed("List remotes", e),
        },
        Req::RefList { op, epoch } => match backend.list_refs() {
            Ok(pairs) => {
                let items = pairs
                    .into_iter()
                    .map(|(rev, label)| crate::view_state::PickItem { rev, label })
                    .collect();
                Msg::RefListLoaded { op, items, epoch }
            }
            Err(e) => read_failed("Load branches", e),
        },
        // A failed poll is `None`, NOT a ReqFailed notice: at a 2s cadence a transient
        // index.lock (an external git op in progress) would spam the toolbar, and the
        // next tick self-heals. The deliberate exception to the no-silent-failures rule.
        Req::StatusPoll => Msg::StatusPolled { sig: backend.status_sig().ok() },
    }
}

/// A per-request failure on a (presumably) populated repo: `Status::Error` only
/// renders over an EMPTY repo, so route it to the transient-Notice surface instead -
/// a failed read/save must never be a silent no-op. (`Msg::BackendError` stays
/// reserved for the initial boot load, where the empty-repo placeholder shows it.)
fn read_failed(what: &'static str, e: crate::backend::BackendError) -> Msg {
    Msg::ReqFailed { what, error: e.to_string() }
}

/// Execute a repo-level git write, mapping its result to a `Msg`. A commit/amend/tag/
/// pull changed the log or working tree, so `reload: true` re-fetches the repo; a push
/// leaves local history unchanged (`reload: false`). An error surfaces as a status.
fn git_action(backend: &dyn GitBackend, action: crate::view_state::GitAction) -> Msg {
    use crate::view_state::GitAction;
    let (result, reload) = match action {
        GitAction::Commit(m) => (backend.commit(&m), true),
        GitAction::Amend(m) => (backend.amend(&m), true),
        GitAction::Tag(n) => (backend.tag(&n), true),
        GitAction::Push => (backend.push(), true),
        GitAction::Pull => (backend.pull(), true),
        GitAction::PullStrategy { rebase } => (backend.pull_mode(rebase), true),
        GitAction::BranchAt { name, commit, checkout } => {
            (backend.branch_create(&name, &commit, checkout), true)
        }
        GitAction::TagAt { name, commit } => (backend.tag_at(&name, &commit), true),
        GitAction::RewordAt { commit, message } => (backend.reword_at(&commit, &message), true),
        GitAction::Checkout { commit } => (backend.checkout(&commit), true),
        GitAction::CherryPick { commit } => (backend.cherry_pick(&commit), true),
        GitAction::RevertCommit { commit } => (backend.revert_commit(&commit), true),
        // Unpack the store-layer ResetMode into the primitive `git reset` flag so the
        // backend stays free of the view_state vocabulary.
        GitAction::ResetTo { commit, mode } => (backend.reset(&commit, mode.flag()), true),
        GitAction::UndoCommit => (backend.undo_commit(), true),
        // Working-tree resets: both empty the working changes, so reload the tree + <current>.
        GitAction::Stash => (backend.stash(), true),
        GitAction::DiscardAll => (backend.discard_all(), true),
        // Fetch only updates remote-tracking refs (reload so unpushed markers refresh);
        // unstash re-applies working changes (reload the tree + <current>).
        GitAction::Fetch => (backend.fetch(), true),
        // Update integrates remote commits into the working branch -> reload the graph + tree.
        GitAction::UpdateProject => (backend.update_project(), true),
        GitAction::Unstash => (backend.unstash(), true),
        // Writing a patch file changes neither the graph nor the working tree -> no reload.
        GitAction::CreatePatch { commit, path } => (backend.create_patch(&commit, &path), false),
        GitAction::CreateWorkingPatch { file, path } => {
            (backend.create_working_patch(&file, &path), false)
        }
        GitAction::CreatePatchSelected { paths, path } => {
            (backend.create_working_patch_multi(&paths, &path), false)
        }
        // Archiving the repo writes a zip; it touches neither the graph nor the tree -> no reload.
        GitAction::ArchiveProject { rev, path } => (backend.archive_project(&rev, &path), false),
        // Per-file commit moves HEAD; per-file delete changes the working tree - both reload.
        GitAction::CommitFile { file, message } => (backend.commit_file(&file, &message), true),
        GitAction::CommitFolder { dir, message } => (backend.commit_dir(&dir, &message), true),
        GitAction::DeleteFile { file } => (backend.delete_file(&file), true),
        // Marked-set commit moves HEAD; marked-set delete changes the tree - both reload.
        GitAction::CommitSelected { paths, message } => (backend.commit_paths(&paths, &message), true),
        GitAction::DeleteSelected { paths } => (backend.delete_paths(&paths), true),
        // Cherry-pick moves HEAD (reload); the patch series only writes files (no reload).
        GitAction::CherryPickSelected { commits } => (backend.cherry_pick_multi(&commits), true),
        GitAction::CreatePatchSeries { commits, dir } => (backend.create_patch_series(&commits, &dir), false),
        // Unpack the store-layer RebaseAction into its primitive git todo-verb so the
        // backend stays free of the view_state vocabulary (mirrors ResetMode -> flag).
        GitAction::RebaseTodo { base, ops } => {
            let ops: Vec<(String, String)> =
                ops.into_iter().map(|(hash, action)| (hash, action.label().to_string())).collect();
            (backend.rebase_todo(&base, &ops), true)
        }
        GitAction::CheckoutRef { name } => (backend.checkout_ref(&name), true),
        GitAction::MergeRef { name } => (backend.merge_ref(&name), true),
        GitAction::RebaseOnto { name } => (backend.rebase_onto(&name), true),
        GitAction::BranchRename { old, new } => (backend.branch_rename(&old, &new), true),
        GitAction::BranchDelete { name } => (backend.branch_delete(&name, false), true),
        GitAction::TagDelete { name } => (backend.tag_delete(&name), true),
        // Push changes no local graph state, but the ref's pushed/unpushed status flips
        // (hollow -> filled node), so reload to refresh the decoration.
        GitAction::PushRef { name } => (backend.push_ref(&name), true),
        GitAction::PullRef { remote, branch, rebase } => {
            (backend.pull_ref(&remote, &branch, rebase), true)
        }
        // Remote config edits touch neither the graph nor the working tree, but Fetch/Push
        // node decorations read the remote set, so reload to refresh them.
        GitAction::RemoteAdd { name, url } => (backend.remote_add(&name, &url), true),
        GitAction::RemoteRemove { name } => (backend.remote_remove(&name), true),
        GitAction::RemoteSetUrl { name, url } => (backend.remote_set_url(&name, &url), true),
        // Applying a patch mutates the working tree -> reload the tree + <current>.
        GitAction::ApplyPatch { path } => (backend.apply_patch(&path), true),
    };
    // Surface BOTH outcomes through GitActionDone (a status Notice): a `BackendError`
    // is only visible on an empty repo, so a failed push/commit over a populated repo
    // would otherwise vanish. On error, don't reload (nothing changed).
    match result {
        Ok(summary) => Msg::GitActionDone { summary, reload },
        Err(e) => Msg::GitActionDone { summary: e.to_string(), reload: false },
    }
}

/// Revert `commit`'s change to each of `paths` in the working tree, COLLECTING the
/// paths it successfully reverted (in order). The FIRST error stops the batch; the
/// already-reverted paths are still returned so `apply` prunes exactly the rows that
/// were actually reverted (a partial revert removes only its successful files). The
/// `summary` reports the count (and the error on a partial run); `apply` then prunes
/// those rows, clears the marks, and sets the status. The destructive write happens
/// here, never in `apply`.
fn revert_batch(backend: &dyn GitBackend, commit: &str, paths: &[String]) -> Msg {
    let mut reverted: Vec<String> = Vec::with_capacity(paths.len());
    let mut error: Option<String> = None;
    for path in paths {
        match backend.revert_file(commit, path) {
            Ok(_) => reverted.push(path.clone()),
            Err(e) => {
                error = Some(e.to_string());
                break;
            }
        }
    }
    let done = reverted.len();
    let total = paths.len();
    let summary = match error {
        // Partial run: the noun agrees with the DENOMINATOR (the total), which the
        // fraction makes plural unless there was a single target - "Reverted 1/3 files".
        Some(e) => {
            let noun = if total == 1 { "file" } else { "files" };
            format!("Reverted {done}/{total} {noun}; revert failed: {e}")
        }
        // Full run: the noun agrees with the count actually reverted.
        None => {
            let noun = if done == 1 { "file" } else { "files" };
            format!("Reverted {done} {noun}")
        }
    };
    Msg::RevertDone {
        paths: reverted,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendError, RepoSnapshot, RevertOutcome};
    use crate::model::CommitDetail;

    /// A no-git mock backend: `revert_file` succeeds for every path EXCEPT the ones
    /// in `fail`, where it errors with `unsafe revert path: <path>`. Only the revert
    /// method is exercised; the rest of the trait is unreachable in these tests.
    struct MockRevert {
        fail: Vec<String>,
    }

    impl GitBackend for MockRevert {
        fn load_repo(&self) -> Result<RepoSnapshot, BackendError> {
            unreachable!("not exercised by the revert grammar tests")
        }
        fn commit_detail(&self, _hash: &str) -> Result<CommitDetail, BackendError> {
            unreachable!("not exercised by the revert grammar tests")
        }
        fn file_view(&self, _c: &str, _p: &str) -> Result<Option<crate::diff::FileView>, BackendError> {
            unreachable!("not exercised by the revert grammar tests")
        }
        fn revert_file(&self, _commit: &str, path: &str) -> Result<RevertOutcome, BackendError> {
            if self.fail.iter().any(|f| f == path) {
                Err(BackendError(format!("unsafe revert path: {path}")))
            } else {
                Ok(RevertOutcome::Overwritten(path.to_string()))
            }
        }
    }

    fn summary_of(msg: &Msg) -> &str {
        match msg {
            Msg::RevertDone { summary, .. } => summary,
            other => panic!("expected RevertDone, got {other:?}"),
        }
    }

    #[test]
    fn revert_batch_partial_failure_pluralizes_with_the_total() {
        // 3 targets, the middle one errors -> the batch stops after 1 success. The
        // noun must agree with the fraction's DENOMINATOR (3 -> plural "files"), not
        // the numerator: "Reverted 1/3 files; revert failed: ...".
        let backend = MockRevert { fail: vec!["../escape".to_string()] };
        let paths = vec!["a.go".to_string(), "../escape".to_string(), "b.go".to_string()];
        let msg = revert_batch(&backend, "deadbeef", &paths);
        assert_eq!(
            summary_of(&msg),
            "Reverted 1/3 files; revert failed: unsafe revert path: ../escape",
            "partial-failure noun agrees with the plural total"
        );
        // Only the pre-error successes are reported as reverted.
        match &msg {
            Msg::RevertDone { paths, .. } => assert_eq!(paths, &vec!["a.go".to_string()]),
            other => panic!("expected RevertDone, got {other:?}"),
        }
    }

    #[test]
    fn revert_batch_full_run_noun_agrees_with_the_count() {
        // Single success -> "Reverted 1 file"; multiple -> "Reverted N files".
        let backend = MockRevert { fail: vec![] };
        let one = revert_batch(&backend, "c", &["only.go".to_string()]);
        assert_eq!(summary_of(&one), "Reverted 1 file");
        let two = revert_batch(&backend, "c", &["a.go".to_string(), "b.go".to_string()]);
        assert_eq!(summary_of(&two), "Reverted 2 files");
    }

    #[test]
    fn drain_preserves_arrival_order_across_action_classes() {
        // An autosave queued BEFORE a rollback of the same file must run BEFORE it:
        // the old by-class drain (reverts, then saves, then git) hoisted the revert
        // ahead of the earlier save, so the stale save resurrected the reverted file.
        let mut pending = Pending::default();
        pending.insert(Req::SaveFile { path: "a.txt".to_string(), content: "edited".to_string() });
        pending.insert(Req::Revert { commit: "c".to_string(), paths: vec!["a.txt".to_string()] });
        pending.insert(Req::Git(crate::view_state::GitAction::Push));
        pending.insert(Req::SaveFile { path: "b.txt".to_string(), content: "x".to_string() });
        let order: Vec<&'static str> = pending
            .drain()
            .iter()
            .map(|r| match r {
                Req::SaveFile { .. } => "save",
                Req::Revert { .. } => "revert",
                Req::Git(_) => "git",
                _ => "other",
            })
            .collect();
        assert_eq!(order, ["save", "revert", "git", "save"], "actions serve in arrival order");
    }

    #[test]
    fn the_status_poll_drains_last() {
        // The background freshness tick must never queue ahead of a read or write the
        // user is waiting on, regardless of arrival order.
        let mut pending = Pending::default();
        pending.insert(Req::StatusPoll);
        pending.insert(Req::Detail("h".to_string()));
        pending.insert(Req::SaveFile { path: "a.txt".to_string(), content: "x".to_string() });
        let reqs = pending.drain();
        assert_eq!(reqs.len(), 3);
        assert!(matches!(reqs.last(), Some(Req::StatusPoll)), "poll served last");
    }

    #[test]
    fn revert_batch_single_target_failure_stays_singular() {
        // A 1/1 failure has a singular denominator -> "Reverted 0/1 file".
        let backend = MockRevert { fail: vec!["x.go".to_string()] };
        let msg = revert_batch(&backend, "c", &["x.go".to_string()]);
        assert_eq!(
            summary_of(&msg),
            "Reverted 0/1 file; revert failed: unsafe revert path: x.go"
        );
    }
}
