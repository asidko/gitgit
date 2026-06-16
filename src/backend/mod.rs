//! The ONLY git boundary.
//!
//! Everything behind this seam speaks owned `model`/`diff` types; nothing here
//! touches the render/terminal/view-state/Msg/highlight layers. A [`GitBackend`] yields a
//! [`RepoSnapshot`] (raw rows + parents) which [`build_repo_model`] turns into the
//! UI's [`RepoModel`] - the ONE place the pure graph layout runs and the ONE place
//! `is_me` is computed from the explicit current user. Swapping the data source
//! (fixtures today, libgit2 later) is a backend swap, never a UI or store rewrite.

use std::error::Error;
use std::fmt;

use crate::diff::FileView;
use crate::graph_engine;
use crate::model::{Commit, CommitDetail, RepoModel, TreeNode, WORKING_REV};

pub mod build;
mod fixtures;
mod git;

// The concrete impls stay crate-internal; only the trait + snapshot + builder are
// the public surface. The fixture bootstrap picks `FixtureBackend`; the runtime
// loader + the `--real` snapshot pick `RealBackend`. Both are reached only from
// the crate's composition roots in `lib.rs`, never from `ui`/`store`.
pub(crate) use fixtures::FixtureBackend;
pub(crate) use git::RealBackend;

/// Raw repository data a backend returns, before any UI-side derivation. Holds
/// only owned domain types; the graph layout and `is_me` are NOT precomputed here
/// (that is [`build_repo_model`]'s job, so there is one derivation site).
pub struct RepoSnapshot {
    /// Commit rows, newest first. Each carries its parent short hashes.
    pub commits: Vec<Commit>,
    /// Changed-files tree for the default selection.
    pub tree: Vec<TreeNode>,
    /// Flattened-tree index to select on startup.
    pub default_selection: usize,
    /// The logged-in user; their commits render bold. An EXPLICIT field so the
    /// "am I the author" decision has a single, testable input (no global const).
    pub current_user: String,
    /// Full hashes of commits not yet on any remote-tracking branch (drawn with a
    /// hollow graph node). Empty when the repo has no remotes.
    pub unpushed: std::collections::HashSet<String>,
    /// Whether the repo has any configured remote (so `unpushed` being empty means
    /// "all pushed" rather than "nowhere to push").
    pub has_remotes: bool,
    /// The commit walk hit the cap with MORE history beyond it - drives the log's trailing
    /// "Load more history" row. `false` when the whole reachable history fit in the cap.
    pub truncated: bool,
    /// Signature of (HEAD oid + working-tree statuses) AT load time. The runtime's
    /// periodic [`GitBackend::status_sig`] poll compares against this: a mismatch means
    /// the tree changed externally and the panes refresh. MUST be computed by the same
    /// function the poll uses, or every poll would re-trigger a reload.
    pub status_sig: u64,
}

/// What a single-file revert did to the working tree, for the aggregated summary.
/// `Demo` is the fixture backend's no-op (it never touches disk).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevertOutcome {
    /// The file was MODIFIED in the commit; the working tree now holds the parent's
    /// content (the path is carried for the summary).
    Overwritten(String),
    /// The file was ADDED in the commit (absent in the parent); it was DELETED.
    Deleted(String),
    /// The file was DELETED in the commit (present in the parent); it was RESTORED.
    Restored(String),
    /// The fixture backend's no-op outcome: nothing was written to disk.
    Demo,
}

/// The result of opening a file for the viewer: an editable working buffer (the
/// common case) or a read-only view (binary / no working copy). Owned data only.
#[derive(Clone, Debug)]
pub enum OpenFile {
    /// The working copy is editable text: `base` = the commit blob (`None` if absent),
    /// `work` = the current working-tree text (the live-editable right side).
    Editable { base: Option<String>, work: String },
    /// Not editable: the historical/binary view to show read-only (`None` = empty).
    ReadOnly(Option<FileView>),
}

/// A backend failure carrying a human-readable message. The `.0` string feeds
/// [`crate::message::Msg::BackendError`] verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendError(pub String);

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for BackendError {}

/// The git data source. A single trait object the store/loader holds; the fixture
/// and real (libgit2) impls are interchangeable. `Send + Sync` so a later loader
/// can call it off the UI thread. Returns owned `model`/`diff` types ONLY.
pub trait GitBackend: Send + Sync {
    /// Load the whole repository snapshot (commits + default tree + user).
    fn load_repo(&self) -> Result<RepoSnapshot, BackendError>;

    /// Re-load with a DEEPER commit cap (the log's "Load more history" row). Default = a plain
    /// reload (a fixed-history backend has nothing more to page in).
    fn load_more(&self) -> Result<RepoSnapshot, BackendError> {
        self.load_repo()
    }

    /// Cheap signature of (HEAD oid + working-tree statuses), polled periodically by the
    /// runtime; a value differing from the loaded snapshot's [`RepoSnapshot::status_sig`]
    /// triggers a repo refresh so the panes track external changes. Default = constant
    /// (a fixture backend never changes under the app).
    fn status_sig(&self) -> Result<u64, BackendError> {
        Ok(0)
    }

    /// Full detail for one commit (committer, containing branches, ...), keyed by
    /// short hash. `git branch --contains` is folded in here, not a separate call.
    fn commit_detail(&self, hash: &str) -> Result<CommitDetail, BackendError>;

    /// The diff/source preview for `path` at `commit`. `Ok(None)` -> the file has
    /// no previewable content (the viewer shows its empty state).
    fn file_view(&self, commit: &str, path: &str) -> Result<Option<FileView>, BackendError>;

    /// `path`'s CONTENT at revision `rev` as a read-only source view (the inspect overlay's
    /// "Show Current Revision"). Distinct from `file_view` (which diffs parent-vs-commit):
    /// this is the file AS IT IS at that single revision. `Ok(None)` -> the path does not
    /// exist there. The default errors (test stub).
    fn revision_source(&self, rev: &str, path: &str) -> Result<Option<FileView>, BackendError> {
        let _ = (rev, path);
        Err(BackendError("revision source not supported by this backend".to_string()))
    }

    /// `path` at `rev` diffed against `base` (the Compare-with-Revision/Branch overlay): a
    /// read-only `Diff` labelled `<rev>` vs the base. `base` == [`crate::model::WORKING_REV`]
    /// diffs against the live working tree (labelled "working"); any other `base` diffs against
    /// that commit's blob (the historical-row compare). `Ok(None)` -> no difference (or the path
    /// is absent at `rev`). The default errors (test stub).
    fn compare_view(&self, base: &str, rev: &str, path: &str) -> Result<Option<FileView>, BackendError> {
        let _ = (base, rev, path);
        Err(BackendError("compare not supported by this backend".to_string()))
    }

    /// `path` annotated with per-line git blame at `rev` (Annotate with Git Blame): a read-only
    /// `Blame` view. `rev` == [`crate::model::WORKING_REV`] blames the live working tree (so an
    /// uncommitted line reads "Not Committed Yet"); any other rev blames the file at that commit.
    /// `Ok(None)` -> the path is untracked / absent there. The default returns `None` (test stub).
    fn blame(&self, rev: &str, path: &str) -> Result<Option<FileView>, BackendError> {
        let _ = (rev, path);
        Ok(None)
    }

    /// The revisions that TOUCHED `path` (its history), newest first, as `(short_hash, label)`
    /// where label is `hash  date  subject` (the date prefixes the subject so same-subject
    /// commits stay distinguishable). Drives the Compare-with-Revision picker. The default
    /// returns an empty list (test stub).
    fn file_revisions(&self, path: &str) -> Result<Vec<(String, String)>, BackendError> {
        let _ = path;
        Ok(Vec::new())
    }

    /// The repo's branches + tags as `(ref_name, label)` (label = `name (branch|tag)`), for the
    /// Compare-with-Branch/Tag picker. The default returns an empty list (test stub).
    fn list_refs(&self) -> Result<Vec<(String, String)>, BackendError> {
        Ok(Vec::new())
    }

    /// The changed-files tree for commit `hash` (its diff against the mainline
    /// parent). The lazy per-commit tree the files pane follows as the log
    /// selection moves: `load_repo` seeds the default tree, then a later
    /// `Msg::TreeLoaded` swaps in this result for the newly-selected commit. The
    /// default returns an empty tree so a backend without per-commit trees (a test
    /// stub) still satisfies the trait; the real backend diffs the commit.
    fn changed_files(&self, hash: &str) -> Result<Vec<TreeNode>, BackendError> {
        let _ = hash;
        Ok(Vec::new())
    }

    /// The FULL file tree of commit `hash` (every blob the commit's tree holds),
    /// with each changed path carrying its [`crate::model::FileStatus`] (Added/
    /// Modified/Deleted vs the mainline parent[0]) and every other file marked
    /// [`crate::model::FileStatus::Unchanged`]. Powers the files pane's "All"
    /// toggle: OFF (default) follows [`changed_files`]; ON follows this. The
    /// default returns an empty tree so a backend without per-commit trees (a test
    /// stub) still satisfies the trait; the real backend walks the commit tree.
    fn full_tree(&self, hash: &str) -> Result<Vec<TreeNode>, BackendError> {
        let _ = hash;
        Ok(Vec::new())
    }

    /// Full paths in commit `hash`'s tree that match a `.gitignore` rule - tracked or
    /// force-added files that the ignore rules would otherwise exclude. The All-files
    /// view renders these faint. The default returns an empty set (a stub/test backend
    /// has no ignore rules); only the real backend evaluates `.gitignore`.
    fn ignored_paths(&self, hash: &str) -> Result<std::collections::HashSet<String>, BackendError> {
        let _ = hash;
        Ok(std::collections::HashSet::new())
    }

    /// Undo `commit`'s change to `path` IN THE WORKING TREE (GoLand "Revert
    /// Selected Changes" semantics, whole-file vs the mainline parent[0]):
    /// - MODIFIED in the commit -> overwrite the working file with the parent blob.
    /// - ADDED in the commit (absent in parent) -> delete it from the working tree.
    /// - DELETED in the commit (present in parent) -> restore the parent blob.
    ///
    /// DESTRUCTIVE: it writes the user's working tree (NOT the index, NOT a commit).
    /// MUST run only in the loader/backend, never in `apply`. The default errors so
    /// a backend without working-tree access (a test stub) still satisfies the trait.
    fn revert_file(&self, commit: &str, path: &str) -> Result<RevertOutcome, BackendError> {
        let _ = (commit, path);
        Err(BackendError("revert not supported by this backend".to_string()))
    }

    /// Revert ONE hunk of `commit`'s change to `path` in the WORKING TREE: the hunk
    /// at index `hunk` (the same ordering [`file_view`]'s diff lines carry) is
    /// reverse-applied to the on-disk file, undoing just that block while leaving the
    /// rest of the file as-is. DESTRUCTIVE working-tree write; loader-only. Errors if
    /// the hunk does not apply cleanly (the working file diverged from the commit).
    /// The default errors (test stub).
    fn revert_hunk(&self, commit: &str, path: &str, hunk: usize) -> Result<RevertOutcome, BackendError> {
        let _ = (commit, path, hunk);
        Err(BackendError("hunk revert not supported by this backend".to_string()))
    }

    /// Read the WORKING-TREE text of `path` (the on-disk file the editor edits, NOT a
    /// commit blob). Errors when the file is missing, escapes the working tree, or is
    /// not valid UTF-8 (the editor is text-only). The default errors so a backend
    /// without working-tree access (a test stub) still satisfies the trait.
    fn read_worktree(&self, path: &str) -> Result<String, BackendError> {
        let _ = path;
        Err(BackendError("editing not supported by this backend".to_string()))
    }

    /// The committed text of `path` at `commit` (the LEFT/base side of the live
    /// editable diff). `Ok(None)` -> the file is absent at that commit (the diff
    /// treats the base as empty). Errors on a non-UTF-8 blob. The default returns
    /// `None` (a test stub diffs against an empty base).
    fn read_commit_file(&self, commit: &str, path: &str) -> Result<Option<String>, BackendError> {
        let _ = (commit, path);
        Ok(None)
    }

    /// Open `path` (selected at `commit`) for the file viewer.
    ///
    /// Only the synthetic working row ([`WORKING_REV`]) is EDITABLE: its diff is the live
    /// working file ([`OpenFile::Editable`], `base` = HEAD blob, `work` = the working
    /// tree). A real (historical) commit is shown READ-ONLY as PARENT-vs-commit - what
    /// that commit changed - via [`OpenFile::ReadOnly`] / `file_view`, so the right side
    /// is the selected commit's blob, not the working tree. The default composes the
    /// other trait methods, so both backends share one policy.
    fn open_file(&self, commit: &str, path: &str) -> Result<OpenFile, BackendError> {
        if commit == WORKING_REV {
            if let Ok(work) = self.read_worktree(path) {
                return Ok(OpenFile::Editable {
                    base: self.read_commit_file(commit, path)?,
                    work,
                });
            }
        }
        Ok(OpenFile::ReadOnly(self.file_view(commit, path)?))
    }

    /// Write `content` to the WORKING-TREE file at `path` (the editor's save).
    /// DESTRUCTIVE: it overwrites the user's on-disk file. MUST run only in the
    /// loader/backend, never in `apply`. The default errors (test stub).
    fn write_worktree(&self, path: &str, content: &str) -> Result<(), BackendError> {
        let _ = (path, content);
        Err(BackendError("editing not supported by this backend".to_string()))
    }

    /// Stage all changes and commit with `message`. DESTRUCTIVE; loader-only. Returns a
    /// human-readable summary for the status line. The default errors (test stub).
    fn commit(&self, message: &str) -> Result<String, BackendError> {
        let _ = message;
        Err(BackendError("commit not supported by this backend".to_string()))
    }

    /// Stage all changes and amend HEAD with `message`. DESTRUCTIVE history rewrite;
    /// loader-only. The default errors (test stub).
    fn amend(&self, message: &str) -> Result<String, BackendError> {
        let _ = message;
        Err(BackendError("amend not supported by this backend".to_string()))
    }

    /// Create a lightweight tag `name` at HEAD. Loader-only. Default errors.
    fn tag(&self, name: &str) -> Result<String, BackendError> {
        let _ = name;
        Err(BackendError("tag not supported by this backend".to_string()))
    }

    /// Create a branch `name` at `commit`; `checkout` switches to it. Loader-only.
    /// Default errors.
    fn branch_create(&self, name: &str, commit: &str, checkout: bool) -> Result<String, BackendError> {
        let _ = (name, commit, checkout);
        Err(BackendError("branching not supported by this backend".to_string()))
    }

    /// Create a lightweight tag `name` at `commit`. Loader-only. Default errors.
    fn tag_at(&self, name: &str, commit: &str) -> Result<String, BackendError> {
        let _ = (name, commit);
        Err(BackendError("tag not supported by this backend".to_string()))
    }

    /// Reword `commit`'s message. HEAD -> message-only amend; an older commit ->
    /// rebase-reword (autostash, abort on conflict). DESTRUCTIVE history rewrite;
    /// loader-only. Default errors.
    fn reword_at(&self, commit: &str, message: &str) -> Result<String, BackendError> {
        let _ = (commit, message);
        Err(BackendError("reword not supported by this backend".to_string()))
    }

    /// Check out `commit` (`git checkout <commit>`): moves HEAD (detaches it on a raw
    /// hash) and the working tree. Git refuses when the switch would overwrite
    /// uncommitted changes. Loader-only. Default errors.
    fn checkout(&self, commit: &str) -> Result<String, BackendError> {
        let _ = commit;
        Err(BackendError("checkout not supported by this backend".to_string()))
    }

    /// Cherry-pick `commit` onto the current branch (`git cherry-pick <commit>`). A
    /// conflict aborts the pick so the repo never sticks mid-op. DESTRUCTIVE (adds a
    /// commit); loader-only. Default errors. Distinct from [`Self::revert_file`]
    /// (a working-tree-only file revert, not a history op).
    fn cherry_pick(&self, commit: &str) -> Result<String, BackendError> {
        let _ = commit;
        Err(BackendError("cherry-pick not supported by this backend".to_string()))
    }

    /// Cherry-pick the `commits` (oldest-first) onto the current branch in one op; a conflict
    /// aborts the whole series. Moves HEAD; loader-only. The multi-commit analog of
    /// [`cherry_pick`]. Default errors.
    fn cherry_pick_multi(&self, commits: &[String]) -> Result<String, BackendError> {
        let _ = commits;
        Err(BackendError("cherry-pick-multi not supported by this backend".to_string()))
    }

    /// Write the `commits` as a numbered patch SERIES (`git format-patch -o <dir>`) into `dir`.
    /// Read-only on the repo (writes only the patch FILES); loader-only. Default errors.
    fn create_patch_series(&self, commits: &[String], dir: &str) -> Result<String, BackendError> {
        let _ = (commits, dir);
        Err(BackendError("patch-series not supported by this backend".to_string()))
    }

    /// Revert `commit` with an inverse commit (`git revert --no-edit <commit>`). A
    /// conflict aborts the revert. DESTRUCTIVE (adds a commit); loader-only. Default
    /// errors. Distinct from [`Self::revert_file`] (a working-tree-only file revert).
    fn revert_commit(&self, commit: &str) -> Result<String, BackendError> {
        let _ = commit;
        Err(BackendError("revert not supported by this backend".to_string()))
    }

    /// Reset the current branch to `commit` with `mode_flag` (a `git reset` flag like
    /// `--soft`/`--hard`). DESTRUCTIVE (`--hard` discards the working tree); loader-only.
    /// Default errors. The flag is supplied by the loader from the store-layer mode.
    fn reset(&self, commit: &str, mode_flag: &str) -> Result<String, BackendError> {
        let _ = (commit, mode_flag);
        Err(BackendError("reset not supported by this backend".to_string()))
    }

    /// Undo the latest commit (`git reset --soft HEAD~1`): move HEAD back one, keeping
    /// its changes staged. DESTRUCTIVE history move; loader-only. Default errors.
    fn undo_commit(&self) -> Result<String, BackendError> {
        Err(BackendError("undo not supported by this backend".to_string()))
    }

    /// Stash the working tree (`git stash push --include-untracked`): set the uncommitted
    /// changes aside, leaving a clean tree (reversible via `git stash pop`). Loader-only.
    /// Default errors.
    fn stash(&self) -> Result<String, BackendError> {
        Err(BackendError("stash not supported by this backend".to_string()))
    }

    /// Discard all uncommitted changes to tracked files (`git reset --hard HEAD`).
    /// DESTRUCTIVE and not undoable; loader-only. Default errors.
    fn discard_all(&self) -> Result<String, BackendError> {
        Err(BackendError("discard not supported by this backend".to_string()))
    }

    /// Write `commit` as a patch to `path` (`git format-patch`). Read-only on the repo
    /// graph + working tree (it only writes the patch FILE); loader-only. Default errors.
    fn create_patch(&self, commit: &str, path: &str) -> Result<String, BackendError> {
        let _ = (commit, path);
        Err(BackendError("create-patch not supported by this backend".to_string()))
    }

    /// The local-changes unified diff of `file` (`git diff HEAD -- <file>`): the working
    /// tree vs its committed version, byte-exact (trailing newline kept so `git apply`
    /// accepts it). Empty when `file` has no changes vs HEAD. Read-only; loader-only.
    fn working_patch(&self, file: &str) -> Result<String, BackendError> {
        let _ = file;
        Err(BackendError("working-patch not supported by this backend".to_string()))
    }

    /// Write `file`'s local-changes diff to `path` (`git diff HEAD -- <file>` into the
    /// file). Read-only on the repo; loader-only. Default errors.
    fn create_working_patch(&self, file: &str, path: &str) -> Result<String, BackendError> {
        let _ = (file, path);
        Err(BackendError("create-working-patch not supported by this backend".to_string()))
    }

    /// Archive the repository AT `rev` to `path` (`git archive -o <path> <rev>`; the format -
    /// zip / tar / tar.gz - is read from `path`'s extension; `WORKING_REV` archives HEAD).
    /// Read-only on the repo (it only writes the archive FILE); loader-only. Default errors.
    fn archive_project(&self, rev: &str, path: &str) -> Result<String, BackendError> {
        let _ = (rev, path);
        Err(BackendError("archive-project not supported by this backend".to_string()))
    }

    /// Commit ONLY `file`'s working-tree changes (`git commit -m <message> -- <file>`),
    /// leaving the rest of the working tree uncommitted. Moves HEAD; loader-only. Errors
    /// (surfaced as a Notice) when the file has no changes. Default errors.
    fn commit_file(&self, file: &str, message: &str) -> Result<String, BackendError> {
        let _ = (file, message);
        Err(BackendError("commit-file not supported by this backend".to_string()))
    }

    /// Commit every working change UNDER `dir` (`git add --all -- <dir>` then `git commit -m
    /// <message> -- <dir>`), leaving the rest of the working tree uncommitted. Moves HEAD;
    /// loader-only. Errors (surfaced as a Notice) when nothing under `dir` changed. Default errors.
    fn commit_dir(&self, dir: &str, message: &str) -> Result<String, BackendError> {
        let _ = (dir, message);
        Err(BackendError("commit-dir not supported by this backend".to_string()))
    }

    /// Delete `file` from the working tree and git (`git rm` if tracked, else an fs remove
    /// for an untracked file). DESTRUCTIVE; loader-only. Default errors.
    fn delete_file(&self, file: &str) -> Result<String, BackendError> {
        let _ = file;
        Err(BackendError("delete-file not supported by this backend".to_string()))
    }

    /// Commit ONLY the selected `paths`' working-tree changes (`git add --all -- <paths>` then
    /// `git commit -m <message> -- <paths>`), leaving the rest uncommitted. Moves HEAD;
    /// loader-only. The multi-file analog of [`commit_dir`]. Default errors.
    fn commit_paths(&self, paths: &[String], message: &str) -> Result<String, BackendError> {
        let _ = (paths, message);
        Err(BackendError("commit-paths not supported by this backend".to_string()))
    }

    /// The local-changes diff of the selected `paths` as raw patch bytes (the multi-file analog
    /// of [`working_patch_bytes`]). Read-only; loader-only. Default errors.
    fn working_patch_bytes_multi(&self, paths: &[String]) -> Result<Vec<u8>, BackendError> {
        let _ = paths;
        Err(BackendError("working-patch-multi not supported by this backend".to_string()))
    }

    /// The selected `paths`' local-changes patch as a String (clipboard target; lossy UTF-8 is
    /// acceptable for a text clipboard). Read-only; loader-only.
    fn working_patch_multi(&self, paths: &[String]) -> Result<String, BackendError> {
        Ok(String::from_utf8_lossy(&self.working_patch_bytes_multi(paths)?).into_owned())
    }

    /// Write the selected `paths`' local-changes patch to `path`. Read-only on the repo (writes
    /// only the patch FILE); loader-only.
    fn create_working_patch_multi(&self, paths: &[String], path: &str) -> Result<String, BackendError> {
        let bytes = self.working_patch_bytes_multi(paths)?;
        if bytes.is_empty() {
            return Err(BackendError("no local changes in the selected files".to_string()));
        }
        std::fs::write(path, &bytes).map_err(|e| BackendError(format!("write {path}: {e}")))?;
        Ok(format!("Wrote {path}"))
    }

    /// Delete the selected `paths` from the working tree and git (per-file [`delete_file`]).
    /// DESTRUCTIVE; loader-only. Default errors.
    fn delete_paths(&self, paths: &[String]) -> Result<String, BackendError> {
        let _ = paths;
        Err(BackendError("delete-paths not supported by this backend".to_string()))
    }

    /// Interactive rebase of `base..HEAD` applying each `(full_hash, verb)` in `ops` to its
    /// todo line (`verb` in `drop`/`squash`/`fixup`; the loader maps the store's
    /// `RebaseAction` to this primitive string). A non-interactive `git rebase -i <base>^`
    /// rewrites those lines; a conflict aborts. DESTRUCTIVE history rewrite; loader-only.
    fn rebase_todo(&self, base: &str, ops: &[(String, String)]) -> Result<String, BackendError> {
        let _ = (base, ops);
        Err(BackendError("rebase not supported by this backend".to_string()))
    }

    /// Push to the configured upstream (system git: uses the user's remote + creds).
    /// Network IO, loader-only. Default errors.
    fn push(&self) -> Result<String, BackendError> {
        Err(BackendError("push not supported by this backend".to_string()))
    }

    /// Pull from the configured upstream (system git). Network IO, loader-only.
    /// Default errors.
    fn pull(&self) -> Result<String, BackendError> {
        Err(BackendError("pull not supported by this backend".to_string()))
    }

    /// Fetch from the configured remote without integrating (`git fetch`). Network IO,
    /// loader-only. Default errors.
    fn fetch(&self) -> Result<String, BackendError> {
        Err(BackendError("fetch not supported by this backend".to_string()))
    }

    /// One-click "Update Project": fetch then ff-only pull (rebase fallback on divergence),
    /// tolerating a local-only repo or an upstream-less branch (notice, never hard-fail).
    /// Network IO, loader-only. Default errors.
    fn update_project(&self) -> Result<String, BackendError> {
        Err(BackendError("update not supported by this backend".to_string()))
    }

    /// Pull the current branch with an integration strategy (`git pull` + `--ff-only` /
    /// `--no-rebase` / `--rebase`): `None` = ff-only, `Some(false)` = merge, `Some(true)` =
    /// rebase. A conflicting merge/rebase aborts so the repo never sticks. Network IO,
    /// loader-only. Default errors.
    fn pull_mode(&self, rebase: Option<bool>) -> Result<String, BackendError> {
        let _ = rebase;
        Err(BackendError("pull not supported by this backend".to_string()))
    }

    /// Apply + drop the most recent stash (`git stash pop`). Errors (surfaced as a Notice)
    /// when there is no stash or the pop conflicts; loader-only. Default errors.
    fn unstash(&self) -> Result<String, BackendError> {
        Err(BackendError("unstash not supported by this backend".to_string()))
    }

    // -- ref (branch/tag) ops, from the commit menu's branch/tag submenu --------

    /// Check out the ref `name` (`git checkout <name>`): a branch ATTACHES HEAD to it, a
    /// tag DETACHES. Git refuses on a dirty tree. Loader-only. Default errors.
    fn checkout_ref(&self, name: &str) -> Result<String, BackendError> {
        let _ = name;
        Err(BackendError("checkout not supported by this backend".to_string()))
    }

    /// Merge the ref `name` into the current branch (`git merge --no-edit <name>`). A
    /// conflict aborts the merge so the repo never sticks. DESTRUCTIVE (adds a merge
    /// commit / fast-forwards); loader-only. Default errors.
    fn merge_ref(&self, name: &str) -> Result<String, BackendError> {
        let _ = name;
        Err(BackendError("merge not supported by this backend".to_string()))
    }

    /// Rebase the current branch onto the ref `name` (`git rebase <name>`). A conflict
    /// aborts. DESTRUCTIVE history rewrite; loader-only. Default errors.
    fn rebase_onto(&self, name: &str) -> Result<String, BackendError> {
        let _ = name;
        Err(BackendError("rebase not supported by this backend".to_string()))
    }

    /// Rename branch `old` to `new` (`git branch -m <old> <new>`). Loader-only. Default
    /// errors.
    fn branch_rename(&self, old: &str, new: &str) -> Result<String, BackendError> {
        let _ = (old, new);
        Err(BackendError("rename not supported by this backend".to_string()))
    }

    /// Delete the local branch `name` (`git branch -d/-D <name>`; `force` = `-D`, drops
    /// the merged-into check). Git refuses to delete the current branch. Loader-only.
    /// Default errors.
    fn branch_delete(&self, name: &str, force: bool) -> Result<String, BackendError> {
        let _ = (name, force);
        Err(BackendError("branch delete not supported by this backend".to_string()))
    }

    /// Delete the tag `name` (`git tag -d <name>`). Loader-only. Default errors.
    fn tag_delete(&self, name: &str) -> Result<String, BackendError> {
        let _ = name;
        Err(BackendError("tag delete not supported by this backend".to_string()))
    }

    /// Push the local branch `name` to its remote (`git push <remote> <name>`, remote =
    /// the repo's first configured remote). Network IO (system git creds), loader-only.
    /// Default errors.
    fn push_ref(&self, name: &str) -> Result<String, BackendError> {
        let _ = name;
        Err(BackendError("push not supported by this backend".to_string()))
    }

    /// Pull `remote`/`branch` into the current branch using rebase (`rebase=true`) or
    /// merge (`git pull --rebase|--no-rebase <remote> <branch>`). A conflict aborts so the
    /// repo never sticks. Network IO (system git), loader-only. Default errors.
    fn pull_ref(&self, remote: &str, branch: &str, rebase: bool) -> Result<String, BackendError> {
        let _ = (remote, branch, rebase);
        Err(BackendError("pull not supported by this backend".to_string()))
    }

    /// Apply a unified-diff patch file onto the working tree (`git apply <path>`). A bad/
    /// conflicting patch errors (Notice); `git apply` is atomic so the tree stays intact.
    /// Loader-only. Default errors.
    fn apply_patch(&self, path: &str) -> Result<String, BackendError> {
        let _ = path;
        Err(BackendError("apply patch not supported by this backend".to_string()))
    }

    // -- remotes, from the global Git menu's Manage Remotes dialog ---------------

    /// The repo's configured remotes as `(name, fetch_url)` pairs, in git's order. A
    /// READ (no writes); powers the Manage Remotes list. Default returns empty (stub).
    fn remote_list(&self) -> Result<Vec<(String, String)>, BackendError> {
        Ok(Vec::new())
    }

    /// Add a remote `name` -> `url` (`git remote add <name> <url>`). Loader-only. Default
    /// errors.
    fn remote_add(&self, name: &str, url: &str) -> Result<String, BackendError> {
        let _ = (name, url);
        Err(BackendError("remote add not supported by this backend".to_string()))
    }

    /// Remove the remote `name` (`git remote remove <name>`). Loader-only. Default errors.
    fn remote_remove(&self, name: &str) -> Result<String, BackendError> {
        let _ = name;
        Err(BackendError("remote remove not supported by this backend".to_string()))
    }

    /// Set the remote `name`'s fetch URL (`git remote set-url <name> <url>`). Loader-only.
    /// Default errors.
    fn remote_set_url(&self, name: &str, url: &str) -> Result<String, BackendError> {
        let _ = (name, url);
        Err(BackendError("remote set-url not supported by this backend".to_string()))
    }
}

/// Turn a raw [`RepoSnapshot`] into the UI's [`RepoModel`]. THE single derivation
/// site: it runs the pure graph layout over the commits and stamps `is_me` on each
/// row from `current_user`. `detail`/`preview` start `None` (filled on selection).
pub fn build_repo_model(snapshot: RepoSnapshot) -> RepoModel {
    let RepoSnapshot {
        mut commits,
        tree,
        current_user,
        unpushed,
        has_remotes,
        truncated,
        status_sig,
        ..
    } = snapshot;
    for c in &mut commits {
        c.is_me = c.author == current_user;
    }
    let graph = graph_engine::build_layout(&commits);
    RepoModel {
        commits,
        graph,
        detail: None,
        tree,
        ignored: std::collections::HashSet::new(),
        unpushed,
        has_remotes,
        preview: None,
        more_history: truncated,
        status_sig: Some(status_sig),
    }
}
